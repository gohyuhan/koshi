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
    checked_rgba_len, validate_dimensions, DecodedImage, GraphicsProtocol, MAX_IMAGE_BYTES,
};
use koshi_iterm::{Encoder as ItermEncoder, OutputOptions, MAX_ITERM_PACKET_BYTES};
use koshi_kitty::{
    write_kitty_delete_all, write_kitty_image_delete, write_kitty_placement,
    write_kitty_visible_placement_delete, KittyOutputError, KittyPlacement, KittyUpload,
};
use koshi_renderer::{
    ImageCellSnapshot, ImageCellState, ImagePaint, ImagePlacementKey, ImageSourceRect,
    MAX_IMAGE_CELL_SNAPSHOT_CELLS,
};
use koshi_sixel::{
    PreparedSixelPalette, SixelEncodeOptions, SixelEncoder, MAX_PALETTE_COLORS,
    MAX_SIXEL_OUTPUT_BYTES, MAX_SIXEL_TILE_BYTES, MIN_PALETTE_COLORS,
};
use koshi_terminal::graphics::{ImageRecord, SixelBackground};

use super::{restore_cursor_state, GraphicsSupport};

const MAX_OUTPUT_PAINTS: usize = 4_096;
const MAX_NATIVE_FRAME_OUTPUT_BYTES: usize =
    MAX_IMAGE_BYTES * 2 + MAX_OUTPUT_PAINTS * (std::mem::size_of::<OutputPaint>() + 256);

const SIXEL_MODE_SAVE: &[u8] = b"\x1b[?80s\x1b[?8452s\x1b[?1070s";
const SIXEL_MODE_RESET: &[u8] = b"\x1b[?80l\x1b[?8452l\x1b[?1070h";
const SIXEL_MODE_RESTORE: &[u8] = b"\x1b[?80r\x1b[?8452r\x1b[?1070r";
const IMAGE_ABORT: &[u8] = b"\x18\x1b\\";
const SCREEN_RESET: &[u8] = b"\x1b[2J";
const KITTY_BACKGROUND_LAYER_Z: i32 = i32::MIN / 2;

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
        palette_colors: usize,
        /// Maximum Sixel width in pixels.
        max_width: Option<u32>,
        /// Maximum Sixel height in pixels.
        max_height: Option<u32>,
    },
}

/// Describes a native image composition that is not exact on the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ImageCompatibility {
    /// The host cannot express the image layer order relative to text glyphs.
    pub(crate) text_layer_order: bool,
    /// iTerm2 alpha cannot preserve the required underlying pixels.
    pub(crate) iterm_alpha: bool,
    /// Sixel partial alpha uses an unknown underlying color.
    pub(crate) sixel_alpha: bool,
    /// Sixel terminal-background pixels use an unknown cell color.
    pub(crate) sixel_terminal_background: bool,
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
                palette_colors,
                max_width,
                max_height,
            } => Some(Self::Sixel {
                palette_colors,
                max_width,
                max_height,
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
    pub(crate) const fn composes_with_cells(self) -> bool {
        !matches!(self, Self::Kitty)
    }
}

/// Return the Sixel mode-save sequence used at application-mode setup.
pub(crate) const fn sixel_mode_save() -> &'static [u8] {
    SIXEL_MODE_SAVE
}

/// Return the Sixel mode-restore sequence used at application-mode cleanup.
pub(crate) const fn sixel_mode_restore() -> &'static [u8] {
    SIXEL_MODE_RESTORE
}

/// Write the cancellation sequence for an open image string.
pub(crate) fn write_image_abort<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(IMAGE_ABORT)
}

/// One image's current connection-local output state.
#[derive(Debug, Clone)]
struct OutputPaint {
    key: ImagePlacementKey,
    content_id: u64,
    record: Arc<ImageRecord>,
    target: Rect,
    source: ImageSourceRect,
    cell_offset_x: Option<u32>,
    cell_offset_y: Option<u32>,
    z_index: i32,
    alpha: Option<AlphaStats>,
}

impl OutputPaint {
    fn from_paint(paint: &ImagePaint, alpha: Option<AlphaStats>) -> Self {
        Self {
            key: (paint.pane_id, paint.placement_id),
            content_id: paint.content_id,
            record: Arc::clone(&paint.record),
            target: paint.target,
            source: paint.source,
            cell_offset_x: paint.cell_offset_x,
            cell_offset_y: paint.cell_offset_y,
            z_index: paint.z_index,
            alpha,
        }
    }

    /// Return whether this paint's encoded pixels read the cells it covers.
    ///
    /// A fully opaque image at a z-index at or above zero replaces every cell
    /// it covers, so its pixels and its host compatibility read no cell. An
    /// unknown alpha coverage reads the cells.
    fn depends_on_target_cells(&self, kind: ImageOutputKind) -> bool {
        if !kind.composes_with_cells() {
            return false;
        }
        !(self.z_index >= 0 && self.alpha.is_some_and(AlphaStats::is_fully_opaque))
    }
}

