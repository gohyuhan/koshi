//! Bounded connection-local iTerm2 and Sixel image output.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ratatui::layout::{Position, Rect};

use koshi_core::geometry::PixelCellSize;
use koshi_image::{checked_rgba_len, validate_dimensions, DecodedImage, GraphicsProtocol};
use koshi_iterm::{Encoder as ItermEncoder, OutputOptions, MAX_ITERM_PACKET_BYTES};
use koshi_renderer::{
    ImageCellSnapshot, ImagePaint, ImagePlacementKey, ImageSourceRect,
    MAX_IMAGE_CELL_SNAPSHOT_CELLS,
};
use koshi_sixel::{
    PreparedSixelPalette, SixelEncodeOptions, SixelEncoder, MAX_PALETTE_COLORS,
    MAX_SIXEL_OUTPUT_BYTES, MAX_SIXEL_TILE_BYTES, MIN_PALETTE_COLORS,
};
use koshi_terminal::graphics::{ImageRecord, SixelBackground};

use super::{restore_cursor_state, GraphicsSupport};

const MAX_OUTPUT_PAINTS: usize = 4096;
const MAX_REPAIR_RECTS: usize = 256;

const SIXEL_MODE_SAVE: &[u8] = b"\x1b[?80s\x1b[?8452s\x1b[?1070s";
const SIXEL_MODE_RESET: &[u8] = b"\x1b[?80l\x1b[?8452l\x1b[?1070h";
const SIXEL_MODE_RESTORE: &[u8] = b"\x1b[?80r\x1b[?8452r\x1b[?1070r";
const IMAGE_ABORT: &[u8] = b"\x18\x1b\\";

/// Output protocol settings for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImageOutputKind {
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

impl ImageOutputKind {
    /// Return the output protocol for a terminal capability.
    pub(crate) fn from_support(support: GraphicsSupport) -> Option<Self> {
        match support {
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
            GraphicsSupport::Unsupported | GraphicsSupport::Kitty => None,
        }
    }

    /// Return whether the protocol uses Sixel host modes.
    pub(crate) const fn is_sixel(self) -> bool {
        matches!(self, Self::Sixel { .. })
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
    z_index: i32,
}

impl OutputPaint {
    fn from_paint(paint: &ImagePaint) -> Self {
        Self {
            key: (paint.pane_id, paint.placement_id),
            content_id: paint.content_id,
            record: Arc::clone(&paint.record),
            target: paint.target,
            source: paint.source,
            z_index: paint.z_index,
        }
    }
}

/// Fields that require an image encoder restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EncodeKey {
    content_id: u64,
    record_address: usize,
    source: ImageSourceKey,
    target_width: u16,
    target_height: u16,
    cell_size: PixelCellSize,
    kind: ImageOutputKind,
    composition: u64,
}

