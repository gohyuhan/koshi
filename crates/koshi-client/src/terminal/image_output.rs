//! Bounded connection-local Kitty, iTerm2, and Sixel image output.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ratatui::layout::{Position, Rect};

use koshi_core::geometry::PixelCellSize;
use koshi_image::{
    compute_rgba_byte_count, validate_image_dimensions, DecodedImage, GraphicsProtocol,
    MAX_IMAGE_BYTE_COUNT,
};
use koshi_iterm::{ItermEncoder, ItermOutputOptions, MAX_ITERM_PACKET_BYTE_COUNT};
use koshi_kitty::{
    write_kitty_delete_all, write_kitty_image_delete, write_kitty_placement,
    write_kitty_visible_placement_delete, KittyOutputError, KittyPlacement, KittyUpload,
};
use koshi_renderer::{
    ImageCellSnapshot, ImageCellState, ImagePaint, ImagePlacementKey, ImageSourceRect,
    MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT,
};
use koshi_sixel::{
    PreparedSixelPalette, SixelEncodeOptions, SixelEncoder, MAX_PALETTE_COLOR_COUNT,
    MAX_SIXEL_OUTPUT_BYTE_COUNT, MAX_SIXEL_TILE_BYTE_COUNT, MIN_PALETTE_COLOR_COUNT,
};
use koshi_terminal::graphics::{ImageRecord, SixelBackground};

use super::{restore_cursor_state, GraphicsSupport};

const MAX_OUTPUT_PAINT_COUNT: usize = 4_096;
const MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT: usize =
    MAX_IMAGE_BYTE_COUNT * 2 + MAX_OUTPUT_PAINT_COUNT * (std::mem::size_of::<OutputPaint>() + 256);

const SIXEL_MODE_SAVE_BYTES: &[u8] = b"\x1b[?80s\x1b[?8452s\x1b[?1070s";
const SIXEL_MODE_RESET_BYTES: &[u8] = b"\x1b[?80l\x1b[?8452l\x1b[?1070h";
const SIXEL_MODE_RESTORE_BYTES: &[u8] = b"\x1b[?80r\x1b[?8452r\x1b[?1070r";
const IMAGE_ABORT_BYTES: &[u8] = b"\x18\x1b\\";
const SCREEN_RESET_BYTES: &[u8] = b"\x1b[2J";
const KITTY_BACKGROUND_LAYER_Z_INDEX: i32 = i32::MIN / 2;

/// Output protocol settings for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImageOutputKind {
    /// Kitty graphics protocol output.
    Kitty,
    /// iTerm2 OSC 1337 image output.
    Iterm,
    /// DEC Sixel output with the host's measured limits.
    Sixel {
        /// Maximum palette entries accepted by the host.
        palette_color_count: usize,
        /// Maximum Sixel width in pixels.
        max_pixel_width: Option<u32>,
        /// Maximum Sixel height in pixels.
        max_pixel_height: Option<u32>,
    },
}

/// Describes a native image composition that is not exact on the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ImageCompatibility {
    /// The host cannot express the image layer order relative to text glyphs.
    pub(crate) has_text_layer_order_mismatch: bool,
    /// iTerm2 alpha cannot preserve the required underlying pixels.
    pub(crate) has_iterm_alpha_mismatch: bool,
    /// Sixel partial alpha uses an unknown underlying color.
    pub(crate) has_sixel_alpha_mismatch: bool,
    /// Sixel terminal-background pixels use an unknown cell color.
    pub(crate) has_sixel_terminal_background_mismatch: bool,
}

impl ImageCompatibility {
    fn exact() -> Self {
        Self::default()
    }
}

impl ImageOutputKind {
    /// Return the output protocol for a terminal capability.
    pub(crate) fn from_support(support: GraphicsSupport) -> Option<Self> {
        match support {
            GraphicsSupport::Kitty => Some(Self::Kitty),
            GraphicsSupport::Iterm => Some(Self::Iterm),
            GraphicsSupport::Sixel {
                palette_color_count,
                max_pixel_width,
                max_pixel_height,
            } => Some(Self::Sixel {
                palette_color_count,
                max_pixel_width,
                max_pixel_height,
            }),
            GraphicsSupport::Unsupported => None,
        }
    }

    /// Return whether the protocol uses Sixel host modes.
    pub(crate) const fn is_sixel(self) -> bool {
        matches!(self, Self::Sixel { .. })
    }

    /// Return whether the protocol blends image pixels with the cells the
    /// image covers. Kitty places pixels by z-index, so it reads no cell.
    pub(crate) const fn uses_cell_composition(self) -> bool {
        !matches!(self, Self::Kitty)
    }
}

/// Return the Sixel mode-save sequence used at application-mode setup.
pub(crate) const fn get_sixel_mode_save_bytes() -> &'static [u8] {
    SIXEL_MODE_SAVE_BYTES
}

/// Return the Sixel mode-restore sequence used at application-mode cleanup.
pub(crate) const fn get_sixel_mode_restore_bytes() -> &'static [u8] {
    SIXEL_MODE_RESTORE_BYTES
}

/// Write the cancellation sequence for an open image string.
pub(crate) fn write_image_abort<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(IMAGE_ABORT_BYTES)
}

/// One image's current connection-local output state.
#[derive(Debug, Clone)]
struct OutputPaint {
    placement_key: ImagePlacementKey,
    image_content_id: u64,
    image_record: Arc<ImageRecord>,
    target_area: Rect,
    source_rect: ImageSourceRect,
    cell_pixel_offset_x: Option<u32>,
    cell_pixel_offset_y: Option<u32>,
    z_index: i32,
    alpha_stats: Option<AlphaStats>,
}

impl OutputPaint {
    fn from_paint(image_paint: &ImagePaint, alpha_stats: Option<AlphaStats>) -> Self {
        Self {
            placement_key: (image_paint.pane_id, image_paint.placement_id),
            image_content_id: image_paint.image_content_id,
            image_record: Arc::clone(&image_paint.image_record),
            target_area: image_paint.target_area,
            source_rect: image_paint.source_rect,
            cell_pixel_offset_x: image_paint.cell_pixel_offset_x,
            cell_pixel_offset_y: image_paint.cell_pixel_offset_y,
            z_index: image_paint.z_index,
            alpha_stats,
        }
    }

    /// Return whether this paint's encoded pixels read the cells it covers.
    ///
    /// A fully opaque image at a z-index at or above zero replaces every cell
    /// it covers, so its pixels and its host compatibility read no cell. An
    /// unknown alpha coverage reads the cells.
    fn needs_target_cell_composition(&self, output_kind: ImageOutputKind) -> bool {
        if !output_kind.uses_cell_composition() {
            return false;
        }
        !(self.z_index >= 0 && self.alpha_stats.is_some_and(AlphaStats::is_fully_opaque))
    }
}

/// Values that identify exact image output inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EncodeKey {
    image_content_id: u64,
    image_memory_address: usize,
    source_rect: ImageSourceKey,
    target_column_count: u16,
    target_row_count: u16,
    pixel_cell_size: PixelCellSize,
    output_kind: ImageOutputKind,
    z_index: i32,
    has_terminal_background: bool,
    composition_revision: u64,
    composition_pixel_cell_size: Option<PixelCellSize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageSourceKey {
    pixel_x: u32,
    pixel_y: u32,
    pixel_width: u32,
    pixel_height: u32,
}

#[derive(Debug)]
struct CompositionFrame {
    cell_snapshot: Arc<ImageCellSnapshot>,
    composition_paints: Arc<[CompositionPaint]>,
}

#[derive(Debug, Clone)]
struct CompositionState {
    composition_frame: Arc<CompositionFrame>,
    composition_paint_index: usize,
}

