//! Bounded connection-local Kitty, iTerm2, and Sixel image output.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ratatui::layout::{Position, Rect};

use koshi_core::geometry::PixelCellSize;
use koshi_core::random::generate_random_number;
use koshi_image::{compute_rgba_byte_count, DecodedImage, GraphicsProtocol, MAX_IMAGE_BYTE_COUNT};
use koshi_ipc::frame::MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT;
use koshi_iterm::{ItermEncoder, ItermOutputOptions, MAX_ITERM_PACKET_BYTE_COUNT};
use koshi_kitty::{
    compress_kitty_image, write_kitty_delete_all, write_kitty_image_delete, write_kitty_placement,
    write_kitty_transmit, write_kitty_visible_placement_delete, KittyPlacement,
};
use koshi_renderer::{
    ImageCellSnapshot, ImageCellState, ImagePaint, ImagePlacementKey, ImageSourceRect,
    MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT,
};
use koshi_sixel::{
    PreparedSixelPalette, SixelEncodeOptions, SixelEncoder, MAX_PALETTE_COLOR_COUNT,
    MAX_SIXEL_CHUNK_BYTE_COUNT, MAX_SIXEL_OUTPUT_BYTE_COUNT, MIN_PALETTE_COLOR_COUNT,
};
use koshi_terminal::graphics::{ImageRecord, SixelBackground};

use super::{convert_kitty_output_error, GraphicsSupport};

const MAX_OUTPUT_PAINT_COUNT: usize = 4_096;
/// The largest Kitty image id this output hands out. Ids run from `1` to this
/// value, and the id after it is `1`.
const MAX_KITTY_IMAGE_ID: u32 = 16_777_215;
const MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT: usize =
    MAX_IMAGE_BYTE_COUNT * 2 + MAX_OUTPUT_PAINT_COUNT * (std::mem::size_of::<OutputPaint>() + 256);

const SIXEL_MODE_SAVE_BYTES: &[u8] = b"\x1b[?80s\x1b[?8452s\x1b[?1070s";
const SIXEL_MODE_RESET_BYTES: &[u8] = b"\x1b[?80l\x1b[?8452l\x1b[?1070h";
const SIXEL_MODE_RESTORE_BYTES: &[u8] = b"\x1b[?80r\x1b[?8452r\x1b[?1070r";
const IMAGE_ABORT_BYTES: &[u8] = b"\x18\x1b\\";
/// The most bytes [`ImageOutputState::build_frame_output`] writes around one
/// unit or one Kitty placement: cursor moves, Sixel mode changes, and the
/// placement command.
const UNIT_FRAMING_BYTE_COUNT: usize = 256;
/// The most bytes [`ImageOutputState::build_frame_output`] writes once per
/// frame around its units.
const FRAME_FRAMING_BYTE_COUNT: usize = 32;
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
        maximum_pixel_width: Option<u32>,
        /// Maximum Sixel height in pixels.
        maximum_pixel_height: Option<u32>,
    },
}

/// Describes a native image composition that is not exact on the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct ImageCompatibility {
    /// The host cannot express the image layer order relative to text glyphs.
    has_text_layer_order_mismatch: bool,
    /// iTerm2 alpha cannot preserve the required underlying pixels.
    has_iterm_alpha_mismatch: bool,
    /// Sixel partial alpha uses an unknown underlying color.
    has_sixel_alpha_mismatch: bool,
    /// Sixel terminal-background pixels use an unknown cell color.
    has_sixel_terminal_background_mismatch: bool,
}

impl ImageOutputKind {
    /// Return the output protocol for a terminal capability.
    pub(crate) fn from_support(support: GraphicsSupport) -> Option<Self> {
        match support {
            GraphicsSupport::Kitty => Some(Self::Kitty),
            GraphicsSupport::Iterm => Some(Self::Iterm),
            GraphicsSupport::Sixel {
                palette_color_count,
                maximum_pixel_width,
                maximum_pixel_height,
            } => Some(Self::Sixel {
                palette_color_count,
                maximum_pixel_width,
                maximum_pixel_height,
            }),
            GraphicsSupport::Unsupported => None,
        }
    }

    /// Return whether the protocol uses Sixel host modes.
    pub(crate) const fn is_sixel(self) -> bool {
        matches!(self, Self::Sixel { .. })
    }

    /// Return whether the protocol blends image pixels with the cells the
    /// image covers: `true` for iTerm2 and Sixel, `false` for Kitty, which
    /// places pixels by z-index.
    pub(crate) const fn needs_cell_composition(self) -> bool {
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
    /// `false` on Kitty, and for a fully opaque image at a z-index at or above
    /// zero, which replaces every cell it covers. `true` for every other
    /// paint, an unknown alpha coverage included.
    fn needs_target_cell_composition(&self, output_kind: ImageOutputKind) -> bool {
        if !output_kind.needs_cell_composition() {
            return false;
        }
        !(self.z_index >= 0 && self.alpha_stats.is_some_and(AlphaStats::is_fully_opaque))
    }
}

/// Values that identify exact image output inputs: two paints with equal keys
/// encode to equal bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EncodeKey {
    image_content_id: u64,
    source_rect: ImageSourceKey,
    target_column_count: u16,
    target_row_count: u16,
    pixel_cell_size: PixelCellSize,
    output_kind: ImageOutputKind,
    z_index: i32,
    has_terminal_background: bool,
    /// What the encoded pixels compose with, for a paint whose pixels read
    /// the cells it covers. `None` for every other paint.
    composition_input: Option<Arc<CompositionInput>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageSourceKey {
    pixel_x: u32,
    pixel_y: u32,
    pixel_width: u32,
    pixel_height: u32,
}

/// What a composed paint's encoded pixels read besides its own image.
#[derive(Debug, PartialEq, Eq, Hash)]
struct CompositionInput {
    /// The paint's own classification.
    plan_outcome: PlanOutcome,
    /// The cell size the outer terminal reported, if it reported one.
    measured_pixel_cell_size: Option<PixelCellSize>,
    /// The cells under the paint's target area, row by row. `None` marks a
    /// cell outside the frame's cell snapshot.
    covered_cells: Arc<[Option<ImageCellState>]>,
    /// Each lower paint whose target area shares a cell with the paint's, in
    /// draw order.
    lower_layers: Vec<LowerLayer>,
}

/// One lower paint that a composed paint covers part of, placed relative to
/// the composed paint's top-left cell.
#[derive(Debug, PartialEq, Eq, Hash)]
struct LowerLayer {
    image_content_id: u64,
    source_rect: ImageSourceKey,
    /// The lower paint's left column minus the composed paint's left column.
    column_offset: i32,
    /// The lower paint's top row minus the composed paint's top row.
    row_offset: i32,
    column_count: u16,
    row_count: u16,
    has_terminal_background: bool,
    plan_outcome: PlanOutcome,
}

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

/// The encode inputs of one frame's paints, in draw order.
#[derive(Debug, Default, PartialEq)]
struct FrameEncodeInputs {
    /// The encode key of each paint.
    encode_keys: Vec<EncodeKey>,
    /// The classification of each paint, for an output that draws over
    /// cells. Empty for Kitty.
    plan_outcomes: Vec<PlanOutcome>,
    /// The cells under each paint's target area, row by row, for an output
    /// that draws over cells. Empty for Kitty.
    covered_cells: Vec<Arc<[Option<ImageCellState>]>>,
}

/// One Kitty image a worker request writes under its own id.
#[derive(Debug, Clone)]
struct KittyUploadRequest {
    image_content_id: u64,
    /// The Kitty image id carried by `i=`, from `1` to [`MAX_KITTY_IMAGE_ID`].
    kitty_image_id: u32,
    image: Arc<DecodedImage>,
    /// The image's zlib payload that the encode cache keeps, or `None` when
    /// the worker compresses the image.
    compressed_bytes: Option<Arc<[u8]>>,
}

/// One Kitty upload that no paint has written.
#[derive(Debug, Clone)]
struct FinishedKittyUpload {
    image_content_id: u64,
    /// The Kitty image id carried by `i=`, from `1` to [`MAX_KITTY_IMAGE_ID`].
    kitty_image_id: u32,
    /// The `a=t` commands that carry the image's compressed pixels.
    upload_bytes: Arc<[u8]>,
}

/// A worker request: the paints of one iTerm2 or Sixel frame to encode, or
/// Kitty images to upload.
struct WorkerRequest {
    frame_generation: u64,
    output_kind: ImageOutputKind,
    pixel_cell_size: PixelCellSize,
    measured_pixel_cell_size: Option<PixelCellSize>,
    cell_snapshot: Option<Arc<ImageCellSnapshot>>,
    /// Every paint of an iTerm2 or Sixel frame, in draw order. Empty for
    /// Kitty.
    output_paints: Vec<OutputPaint>,
    /// The classification of each output paint, in the same order.
    plan_outcomes: Vec<PlanOutcome>,
    /// One entry per output paint, in the same order: the key the request
    /// encodes the paint under, or `None` for a paint the request does not
    /// encode and only composes the paints above it over.
    encode_keys: Vec<Option<EncodeKey>>,
    /// The images a Kitty request uploads. Empty for every other protocol.
    kitty_upload_requests: Vec<KittyUploadRequest>,
    is_cancellation_requested: Arc<AtomicBool>,
}

impl WorkerRequest {
    /// Return the placement and encode key of each paint the request encodes.
    fn list_requested_encode_keys(&self) -> Vec<(ImagePlacementKey, EncodeKey)> {
        self.output_paints
            .iter()
            .zip(&self.encode_keys)
            .filter_map(|(output_paint, encode_key)| {
                encode_key
                    .clone()
                    .map(|encode_key| (output_paint.placement_key, encode_key))
            })
            .collect()
    }

    /// Return the content id and Kitty image id of each image the request
    /// uploads.
    fn list_requested_kitty_images(&self) -> Vec<(u64, u32)> {
        self.kitty_upload_requests
            .iter()
            .map(|kitty_upload_request| {
                (
                    kitty_upload_request.image_content_id,
                    kitty_upload_request.kitty_image_id,
                )
            })
            .collect()
    }
}

/// A worker output unit with relative cell geometry.
#[derive(Debug)]
struct OutputUnit {
    frame_generation: u64,
    placement_key: ImagePlacementKey,
    tile_offset: (u16, u16),
    output_bytes: Arc<[u8]>,
}

/// Messages sent by the bounded image worker.
#[derive(Debug)]
enum WorkerMessage {
    /// The units of `placement_key` follow.
    Prepared {
        frame_generation: u64,
        placement_key: ImagePlacementKey,
    },
    /// The host cannot draw `placement_key`.
    Unavailable {
        frame_generation: u64,
        placement_key: ImagePlacementKey,
    },
    Unit(OutputUnit),
    /// One Kitty image's upload, and the zlib payload it carries.
    KittyUploaded {
        frame_generation: u64,
        finished_kitty_upload: FinishedKittyUpload,
        compressed_bytes: Arc<[u8]>,
    },
    Finished {
        frame_generation: u64,
        has_failed: bool,
    },
}

/// What a finished encode produced for one placement.
#[derive(Debug, Clone)]
enum EncodedPixels {
    /// The units that draw the placement, at offsets from its target area's
    /// top-left cell.
    Drawn(Arc<[TemplateUnit]>),
    /// The host cannot draw the placement.
    Unavailable,
}

/// The newest finished iTerm2 or Sixel encode of one placement.
#[derive(Debug, Clone)]
struct EncodedPlacement {
    encode_key: EncodeKey,
    encoded_pixels: EncodedPixels,
}

/// What the worker has sent for one placement of the active request.
enum ReceivedPixels {
    Drawn(Vec<TemplateUnit>),
    Unavailable,
}

/// The request the worker runs, and what it has sent back so far.
struct ActiveJob {
    frame_generation: u64,
    is_cancellation_requested: Arc<AtomicBool>,
    /// The placement and encode key of each paint the request encodes. Empty
    /// for Kitty.
    requested_encode_keys: Vec<(ImagePlacementKey, EncodeKey)>,
    /// The content id and Kitty image id of each image the request uploads.
    /// Empty for every other protocol.
    requested_kitty_images: Vec<(u64, u32)>,
    received_pixels_by_placement_key: HashMap<ImagePlacementKey, ReceivedPixels>,
    received_kitty_uploads: Vec<FinishedKittyUpload>,
    /// The content id and zlib payload of each received Kitty upload.
    received_kitty_compressions: Vec<(u64, Arc<[u8]>)>,
    /// The output bytes received, at most [`MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT`].
    received_byte_count: usize,
    /// Whether the received output passed its byte limit.
    has_failed: bool,
}

impl ActiveJob {
    fn from_worker_request(worker_request: &WorkerRequest) -> Self {
        Self {
            frame_generation: worker_request.frame_generation,
            is_cancellation_requested: Arc::clone(&worker_request.is_cancellation_requested),
            requested_encode_keys: worker_request.list_requested_encode_keys(),
            requested_kitty_images: worker_request.list_requested_kitty_images(),
            received_pixels_by_placement_key: HashMap::new(),
            received_kitty_uploads: Vec::new(),
            received_kitty_compressions: Vec::new(),
            received_byte_count: 0,
            has_failed: false,
        }
    }