/// Values that identify exact image output inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EncodeKey {
    content_id: u64,
    image_address: usize,
    source: ImageSourceKey,
    target_width: u16,
    target_height: u16,
    cell_size: PixelCellSize,
    kind: ImageOutputKind,
    z_index: i32,
    uses_terminal_background: bool,
    composition: u64,
    composition_cell_size: Option<PixelCellSize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageSourceKey {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
struct CompositionFrame {
    cells: Arc<ImageCellSnapshot>,
    paints: Arc<[CompositionPaint]>,
}

#[derive(Debug, Clone)]
struct CompositionState {
    frame: Arc<CompositionFrame>,
    paint_index: usize,
}

#[derive(Debug)]
struct VersionedComposition {
    state: CompositionState,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompositionPaint {
    image_address: usize,
    content_id: u64,
    record: Arc<ImageRecord>,
    key: ImagePlacementKey,
    target: Rect,
    source: ImageSourceKey,
    z_index: i32,
}

impl CompositionFrame {
    fn new(cells: &Arc<ImageCellSnapshot>, paints: &[OutputPaint]) -> Self {
        Self {
            cells: Arc::clone(cells),
            paints: paints
                .iter()
                .map(|paint| CompositionPaint {
                    image_address: Arc::as_ptr(&paint.record.image) as usize,
                    content_id: paint.content_id,
                    record: Arc::clone(&paint.record),
                    key: paint.key,
                    target: paint.target,
                    source: ImageSourceKey::from_source(paint.source),
                    z_index: paint.z_index,
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

impl PartialEq for CompositionState {
    fn eq(&self, other: &Self) -> bool {
        let Some(paint) = self.frame.paints.get(self.paint_index) else {
            return false;
        };
        let Some(other_paint) = other.frame.paints.get(other.paint_index) else {
            return false;
        };
        paint == other_paint
            && (paint.target.y..paint.target.bottom()).all(|y| {
                (paint.target.x..paint.target.right())
                    .all(|x| self.frame.cells.cell(x, y) == other.frame.cells.cell(x, y))
            })
            && self.frame.paints[..self.paint_index]
                .iter()
                .filter(|lower| rectangles_overlap(lower.target, paint.target))
                .eq(other.frame.paints[..other.paint_index]
                    .iter()
                    .filter(|lower| rectangles_overlap(lower.target, other_paint.target)))
    }
}

impl Eq for CompositionState {}

impl ImageSourceKey {
    fn from_source(source: ImageSourceRect) -> Self {
        Self {
            x: source.x,
            y: source.y,
            width: source.width,
            height: source.height,
        }
    }
}

/// One Kitty image number the host already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyImage {
    /// The nonzero Kitty image number carried by `I=`.
    number: u32,
    /// The address of the `DecodedImage` transmitted under `number`.
    address: usize,
}

/// The Kitty image number one paint places, and whether that paint carries the
/// pixels. `transmit` is false when the host already holds `number`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KittyPaintImage {
    /// The nonzero Kitty image number this paint places.
    number: u32,
    /// Whether this paint writes the image's pixels before it places them.
    transmit: bool,
}

/// A worker request containing one bounded frame's images and one shared cell snapshot.
struct WorkerRequest {
    generation: u64,
    kind: ImageOutputKind,
    cell_size: PixelCellSize,
    measured_cell_size: Option<PixelCellSize>,
    cells: Option<Arc<ImageCellSnapshot>>,
    paints: Vec<OutputPaint>,
    keys: Vec<EncodeKey>,
    /// One entry per paint, in the same order. Empty for every other protocol.
    kitty_images: Vec<KittyPaintImage>,
    cancel: Arc<AtomicBool>,
}

/// A worker output unit with relative cell geometry.
#[derive(Debug)]
struct OutputUnit {
    generation: u64,
    key: ImagePlacementKey,
    kind: ImageOutputKind,
    offset: (u16, u16),
    bytes: Arc<[u8]>,
}

/// Messages sent by the bounded image worker.
#[derive(Debug)]
enum WorkerMessage {
    Prepared {
        generation: u64,
        key: ImagePlacementKey,
        compatibility: ImageCompatibility,
    },
    Unavailable {
        generation: u64,
        key: ImagePlacementKey,
    },
    Unit(OutputUnit),
    Finished {
        generation: u64,
        failed: bool,
    },
}

/// One active request and its cancellation token.
struct ActiveJob {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

/// Bounded image output state owned by one terminal connection.
pub(crate) struct ImageOutputState {
    kind: Option<ImageOutputKind>,
    requests: Option<SyncSender<WorkerRequest>>,
    messages: Option<Receiver<WorkerMessage>>,
    worker: Option<JoinHandle<()>>,
    generation: u64,
    active: Option<ActiveJob>,
    pending: Option<WorkerRequest>,
    latest: Vec<OutputPaint>,
    latest_index: HashMap<ImagePlacementKey, usize>,
    latest_keys: Vec<EncodeKey>,
    /// The composition revision of the cells under each paint in `latest`.
    latest_coverage: Vec<u64>,
    composition_states: HashMap<ImagePlacementKey, VersionedComposition>,
    composition_revision: u64,
    settled: bool,
    ready: bool,
    prepared: Vec<ImagePlacementKey>,
    prepared_set: HashSet<ImagePlacementKey>,
    compatibility: HashMap<ImagePlacementKey, ImageCompatibility>,
    units: Vec<OutputUnit>,
    unit_bytes: usize,
    host_pixels_present: bool,
    screen_reset_needed: bool,
    needs_abort: bool,
    /// Host screen size in columns and rows at the last painted frame.
    host_size: Option<(u16, u16)>,
    /// Alpha coverage per decoded-image address and source rectangle. Holds
    /// only the addresses `latest` retains, so no address is reused under a
    /// stale entry.
    alpha_cache: HashMap<(usize, ImageSourceKey), Option<AlphaStats>>,
    /// Kitty image numbers the host holds, keyed by content identity.
    kitty_images: HashMap<u64, KittyImage>,
    /// Kitty image numbers this frame transmits. They join `kitty_images` when
    /// the frame commits, and are dropped when it does not.
    pending_kitty_images: Vec<(u64, KittyImage)>,
    /// Kitty image numbers the next frame reset frees on the host.
    kitty_image_deletes: Vec<u32>,
    /// Whether the next frame reset frees every Kitty image the host holds.
    kitty_free_all: bool,
    /// The next Kitty image number handed out. Never zero.
    next_kitty_image_number: u32,
}

impl ImageOutputState {
    fn rebuild_latest_index(&mut self) {
        self.latest_index.clear();
        self.latest_index.extend(
            self.latest
                .iter()
                .enumerate()
                .map(|(index, paint)| (paint.key, index)),
        );
    }

    /// Build a connection-local worker for the selected output protocol.
    pub(crate) fn new(kind: Option<ImageOutputKind>) -> Self {
        let (requests, messages, worker) = if kind.is_some() {
            let (request_tx, request_rx) = mpsc::sync_channel(1);
            let (message_tx, message_rx) = mpsc::sync_channel(1);
            match thread::Builder::new()
                .name(String::from("koshi-image-output"))
                .spawn(move || worker_loop(request_rx, message_tx))
            {
                Ok(worker) => (Some(request_tx), Some(message_rx), Some(worker)),
                Err(error) => {
                    tracing::warn!(%error, "could not start the image output worker");
                    (None, None, None)
                }
            }
        } else {
            (None, None, None)
        };
        Self {
            kind,
            requests,
            messages,
            worker,
            generation: 0,
            active: None,
            pending: None,
            latest: Vec::new(),
            latest_index: HashMap::new(),
            latest_keys: Vec::new(),
            latest_coverage: Vec::new(),
            composition_states: HashMap::new(),
            composition_revision: 0,
            settled: false,
            ready: true,
            prepared: Vec::new(),
            prepared_set: HashSet::new(),
            compatibility: HashMap::new(),
            units: Vec::new(),
            unit_bytes: 0,
            host_pixels_present: false,
            screen_reset_needed: false,
            host_size: None,
            needs_abort: false,
            alpha_cache: HashMap::new(),
            kitty_images: HashMap::new(),
            pending_kitty_images: Vec::new(),
            kitty_image_deletes: Vec::new(),
            kitty_free_all: false,
            next_kitty_image_number: 1,
        }
    }

    /// Return an inactive state for tests and terminals without image output.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::new(None)
    }

    /// Return the connection's output protocol, if it has one.
    pub(crate) const fn kind(&self) -> Option<ImageOutputKind> {
        self.kind
    }

    /// Return prepared placement keys for renderer selection.
    pub(crate) fn prepared_keys(&self) -> &[ImagePlacementKey] {
        &self.prepared
    }

    /// Return the host composition status for one prepared placement.
    pub(crate) fn compatibility(&self, key: ImagePlacementKey) -> Option<ImageCompatibility> {
        self.compatibility.get(&key).copied()
    }

    /// Return whether this state needs another attachment-loop pass.
    pub(crate) fn work_pending(&self) -> bool {
        !self.ready || self.active.is_some() || self.pending.is_some()
    }

    /// Return whether the newest frame can re-emit the encoded output the
    /// committed frame already holds.
    ///
    /// [`frame_output`](Self::frame_output) reads each unit's screen position
    /// from `latest` when it writes the frame, so output whose encode keys and
    /// placement identities are unchanged stays correct at a new position.
    /// Kitty writes each placement's position into its own unit, so its output
    /// is never reused.
    fn can_reuse_output(&self, latest: &[OutputPaint], keys: &[EncodeKey]) -> bool {
        matches!(
            self.kind,
            Some(ImageOutputKind::Iterm | ImageOutputKind::Sixel { .. })
        ) && self.ready
            && !self.units.is_empty()
            && self.latest_keys == keys
            && self.latest.len() == latest.len()
            && self
                .latest
                .iter()
                .zip(latest)
                .all(|(held, next)| held.key == next.key)
    }

    /// Assign one Kitty image number per paint in `self.latest`.
    ///
    /// A content identity the host already holds keeps its number and
    /// transmits no pixels. Every number whose content identity left the frame
    /// joins `kitty_image_deletes`. Returns `None` when the numbers cannot be
    /// reserved.
    fn plan_kitty_images(&mut self) -> Option<Vec<KittyPaintImage>> {
        if self.next_kitty_image_number == u32::MAX {
            self.forget_kitty_images();
        }
        let mut plan = Vec::new();
        plan.try_reserve_exact(self.latest.len()).ok()?;
        self.pending_kitty_images.clear();
        let mut frame_numbers = HashMap::new();
        frame_numbers.try_reserve(self.latest.len()).ok()?;
        for index in 0..self.latest.len() {
            let paint = &self.latest[index];
            let content_id = paint.content_id;
            let address = Arc::as_ptr(&paint.record.image) as usize;
            if let Some(&number) = frame_numbers.get(&(content_id, address)) {
                plan.push(KittyPaintImage {
                    number,
                    transmit: false,
                });
                continue;
            }
            let held = self
                .kitty_images
                .get(&content_id)
                .filter(|image| image.address == address)
                .copied();
            let (number, transmit) = match held {
                Some(image) => (image.number, false),
                None => {
                    let number = self.next_kitty_image_number;
                    self.next_kitty_image_number = number.checked_add(1)?;
                    self.pending_kitty_images
                        .push((content_id, KittyImage { number, address }));
                    (number, true)
                }
            };
            frame_numbers.insert((content_id, address), number);
            plan.push(KittyPaintImage { number, transmit });
        }
        let departed = self
            .kitty_images
            .iter()
            .filter(|(content_id, image)| {
                !frame_numbers.contains_key(&(**content_id, image.address))
            })
            .map(|(content_id, image)| (*content_id, image.number))
            .collect::<Vec<_>>();
        for (content_id, number) in departed {
            self.kitty_images.remove(&content_id);
            self.kitty_image_deletes.push(number);
        }
        Some(plan)
    }

    /// Drop every Kitty image number and free the host's data at the next
    /// reset. On every other protocol this frees nothing.
    fn forget_kitty_images(&mut self) {
        self.kitty_images.clear();
        self.pending_kitty_images.clear();
        self.kitty_image_deletes.clear();
        self.kitty_free_all = matches!(self.kind, Some(ImageOutputKind::Kitty));
        self.next_kitty_image_number = 1;
    }

    /// Record the host screen size for the frame being painted.
    ///
    /// A changed size forgets every Kitty image number. The next frame
    /// transmits its images again.
    pub(crate) fn note_host_size(&mut self, columns: u16, rows: u16) {
        let size = Some((columns, rows));
        if self.host_size == size {
            return;
        }
        let resized = self.host_size.is_some();
        self.host_size = size;
        if resized && matches!(self.kind, Some(ImageOutputKind::Kitty)) {
            self.forget_kitty_images();
            self.latest_keys.clear();
            self.latest_coverage.clear();
        }
    }

    /// Submit the newest frame and return whether it can be committed.
    pub(crate) fn prepare_frame(
        &mut self,
        paints: &[ImagePaint],
        cells: Option<Arc<ImageCellSnapshot>>,
        cell_size: Option<PixelCellSize>,
    ) -> bool {
        self.poll();
        let Some(kind) = self.kind else {
            return true;
        };
        let measured_cell_size = cell_size;
        let cell_size = if kind.is_sixel() {
            let Some(cell_size) = cell_size else {
                self.set_unavailable_frame(paints);
                return true;
            };
            cell_size
        } else {
            PixelCellSize::new(1, 1).expect("one-pixel cell is nonzero")
        };
        if paints.len() > MAX_OUTPUT_PAINTS {
            self.set_unavailable_frame(paints);
            return true;
        }
        if !paints.is_empty()
            && kind.composes_with_cells()
            && cells.as_ref().is_none_or(|cells| {
                usize::try_from(cells.area.area()).ok() > Some(MAX_IMAGE_CELL_SNAPSHOT_CELLS)
            })
        {
            self.set_unavailable_frame(paints);
            return true;
        }
        let mut latest = Vec::new();
        let mut alpha_cache = HashMap::new();
        if latest.try_reserve_exact(paints.len()).is_err()
            || alpha_cache.try_reserve(paints.len()).is_err()
        {
            self.set_unavailable_frame(paints);
            return true;
        }
        for paint in paints {
            let key = (
                Arc::as_ptr(&paint.record.image) as usize,
                ImageSourceKey::from_source(paint.source),
            );
            let alpha = match self.alpha_cache.get(&key).copied() {
                Some(alpha) => alpha,
                None if kind.composes_with_cells() => {
                    alpha_stats(&paint.record.image, paint.source)
                }
                None => None,
            };
            alpha_cache.insert(key, alpha);
            latest.push(OutputPaint::from_paint(paint, alpha));
        }
        self.alpha_cache = alpha_cache;
        let coverage = if let Some(cells) = cells.as_ref() {
            self.update_composition_revisions(kind, cells, &latest)
        } else {
            vec![0; latest.len()]
        };
        let mut covered_cells = HashSet::new();
        let keys = latest
            .iter()
            .zip(coverage.iter().copied())
            .map(|(paint, composition_revision)| {
                let overlaps_lower = target_contains_covered_cell(paint.target, &covered_cells);
                let mut key = output_encode_key(kind, cell_size, paint);
                if paint.depends_on_target_cells(kind) {
                    key.composition = composition_revision;
                    if matches!(kind, ImageOutputKind::Iterm)
                        && paint.alpha.is_some_and(|alpha| !alpha.is_fully_opaque())
                        && (overlaps_lower
                            || cells.as_ref().is_some_and(|cells| {
                                let composition = composition_info(cells, paint);
                                !composition.default_blank && composition.solid_background.is_none()
                            }))
                    {
                        key.composition_cell_size = measured_cell_size;
                    }
                }
                add_target_cells(paint.target, &mut covered_cells);
                key
            })
            .collect::<Vec<_>>();
        let same_paints = output_frames_equal(&self.latest, &latest);
        if same_paints && self.latest_keys == keys && self.latest_coverage == coverage {
            return self.ready;
        }
        if self.can_reuse_output(&latest, &keys) {
            self.latest = latest;
            self.latest_coverage = coverage;
            self.rebuild_latest_index();
            self.settled = false;
            self.screen_reset_needed = !same_paints && self.host_pixels_present;
            return true;
        }
        self.latest = latest;
        self.latest_keys = keys.clone();
        self.latest_coverage = coverage;
        self.rebuild_latest_index();
        let kitty_images = if matches!(kind, ImageOutputKind::Kitty) {
            match self.plan_kitty_images() {
                Some(images) => images,
                None => {
                    self.set_unavailable_frame(paints);
                    return true;
                }
            }
        } else {
            Vec::new()
        };
        self.settled = false;
        self.ready = self.latest.is_empty();
        self.screen_reset_needed = self.host_pixels_present;
        self.clear_prepared_output();
        self.cancel_active();
        if let Some(previous) = self.pending.take() {
            previous.cancel.store(true, Ordering::Release);
        }
        if self.ready {
            self.next_generation();
            return true;
        }
        let request_cancel = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest {
            generation: self.next_generation(),
            kind,
            cell_size,
            measured_cell_size,
            cells,
            paints: self.latest.clone(),
            keys: keys.clone(),
            kitty_images,
            cancel: Arc::clone(&request_cancel),
        };
        if self.active.is_none() {
            self.start_request(request);
        } else {
            self.pending = Some(request);
        }
        self.ready
    }

    /// Receive worker results without waiting on the output queue.
    pub(crate) fn poll(&mut self) {
        loop {
            let message = match self.messages.as_ref().map(Receiver::try_recv) {
                Some(Ok(message)) => message,
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.active = None;
                    self.pending = None;
                    self.fail_current_generation();
                    break;
                }
            };
            match message {
                WorkerMessage::Prepared {
                    generation,
                    key,
                    compatibility,
                } => {
                    if self.current_generation(generation) && self.latest_index.contains_key(&key) {
                        if self.prepared.len() < MAX_OUTPUT_PAINTS && self.prepared_set.insert(key)
                        {
                            self.prepared.push(key);
                        }
                        self.compatibility.insert(key, compatibility);
                    }
                }
                WorkerMessage::Unavailable { generation, key } => {
                    if self.current_generation(generation) {
                        self.compatibility.remove(&key);
                    }
                }
                WorkerMessage::Unit(unit) => {
                    if self.current_generation(unit.generation)
                        && self.prepared_set.contains(&unit.key)
                        && self.latest_index.contains_key(&unit.key)
                    {
                        let Some(next_bytes) = self.unit_bytes.checked_add(unit.bytes.len()) else {
                            self.fail_current_generation();
                            continue;
                        };
                        if next_bytes > MAX_NATIVE_FRAME_OUTPUT_BYTES
                            || self.units.try_reserve(1).is_err()
                        {
                            self.fail_current_generation();
                            continue;
                        }
                        self.unit_bytes = next_bytes;
                        self.units.push(unit);
                    }
                }
                WorkerMessage::Finished { generation, failed } => {
                    if self.current_generation(generation) {
                        if failed {
                            self.fail_current_generation();
                        } else {
                            self.ready = true;
                        }
                    }
                    if self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.generation == generation)
                    {
                        self.active = None;
                        self.start_pending();
                    }
                }
            }
        }
    }

    /// Return whether the newest frame changes native terminal image state.
    pub(crate) const fn native_commit_pending(&self) -> bool {
        !self.settled
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
        let kitty_pending = self.kitty_free_all || !self.kitty_image_deletes.is_empty();
        if !self.screen_reset_needed && !self.needs_abort && !kitty_pending {
            return Ok(false);
        }
        if self.needs_abort {
            write_image_abort(writer)?;
            self.needs_abort = false;
        }
        let kitty = matches!(self.kind, Some(ImageOutputKind::Kitty));
        if kitty {
            if self.kitty_free_all {
                write_kitty_delete_all(writer).map_err(kitty_output_error)?;
                self.kitty_free_all = false;
                self.kitty_image_deletes.clear();
            } else {
                if self.screen_reset_needed {
                    write_kitty_visible_placement_delete(writer).map_err(kitty_output_error)?;
                }
                for number in self.kitty_image_deletes.drain(..) {
                    write_kitty_image_delete(writer, number).map_err(kitty_output_error)?;
                }
            }
        }
        let clears_screen = self.screen_reset_needed && !kitty;
        if clears_screen {
            writer.write_all(SCREEN_RESET)?;
        }
        writer.flush()?;
        Ok(clears_screen)
    }

    /// Build all native bytes written after the newest base-cell frame.
    pub(crate) fn frame_output(&self, cursor: Option<Position>) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let overhead = self.units.len().saturating_mul(128).saturating_add(32);
        output
            .try_reserve(self.unit_bytes.saturating_add(overhead))
            .map_err(|_| invalid_output("image output storage could not be allocated"))?;
        for unit in &self.units {
            let Some(&index) = self.latest_index.get(&unit.key) else {
                continue;
            };
            let paint = &self.latest[index];
            let x = paint
                .target
                .x
                .checked_add(unit.offset.0)
                .ok_or_else(|| invalid_output("image tile x coordinate overflows the frame"))?;
            let y = paint
                .target
                .y
                .checked_add(unit.offset.1)
                .ok_or_else(|| invalid_output("image tile y coordinate overflows the frame"))?;
            match unit.kind {
                ImageOutputKind::Kitty => output.extend_from_slice(&unit.bytes),
                ImageOutputKind::Iterm => {
                    write_cursor_position(&mut output, x, y)?;
                    output.extend_from_slice(&unit.bytes);
                    restore_cursor_state(&mut output, cursor)?;
                }
                ImageOutputKind::Sixel { .. } => {
                    output.extend_from_slice(SIXEL_MODE_RESET);
                    write_cursor_position(&mut output, x, y)?;
                    output.extend_from_slice(&unit.bytes);
                    restore_cursor_state(&mut output, cursor)?;
                    output.extend_from_slice(SIXEL_MODE_RESTORE);
                }
            }
        }
        if matches!(self.kind, Some(ImageOutputKind::Kitty)) && !output.is_empty() {
            restore_cursor_state(&mut output, cursor)?;
        }
        if output.len() > MAX_NATIVE_FRAME_OUTPUT_BYTES {
            return Err(invalid_output(
                "native image frame exceeds its output limit",
            ));
        }
        Ok(output)
    }

    /// Adopt the newest frame after its base cells and native bytes are written.
    pub(crate) fn commit_frame(&mut self) {
        self.host_pixels_present = !self.prepared.is_empty();
        self.settled = true;
        self.ready = true;
        self.screen_reset_needed = false;
        for (content_id, image) in self.pending_kitty_images.drain(..) {
            self.kitty_images.insert(content_id, image);
        }
    }

    /// Keep the newest frame uncommitted after a native output failure.
    pub(crate) fn fail_frame_commit(&mut self) {
        self.needs_abort = self.kind.is_some();
        self.cancel_active();
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.next_generation();
        self.clear_prepared_output();
        self.latest.clear();
        self.latest_index.clear();
        self.latest_keys.clear();
        self.latest_coverage.clear();
        self.alpha_cache.clear();
        self.host_pixels_present = true;
        self.screen_reset_needed = true;
        self.settled = false;
        self.ready = true;
        self.forget_kitty_images();
    }

    /// Reset this output state when a connection is replaced.
    pub(crate) fn reset_connection(&mut self) {
        self.settled = false;
        self.ready = true;
        self.cancel_active();
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.generation = self.generation.wrapping_add(1).max(1);
        self.clear_prepared_output();
        self.composition_states.clear();
        self.composition_revision = 0;
        self.latest.clear();
        self.latest_index.clear();
        self.latest_keys.clear();
        self.latest_coverage.clear();
        self.screen_reset_needed = self.host_pixels_present;
        self.alpha_cache.clear();
        self.forget_kitty_images();
    }

    fn update_composition_revisions(
        &mut self,
        kind: ImageOutputKind,
        cells: &Arc<ImageCellSnapshot>,
        paints: &[OutputPaint],
    ) -> Vec<u64> {
        if self.composition_revision == u64::MAX {
            self.composition_states.clear();
            self.composition_revision = 0;
        }
        let frame = Arc::new(CompositionFrame::new(cells, paints));
        let mut active = HashSet::new();
        let revisions = paints
            .iter()
            .enumerate()
            .map(|(paint_index, paint)| {
                if !kind.composes_with_cells() {
                    return 0;
                }
                active.insert(paint.key);
                let state = CompositionState {
                    frame: Arc::clone(&frame),
                    paint_index,
                };
                if let Some(versioned) = self.composition_states.get(&paint.key) {
                    if versioned.state == state {
                        return versioned.revision;
                    }
                }
                self.composition_revision += 1;
                self.composition_states.insert(
                    paint.key,
                    VersionedComposition {
                        state,
                        revision: self.composition_revision,
                    },
                );
                self.composition_revision
            })
            .collect();
        self.composition_states
            .retain(|key, _| active.contains(key));
        revisions
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    fn start_request(&mut self, request: WorkerRequest) {
        let active = ActiveJob {
            generation: request.generation,
            cancel: Arc::clone(&request.cancel),
        };
        let Some(sender) = &self.requests else {
            self.fail_current_generation();
            return;
        };
        match sender.try_send(request) {
            Ok(()) => {
                self.active = Some(active);
            }
            Err(TrySendError::Full(request)) => {
                self.pending = Some(request);
            }
            Err(TrySendError::Disconnected(_)) => self.fail_current_generation(),
        }
    }

    fn start_pending(&mut self) {
        let Some(request) = self.pending.take() else {
            return;
        };
        self.start_request(request);
    }

    fn current_generation(&self, generation: u64) -> bool {
        generation == self.generation
    }

    fn clear_prepared_output(&mut self) {
        self.prepared.clear();
        self.prepared_set.clear();
        self.compatibility.clear();
        self.units.clear();
        self.unit_bytes = 0;
    }

    fn set_unavailable_frame(&mut self, paints: &[ImagePaint]) {
        self.cancel_active();
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.next_generation();
        self.latest = paints
            .iter()
            .map(|paint| OutputPaint::from_paint(paint, None))
            .collect();
        self.rebuild_latest_index();
        self.alpha_cache.clear();
        self.forget_kitty_images();
        self.latest_keys.clear();
        self.latest_coverage.clear();
        self.settled = false;
        self.ready = true;
        self.screen_reset_needed = self.host_pixels_present;
        self.clear_prepared_output();
    }

    fn fail_current_generation(&mut self) {
        self.cancel_active();
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.next_generation();
        self.clear_prepared_output();
        self.pending_kitty_images.clear();
        self.ready = true;
    }

    fn cancel_active(&mut self) {
        if let Some(active) = self.active.as_ref() {
            active.cancel.store(true, Ordering::Release);
        }
    }
}

impl Drop for ImageOutputState {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Release);
        }
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.messages.take();
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn output_frames_equal(left: &[OutputPaint], right: &[OutputPaint]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.key == right.key
                && left.content_id == right.content_id
                && Arc::ptr_eq(&left.record.image, &right.record.image)
                && left.record.display.sixel_background == right.record.display.sixel_background
                && left.target == right.target
                && left.source == right.source
                && left.cell_offset_x == right.cell_offset_x
                && left.cell_offset_y == right.cell_offset_y
                && left.z_index == right.z_index
        })
}