impl EncodeKey {
    fn without_composition(self) -> Self {
        Self {
            composition: 0,
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageSourceKey {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

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

/// A worker request containing one bounded frame's images and one shared cell snapshot.
struct WorkerRequest {
    generation: u64,
    kind: ImageOutputKind,
    cell_size: PixelCellSize,
    cells: Arc<ImageCellSnapshot>,
    paints: Vec<OutputPaint>,
    keys: Vec<EncodeKey>,
    cancel: Arc<AtomicBool>,
}

/// A worker output unit with relative cell geometry.
#[derive(Debug)]
struct OutputUnit {
    generation: u64,
    key: ImagePlacementKey,
    encode_key: EncodeKey,
    kind: ImageOutputKind,
    offset: (u16, u16),
    first: bool,
    last: bool,
    replay: bool,
    bytes: Arc<[u8]>,
}

/// Messages sent by the bounded image worker.
#[derive(Debug)]
enum WorkerMessage {
    Prepared {
        generation: u64,
        key: ImagePlacementKey,
        encode_key: EncodeKey,
        depends_on_cells: bool,
    },
    Unavailable {
        generation: u64,
        key: ImagePlacementKey,
    },
    Unit(OutputUnit),
    Complete {
        generation: u64,
        key: ImagePlacementKey,
        encode_key: EncodeKey,
    },
    Finished {
        generation: u64,
        failed: bool,
    },
}

/// One active request and its cancellation token.
struct ActiveJob {
    generation: u64,
    keys: Vec<EncodeKey>,
    placement_keys: Vec<ImagePlacementKey>,
    cancel: Arc<AtomicBool>,
    stale: bool,
}

#[derive(Debug)]
struct CachedUnit {
    offset: (u16, u16),
    first: bool,
    last: bool,
    bytes: Arc<[u8]>,
}

struct ReplayState {
    placements: Vec<(ImagePlacementKey, EncodeKey)>,
    placement_index: usize,
    unit_index: usize,
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
    latest_keys: Vec<EncodeKey>,
    prepared: Vec<ImagePlacementKey>,
    painted: Vec<ImagePlacementKey>,
    written: Vec<ImagePlacementKey>,
    pending_unit: Option<OutputUnit>,
    base_repaint_needed: bool,
    base_ready: bool,
    repair_rects: Vec<Rect>,
    cache_building: HashMap<EncodeKey, Vec<CachedUnit>>,
    cached: HashMap<EncodeKey, Arc<[CachedUnit]>>,
    cached_keys: HashSet<EncodeKey>,
    cached_bytes: usize,
    replay: Option<ReplayState>,
    dependency_keys: HashSet<EncodeKey>,
    needs_abort: bool,
    multipart_open: bool,
}

impl ImageOutputState {
    /// Build a connection-local worker for the selected output protocol.
    pub(crate) fn new(kind: Option<ImageOutputKind>) -> Self {
        let Some(kind) = kind else {
            return Self {
                kind: None,
                requests: None,
                messages: None,
                worker: None,
                generation: 0,
                active: None,
                pending: None,
                latest: Vec::new(),
                latest_keys: Vec::new(),
                prepared: Vec::new(),
                painted: Vec::new(),
                written: Vec::new(),
                pending_unit: None,
                base_repaint_needed: false,
                base_ready: false,
                repair_rects: Vec::new(),
                cache_building: HashMap::new(),
                cached: HashMap::new(),
                cached_keys: HashSet::new(),
                cached_bytes: 0,
                replay: None,
                dependency_keys: HashSet::new(),
                needs_abort: false,
                multipart_open: false,
            };
        };
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let (message_tx, message_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || worker_loop(request_rx, message_tx));
        Self {
            kind: Some(kind),
            requests: Some(request_tx),
            messages: Some(message_rx),
            worker: Some(worker),
            generation: 0,
            active: None,
            pending: None,
            latest: Vec::new(),
            latest_keys: Vec::new(),
            prepared: Vec::new(),
            painted: Vec::new(),
            written: Vec::new(),
            pending_unit: None,
            base_repaint_needed: false,
            base_ready: false,
            repair_rects: Vec::new(),
            cache_building: HashMap::new(),
            cached: HashMap::new(),
            cached_keys: HashSet::new(),
            cached_bytes: 0,
            replay: None,
            dependency_keys: HashSet::new(),
            needs_abort: false,
            multipart_open: false,
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

    /// Return whether a base-cell repaint must precede native units.
    pub(crate) const fn base_repaint_needed(&self) -> bool {
        self.base_repaint_needed
    }

    /// Return whether a screen reset is required to remove old image pixels.
    pub(crate) fn screen_reset_needed(&self) -> bool {
        !self.repair_rects.is_empty()
    }

    /// Return whether this state needs another attachment-loop pass.
    pub(crate) fn work_pending(&self) -> bool {
        self.active.is_some()
            || self.pending.is_some()
            || self.pending_unit.is_some()
            || self.replay.is_some()
            || self.base_repaint_needed
            || self.screen_reset_needed()
            || self.needs_abort
    }

    /// Submit the newest frame, retaining one compatible active request.
    pub(crate) fn submit_frame(
        &mut self,
        paints: &[ImagePaint],
        cells: Option<Arc<ImageCellSnapshot>>,
        cell_size: Option<PixelCellSize>,
    ) {
        self.poll();
        let Some(kind) = self.kind else {
            return;
        };
        let cell_size = if kind.is_sixel() {
            let Some(cell_size) = cell_size else {
                self.invalidate_current();
                return;
            };
            cell_size
        } else {
            PixelCellSize::new(1, 1).expect("one-pixel cell is nonzero")
        };
        let Some(cells) = cells else {
            self.invalidate_current();
            return;
        };
        if paints.len() > MAX_OUTPUT_PAINTS
            || usize::try_from(cells.area.area()).ok() > Some(MAX_IMAGE_CELL_SNAPSHOT_CELLS)
        {
            self.invalidate_current();
            return;
        }
        let mut latest = Vec::new();
        if latest.try_reserve_exact(paints.len()).is_err() {
            self.invalidate_current();
            return;
        }
        latest.extend(paints.iter().map(OutputPaint::from_paint));
        let keys = latest
            .iter()
            .map(|paint| self.encode_key(kind, cell_size, &cells, paint))
            .collect::<Vec<_>>();
        let old_latest = std::mem::replace(&mut self.latest, latest);
        let old_keys = std::mem::replace(&mut self.latest_keys, keys.clone());
        self.prune_cache(&keys);
        let new_latest = self.latest.clone();
        let damaged = self.record_position_damage(&old_latest, &new_latest);
        let moved = old_latest.iter().any(|old| {
            new_latest
                .iter()
                .find(|new| new.key == old.key)
                .is_some_and(|new| new.target != old.target)
        });

        let request_cancel = Arc::new(AtomicBool::new(false));
        let request = WorkerRequest {
            generation: self.next_generation(),
            kind,
            cell_size,
            cells,
            paints: self.latest.clone(),
            keys: keys.clone(),
            cancel: Arc::clone(&request_cancel),
        };
        let Some(active) = self.active.as_ref() else {
            if old_keys == keys && self.all_cached(&keys) {
                if moved || damaged {
                    self.painted.clear();
                    self.base_repaint_needed = true;
                    self.base_ready = false;
                    self.prepare_replay();
                }
                return;
            }
            self.clear_pending_output();
            self.start_request(request);
            return;
        };
        let placement_keys = placement_keys(&self.latest);
        if !active.stale && active.keys == keys && active.placement_keys == placement_keys {
            if moved {
                self.painted.clear();
                self.base_repaint_needed = true;
                self.base_ready = false;
            }
            return;
        }

        let removed = active
            .placement_keys
            .iter()
            .any(|key| !placement_keys.contains(key));
        self.clear_pending_output();
        if removed {
            if let Some(active) = self.active.take() {
                if self.multipart_open {
                    self.needs_abort = true;
                }
                active.cancel.store(true, Ordering::Release);
            }
            self.start_request(request);
        } else {
            if let Some(active) = self.active.as_mut() {
                active.stale = true;
            }
            if let Some(previous) = self.pending.replace(request) {
                previous.cancel.store(true, Ordering::Release);
            }
        }
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
                    break;
                }
            };
            match message {
                WorkerMessage::Prepared {
                    generation,
                    key,
                    encode_key,
                    depends_on_cells,
                } => {
                    if self.current_generation(generation) && self.latest.contains_key(&key) {
                        if depends_on_cells {
                            self.dependency_keys
                                .insert(encode_key.without_composition());
                        }
                        if let Some(index) = self.latest.iter().position(|paint| paint.key == key) {
                            self.latest_keys[index] = encode_key;
                            if let Some(active) = self.active.as_mut() {
                                if let Some(active_key) = active
                                    .placement_keys
                                    .iter()
                                    .position(|candidate| *candidate == key)
                                {
                                    active.keys[active_key] = encode_key;
                                }
                            }
                        }
                        if !self.prepared.contains(&key) && self.prepared.len() < MAX_OUTPUT_PAINTS
                        {
                            self.prepared.push(key);
                        }
                        self.base_repaint_needed = true;
                        self.base_ready = false;
                    }
                }
                WorkerMessage::Unavailable { generation, key } => {
                    if self.current_generation(generation) {
                        self.prepared.retain(|candidate| *candidate != key);
                        self.painted.retain(|candidate| *candidate != key);
                        self.pending_unit = self.pending_unit.take().filter(|unit| unit.key != key);
                    }
                }
                WorkerMessage::Unit(unit) => {
                    self.remember_unit(&unit);
                    if self.current_generation(unit.generation)
                        && self.prepared.contains(&unit.key)
                        && self.latest.contains_key(&unit.key)
                        && self.pending_unit.is_none()
                    {
                        self.pending_unit = Some(unit);
                        break;
                    }
                }
                WorkerMessage::Complete {
                    generation,
                    key,
                    encode_key,
                } => {
                    if self.current_generation(generation)
                        && self.latest.contains_key(&key)
                        && !self.painted.contains(&key)
                    {
                        self.finish_cached_template(encode_key);
                        if self.painted.len() < MAX_OUTPUT_PAINTS {
                            self.painted.push(key);
                        }
                    }
                }
                WorkerMessage::Finished { generation, failed } => {
                    if failed {
                        self.cache_building.clear();
                        if self.current_generation(generation) {
                            self.invalidate_worker_failure();
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

    /// Mark the ordinary cell buffer as painted before native units are written.
    pub(crate) fn mark_base_painted(&mut self) {
        if self.base_repaint_needed {
            self.base_repaint_needed = false;
            self.base_ready = true;
            self.repair_rects.clear();
        }
    }

    /// Advance one complete iTerm packet or Sixel tile.
    pub(crate) fn advance<W: Write>(
        &mut self,
        writer: &mut W,
        cursor: Option<Position>,
    ) -> io::Result<bool> {
        self.poll();
        if self.needs_abort {
            if let Err(error) = write_image_abort(writer).and_then(|()| writer.flush()) {
                self.invalidate_after_write_failure();
                return Err(error);
            }
            self.needs_abort = false;
            self.multipart_open = false;
        }
        if !self.base_ready {
            return Ok(false);
        }
        let Some(unit) = self.pending_unit.take() else {
            self.pending_unit = self.take_replay_unit();
            let Some(unit) = self.pending_unit.take() else {
                return Ok(false);
            };
            return self.write_unit(writer, cursor, unit);
        };
        self.write_unit(writer, cursor, unit)
    }

    fn write_unit<W: Write>(
        &mut self,
        writer: &mut W,
        cursor: Option<Position>,
        unit: OutputUnit,
    ) -> io::Result<bool> {
        let Some(paint) = self.latest.get_key(&unit.key) else {
            return Ok(false);
        };
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
        let mut output = Vec::new();
        let overhead = if unit.kind.is_sixel() { 128 } else { 64 };
        output
            .try_reserve(unit.bytes.len().saturating_add(overhead))
            .map_err(|_| invalid_output("image output storage could not be allocated"))?;
        match unit.kind {
            ImageOutputKind::Iterm => {
                if unit.first {
                    write_cursor_position(&mut output, x, y)?;
                }
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
        if let Err(error) = writer.write_all(&output).and_then(|()| writer.flush()) {
            let abort_result = write_image_abort(writer).and_then(|()| writer.flush());
            self.needs_abort = abort_result.is_err();
            self.multipart_open = false;
            self.invalidate_after_write_failure();
            return Err(error);
        }
        if matches!(unit.kind, ImageOutputKind::Iterm) {
            self.multipart_open = !unit.last;
        }
        if !self.written.contains(&unit.key) && self.written.len() < MAX_OUTPUT_PAINTS {
            self.written.push(unit.key);
        }
        if unit.replay
            && unit.last
            && !self.painted.contains(&unit.key)
            && self.painted.len() < MAX_OUTPUT_PAINTS
        {
            self.painted.push(unit.key);
        }
        Ok(true)
    }

    /// Reset this output state when a connection is replaced.
    pub(crate) fn reset_connection(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Release);
        }
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.repair_native_output();
        self.generation = self.generation.saturating_add(1);
        self.clear_pending_output();
        self.needs_abort = false;
        self.multipart_open = false;
        self.prepared.clear();
        self.painted.clear();
        self.written.clear();
        self.base_repaint_needed = !self.repair_rects.is_empty();
        self.base_ready = false;
    }

    fn all_cached(&self, keys: &[EncodeKey]) -> bool {
        keys.iter().all(|key| self.cached.contains_key(key))
    }

    fn prepare_replay(&mut self) {
        let placements = self
            .latest
            .iter()
            .zip(&self.latest_keys)
            .map(|(paint, key)| (paint.key, *key))
            .filter(|(_, key)| self.cached.contains_key(key))
            .collect::<Vec<_>>();
        if !placements.is_empty() && placements.len() == self.latest.len() {
            self.replay = Some(ReplayState {
                placements,
                placement_index: 0,
                unit_index: 0,
            });
        } else {
            self.replay = None;
        }
    }

    fn take_replay_unit(&mut self) -> Option<OutputUnit> {
        let replay = self.replay.as_mut()?;
        loop {
            let (key, encode_key) = *replay.placements.get(replay.placement_index)?;
            let units = self.cached.get(&encode_key)?;
            let Some(cached) = units.get(replay.unit_index) else {
                replay.placement_index = replay.placement_index.saturating_add(1);
                replay.unit_index = 0;
                continue;
            };
            replay.unit_index = replay.unit_index.saturating_add(1);
            return Some(OutputUnit {
                generation: self.generation,
                key,
                encode_key,
                kind: self.kind?,
                offset: cached.offset,
                first: cached.first,
                last: cached.last,
                replay: true,
                bytes: Arc::clone(&cached.bytes),
            });
        }
    }

    fn remember_unit(&mut self, unit: &OutputUnit) {
        if self.cached_keys.contains(&unit.encode_key) {
            return;
        }
        let entry = self.cache_building.entry(unit.encode_key).or_default();
        let next = entry
            .iter()
            .map(|cached| cached.bytes.len())
            .sum::<usize>()
            .saturating_add(unit.bytes.len());
        if self
            .cached_bytes
            .saturating_add(next)
            .saturating_sub(entry.iter().map(|cached| cached.bytes.len()).sum::<usize>())
            > MAX_SIXEL_OUTPUT_BYTES
        {
            self.cache_building.remove(&unit.encode_key);
            self.cached_keys.insert(unit.encode_key);
            return;
        }
        entry.push(CachedUnit {
            offset: unit.offset,
            first: unit.first,
            last: unit.last,
            bytes: Arc::clone(&unit.bytes),
        });
    }

    fn finish_cached_template(&mut self, key: EncodeKey) {
        if self.cached_keys.contains(&key) {
            return;
        }
        self.cached_keys.insert(key);
        let Some(units) = self.cache_building.remove(&key) else {
            return;
        };
        let bytes = units.iter().map(|unit| unit.bytes.len()).sum::<usize>();
        if self.cached_bytes.saturating_add(bytes) > MAX_SIXEL_OUTPUT_BYTES {
            return;
        }
        self.cached_bytes = self.cached_bytes.saturating_add(bytes);
        self.cached.insert(key, units.into());
    }

    fn prune_cache(&mut self, keys: &[EncodeKey]) {
        self.cached.retain(|key, units| {
            if keys.contains(key) {
                true
            } else {
                self.cached_bytes = self
                    .cached_bytes
                    .saturating_sub(units.iter().map(|unit| unit.bytes.len()).sum::<usize>());
                false
            }
        });
        self.cache_building.retain(|key, _| keys.contains(key));
        self.cached_keys.retain(|key| keys.contains(key));
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    fn start_request(&mut self, request: WorkerRequest) {
        let active = ActiveJob {
            generation: request.generation,
            keys: request.keys.clone(),
            placement_keys: placement_keys(&request.paints),
            cancel: Arc::clone(&request.cancel),
            stale: false,
        };
        let Some(sender) = &self.requests else {
            return;
        };
        match sender.try_send(request) {
            Ok(()) => {
                self.active = Some(active);
                self.prepared.clear();
                self.painted.clear();
                self.written.clear();
                self.pending_unit = None;
                self.base_repaint_needed = !self.repair_rects.is_empty();
                self.base_ready = false;
            }
            Err(TrySendError::Full(request)) => {
                self.pending = Some(request);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn start_pending(&mut self) {
        let Some(request) = self.pending.take() else {
            return;
        };
        self.start_request(request);
    }

    fn current_generation(&self, generation: u64) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.generation == generation && !active.stale)
    }

    fn record_position_damage(&mut self, old: &[OutputPaint], new: &[OutputPaint]) -> bool {
        let mut damaged = false;
        for paint in old {
            if !new.iter().any(|candidate| candidate.key == paint.key)
                && (self.painted.contains(&paint.key)
                    || self.prepared.contains(&paint.key)
                    || self.written.contains(&paint.key))
            {
                self.add_repair(paint.target);
                damaged = true;
            }
        }
        for paint in new {
            let Some(previous) = old.iter().find(|candidate| candidate.key == paint.key) else {
                continue;
            };
            if (previous.target != paint.target
                || previous.content_id != paint.content_id
                || !Arc::ptr_eq(&previous.record, &paint.record)
                || previous.source != paint.source
                || previous.z_index != paint.z_index)
                && (self.painted.contains(&previous.key)
                    || self.prepared.contains(&previous.key)
                    || self.written.contains(&previous.key))
            {
                self.add_repair(previous.target);
                self.add_repair(paint.target);
                damaged = true;
            }
        }
        damaged
    }

    fn add_repair(&mut self, rect: Rect) {
        if rect.width == 0 || rect.height == 0 || self.repair_rects.contains(&rect) {
            return;
        }
        if self.repair_rects.len() < MAX_REPAIR_RECTS {
            self.repair_rects.push(rect);
        }
    }

    fn clear_pending_output(&mut self) {
        self.pending_unit = None;
        if self.multipart_open {
            self.needs_abort = true;
        }
        self.replay = None;
        self.prepared.clear();
        self.painted.clear();
    }

    fn invalidate_worker_failure(&mut self) {
        self.repair_native_output();
        self.clear_pending_output();
        self.written.clear();
        self.base_repaint_needed = !self.repair_rects.is_empty();
        self.base_ready = false;
    }

    fn invalidate_current(&mut self) {
        self.repair_native_output();
        if let Some(active) = self.active.take() {
            if self.multipart_open {
                self.needs_abort = true;
            }
            active.cancel.store(true, Ordering::Release);
        }
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
        }
        self.clear_pending_output();
        self.written.clear();
        self.base_repaint_needed = !self.repair_rects.is_empty();
        self.base_ready = false;
    }

    fn invalidate_after_write_failure(&mut self) {
        self.repair_native_output();
        if let Some(active) = self.active.take() {
            active.cancel.store(true, Ordering::Release);
        }
        self.clear_pending_output();
        self.written.clear();
        self.base_repaint_needed = !self.repair_rects.is_empty();
        self.base_ready = false;
    }

    fn repair_native_output(&mut self) {
        let targets: Vec<Rect> = self
            .latest
            .iter()
            .filter(|paint| {
                self.prepared.contains(&paint.key)
                    || self.painted.contains(&paint.key)
                    || self.written.contains(&paint.key)
            })
            .map(|paint| paint.target)
            .collect();
        for target in targets {
            self.add_repair(target);
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

trait PaintList {
    fn get_key(&self, key: &ImagePlacementKey) -> Option<&OutputPaint>;
    fn contains_key(&self, key: &ImagePlacementKey) -> bool;
}

impl PaintList for Vec<OutputPaint> {
    fn get_key(&self, key: &ImagePlacementKey) -> Option<&OutputPaint> {
        self.iter().find(|paint| &paint.key == key)
    }

    fn contains_key(&self, key: &ImagePlacementKey) -> bool {
        self.iter().any(|paint| &paint.key == key)
    }
}

fn placement_keys(paints: &[OutputPaint]) -> Vec<ImagePlacementKey> {
    paints.iter().map(|paint| paint.key).collect()
}

fn output_encode_key(
    kind: ImageOutputKind,
    cell_size: PixelCellSize,
    paint: &OutputPaint,
) -> EncodeKey {
    EncodeKey {
        content_id: paint.content_id,
        record_address: Arc::as_ptr(&paint.record) as usize,
        source: ImageSourceKey::from_source(paint.source),
        target_width: paint.target.width,
        target_height: paint.target.height,
        cell_size,
        kind,
        composition: 0,
    }
}

impl ImageOutputState {
    fn encode_key(
        &self,
        kind: ImageOutputKind,
        cell_size: PixelCellSize,
        cells: &ImageCellSnapshot,
        paint: &OutputPaint,
    ) -> EncodeKey {
        let key = output_encode_key(kind, cell_size, paint);
        if self.dependency_keys.contains(&key) {
            EncodeKey {
                composition: composition_fingerprint(cells, paint),
                ..key
            }
        } else {
            key
        }
    }
}

fn composition_fingerprint(cells: &ImageCellSnapshot, paint: &OutputPaint) -> u64 {
    let mut hash = 1469598103934665603u64;
    for row in 0..paint.target.height {
        for column in 0..paint.target.width {
            let x = paint.target.x.saturating_add(column);
            let y = paint.target.y.saturating_add(row);
            let Some(cell) = cells.cell(x, y) else {
                hash = mix_hash(hash, u64::MAX);
                continue;
            };
            hash = mix_color(hash, cell.style.bg());
        }
    }
    hash
}

fn mix_color(hash: u64, color: koshi_terminal::style::Color) -> u64 {
    match color {
        koshi_terminal::style::Color::Default => mix_hash(hash, 0),
        koshi_terminal::style::Color::Indexed(value) => mix_hash(hash, 1 << 8 | u64::from(value)),
        koshi_terminal::style::Color::Rgb(red, green, blue) => mix_hash(
            hash,
            2 << 24 | u64::from(red) << 16 | u64::from(green) << 8 | u64::from(blue),
        ),
    }
}

fn mix_hash(hash: u64, value: u64) -> u64 {
    (hash ^ value).wrapping_mul(1099511628211)
}

#[derive(Debug, Clone, Copy, Default)]
struct AlphaStats {
    has_zero: bool,
    has_partial: bool,
    has_opaque: bool,
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
                255 => stats.has_opaque = true,
                _ => stats.has_partial = true,
            }
        }
    }
    Some(stats)
}

#[derive(Debug, Clone, Copy)]
struct CompositionInfo {
    has_glyph: bool,
    solid_background: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Copy)]
enum BackgroundAccumulator {
    Initial,
    Uniform([u8; 3]),
    Incompatible,
}

fn composition_info(cells: &ImageCellSnapshot, paint: &OutputPaint) -> CompositionInfo {
    let mut has_glyph = false;
    let mut background = BackgroundAccumulator::Initial;
    for row in 0..paint.target.height {
        for column in 0..paint.target.width {
            let x = paint.target.x.saturating_add(column);
            let y = paint.target.y.saturating_add(row);
            let Some(cell) = cells.cell(x, y) else {
                has_glyph = true;
                background = BackgroundAccumulator::Incompatible;
                continue;
            };
            let glyph = cell.ch != ' ' || cell.width != 1 || cell.has_combining;
            has_glyph |= glyph;
            if glyph || cell.style.attrs() != Default::default() {
                background = BackgroundAccumulator::Incompatible;
                continue;
            }
            let koshi_terminal::style::Color::Rgb(red, green, blue) = cell.style.bg() else {
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
        solid_background: match background {
            BackgroundAccumulator::Uniform(color) => Some(color),
            BackgroundAccumulator::Initial | BackgroundAccumulator::Incompatible => None,
        },
    }
}

#[derive(Debug, Clone, Copy)]
struct Plan<'a> {
    paint: &'a OutputPaint,
    key: EncodeKey,
    background: Option<[u8; 3]>,
    depends_on_cells: bool,
}

fn classify<'a>(
    kind: ImageOutputKind,
    cells: &ImageCellSnapshot,
    plans: &[Plan<'a>],
    paint: &'a OutputPaint,
    key: EncodeKey,
) -> Option<Plan<'a>> {
    let stats = alpha_stats(&paint.record.image, paint.source)?;
    let composition = composition_info(cells, paint);
    if paint.z_index < 0 && composition.has_glyph && stats.has_opaque {
        return None;
    }
    if !stats.has_zero && !stats.has_partial {
        return Some(Plan {
            paint,
            key,
            background: None,
            depends_on_cells: false,
        });
    }
    if paint.z_index < 0 && composition.has_glyph {
        return None;
    }
    let overlaps_lower = plans.iter().any(|plan| {
        plan.paint.target.intersection(paint.target).width > 0
            && plan.paint.target.intersection(paint.target).height > 0
    });
    match kind {
        ImageOutputKind::Iterm => {
            if overlaps_lower && (stats.has_zero || stats.has_partial) {
                return None;
            }
            if stats.has_partial && composition.solid_background.is_none() {
                return None;
            }
            if stats.has_zero && composition.has_glyph && composition.solid_background.is_none() {
                return None;
            }
            Some(Plan {
                paint,
                key,
                background: composition.solid_background,
                depends_on_cells: false,
            })
        }
        ImageOutputKind::Sixel { .. } => {
            if stats.has_partial && (overlaps_lower || composition.solid_background.is_none()) {
                return None;
            }
            if stats.has_zero
                && paint.record.display.sixel_background == Some(SixelBackground::Terminal)
                && composition.solid_background.is_none()
            {
                return None;
            }
            Some(Plan {
                paint,
                key,
                background: composition.solid_background.filter(|_| stats.has_partial),
                depends_on_cells: false,
            })
        }
    }
}

fn worker_loop(requests: Receiver<WorkerRequest>, messages: SyncSender<WorkerMessage>) {
    while let Ok(request) = requests.recv() {
        if request.cancel.load(Ordering::Acquire) {
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
    let mut plans = Vec::new();
    if plans.try_reserve_exact(request.paints.len()).is_err() {
        return Err(());
    }
    for (paint, key) in request.paints.iter().zip(&request.keys) {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(mut plan) = classify(request.kind, &request.cells, &plans, paint, *key) {
            plan.depends_on_cells = plan.background.is_some();
            if plan.depends_on_cells {
                plan.key = EncodeKey {
                    composition: composition_fingerprint(&request.cells, paint),
                    ..plan.key
                };
            }
            plans.push(plan);
        }
    }

    let mut groups: Vec<(EncodeKey, Vec<usize>)> = Vec::new();
    let mut group_indices = HashMap::new();
    for (index, plan) in plans.iter().enumerate() {
        let group = if let Some(group) = group_indices.get(&plan.key) {
            *group
        } else {
            let group = groups.len();
            group_indices.insert(plan.key, group);
            groups.push((plan.key, Vec::new()));
            group
        };
        groups[group].1.push(index);
    }

    for (key, indices) in groups {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let first = plans[indices[0]];
        let template = match encode_template(request, first) {
            Ok(template) => template,
            Err(TemplateError::Unavailable) => {
                for index in indices {
                    send_message(
                        messages,
                        &request.cancel,
                        WorkerMessage::Unavailable {
                            generation: request.generation,
                            key: plans[index].paint.key,
                        },
                    )?;
                }
                continue;
            }
            Err(TemplateError::Failed) => {
                for index in indices {
                    send_message(
                        messages,
                        &request.cancel,
                        WorkerMessage::Unavailable {
                            generation: request.generation,
                            key: plans[index].paint.key,
                        },
                    )?;
                }
                continue;
            }
        };
        for index in &indices {
            send_message(
                messages,
                &request.cancel,
                WorkerMessage::Prepared {
                    generation: request.generation,
                    key: plans[*index].paint.key,
                    encode_key: plans[*index].key,
                    depends_on_cells: plans[*index].depends_on_cells,
                },
            )?;
        }
        for index in indices {
            for (unit_index, unit) in template.iter().enumerate() {
                send_message(
                    messages,
                    &request.cancel,
                    WorkerMessage::Unit(OutputUnit {
                        generation: request.generation,
                        key: plans[index].paint.key,
                        encode_key: plans[index].key,
                        kind: request.kind,
                        offset: unit.offset,
                        first: unit_index == 0,
                        last: unit_index + 1 == template.len(),
                        replay: false,
                        bytes: Arc::clone(&unit.bytes),
                    }),
                )?;
            }
            send_message(
                messages,
                &request.cancel,
                WorkerMessage::Complete {
                    generation: request.generation,
                    key: plans[index].paint.key,
                    encode_key: plans[index].key,
                },
            )?;
        }
        let _ = key;
    }
    Ok(())
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
    plan: Plan<'_>,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    match request.kind {
        ImageOutputKind::Iterm => encode_iterm_template(plan),
        ImageOutputKind::Sixel {
            palette_colors,
            max_width,
            max_height,
        } => encode_sixel_template(request, plan, palette_colors, max_width, max_height),
    }
}

fn encode_iterm_template(plan: Plan<'_>) -> Result<Vec<TemplateUnit>, TemplateError> {
    let image = crop_image(plan.paint, plan.background).map_err(|_| TemplateError::Failed)?;
    let options = OutputOptions::new(
        u32::from(plan.paint.target.width),
        u32::from(plan.paint.target.height),
    )
    .map_err(|_| TemplateError::Unavailable)?;
    let mut encoder = ItermEncoder::new(&image, options).map_err(|_| TemplateError::Failed)?;
    let mut units = Vec::new();
    let mut total = 0usize;
    while let Some(packet) = encoder.next_packet() {
        if packet.len() > MAX_ITERM_PACKET_BYTES {
            return Err(TemplateError::Failed);
        }
        total = total
            .checked_add(packet.len())
            .ok_or(TemplateError::Failed)?;
        if total > MAX_SIXEL_OUTPUT_BYTES {
            return Err(TemplateError::Failed);
        }
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

#[derive(Debug, Clone, Copy)]
struct TileRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

fn encode_sixel_template(
    request: &WorkerRequest,
    plan: Plan<'_>,
    palette_colors: usize,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Result<Vec<TemplateUnit>, TemplateError> {
    if palette_colors < MIN_PALETTE_COLORS {
        return Err(TemplateError::Unavailable);
    }
    let image = crop_image(plan.paint, plan.background).map_err(|_| TemplateError::Failed)?;
    let options = SixelEncodeOptions::new(palette_colors.min(MAX_PALETTE_COLORS));
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
        return Err(TemplateError::Unavailable);
    }
    let mut output = Vec::new();
    append_sixel_tiles(
        request,
        plan,
        &image,
        &palette,
        cell_width,
        cell_height,
        TileRect {
            x: 0,
            y: 0,
            width: u16::try_from(max_columns).map_err(|_| TemplateError::Failed)?,
            height: u16::try_from(max_rows).map_err(|_| TemplateError::Failed)?,
        },
        &mut output,
    )?;
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn append_sixel_tiles(
    request: &WorkerRequest,
    plan: Plan<'_>,
    source: &DecodedImage,
    palette: &PreparedSixelPalette,
    cell_width: u32,
    cell_height: u32,
    tile: TileRect,
    output: &mut Vec<TemplateUnit>,
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
    let mut encoder =
        SixelEncoder::with_palette(tile_image, [0, 0, 0], palette.clone_for_encoder())
            .map_err(|_| TemplateError::Failed)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = encoder
        .next_chunk(MAX_SIXEL_TILE_BYTES)
        .map_err(|_| TemplateError::Failed)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_SIXEL_TILE_BYTES {
            if tile.width == 1 && tile.height == 1 {
                return Err(TemplateError::Unavailable);
            }
            let (first, second) = split_tile(tile);
            append_sixel_tiles(
                request,
                plan,
                source,
                palette,
                cell_width,
                cell_height,
                first,
                output,
            )?;
            append_sixel_tiles(
                request,
                plan,
                source,
                palette,
                cell_width,
                cell_height,
                second,
                output,
            )?;
            return Ok(());
        }
        bytes.extend_from_slice(chunk);
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let total = output
        .iter()
        .try_fold(bytes.len(), |total, unit| {
            total.checked_add(unit.bytes.len())
        })
        .ok_or(TemplateError::Failed)?;
    if total > MAX_SIXEL_OUTPUT_BYTES {
        return Err(TemplateError::Failed);
    }
    output.push(TemplateUnit {
        offset: (tile.x, tile.y),
        bytes: bytes.into(),
    });
    if tile.x + tile.width < plan.paint.target.width {
        let next = TileRect {
            x: tile.x + tile.width,
            y: tile.y,
            width: (plan.paint.target.width - tile.x - tile.width).min(tile.width),
            height: tile.height,
        };
        append_sixel_tiles(
            request,
            plan,
            source,
            palette,
            cell_width,
            cell_height,
            next,
            output,
        )?;
    } else if tile.y + tile.height < plan.paint.target.height {
        let next = TileRect {
            x: 0,
            y: tile.y + tile.height,
            width: tile.width.min(plan.paint.target.width),
            height: (plan.paint.target.height - tile.y - tile.height).min(tile.height),
        };
        append_sixel_tiles(
            request,
            plan,
            source,
            palette,
            cell_width,
            cell_height,
            next,
            output,
        )?;
    }
    Ok(())
}

fn split_tile(tile: TileRect) -> (TileRect, TileRect) {
    if tile.width >= tile.height && tile.width > 1 {
        let left = tile.width / 2;
        (
            TileRect {
                width: left,
                ..tile
            },
            TileRect {
                x: tile.x + left,
                width: tile.width - left,
                ..tile
            },
        )
    } else {
        let top = tile.height / 2;
        (
            TileRect {
                height: top,
                ..tile
            },
            TileRect {
                y: tile.y + top,
                height: tile.height - top,
                ..tile
            },
        )
    }
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
        let source_y = (u64::from(full_y) * u64::from(source_rect.height) / u64::from(full_height))
            .min(u64::from(source_rect.height - 1)) as u32;
        for x in 0..width {
            let full_x = u32::from(tile.x)
                .checked_mul(cell_width)
                .and_then(|value| value.checked_add(x))
                .ok_or(())?;
            let source_x = (u64::from(full_x) * u64::from(source_rect.width)
                / u64::from(full_width))
            .min(u64::from(source_rect.width - 1)) as u32;
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

trait PaletteClone {
    fn clone_for_encoder(&self) -> PreparedSixelPalette;
}

impl PaletteClone for PreparedSixelPalette {
    fn clone_for_encoder(&self) -> PreparedSixelPalette {
        self.clone()
    }
}

fn write_cursor_position<W: Write>(writer: &mut W, x: u16, y: u16) -> io::Result<()> {
    write!(writer, "\x1b[{};{}H", u32::from(y) + 1, u32::from(x) + 1)
}

fn invalid_output(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