    /// Count `output_byte_count` more received bytes. Past
    /// [`MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT`] the job fails and its worker
    /// request is cancelled. Returns whether the bytes fit.
    fn count_received_bytes(&mut self, output_byte_count: usize) -> bool {
        match self.received_byte_count.checked_add(output_byte_count) {
            Some(received_byte_count)
                if received_byte_count <= MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT =>
            {
                self.received_byte_count = received_byte_count;
                true
            }
            _ => {
                self.has_failed = true;
                self.is_cancellation_requested
                    .store(true, Ordering::Release);
                false
            }
        }
    }
}

/// The pixels one placement shows on the host.
#[derive(Debug, Clone)]
enum SelectedPixels {
    /// iTerm2 or Sixel units encoded for `encode_key`.
    Units {
        encode_key: EncodeKey,
        output_units: Arc<[TemplateUnit]>,
    },
    /// A Kitty image placed by `kitty_placement`. `encode_key` is the key of
    /// the paint the image was placed for first.
    Kitty {
        encode_key: EncodeKey,
        kitty_placement: KittyPlacement,
        image: Arc<DecodedImage>,
    },
}

impl PartialEq for SelectedPixels {
    fn eq(&self, other_selected_pixels: &Self) -> bool {
        match (self, other_selected_pixels) {
            (
                Self::Units {
                    encode_key,
                    output_units,
                },
                Self::Units {
                    encode_key: other_encode_key,
                    output_units: other_output_units,
                },
            ) => encode_key == other_encode_key && Arc::ptr_eq(output_units, other_output_units),
            (
                Self::Kitty {
                    kitty_placement, ..
                },
                Self::Kitty {
                    kitty_placement: other_kitty_placement,
                    ..
                },
            ) => kitty_placement == other_kitty_placement,
            _ => false,
        }
    }
}

/// One placement's pixels, where they sit, and the cells under them.
#[derive(Debug, Clone, PartialEq)]
struct SelectedPlacement {
    placement_key: ImagePlacementKey,
    /// The cells the pixels cover.
    target_area: Rect,
    /// The cells under `target_area`, row by row, for an output that draws
    /// over cells. `None` for Kitty.
    covered_cells: Option<Arc<[Option<ImageCellState>]>>,
    pixels: SelectedPixels,
}

/// The key of one output the encode cache keeps.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CachedOutputKey {
    /// The iTerm2 or Sixel units of every paint with this encode key.
    Units(EncodeKey),
    /// The zlib payload of the Kitty image with this content id.
    KittyCompressed { image_content_id: u64 },
}

impl CachedOutputKey {
    /// Return whether the output reads an image in `image_content_ids`: its
    /// own image, or an image below it that it composes with.
    fn is_reading_any_image(&self, image_content_ids: &HashSet<u64>) -> bool {
        match self {
            Self::Units(encode_key) => {
                image_content_ids.contains(&encode_key.image_content_id)
                    || encode_key
                        .composition_input
                        .as_ref()
                        .is_some_and(|composition_input| {
                            composition_input.lower_layers.iter().any(|lower_layer| {
                                image_content_ids.contains(&lower_layer.image_content_id)
                            })
                        })
            }
            Self::KittyCompressed { image_content_id } => {
                image_content_ids.contains(image_content_id)
            }
        }
    }
}

/// One output the encode cache keeps.
#[derive(Debug, Clone)]
enum CachedOutput {
    /// The units that draw a paint.
    Units(Arc<[TemplateUnit]>),
    /// The zlib payload of a Kitty image.
    KittyCompressed(Arc<[u8]>),
}

/// One output the encode cache keeps, its size, and when it was last used.
#[derive(Debug)]
struct CachedOutputEntry {
    cached_output: CachedOutput,
    /// The bytes of the output and of the cells and layers its key holds.
    byte_count: u64,
    /// The use sequence of the last insert or hit.
    last_use_sequence: u64,
}

/// Encoded image output kept for reuse.
///
/// The kept outputs and the decoded image records the client holds share
/// one image budget, [`MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT`] (64 MiB). The
/// cache gives way first: when the budget needs room, the least recently used
/// output leaves, then the next. An output larger than the room the records
/// leave is not kept, and its paint encodes again the next time it misses.
/// Example: with 60 MiB of records held, a 5 MiB Sixel string is not kept,
/// and a 3 MiB one is kept after outputs that hold more than 1 MiB leave.
#[derive(Debug)]
struct EncodedOutputCache {
    cached_output_entry_by_key: HashMap<CachedOutputKey, CachedOutputEntry>,
    /// The key of each entry, by its last use sequence. The first key is the
    /// least recently used.
    key_by_last_use_sequence: BTreeMap<u64, CachedOutputKey>,
    /// The use sequence the next insert or hit takes.
    next_use_sequence: u64,
    /// The sum of every entry's byte count.
    cached_byte_count: u64,
    /// The bytes of decoded pixels the client's image records hold.
    held_image_byte_count: u64,
}

impl EncodedOutputCache {
    /// An empty cache beside image records that hold no bytes.
    fn new() -> Self {
        Self {
            cached_output_entry_by_key: HashMap::new(),
            key_by_last_use_sequence: BTreeMap::new(),
            next_use_sequence: 0,
            cached_byte_count: 0,
            held_image_byte_count: 0,
        }
    }

    /// Return the units kept for `encode_key`, and mark them used.
    fn find_cached_units(&mut self, encode_key: &EncodeKey) -> Option<Arc<[TemplateUnit]>> {
        match self.find_cached_output(&CachedOutputKey::Units(encode_key.clone()))? {
            CachedOutput::Units(output_units) => Some(output_units),
            CachedOutput::KittyCompressed(_) => None,
        }
    }

    /// Return the zlib payload kept for the Kitty image `image_content_id`,
    /// and mark it used.
    fn find_kitty_compressed_bytes(&mut self, image_content_id: u64) -> Option<Arc<[u8]>> {
        match self.find_cached_output(&CachedOutputKey::KittyCompressed { image_content_id })? {
            CachedOutput::KittyCompressed(compressed_bytes) => Some(compressed_bytes),
            CachedOutput::Units(_) => None,
        }
    }

    /// Keep `output_units` for `encode_key` when they fit the image budget.
    fn insert_cached_units(&mut self, encode_key: EncodeKey, output_units: Arc<[TemplateUnit]>) {
        let unit_byte_count = output_units
            .iter()
            .map(|output_unit| output_unit.output_bytes.len())
            .fold(0usize, usize::saturating_add);
        let key_byte_count = encode_key
            .composition_input
            .as_ref()
            .map_or(0, |composition_input| {
                composition_input
                    .covered_cells
                    .len()
                    .saturating_mul(std::mem::size_of::<Option<ImageCellState>>())
                    .saturating_add(
                        composition_input
                            .lower_layers
                            .len()
                            .saturating_mul(std::mem::size_of::<LowerLayer>()),
                    )
            });
        let byte_count =
            u64::try_from(unit_byte_count.saturating_add(key_byte_count)).unwrap_or(u64::MAX);
        self.insert_cached_output(
            CachedOutputKey::Units(encode_key),
            CachedOutput::Units(output_units),
            byte_count,
        );
    }

    /// Keep `compressed_bytes`, the zlib payload of the Kitty image
    /// `image_content_id`, when it fits the image budget.
    fn insert_kitty_compressed_bytes(
        &mut self,
        image_content_id: u64,
        compressed_bytes: Arc<[u8]>,
    ) {
        let byte_count = u64::try_from(compressed_bytes.len()).unwrap_or(u64::MAX);
        self.insert_cached_output(
            CachedOutputKey::KittyCompressed { image_content_id },
            CachedOutput::KittyCompressed(compressed_bytes),
            byte_count,
        );
    }

    /// Record that the client's image records hold `held_image_byte_count`
    /// bytes, and drop the least recently used outputs until the kept
    /// outputs fit the room the records leave.
    fn set_held_image_byte_count(&mut self, held_image_byte_count: u64) {
        self.held_image_byte_count = held_image_byte_count;
        self.remove_least_recently_used_outputs(0);
    }

    /// Drop every output that reads an image in `image_content_ids`.
    fn remove_released_outputs(&mut self, image_content_ids: &[u64]) {
        let released_image_content_ids = image_content_ids.iter().copied().collect::<HashSet<_>>();
        let released_keys = self
            .cached_output_entry_by_key
            .keys()
            .filter(|cached_output_key| {
                cached_output_key.is_reading_any_image(&released_image_content_ids)
            })
            .cloned()
            .collect::<Vec<_>>();
        for released_key in released_keys {
            self.remove_cached_output(&released_key);
        }
    }

    /// Drop every kept output.
    fn clear_cached_outputs(&mut self) {
        self.cached_output_entry_by_key.clear();
        self.key_by_last_use_sequence.clear();
        self.cached_byte_count = 0;
    }

    /// Return the output kept for `cached_output_key`, and mark it used.
    fn find_cached_output(&mut self, cached_output_key: &CachedOutputKey) -> Option<CachedOutput> {
        let use_sequence = self.next_use_sequence;
        let cached_output_entry = self.cached_output_entry_by_key.get_mut(cached_output_key)?;
        self.key_by_last_use_sequence
            .remove(&cached_output_entry.last_use_sequence);
        cached_output_entry.last_use_sequence = use_sequence;
        self.key_by_last_use_sequence
            .insert(use_sequence, cached_output_key.clone());
        self.next_use_sequence += 1;
        Some(cached_output_entry.cached_output.clone())
    }

    /// Keep `cached_output` of `byte_count` bytes for `cached_output_key`, in
    /// place of what the key held, after the least recently used outputs
    /// leave to make room. An output larger than the room the image records
    /// leave is not kept.
    fn insert_cached_output(
        &mut self,
        cached_output_key: CachedOutputKey,
        cached_output: CachedOutput,
        byte_count: u64,
    ) {
        self.remove_cached_output(&cached_output_key);
        let room_byte_count =
            MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT.saturating_sub(self.held_image_byte_count);
        if byte_count > room_byte_count {
            return;
        }
        self.remove_least_recently_used_outputs(byte_count);
        let use_sequence = self.next_use_sequence;
        self.next_use_sequence += 1;
        self.key_by_last_use_sequence
            .insert(use_sequence, cached_output_key.clone());
        self.cached_output_entry_by_key.insert(
            cached_output_key,
            CachedOutputEntry {
                cached_output,
                byte_count,
                last_use_sequence: use_sequence,
            },
        );
        self.cached_byte_count += byte_count;
    }

    /// Drop the least recently used outputs until the kept outputs and
    /// `incoming_byte_count` more bytes fit beside the image records.
    fn remove_least_recently_used_outputs(&mut self, incoming_byte_count: u64) {
        while self
            .held_image_byte_count
            .saturating_add(self.cached_byte_count)
            .saturating_add(incoming_byte_count)
            > MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT
        {
            let Some((_, least_recently_used_key)) = self.key_by_last_use_sequence.pop_first()
            else {
                return;
            };
            if let Some(cached_output_entry) = self
                .cached_output_entry_by_key
                .remove(&least_recently_used_key)
            {
                self.cached_byte_count -= cached_output_entry.byte_count;
            }
        }
    }

    /// Drop the output kept for `cached_output_key`, if one is kept.
    fn remove_cached_output(&mut self, cached_output_key: &CachedOutputKey) {
        if let Some(cached_output_entry) = self.cached_output_entry_by_key.remove(cached_output_key)
        {
            self.key_by_last_use_sequence
                .remove(&cached_output_entry.last_use_sequence);
            self.cached_byte_count -= cached_output_entry.byte_count;
        }
    }
}

/// Bounded image output state owned by one terminal connection.
///
/// The newest frame's text never waits for an image. Each paint shows, for
/// every image placement of the newest frame, in this order of preference:
///
/// 1. pixels encoded for the placement's exact encode key: the newest encode
///    of the placement, the pixels the host shows, or an entry of the encode
///    cache;
/// 2. older pixels of the same placement whose encode key has the same source
///    rectangle, cell size, z-index and protocol, until the exact encode
///    finishes;
/// 3. no pixels, while the encode runs, and the unavailable-image text once
///    the encode for the exact key failed or found the image undrawable.
///
/// The encode cache ([`EncodedOutputCache`]) keeps iTerm2 and Sixel units by
/// encode key, and Kitty zlib payloads by content id. A frame whose every
/// iTerm2 or Sixel paint hits the cache gives the worker no work, and the
/// worker writes a Kitty image whose payload the cache keeps without
/// compressing it again. Example: a looping 30-frame GIF encodes each frame
/// once in its first loop, and every loop after the first writes the frames
/// from the cache.
///
/// One worker request runs at a time and always runs to its end. The newest
/// frame's request replaces only the request that waits to start. Example: a
/// Sixel video whose frames arrive every 30 ms and take 50 ms to encode shows
/// every second frame, and the text around it never waits.
pub(crate) struct ImageOutputState {
    output_kind: Option<ImageOutputKind>,
    worker_request_sender: Option<Sender<WorkerRequest>>,
    worker_messages: Option<Receiver<WorkerMessage>>,
    worker: Option<JoinHandle<()>>,
    /// The generation of the newest worker request.
    frame_generation: u64,
    active_job: Option<ActiveJob>,
    pending_worker_request: Option<WorkerRequest>,
    latest_output_paints: Vec<OutputPaint>,
    /// The encode key, classification and covered cells of each latest paint.
    latest_encode_inputs: FrameEncodeInputs,
    /// Encoded output kept for reuse within the client's image budget.
    encoded_output_cache: EncodedOutputCache,
    /// Whether no image of the latest frame can be drawn: the frame named no
    /// measured cell size for Sixel, more than [`MAX_OUTPUT_PAINT_COUNT`]
    /// paints, no cell snapshot, or storage that could not be reserved.
    is_latest_frame_unavailable: bool,
    /// Whether a worker request finished, the host state changed, or a paint
    /// left finished Kitty uploads unwritten, after the last placement
    /// selection.
    has_unselected_results: bool,
    /// The newest finished iTerm2 or Sixel encode of each latest placement.
    encoded_placement_by_key: HashMap<ImagePlacementKey, EncodedPlacement>,
    /// Kitty uploads the worker finished and no paint has written.
    finished_kitty_uploads: Vec<FinishedKittyUpload>,
    /// The Kitty image ids of `finished_kitty_uploads` the next paint writes.
    selected_kitty_upload_ids: Vec<u32>,
    /// The content ids of the latest Kitty images whose upload failed.
    failed_kitty_image_content_ids: HashSet<u64>,
    /// The pixels the next paint shows, in draw order.
    selected_placements: Vec<SelectedPlacement>,
    /// The latest placements that show the unavailable-image text.
    unavailable_placement_keys: Vec<ImagePlacementKey>,
    /// The pixels the host shows, in draw order.
    shown_placements: Vec<SelectedPlacement>,
    /// Whether the host may show image pixels: `shown_placements` is not
    /// empty, or the host screen is unknown.
    has_host_pixels: bool,
    /// Whether a failed frame or a replaced connection left the host screen
    /// unknown. The next paint clears it and draws every cell and image.
    is_host_screen_unknown: bool,
    needs_screen_reset: bool,
    needs_image_abort: bool,
    /// Alpha coverage per decoded-image address and source rectangle. Holds
    /// only the addresses retained by the latest frame.
    alpha_stats_by_image_and_source: HashMap<(usize, ImageSourceKey), Option<AlphaStats>>,
    /// The Kitty image id of each image the host holds, by content id.
    held_kitty_image_id_by_content_id: HashMap<u64, u32>,
    /// Kitty image ids the next frame reset frees on the host.
    kitty_image_ids_to_delete: Vec<u32>,
    /// Whether the next frame reset frees every Kitty image the host holds.
    should_free_all_kitty_images: bool,
    /// The Kitty image id that the next new image tries first, from `1` to
    /// [`MAX_KITTY_IMAGE_ID`]. It starts at a random id, counts up, and goes
    /// to `1` after [`MAX_KITTY_IMAGE_ID`].
    next_kitty_image_id: u32,
    /// Called by the worker after it ends each request, and by
    /// [`Self::commit_frame`] when finished Kitty uploads wait for the next
    /// paint.
    notify_image_output_ready: Arc<dyn Fn() + Send + Sync>,
}