fn output_encode_key(
    kind: ImageOutputKind,
    cell_size: PixelCellSize,
    paint: &OutputPaint,
) -> EncodeKey {
    EncodeKey {
        content_id: paint.content_id,
        image_address: Arc::as_ptr(&paint.record.image) as usize,
        source: ImageSourceKey::from_source(paint.source),
        target_width: paint.target.width,
        target_height: paint.target.height,
        cell_size,
        kind,
        z_index: paint.z_index,
        uses_terminal_background: matches!(
            paint.record.display.sixel_background,
            Some(SixelBackground::Terminal)
        ),
        composition: 0,
        composition_cell_size: None,
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

fn alpha_stats(image: &DecodedImage, source: ImageSourceRect) -> Option<AlphaStats> {
    let width = usize::try_from(image.width).ok()?;
    let height = usize::try_from(image.height).ok()?;
    validate_dimensions(GraphicsProtocol::Iterm2, width, height).ok()?;
    let expected = checked_rgba_len(GraphicsProtocol::Iterm2, width, height).ok()?;
    if image.rgba.len() != expected
        || source.width == 0
        || source.height == 0
        || source.x.checked_add(source.width)? > image.width
        || source.y.checked_add(source.height)? > image.height
    {
        return None;
    }
    let mut stats = AlphaStats::default();
    for row in source.y..source.y + source.height {
        let row_start = usize::try_from(row).ok()?.checked_mul(width)?;
        for column in source.x..source.x + source.width {
            let index = row_start
                .checked_add(usize::try_from(column).ok()?)?
                .checked_mul(4)?;
            let alpha = *image.rgba.get(index + 3)?;
            match alpha {
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
    per_cell_backgrounds_known: bool,
    solid_background: Option<[u8; 3]>,
    default_blank: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ItermComposition {
    background: Option<[u8; 3]>,
    per_cell: bool,
    per_cell_backgrounds_known: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct SixelComposition {
    background: Option<[u8; 3]>,
    alpha: bool,
    terminal_background: bool,
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
    cell.ch != ' ' || cell.width != 1 || !cell.combining.is_empty()
}

fn composition_info(cells: &ImageCellSnapshot, paint: &OutputPaint) -> CompositionInfo {
    let mut has_glyph = false;
    let mut has_non_default_background = false;
    let mut per_cell_backgrounds_known = true;
    let mut default_blank = true;
    let mut background = BackgroundAccumulator::Initial;
    for row in 0..paint.target.height {
        for column in 0..paint.target.width {
            let x = paint.target.x.saturating_add(column);
            let y = paint.target.y.saturating_add(row);
            let Some(cell) = cells.cell(x, y) else {
                has_glyph = true;
                has_non_default_background = true;
                per_cell_backgrounds_known = false;
                default_blank = false;
                background = BackgroundAccumulator::Incompatible;
                continue;
            };
            has_non_default_background |= cell.style.bg() != koshi_terminal::style::Color::Default;
            default_blank &= cell == &ImageCellState::default();
            let glyph = cell_has_glyph(Some(cell));
            has_glyph |= glyph;
            if cell == &ImageCellState::default() {
                background = BackgroundAccumulator::Incompatible;
                continue;
            }
            if glyph || cell.style.attrs() != Default::default() {
                per_cell_backgrounds_known = false;
                background = BackgroundAccumulator::Incompatible;
                continue;
            }
            let koshi_terminal::style::Color::Rgb(red, green, blue) = cell.style.bg() else {
                per_cell_backgrounds_known = false;
                background = BackgroundAccumulator::Incompatible;
                continue;
            };
            let color = [red, green, blue];
            background = match background {
                BackgroundAccumulator::Initial => BackgroundAccumulator::Uniform(color),
                BackgroundAccumulator::Uniform(previous) if previous == color => {
                    BackgroundAccumulator::Uniform(previous)
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
        per_cell_backgrounds_known,
        solid_background: match background {
            BackgroundAccumulator::Uniform(color) => Some(color),
            BackgroundAccumulator::Initial | BackgroundAccumulator::Incompatible => None,
        },
        default_blank,
    }
}

#[derive(Debug)]
struct Plan<'a> {
    paint: &'a OutputPaint,
    key: EncodeKey,
    iterm: ItermComposition,
    sixel: SixelComposition,
    compatibility: ImageCompatibility,
    opaque: bool,
}

fn has_known_cell_background(cells: &ImageCellSnapshot, paint: &OutputPaint) -> bool {
    (0..paint.target.height).all(|row| {
        (0..paint.target.width).all(|column| {
            let x = paint.target.x.saturating_add(column);
            let y = paint.target.y.saturating_add(row);
            let Some(cell) = cells.cell(x, y) else {
                return false;
            };
            matches!(cell.style.bg(), koshi_terminal::style::Color::Rgb(..))
                && cell.ch == ' '
                && cell.width == 1
                && cell.combining.is_empty()
                && cell.style.attrs() == Default::default()
        })
    })
}

fn classify<'a>(
    kind: ImageOutputKind,
    cells: &ImageCellSnapshot,
    covered_cells: &HashSet<(u16, u16)>,
    paint: &'a OutputPaint,
    key: EncodeKey,
) -> Result<Plan<'a>, TemplateError> {
    let stats = paint.alpha.ok_or(TemplateError::Failed)?;
    let composition = composition_info(cells, paint);
    let overlaps_lower = target_contains_covered_cell(paint.target, covered_cells);
    let known_cell_background = has_known_cell_background(cells, paint);
    let mut compatibility = ImageCompatibility::exact();
    compatibility.text_layer_order = (paint.z_index < 0 && composition.has_glyph)
        || (paint.z_index < KITTY_BACKGROUND_LAYER_Z && composition.has_non_default_background);
    match kind {
        ImageOutputKind::Kitty => Ok(Plan {
            paint,
            key,
            iterm: ItermComposition::default(),
            sixel: SixelComposition::default(),
            compatibility,
            opaque: stats.is_fully_opaque(),
        }),
        ImageOutputKind::Iterm => {
            let has_alpha = !stats.is_fully_opaque();
            let mut iterm = ItermComposition::default();
            if has_alpha {
                if composition.has_glyph {
                    compatibility.iterm_alpha = true;
                } else if overlaps_lower
                    || (!composition.default_blank && composition.solid_background.is_none())
                {
                    compatibility.iterm_alpha = true;
                    iterm.per_cell = true;
                    iterm.per_cell_backgrounds_known = composition.per_cell_backgrounds_known;
                } else if let Some(background) = composition.solid_background {
                    iterm.background = Some(background);
                } else if !composition.default_blank {
                    compatibility.iterm_alpha = true;
                }
            }
            Ok(Plan {
                paint,
                key,
                iterm,
                sixel: SixelComposition::default(),
                compatibility,
                opaque: stats.is_fully_opaque() || iterm.background.is_some(),
            })
        }
        ImageOutputKind::Sixel { .. } => {
            let terminal_background =
                paint.record.display.sixel_background == Some(SixelBackground::Terminal);
            let background = if !overlaps_lower
                && composition.solid_background.is_some()
                && ((stats.has_partial && !stats.has_zero)
                    || (stats.has_zero && terminal_background))
            {
                composition.solid_background
            } else {
                None
            };
            let alpha = stats.has_partial && background.is_none();
            let terminal_background = stats.has_zero && terminal_background && background.is_none();
            compatibility.sixel_alpha = alpha && !known_cell_background;
            compatibility.sixel_terminal_background = terminal_background && !known_cell_background;
            Ok(Plan {
                paint,
                key,
                iterm: ItermComposition::default(),
                sixel: SixelComposition {
                    background,
                    alpha,
                    terminal_background,
                },
                compatibility,
                opaque: background.is_some()
                    || (!stats.has_zero && !stats.has_partial)
                    || (known_cell_background && (alpha || terminal_background)),
            })
        }
    }
}

fn lower_images_cover_target(paint: &OutputPaint, lower: &[Plan<'_>]) -> bool {
    let mut covered = HashSet::new();
    for plan in lower {
        if !plan.opaque || plan.compatibility != ImageCompatibility::default() {
            continue;
        }
        let left = paint.target.x.max(plan.paint.target.x);
        let top = paint.target.y.max(plan.paint.target.y);
        let right = paint.target.right().min(plan.paint.target.right());
        let bottom = paint.target.bottom().min(plan.paint.target.bottom());
        for y in top..bottom {
            for x in left..right {
                covered.insert((x, y));
            }
        }
    }
    !((paint.target.y..paint.target.bottom())
        .any(|y| (paint.target.x..paint.target.right()).any(|x| !covered.contains(&(x, y)))))
}

fn resolve_compatibility(
    kind: ImageOutputKind,
    measured_cell_size: Option<PixelCellSize>,
    plans: &mut [Plan<'_>],
) {
    for index in 0..plans.len() {
        let covered = lower_images_cover_target(plans[index].paint, &plans[..index]);
        if matches!(kind, ImageOutputKind::Iterm) {
            if plans[index].iterm.per_cell
                && plans[index].iterm.per_cell_backgrounds_known
                && measured_cell_size.is_some()
            {
                plans[index].compatibility.iterm_alpha = false;
            }
            continue;
        }
        let unavailable_lower = plans[..index].iter().any(|plan| {
            plan.compatibility != ImageCompatibility::default()
                && plan
                    .paint
                    .target
                    .intersection(plans[index].paint.target)
                    .width
                    > 0
                && plan
                    .paint
                    .target
                    .intersection(plans[index].paint.target)
                    .height
                    > 0
        });
        if covered {
            let plan = &mut plans[index];
            plan.compatibility.sixel_alpha = false;
        } else if unavailable_lower && plans[index].sixel.alpha {
            plans[index].compatibility.sixel_alpha = true;
        }
    }
}

fn target_contains_covered_cell(target: Rect, covered_cells: &HashSet<(u16, u16)>) -> bool {
    (target.y..target.bottom())
        .any(|y| (target.x..target.right()).any(|x| covered_cells.contains(&(x, y))))
}

fn rectangles_overlap(first: Rect, second: Rect) -> bool {
    first.x < second.right()
        && second.x < first.right()
        && first.y < second.bottom()
        && second.y < first.bottom()
}

fn add_target_cells(target: Rect, covered_cells: &mut HashSet<(u16, u16)>) {
    for y in target.y..target.bottom() {
        for x in target.x..target.right() {
            covered_cells.insert((x, y));
        }
    }
}

fn worker_loop(requests: Receiver<WorkerRequest>, messages: SyncSender<WorkerMessage>) {
    while let Ok(request) = requests.recv() {
        if request.cancel.load(Ordering::Acquire) {
            let _ = messages.send(WorkerMessage::Finished {
                generation: request.generation,
                failed: false,
            });
            continue;
        }
        let generation = request.generation;
        let failed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_job(&request, &messages)
        })) {
            Ok(result) => result.is_err(),
            Err(_) => true,
        };
        let _ = messages.send(WorkerMessage::Finished { generation, failed });
    }
}

fn run_job(request: &WorkerRequest, messages: &SyncSender<WorkerMessage>) -> Result<(), ()> {
    run_job_with_limit(request, messages, MAX_NATIVE_FRAME_OUTPUT_BYTES)
}

fn run_job_with_limit(
    request: &WorkerRequest,
    messages: &SyncSender<WorkerMessage>,
    output_limit: usize,
) -> Result<(), ()> {
    if matches!(request.kind, ImageOutputKind::Kitty) {
        return run_kitty_job(request, messages);
    }
    let cells = request.cells.as_deref().ok_or(())?;
    let mut plans = Vec::new();
    let mut covered_cells = HashSet::new();
    if plans.try_reserve_exact(request.paints.len()).is_err() {
        return Err(());
    }
    for (paint, key) in request.paints.iter().zip(&request.keys) {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let plan = classify(request.kind, cells, &covered_cells, paint, *key).map_err(|_| ())?;
        add_target_cells(paint.target, &mut covered_cells);
        plans.push(plan);
    }
    resolve_compatibility(request.kind, request.measured_cell_size, &mut plans);

    let mut templates: Vec<Option<Vec<TemplateUnit>>> = Vec::new();
    let mut template_indices = HashMap::new();
    let mut template_bytes = 0usize;
    let mut emitted_bytes = 0usize;
    for (index, plan) in plans.iter().enumerate() {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if plan.compatibility != ImageCompatibility::default() {
            send_message(
                messages,
                &request.cancel,
                WorkerMessage::Unavailable {
                    generation: request.generation,
                    key: plan.paint.key,
                },
            )?;
            continue;
        }
        let template_index = if let Some(index) = template_indices.get(&plan.key) {
            *index
        } else {
            let remaining = output_limit.checked_sub(template_bytes).ok_or(())?;
            let template = match encode_template(request, plan, &plans[..index], remaining) {
                Ok(template) => {
                    let encoded = template
                        .iter()
                        .try_fold(0usize, |total, unit| total.checked_add(unit.bytes.len()));
                    let encoded = encoded.filter(|encoded| *encoded <= remaining).ok_or(())?;
                    template_bytes = template_bytes.checked_add(encoded).ok_or(())?;
                    Some(template)
                }
                Err(TemplateError::Unavailable) if request.cancel.load(Ordering::Acquire) => {
                    return Ok(())
                }
                Err(TemplateError::Unavailable) => None,
                Err(TemplateError::Failed) => return Err(()),
            };
            let index = templates.len();
            templates.push(template);
            template_indices.insert(plan.key, index);
            index
        };
        let Some(template) = &templates[template_index] else {
            send_message(
                messages,
                &request.cancel,
                WorkerMessage::Unavailable {
                    generation: request.generation,
                    key: plan.paint.key,
                },
            )?;
            continue;
        };
        let encoded = template
            .iter()
            .try_fold(0usize, |total, unit| total.checked_add(unit.bytes.len()));
        let encoded = encoded.ok_or(())?;
        emitted_bytes = emitted_bytes
            .checked_add(encoded)
            .filter(|bytes| *bytes <= output_limit)
            .ok_or(())?;
        send_message(
            messages,
            &request.cancel,
            WorkerMessage::Prepared {
                generation: request.generation,
                key: plan.paint.key,
                compatibility: plan.compatibility,
            },
        )?;
        for unit in template {
            send_message(
                messages,
                &request.cancel,
                WorkerMessage::Unit(OutputUnit {
                    generation: request.generation,
                    key: plan.paint.key,
                    kind: request.kind,
                    offset: unit.offset,
                    bytes: Arc::clone(&unit.bytes),
                }),
            )?;
        }
    }
    Ok(())
}

fn run_kitty_job(request: &WorkerRequest, messages: &SyncSender<WorkerMessage>) -> Result<(), ()> {
    if request.kitty_images.len() != request.paints.len() {
        return Err(());
    }
    let mut output = BoundedOutput::new(MAX_NATIVE_FRAME_OUTPUT_BYTES);
    for (paint, image) in request.paints.iter().zip(&request.kitty_images) {
        if !image.transmit {
            continue;
        }
        let mut upload =
            KittyUpload::new(Arc::clone(&paint.record.image), image.number).map_err(|_| ())?;
        while !upload.complete() {
            if request.cancel.load(Ordering::Acquire) {
                return Ok(());
            }
            upload.advance(&mut output).map_err(|_| ())?;
        }
    }
    for (index, (paint, image)) in request.paints.iter().zip(&request.kitty_images).enumerate() {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        write_cursor_position(&mut output, paint.target.x, paint.target.y).map_err(|_| ())?;
        let placement = kitty_placement(
            image.number,
            u32::try_from(index + 1).map_err(|_| ())?,
            paint,
        );
        write_kitty_placement(&mut output, &paint.record.image, &placement).map_err(|_| ())?;
    }
    for paint in &request.paints {
        send_message(
            messages,
            &request.cancel,
            WorkerMessage::Prepared {
                generation: request.generation,
                key: paint.key,
                compatibility: ImageCompatibility::exact(),
            },
        )?;
    }
    let first = request.paints.first().ok_or(())?;
    send_message(
        messages,
        &request.cancel,
        WorkerMessage::Unit(OutputUnit {
            generation: request.generation,
            key: first.key,
            kind: request.kind,
            offset: (0, 0),
            bytes: output.finish().into(),
        }),
    )
}

struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedOutput {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|length| *length <= self.limit)
            .ok_or_else(|| invalid_output("native image frame exceeds its output limit"))?;
        self.bytes
            .try_reserve(next - self.bytes.len())
            .map_err(|_| invalid_output("native image output storage could not be allocated"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn kitty_placement(image_number: u32, placement_id: u32, paint: &OutputPaint) -> KittyPlacement {
    KittyPlacement {
        image_number,
        placement_id,
        source_x: paint.source.x,
        source_y: paint.source.y,
        source_width: paint.source.width,
        source_height: paint.source.height,
        columns: u32::from(paint.target.width),
        rows: u32::from(paint.target.height),
        cell_offset_x: paint.cell_offset_x,
        cell_offset_y: paint.cell_offset_y,
        z_index: paint.z_index,
    }
}

fn kitty_output_error(error: KittyOutputError) -> io::Error {
    match error {
        KittyOutputError::Io(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidData, error),
    }
}

fn send_message(
    messages: &SyncSender<WorkerMessage>,
    cancel: &AtomicBool,
    message: WorkerMessage,
) -> Result<(), ()> {
    if cancel.load(Ordering::Acquire) {
        return Err(());
    }
    messages.send(message).map_err(|_| ())
}

#[derive(Debug)]
enum TemplateError {
    Unavailable,
    Failed,
}

#[derive(Debug)]
struct TemplateUnit {
    offset: (u16, u16),
    bytes: Arc<[u8]>,
}

fn encode_template(
    request: &WorkerRequest,
    plan: &Plan<'_>,
    lower: &[Plan<'_>],
    output_limit: usize,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    match request.kind {
        ImageOutputKind::Kitty => Err(TemplateError::Failed),
        ImageOutputKind::Iterm => encode_iterm_template(request, plan, lower, output_limit),
        ImageOutputKind::Sixel {
            palette_colors,
            max_width,
            max_height,
        } => encode_sixel_template(
            request,
            plan,
            lower,
            palette_colors,
            max_width,
            max_height,
            output_limit,
        ),
    }
}

fn encode_iterm_template(
    request: &WorkerRequest,
    plan: &Plan<'_>,
    lower: &[Plan<'_>],
    output_limit: usize,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    let image = if plan.iterm.per_cell {
        compose_iterm_image(request, plan, lower)?
    } else {
        crop_image(plan.paint, plan.iterm.background).map_err(|_| TemplateError::Failed)?
    };
    let options = OutputOptions::new(
        u32::from(plan.paint.target.width),
        u32::from(plan.paint.target.height),
    )
    .map_err(|_| TemplateError::Failed)?;
    let mut encoder = ItermEncoder::new(&image, options).map_err(|_| TemplateError::Failed)?;
    let mut units = Vec::new();
    let mut total = 0usize;
    while let Some(packet) = encoder.next_packet() {
        if packet.len() > MAX_ITERM_PACKET_BYTES {
            return Err(TemplateError::Failed);
        }
        total = checked_output_len(total, packet.len(), output_limit)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(packet.len())
            .map_err(|_| TemplateError::Failed)?;
        bytes.extend_from_slice(packet);
        units.push(TemplateUnit {
            offset: (0, 0),
            bytes: bytes.into(),
        });
    }
    Ok(units)
}

fn checked_output_len(
    total: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, TemplateError> {
    total
        .checked_add(additional)
        .filter(|length| *length <= limit)
        .ok_or(TemplateError::Failed)
}

fn compose_iterm_image(
    request: &WorkerRequest,
    plan: &Plan<'_>,
    lower: &[Plan<'_>],
) -> Result<Arc<DecodedImage>, TemplateError> {
    let cell_size = request.measured_cell_size.ok_or(TemplateError::Failed)?;
    let cell_width = u32::from(cell_size.width());
    let cell_height = u32::from(cell_size.height());
    let source = scaled_tile(
        &plan.paint.record.image,
        plan.paint.source,
        plan.paint.target,
        TileRect {
            x: 0,
            y: 0,
            width: plan.paint.target.width,
            height: plan.paint.target.height,
        },
        cell_width,
        cell_height,
        None,
    )
    .map_err(|_| TemplateError::Failed)?;
    let mut rgba = source.rgba.clone();
    let width = usize::try_from(source.width).map_err(|_| TemplateError::Failed)?;
    for y in 0..source.height {
        for x in 0..source.width {
            let global_x = u32::from(plan.paint.target.x)
                .checked_mul(cell_width)
                .and_then(|value| value.checked_add(x))
                .ok_or(TemplateError::Failed)?;
            let global_y = u32::from(plan.paint.target.y)
                .checked_mul(cell_height)
                .and_then(|value| value.checked_add(y))
                .ok_or(TemplateError::Failed)?;
            let mut under = iterm_cell_background(request, global_x, global_y)
                .ok_or(TemplateError::Unavailable)?;
            for lower_plan in lower {
                if let Some(pixel) = sample_scaled_pixel(cell_size, lower_plan, global_x, global_y)
                {
                    under = blend_pixel(pixel, [under[0], under[1], under[2]], under[3]);
                }
            }
            let index = (usize::try_from(y).map_err(|_| TemplateError::Failed)? * width
                + usize::try_from(x).map_err(|_| TemplateError::Failed)?)
                * 4;
            let pixel: [u8; 4] = rgba[index..index + 4]
                .try_into()
                .map_err(|_| TemplateError::Failed)?;
            let output = blend_pixel(pixel, [under[0], under[1], under[2]], under[3]);
            rgba[index..index + 4].copy_from_slice(&output);
        }
    }
    Ok(Arc::new(DecodedImage {
        width: source.width,
        height: source.height,
        rgba,
    }))
}

fn iterm_cell_background(request: &WorkerRequest, pixel_x: u32, pixel_y: u32) -> Option<[u8; 4]> {
    let cell_size = request.measured_cell_size?;
    let cell_x = u16::try_from(pixel_x / u32::from(cell_size.width())).ok()?;
    let cell_y = u16::try_from(pixel_y / u32::from(cell_size.height())).ok()?;
    let cell = request.cells.as_deref()?.cell(cell_x, cell_y)?;
    if cell == &ImageCellState::default() {
        return Some([0, 0, 0, 0]);
    }
    if cell_has_glyph(Some(cell)) || cell.style.attrs() != Default::default() {
        return None;
    }
    let koshi_terminal::style::Color::Rgb(red, green, blue) = cell.style.bg() else {
        return None;
    };
    Some([red, green, blue, 255])
}

#[derive(Debug, Clone, Copy)]
struct TileRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

fn encode_sixel_template(
    request: &WorkerRequest,
    plan: &Plan<'_>,
    lower: &[Plan<'_>],
    palette_colors: usize,
    max_width: Option<u32>,
    max_height: Option<u32>,
    output_limit: usize,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    let image = crop_image(plan.paint, None).map_err(|_| TemplateError::Failed)?;
    let options =
        SixelEncodeOptions::new(palette_colors.clamp(MIN_PALETTE_COLORS, MAX_PALETTE_COLORS));
    let palette = PreparedSixelPalette::prepare(&image, [0, 0, 0], options)
        .map_err(|_| TemplateError::Failed)?;
    let cell_width = u32::from(request.cell_size.width());
    let cell_height = u32::from(request.cell_size.height());
    let max_columns = max_width
        .map(|width| width / cell_width)
        .unwrap_or(u32::from(plan.paint.target.width));
    let max_rows = max_height
        .map(|height| height / cell_height)
        .unwrap_or(u32::from(plan.paint.target.height));
    let max_columns = max_columns.min(u32::from(plan.paint.target.width));
    let max_rows = max_rows.min(u32::from(plan.paint.target.height));
    if max_columns == 0 || max_rows == 0 {
        return Err(TemplateError::Failed);
    }
    let mut output = Vec::new();
    let tile_width = u16::try_from(max_columns).map_err(|_| TemplateError::Failed)?;
    let tile_height = u16::try_from(max_rows).map_err(|_| TemplateError::Failed)?;
    let target_width = plan.paint.target.width;
    let target_height = plan.paint.target.height;
    let mut output_bytes = 0usize;
    let mut y = 0;
    while y < target_height {
        let height = tile_height.min(target_height - y);
        let mut x = 0;
        while x < target_width {
            let width = tile_width.min(target_width - x);
            append_sixel_tiles(
                request,
                plan,
                &image,
                lower,
                palette_colors,
                &palette,
                cell_width,
                cell_height,
                TileRect {
                    x,
                    y,
                    width,
                    height,
                },
                &mut output,
                &mut output_bytes,
                output_limit,
            )?;
            x = x.saturating_add(width);
        }
        y = y.saturating_add(height);
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn append_sixel_tiles(
    request: &WorkerRequest,
    plan: &Plan<'_>,
    source: &DecodedImage,
    lower: &[Plan<'_>],
    palette_colors: usize,
    palette: &PreparedSixelPalette,
    cell_width: u32,
    cell_height: u32,
    tile: TileRect,
    output: &mut Vec<TemplateUnit>,
    output_bytes: &mut usize,
    output_limit: usize,
) -> Result<(), TemplateError> {
    if request.cancel.load(Ordering::Acquire) {
        return Err(TemplateError::Unavailable);
    }
    let tile_image = scaled_tile(
        source,
        ImageSourceRect {
            x: 0,
            y: 0,
            width: source.width,
            height: source.height,
        },
        plan.paint.target,
        tile,
        cell_width,
        cell_height,
        None,
    )
    .map_err(|_| TemplateError::Failed)?;
    let compose =
        plan.sixel.background.is_some() || plan.sixel.alpha || plan.sixel.terminal_background;
    let tile_image = if compose {
        compose_sixel_tile(request, plan, lower, tile, tile_image)?
    } else {
        tile_image
    };
    let mut encoder = if compose {
        SixelEncoder::with_options(
            Arc::clone(&tile_image),
            [0, 0, 0],
            SixelEncodeOptions::new(palette_colors.clamp(MIN_PALETTE_COLORS, MAX_PALETTE_COLORS)),
        )
    } else {
        SixelEncoder::with_palette(tile_image, [0, 0, 0], palette.clone())
    }
    .map_err(|_| TemplateError::Failed)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = encoder
        .next_chunk(MAX_SIXEL_TILE_BYTES)
        .map_err(|_| TemplateError::Failed)?
    {
        if request.cancel.load(Ordering::Acquire) {
            return Err(TemplateError::Unavailable);
        }
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .filter(|length| *length <= MAX_SIXEL_OUTPUT_BYTES)
            .ok_or(TemplateError::Failed)?;
        checked_output_len(*output_bytes, next_len, output_limit)?;
        bytes
            .try_reserve(next_len - bytes.len())
            .map_err(|_| TemplateError::Failed)?;
        bytes.extend_from_slice(chunk);
    }
    if bytes.is_empty() {
        return Ok(());
    }
    *output_bytes = checked_output_len(*output_bytes, bytes.len(), output_limit)?;
    output.push(TemplateUnit {
        offset: (tile.x, tile.y),
        bytes: bytes.into(),
    });
    Ok(())
}

fn compose_sixel_tile(
    request: &WorkerRequest,
    plan: &Plan<'_>,
    lower: &[Plan<'_>],
    tile: TileRect,
    source: Arc<DecodedImage>,
) -> Result<Arc<DecodedImage>, TemplateError> {
    let cell_width = u32::from(request.cell_size.width());
    let cell_height = u32::from(request.cell_size.height());
    let mut rgba = source.rgba.clone();
    let width = usize::try_from(source.width).map_err(|_| TemplateError::Failed)?;
    for y in 0..source.height {
        for x in 0..source.width {
            let global_x = u32::from(plan.paint.target.x)
                .checked_mul(cell_width)
                .and_then(|value| {
                    u32::from(tile.x)
                        .checked_mul(cell_width)
                        .and_then(|offset| value.checked_add(offset))
                })
                .and_then(|value| value.checked_add(x))
                .ok_or(TemplateError::Failed)?;
            let global_y = u32::from(plan.paint.target.y)
                .checked_mul(cell_height)
                .and_then(|value| {
                    u32::from(tile.y)
                        .checked_mul(cell_height)
                        .and_then(|offset| value.checked_add(offset))
                })
                .and_then(|value| value.checked_add(y))
                .ok_or(TemplateError::Failed)?;
            let (base, _) = terminal_background(request, global_x, global_y);
            let mut under = [base[0], base[1], base[2], 255];
            for lower_plan in lower {
                if lower_plan.compatibility != ImageCompatibility::default() {
                    continue;
                }
                if let Some(pixel) =
                    sample_scaled_pixel(request.cell_size, lower_plan, global_x, global_y)
                {
                    under = apply_sixel_layer(lower_plan, pixel, base, under);
                }
            }
            let index = (usize::try_from(y).map_err(|_| TemplateError::Failed)? * width
                + usize::try_from(x).map_err(|_| TemplateError::Failed)?)
                * 4;
            let pixel: [u8; 4] = rgba[index..index + 4]
                .try_into()
                .map_err(|_| TemplateError::Failed)?;
            let output = apply_current_sixel_layer(plan, pixel, base, under);
            rgba[index..index + 4].copy_from_slice(&output);
        }
    }
    Ok(Arc::new(DecodedImage {
        width: source.width,
        height: source.height,
        rgba,
    }))
}

fn terminal_background(request: &WorkerRequest, pixel_x: u32, pixel_y: u32) -> ([u8; 3], bool) {
    let cell_width = u32::from(request.cell_size.width());
    let cell_height = u32::from(request.cell_size.height());
    let cell_x = u16::try_from(pixel_x / cell_width).ok();
    let cell_y = u16::try_from(pixel_y / cell_height).ok();
    let Some(cell) = cell_x.and_then(|x| {
        cell_y.and_then(|y| request.cells.as_deref().and_then(|cells| cells.cell(x, y)))
    }) else {
        return ([0, 0, 0], false);
    };
    let koshi_terminal::style::Color::Rgb(red, green, blue) = cell.style.bg() else {
        return ([0, 0, 0], false);
    };
    let known = cell.ch == ' '
        && cell.width == 1
        && cell.combining.is_empty()
        && cell.style.attrs() == Default::default();
    ([red, green, blue], known)
}

fn sample_scaled_pixel(
    cell_size: PixelCellSize,
    plan: &Plan<'_>,
    pixel_x: u32,
    pixel_y: u32,
) -> Option<[u8; 4]> {
    let cell_width = u32::from(cell_size.width());
    let cell_height = u32::from(cell_size.height());
    let origin_x = u32::from(plan.paint.target.x).checked_mul(cell_width)?;
    let origin_y = u32::from(plan.paint.target.y).checked_mul(cell_height)?;
    let width = u32::from(plan.paint.target.width).checked_mul(cell_width)?;
    let height = u32::from(plan.paint.target.height).checked_mul(cell_height)?;
    let local_x = pixel_x.checked_sub(origin_x)?;
    let local_y = pixel_y.checked_sub(origin_y)?;
    if local_x >= width || local_y >= height {
        return None;
    }
    let source_x = plan.paint.source.x.checked_add(
        u32::try_from(
            u64::from(local_x)
                .checked_mul(u64::from(plan.paint.source.width))?
                .checked_div(u64::from(width))?,
        )
        .ok()?,
    )?;
    let source_y = plan.paint.source.y.checked_add(
        u32::try_from(
            u64::from(local_y)
                .checked_mul(u64::from(plan.paint.source.height))?
                .checked_div(u64::from(height))?,
        )
        .ok()?,
    )?;
    let image_width = usize::try_from(plan.paint.record.image.width).ok()?;
    let index = (usize::try_from(source_y).ok()? * image_width + usize::try_from(source_x).ok()?)
        .checked_mul(4)?;
    plan.paint
        .record
        .image
        .rgba
        .get(index..index + 4)?
        .try_into()
        .ok()
}

fn apply_sixel_layer(plan: &Plan<'_>, pixel: [u8; 4], base: [u8; 3], under: [u8; 4]) -> [u8; 4] {
    if let Some(background) = plan.sixel.background {
        return blend_pixel(pixel, background, 255);
    }
    if pixel[3] == 0 {
        return if plan.paint.record.display.sixel_background == Some(SixelBackground::Terminal) {
            [base[0], base[1], base[2], 255]
        } else {
            under
        };
    }
    blend_pixel(pixel, [under[0], under[1], under[2]], under[3])
}

fn apply_current_sixel_layer(
    plan: &Plan<'_>,
    pixel: [u8; 4],
    base: [u8; 3],
    under: [u8; 4],
) -> [u8; 4] {
    if let Some(background) = plan.sixel.background {
        return blend_pixel(pixel, background, 255);
    }
    if pixel[3] == 0 {
        return if plan.paint.record.display.sixel_background == Some(SixelBackground::Terminal) {
            [base[0], base[1], base[2], 255]
        } else {
            [0, 0, 0, 0]
        };
    }
    blend_pixel(pixel, [under[0], under[1], under[2]], under[3])
}

fn blend_pixel(pixel: [u8; 4], background: [u8; 3], background_alpha: u8) -> [u8; 4] {
    let alpha = u32::from(pixel[3]);
    let inverse = 255u32.saturating_sub(alpha);
    let background_alpha = u32::from(background_alpha);
    let output_alpha = alpha * 255 + background_alpha * inverse;
    if output_alpha == 0 {
        return [0, 0, 0, 0];
    }
    let channel = |source: u8, under: u8| {
        let numerator =
            u32::from(source) * alpha * 255 + u32::from(under) * background_alpha * inverse;
        u8::try_from((numerator + output_alpha / 2) / output_alpha).unwrap_or(255)
    };
    [
        channel(pixel[0], background[0]),
        channel(pixel[1], background[1]),
        channel(pixel[2], background[2]),
        u8::try_from(((output_alpha + 127) / 255).min(255)).unwrap_or(255),
    ]
}

fn crop_image(paint: &OutputPaint, background: Option<[u8; 3]>) -> Result<Arc<DecodedImage>, ()> {
    let image = &paint.record.image;
    let width = usize::try_from(image.width).map_err(|_| ())?;
    let height = usize::try_from(image.height).map_err(|_| ())?;
    validate_dimensions(GraphicsProtocol::Iterm2, width, height).map_err(|_| ())?;
    let expected = checked_rgba_len(GraphicsProtocol::Iterm2, width, height).map_err(|_| ())?;
    if image.rgba.len() != expected
        || paint.source.width == 0
        || paint.source.height == 0
        || paint.source.x.checked_add(paint.source.width).ok_or(())? > image.width
        || paint.source.y.checked_add(paint.source.height).ok_or(())? > image.height
    {
        return Err(());
    }
    let crop_width = usize::try_from(paint.source.width).map_err(|_| ())?;
    let crop_height = usize::try_from(paint.source.height).map_err(|_| ())?;
    let bytes_len =
        checked_rgba_len(GraphicsProtocol::Iterm2, crop_width, crop_height).map_err(|_| ())?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(bytes_len).map_err(|_| ())?;
    rgba.resize(bytes_len, 0);
    for row in 0..crop_height {
        let source_row = usize::try_from(paint.source.y).map_err(|_| ())? + row;
        let source_start =
            (source_row * width + usize::try_from(paint.source.x).map_err(|_| ())?) * 4;
        let destination_start = row * crop_width * 4;
        for column in 0..crop_width {
            let source = &image.rgba[source_start + column * 4..source_start + column * 4 + 4];
            let destination =
                &mut rgba[destination_start + column * 4..destination_start + column * 4 + 4];
            if let Some(background) = background {
                let alpha = u16::from(source[3]);
                let inverse = 255u16.saturating_sub(alpha);
                destination[0] =
                    ((u16::from(source[0]) * alpha + u16::from(background[0]) * inverse + 127)
                        / 255) as u8;
                destination[1] =
                    ((u16::from(source[1]) * alpha + u16::from(background[1]) * inverse + 127)
                        / 255) as u8;
                destination[2] =
                    ((u16::from(source[2]) * alpha + u16::from(background[2]) * inverse + 127)
                        / 255) as u8;
                destination[3] = 255;
            } else {
                destination.copy_from_slice(source);
            }
        }
    }
    Ok(Arc::new(DecodedImage {
        width: paint.source.width,
        height: paint.source.height,
        rgba,
    }))
}

fn scaled_tile(
    source: &DecodedImage,
    source_rect: ImageSourceRect,
    target: Rect,
    tile: TileRect,
    cell_width: u32,
    cell_height: u32,
    background: Option<[u8; 3]>,
) -> Result<Arc<DecodedImage>, ()> {
    let source_right = source_rect.x.checked_add(source_rect.width).ok_or(())?;
    let source_bottom = source_rect.y.checked_add(source_rect.height).ok_or(())?;
    let tile_right = tile.x.checked_add(tile.width).ok_or(())?;
    let tile_bottom = tile.y.checked_add(tile.height).ok_or(())?;
    if source_rect.width == 0
        || source_rect.height == 0
        || source_right > source.width
        || source_bottom > source.height
        || target.width == 0
        || target.height == 0
        || tile.width == 0
        || tile.height == 0
        || tile_right > target.width
        || tile_bottom > target.height
    {
        return Err(());
    }
    let width = u32::from(tile.width).checked_mul(cell_width).ok_or(())?;
    let height = u32::from(tile.height).checked_mul(cell_height).ok_or(())?;
    let full_width = u32::from(target.width).checked_mul(cell_width).ok_or(())?;
    let full_height = u32::from(target.height)
        .checked_mul(cell_height)
        .ok_or(())?;
    let width_usize = usize::try_from(width).map_err(|_| ())?;
    let height_usize = usize::try_from(height).map_err(|_| ())?;
    let bytes_len =
        checked_rgba_len(GraphicsProtocol::Sixel, width_usize, height_usize).map_err(|_| ())?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(bytes_len).map_err(|_| ())?;
    rgba.resize(bytes_len, 0);
    let source_width = usize::try_from(source.width).map_err(|_| ())?;
    for y in 0..height {
        let full_y = u32::from(tile.y)
            .checked_mul(cell_height)
            .and_then(|value| value.checked_add(y))
            .ok_or(())?;
        let source_y = source_rect
            .y
            .checked_add(
                (u64::from(full_y) * u64::from(source_rect.height) / u64::from(full_height))
                    .min(u64::from(source_rect.height - 1)) as u32,
            )
            .ok_or(())?;
        for x in 0..width {
            let full_x = u32::from(tile.x)
                .checked_mul(cell_width)
                .and_then(|value| value.checked_add(x))
                .ok_or(())?;
            let source_x = source_rect
                .x
                .checked_add(
                    (u64::from(full_x) * u64::from(source_rect.width) / u64::from(full_width))
                        .min(u64::from(source_rect.width - 1)) as u32,
                )
                .ok_or(())?;
            let source_index = (usize::try_from(source_y).map_err(|_| ())? * source_width
                + usize::try_from(source_x).map_err(|_| ())?)
                * 4;
            let destination_index = (usize::try_from(y).map_err(|_| ())? * width_usize
                + usize::try_from(x).map_err(|_| ())?)
                * 4;
            let pixel = &source.rgba[source_index..source_index + 4];
            let destination = &mut rgba[destination_index..destination_index + 4];
            if let Some(background) = background {
                let alpha = u16::from(pixel[3]);
                let inverse = 255u16.saturating_sub(alpha);
                destination[0] = ((u16::from(pixel[0]) * alpha
                    + u16::from(background[0]) * inverse
                    + 127)
                    / 255) as u8;
                destination[1] = ((u16::from(pixel[1]) * alpha
                    + u16::from(background[1]) * inverse
                    + 127)
                    / 255) as u8;
                destination[2] = ((u16::from(pixel[2]) * alpha
                    + u16::from(background[2]) * inverse
                    + 127)
                    / 255) as u8;
                destination[3] = 255;
            } else {
                destination.copy_from_slice(pixel);
            }
        }
    }
    Ok(Arc::new(DecodedImage {
        width,
        height,
        rgba,
    }))
}

fn write_cursor_position<W: Write>(writer: &mut W, x: u16, y: u16) -> io::Result<()> {
    write!(writer, "\x1b[{};{}H", u32::from(y) + 1, u32::from(x) + 1)
}

fn invalid_output(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