#[derive(Debug)]
struct VersionedComposition {
    composition_state: CompositionState,
    composition_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompositionPaint {
    image_memory_address: usize,
    image_content_id: u64,
    image_record: Arc<ImageRecord>,
    placement_key: ImagePlacementKey,
    target_area: Rect,
    source_rect: ImageSourceKey,
    z_index: i32,
}

impl CompositionFrame {
    fn from_cell_snapshot_and_output_paints(
        cell_snapshot: &Arc<ImageCellSnapshot>,
        output_paints: &[OutputPaint],
    ) -> Self {
        Self {
            cell_snapshot: Arc::clone(cell_snapshot),
            composition_paints: output_paints
                .iter()
                .map(|output_paint| CompositionPaint {
                    image_memory_address: Arc::as_ptr(&output_paint.image_record.image) as usize,
                    image_content_id: output_paint.image_content_id,
                    image_record: Arc::clone(&output_paint.image_record),
                    placement_key: output_paint.placement_key,
                    target_area: output_paint.target_area,
                    source_rect: ImageSourceKey::from_source_rect(output_paint.source_rect),
                    z_index: output_paint.z_index,
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

impl PartialEq for CompositionState {
    fn eq(&self, other: &Self) -> bool {
        let Some(composition_paint) = self
            .composition_frame
            .composition_paints
            .get(self.composition_paint_index)
        else {
            return false;
        };
        let Some(other_composition_paint) = other
            .composition_frame
            .composition_paints
            .get(other.composition_paint_index)
        else {
            return false;
        };
        composition_paint == other_composition_paint
            && (composition_paint.target_area.y..composition_paint.target_area.bottom()).all(
                |row_index| {
                    (composition_paint.target_area.x..composition_paint.target_area.right()).all(
                        |column_index| {
                            self.composition_frame
                                .cell_snapshot
                                .find_cell(column_index, row_index)
                                == other
                                    .composition_frame
                                    .cell_snapshot
                                    .find_cell(column_index, row_index)
                        },
                    )
                },
            )
            && self.composition_frame.composition_paints[..self.composition_paint_index]
                .iter()
                .filter(|lower_paint| {
                    is_rectangles_overlapping(
                        lower_paint.target_area,
                        composition_paint.target_area,
                    )
                })
                .eq(
                    other.composition_frame.composition_paints[..other.composition_paint_index]
                        .iter()
                        .filter(|lower_paint| {
                            is_rectangles_overlapping(
                                lower_paint.target_area,
                                other_composition_paint.target_area,
                            )
                        }),
                )
    }
}

impl Eq for CompositionState {}

impl ImageSourceKey {
    fn from_source_rect(source_rect: ImageSourceRect) -> Self {
        Self {
            pixel_x: source_rect.pixel_x,
            pixel_y: source_rect.pixel_y,
            pixel_width: source_rect.pixel_width,
            pixel_height: source_rect.pixel_height,
        }
    }
}

/// One Kitty image number the host already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyImage {
    /// The nonzero Kitty image number carried by `I=`.
    kitty_image_number: u32,
    /// The address of the `DecodedImage` transmitted under `kitty_image_number`.
    image_memory_address: usize,
}

/// The Kitty image number one paint places, and whether that paint carries the
/// pixels. `should_transmit_image` is false when the host already holds the
/// image content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyPaintImage {
    /// The nonzero Kitty image number this paint places.
    kitty_image_number: u32,
    /// Whether this paint writes the image's pixels before it places them.
    should_transmit_image: bool,
}

/// A worker request containing one bounded frame's images and one shared cell snapshot.
struct WorkerRequest {
    frame_generation: u64,
    output_kind: ImageOutputKind,
    pixel_cell_size: PixelCellSize,
    measured_pixel_cell_size: Option<PixelCellSize>,
    cell_snapshot: Option<Arc<ImageCellSnapshot>>,
    output_paints: Vec<OutputPaint>,
    encode_keys: Vec<EncodeKey>,
    /// One entry per output paint, in the same order. Empty for every other protocol.
    kitty_paint_images: Vec<KittyPaintImage>,
    cancellation_token: Arc<AtomicBool>,
}

/// A worker output unit with relative cell geometry.
#[derive(Debug)]
struct OutputUnit {
    frame_generation: u64,
    placement_key: ImagePlacementKey,
    output_kind: ImageOutputKind,
    tile_offset: (u16, u16),
    output_bytes: Arc<[u8]>,
}

/// Messages sent by the bounded image worker.
#[derive(Debug)]
enum WorkerMessage {
    Prepared {
        frame_generation: u64,
        placement_key: ImagePlacementKey,
        image_compatibility: ImageCompatibility,
    },
    Unavailable {
        frame_generation: u64,
        placement_key: ImagePlacementKey,
    },
    Unit(OutputUnit),
    Finished {
        frame_generation: u64,
        has_failed: bool,
    },
}

/// One active request and its cancellation token.
struct ActiveJob {
    frame_generation: u64,
    cancellation_token: Arc<AtomicBool>,
}

/// Bounded image output state owned by one terminal connection.
pub(crate) struct ImageOutputState {
    output_kind: Option<ImageOutputKind>,
    worker_request_sender: Option<SyncSender<WorkerRequest>>,
    worker_messages: Option<Receiver<WorkerMessage>>,
    worker: Option<JoinHandle<()>>,
    frame_generation: u64,
    active_job: Option<ActiveJob>,
    pending_worker_request: Option<WorkerRequest>,
    latest_output_paints: Vec<OutputPaint>,
    latest_paint_index_by_placement_key: HashMap<ImagePlacementKey, usize>,
    latest_encode_keys: Vec<EncodeKey>,
    /// The composition revision of the cells under each output paint in the latest frame.
    latest_composition_revisions: Vec<u64>,
    composition_state_by_placement_key: HashMap<ImagePlacementKey, VersionedComposition>,
    next_composition_revision: u64,
    is_settled: bool,
    is_ready: bool,
    prepared_placement_keys: Vec<ImagePlacementKey>,
    prepared_placement_key_set: HashSet<ImagePlacementKey>,
    compatibility_by_placement_key: HashMap<ImagePlacementKey, ImageCompatibility>,
    output_units: Vec<OutputUnit>,
    output_unit_byte_count: usize,
    has_host_pixels: bool,
    needs_screen_reset: bool,
    needs_image_abort: bool,
    /// Host screen size in columns and rows at the last painted frame.
    host_terminal_size: Option<(u16, u16)>,
    /// Alpha coverage per decoded-image address and source rectangle. Holds
    /// only the addresses retained by the latest frame.
    alpha_stats_by_image_and_source: HashMap<(usize, ImageSourceKey), Option<AlphaStats>>,
    /// Kitty image numbers the host holds, keyed by content identity.
    kitty_image_by_content_id: HashMap<u64, KittyImage>,
    /// Kitty image numbers this frame transmits. They join the committed image
    /// map when the frame commits, and are dropped when it does not.
    pending_kitty_images_by_content_id: Vec<(u64, KittyImage)>,
    /// Kitty image numbers the next frame reset frees on the host.
    kitty_image_numbers_to_delete: Vec<u32>,
    /// Whether the next frame reset frees every Kitty image the host holds.
    should_free_all_kitty_images: bool,
    /// The next Kitty image number handed out. Never zero.
    next_kitty_image_number: u32,
}

impl ImageOutputState {
    fn rebuild_latest_index(&mut self) {
        self.latest_paint_index_by_placement_key.clear();
        self.latest_paint_index_by_placement_key.extend(
            self.latest_output_paints
                .iter()
                .enumerate()
                .map(|(paint_index, output_paint)| (output_paint.placement_key, paint_index)),
        );
    }

    /// Build a connection-local worker for the selected output protocol.
    pub(crate) fn from_output_kind(output_kind: Option<ImageOutputKind>) -> Self {
        let (worker_request_sender, worker_messages, worker) = if output_kind.is_some() {
            let (request_sender, request_receiver) = mpsc::sync_channel(1);
            let (message_sender, message_receiver) = mpsc::sync_channel(1);
            match thread::Builder::new()
                .name(String::from("koshi-image-output"))
                .spawn(move || worker_loop(request_receiver, message_sender))
            {
                Ok(worker) => (Some(request_sender), Some(message_receiver), Some(worker)),
                Err(worker_spawn_error) => {
                    tracing::warn!(%worker_spawn_error, "could not start the image output worker");
                    (None, None, None)
                }
            }
        } else {
            (None, None, None)
        };
        Self {
            output_kind,
            worker_request_sender,
            worker_messages,
            worker,
            frame_generation: 0,
            active_job: None,
            pending_worker_request: None,
            latest_output_paints: Vec::new(),
            latest_paint_index_by_placement_key: HashMap::new(),
            latest_encode_keys: Vec::new(),
            latest_composition_revisions: Vec::new(),
            composition_state_by_placement_key: HashMap::new(),
            next_composition_revision: 0,
            is_settled: false,
            is_ready: true,
            prepared_placement_keys: Vec::new(),
            prepared_placement_key_set: HashSet::new(),
            compatibility_by_placement_key: HashMap::new(),
            output_units: Vec::new(),
            output_unit_byte_count: 0,
            has_host_pixels: false,
            needs_screen_reset: false,
            needs_image_abort: false,
            host_terminal_size: None,
            alpha_stats_by_image_and_source: HashMap::new(),
            kitty_image_by_content_id: HashMap::new(),
            pending_kitty_images_by_content_id: Vec::new(),
            kitty_image_numbers_to_delete: Vec::new(),
            should_free_all_kitty_images: false,
            next_kitty_image_number: 1,
        }
    }

    /// Return an inactive state for tests and terminals without image output.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::from_output_kind(None)
    }

    /// Return the connection's output protocol, if it has one.
    pub(crate) const fn output_kind(&self) -> Option<ImageOutputKind> {
        self.output_kind
    }

    /// Return prepared placement keys for renderer selection.
    pub(crate) fn list_prepared_placement_keys(&self) -> &[ImagePlacementKey] {
        &self.prepared_placement_keys
    }

    /// Return the host composition status for one prepared placement.
    pub(crate) fn get_image_compatibility(
        &self,
        placement_key: ImagePlacementKey,
    ) -> Option<ImageCompatibility> {
        self.compatibility_by_placement_key
            .get(&placement_key)
            .copied()
    }

    /// Return whether this state needs another attachment-loop pass.
    pub(crate) fn work_pending(&self) -> bool {
        !self.is_ready || self.active_job.is_some() || self.pending_worker_request.is_some()
    }

    /// Return whether the newest frame can re-emit the encoded output the
    /// committed frame already holds.
    ///
    /// [`frame_output`](Self::frame_output) reads each unit's screen position
    /// from the latest output paints when it writes the frame, so output whose encode keys and
    /// placement identities are unchanged stays correct at a new position.
    /// Kitty writes each placement's position into its own unit, so its output
    /// is never reused.
    fn can_reuse_native_output(
        &self,
        latest_output_paints: &[OutputPaint],
        encode_keys: &[EncodeKey],
    ) -> bool {
        matches!(
            self.output_kind,
            Some(ImageOutputKind::Iterm | ImageOutputKind::Sixel { .. })
        ) && self.is_ready
            && !self.output_units.is_empty()
            && self.latest_encode_keys == encode_keys
            && self.latest_output_paints.len() == latest_output_paints.len()
            && self
                .latest_output_paints
                .iter()
                .zip(latest_output_paints)
                .all(|(held_paint, next_paint)| {
                    held_paint.placement_key == next_paint.placement_key
                })
    }

    /// Assign one Kitty image number per output paint in `self.latest_output_paints`.
    ///
    /// A content identity the host already holds keeps its number and
    /// transmits no pixels. Every number whose content identity left the frame
    /// joins `kitty_image_numbers_to_delete`. Returns `None` when the numbers cannot be
    /// reserved.
    fn plan_kitty_images(&mut self) -> Option<Vec<KittyPaintImage>> {
        if self.next_kitty_image_number == u32::MAX {
            self.forget_kitty_images();
        }
        let mut kitty_paint_images = Vec::new();
        kitty_paint_images
            .try_reserve_exact(self.latest_output_paints.len())
            .ok()?;
        self.pending_kitty_images_by_content_id.clear();
        let mut kitty_image_number_by_content_and_address = HashMap::new();
        kitty_image_number_by_content_and_address
            .try_reserve(self.latest_output_paints.len())
            .ok()?;
        for paint_index in 0..self.latest_output_paints.len() {
            let output_paint = &self.latest_output_paints[paint_index];
            let image_content_id = output_paint.image_content_id;
            let image_memory_address = Arc::as_ptr(&output_paint.image_record.image) as usize;
            if let Some(&kitty_image_number) = kitty_image_number_by_content_and_address
                .get(&(image_content_id, image_memory_address))
            {
                kitty_paint_images.push(KittyPaintImage {
                    kitty_image_number,
                    should_transmit_image: false,
                });
                continue;
            }
            let held_kitty_image = self
                .kitty_image_by_content_id
                .get(&image_content_id)
                .filter(|kitty_image| kitty_image.image_memory_address == image_memory_address)
                .copied();
            let (kitty_image_number, should_transmit_image) = match held_kitty_image {
                Some(kitty_image) => (kitty_image.kitty_image_number, false),
                None => {
                    let kitty_image_number = self.next_kitty_image_number;
                    self.next_kitty_image_number = kitty_image_number.checked_add(1)?;
                    self.pending_kitty_images_by_content_id.push((
                        image_content_id,
                        KittyImage {
                            kitty_image_number,
                            image_memory_address,
                        },
                    ));
                    (kitty_image_number, true)
                }
            };
            kitty_image_number_by_content_and_address
                .insert((image_content_id, image_memory_address), kitty_image_number);
            kitty_paint_images.push(KittyPaintImage {
                kitty_image_number,
                should_transmit_image,
            });
        }
        let departed_kitty_images = self
            .kitty_image_by_content_id
            .iter()
            .filter(|(image_content_id, kitty_image)| {
                !kitty_image_number_by_content_and_address
                    .contains_key(&(**image_content_id, kitty_image.image_memory_address))
            })
            .map(|(image_content_id, kitty_image)| {
                (*image_content_id, kitty_image.kitty_image_number)
            })
            .collect::<Vec<_>>();
        for (image_content_id, kitty_image_number) in departed_kitty_images {
            self.kitty_image_by_content_id.remove(&image_content_id);
            self.kitty_image_numbers_to_delete.push(kitty_image_number);
        }
        Some(kitty_paint_images)
    }

    /// Drop every Kitty image number and free the host's data at the next
    /// reset. On every other protocol this frees nothing.
    fn forget_kitty_images(&mut self) {
        self.kitty_image_by_content_id.clear();
        self.pending_kitty_images_by_content_id.clear();
        self.kitty_image_numbers_to_delete.clear();
        self.should_free_all_kitty_images =
            matches!(self.output_kind, Some(ImageOutputKind::Kitty));
        self.next_kitty_image_number = 1;
    }

    /// Record the host screen size for the frame being painted.
    ///
    /// A changed size forgets every Kitty image number. The next frame
    /// transmits its images again.
    pub(crate) fn set_host_terminal_size(&mut self, column_count: u16, row_count: u16) {
        let host_terminal_size = Some((column_count, row_count));
        if self.host_terminal_size == host_terminal_size {
            return;
        }
        let has_previous_terminal_size = self.host_terminal_size.is_some();
        self.host_terminal_size = host_terminal_size;
        if has_previous_terminal_size && matches!(self.output_kind, Some(ImageOutputKind::Kitty)) {
            self.forget_kitty_images();
            self.latest_encode_keys.clear();
            self.latest_composition_revisions.clear();
        }
    }

    /// Submit the newest frame and return whether it can be committed.
    pub(crate) fn prepare_frame(
        &mut self,
        image_paints: &[ImagePaint],
        cell_snapshot: Option<Arc<ImageCellSnapshot>>,
        measured_pixel_cell_size: Option<PixelCellSize>,
    ) -> bool {
        self.poll();
        let Some(output_kind) = self.output_kind else {
            return true;
        };
        let pixel_cell_size = if output_kind.is_sixel() {
            let Some(pixel_cell_size) = measured_pixel_cell_size else {
                self.set_unavailable_frame(image_paints);
                return true;
            };
            pixel_cell_size
        } else {
            PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell is nonzero")
        };
        if image_paints.len() > MAX_OUTPUT_PAINT_COUNT {
            self.set_unavailable_frame(image_paints);
            return true;
        }
        if !image_paints.is_empty()
            && output_kind.uses_cell_composition()
            && cell_snapshot.as_ref().is_none_or(|cell_snapshot| {
                usize::try_from(cell_snapshot.screen_area.area()).ok()
                    > Some(MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT)
            })
        {
            self.set_unavailable_frame(image_paints);
            return true;
        }
        let mut latest_output_paints = Vec::new();
        let mut alpha_stats_by_image_and_source = HashMap::new();
        if latest_output_paints
            .try_reserve_exact(image_paints.len())
            .is_err()
            || alpha_stats_by_image_and_source
                .try_reserve(image_paints.len())
                .is_err()
        {
            self.set_unavailable_frame(image_paints);
            return true;
        }
        for image_paint in image_paints {
            let image_source_key = (
                Arc::as_ptr(&image_paint.image_record.image) as usize,
                ImageSourceKey::from_source_rect(image_paint.source_rect),
            );
            let image_alpha_stats = match self
                .alpha_stats_by_image_and_source
                .get(&image_source_key)
                .copied()
            {
                Some(alpha_stats) => alpha_stats,
                None if output_kind.uses_cell_composition() => {
                    compute_alpha_stats(&image_paint.image_record.image, image_paint.source_rect)
                }
                None => None,
            };
            alpha_stats_by_image_and_source.insert(image_source_key, image_alpha_stats);
            latest_output_paints.push(OutputPaint::from_paint(image_paint, image_alpha_stats));
        }
        self.alpha_stats_by_image_and_source = alpha_stats_by_image_and_source;
        let composition_revisions = if let Some(cell_snapshot) = cell_snapshot.as_ref() {
            self.update_composition_revisions(output_kind, cell_snapshot, &latest_output_paints)
        } else {
            vec![0; latest_output_paints.len()]
        };
        let mut covered_cell_positions = HashSet::new();
        let encode_keys = latest_output_paints
            .iter()
            .zip(composition_revisions.iter().copied())
            .map(|(output_paint, composition_revision)| {
                let has_lower_overlap =
                    has_covered_target_cell(output_paint.target_area, &covered_cell_positions);
                let mut encode_key =
                    build_output_encode_key(output_kind, pixel_cell_size, output_paint);
                if output_paint.needs_target_cell_composition(output_kind) {
                    encode_key.composition_revision = composition_revision;
                    if matches!(output_kind, ImageOutputKind::Iterm)
                        && output_paint
                            .alpha_stats
                            .is_some_and(|alpha_stats| !alpha_stats.is_fully_opaque())
                        && (has_lower_overlap
                            || cell_snapshot.as_ref().is_some_and(|cell_snapshot| {
                                let composition_details =
                                    compute_composition_info(cell_snapshot, output_paint);
                                !composition_details.is_default_blank
                                    && composition_details.solid_background_color.is_none()
                            }))
                    {
                        encode_key.composition_pixel_cell_size = measured_pixel_cell_size;
                    }
                }
                add_target_area_cells(output_paint.target_area, &mut covered_cell_positions);
                encode_key
            })
            .collect::<Vec<_>>();
        let are_output_paints_unchanged =
            is_output_paint_sequence_equal(&self.latest_output_paints, &latest_output_paints);
        if are_output_paints_unchanged
            && self.latest_encode_keys == encode_keys
            && self.latest_composition_revisions == composition_revisions
        {
            return self.is_ready;
        }
        if self.can_reuse_native_output(&latest_output_paints, &encode_keys) {
            self.latest_output_paints = latest_output_paints;
            self.latest_composition_revisions = composition_revisions;
            self.rebuild_latest_index();
            self.is_settled = false;
            self.needs_screen_reset = !are_output_paints_unchanged && self.has_host_pixels;
            return true;
        }
        self.latest_output_paints = latest_output_paints;
        self.latest_encode_keys = encode_keys.clone();
        self.latest_composition_revisions = composition_revisions;
        self.rebuild_latest_index();
        let kitty_paint_images = if matches!(output_kind, ImageOutputKind::Kitty) {
            match self.plan_kitty_images() {
                Some(kitty_paint_images) => kitty_paint_images,
                None => {
                    self.set_unavailable_frame(image_paints);
                    return true;
                }
            }
        } else {
            Vec::new()
        };
        self.is_settled = false;
        self.is_ready = self.latest_output_paints.is_empty();
        self.needs_screen_reset = self.has_host_pixels;
        self.clear_prepared_output();
        self.cancel_active();
        if let Some(previous_request) = self.pending_worker_request.take() {
            previous_request
                .cancellation_token
                .store(true, Ordering::Release);
        }
        if self.is_ready {
            self.next_generation();
            return true;
        }
        let request_cancellation_token = Arc::new(AtomicBool::new(false));
        let worker_request = WorkerRequest {
            frame_generation: self.next_generation(),
            output_kind,
            pixel_cell_size,
            measured_pixel_cell_size,
            cell_snapshot,
            output_paints: self.latest_output_paints.clone(),
            encode_keys: encode_keys.clone(),
            kitty_paint_images,
            cancellation_token: Arc::clone(&request_cancellation_token),
        };
        if self.active_job.is_none() {
            self.start_worker_request(worker_request);
        } else {
            self.pending_worker_request = Some(worker_request);
        }
        self.is_ready
    }

    /// Receive worker results without waiting on the output queue.
    pub(crate) fn poll(&mut self) {
        loop {
            let worker_message = match self.worker_messages.as_ref().map(Receiver::try_recv) {
                Some(Ok(worker_message)) => worker_message,
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.active_job = None;
                    self.pending_worker_request = None;
                    self.fail_current_generation();
                    break;
                }
            };
            match worker_message {
                WorkerMessage::Prepared {
                    frame_generation,
                    placement_key,
                    image_compatibility,
                } => {
                    if self.is_current_generation(frame_generation)
                        && self
                            .latest_paint_index_by_placement_key
                            .contains_key(&placement_key)
                    {
                        if self.prepared_placement_keys.len() < MAX_OUTPUT_PAINT_COUNT
                            && self.prepared_placement_key_set.insert(placement_key)
                        {
                            self.prepared_placement_keys.push(placement_key);
                        }
                        self.compatibility_by_placement_key
                            .insert(placement_key, image_compatibility);
                    }
                }
                WorkerMessage::Unavailable {
                    frame_generation,
                    placement_key,
                } => {
                    if self.is_current_generation(frame_generation) {
                        self.compatibility_by_placement_key.remove(&placement_key);
                    }
                }
                WorkerMessage::Unit(output_unit) => {
                    if self.is_current_generation(output_unit.frame_generation)
                        && self
                            .prepared_placement_key_set
                            .contains(&output_unit.placement_key)
                        && self
                            .latest_paint_index_by_placement_key
                            .contains_key(&output_unit.placement_key)
                    {
                        let Some(next_output_byte_count) = self
                            .output_unit_byte_count
                            .checked_add(output_unit.output_bytes.len())
                        else {
                            self.fail_current_generation();
                            continue;
                        };
                        if next_output_byte_count > MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT
                            || self.output_units.try_reserve(1).is_err()
                        {
                            self.fail_current_generation();
                            continue;
                        }
                        self.output_unit_byte_count = next_output_byte_count;
                        self.output_units.push(output_unit);
                    }
                }
                WorkerMessage::Finished {
                    frame_generation,
                    has_failed,
                } => {
                    if self.is_current_generation(frame_generation) {
                        if has_failed {
                            self.fail_current_generation();
                        } else {
                            self.is_ready = true;
                        }
                    }
                    if self
                        .active_job
                        .as_ref()
                        .is_some_and(|active_job| active_job.frame_generation == frame_generation)
                    {
                        self.active_job = None;
                        self.start_pending_worker_request();
                    }
                }
            }
        }
    }

    /// Return whether the newest frame changes native terminal image state.
    pub(crate) const fn native_commit_pending(&self) -> bool {
        !self.is_settled
    }

    /// Write the host-side reset that precedes the newest frame's base cells.
    ///
    /// Writes, in order: the abort of an open image string, the Kitty deletes
    /// (every image when a failed or replaced connection made the host state
    /// unknown, otherwise the visible placements plus each image number that
    /// left the frame), and, for Sixel and iTerm2 output only, `ESC[2J` when
    /// the previous frame's pixels are stale. Kitty output never writes
    /// `ESC[2J`. Returns whether the caller must redraw every text cell.
    pub(crate) fn write_frame_reset<W: Write>(&mut self, writer: &mut W) -> io::Result<bool> {
        let has_pending_kitty_cleanup =
            self.should_free_all_kitty_images || !self.kitty_image_numbers_to_delete.is_empty();
        if !self.needs_screen_reset && !self.needs_image_abort && !has_pending_kitty_cleanup {
            return Ok(false);
        }
        if self.needs_image_abort {
            write_image_abort(writer)?;
            self.needs_image_abort = false;
        }
        let is_kitty_output = matches!(self.output_kind, Some(ImageOutputKind::Kitty));
        if is_kitty_output {
            if self.should_free_all_kitty_images {
                write_kitty_delete_all(writer).map_err(convert_kitty_output_error)?;
                self.should_free_all_kitty_images = false;
                self.kitty_image_numbers_to_delete.clear();
            } else {
                if self.needs_screen_reset {
                    write_kitty_visible_placement_delete(writer)
                        .map_err(convert_kitty_output_error)?;
                }
                for kitty_image_number in self.kitty_image_numbers_to_delete.drain(..) {
                    write_kitty_image_delete(writer, kitty_image_number)
                        .map_err(convert_kitty_output_error)?;
                }
            }
        }
        let clears_screen = self.needs_screen_reset && !is_kitty_output;
        if clears_screen {
            writer.write_all(SCREEN_RESET_BYTES)?;
        }
        writer.flush()?;
        Ok(clears_screen)
    }

    /// Build all native bytes written after the newest base-cell frame.
    pub(crate) fn frame_output(&self, cursor: Option<Position>) -> io::Result<Vec<u8>> {
        let mut output_bytes = Vec::new();
        let overhead = self
            .output_units
            .len()
            .saturating_mul(128)
            .saturating_add(32);
        output_bytes
            .try_reserve(self.output_unit_byte_count.saturating_add(overhead))
            .map_err(|_| invalid_output("image output storage could not be allocated"))?;
        for output_unit in &self.output_units {
            let Some(&paint_index) = self
                .latest_paint_index_by_placement_key
                .get(&output_unit.placement_key)
            else {
                continue;
            };
            let output_paint = &self.latest_output_paints[paint_index];
            let screen_column = output_paint
                .target_area
                .x
                .checked_add(output_unit.tile_offset.0)
                .ok_or_else(|| invalid_output("image tile x coordinate overflows the frame"))?;
            let screen_row = output_paint
                .target_area
                .y
                .checked_add(output_unit.tile_offset.1)
                .ok_or_else(|| invalid_output("image tile y coordinate overflows the frame"))?;
            match output_unit.output_kind {
                ImageOutputKind::Kitty => output_bytes.extend_from_slice(&output_unit.output_bytes),
                ImageOutputKind::Iterm => {
                    write_cursor_position(&mut output_bytes, screen_column, screen_row)?;
                    output_bytes.extend_from_slice(&output_unit.output_bytes);
                    restore_cursor_state(&mut output_bytes, cursor)?;
                }
                ImageOutputKind::Sixel { .. } => {
                    output_bytes.extend_from_slice(SIXEL_MODE_RESET_BYTES);
                    write_cursor_position(&mut output_bytes, screen_column, screen_row)?;
                    output_bytes.extend_from_slice(&output_unit.output_bytes);
                    restore_cursor_state(&mut output_bytes, cursor)?;
                    output_bytes.extend_from_slice(SIXEL_MODE_RESTORE_BYTES);
                }
            }
        }
        if matches!(self.output_kind, Some(ImageOutputKind::Kitty)) && !output_bytes.is_empty() {
            restore_cursor_state(&mut output_bytes, cursor)?;
        }
        if output_bytes.len() > MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT {
            return Err(invalid_output(
                "native image frame exceeds its output limit",
            ));
        }
        Ok(output_bytes)
    }

    /// Adopt the newest frame after its base cells and native bytes are written.
    pub(crate) fn commit_frame(&mut self) {
        self.has_host_pixels = !self.prepared_placement_keys.is_empty();
        self.is_settled = true;
        self.is_ready = true;
        self.needs_screen_reset = false;
        for (image_content_id, kitty_image) in self.pending_kitty_images_by_content_id.drain(..) {
            self.kitty_image_by_content_id
                .insert(image_content_id, kitty_image);
        }
    }

    /// Keep the newest frame uncommitted after a native output failure.
    pub(crate) fn fail_frame_commit(&mut self) {
        self.needs_image_abort = self.output_kind.is_some();
        self.cancel_active();
        if let Some(pending_request) = self.pending_worker_request.take() {
            pending_request
                .cancellation_token
                .store(true, Ordering::Release);
        }
        self.next_generation();
        self.clear_prepared_output();
        self.latest_output_paints.clear();
        self.latest_paint_index_by_placement_key.clear();
        self.latest_encode_keys.clear();
        self.latest_composition_revisions.clear();
        self.alpha_stats_by_image_and_source.clear();
        self.has_host_pixels = true;
        self.needs_screen_reset = true;
        self.is_settled = false;
        self.is_ready = true;
        self.forget_kitty_images();
    }

    /// Reset this output state when a connection is replaced.
    pub(crate) fn reset_connection(&mut self) {
        self.is_settled = false;
        self.is_ready = true;
        self.cancel_active();
        if let Some(pending_request) = self.pending_worker_request.take() {
            pending_request
                .cancellation_token
                .store(true, Ordering::Release);
        }
        self.frame_generation = self.frame_generation.wrapping_add(1).max(1);
        self.clear_prepared_output();
        self.composition_state_by_placement_key.clear();
        self.next_composition_revision = 0;
        self.latest_output_paints.clear();
        self.latest_paint_index_by_placement_key.clear();
        self.latest_encode_keys.clear();
        self.latest_composition_revisions.clear();
        self.needs_screen_reset = self.has_host_pixels;
        self.alpha_stats_by_image_and_source.clear();
        self.forget_kitty_images();
    }

    fn update_composition_revisions(
        &mut self,
        output_kind: ImageOutputKind,
        cell_snapshot: &Arc<ImageCellSnapshot>,
        output_paints: &[OutputPaint],
    ) -> Vec<u64> {
        if self.next_composition_revision == u64::MAX {
            self.composition_state_by_placement_key.clear();
            self.next_composition_revision = 0;
        }
        let composition_frame = Arc::new(CompositionFrame::from_cell_snapshot_and_output_paints(
            cell_snapshot,
            output_paints,
        ));
        let mut active_placement_keys = HashSet::new();
        let revisions = output_paints
            .iter()
            .enumerate()
            .map(|(paint_index, output_paint)| {
                if !output_kind.uses_cell_composition() {
                    return 0;
                }
                active_placement_keys.insert(output_paint.placement_key);
                let composition_state = CompositionState {
                    composition_frame: Arc::clone(&composition_frame),
                    composition_paint_index: paint_index,
                };
                if let Some(versioned_composition) = self
                    .composition_state_by_placement_key
                    .get(&output_paint.placement_key)
                {
                    if versioned_composition.composition_state == composition_state {
                        return versioned_composition.composition_revision;
                    }
                }
                self.next_composition_revision += 1;
                self.composition_state_by_placement_key.insert(
                    output_paint.placement_key,
                    VersionedComposition {
                        composition_state,
                        composition_revision: self.next_composition_revision,
                    },
                );
                self.next_composition_revision
            })
            .collect();
        self.composition_state_by_placement_key
            .retain(|placement_key, _| active_placement_keys.contains(placement_key));
        revisions
    }

    fn next_generation(&mut self) -> u64 {
        self.frame_generation = self.frame_generation.wrapping_add(1).max(1);
        self.frame_generation
    }

    fn start_worker_request(&mut self, worker_request: WorkerRequest) {
        let active_job = ActiveJob {
            frame_generation: worker_request.frame_generation,
            cancellation_token: Arc::clone(&worker_request.cancellation_token),
        };
        let Some(worker_request_sender) = &self.worker_request_sender else {
            self.fail_current_generation();
            return;
        };
        match worker_request_sender.try_send(worker_request) {
            Ok(()) => {
                self.active_job = Some(active_job);
            }
            Err(TrySendError::Full(worker_request)) => {
                self.pending_worker_request = Some(worker_request);
            }
            Err(TrySendError::Disconnected(_)) => self.fail_current_generation(),
        }
    }

    fn start_pending_worker_request(&mut self) {
        let Some(worker_request) = self.pending_worker_request.take() else {
            return;
        };
        self.start_worker_request(worker_request);
    }

    fn is_current_generation(&self, generation: u64) -> bool {
        generation == self.frame_generation
    }

    fn clear_prepared_output(&mut self) {
        self.prepared_placement_keys.clear();
        self.prepared_placement_key_set.clear();
        self.compatibility_by_placement_key.clear();
        self.output_units.clear();
        self.output_unit_byte_count = 0;
    }

    fn set_unavailable_frame(&mut self, image_paints: &[ImagePaint]) {
        self.cancel_active();
        if let Some(pending_request) = self.pending_worker_request.take() {
            pending_request
                .cancellation_token
                .store(true, Ordering::Release);
        }
        self.next_generation();
        self.latest_output_paints = image_paints
            .iter()
            .map(|image_paint| OutputPaint::from_paint(image_paint, None))
            .collect();
        self.rebuild_latest_index();
        self.alpha_stats_by_image_and_source.clear();
        self.forget_kitty_images();
        self.latest_encode_keys.clear();
        self.latest_composition_revisions.clear();
        self.is_settled = false;
        self.is_ready = true;
        self.needs_screen_reset = self.has_host_pixels;
        self.clear_prepared_output();
    }

    fn fail_current_generation(&mut self) {
        self.cancel_active();
        if let Some(pending_request) = self.pending_worker_request.take() {
            pending_request
                .cancellation_token
                .store(true, Ordering::Release);
        }
        self.next_generation();
        self.clear_prepared_output();
        self.latest_encode_keys.clear();
        self.latest_composition_revisions.clear();
        self.pending_kitty_images_by_content_id.clear();
        self.is_ready = true;
    }

    fn cancel_active(&mut self) {
        if let Some(active_job) = self.active_job.as_ref() {
            active_job.cancellation_token.store(true, Ordering::Release);
        }
    }
}

impl Drop for ImageOutputState {
    fn drop(&mut self) {
        if let Some(active_job) = self.active_job.take() {
            active_job.cancellation_token.store(true, Ordering::Release);
        }
        if let Some(pending_request) = self.pending_worker_request.take() {
            pending_request
                .cancellation_token
                .store(true, Ordering::Release);
        }
        self.worker_messages.take();
        self.worker_request_sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn is_output_paint_sequence_equal(
    committed_output_paints: &[OutputPaint],
    next_output_paints: &[OutputPaint],
) -> bool {
    committed_output_paints.len() == next_output_paints.len()
        && committed_output_paints.iter().zip(next_output_paints).all(
            |(committed_output_paint, next_output_paint)| {
                committed_output_paint.placement_key == next_output_paint.placement_key
                    && committed_output_paint.image_content_id == next_output_paint.image_content_id
                    && Arc::ptr_eq(
                        &committed_output_paint.image_record.image,
                        &next_output_paint.image_record.image,
                    )
                    && committed_output_paint.image_record.display.sixel_background
                        == next_output_paint.image_record.display.sixel_background
                    && committed_output_paint.target_area == next_output_paint.target_area
                    && committed_output_paint.source_rect == next_output_paint.source_rect
                    && committed_output_paint.cell_pixel_offset_x
                        == next_output_paint.cell_pixel_offset_x
                    && committed_output_paint.cell_pixel_offset_y
                        == next_output_paint.cell_pixel_offset_y
                    && committed_output_paint.z_index == next_output_paint.z_index
            },
        )
}

fn build_output_encode_key(
    output_kind: ImageOutputKind,
    pixel_cell_size: PixelCellSize,
    output_paint: &OutputPaint,
) -> EncodeKey {
    EncodeKey {
        image_content_id: output_paint.image_content_id,
        image_memory_address: Arc::as_ptr(&output_paint.image_record.image) as usize,
        source_rect: ImageSourceKey::from_source_rect(output_paint.source_rect),
        target_column_count: output_paint.target_area.width,
        target_row_count: output_paint.target_area.height,
        pixel_cell_size,
        output_kind,
        z_index: output_paint.z_index,
        has_terminal_background: matches!(
            output_paint.image_record.display.sixel_background,
            Some(SixelBackground::Terminal)
        ),
        composition_revision: 0,
        composition_pixel_cell_size: None,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AlphaStats {
    has_zero: bool,
    has_partial: bool,
}

impl AlphaStats {
    fn is_fully_opaque(self) -> bool {
        !self.has_zero && !self.has_partial
    }
}

fn compute_alpha_stats(
    decoded_image: &DecodedImage,
    source_rect: ImageSourceRect,
) -> Option<AlphaStats> {
    let image_pixel_width = usize::try_from(decoded_image.pixel_width).ok()?;
    let image_pixel_height = usize::try_from(decoded_image.pixel_height).ok()?;
    validate_image_dimensions(
        GraphicsProtocol::Iterm2,
        image_pixel_width,
        image_pixel_height,
    )
    .ok()?;
    let expected_rgba_byte_count = compute_rgba_byte_count(
        GraphicsProtocol::Iterm2,
        image_pixel_width,
        image_pixel_height,
    )
    .ok()?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count
        || source_rect.pixel_width == 0
        || source_rect.pixel_height == 0
        || source_rect.pixel_x.checked_add(source_rect.pixel_width)? > decoded_image.pixel_width
        || source_rect.pixel_y.checked_add(source_rect.pixel_height)? > decoded_image.pixel_height
    {
        return None;
    }
    let mut stats = AlphaStats::default();
    for pixel_row in source_rect.pixel_y..source_rect.pixel_y + source_rect.pixel_height {
        let row_start = usize::try_from(pixel_row)
            .ok()?
            .checked_mul(image_pixel_width)?;
        for pixel_column in source_rect.pixel_x..source_rect.pixel_x + source_rect.pixel_width {
            let rgba_byte_index = row_start
                .checked_add(usize::try_from(pixel_column).ok()?)?
                .checked_mul(4)?;
            let alpha_value = *decoded_image.rgba_bytes.get(rgba_byte_index + 3)?;
            match alpha_value {
                0 => stats.has_zero = true,
                255 => {}
                _ => stats.has_partial = true,
            }
        }
    }
    Some(stats)
}

#[derive(Debug, Clone, Copy)]
struct CompositionInfo {
    has_glyph: bool,
    has_non_default_background: bool,
    has_known_per_cell_backgrounds: bool,
    solid_background_color: Option<[u8; 3]>,
    is_default_blank: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ItermComposition {
    background_color: Option<[u8; 3]>,
    uses_per_cell_composition: bool,
    has_known_per_cell_backgrounds: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct SixelComposition {
    background_color: Option<[u8; 3]>,
    has_alpha: bool,
    has_terminal_background: bool,
}

#[derive(Debug, Clone, Copy)]
enum BackgroundAccumulator {
    Initial,
    Uniform([u8; 3]),
    Incompatible,
}

fn cell_has_glyph(cell: Option<&ImageCellState>) -> bool {
    let Some(cell) = cell else {
        return true;
    };
    cell.character != ' ' || cell.cell_width != 1 || !cell.combining_characters.is_empty()
}

fn compute_composition_info(
    cell_snapshot: &ImageCellSnapshot,
    output_paint: &OutputPaint,
) -> CompositionInfo {
    let mut has_glyph = false;
    let mut has_non_default_background = false;
    let mut has_known_per_cell_backgrounds = true;
    let mut is_default_blank = true;
    let mut background_accumulator = BackgroundAccumulator::Initial;
    for row_offset in 0..output_paint.target_area.height {
        for column_offset in 0..output_paint.target_area.width {
            let cell_column = output_paint.target_area.x.saturating_add(column_offset);
            let cell_row = output_paint.target_area.y.saturating_add(row_offset);
            let Some(cell) = cell_snapshot.find_cell(cell_column, cell_row) else {
                has_glyph = true;
                has_non_default_background = true;
                has_known_per_cell_backgrounds = false;
                is_default_blank = false;
                background_accumulator = BackgroundAccumulator::Incompatible;
                continue;
            };
            has_non_default_background |=
                cell.style.get_background_color() != koshi_terminal::style::Color::Default;
            is_default_blank &= cell == &ImageCellState::default();
            let glyph = cell_has_glyph(Some(cell));
            has_glyph |= glyph;
            if cell == &ImageCellState::default() {
                background_accumulator = BackgroundAccumulator::Incompatible;
                continue;
            }
            if glyph || cell.style.get_attributes() != Default::default() {
                has_known_per_cell_backgrounds = false;
                background_accumulator = BackgroundAccumulator::Incompatible;
                continue;
            }
            let koshi_terminal::style::Color::Rgb(red, green, blue) =
                cell.style.get_background_color()
            else {
                has_known_per_cell_backgrounds = false;
                background_accumulator = BackgroundAccumulator::Incompatible;
                continue;
            };
            let background_color = [red, green, blue];
            background_accumulator = match background_accumulator {
                BackgroundAccumulator::Initial => BackgroundAccumulator::Uniform(background_color),
                BackgroundAccumulator::Uniform(previous_color)
                    if previous_color == background_color =>
                {
                    BackgroundAccumulator::Uniform(previous_color)
                }
                BackgroundAccumulator::Uniform(_) | BackgroundAccumulator::Incompatible => {
                    BackgroundAccumulator::Incompatible
                }
            }
        }
    }
    CompositionInfo {
        has_glyph,
        has_non_default_background,
        has_known_per_cell_backgrounds,
        solid_background_color: match background_accumulator {
            BackgroundAccumulator::Uniform(background_color) => Some(background_color),
            BackgroundAccumulator::Initial | BackgroundAccumulator::Incompatible => None,
        },
        is_default_blank,
    }
}

#[derive(Debug)]
struct Plan<'a> {
    output_paint: &'a OutputPaint,
    encode_key: EncodeKey,
    iterm_composition: ItermComposition,
    sixel_composition: SixelComposition,
    image_compatibility: ImageCompatibility,
    is_opaque: bool,
}

fn has_known_cell_background(
    cell_snapshot: &ImageCellSnapshot,
    output_paint: &OutputPaint,
) -> bool {
    (0..output_paint.target_area.height).all(|row_offset| {
        (0..output_paint.target_area.width).all(|column_offset| {
            let cell_column = output_paint.target_area.x.saturating_add(column_offset);
            let cell_row = output_paint.target_area.y.saturating_add(row_offset);
            let Some(cell) = cell_snapshot.find_cell(cell_column, cell_row) else {
                return false;
            };
            matches!(
                cell.style.get_background_color(),
                koshi_terminal::style::Color::Rgb(..)
            ) && cell.character == ' '
                && cell.cell_width == 1
                && cell.combining_characters.is_empty()
                && cell.style.get_attributes() == Default::default()
        })
    })
}

fn classify_output_paint<'a>(
    output_kind: ImageOutputKind,
    cell_snapshot: &ImageCellSnapshot,
    covered_cell_positions: &HashSet<(u16, u16)>,
    output_paint: &'a OutputPaint,
    encode_key: EncodeKey,
) -> Result<Plan<'a>, TemplateError> {
    let alpha_stats = output_paint.alpha_stats.ok_or(TemplateError::Failed)?;
    let composition_details = compute_composition_info(cell_snapshot, output_paint);
    let is_overlapping_lower =
        has_covered_target_cell(output_paint.target_area, covered_cell_positions);
    let has_known_cell_background = has_known_cell_background(cell_snapshot, output_paint);
    let mut image_compatibility = ImageCompatibility::exact();
    image_compatibility.has_text_layer_order_mismatch = (output_paint.z_index < 0
        && composition_details.has_glyph)
        || (output_paint.z_index < KITTY_BACKGROUND_LAYER_Z_INDEX
            && composition_details.has_non_default_background);
    match output_kind {
        ImageOutputKind::Kitty => Ok(Plan {
            output_paint,
            encode_key,
            iterm_composition: ItermComposition::default(),
            sixel_composition: SixelComposition::default(),
            image_compatibility,
            is_opaque: alpha_stats.is_fully_opaque(),
        }),
        ImageOutputKind::Iterm => {
            let has_alpha = !alpha_stats.is_fully_opaque();
            let mut iterm_composition = ItermComposition::default();
            if has_alpha {
                if composition_details.has_glyph {
                    image_compatibility.has_iterm_alpha_mismatch = true;
                } else if is_overlapping_lower
                    || (!composition_details.is_default_blank
                        && composition_details.solid_background_color.is_none())
                {
                    image_compatibility.has_iterm_alpha_mismatch = true;
                    iterm_composition.uses_per_cell_composition = true;
                    iterm_composition.has_known_per_cell_backgrounds =
                        composition_details.has_known_per_cell_backgrounds;
                } else if let Some(background_color) = composition_details.solid_background_color {
                    iterm_composition.background_color = Some(background_color);
                } else if !composition_details.is_default_blank {
                    image_compatibility.has_iterm_alpha_mismatch = true;
                }
            }
            Ok(Plan {
                output_paint,
                encode_key,
                iterm_composition,
                sixel_composition: SixelComposition::default(),
                image_compatibility,
                is_opaque: alpha_stats.is_fully_opaque()
                    || iterm_composition.background_color.is_some(),
            })
        }
        ImageOutputKind::Sixel { .. } => {
            let has_terminal_background = output_paint.image_record.display.sixel_background
                == Some(SixelBackground::Terminal);
            let background_color = if !is_overlapping_lower
                && composition_details.solid_background_color.is_some()
                && ((alpha_stats.has_partial && !alpha_stats.has_zero)
                    || (alpha_stats.has_zero && has_terminal_background))
            {
                composition_details.solid_background_color
            } else {
                None
            };
            let has_alpha = alpha_stats.has_partial && background_color.is_none();
            let has_terminal_background =
                alpha_stats.has_zero && has_terminal_background && background_color.is_none();
            image_compatibility.has_sixel_alpha_mismatch = has_alpha && !has_known_cell_background;
            image_compatibility.has_sixel_terminal_background_mismatch =
                has_terminal_background && !has_known_cell_background;
            Ok(Plan {
                output_paint,
                encode_key,
                iterm_composition: ItermComposition::default(),
                sixel_composition: SixelComposition {
                    background_color,
                    has_alpha,
                    has_terminal_background,
                },
                image_compatibility,
                is_opaque: background_color.is_some()
                    || (!alpha_stats.has_zero && !alpha_stats.has_partial)
                    || (has_known_cell_background && (has_alpha || has_terminal_background)),
            })
        }
    }
}

fn lower_images_cover_target(output_paint: &OutputPaint, lower_plans: &[Plan<'_>]) -> bool {
    let mut covered_cell_positions = HashSet::new();
    for lower_plan in lower_plans {
        if !lower_plan.is_opaque || lower_plan.image_compatibility != ImageCompatibility::default()
        {
            continue;
        }
        let left_column = output_paint
            .target_area
            .x
            .max(lower_plan.output_paint.target_area.x);
        let top_row_index = output_paint
            .target_area
            .y
            .max(lower_plan.output_paint.target_area.y);
        let right_column = output_paint
            .target_area
            .right()
            .min(lower_plan.output_paint.target_area.right());
        let bottom_row_index = output_paint
            .target_area
            .bottom()
            .min(lower_plan.output_paint.target_area.bottom());
        for row_index in top_row_index..bottom_row_index {
            for column_index in left_column..right_column {
                covered_cell_positions.insert((column_index, row_index));
            }
        }
    }
    !((output_paint.target_area.y..output_paint.target_area.bottom()).any(|row_index| {
        (output_paint.target_area.x..output_paint.target_area.right())
            .any(|column_index| !covered_cell_positions.contains(&(column_index, row_index)))
    }))
}

fn resolve_image_compatibility(
    output_kind: ImageOutputKind,
    measured_pixel_cell_size: Option<PixelCellSize>,
    image_plans: &mut [Plan<'_>],
) {
    for plan_index in 0..image_plans.len() {
        let is_covered_by_lower_images = lower_images_cover_target(
            image_plans[plan_index].output_paint,
            &image_plans[..plan_index],
        );
        if matches!(output_kind, ImageOutputKind::Iterm) {
            if image_plans[plan_index]
                .iterm_composition
                .uses_per_cell_composition
                && image_plans[plan_index]
                    .iterm_composition
                    .has_known_per_cell_backgrounds
                && measured_pixel_cell_size.is_some()
            {
                image_plans[plan_index]
                    .image_compatibility
                    .has_iterm_alpha_mismatch = false;
            }
            continue;
        }
        let has_unavailable_lower_image = image_plans[..plan_index].iter().any(|lower_plan| {
            lower_plan.image_compatibility != ImageCompatibility::default()
                && lower_plan
                    .output_paint
                    .target_area
                    .intersection(image_plans[plan_index].output_paint.target_area)
                    .width
                    > 0
                && lower_plan
                    .output_paint
                    .target_area
                    .intersection(image_plans[plan_index].output_paint.target_area)
                    .height
                    > 0
        });
        if is_covered_by_lower_images {
            let image_plan = &mut image_plans[plan_index];
            image_plan.image_compatibility.has_sixel_alpha_mismatch = false;
        } else if has_unavailable_lower_image && image_plans[plan_index].sixel_composition.has_alpha
        {
            image_plans[plan_index]
                .image_compatibility
                .has_sixel_alpha_mismatch = true;
        }
    }
}

fn has_covered_target_cell(
    target_area: Rect,
    covered_cell_positions: &HashSet<(u16, u16)>,
) -> bool {
    (target_area.y..target_area.bottom()).any(|row_index| {
        (target_area.x..target_area.right())
            .any(|column_index| covered_cell_positions.contains(&(column_index, row_index)))
    })
}

fn is_rectangles_overlapping(left_rect: Rect, right_rect: Rect) -> bool {
    left_rect.x < right_rect.right()
        && right_rect.x < left_rect.right()
        && left_rect.y < right_rect.bottom()
        && right_rect.y < left_rect.bottom()
}

fn add_target_area_cells(target_area: Rect, covered_cell_positions: &mut HashSet<(u16, u16)>) {
    for row_index in target_area.y..target_area.bottom() {
        for column_index in target_area.x..target_area.right() {
            covered_cell_positions.insert((column_index, row_index));
        }
    }
}

fn worker_loop(
    worker_request_receiver: Receiver<WorkerRequest>,
    worker_message_sender: SyncSender<WorkerMessage>,
) {
    while let Ok(worker_request) = worker_request_receiver.recv() {
        if worker_request.cancellation_token.load(Ordering::Acquire) {
            let _ = worker_message_sender.send(WorkerMessage::Finished {
                frame_generation: worker_request.frame_generation,
                has_failed: false,
            });
            continue;
        }
        let frame_generation = worker_request.frame_generation;
        let has_failed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_worker_job(&worker_request, &worker_message_sender)
        })) {
            Ok(job_result) => job_result.is_err(),
            Err(_) => true,
        };
        let _ = worker_message_sender.send(WorkerMessage::Finished {
            frame_generation,
            has_failed,
        });
    }
}

fn run_worker_job(
    worker_request: &WorkerRequest,
    worker_message_sender: &SyncSender<WorkerMessage>,
) -> Result<(), ()> {
    run_worker_job_with_limit(
        worker_request,
        worker_message_sender,
        MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
    )
}

fn run_worker_job_with_limit(
    worker_request: &WorkerRequest,
    worker_message_sender: &SyncSender<WorkerMessage>,
    output_byte_limit: usize,
) -> Result<(), ()> {
    if matches!(worker_request.output_kind, ImageOutputKind::Kitty) {
        return run_kitty_worker_job(worker_request, worker_message_sender);
    }
    let cell_snapshot = worker_request.cell_snapshot.as_deref().ok_or(())?;
    let mut image_plans = Vec::new();
    let mut covered_cell_positions = HashSet::new();
    if image_plans
        .try_reserve_exact(worker_request.output_paints.len())
        .is_err()
    {
        return Err(());
    }
    for (output_paint, encode_key) in worker_request
        .output_paints
        .iter()
        .zip(&worker_request.encode_keys)
    {
        if worker_request.cancellation_token.load(Ordering::Acquire) {
            return Ok(());
        }
        let image_plan = classify_output_paint(
            worker_request.output_kind,
            cell_snapshot,
            &covered_cell_positions,
            output_paint,
            *encode_key,
        )
        .map_err(|_| ())?;
        add_target_area_cells(output_paint.target_area, &mut covered_cell_positions);
        image_plans.push(image_plan);
    }
    resolve_image_compatibility(
        worker_request.output_kind,
        worker_request.measured_pixel_cell_size,
        &mut image_plans,
    );

    let mut encoded_templates: Vec<Option<Vec<TemplateUnit>>> = Vec::new();
    let mut template_index_by_encode_key = HashMap::new();
    let mut template_byte_count = 0usize;
    let mut emitted_byte_count = 0usize;
    for (plan_index, image_plan) in image_plans.iter().enumerate() {
        if worker_request.cancellation_token.load(Ordering::Acquire) {
            return Ok(());
        }
        if image_plan.image_compatibility != ImageCompatibility::default() {
            send_worker_message(
                worker_message_sender,
                &worker_request.cancellation_token,
                WorkerMessage::Unavailable {
                    frame_generation: worker_request.frame_generation,
                    placement_key: image_plan.output_paint.placement_key,
                },
            )?;
            continue;
        }
        let template_index = if let Some(template_index) =
            template_index_by_encode_key.get(&image_plan.encode_key)
        {
            *template_index
        } else {
            let remaining_output_bytes = output_byte_limit
                .checked_sub(template_byte_count)
                .ok_or(())?;
            let encoded_template = match encode_output_template(
                worker_request,
                image_plan,
                &image_plans[..plan_index],
                remaining_output_bytes,
            ) {
                Ok(template_units) => {
                    let encoded_byte_count = template_units
                        .iter()
                        .try_fold(0usize, |total, template_unit| {
                            total.checked_add(template_unit.output_bytes.len())
                        });
                    let encoded_byte_count = encoded_byte_count
                        .filter(|byte_count| *byte_count <= remaining_output_bytes)
                        .ok_or(())?;
                    template_byte_count = template_byte_count
                        .checked_add(encoded_byte_count)
                        .ok_or(())?;
                    Some(template_units)
                }
                Err(TemplateError::Unavailable)
                    if worker_request.cancellation_token.load(Ordering::Acquire) =>
                {
                    return Ok(())
                }
                Err(TemplateError::Unavailable) => None,
                Err(TemplateError::Failed) => return Err(()),
            };
            let template_index = encoded_templates.len();
            encoded_templates.push(encoded_template);
            template_index_by_encode_key.insert(image_plan.encode_key, template_index);
            template_index
        };
        let Some(template_units) = &encoded_templates[template_index] else {
            send_worker_message(
                worker_message_sender,
                &worker_request.cancellation_token,
                WorkerMessage::Unavailable {
                    frame_generation: worker_request.frame_generation,
                    placement_key: image_plan.output_paint.placement_key,
                },
            )?;
            continue;
        };
        let encoded_byte_count = template_units
            .iter()
            .try_fold(0usize, |total, template_unit| {
                total.checked_add(template_unit.output_bytes.len())
            });
        let encoded_byte_count = encoded_byte_count.ok_or(())?;
        emitted_byte_count = emitted_byte_count
            .checked_add(encoded_byte_count)
            .filter(|byte_count| *byte_count <= output_byte_limit)
            .ok_or(())?;
        send_worker_message(
            worker_message_sender,
            &worker_request.cancellation_token,
            WorkerMessage::Prepared {
                frame_generation: worker_request.frame_generation,
                placement_key: image_plan.output_paint.placement_key,
                image_compatibility: image_plan.image_compatibility,
            },
        )?;
        for template_unit in template_units {
            send_worker_message(
                worker_message_sender,
                &worker_request.cancellation_token,
                WorkerMessage::Unit(OutputUnit {
                    frame_generation: worker_request.frame_generation,
                    placement_key: image_plan.output_paint.placement_key,
                    output_kind: worker_request.output_kind,
                    tile_offset: template_unit.tile_offset,
                    output_bytes: Arc::clone(&template_unit.output_bytes),
                }),
            )?;
        }
    }
    Ok(())
}

fn run_kitty_worker_job(
    worker_request: &WorkerRequest,
    worker_message_sender: &SyncSender<WorkerMessage>,
) -> Result<(), ()> {
    if worker_request.kitty_paint_images.len() != worker_request.output_paints.len() {
        return Err(());
    }
    let mut bounded_output =
        BoundedOutput::from_output_byte_limit(MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT);
    for (output_paint, kitty_paint_image) in worker_request
        .output_paints
        .iter()
        .zip(&worker_request.kitty_paint_images)
    {
        if !kitty_paint_image.should_transmit_image {
            continue;
        }
        let mut kitty_upload = KittyUpload::from_decoded_image(
            Arc::clone(&output_paint.image_record.image),
            kitty_paint_image.kitty_image_number,
        )
        .map_err(|_| ())?;
        while !kitty_upload.is_upload_complete() {
            if worker_request.cancellation_token.load(Ordering::Acquire) {
                return Ok(());
            }
            kitty_upload
                .advance_upload(&mut bounded_output)
                .map_err(|_| ())?;
        }
    }
    for (paint_index, (output_paint, kitty_paint_image)) in worker_request
        .output_paints
        .iter()
        .zip(&worker_request.kitty_paint_images)
        .enumerate()
    {
        if worker_request.cancellation_token.load(Ordering::Acquire) {
            return Ok(());
        }
        write_cursor_position(
            &mut bounded_output,
            output_paint.target_area.x,
            output_paint.target_area.y,
        )
        .map_err(|_| ())?;
        let placement = build_kitty_placement(
            kitty_paint_image.kitty_image_number,
            u32::try_from(paint_index + 1).map_err(|_| ())?,
            output_paint,
        );
        write_kitty_placement(
            &mut bounded_output,
            &output_paint.image_record.image,
            &placement,
        )
        .map_err(|_| ())?;
    }
    for output_paint in &worker_request.output_paints {
        send_worker_message(
            worker_message_sender,
            &worker_request.cancellation_token,
            WorkerMessage::Prepared {
                frame_generation: worker_request.frame_generation,
                placement_key: output_paint.placement_key,
                image_compatibility: ImageCompatibility::exact(),
            },
        )?;
    }
    let first_output_paint = worker_request.output_paints.first().ok_or(())?;
    send_worker_message(
        worker_message_sender,
        &worker_request.cancellation_token,
        WorkerMessage::Unit(OutputUnit {
            frame_generation: worker_request.frame_generation,
            placement_key: first_output_paint.placement_key,
            output_kind: worker_request.output_kind,
            tile_offset: (0, 0),
            output_bytes: bounded_output.into_output_bytes().into(),
        }),
    )
}

struct BoundedOutput {
    output_bytes: Vec<u8>,
    output_byte_limit: usize,
}

impl BoundedOutput {
    fn from_output_byte_limit(output_byte_limit: usize) -> Self {
        Self {
            output_bytes: Vec::new(),
            output_byte_limit,
        }
    }

    fn into_output_bytes(self) -> Vec<u8> {
        self.output_bytes
    }
}

impl Write for BoundedOutput {
    fn write(&mut self, chunk_bytes: &[u8]) -> io::Result<usize> {
        let next_output_byte_count = self
            .output_bytes
            .len()
            .checked_add(chunk_bytes.len())
            .filter(|byte_count| *byte_count <= self.output_byte_limit)
            .ok_or_else(|| invalid_output("native image frame exceeds its output limit"))?;
        self.output_bytes
            .try_reserve(next_output_byte_count - self.output_bytes.len())
            .map_err(|_| invalid_output("native image output storage could not be allocated"))?;
        self.output_bytes.extend_from_slice(chunk_bytes);
        Ok(chunk_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn build_kitty_placement(
    kitty_image_number: u32,
    kitty_placement_id: u32,
    output_paint: &OutputPaint,
) -> KittyPlacement {
    KittyPlacement {
        image_number: kitty_image_number,
        placement_id: kitty_placement_id,
        source_x_pixels: output_paint.source_rect.pixel_x,
        source_y_pixels: output_paint.source_rect.pixel_y,
        source_width_pixels: output_paint.source_rect.pixel_width,
        source_height_pixels: output_paint.source_rect.pixel_height,
        column_count: u32::from(output_paint.target_area.width),
        row_count: u32::from(output_paint.target_area.height),
        cell_pixel_offset_x: output_paint.cell_pixel_offset_x,
        cell_pixel_offset_y: output_paint.cell_pixel_offset_y,
        z_index: output_paint.z_index,
    }
}

fn convert_kitty_output_error(kitty_output_error: KittyOutputError) -> io::Error {
    match kitty_output_error {
        KittyOutputError::Io(io_error) => io_error,
        other_error => io::Error::new(io::ErrorKind::InvalidData, other_error),
    }
}

fn send_worker_message(
    worker_message_sender: &SyncSender<WorkerMessage>,
    cancellation_token: &AtomicBool,
    worker_message: WorkerMessage,
) -> Result<(), ()> {
    if cancellation_token.load(Ordering::Acquire) {
        return Err(());
    }
    worker_message_sender.send(worker_message).map_err(|_| ())
}

#[derive(Debug)]
enum TemplateError {
    Unavailable,
    Failed,
}

#[derive(Debug)]
struct TemplateUnit {
    tile_offset: (u16, u16),
    output_bytes: Arc<[u8]>,
}

fn encode_output_template(
    worker_request: &WorkerRequest,
    image_plan: &Plan<'_>,
    lower_plans: &[Plan<'_>],
    output_byte_limit: usize,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    match worker_request.output_kind {
        ImageOutputKind::Kitty => Err(TemplateError::Failed),
        ImageOutputKind::Iterm => {
            encode_iterm_template(worker_request, image_plan, lower_plans, output_byte_limit)
        }
        ImageOutputKind::Sixel {
            palette_color_count,
            max_pixel_width,
            max_pixel_height,
        } => encode_sixel_template(
            worker_request,
            image_plan,
            lower_plans,
            palette_color_count,
            max_pixel_width,
            max_pixel_height,
            output_byte_limit,
        ),
    }
}

fn encode_iterm_template(
    worker_request: &WorkerRequest,
    image_plan: &Plan<'_>,
    lower_plans: &[Plan<'_>],
    output_byte_limit: usize,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    let decoded_image = if image_plan.iterm_composition.uses_per_cell_composition {
        compose_iterm_image(worker_request, image_plan, lower_plans)?
    } else {
        crop_output_image(
            image_plan.output_paint,
            image_plan.iterm_composition.background_color,
        )
        .map_err(|_| TemplateError::Failed)?
    };
    let options = ItermOutputOptions::from_cell_dimensions(
        u32::from(image_plan.output_paint.target_area.width),
        u32::from(image_plan.output_paint.target_area.height),
    )
    .map_err(|_| TemplateError::Failed)?;
    let mut encoder =
        ItermEncoder::from_image(&decoded_image, options).map_err(|_| TemplateError::Failed)?;
    let mut template_units = Vec::new();
    let mut output_byte_count = 0usize;
    while let Some(packet_bytes) = encoder.take_next_packet() {
        if packet_bytes.len() > MAX_ITERM_PACKET_BYTE_COUNT {
            return Err(TemplateError::Failed);
        }
        output_byte_count = compute_checked_output_byte_count(
            output_byte_count,
            packet_bytes.len(),
            output_byte_limit,
        )?;
        let mut output_bytes = Vec::new();
        output_bytes
            .try_reserve_exact(packet_bytes.len())
            .map_err(|_| TemplateError::Failed)?;
        output_bytes.extend_from_slice(packet_bytes);
        template_units.push(TemplateUnit {
            tile_offset: (0, 0),
            output_bytes: output_bytes.into(),
        });
    }
    Ok(template_units)
}

fn compute_checked_output_byte_count(
    current_output_byte_count: usize,
    additional_output_byte_count: usize,
    output_byte_limit: usize,
) -> Result<usize, TemplateError> {
    current_output_byte_count
        .checked_add(additional_output_byte_count)
        .filter(|output_byte_count| *output_byte_count <= output_byte_limit)
        .ok_or(TemplateError::Failed)
}

fn compose_iterm_image(
    worker_request: &WorkerRequest,
    image_plan: &Plan<'_>,
    lower_plans: &[Plan<'_>],
) -> Result<Arc<DecodedImage>, TemplateError> {
    let measured_pixel_cell_size = worker_request
        .measured_pixel_cell_size
        .ok_or(TemplateError::Failed)?;
    let cell_pixel_width = u32::from(measured_pixel_cell_size.get_pixel_width());
    let cell_pixel_height = u32::from(measured_pixel_cell_size.get_pixel_height());
    let scaled_image = scale_output_tile(
        &image_plan.output_paint.image_record.image,
        image_plan.output_paint.source_rect,
        image_plan.output_paint.target_area,
        TileRect {
            column_offset: 0,
            row_offset: 0,
            column_count: image_plan.output_paint.target_area.width,
            row_count: image_plan.output_paint.target_area.height,
        },
        cell_pixel_width,
        cell_pixel_height,
        None,
    )
    .map_err(|_| TemplateError::Failed)?;
    let mut output_rgba = scaled_image.rgba_bytes.clone();
    let output_pixel_width =
        usize::try_from(scaled_image.pixel_width).map_err(|_| TemplateError::Failed)?;
    for output_pixel_row in 0..scaled_image.pixel_height {
        for output_pixel_column in 0..scaled_image.pixel_width {
            let global_pixel_x = u32::from(image_plan.output_paint.target_area.x)
                .checked_mul(cell_pixel_width)
                .and_then(|pixel_origin_x| pixel_origin_x.checked_add(output_pixel_column))
                .ok_or(TemplateError::Failed)?;
            let global_pixel_y = u32::from(image_plan.output_paint.target_area.y)
                .checked_mul(cell_pixel_height)
                .and_then(|pixel_origin_y| pixel_origin_y.checked_add(output_pixel_row))
                .ok_or(TemplateError::Failed)?;
            let mut underlying_pixel =
                get_iterm_cell_background(worker_request, global_pixel_x, global_pixel_y)
                    .ok_or(TemplateError::Unavailable)?;
            for lower_plan in lower_plans {
                if let Some(lower_image_pixel) = sample_scaled_pixel(
                    measured_pixel_cell_size,
                    lower_plan,
                    global_pixel_x,
                    global_pixel_y,
                ) {
                    underlying_pixel = blend_pixel(
                        lower_image_pixel,
                        [
                            underlying_pixel[0],
                            underlying_pixel[1],
                            underlying_pixel[2],
                        ],
                        underlying_pixel[3],
                    );
                }
            }
            let rgba_byte_index = (usize::try_from(output_pixel_row)
                .map_err(|_| TemplateError::Failed)?
                * output_pixel_width
                + usize::try_from(output_pixel_column).map_err(|_| TemplateError::Failed)?)
                * 4;
            let image_pixel: [u8; 4] = output_rgba[rgba_byte_index..rgba_byte_index + 4]
                .try_into()
                .map_err(|_| TemplateError::Failed)?;
            let composited_pixel = blend_pixel(
                image_pixel,
                [
                    underlying_pixel[0],
                    underlying_pixel[1],
                    underlying_pixel[2],
                ],
                underlying_pixel[3],
            );
            output_rgba[rgba_byte_index..rgba_byte_index + 4].copy_from_slice(&composited_pixel);
        }
    }
    Ok(Arc::new(DecodedImage {
        pixel_width: scaled_image.pixel_width,
        pixel_height: scaled_image.pixel_height,
        rgba_bytes: output_rgba,
    }))
}

fn get_iterm_cell_background(
    worker_request: &WorkerRequest,
    pixel_x: u32,
    pixel_y: u32,
) -> Option<[u8; 4]> {
    let pixel_cell_size = worker_request.measured_pixel_cell_size?;
    let cell_x = u16::try_from(pixel_x / u32::from(pixel_cell_size.get_pixel_width())).ok()?;
    let cell_y = u16::try_from(pixel_y / u32::from(pixel_cell_size.get_pixel_height())).ok()?;
    let cell = worker_request
        .cell_snapshot
        .as_deref()?
        .find_cell(cell_x, cell_y)?;
    if cell == &ImageCellState::default() {
        return Some([0, 0, 0, 0]);
    }
    if cell_has_glyph(Some(cell)) || cell.style.get_attributes() != Default::default() {
        return None;
    }
    let koshi_terminal::style::Color::Rgb(red, green, blue) = cell.style.get_background_color()
    else {
        return None;
    };
    Some([red, green, blue, 255])
}

#[derive(Debug, Clone, Copy)]
struct TileRect {
    column_offset: u16,
    row_offset: u16,
    column_count: u16,
    row_count: u16,
}

fn encode_sixel_template(
    worker_request: &WorkerRequest,
    image_plan: &Plan<'_>,
    lower_plans: &[Plan<'_>],
    palette_color_count: usize,
    max_pixel_width: Option<u32>,
    max_pixel_height: Option<u32>,
    output_byte_limit: usize,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    let cropped_image =
        crop_output_image(image_plan.output_paint, None).map_err(|_| TemplateError::Failed)?;
    let encode_options = SixelEncodeOptions::with_maximum_palette_color_count(
        palette_color_count.clamp(MIN_PALETTE_COLOR_COUNT, MAX_PALETTE_COLOR_COUNT),
    );
    let palette = PreparedSixelPalette::prepare(&cropped_image, [0, 0, 0], encode_options)
        .map_err(|_| TemplateError::Failed)?;
    let cell_pixel_width = u32::from(worker_request.pixel_cell_size.get_pixel_width());
    let cell_pixel_height = u32::from(worker_request.pixel_cell_size.get_pixel_height());
    let max_columns = max_pixel_width
        .map(|pixel_width| pixel_width / cell_pixel_width)
        .unwrap_or(u32::from(image_plan.output_paint.target_area.width));
    let max_rows = max_pixel_height
        .map(|pixel_height| pixel_height / cell_pixel_height)
        .unwrap_or(u32::from(image_plan.output_paint.target_area.height));
    let max_columns = max_columns.min(u32::from(image_plan.output_paint.target_area.width));
    let max_rows = max_rows.min(u32::from(image_plan.output_paint.target_area.height));
    if max_columns == 0 || max_rows == 0 {
        return Err(TemplateError::Failed);
    }
    let mut template_units = Vec::new();
    let tile_column_count = u16::try_from(max_columns).map_err(|_| TemplateError::Failed)?;
    let tile_row_count = u16::try_from(max_rows).map_err(|_| TemplateError::Failed)?;
    let target_column_count = image_plan.output_paint.target_area.width;
    let target_row_count = image_plan.output_paint.target_area.height;
    let mut template_output_byte_count = 0usize;
    let mut row_offset = 0;
    while row_offset < target_row_count {
        let row_count = tile_row_count.min(target_row_count - row_offset);
        let mut column_offset = 0;
        while column_offset < target_column_count {
            let column_count = tile_column_count.min(target_column_count - column_offset);
            append_sixel_tiles(
                worker_request,
                image_plan,
                &cropped_image,
                lower_plans,
                palette_color_count,
                &palette,
                cell_pixel_width,
                cell_pixel_height,
                TileRect {
                    column_offset,
                    row_offset,
                    column_count,
                    row_count,
                },
                &mut template_units,
                &mut template_output_byte_count,
                output_byte_limit,
            )?;
            column_offset = column_offset.saturating_add(column_count);
        }
        row_offset = row_offset.saturating_add(row_count);
    }
    Ok(template_units)
}

#[allow(clippy::too_many_arguments)]
fn append_sixel_tiles(
    worker_request: &WorkerRequest,
    image_plan: &Plan<'_>,
    source_image: &DecodedImage,
    lower_plans: &[Plan<'_>],
    palette_color_count: usize,
    palette: &PreparedSixelPalette,
    cell_pixel_width: u32,
    cell_pixel_height: u32,
    tile: TileRect,
    template_units: &mut Vec<TemplateUnit>,
    template_output_byte_count: &mut usize,
    output_byte_limit: usize,
) -> Result<(), TemplateError> {
    if worker_request.cancellation_token.load(Ordering::Acquire) {
        return Err(TemplateError::Unavailable);
    }
    let tile_image = scale_output_tile(
        source_image,
        ImageSourceRect {
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: source_image.pixel_width,
            pixel_height: source_image.pixel_height,
        },
        image_plan.output_paint.target_area,
        tile,
        cell_pixel_width,
        cell_pixel_height,
        None,
    )
    .map_err(|_| TemplateError::Failed)?;
    let should_compose = image_plan.sixel_composition.background_color.is_some()
        || image_plan.sixel_composition.has_alpha
        || image_plan.sixel_composition.has_terminal_background;
    let tile_image = if should_compose {
        compose_sixel_tile(worker_request, image_plan, lower_plans, tile, tile_image)?
    } else {
        tile_image
    };
    let mut encoder = if should_compose {
        SixelEncoder::with_options(
            Arc::clone(&tile_image),
            [0, 0, 0],
            SixelEncodeOptions::with_maximum_palette_color_count(
                palette_color_count.clamp(MIN_PALETTE_COLOR_COUNT, MAX_PALETTE_COLOR_COUNT),
            ),
        )
    } else {
        SixelEncoder::with_palette(tile_image, [0, 0, 0], palette.clone())
    }
    .map_err(|_| TemplateError::Failed)?;
    let mut tile_output_bytes = Vec::new();
    while let Some(chunk_bytes) = encoder
        .take_next_chunk(MAX_SIXEL_TILE_BYTE_COUNT)
        .map_err(|_| TemplateError::Failed)?
    {
        if worker_request.cancellation_token.load(Ordering::Acquire) {
            return Err(TemplateError::Unavailable);
        }
        let next_output_byte_count = tile_output_bytes
            .len()
            .checked_add(chunk_bytes.len())
            .filter(|output_byte_count| *output_byte_count <= MAX_SIXEL_OUTPUT_BYTE_COUNT)
            .ok_or(TemplateError::Failed)?;
        compute_checked_output_byte_count(
            *template_output_byte_count,
            next_output_byte_count,
            output_byte_limit,
        )?;
        tile_output_bytes
            .try_reserve(next_output_byte_count - tile_output_bytes.len())
            .map_err(|_| TemplateError::Failed)?;
        tile_output_bytes.extend_from_slice(chunk_bytes);
    }
    if tile_output_bytes.is_empty() {
        return Ok(());
    }
    *template_output_byte_count = compute_checked_output_byte_count(
        *template_output_byte_count,
        tile_output_bytes.len(),
        output_byte_limit,
    )?;
    template_units.push(TemplateUnit {
        tile_offset: (tile.column_offset, tile.row_offset),
        output_bytes: tile_output_bytes.into(),
    });
    Ok(())
}

fn compose_sixel_tile(
    worker_request: &WorkerRequest,
    image_plan: &Plan<'_>,
    lower_plans: &[Plan<'_>],
    tile: TileRect,
    tile_image: Arc<DecodedImage>,
) -> Result<Arc<DecodedImage>, TemplateError> {
    let cell_pixel_width = u32::from(worker_request.pixel_cell_size.get_pixel_width());
    let cell_pixel_height = u32::from(worker_request.pixel_cell_size.get_pixel_height());
    let mut output_rgba = tile_image.rgba_bytes.clone();
    let tile_pixel_width =
        usize::try_from(tile_image.pixel_width).map_err(|_| TemplateError::Failed)?;
    for tile_pixel_row in 0..tile_image.pixel_height {
        for tile_pixel_column in 0..tile_image.pixel_width {
            let global_pixel_x = u32::from(image_plan.output_paint.target_area.x)
                .checked_mul(cell_pixel_width)
                .and_then(|image_origin_x| {
                    u32::from(tile.column_offset)
                        .checked_mul(cell_pixel_width)
                        .and_then(|tile_origin_x| image_origin_x.checked_add(tile_origin_x))
                })
                .and_then(|pixel_origin_x| pixel_origin_x.checked_add(tile_pixel_column))
                .ok_or(TemplateError::Failed)?;
            let global_pixel_y = u32::from(image_plan.output_paint.target_area.y)
                .checked_mul(cell_pixel_height)
                .and_then(|image_origin_y| {
                    u32::from(tile.row_offset)
                        .checked_mul(cell_pixel_height)
                        .and_then(|tile_origin_y| image_origin_y.checked_add(tile_origin_y))
                })
                .and_then(|pixel_origin_y| pixel_origin_y.checked_add(tile_pixel_row))
                .ok_or(TemplateError::Failed)?;
            let (terminal_background_color, _) =
                get_terminal_background(worker_request, global_pixel_x, global_pixel_y);
            let mut underlying_pixel = [
                terminal_background_color[0],
                terminal_background_color[1],
                terminal_background_color[2],
                255,
            ];
            for lower_plan in lower_plans {
                if lower_plan.image_compatibility != ImageCompatibility::default() {
                    continue;
                }
                if let Some(lower_image_pixel) = sample_scaled_pixel(
                    worker_request.pixel_cell_size,
                    lower_plan,
                    global_pixel_x,
                    global_pixel_y,
                ) {
                    underlying_pixel = apply_sixel_layer(
                        lower_plan,
                        lower_image_pixel,
                        terminal_background_color,
                        underlying_pixel,
                    );
                }
            }
            let rgba_byte_index = (usize::try_from(tile_pixel_row)
                .map_err(|_| TemplateError::Failed)?
                * tile_pixel_width
                + usize::try_from(tile_pixel_column).map_err(|_| TemplateError::Failed)?)
                * 4;
            let image_pixel: [u8; 4] = output_rgba[rgba_byte_index..rgba_byte_index + 4]
                .try_into()
                .map_err(|_| TemplateError::Failed)?;
            let composited_pixel = apply_current_sixel_layer(
                image_plan,
                image_pixel,
                terminal_background_color,
                underlying_pixel,
            );
            output_rgba[rgba_byte_index..rgba_byte_index + 4].copy_from_slice(&composited_pixel);
        }
    }
    Ok(Arc::new(DecodedImage {
        pixel_width: tile_image.pixel_width,
        pixel_height: tile_image.pixel_height,
        rgba_bytes: output_rgba,
    }))
}

fn get_terminal_background(
    worker_request: &WorkerRequest,
    pixel_x: u32,
    pixel_y: u32,
) -> ([u8; 3], bool) {
    let cell_pixel_width = u32::from(worker_request.pixel_cell_size.get_pixel_width());
    let cell_pixel_height = u32::from(worker_request.pixel_cell_size.get_pixel_height());
    let cell_column = u16::try_from(pixel_x / cell_pixel_width).ok();
    let cell_row = u16::try_from(pixel_y / cell_pixel_height).ok();
    let Some(cell) = cell_column.and_then(|cell_column| {
        cell_row.and_then(|cell_row| {
            worker_request
                .cell_snapshot
                .as_deref()
                .and_then(|cell_snapshot| cell_snapshot.find_cell(cell_column, cell_row))
        })
    }) else {
        return ([0, 0, 0], false);
    };
    let koshi_terminal::style::Color::Rgb(red, green, blue) = cell.style.get_background_color()
    else {
        return ([0, 0, 0], false);
    };
    let is_known = cell.character == ' '
        && cell.cell_width == 1
        && cell.combining_characters.is_empty()
        && cell.style.get_attributes() == Default::default();
    ([red, green, blue], is_known)
}

fn sample_scaled_pixel(
    pixel_cell_size: PixelCellSize,
    image_plan: &Plan<'_>,
    pixel_x: u32,
    pixel_y: u32,
) -> Option<[u8; 4]> {
    let cell_pixel_width = u32::from(pixel_cell_size.get_pixel_width());
    let cell_pixel_height = u32::from(pixel_cell_size.get_pixel_height());
    let image_origin_x =
        u32::from(image_plan.output_paint.target_area.x).checked_mul(cell_pixel_width)?;
    let image_origin_y =
        u32::from(image_plan.output_paint.target_area.y).checked_mul(cell_pixel_height)?;
    let image_pixel_width =
        u32::from(image_plan.output_paint.target_area.width).checked_mul(cell_pixel_width)?;
    let image_pixel_height =
        u32::from(image_plan.output_paint.target_area.height).checked_mul(cell_pixel_height)?;
    let local_pixel_x = pixel_x.checked_sub(image_origin_x)?;
    let local_pixel_y = pixel_y.checked_sub(image_origin_y)?;
    if local_pixel_x >= image_pixel_width || local_pixel_y >= image_pixel_height {
        return None;
    }
    let source_pixel_x = image_plan.output_paint.source_rect.pixel_x.checked_add(
        u32::try_from(
            u64::from(local_pixel_x)
                .checked_mul(u64::from(image_plan.output_paint.source_rect.pixel_width))?
                .checked_div(u64::from(image_pixel_width))?,
        )
        .ok()?,
    )?;
    let source_pixel_y = image_plan.output_paint.source_rect.pixel_y.checked_add(
        u32::try_from(
            u64::from(local_pixel_y)
                .checked_mul(u64::from(image_plan.output_paint.source_rect.pixel_height))?
                .checked_div(u64::from(image_pixel_height))?,
        )
        .ok()?,
    )?;
    let decoded_image_width =
        usize::try_from(image_plan.output_paint.image_record.image.pixel_width).ok()?;
    let rgba_byte_index = (usize::try_from(source_pixel_y).ok()? * decoded_image_width
        + usize::try_from(source_pixel_x).ok()?)
    .checked_mul(4)?;
    image_plan
        .output_paint
        .image_record
        .image
        .rgba_bytes
        .get(rgba_byte_index..rgba_byte_index + 4)?
        .try_into()
        .ok()
}

fn apply_sixel_layer(
    image_plan: &Plan<'_>,
    image_pixel: [u8; 4],
    terminal_background_color: [u8; 3],
    underlying_pixel: [u8; 4],
) -> [u8; 4] {
    if let Some(background_color) = image_plan.sixel_composition.background_color {
        return blend_pixel(image_pixel, background_color, 255);
    }
    if image_pixel[3] == 0 {
        return if image_plan
            .output_paint
            .image_record
            .display
            .sixel_background
            == Some(SixelBackground::Terminal)
        {
            [
                terminal_background_color[0],
                terminal_background_color[1],
                terminal_background_color[2],
                255,
            ]
        } else {
            underlying_pixel
        };
    }
    blend_pixel(
        image_pixel,
        [
            underlying_pixel[0],
            underlying_pixel[1],
            underlying_pixel[2],
        ],
        underlying_pixel[3],
    )
}

fn apply_current_sixel_layer(
    image_plan: &Plan<'_>,
    image_pixel: [u8; 4],
    terminal_background_color: [u8; 3],
    underlying_pixel: [u8; 4],
) -> [u8; 4] {
    if let Some(background_color) = image_plan.sixel_composition.background_color {
        return blend_pixel(image_pixel, background_color, 255);
    }
    if image_pixel[3] == 0 {
        return if image_plan
            .output_paint
            .image_record
            .display
            .sixel_background
            == Some(SixelBackground::Terminal)
        {
            [
                terminal_background_color[0],
                terminal_background_color[1],
                terminal_background_color[2],
                255,
            ]
        } else {
            [0, 0, 0, 0]
        };
    }
    blend_pixel(
        image_pixel,
        [
            underlying_pixel[0],
            underlying_pixel[1],
            underlying_pixel[2],
        ],
        underlying_pixel[3],
    )
}

fn blend_pixel(image_pixel: [u8; 4], background_color: [u8; 3], background_alpha: u8) -> [u8; 4] {
    let image_alpha = u32::from(image_pixel[3]);
    let inverse_image_alpha = 255u32.saturating_sub(image_alpha);
    let background_alpha = u32::from(background_alpha);
    let composited_alpha = image_alpha * 255 + background_alpha * inverse_image_alpha;
    if composited_alpha == 0 {
        return [0, 0, 0, 0];
    }
    let blend_channel = |image_channel: u8, background_channel: u8| {
        let numerator = u32::from(image_channel) * image_alpha * 255
            + u32::from(background_channel) * background_alpha * inverse_image_alpha;
        u8::try_from((numerator + composited_alpha / 2) / composited_alpha).unwrap_or(255)
    };
    [
        blend_channel(image_pixel[0], background_color[0]),
        blend_channel(image_pixel[1], background_color[1]),
        blend_channel(image_pixel[2], background_color[2]),
        u8::try_from(((composited_alpha + 127) / 255).min(255)).unwrap_or(255),
    ]
}

fn blend_onto_background(
    image_pixel_bytes: &[u8],
    background_color: [u8; 3],
    destination_pixel_bytes: &mut [u8],
) {
    let image_alpha = u16::from(image_pixel_bytes[3]);
    let inverse_image_alpha = 255u16.saturating_sub(image_alpha);
    destination_pixel_bytes[0] = ((u16::from(image_pixel_bytes[0]) * image_alpha
        + u16::from(background_color[0]) * inverse_image_alpha
        + 127)
        / 255) as u8;
    destination_pixel_bytes[1] = ((u16::from(image_pixel_bytes[1]) * image_alpha
        + u16::from(background_color[1]) * inverse_image_alpha
        + 127)
        / 255) as u8;
    destination_pixel_bytes[2] = ((u16::from(image_pixel_bytes[2]) * image_alpha
        + u16::from(background_color[2]) * inverse_image_alpha
        + 127)
        / 255) as u8;
    destination_pixel_bytes[3] = 255;
}

fn crop_output_image(
    output_paint: &OutputPaint,
    background_color: Option<[u8; 3]>,
) -> Result<Arc<DecodedImage>, ()> {
    let decoded_image = &output_paint.image_record.image;
    let image_pixel_width = usize::try_from(decoded_image.pixel_width).map_err(|_| ())?;
    let image_pixel_height = usize::try_from(decoded_image.pixel_height).map_err(|_| ())?;
    validate_image_dimensions(
        GraphicsProtocol::Iterm2,
        image_pixel_width,
        image_pixel_height,
    )
    .map_err(|_| ())?;
    let expected_rgba_byte_count = compute_rgba_byte_count(
        GraphicsProtocol::Iterm2,
        image_pixel_width,
        image_pixel_height,
    )
    .map_err(|_| ())?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count
        || output_paint.source_rect.pixel_width == 0
        || output_paint.source_rect.pixel_height == 0
        || output_paint
            .source_rect
            .pixel_x
            .checked_add(output_paint.source_rect.pixel_width)
            .ok_or(())?
            > decoded_image.pixel_width
        || output_paint
            .source_rect
            .pixel_y
            .checked_add(output_paint.source_rect.pixel_height)
            .ok_or(())?
            > decoded_image.pixel_height
    {
        return Err(());
    }
    let crop_pixel_width = usize::try_from(output_paint.source_rect.pixel_width).map_err(|_| ())?;
    let crop_pixel_height =
        usize::try_from(output_paint.source_rect.pixel_height).map_err(|_| ())?;
    let crop_rgba_byte_count = compute_rgba_byte_count(
        GraphicsProtocol::Iterm2,
        crop_pixel_width,
        crop_pixel_height,
    )
    .map_err(|_| ())?;
    let mut cropped_rgba = Vec::new();
    cropped_rgba
        .try_reserve_exact(crop_rgba_byte_count)
        .map_err(|_| ())?;
    cropped_rgba.resize(crop_rgba_byte_count, 0);
    for crop_row in 0..crop_pixel_height {
        let source_pixel_row =
            usize::try_from(output_paint.source_rect.pixel_y).map_err(|_| ())? + crop_row;
        let source_rgba_byte_index = (source_pixel_row * image_pixel_width
            + usize::try_from(output_paint.source_rect.pixel_x).map_err(|_| ())?)
            * 4;
        let destination_rgba_byte_index = crop_row * crop_pixel_width * 4;
        for crop_column in 0..crop_pixel_width {
            let source_pixel_bytes = &decoded_image.rgba_bytes[source_rgba_byte_index
                + crop_column * 4
                ..source_rgba_byte_index + crop_column * 4 + 4];
            let destination_pixel_bytes = &mut cropped_rgba[destination_rgba_byte_index
                + crop_column * 4
                ..destination_rgba_byte_index + crop_column * 4 + 4];
            if let Some(background_color) = background_color {
                blend_onto_background(
                    source_pixel_bytes,
                    background_color,
                    destination_pixel_bytes,
                );
            } else {
                destination_pixel_bytes.copy_from_slice(source_pixel_bytes);
            }
        }
    }
    Ok(Arc::new(DecodedImage {
        pixel_width: output_paint.source_rect.pixel_width,
        pixel_height: output_paint.source_rect.pixel_height,
        rgba_bytes: cropped_rgba,
    }))
}

fn scale_output_tile(
    source_image: &DecodedImage,
    source_rect: ImageSourceRect,
    target_area: Rect,
    tile: TileRect,
    cell_pixel_width: u32,
    cell_pixel_height: u32,
    background_color: Option<[u8; 3]>,
) -> Result<Arc<DecodedImage>, ()> {
    let source_pixel_right = source_rect
        .pixel_x
        .checked_add(source_rect.pixel_width)
        .ok_or(())?;
    let source_pixel_bottom = source_rect
        .pixel_y
        .checked_add(source_rect.pixel_height)
        .ok_or(())?;
    let tile_column_end = tile
        .column_offset
        .checked_add(tile.column_count)
        .ok_or(())?;
    let tile_row_end = tile.row_offset.checked_add(tile.row_count).ok_or(())?;
    if source_rect.pixel_width == 0
        || source_rect.pixel_height == 0
        || source_pixel_right > source_image.pixel_width
        || source_pixel_bottom > source_image.pixel_height
        || target_area.width == 0
        || target_area.height == 0
        || tile.column_count == 0
        || tile.row_count == 0
        || tile_column_end > target_area.width
        || tile_row_end > target_area.height
    {
        return Err(());
    }
    let tile_pixel_width = u32::from(tile.column_count)
        .checked_mul(cell_pixel_width)
        .ok_or(())?;
    let tile_pixel_height = u32::from(tile.row_count)
        .checked_mul(cell_pixel_height)
        .ok_or(())?;
    let target_pixel_width = u32::from(target_area.width)
        .checked_mul(cell_pixel_width)
        .ok_or(())?;
    let target_pixel_height = u32::from(target_area.height)
        .checked_mul(cell_pixel_height)
        .ok_or(())?;
    let tile_pixel_width_usize = usize::try_from(tile_pixel_width).map_err(|_| ())?;
    let tile_pixel_height_usize = usize::try_from(tile_pixel_height).map_err(|_| ())?;
    let bytes_len = compute_rgba_byte_count(
        GraphicsProtocol::Sixel,
        tile_pixel_width_usize,
        tile_pixel_height_usize,
    )
    .map_err(|_| ())?;
    let mut scaled_rgba = Vec::new();
    scaled_rgba.try_reserve_exact(bytes_len).map_err(|_| ())?;
    scaled_rgba.resize(bytes_len, 0);
    let source_image_pixel_width = usize::try_from(source_image.pixel_width).map_err(|_| ())?;
    for tile_pixel_row in 0..tile_pixel_height {
        let target_pixel_row = u32::from(tile.row_offset)
            .checked_mul(cell_pixel_height)
            .and_then(|tile_pixel_origin_y| tile_pixel_origin_y.checked_add(tile_pixel_row))
            .ok_or(())?;
        let source_pixel_y = source_rect
            .pixel_y
            .checked_add(
                (u64::from(target_pixel_row) * u64::from(source_rect.pixel_height)
                    / u64::from(target_pixel_height))
                .min(u64::from(source_rect.pixel_height - 1)) as u32,
            )
            .ok_or(())?;
        for tile_pixel_column in 0..tile_pixel_width {
            let target_pixel_column = u32::from(tile.column_offset)
                .checked_mul(cell_pixel_width)
                .and_then(|tile_pixel_origin_x| tile_pixel_origin_x.checked_add(tile_pixel_column))
                .ok_or(())?;
            let source_pixel_x = source_rect
                .pixel_x
                .checked_add(
                    (u64::from(target_pixel_column) * u64::from(source_rect.pixel_width)
                        / u64::from(target_pixel_width))
                    .min(u64::from(source_rect.pixel_width - 1)) as u32,
                )
                .ok_or(())?;
            let source_rgba_byte_index = (usize::try_from(source_pixel_y).map_err(|_| ())?
                * source_image_pixel_width
                + usize::try_from(source_pixel_x).map_err(|_| ())?)
                * 4;
            let destination_rgba_byte_index = (usize::try_from(tile_pixel_row).map_err(|_| ())?
                * tile_pixel_width_usize
                + usize::try_from(tile_pixel_column).map_err(|_| ())?)
                * 4;
            let source_pixel_bytes =
                &source_image.rgba_bytes[source_rgba_byte_index..source_rgba_byte_index + 4];
            let destination_pixel_bytes =
                &mut scaled_rgba[destination_rgba_byte_index..destination_rgba_byte_index + 4];
            if let Some(background_color) = background_color {
                blend_onto_background(
                    source_pixel_bytes,
                    background_color,
                    destination_pixel_bytes,
                );
            } else {
                destination_pixel_bytes.copy_from_slice(source_pixel_bytes);
            }
        }
    }
    Ok(Arc::new(DecodedImage {
        pixel_width: tile_pixel_width,
        pixel_height: tile_pixel_height,
        rgba_bytes: scaled_rgba,
    }))
}

fn write_cursor_position<W: Write>(
    writer: &mut W,
    screen_column: u16,
    screen_row: u16,
) -> io::Result<()> {
    write!(
        writer,
        "\x1b[{};{}H",
        u32::from(screen_row) + 1,
        u32::from(screen_column) + 1
    )
}

fn invalid_output(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