impl ImageOutputState {
    /// Build a connection-local worker for the selected output protocol.
    ///
    /// `notify_image_output_ready` runs when the next paint has new pixels to
    /// show: the worker calls it after it sends the end of each request, and
    /// [`Self::commit_frame`] calls it when finished Kitty uploads wait for the
    /// next paint.
    pub(crate) fn from_output_kind(
        output_kind: Option<ImageOutputKind>,
        notify_image_output_ready: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (worker_request_sender, worker_messages, worker) = if output_kind.is_some() {
            let (request_sender, request_receiver) = mpsc::channel();
            let (message_sender, message_receiver) = mpsc::channel();
            let worker_notify_image_output_ready = Arc::clone(&notify_image_output_ready);
            match thread::Builder::new()
                .name(String::from("koshi-image-output"))
                .spawn(move || {
                    run_worker_loop(
                        request_receiver,
                        message_sender,
                        worker_notify_image_output_ready,
                    );
                }) {
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
            latest_encode_inputs: FrameEncodeInputs::default(),
            encoded_output_cache: EncodedOutputCache::new(),
            is_latest_frame_unavailable: false,
            has_unselected_results: false,
            encoded_placement_by_key: HashMap::new(),
            finished_kitty_uploads: Vec::new(),
            selected_kitty_upload_ids: Vec::new(),
            failed_kitty_image_content_ids: HashSet::new(),
            selected_placements: Vec::new(),
            unavailable_placement_keys: Vec::new(),
            shown_placements: Vec::new(),
            has_host_pixels: false,
            is_host_screen_unknown: false,
            needs_screen_reset: false,
            needs_image_abort: false,
            alpha_stats_by_image_and_source: HashMap::new(),
            held_kitty_image_id_by_content_id: HashMap::new(),
            kitty_image_ids_to_delete: Vec::new(),
            should_free_all_kitty_images: false,
            next_kitty_image_id: (generate_random_number() % u64::from(MAX_KITTY_IMAGE_ID)) as u32
                + 1,
            notify_image_output_ready,
        }
    }

    /// Return the connection's output protocol, if it has one.
    pub(crate) const fn get_output_kind(&self) -> Option<ImageOutputKind> {
        self.output_kind
    }

    /// Return the latest placements that show the unavailable-image text.
    pub(crate) fn list_unavailable_placement_keys(&self) -> &[ImagePlacementKey] {
        &self.unavailable_placement_keys
    }

    /// Return whether a worker request finished, or the host state changed,
    /// after the last paint picked the pixels it shows.
    pub(crate) const fn has_unselected_results(&self) -> bool {
        self.has_unselected_results
    }

    /// Take the first Kitty image id from `next_kitty_image_id` on that
    /// `reserved_kitty_image_ids` does not hold, add it to
    /// `reserved_kitty_image_ids`, and set `next_kitty_image_id` to the id
    /// after it. The id after [`MAX_KITTY_IMAGE_ID`] is `1`.
    ///
    /// Example: with `next_kitty_image_id` at `16_777_214` and `16_777_214`
    /// reserved, the call returns `16_777_215` and leaves `next_kitty_image_id`
    /// at `1`. Returns `None` when `reserved_kitty_image_ids` holds every id
    /// from `1` to [`MAX_KITTY_IMAGE_ID`].
    fn take_free_kitty_image_id(
        &mut self,
        reserved_kitty_image_ids: &mut HashSet<u32>,
    ) -> Option<u32> {
        for _ in 0..MAX_KITTY_IMAGE_ID {
            let kitty_image_id = self.next_kitty_image_id;
            self.next_kitty_image_id = if kitty_image_id == MAX_KITTY_IMAGE_ID {
                1
            } else {
                kitty_image_id + 1
            };
            if reserved_kitty_image_ids.insert(kitty_image_id) {
                return Some(kitty_image_id);
            }
        }
        None
    }

    /// Record that the host screen is erased before the next frame's cells:
    /// the host shows no image pixels, and the next paint writes every
    /// selected placement again after the cells. `has_host_pixels` becomes
    /// `is_host_screen_unknown`. Encode results, finished uploads and the
    /// encode cache are kept.
    ///
    /// On Kitty output the host also holds no image: every Kitty image id is
    /// dropped, and the next frame reset frees every image with `a=d,d=A`.
    /// Finished uploads keep their ids and are written after the reset.
    /// `next_kitty_image_id` keeps counting from where it is. With no output
    /// protocol this changes nothing.
    ///
    /// Example: an iTerm2 image shown at `(0, 0)` before a host resize is
    /// written again, from its kept units, in the frame whose cells follow
    /// the `ESC[2J` of the resize.
    pub(crate) fn clear_host_images(&mut self) {
        let Some(output_kind) = self.output_kind else {
            return;
        };
        self.shown_placements.clear();
        self.has_host_pixels = self.is_host_screen_unknown;
        self.has_unselected_results = true;
        if !matches!(output_kind, ImageOutputKind::Kitty) {
            return;
        }
        self.held_kitty_image_id_by_content_id.clear();
        self.kitty_image_ids_to_delete.clear();
        self.should_free_all_kitty_images = true;
    }

    /// Read the newest frame's image paints, pick the pixels each placement
    /// shows at the next paint, and queue the worker request the frame still
    /// needs.
    ///
    /// A frame whose paints and encode inputs equal the latest frame's, with
    /// no request finished since, changes nothing. A frame with a paint whose
    /// alpha coverage is unknown shows the unavailable-image text over each
    /// of its placements.
    pub(crate) fn prepare_frame(
        &mut self,
        image_paints: &[ImagePaint],
        cell_snapshot: Option<Arc<ImageCellSnapshot>>,
        measured_pixel_cell_size: Option<PixelCellSize>,
    ) {
        self.poll();
        let Some(output_kind) = self.output_kind else {
            return;
        };
        let pixel_cell_size = if output_kind.is_sixel() {
            let Some(pixel_cell_size) = measured_pixel_cell_size else {
                self.set_unavailable_frame(image_paints);
                return;
            };
            pixel_cell_size
        } else {
            PixelCellSize::from_pixel_dimensions(1, 1).expect("one-pixel cell is nonzero")
        };
        if image_paints.len() > MAX_OUTPUT_PAINT_COUNT {
            self.set_unavailable_frame(image_paints);
            return;
        }
        if !image_paints.is_empty()
            && output_kind.needs_cell_composition()
            && cell_snapshot.as_ref().is_none_or(|cell_snapshot| {
                usize::try_from(cell_snapshot.screen_area.area()).ok()
                    > Some(MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT)
            })
        {
            self.set_unavailable_frame(image_paints);
            return;
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
            return;
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
                None if output_kind.needs_cell_composition() => {
                    compute_alpha_stats(&image_paint.image_record.image, image_paint.source_rect)
                }
                None => None,
            };
            alpha_stats_by_image_and_source.insert(image_source_key, image_alpha_stats);
            latest_output_paints.push(OutputPaint::from_paint(image_paint, image_alpha_stats));
        }
        self.alpha_stats_by_image_and_source = alpha_stats_by_image_and_source;
        let Some(encode_inputs) = build_frame_encode_inputs(
            output_kind,
            pixel_cell_size,
            measured_pixel_cell_size,
            cell_snapshot.as_deref(),
            &latest_output_paints,
        ) else {
            self.set_unavailable_frame(image_paints);
            return;
        };
        let is_latest_frame_unchanged = !self.is_latest_frame_unavailable
            && is_output_paint_sequence_equal(&self.latest_output_paints, &latest_output_paints)
            && self.latest_encode_inputs == encode_inputs;
        if is_latest_frame_unchanged && !self.has_unselected_results {
            return;
        }
        self.latest_output_paints = latest_output_paints;
        self.latest_encode_inputs = encode_inputs;
        self.is_latest_frame_unavailable = false;
        for _ in 0..2 {
            self.has_unselected_results = false;
            self.select_and_request_latest_frame(
                output_kind,
                pixel_cell_size,
                measured_pixel_cell_size,
                cell_snapshot.clone(),
            );
            if !self.has_unselected_results {
                break;
            }
        }
        self.update_screen_reset();
    }

    /// Pick the pixels of each latest placement and queue the worker request
    /// the latest frame still needs. A request that finds the worker gone
    /// fails at once and leaves `has_unselected_results` set; the caller then
    /// picks again, and the failed placements show the unavailable-image text
    /// in the same paint.
    fn select_and_request_latest_frame(
        &mut self,
        output_kind: ImageOutputKind,
        pixel_cell_size: PixelCellSize,
        measured_pixel_cell_size: Option<PixelCellSize>,
        cell_snapshot: Option<Arc<ImageCellSnapshot>>,
    ) {
        if matches!(output_kind, ImageOutputKind::Kitty) {
            let needed_kitty_images = self.select_kitty_placements();
            self.request_kitty_uploads(needed_kitty_images, pixel_cell_size);
        } else {
            let encode_miss_indices = self.select_encoded_placements();
            self.request_latest_frame_encode(
                output_kind,
                pixel_cell_size,
                measured_pixel_cell_size,
                cell_snapshot,
                &encode_miss_indices,
            );
        }
    }

    /// Pick the iTerm2 or Sixel pixels each latest placement shows, in draw
    /// order, under the preference [`ImageOutputState`] states. A placement
    /// whose units with their framing do not fit the bytes left of
    /// [`MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT`] shows no older pixels, and shows
    /// the unavailable-image text when its units are exact. Drops each encode
    /// result of a placement the latest frame no longer holds. Returns the
    /// index of each latest paint with no exact result, in draw order.
    fn select_encoded_placements(&mut self) -> Vec<usize> {
        let shown_pixels_by_placement_key = build_pixels_by_placement_key(&self.shown_placements);
        let mut selected_placements = Vec::with_capacity(self.latest_output_paints.len());
        let mut unavailable_placement_keys = Vec::new();
        let mut encode_miss_indices = Vec::new();
        let mut selected_byte_count = FRAME_FRAMING_BYTE_COUNT;
        for (paint_index, output_paint) in self.latest_output_paints.iter().enumerate() {
            let placement_key = output_paint.placement_key;
            let encode_key = &self.latest_encode_inputs.encode_keys[paint_index];
            let encoded_placement = self.encoded_placement_by_key.get(&placement_key);
            let shown_pixels = shown_pixels_by_placement_key.get(&placement_key).copied();
            let exact_units = match encoded_placement {
                Some(encoded_placement) if encoded_placement.encode_key == *encode_key => {
                    match &encoded_placement.encoded_pixels {
                        EncodedPixels::Drawn(output_units) => Some(Arc::clone(output_units)),
                        EncodedPixels::Unavailable => {
                            unavailable_placement_keys.push(placement_key);
                            continue;
                        }
                    }
                }
                _ => shown_pixels.and_then(|shown_pixels| match shown_pixels {
                    SelectedPixels::Units {
                        encode_key: shown_encode_key,
                        output_units,
                    } if shown_encode_key == encode_key => Some(Arc::clone(output_units)),
                    _ => None,
                }),
            };
            let exact_units =
                exact_units.or_else(|| self.encoded_output_cache.find_cached_units(encode_key));
            let is_exact = exact_units.is_some();
            let selected_units = match exact_units {
                Some(output_units) => Some((encode_key.clone(), output_units)),
                None => {
                    encode_miss_indices.push(paint_index);
                    let encoded_stand_in = encoded_placement.and_then(|encoded_placement| {
                        match &encoded_placement.encoded_pixels {
                            EncodedPixels::Drawn(output_units)
                                if has_equal_encode_geometry(
                                    &encoded_placement.encode_key,
                                    encode_key,
                                ) =>
                            {
                                Some((
                                    encoded_placement.encode_key.clone(),
                                    Arc::clone(output_units),
                                ))
                            }
                            _ => None,
                        }
                    });
                    encoded_stand_in.or_else(|| {
                        shown_pixels.and_then(|shown_pixels| match shown_pixels {
                            SelectedPixels::Units {
                                encode_key: shown_encode_key,
                                output_units,
                            } if has_equal_encode_geometry(shown_encode_key, encode_key) => {
                                Some((shown_encode_key.clone(), Arc::clone(output_units)))
                            }
                            _ => None,
                        })
                    })
                }
            };
            let Some((pixels_encode_key, output_units)) = selected_units else {
                continue;
            };
            let next_selected_byte_count =
                output_units
                    .iter()
                    .try_fold(selected_byte_count, |byte_count, output_unit| {
                        byte_count
                            .checked_add(output_unit.output_bytes.len())?
                            .checked_add(UNIT_FRAMING_BYTE_COUNT)
                    });
            match next_selected_byte_count {
                Some(byte_count) if byte_count <= MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT => {
                    selected_byte_count = byte_count;
                }
                _ => {
                    if is_exact {
                        unavailable_placement_keys.push(placement_key);
                    }
                    continue;
                }
            }
            selected_placements.push(SelectedPlacement {
                placement_key,
                target_area: output_paint.target_area,
                covered_cells: self
                    .latest_encode_inputs
                    .covered_cells
                    .get(paint_index)
                    .cloned(),
                pixels: SelectedPixels::Units {
                    encode_key: pixels_encode_key,
                    output_units,
                },
            });
        }
        let latest_placement_keys = self
            .latest_output_paints
            .iter()
            .map(|output_paint| output_paint.placement_key)
            .collect::<HashSet<_>>();
        self.encoded_placement_by_key
            .retain(|placement_key, _| latest_placement_keys.contains(placement_key));
        self.selected_placements = selected_placements;
        self.unavailable_placement_keys = unavailable_placement_keys;
        encode_miss_indices
    }

    /// Queue the encode of each latest paint in `encode_miss_indices` that
    /// the active request does not encode under the same placement and key.
    /// The new request carries every latest paint, and encodes only those.
    /// A waiting request that encodes the same placements and keys stays.
    /// Any other waiting request is dropped, and so is the waiting request
    /// when no paint needs a new encode.
    fn request_latest_frame_encode(
        &mut self,
        output_kind: ImageOutputKind,
        pixel_cell_size: PixelCellSize,
        measured_pixel_cell_size: Option<PixelCellSize>,
        cell_snapshot: Option<Arc<ImageCellSnapshot>>,
        encode_miss_indices: &[usize],
    ) {
        let mut encode_keys: Vec<Option<EncodeKey>> = vec![None; self.latest_output_paints.len()];
        let mut requested_encode_keys = Vec::new();
        {
            let active_encode_keys: HashSet<(ImagePlacementKey, &EncodeKey)> = self
                .active_job
                .iter()
                .filter(|active_job| !active_job.is_cancellation_requested.load(Ordering::Acquire))
                .flat_map(|active_job| {
                    active_job
                        .requested_encode_keys
                        .iter()
                        .map(|(placement_key, encode_key)| (*placement_key, encode_key))
                })
                .collect();
            for paint_index in encode_miss_indices {
                let placement_key = self.latest_output_paints[*paint_index].placement_key;
                let encode_key = &self.latest_encode_inputs.encode_keys[*paint_index];
                if active_encode_keys.contains(&(placement_key, encode_key)) {
                    continue;
                }
                encode_keys[*paint_index] = Some(encode_key.clone());
                requested_encode_keys.push((placement_key, encode_key.clone()));
            }
        }
        if requested_encode_keys.is_empty() {
            self.drop_pending_worker_request();
            return;
        }
        if self
            .pending_worker_request
            .as_ref()
            .is_some_and(|pending_worker_request| {
                pending_worker_request.list_requested_encode_keys() == requested_encode_keys
            })
        {
            return;
        }
        self.drop_pending_worker_request();
        let worker_request = WorkerRequest {
            frame_generation: self.advance_frame_generation(),
            output_kind,
            pixel_cell_size,
            measured_pixel_cell_size,
            cell_snapshot,
            output_paints: self.latest_output_paints.clone(),
            plan_outcomes: self.latest_encode_inputs.plan_outcomes.clone(),
            encode_keys,
            kitty_upload_requests: Vec::new(),
            is_cancellation_requested: Arc::new(AtomicBool::new(false)),
        };
        self.queue_worker_request(worker_request);
    }

    /// Pick the Kitty image each latest placement shows, in draw order, under
    /// the preference [`ImageOutputState`] states. A held image, or a finished
    /// upload, is exact for every paint of the same image. The next paint
    /// writes the finished uploads it places, until their bytes and the
    /// framing of every latest paint would pass
    /// [`MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT`]; a placement whose upload does
    /// not fit shows its older pixels or none, and the paint after writes it.
    ///
    /// Every held image that no selected placement shows joins
    /// `kitty_image_ids_to_delete`. Finished uploads and failed images the
    /// latest frame no longer holds are dropped. Returns each latest image
    /// that is neither held, finished, nor failed, once, in paint order.
    fn select_kitty_placements(&mut self) -> Vec<(u64, Arc<DecodedImage>)> {
        let shown_pixels_by_placement_key = build_pixels_by_placement_key(&self.shown_placements);
        let mut selected_placements = Vec::with_capacity(self.latest_output_paints.len());
        let mut unavailable_placement_keys = Vec::new();
        let mut selected_kitty_upload_ids = Vec::new();
        let mut shown_kitty_image_ids = HashSet::new();
        let mut needed_kitty_images: Vec<(u64, Arc<DecodedImage>)> = Vec::new();
        let mut needed_image_content_ids = HashSet::new();
        let mut upload_byte_count = self
            .latest_output_paints
            .len()
            .saturating_mul(UNIT_FRAMING_BYTE_COUNT)
            .saturating_add(FRAME_FRAMING_BYTE_COUNT);
        for (paint_index, output_paint) in self.latest_output_paints.iter().enumerate() {
            let placement_key = output_paint.placement_key;
            let encode_key = &self.latest_encode_inputs.encode_keys[paint_index];
            let image_content_id = output_paint.image_content_id;
            let mut exact_kitty_image_id = self
                .held_kitty_image_id_by_content_id
                .get(&image_content_id)
                .copied();
            let mut is_upload_waiting = false;
            if exact_kitty_image_id.is_none() {
                if let Some(finished_kitty_upload) =
                    self.finished_kitty_uploads
                        .iter()
                        .find(|finished_kitty_upload| {
                            finished_kitty_upload.image_content_id == image_content_id
                        })
                {
                    let kitty_image_id = finished_kitty_upload.kitty_image_id;
                    if selected_kitty_upload_ids.contains(&kitty_image_id) {
                        exact_kitty_image_id = Some(kitty_image_id);
                    } else {
                        match upload_byte_count
                            .checked_add(finished_kitty_upload.upload_bytes.len())
                        {
                            Some(byte_count)
                                if byte_count <= MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT =>
                            {
                                upload_byte_count = byte_count;
                                selected_kitty_upload_ids.push(kitty_image_id);
                                exact_kitty_image_id = Some(kitty_image_id);
                            }
                            _ => is_upload_waiting = true,
                        }
                    }
                }
            }
            let (kitty_image_id, pixels_encode_key, image) = match exact_kitty_image_id {
                Some(kitty_image_id) => (
                    kitty_image_id,
                    encode_key.clone(),
                    Arc::clone(&output_paint.image_record.image),
                ),
                None => {
                    if self
                        .failed_kitty_image_content_ids
                        .contains(&image_content_id)
                    {
                        unavailable_placement_keys.push(placement_key);
                        continue;
                    }
                    if !is_upload_waiting && needed_image_content_ids.insert(image_content_id) {
                        needed_kitty_images.push((
                            image_content_id,
                            Arc::clone(&output_paint.image_record.image),
                        ));
                    }
                    let shown_stand_in = shown_pixels_by_placement_key
                        .get(&placement_key)
                        .and_then(|shown_pixels| match shown_pixels {
                            SelectedPixels::Kitty {
                                encode_key: shown_encode_key,
                                kitty_placement,
                                image,
                            } if has_equal_encode_geometry(shown_encode_key, encode_key) => Some((
                                kitty_placement.image_id,
                                shown_encode_key.clone(),
                                Arc::clone(image),
                            )),
                            _ => None,
                        });
                    let Some(shown_stand_in) = shown_stand_in else {
                        continue;
                    };
                    shown_stand_in
                }
            };
            let Ok(kitty_placement_id) = u32::try_from(paint_index + 1) else {
                unavailable_placement_keys.push(placement_key);
                continue;
            };
            shown_kitty_image_ids.insert(kitty_image_id);
            selected_placements.push(SelectedPlacement {
                placement_key,
                target_area: output_paint.target_area,
                covered_cells: None,
                pixels: SelectedPixels::Kitty {
                    encode_key: pixels_encode_key,
                    kitty_placement: build_kitty_placement(
                        kitty_image_id,
                        kitty_placement_id,
                        output_paint,
                    ),
                    image,
                },
            });
        }
        let departed_kitty_images = self
            .held_kitty_image_id_by_content_id
            .iter()
            .filter(|(_, kitty_image_id)| !shown_kitty_image_ids.contains(*kitty_image_id))
            .map(|(image_content_id, kitty_image_id)| (*image_content_id, *kitty_image_id))
            .collect::<Vec<_>>();
        for (image_content_id, kitty_image_id) in departed_kitty_images {
            self.held_kitty_image_id_by_content_id
                .remove(&image_content_id);
            self.kitty_image_ids_to_delete.push(kitty_image_id);
        }
        let latest_image_content_ids = self
            .latest_output_paints
            .iter()
            .map(|output_paint| output_paint.image_content_id)
            .collect::<HashSet<_>>();
        self.finished_kitty_uploads.retain(|finished_kitty_upload| {
            latest_image_content_ids.contains(&finished_kitty_upload.image_content_id)
        });
        self.failed_kitty_image_content_ids
            .retain(|image_content_id| latest_image_content_ids.contains(image_content_id));
        self.selected_placements = selected_placements;
        self.unavailable_placement_keys = unavailable_placement_keys;
        self.selected_kitty_upload_ids = selected_kitty_upload_ids;
        needed_kitty_images
    }

    /// Queue the upload of each image in `needed_kitty_images` that the
    /// active request does not upload. A waiting request that uploads the
    /// same images stays; any other waiting request is dropped, and an image
    /// it named keeps its id in the new request.
    ///
    /// A new image takes an id from [`Self::take_free_kitty_image_id`] that no
    /// held image, finished upload, active or waiting upload, or queued delete
    /// holds. An image that finds no free id joins
    /// `failed_kitty_image_content_ids`. Each upload carries the zlib payload
    /// the encode cache keeps for its image, and the worker compresses only an
    /// image that has none.
    fn request_kitty_uploads(
        &mut self,
        needed_kitty_images: Vec<(u64, Arc<DecodedImage>)>,
        pixel_cell_size: PixelCellSize,
    ) {
        let active_image_content_ids = self
            .active_job
            .as_ref()
            .filter(|active_job| !active_job.is_cancellation_requested.load(Ordering::Acquire))
            .map(|active_job| {
                active_job
                    .requested_kitty_images
                    .iter()
                    .map(|(image_content_id, _)| *image_content_id)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let needed_kitty_images = needed_kitty_images
            .into_iter()
            .filter(|(image_content_id, _)| !active_image_content_ids.contains(image_content_id))
            .collect::<Vec<_>>();
        if needed_kitty_images.is_empty() {
            self.drop_pending_worker_request();
            return;
        }
        let mut reserved_kitty_image_ids = self
            .held_kitty_image_id_by_content_id
            .values()
            .copied()
            .chain(
                self.finished_kitty_uploads
                    .iter()
                    .map(|finished_kitty_upload| finished_kitty_upload.kitty_image_id),
            )
            .chain(self.active_job.iter().flat_map(|active_job| {
                active_job
                    .requested_kitty_images
                    .iter()
                    .map(|(_, kitty_image_id)| *kitty_image_id)
            }))
            .chain(
                self.pending_worker_request
                    .iter()
                    .flat_map(|pending_worker_request| {
                        pending_worker_request
                            .kitty_upload_requests
                            .iter()
                            .map(|kitty_upload_request| kitty_upload_request.kitty_image_id)
                    }),
            )
            .chain(self.kitty_image_ids_to_delete.iter().copied())
            .collect::<HashSet<_>>();
        let pending_worker_request = self.pending_worker_request.take();
        let pending_kitty_image_id_by_content_id = pending_worker_request
            .as_ref()
            .map(|pending_worker_request| {
                pending_worker_request
                    .list_requested_kitty_images()
                    .into_iter()
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        if let Some(pending_worker_request) = pending_worker_request {
            let is_pending_request_latest = pending_worker_request.kitty_upload_requests.len()
                == needed_kitty_images.len()
                && pending_worker_request
                    .kitty_upload_requests
                    .iter()
                    .zip(&needed_kitty_images)
                    .all(|(kitty_upload_request, (image_content_id, _))| {
                        kitty_upload_request.image_content_id == *image_content_id
                    });
            if is_pending_request_latest {
                self.pending_worker_request = Some(pending_worker_request);
                return;
            }
            pending_worker_request
                .is_cancellation_requested
                .store(true, Ordering::Release);
        }
        let mut kitty_upload_requests = Vec::with_capacity(needed_kitty_images.len());
        for (image_content_id, image) in needed_kitty_images {
            let kitty_image_id = match pending_kitty_image_id_by_content_id.get(&image_content_id) {
                Some(kitty_image_id) => *kitty_image_id,
                None => match self.take_free_kitty_image_id(&mut reserved_kitty_image_ids) {
                    Some(kitty_image_id) => kitty_image_id,
                    None => {
                        self.failed_kitty_image_content_ids.insert(image_content_id);
                        self.has_unselected_results = true;
                        continue;
                    }
                },
            };
            let compressed_bytes = self
                .encoded_output_cache
                .find_kitty_compressed_bytes(image_content_id);
            kitty_upload_requests.push(KittyUploadRequest {
                image_content_id,
                kitty_image_id,
                image,
                compressed_bytes,
            });
        }
        if kitty_upload_requests.is_empty() {
            return;
        }
        let worker_request = WorkerRequest {
            frame_generation: self.advance_frame_generation(),
            output_kind: ImageOutputKind::Kitty,
            pixel_cell_size,
            measured_pixel_cell_size: None,
            cell_snapshot: None,
            output_paints: Vec::new(),
            plan_outcomes: Vec::new(),
            encode_keys: Vec::new(),
            kitty_upload_requests,
            is_cancellation_requested: Arc::new(AtomicBool::new(false)),
        };
        self.queue_worker_request(worker_request);
    }

    /// Set `needs_screen_reset` when the host screen is unknown, or when a
    /// placement the host shows leaves, moves, or changes its pixels in the
    /// next paint. A placement whose covered cells change, and a new
    /// placement, need no reset: the paint writes every selected placement
    /// again over its own cells.
    fn update_screen_reset(&mut self) {
        let selected_placement_by_key = self
            .selected_placements
            .iter()
            .map(|selected_placement| (selected_placement.placement_key, selected_placement))
            .collect::<HashMap<_, _>>();
        self.needs_screen_reset = self.is_host_screen_unknown
            || self.shown_placements.iter().any(|shown_placement| {
                selected_placement_by_key
                    .get(&shown_placement.placement_key)
                    .is_none_or(|selected_placement| {
                        selected_placement.target_area != shown_placement.target_area
                            || selected_placement.pixels != shown_placement.pixels
                    })
            });
    }

    /// Receive worker results without waiting.
    ///
    /// A finished request's results join the encode results or the finished
    /// Kitty uploads, and the waiting request starts. A failed request, or one
    /// whose output passed [`MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT`], makes each
    /// of its placements, or each of its Kitty images, unavailable for the
    /// keys it named. A worker that is gone fails the active and the waiting
    /// request. Results of a cancelled request are dropped.
    pub(crate) fn poll(&mut self) {
        loop {
            let worker_message = match self.worker_messages.as_ref().map(Receiver::try_recv) {
                Some(Ok(worker_message)) => worker_message,
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    if let Some(active_job) = self.active_job.take() {
                        self.fail_requested_images(
                            &active_job.requested_encode_keys,
                            &active_job.requested_kitty_images,
                        );
                    }
                    if let Some(pending_worker_request) = self.pending_worker_request.take() {
                        self.fail_worker_request(&pending_worker_request);
                    }
                    break;
                }
            };
            match worker_message {
                WorkerMessage::Prepared {
                    frame_generation,
                    placement_key,
                } => {
                    if let Some(active_job) = self.find_active_job(frame_generation) {
                        active_job
                            .received_pixels_by_placement_key
                            .insert(placement_key, ReceivedPixels::Drawn(Vec::new()));
                    }
                }
                WorkerMessage::Unavailable {
                    frame_generation,
                    placement_key,
                } => {
                    if let Some(active_job) = self.find_active_job(frame_generation) {
                        active_job
                            .received_pixels_by_placement_key
                            .insert(placement_key, ReceivedPixels::Unavailable);
                    }
                }
                WorkerMessage::Unit(output_unit) => {
                    if let Some(active_job) = self.find_active_job(output_unit.frame_generation) {
                        if active_job.count_received_bytes(output_unit.output_bytes.len()) {
                            if let Some(ReceivedPixels::Drawn(output_units)) = active_job
                                .received_pixels_by_placement_key
                                .get_mut(&output_unit.placement_key)
                            {
                                output_units.push(TemplateUnit {
                                    tile_offset: output_unit.tile_offset,
                                    output_bytes: output_unit.output_bytes,
                                });
                            }
                        }
                    }
                }
                WorkerMessage::KittyUploaded {
                    frame_generation,
                    finished_kitty_upload,
                    compressed_bytes,
                } => {
                    if let Some(active_job) = self.find_active_job(frame_generation) {
                        if active_job.count_received_bytes(finished_kitty_upload.upload_bytes.len())
                        {
                            active_job
                                .received_kitty_compressions
                                .push((finished_kitty_upload.image_content_id, compressed_bytes));
                            active_job
                                .received_kitty_uploads
                                .push(finished_kitty_upload);
                        }
                    }
                }
                WorkerMessage::Finished {
                    frame_generation,
                    has_failed,
                } => {
                    if let Some(active_job) = self
                        .active_job
                        .take_if(|active_job| active_job.frame_generation == frame_generation)
                    {
                        self.finish_active_job(active_job, has_failed);
                        if let Some(worker_request) = self.pending_worker_request.take() {
                            self.start_worker_request(worker_request);
                        }
                    }
                }
            }
        }
    }

    /// Return the active job when it runs `frame_generation`, and its results
    /// are kept.
    fn find_active_job(&mut self, frame_generation: u64) -> Option<&mut ActiveJob> {
        self.active_job.as_mut().filter(|active_job| {
            active_job.frame_generation == frame_generation && !active_job.has_failed
        })
    }

    /// Adopt the results of `active_job`, which the worker ended with
    /// `has_failed`. Each drawn placement and each Kitty zlib payload also
    /// joins the encode cache.
    fn finish_active_job(&mut self, mut active_job: ActiveJob, has_failed: bool) {
        if active_job.is_cancellation_requested.load(Ordering::Acquire) && !active_job.has_failed {
            return;
        }
        if has_failed || active_job.has_failed {
            self.fail_requested_images(
                &active_job.requested_encode_keys,
                &active_job.requested_kitty_images,
            );
            return;
        }
        for (placement_key, encode_key) in active_job.requested_encode_keys {
            let encoded_pixels = match active_job
                .received_pixels_by_placement_key
                .remove(&placement_key)
            {
                Some(ReceivedPixels::Drawn(output_units)) => {
                    let output_units: Arc<[TemplateUnit]> = Arc::from(output_units);
                    self.encoded_output_cache
                        .insert_cached_units(encode_key.clone(), Arc::clone(&output_units));
                    EncodedPixels::Drawn(output_units)
                }
                Some(ReceivedPixels::Unavailable) | None => EncodedPixels::Unavailable,
            };
            self.encoded_placement_by_key.insert(
                placement_key,
                EncodedPlacement {
                    encode_key,
                    encoded_pixels,
                },
            );
        }
        for (image_content_id, compressed_bytes) in active_job.received_kitty_compressions {
            self.encoded_output_cache
                .insert_kitty_compressed_bytes(image_content_id, compressed_bytes);
        }
        self.finished_kitty_uploads
            .append(&mut active_job.received_kitty_uploads);
        self.has_unselected_results = true;
    }

    /// Make each placement in `requested_encode_keys` unavailable for its
    /// key, and each image in `requested_kitty_images` failed.
    fn fail_requested_images(
        &mut self,
        requested_encode_keys: &[(ImagePlacementKey, EncodeKey)],
        requested_kitty_images: &[(u64, u32)],
    ) {
        for (placement_key, encode_key) in requested_encode_keys {
            self.encoded_placement_by_key.insert(
                *placement_key,
                EncodedPlacement {
                    encode_key: encode_key.clone(),
                    encoded_pixels: EncodedPixels::Unavailable,
                },
            );
        }
        self.failed_kitty_image_content_ids.extend(
            requested_kitty_images
                .iter()
                .map(|(image_content_id, _)| *image_content_id),
        );
        self.has_unselected_results = true;
    }

    /// Fail every image `worker_request` names, as [`Self::fail_requested_images`] does.
    fn fail_worker_request(&mut self, worker_request: &WorkerRequest) {
        self.fail_requested_images(
            &worker_request.list_requested_encode_keys(),
            &worker_request.list_requested_kitty_images(),
        );
    }

    /// Return whether the next paint shows other pixels than the host shows,
    /// or must clear a host screen that is unknown.
    pub(crate) fn is_native_commit_pending(&self) -> bool {
        self.is_host_screen_unknown || self.selected_placements != self.shown_placements
    }

    /// Write the host-side reset that precedes the newest frame's base cells.
    ///
    /// Writes, in order: the abort of an open image string, the Kitty deletes
    /// (every image when a failed or replaced connection made the host state
    /// unknown, otherwise the visible placements when they change, plus each
    /// image id that left the frame, as `a=d,d=I,i=<id>`), and, for Sixel and
    /// iTerm2 output only, `ESC[2J` when the host's pixels change or are
    /// unknown. Kitty output never writes `ESC[2J`. Nothing flushes `writer`.
    /// Returns whether the caller must redraw every text cell.
    pub(crate) fn write_frame_reset<W: Write>(&mut self, writer: &mut W) -> io::Result<bool> {
        let has_pending_kitty_cleanup =
            self.should_free_all_kitty_images || !self.kitty_image_ids_to_delete.is_empty();
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
                self.kitty_image_ids_to_delete.clear();
            } else {
                if self.needs_screen_reset {
                    write_kitty_visible_placement_delete(writer)
                        .map_err(convert_kitty_output_error)?;
                }
                for kitty_image_id in self.kitty_image_ids_to_delete.drain(..) {
                    write_kitty_image_delete(writer, kitty_image_id)
                        .map_err(convert_kitty_output_error)?;
                }
            }
        }
        let should_clear_screen = self.needs_screen_reset && !is_kitty_output;
        if should_clear_screen {
            writer.write_all(SCREEN_RESET_BYTES)?;
        }
        Ok(should_clear_screen)
    }

    /// Build all native bytes written after the newest base-cell frame: the
    /// selected finished Kitty uploads, then every selected placement in draw
    /// order at its target area. After each iTerm2 or Sixel unit, and once
    /// after Kitty output that is not empty, the cursor moves back to
    /// `cursor_position`, or is hidden when `cursor_position` is `None`.
    pub(crate) fn build_frame_output(
        &self,
        cursor_position: Option<Position>,
    ) -> io::Result<Vec<u8>> {
        let mut output_bytes = Vec::new();
        let unit_count = self
            .selected_placements
            .iter()
            .map(|selected_placement| match &selected_placement.pixels {
                SelectedPixels::Units { output_units, .. } => output_units.len(),
                SelectedPixels::Kitty { .. } => 1,
            })
            .sum::<usize>();
        let selected_byte_count = self
            .selected_placements
            .iter()
            .flat_map(|selected_placement| match &selected_placement.pixels {
                SelectedPixels::Units { output_units, .. } => output_units.as_ref(),
                SelectedPixels::Kitty { .. } => &[],
            })
            .map(|output_unit| output_unit.output_bytes.len())
            .chain(
                self.list_selected_kitty_uploads()
                    .map(|finished_kitty_upload| finished_kitty_upload.upload_bytes.len()),
            )
            .fold(0usize, usize::saturating_add);
        let framing_byte_count = unit_count
            .saturating_mul(UNIT_FRAMING_BYTE_COUNT)
            .saturating_add(FRAME_FRAMING_BYTE_COUNT);
        output_bytes
            .try_reserve(selected_byte_count.saturating_add(framing_byte_count))
            .map_err(|_| {
                build_invalid_output_error("image output storage could not be allocated")
            })?;
        for finished_kitty_upload in self.list_selected_kitty_uploads() {
            output_bytes.extend_from_slice(&finished_kitty_upload.upload_bytes);
        }
        for selected_placement in &self.selected_placements {
            let target_area = selected_placement.target_area;
            match &selected_placement.pixels {
                SelectedPixels::Kitty {
                    kitty_placement,
                    image,
                    ..
                } => {
                    write_cursor_position(&mut output_bytes, target_area.x, target_area.y)?;
                    write_kitty_placement(&mut output_bytes, image, kitty_placement)
                        .map_err(convert_kitty_output_error)?;
                }
                SelectedPixels::Units { output_units, .. } => {
                    let is_sixel_output = self.output_kind.is_some_and(ImageOutputKind::is_sixel);
                    for output_unit in output_units.iter() {
                        let screen_column = target_area
                            .x
                            .checked_add(output_unit.tile_offset.0)
                            .ok_or_else(|| {
                                build_invalid_output_error(
                                    "image tile x coordinate overflows the frame",
                                )
                            })?;
                        let screen_row = target_area
                            .y
                            .checked_add(output_unit.tile_offset.1)
                            .ok_or_else(|| {
                                build_invalid_output_error(
                                    "image tile y coordinate overflows the frame",
                                )
                            })?;
                        if is_sixel_output {
                            output_bytes.extend_from_slice(SIXEL_MODE_RESET_BYTES);
                        }
                        write_cursor_position(&mut output_bytes, screen_column, screen_row)?;
                        output_bytes.extend_from_slice(&output_unit.output_bytes);
                        restore_cursor_state(&mut output_bytes, cursor_position)?;
                        if is_sixel_output {
                            output_bytes.extend_from_slice(SIXEL_MODE_RESTORE_BYTES);
                        }
                    }
                }
            }
        }
        if matches!(self.output_kind, Some(ImageOutputKind::Kitty)) && !output_bytes.is_empty() {
            restore_cursor_state(&mut output_bytes, cursor_position)?;
        }
        if output_bytes.len() > MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT {
            return Err(build_invalid_output_error(
                "native image frame exceeds its output limit",
            ));
        }
        Ok(output_bytes)
    }

    /// Return the finished Kitty uploads the next paint writes, in the order
    /// the worker finished them.
    fn list_selected_kitty_uploads(&self) -> impl Iterator<Item = &FinishedKittyUpload> {
        self.finished_kitty_uploads
            .iter()
            .filter(|finished_kitty_upload| {
                self.selected_kitty_upload_ids
                    .contains(&finished_kitty_upload.kitty_image_id)
            })
    }

    /// Adopt the selected pixels as the host's after the frame's base cells
    /// and native bytes are written. Each written Kitty upload becomes a held
    /// image. Finished Kitty uploads that did not fit the frame set
    /// `has_unselected_results` and call `notify_image_output_ready`.
    pub(crate) fn commit_frame(&mut self) {
        self.shown_placements.clone_from(&self.selected_placements);
        self.has_host_pixels = !self.shown_placements.is_empty();
        self.is_host_screen_unknown = false;
        self.needs_screen_reset = false;
        let selected_kitty_upload_ids = std::mem::take(&mut self.selected_kitty_upload_ids);
        let mut written_kitty_uploads = Vec::new();
        self.finished_kitty_uploads.retain(|finished_kitty_upload| {
            if selected_kitty_upload_ids.contains(&finished_kitty_upload.kitty_image_id) {
                written_kitty_uploads.push((
                    finished_kitty_upload.image_content_id,
                    finished_kitty_upload.kitty_image_id,
                ));
                false
            } else {
                true
            }
        });
        self.held_kitty_image_id_by_content_id
            .extend(written_kitty_uploads);
        if !self.finished_kitty_uploads.is_empty() {
            self.has_unselected_results = true;
            (self.notify_image_output_ready)();
        }
    }

    /// Mark the host screen unknown after a native output failure. The next
    /// paint aborts any open image string, clears the screen, frees every
    /// Kitty image, and draws every cell and image again. Encode results and
    /// finished Kitty uploads are kept.
    pub(crate) fn fail_frame_commit(&mut self) {
        self.needs_image_abort = self.output_kind.is_some();
        self.selected_kitty_upload_ids.clear();
        self.is_host_screen_unknown = true;
        self.needs_screen_reset = true;
        self.clear_host_images();
    }

    /// Reset this output state when a connection is replaced, when the
    /// session resets its image records, or when the cell size changes. The
    /// active request is cancelled and its results are dropped; the waiting
    /// request, every encode result, every finished upload and the whole
    /// encode cache are dropped. A host that may show image pixels becomes
    /// unknown: the next paint clears it and draws every cell and image again
    /// ([`Self::clear_host_images`]).
    pub(crate) fn reset_connection(&mut self) {
        if let Some(active_job) = self.active_job.as_ref() {
            active_job
                .is_cancellation_requested
                .store(true, Ordering::Release);
        }
        self.drop_pending_worker_request();
        self.advance_frame_generation();
        self.latest_output_paints.clear();
        self.latest_encode_inputs = FrameEncodeInputs::default();
        self.is_latest_frame_unavailable = false;
        self.encoded_placement_by_key.clear();
        self.encoded_output_cache.clear_cached_outputs();
        self.finished_kitty_uploads.clear();
        self.selected_kitty_upload_ids.clear();
        self.failed_kitty_image_content_ids.clear();
        self.selected_placements.clear();
        self.unavailable_placement_keys.clear();
        self.is_host_screen_unknown = self.has_host_pixels;
        self.needs_screen_reset = self.has_host_pixels;
        self.alpha_stats_by_image_and_source.clear();
        self.clear_host_images();
    }

    /// Record that the client's image records hold `held_image_byte_count`
    /// bytes of decoded pixels. Cached output past the room those records
    /// leave in the image budget leaves the encode cache, least recently used
    /// first.
    pub(crate) fn set_held_image_byte_count(&mut self, held_image_byte_count: u64) {
        self.encoded_output_cache
            .set_held_image_byte_count(held_image_byte_count);
    }

    /// Drop the cached output of each image in `image_content_ids`, which the
    /// session released, and of each composed paint drawn over one of them.
    pub(crate) fn release_image_contents(&mut self, image_content_ids: &[u64]) {
        self.encoded_output_cache
            .remove_released_outputs(image_content_ids);
    }

    /// Advance `frame_generation` by one, from `u64::MAX` to `1`, and return
    /// it.
    fn advance_frame_generation(&mut self) -> u64 {
        self.frame_generation = self.frame_generation.wrapping_add(1).max(1);
        self.frame_generation
    }

    /// Start `worker_request` when no request is active, and otherwise keep it
    /// as the request that waits to start.
    fn queue_worker_request(&mut self, worker_request: WorkerRequest) {
        if self.active_job.is_none() {
            self.start_worker_request(worker_request);
        } else {
            self.pending_worker_request = Some(worker_request);
        }
    }

    /// Hand `worker_request` to the worker as the active request. A worker
    /// that is gone fails it.
    fn start_worker_request(&mut self, worker_request: WorkerRequest) {
        let active_job = ActiveJob::from_worker_request(&worker_request);
        let Some(worker_request_sender) = &self.worker_request_sender else {
            self.fail_worker_request(&worker_request);
            return;
        };
        match worker_request_sender.send(worker_request) {
            Ok(()) => {
                self.active_job = Some(active_job);
            }
            Err(mpsc::SendError(worker_request)) => {
                self.fail_worker_request(&worker_request);
            }
        }
    }

    /// Drop the request that waits to start.
    fn drop_pending_worker_request(&mut self) {
        if let Some(pending_worker_request) = self.pending_worker_request.take() {
            pending_worker_request
                .is_cancellation_requested
                .store(true, Ordering::Release);
        }
    }

    /// Show no pixels for the frame `image_paints`, and the unavailable-image
    /// text over each of its placements. The active request runs on, and the
    /// waiting request is dropped.
    fn set_unavailable_frame(&mut self, image_paints: &[ImagePaint]) {
        self.drop_pending_worker_request();
        self.latest_output_paints = image_paints
            .iter()
            .map(|image_paint| OutputPaint::from_paint(image_paint, None))
            .collect();
        self.alpha_stats_by_image_and_source.clear();
        self.latest_encode_inputs = FrameEncodeInputs::default();
        self.is_latest_frame_unavailable = true;
        self.has_unselected_results = false;
        self.unavailable_placement_keys = self
            .latest_output_paints
            .iter()
            .map(|output_paint| output_paint.placement_key)
            .collect();
        self.selected_placements.clear();
        self.selected_kitty_upload_ids.clear();
        let held_kitty_image_ids = self
            .held_kitty_image_id_by_content_id
            .drain()
            .map(|(_, kitty_image_id)| kitty_image_id)
            .collect::<Vec<_>>();
        self.kitty_image_ids_to_delete.extend(held_kitty_image_ids);
        self.update_screen_reset();
    }
}

impl Drop for ImageOutputState {
    fn drop(&mut self) {
        if let Some(active_job) = self.active_job.take() {
            active_job
                .is_cancellation_requested
                .store(true, Ordering::Release);
        }
        self.drop_pending_worker_request();
        self.worker_messages.take();
        self.worker_request_sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Return the pixels of each placement in `placements`, by placement key.
fn build_pixels_by_placement_key(
    placements: &[SelectedPlacement],
) -> HashMap<ImagePlacementKey, &SelectedPixels> {
    placements
        .iter()
        .map(|placement| (placement.placement_key, &placement.pixels))
        .collect()
}

/// Return whether `left_encode_key` and `right_encode_key` draw the same
/// source rectangle over the same cells and cell size, at the same z-index,
/// with the same protocol. Pixels encoded for one fit the place of the other.
fn has_equal_encode_geometry(left_encode_key: &EncodeKey, right_encode_key: &EncodeKey) -> bool {
    left_encode_key.source_rect == right_encode_key.source_rect
        && left_encode_key.target_column_count == right_encode_key.target_column_count
        && left_encode_key.target_row_count == right_encode_key.target_row_count
        && left_encode_key.pixel_cell_size == right_encode_key.pixel_cell_size
        && left_encode_key.output_kind == right_encode_key.output_kind
        && left_encode_key.z_index == right_encode_key.z_index
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
        source_rect: ImageSourceKey::from_source_rect(output_paint.source_rect),
        target_column_count: output_paint.target_area.width,
        target_row_count: output_paint.target_area.height,
        pixel_cell_size,
        output_kind,
        z_index: output_paint.z_index,
        has_terminal_background: has_terminal_background(output_paint),
        composition_input: None,
    }
}

/// Return whether `output_paint`'s Sixel pixels with zero alpha show the
/// terminal background.
fn has_terminal_background(output_paint: &OutputPaint) -> bool {
    output_paint.image_record.display.sixel_background == Some(SixelBackground::Terminal)
}

/// Build the encode inputs of `output_paints`, one frame's paints in draw
/// order, for `output_kind`.
///
/// On an output that draws over cells, each paint is classified against
/// `cell_snapshot` and the paints below it, and the cells under its target
/// area are read from `cell_snapshot`. A paint whose pixels read those cells
/// ([`OutputPaint::needs_target_cell_composition`]) keys its classification,
/// `measured_pixel_cell_size`, its covered cells, and each lower paint whose
/// target area shares a cell with its own. Every other paint keys only its
/// image and geometry. Kitty paints are not classified.
///
/// Returns `None` when a paint's alpha coverage is unknown.
fn build_frame_encode_inputs(
    output_kind: ImageOutputKind,
    pixel_cell_size: PixelCellSize,
    measured_pixel_cell_size: Option<PixelCellSize>,
    cell_snapshot: Option<&ImageCellSnapshot>,
    output_paints: &[OutputPaint],
) -> Option<FrameEncodeInputs> {
    let mut encode_keys = output_paints
        .iter()
        .map(|output_paint| build_output_encode_key(output_kind, pixel_cell_size, output_paint))
        .collect::<Vec<_>>();
    let Some(cell_snapshot) = cell_snapshot.filter(|_| output_kind.needs_cell_composition()) else {
        return Some(FrameEncodeInputs {
            encode_keys,
            plan_outcomes: Vec::new(),
            covered_cells: Vec::new(),
        });
    };
    let plan_outcomes = classify_frame_paints(
        output_kind,
        cell_snapshot,
        output_paints,
        measured_pixel_cell_size,
    )?
    .iter()
    .map(|image_plan| image_plan.outcome)
    .collect::<Vec<_>>();
    let covered_cells = output_paints
        .iter()
        .map(|output_paint| list_covered_cells(cell_snapshot, output_paint.target_area))
        .collect::<Vec<_>>();
    for (paint_index, output_paint) in output_paints.iter().enumerate() {
        if !output_paint.needs_target_cell_composition(output_kind) {
            continue;
        }
        let lower_layers = output_paints[..paint_index]
            .iter()
            .zip(&plan_outcomes)
            .filter(|(lower_output_paint, _)| {
                lower_output_paint
                    .target_area
                    .intersects(output_paint.target_area)
            })
            .map(|(lower_output_paint, lower_plan_outcome)| LowerLayer {
                image_content_id: lower_output_paint.image_content_id,
                source_rect: ImageSourceKey::from_source_rect(lower_output_paint.source_rect),
                column_offset: i32::from(lower_output_paint.target_area.x)
                    - i32::from(output_paint.target_area.x),
                row_offset: i32::from(lower_output_paint.target_area.y)
                    - i32::from(output_paint.target_area.y),
                column_count: lower_output_paint.target_area.width,
                row_count: lower_output_paint.target_area.height,
                has_terminal_background: has_terminal_background(lower_output_paint),
                plan_outcome: *lower_plan_outcome,
            })
            .collect();
        encode_keys[paint_index].composition_input = Some(Arc::new(CompositionInput {
            plan_outcome: plan_outcomes[paint_index],
            measured_pixel_cell_size,
            covered_cells: Arc::clone(&covered_cells[paint_index]),
            lower_layers,
        }));
    }
    Some(FrameEncodeInputs {
        encode_keys,
        plan_outcomes,
        covered_cells,
    })
}

/// Classify each of `output_paints` for `output_kind` over `cell_snapshot`
/// and the paints below it, then settle the compatibility each paint takes
/// from the paints below it. Returns `None` when a paint's alpha coverage is
/// unknown.
fn classify_frame_paints<'a>(
    output_kind: ImageOutputKind,
    cell_snapshot: &ImageCellSnapshot,
    output_paints: &'a [OutputPaint],
    measured_pixel_cell_size: Option<PixelCellSize>,
) -> Option<Vec<Plan<'a>>> {
    let mut image_plans = Vec::with_capacity(output_paints.len());
    for (paint_index, output_paint) in output_paints.iter().enumerate() {
        let image_plan = classify_output_paint(
            output_kind,
            cell_snapshot,
            &output_paints[..paint_index],
            output_paint,
        )
        .ok()?;
        image_plans.push(image_plan);
    }
    resolve_image_compatibility(output_kind, measured_pixel_cell_size, &mut image_plans);
    Some(image_plans)
}

/// Return the cells of `cell_snapshot` under `target_area`, row by row.
/// `None` marks a cell outside `cell_snapshot`.
fn list_covered_cells(
    cell_snapshot: &ImageCellSnapshot,
    target_area: Rect,
) -> Arc<[Option<ImageCellState>]> {
    (target_area.y..target_area.bottom())
        .flat_map(|row_index| {
            (target_area.x..target_area.right())
                .map(move |column_index| cell_snapshot.find_cell(column_index, row_index).cloned())
        })
        .collect()
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
    if !is_source_rect_in_image(decoded_image, source_rect) {
        return None;
    }
    let image_pixel_width = usize::try_from(decoded_image.pixel_width).ok()?;
    let mut alpha_stats = AlphaStats::default();
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
                0 => alpha_stats.has_zero = true,
                255 => {}
                _ => alpha_stats.has_partial = true,
            }
        }
    }
    Some(alpha_stats)
}

/// Return whether `decoded_image` holds exactly the RGBA bytes its pixel size
/// names, within the image size limits, and `source_rect` is a nonempty
/// rectangle inside it.
fn is_source_rect_in_image(decoded_image: &DecodedImage, source_rect: ImageSourceRect) -> bool {
    let (Ok(image_pixel_width), Ok(image_pixel_height)) = (
        usize::try_from(decoded_image.pixel_width),
        usize::try_from(decoded_image.pixel_height),
    ) else {
        return false;
    };
    compute_rgba_byte_count(
        GraphicsProtocol::Iterm2,
        image_pixel_width,
        image_pixel_height,
    )
    .is_ok_and(|rgba_byte_count| rgba_byte_count == decoded_image.rgba_bytes.len())
        && source_rect.pixel_width != 0
        && source_rect.pixel_height != 0
        && source_rect
            .pixel_x
            .checked_add(source_rect.pixel_width)
            .is_some_and(|pixel_right| pixel_right <= decoded_image.pixel_width)
        && source_rect
            .pixel_y
            .checked_add(source_rect.pixel_height)
            .is_some_and(|pixel_bottom| pixel_bottom <= decoded_image.pixel_height)
}

#[derive(Debug, Clone, Copy)]
struct CompositionDetails {
    has_glyph: bool,
    has_non_default_background: bool,
    has_known_per_cell_backgrounds: bool,
    solid_background_color: Option<[u8; 3]>,
    is_default_blank: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct ItermComposition {
    background_color: Option<[u8; 3]>,
    needs_per_cell_composition: bool,
    has_known_per_cell_backgrounds: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
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

/// Return whether `cell` shows a glyph: a character other than a space, a
/// wide character, or combining characters.
fn has_cell_glyph(cell: &ImageCellState) -> bool {
    cell.character != ' ' || cell.cell_width != 1 || !cell.combining_characters.is_empty()
}

/// Return the RGB background color of `cell` when the cell shows no glyph and
/// no text attribute. `None` for every other cell, and for a background that
/// is not an RGB color.
fn find_plain_cell_background_color(cell: &ImageCellState) -> Option<[u8; 3]> {
    if has_cell_glyph(cell) || cell.style.get_attributes() != Default::default() {
        return None;
    }
    match cell.style.get_background_color() {
        koshi_terminal::style::Color::Rgb(red, green, blue) => Some([red, green, blue]),
        _ => None,
    }
}

fn compute_composition_details(
    cell_snapshot: &ImageCellSnapshot,
    output_paint: &OutputPaint,
) -> CompositionDetails {
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
            let is_default_cell = cell == &ImageCellState::default();
            has_non_default_background |=
                cell.style.get_background_color() != koshi_terminal::style::Color::Default;
            is_default_blank &= is_default_cell;
            has_glyph |= has_cell_glyph(cell);
            if is_default_cell {
                background_accumulator = BackgroundAccumulator::Incompatible;
                continue;
            }
            let Some(background_color) = find_plain_cell_background_color(cell) else {
                has_known_per_cell_backgrounds = false;
                background_accumulator = BackgroundAccumulator::Incompatible;
                continue;
            };
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
    CompositionDetails {
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

/// One paint and how it draws on the host.
#[derive(Debug)]
struct Plan<'a> {
    output_paint: &'a OutputPaint,
    outcome: PlanOutcome,
}

/// How one paint draws on the host: the composition its encode applies,
/// whether the host shows it exactly, and whether it hides what lies below.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct PlanOutcome {
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
            cell_snapshot
                .find_cell(cell_column, cell_row)
                .and_then(find_plain_cell_background_color)
                .is_some()
        })
    })
}

fn classify_output_paint<'a>(
    output_kind: ImageOutputKind,
    cell_snapshot: &ImageCellSnapshot,
    lower_output_paints: &[OutputPaint],
    output_paint: &'a OutputPaint,
) -> Result<Plan<'a>, TemplateError> {
    let alpha_stats = output_paint.alpha_stats.ok_or(TemplateError::Failed)?;
    let composition_details = compute_composition_details(cell_snapshot, output_paint);
    let is_overlapping_lower = is_overlapping_lower_paint(output_paint, lower_output_paints);
    let has_known_cell_background = has_known_cell_background(cell_snapshot, output_paint);
    let mut image_compatibility = ImageCompatibility {
        has_text_layer_order_mismatch: (output_paint.z_index < 0 && composition_details.has_glyph)
            || (output_paint.z_index < KITTY_BACKGROUND_LAYER_Z_INDEX
                && composition_details.has_non_default_background),
        ..ImageCompatibility::default()
    };
    match output_kind {
        ImageOutputKind::Kitty => Ok(Plan {
            output_paint,
            outcome: PlanOutcome {
                iterm_composition: ItermComposition::default(),
                sixel_composition: SixelComposition::default(),
                image_compatibility,
                is_opaque: alpha_stats.is_fully_opaque(),
            },
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
                    iterm_composition.needs_per_cell_composition = true;
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
                outcome: PlanOutcome {
                    iterm_composition,
                    sixel_composition: SixelComposition::default(),
                    image_compatibility,
                    is_opaque: alpha_stats.is_fully_opaque()
                        || iterm_composition.background_color.is_some(),
                },
            })
        }
        ImageOutputKind::Sixel { .. } => {
            let is_terminal_background_shown = has_terminal_background(output_paint);
            let background_color = if !is_overlapping_lower
                && composition_details.solid_background_color.is_some()
                && ((alpha_stats.has_partial && !alpha_stats.has_zero)
                    || (alpha_stats.has_zero && is_terminal_background_shown))
            {
                composition_details.solid_background_color
            } else {
                None
            };
            let has_alpha = alpha_stats.has_partial && background_color.is_none();
            let has_terminal_background =
                alpha_stats.has_zero && is_terminal_background_shown && background_color.is_none();
            image_compatibility.has_sixel_alpha_mismatch = has_alpha && !has_known_cell_background;
            image_compatibility.has_sixel_terminal_background_mismatch =
                has_terminal_background && !has_known_cell_background;
            Ok(Plan {
                output_paint,
                outcome: PlanOutcome {
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
                },
            })
        }
    }
}

/// Return whether every cell of `output_paint`'s target area lies under an
/// opaque paint in `lower_plans` that the host draws exactly.
fn is_target_covered_by_lower_images(output_paint: &OutputPaint, lower_plans: &[Plan<'_>]) -> bool {
    let mut covered_cell_positions = HashSet::new();
    for lower_plan in lower_plans {
        if !lower_plan.outcome.is_opaque
            || lower_plan.outcome.image_compatibility != ImageCompatibility::default()
        {
            continue;
        }
        let covered_area = output_paint
            .target_area
            .intersection(lower_plan.output_paint.target_area);
        covered_cell_positions.extend(covered_area.positions());
    }
    output_paint
        .target_area
        .positions()
        .all(|cell_position| covered_cell_positions.contains(&cell_position))
}

fn resolve_image_compatibility(
    output_kind: ImageOutputKind,
    measured_pixel_cell_size: Option<PixelCellSize>,
    image_plans: &mut [Plan<'_>],
) {
    for plan_index in 0..image_plans.len() {
        let is_covered_by_lower_images = is_target_covered_by_lower_images(
            image_plans[plan_index].output_paint,
            &image_plans[..plan_index],
        );
        if matches!(output_kind, ImageOutputKind::Iterm) {
            if image_plans[plan_index]
                .outcome
                .iterm_composition
                .needs_per_cell_composition
                && image_plans[plan_index]
                    .outcome
                    .iterm_composition
                    .has_known_per_cell_backgrounds
                && measured_pixel_cell_size.is_some()
            {
                image_plans[plan_index]
                    .outcome
                    .image_compatibility
                    .has_iterm_alpha_mismatch = false;
            }
            continue;
        }
        let has_unavailable_lower_image = image_plans[..plan_index].iter().any(|lower_plan| {
            lower_plan.outcome.image_compatibility != ImageCompatibility::default()
                && lower_plan
                    .output_paint
                    .target_area
                    .intersects(image_plans[plan_index].output_paint.target_area)
        });
        if is_covered_by_lower_images {
            let image_plan = &mut image_plans[plan_index];
            image_plan
                .outcome
                .image_compatibility
                .has_sixel_alpha_mismatch = false;
        } else if has_unavailable_lower_image
            && image_plans[plan_index].outcome.sixel_composition.has_alpha
        {
            image_plans[plan_index]
                .outcome
                .image_compatibility
                .has_sixel_alpha_mismatch = true;
        }
    }
}

/// Return whether a cell of `output_paint`'s target area lies in the target
/// area of a paint in `lower_output_paints`.
fn is_overlapping_lower_paint(
    output_paint: &OutputPaint,
    lower_output_paints: &[OutputPaint],
) -> bool {
    lower_output_paints.iter().any(|lower_output_paint| {
        lower_output_paint
            .target_area
            .intersects(output_paint.target_area)
    })
}

/// Run each worker request to its end, send its `Finished` message, and call
/// `notify_image_output_ready`. A request cancelled before it starts sends
/// only its `Finished` message.
fn run_worker_loop(
    worker_request_receiver: Receiver<WorkerRequest>,
    worker_message_sender: Sender<WorkerMessage>,
    notify_image_output_ready: Arc<dyn Fn() + Send + Sync>,
) {
    while let Ok(worker_request) = worker_request_receiver.recv() {
        let frame_generation = worker_request.frame_generation;
        let has_failed = if worker_request
            .is_cancellation_requested
            .load(Ordering::Acquire)
        {
            false
        } else {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_worker_job(
                    &worker_request,
                    &worker_message_sender,
                    MAX_NATIVE_FRAME_OUTPUT_BYTE_COUNT,
                )
            })) {
                Ok(job_result) => job_result.is_err(),
                Err(_) => true,
            }
        };
        if worker_message_sender
            .send(WorkerMessage::Finished {
                frame_generation,
                has_failed,
            })
            .is_err()
        {
            return;
        }
        notify_image_output_ready();
    }
}

/// Encode the iTerm2 or Sixel paints of `worker_request`, or write its Kitty
/// uploads, and send each result to `worker_message_sender`. The sent output
/// stays within `output_byte_limit` bytes. Returns `Err` when an encode
/// fails, the output passes `output_byte_limit`, or a message cannot be sent
/// because the request is cancelled or the receiver is gone.
fn run_worker_job(
    worker_request: &WorkerRequest,
    worker_message_sender: &Sender<WorkerMessage>,
    output_byte_limit: usize,
) -> Result<(), ()> {
    if matches!(worker_request.output_kind, ImageOutputKind::Kitty) {
        return run_kitty_worker_job(worker_request, worker_message_sender, output_byte_limit);
    }
    let image_plans = worker_request
        .output_paints
        .iter()
        .zip(&worker_request.plan_outcomes)
        .map(|(output_paint, plan_outcome)| Plan {
            output_paint,
            outcome: *plan_outcome,
        })
        .collect::<Vec<_>>();
    let mut encoded_templates: Vec<Option<Vec<TemplateUnit>>> = Vec::new();
    let mut template_index_by_encode_key = HashMap::new();
    let mut template_byte_count = 0usize;
    let mut emitted_byte_count = 0usize;
    for (plan_index, image_plan) in image_plans.iter().enumerate() {
        let Some(encode_key) = &worker_request.encode_keys[plan_index] else {
            continue;
        };
        if worker_request
            .is_cancellation_requested
            .load(Ordering::Acquire)
        {
            return Ok(());
        }
        if image_plan.outcome.image_compatibility != ImageCompatibility::default() {
            send_worker_message(
                worker_message_sender,
                &worker_request.is_cancellation_requested,
                WorkerMessage::Unavailable {
                    frame_generation: worker_request.frame_generation,
                    placement_key: image_plan.output_paint.placement_key,
                },
            )?;
            continue;
        }
        let template_index = if let Some(template_index) =
            template_index_by_encode_key.get(encode_key)
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
                    if worker_request
                        .is_cancellation_requested
                        .load(Ordering::Acquire) =>
                {
                    return Ok(())
                }
                Err(TemplateError::Unavailable) => None,
                Err(TemplateError::Failed) => return Err(()),
            };
            let template_index = encoded_templates.len();
            encoded_templates.push(encoded_template);
            template_index_by_encode_key.insert(encode_key, template_index);
            template_index
        };
        let Some(template_units) = &encoded_templates[template_index] else {
            send_worker_message(
                worker_message_sender,
                &worker_request.is_cancellation_requested,
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
            &worker_request.is_cancellation_requested,
            WorkerMessage::Prepared {
                frame_generation: worker_request.frame_generation,
                placement_key: image_plan.output_paint.placement_key,
            },
        )?;
        for template_unit in template_units {
            send_worker_message(
                worker_message_sender,
                &worker_request.is_cancellation_requested,
                WorkerMessage::Unit(OutputUnit {
                    frame_generation: worker_request.frame_generation,
                    placement_key: image_plan.output_paint.placement_key,
                    tile_offset: template_unit.tile_offset,
                    output_bytes: Arc::clone(&template_unit.output_bytes),
                }),
            )?;
        }
    }
    Ok(())
}

/// Write the transmit of each image of a Kitty request under its id, from the
/// zlib payload the request carries for it, or from a compression of the
/// image when it carries none. Send each upload with its zlib payload as it
/// finishes. The uploads together stay within `output_byte_limit` bytes; past
/// it the request fails.
fn run_kitty_worker_job(
    worker_request: &WorkerRequest,
    worker_message_sender: &Sender<WorkerMessage>,
    output_byte_limit: usize,
) -> Result<(), ()> {
    let mut remaining_output_byte_count = output_byte_limit;
    for kitty_upload_request in &worker_request.kitty_upload_requests {
        let compressed_bytes = match &kitty_upload_request.compressed_bytes {
            Some(kept_compressed_bytes) => Arc::clone(kept_compressed_bytes),
            None => {
                let Some(compressed_bytes) =
                    compress_kitty_image(&kitty_upload_request.image, || {
                        worker_request
                            .is_cancellation_requested
                            .load(Ordering::Acquire)
                    })
                    .map_err(|_| ())?
                else {
                    return Ok(());
                };
                Arc::from(compressed_bytes)
            }
        };
        let mut bounded_output = BoundedOutput::from_output_byte_limit(remaining_output_byte_count);
        write_kitty_transmit(
            &mut bounded_output,
            kitty_upload_request.image.pixel_width,
            kitty_upload_request.image.pixel_height,
            kitty_upload_request.kitty_image_id,
            &compressed_bytes,
        )
        .map_err(|_| ())?;
        let upload_bytes = bounded_output.into_output_bytes();
        remaining_output_byte_count -= upload_bytes.len();
        send_worker_message(
            worker_message_sender,
            &worker_request.is_cancellation_requested,
            WorkerMessage::KittyUploaded {
                frame_generation: worker_request.frame_generation,
                finished_kitty_upload: FinishedKittyUpload {
                    image_content_id: kitty_upload_request.image_content_id,
                    kitty_image_id: kitty_upload_request.kitty_image_id,
                    upload_bytes: upload_bytes.into(),
                },
                compressed_bytes,
            },
        )?;
    }
    Ok(())
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
            .ok_or_else(|| {
                build_invalid_output_error("native image frame exceeds its output limit")
            })?;
        self.output_bytes
            .try_reserve(next_output_byte_count - self.output_bytes.len())
            .map_err(|_| {
                build_invalid_output_error("native image output storage could not be allocated")
            })?;
        self.output_bytes.extend_from_slice(chunk_bytes);
        Ok(chunk_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn build_kitty_placement(
    kitty_image_id: u32,
    kitty_placement_id: u32,
    output_paint: &OutputPaint,
) -> KittyPlacement {
    KittyPlacement {
        image_id: kitty_image_id,
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

fn send_worker_message(
    worker_message_sender: &Sender<WorkerMessage>,
    is_cancellation_requested: &AtomicBool,
    worker_message: WorkerMessage,
) -> Result<(), ()> {
    if is_cancellation_requested.load(Ordering::Acquire) {
        return Err(());
    }
    worker_message_sender.send(worker_message).map_err(|_| ())
}

#[derive(Debug)]
enum TemplateError {
    Unavailable,
    Failed,
}

/// One unit of encoded iTerm2 or Sixel output, at its offset in cells from
/// the target area's top-left cell.
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
            maximum_pixel_width,
            maximum_pixel_height,
        } => encode_sixel_template(
            worker_request,
            image_plan,
            lower_plans,
            palette_color_count,
            maximum_pixel_width,
            maximum_pixel_height,
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
    let decoded_image = if image_plan
        .outcome
        .iterm_composition
        .needs_per_cell_composition
    {
        compose_iterm_image(worker_request, image_plan, lower_plans)?
    } else {
        crop_output_image(
            image_plan.output_paint,
            image_plan.outcome.iterm_composition.background_color,
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
                find_iterm_cell_background(worker_request, global_pixel_x, global_pixel_y)
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

/// Return the pixel that the cell under the pixel at (`pixel_x`, `pixel_y`)
/// shows below an iTerm2 image, in cells of the measured size: `[0, 0, 0, 0]`
/// for a default cell, and the opaque background color of a plain cell.
/// `None` with no measured cell size, with no cell snapshot, for a cell
/// outside the cell snapshot, and for every other cell.
fn find_iterm_cell_background(
    worker_request: &WorkerRequest,
    pixel_x: u32,
    pixel_y: u32,
) -> Option<[u8; 4]> {
    let cell = find_cell_at_pixel(
        worker_request,
        worker_request.measured_pixel_cell_size?,
        pixel_x,
        pixel_y,
    )?;
    if cell == &ImageCellState::default() {
        return Some([0, 0, 0, 0]);
    }
    let [red, green, blue] = find_plain_cell_background_color(cell)?;
    Some([red, green, blue, 255])
}

/// Return the cell of `worker_request`'s cell snapshot that holds the pixel at
/// (`pixel_x`, `pixel_y`), in cells of `pixel_cell_size`. `None` with no cell
/// snapshot, and for a pixel outside it.
fn find_cell_at_pixel(
    worker_request: &WorkerRequest,
    pixel_cell_size: PixelCellSize,
    pixel_x: u32,
    pixel_y: u32,
) -> Option<&ImageCellState> {
    let cell_column = u16::try_from(pixel_x / u32::from(pixel_cell_size.get_pixel_width())).ok()?;
    let cell_row = u16::try_from(pixel_y / u32::from(pixel_cell_size.get_pixel_height())).ok()?;
    worker_request
        .cell_snapshot
        .as_deref()?
        .find_cell(cell_column, cell_row)
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
    maximum_pixel_width: Option<u32>,
    maximum_pixel_height: Option<u32>,
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
    let maximum_column_count = maximum_pixel_width
        .map(|pixel_width| pixel_width / cell_pixel_width)
        .unwrap_or(u32::from(image_plan.output_paint.target_area.width));
    let maximum_row_count = maximum_pixel_height
        .map(|pixel_height| pixel_height / cell_pixel_height)
        .unwrap_or(u32::from(image_plan.output_paint.target_area.height));
    let maximum_column_count =
        maximum_column_count.min(u32::from(image_plan.output_paint.target_area.width));
    let maximum_row_count =
        maximum_row_count.min(u32::from(image_plan.output_paint.target_area.height));
    if maximum_column_count == 0 || maximum_row_count == 0 {
        return Err(TemplateError::Failed);
    }
    let mut template_units = Vec::new();
    let tile_column_count =
        u16::try_from(maximum_column_count).map_err(|_| TemplateError::Failed)?;
    let tile_row_count = u16::try_from(maximum_row_count).map_err(|_| TemplateError::Failed)?;
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
                encode_options,
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
    encode_options: SixelEncodeOptions,
    palette: &PreparedSixelPalette,
    cell_pixel_width: u32,
    cell_pixel_height: u32,
    tile: TileRect,
    template_units: &mut Vec<TemplateUnit>,
    template_output_byte_count: &mut usize,
    output_byte_limit: usize,
) -> Result<(), TemplateError> {
    if worker_request
        .is_cancellation_requested
        .load(Ordering::Acquire)
    {
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
    )
    .map_err(|_| TemplateError::Failed)?;
    let should_compose = image_plan
        .outcome
        .sixel_composition
        .background_color
        .is_some()
        || image_plan.outcome.sixel_composition.has_alpha
        || image_plan.outcome.sixel_composition.has_terminal_background;
    let tile_image = if should_compose {
        compose_sixel_tile(worker_request, image_plan, lower_plans, tile, tile_image)?
    } else {
        tile_image
    };
    let mut encoder = if should_compose {
        SixelEncoder::with_options(tile_image, [0, 0, 0], encode_options)
    } else {
        SixelEncoder::with_palette(tile_image, [0, 0, 0], palette.clone())
    }
    .map_err(|_| TemplateError::Failed)?;
    let mut tile_output_bytes = Vec::new();
    while let Some(chunk_bytes) = encoder
        .take_next_chunk(MAX_SIXEL_CHUNK_BYTE_COUNT)
        .map_err(|_| TemplateError::Failed)?
    {
        if worker_request
            .is_cancellation_requested
            .load(Ordering::Acquire)
        {
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
    let mut output_rgba_bytes = tile_image.rgba_bytes.clone();
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
            let terminal_background_color =
                get_terminal_background(worker_request, global_pixel_x, global_pixel_y);
            let mut underlying_pixel = [
                terminal_background_color[0],
                terminal_background_color[1],
                terminal_background_color[2],
                255,
            ];
            for lower_plan in lower_plans {
                if lower_plan.outcome.image_compatibility != ImageCompatibility::default() {
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
                    )
                    .unwrap_or(underlying_pixel);
                }
            }
            let rgba_byte_index = (usize::try_from(tile_pixel_row)
                .map_err(|_| TemplateError::Failed)?
                * tile_pixel_width
                + usize::try_from(tile_pixel_column).map_err(|_| TemplateError::Failed)?)
                * 4;
            let image_pixel: [u8; 4] = output_rgba_bytes[rgba_byte_index..rgba_byte_index + 4]
                .try_into()
                .map_err(|_| TemplateError::Failed)?;
            let composited_pixel = apply_sixel_layer(
                image_plan,
                image_pixel,
                terminal_background_color,
                underlying_pixel,
            )
            .unwrap_or([0, 0, 0, 0]);
            output_rgba_bytes[rgba_byte_index..rgba_byte_index + 4]
                .copy_from_slice(&composited_pixel);
        }
    }
    Ok(Arc::new(DecodedImage {
        pixel_width: tile_image.pixel_width,
        pixel_height: tile_image.pixel_height,
        rgba_bytes: output_rgba_bytes,
    }))
}

/// Return the RGB background color of the cell under the pixel at
/// (`pixel_x`, `pixel_y`), in cells of the request's cell size. Black
/// (`[0, 0, 0]`) with no cell snapshot, for a cell outside it, and for a
/// background that is not an RGB color.
fn get_terminal_background(worker_request: &WorkerRequest, pixel_x: u32, pixel_y: u32) -> [u8; 3] {
    let cell = find_cell_at_pixel(
        worker_request,
        worker_request.pixel_cell_size,
        pixel_x,
        pixel_y,
    );
    match cell.map(|cell| cell.style.get_background_color()) {
        Some(koshi_terminal::style::Color::Rgb(red, green, blue)) => [red, green, blue],
        _ => [0, 0, 0],
    }
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

/// The pixel that `image_pixel` of `image_plan`'s Sixel image draws over
/// `underlying_pixel`: blended onto the image's solid background color when
/// its composition has one, `terminal_background_color` for a zero-alpha
/// pixel of an image that shows the terminal background, `None` for any
/// other zero-alpha pixel, and otherwise blended onto `underlying_pixel`.
fn apply_sixel_layer(
    image_plan: &Plan<'_>,
    image_pixel: [u8; 4],
    terminal_background_color: [u8; 3],
    underlying_pixel: [u8; 4],
) -> Option<[u8; 4]> {
    if let Some(background_color) = image_plan.outcome.sixel_composition.background_color {
        return Some(blend_pixel(image_pixel, background_color, 255));
    }
    if image_pixel[3] == 0 {
        return has_terminal_background(image_plan.output_paint).then_some([
            terminal_background_color[0],
            terminal_background_color[1],
            terminal_background_color[2],
            255,
        ]);
    }
    Some(blend_pixel(
        image_pixel,
        [
            underlying_pixel[0],
            underlying_pixel[1],
            underlying_pixel[2],
        ],
        underlying_pixel[3],
    ))
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
    if !is_source_rect_in_image(decoded_image, output_paint.source_rect) {
        return Err(());
    }
    let image_pixel_width = usize::try_from(decoded_image.pixel_width).map_err(|_| ())?;
    let crop_pixel_width = usize::try_from(output_paint.source_rect.pixel_width).map_err(|_| ())?;
    let crop_pixel_height =
        usize::try_from(output_paint.source_rect.pixel_height).map_err(|_| ())?;
    let crop_rgba_byte_count = compute_rgba_byte_count(
        GraphicsProtocol::Iterm2,
        crop_pixel_width,
        crop_pixel_height,
    )
    .map_err(|_| ())?;
    let mut cropped_rgba_bytes = Vec::new();
    cropped_rgba_bytes
        .try_reserve_exact(crop_rgba_byte_count)
        .map_err(|_| ())?;
    cropped_rgba_bytes.resize(crop_rgba_byte_count, 0);
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
            let destination_pixel_bytes = &mut cropped_rgba_bytes[destination_rgba_byte_index
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
        rgba_bytes: cropped_rgba_bytes,
    }))
}

fn scale_output_tile(
    source_image: &DecodedImage,
    source_rect: ImageSourceRect,
    target_area: Rect,
    tile: TileRect,
    cell_pixel_width: u32,
    cell_pixel_height: u32,
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
    let rgba_byte_count = compute_rgba_byte_count(
        GraphicsProtocol::Sixel,
        usize::try_from(tile_pixel_width).map_err(|_| ())?,
        usize::try_from(tile_pixel_height).map_err(|_| ())?,
    )
    .map_err(|_| ())?;
    let mut scaled_rgba_bytes = Vec::new();
    scaled_rgba_bytes
        .try_reserve_exact(rgba_byte_count)
        .map_err(|_| ())?;
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
            scaled_rgba_bytes.extend_from_slice(
                &source_image.rgba_bytes[source_rgba_byte_index..source_rgba_byte_index + 4],
            );
        }
    }
    Ok(Arc::new(DecodedImage {
        pixel_width: tile_pixel_width,
        pixel_height: tile_pixel_height,
        rgba_bytes: scaled_rgba_bytes,
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

/// Write the CUP sequence that moves the cursor to `cursor_position`, or
/// `ESC [ ? 25 l` that hides it when `cursor_position` is `None`.
fn restore_cursor_state<W: Write>(
    writer: &mut W,
    cursor_position: Option<Position>,
) -> io::Result<()> {
    match cursor_position {
        Some(cursor_position) => {
            write_cursor_position(writer, cursor_position.x, cursor_position.y)
        }
        None => writer.write_all(b"\x1b[?25l"),
    }
}

fn build_invalid_output_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
