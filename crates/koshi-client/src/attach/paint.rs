//! The frame reader: turning the [`PaintedFrame`](koshi_ipc::frame::PaintedFrame)
//! a session sends into the
//! [`RenderSnapshot`](koshi_renderer::snapshot::RenderSnapshot) this process
//! paints.
//!
//! [`to_snapshot`](crate::attach::paint::to_snapshot) is the inverse of
//! [`wire_frame`](koshi_runtime::runtime::frame::wire_frame), with the four
//! names the answering session chose filtered on the way in. The session, tab,
//! slot, tab-bar and client parts already hold shared types, so they copy
//! straight back. Each pane's cells are rebuilt: every
//! [`FrameRow`](koshi_ipc::frame::FrameRow) expands its runs back into cells,
//! each cell becomes a [`Cell`](koshi_terminal::grid::state::Cell), and the rows
//! become one [`Grid`](koshi_terminal::grid::state::Grid) behind an
//! [`Arc`](std::sync::Arc).
//!
//! Image placements arrive in the painted frame without RGBA. `ImageCache`
//! keeps complete records by their connection-local content identity and
//! rebuilds the newest snapshot as bounded image chunks finish. A missing
//! record stays as a placement with no pixels, which the renderer draws as
//! `terminal image unavailable`.
//!
//! A run travels once and expands back into as many cells as it stood for: a
//! blank 80-column row arrives as one run with `count: 80` and rebuilds into 80
//! blank cells.
//!
//! The session name, the active tab's name, every tab-bar entry's name and
//! every pane title pass through
//! [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text). This
//! process paints those four into its own terminal and puts two of them inside
//! an `OSC 0` window-title sequence, so a control character in one of them
//! would reach the terminal as a control character. A pane's cells are not
//! filtered: they are the pane's screen, and every byte in them is already a
//! grid cell.
//!
//! Plugin UI does not travel, so every frame read here carries the default
//! [`PluginUiSnapshot`](koshi_renderer::snapshot::PluginUiSnapshot).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use koshi_core::text::sanitize_reported_text;
use koshi_ipc::frame::{
    FrameCell, FrameColor, FrameCursorShape, FrameGraphicsProtocol, FrameImageAction,
    FrameImageChunk, FrameImageDimension, FrameImageDisplay, FrameImagePlacement,
    FrameImageRecordHeader, FrameImageTransfer, FramePane, FrameRow, FrameRowEnd,
    FrameSixelBackground, FrameSlot, FrameStyle, FrameTabMeta, FrameUnderline, FrameWindow,
    PaintedFrame, MAX_FRAME_IMAGE_CHUNK_BYTES, MAX_FRAME_IMAGE_TRANSFERS,
    MAX_FRAME_IMAGE_TRANSFER_BYTES,
};
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot,
    PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta,
    TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};
use koshi_terminal::grid::state::{Cell, Grid, RowEnd};
use koshi_terminal::state::CursorShape;
use koshi_terminal::style::{Color, Style, UnderlineStyle};

#[cfg(test)]
mod tests;

/// Turn one frame read off the event stream into the snapshot the renderer
/// draws, with every image placement marked unavailable.
///
/// The session name, the active tab's name, every tab-bar entry's name and
/// every pane title are filtered by [`sanitize_reported_text`]. A frame naming
/// its session `"dev\u{7}"` reads back naming it `"dev"`.
#[must_use]
pub fn to_snapshot(frame: &PaintedFrame) -> RenderSnapshot {
    to_snapshot_with_images(frame, &HashMap::new())
}

/// Turn one frame into a render snapshot using image records already received.
fn to_snapshot_with_images(
    frame: &PaintedFrame,
    images: &HashMap<u64, Arc<ImageRecord>>,
) -> RenderSnapshot {
    let tab = &frame.session.active_tab;
    RenderSnapshot {
        session: SessionSnapshot {
            id: frame.session.id,
            name: sanitize_reported_text(&frame.session.name),
            active_tab: TabSnapshot {
                id: tab.id,
                name: sanitize_reported_text(&tab.name),
                layout_solved: tab.slots.iter().map(to_slot).collect(),
                effective_size: tab.effective_size,
                stack_headers: tab.stack_headers.clone(),
                layout_mode: tab.layout_mode,
                all_suppressed: tab.all_suppressed,
                gap: tab.gap,
            },
            tabs_metadata: frame.session.tabs.iter().map(to_tab_meta).collect(),
        },
        panes: frame
            .panes
            .iter()
            .map(|pane| to_pane(pane, images))
            .collect(),
        client: ClientSnapshot {
            id: frame.client.id,
            viewport: frame.client.viewport,
            active_tab: frame.client.active_tab,
            focused_pane: frame.client.focused_pane,
            lock_mode: frame.client.lock_mode,
            mouse_select: frame.client.mouse_select,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

/// Image records retained by one attached client connection.
pub(crate) struct ImageCache {
    /// Complete records, keyed by identities in painted frames.
    images: HashMap<u64, Arc<ImageRecord>>,
    /// Total RGBA bytes retained in `images`.
    retained_bytes: u64,
    /// The newest painted frame, retained while records arrive.
    frame: Option<Box<PaintedFrame>>,
    /// Record identities the newest frame still needs.
    missing: HashSet<u64>,
    /// Image transfers accepted after the newest painted frame.
    transfer_count: usize,
    /// The record whose chunks are currently arriving.
    pending: Option<PendingImage>,
}

/// One image transfer and the bytes received for it.
struct PendingImage {
    transfer: FrameImageTransfer,
    rgba: Vec<u8>,
    received: u64,
}

/// A malformed or incomplete image transfer stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageAssemblyError {
    /// Two placements in one pane use one terminal-local identity.
    DuplicatePlacement,
    /// More image transfers followed one painted frame than the batch limit allows.
    TransferCountExceedsFrame,
    /// A transfer does not belong to the newest painted frame.
    UnknownTransfer(u64),
    /// Another image transfer is still open.
    TransferAlreadyOpen,
    /// A complete image record was sent again.
    TransferAlreadyComplete(u64),
    /// A transfer byte length cannot be allocated by this process.
    ByteLengthDoesNotFit,
    /// No painted frame is available for the transfer.
    MissingBaseFrame,
    /// A chunk does not continue at the expected raw-byte offset.
    WrongOffset {
        /// The transfer identity.
        transfer_id: u64,
        /// The offset the receiver expects.
        expected: u64,
        /// The offset the chunk names.
        actual: u64,
    },
    /// A chunk exceeds the declared image byte length.
    ChunkExceedsImage,
    /// The chunk's final marker does not match its ending offset.
    FinalMarkerMismatch,
    /// A chunk is larger than the event limit.
    ChunkTooLarge,
    /// The retained and incoming image bytes exceed the connection limit.
    TransferBytesExceedFrame,
    /// A transfer's declared length does not match its pixel dimensions.
    InvalidTransferLength,
    /// A placement or its complete record cannot become valid render state.
    InvalidPlacement,
}

impl fmt::Display for ImageAssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicatePlacement => {
                formatter.write_str("image placement identity is repeated in one pane")
            }
            Self::TransferCountExceedsFrame => {
                formatter.write_str("painted frame exceeds the image-transfer batch limit")
            }
            Self::UnknownTransfer(id) => {
                write!(formatter, "image transfer identity {id} is not needed")
            }
            Self::TransferAlreadyOpen => {
                formatter.write_str("another image transfer is still open")
            }
            Self::TransferAlreadyComplete(id) => {
                write!(
                    formatter,
                    "image transfer identity {id} is already complete"
                )
            }
            Self::ByteLengthDoesNotFit => {
                formatter.write_str("image transfer byte length cannot fit this process")
            }
            Self::MissingBaseFrame => formatter.write_str("image transfer has no painted frame"),
            Self::WrongOffset {
                transfer_id,
                expected,
                actual,
            } => write!(
                formatter,
                "image transfer {transfer_id} expects offset {expected}, got {actual}"
            ),
            Self::ChunkExceedsImage => formatter.write_str("image chunk exceeds its image"),
            Self::FinalMarkerMismatch => {
                formatter.write_str("image chunk final marker does not match its length")
            }
            Self::ChunkTooLarge => formatter.write_str("image chunk exceeds its event limit"),
            Self::TransferBytesExceedFrame => {
                formatter.write_str("image records exceed the connection byte limit")
            }
            Self::InvalidTransferLength => {
                formatter.write_str("image transfer length does not match its dimensions")
            }
            Self::InvalidPlacement => {
                formatter.write_str("image transfer cannot become a render placement")
            }
        }
    }
}

impl std::error::Error for ImageAssemblyError {}

impl ImageCache {
    /// Build an empty image cache for one connection.
    pub(crate) fn new() -> Self {
        Self {
            images: HashMap::new(),
            retained_bytes: 0,
            frame: None,
            missing: HashSet::new(),
            transfer_count: 0,
            pending: None,
        }
    }

    /// Discard every connection-local image record and incomplete transfer.
    pub(crate) fn reset(&mut self) {
        self.images.clear();
        self.retained_bytes = 0;
        self.frame = None;
        self.missing.clear();
        self.transfer_count = 0;
        self.pending = None;
    }

    /// Adopt a painted frame, prune unreferenced records, and expose missing placements.
    pub(crate) fn begin_frame(
        &mut self,
        frame: Box<PaintedFrame>,
    ) -> Result<RenderSnapshot, ImageAssemblyError> {
        let placement_count = frame
            .panes
            .iter()
            .try_fold(0usize, |count, pane| {
                count.checked_add(pane.image_placements.len())
            })
            .ok_or(ImageAssemblyError::TransferCountExceedsFrame)?;
        let mut content_ids = HashSet::new();
        let mut placements = HashSet::new();
        content_ids
            .try_reserve(placement_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        placements
            .try_reserve(placement_count)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        for pane in &frame.panes {
            for placement in &pane.image_placements {
                if !placements.insert((pane.id, placement.id)) {
                    return Err(ImageAssemblyError::DuplicatePlacement);
                }
                let valid = match (placement.available, self.images.get(&placement.content_id)) {
                    (false, _) => ImagePlacementSnapshot::unavailable(
                        placement.id,
                        placement.content_id,
                        placement.anchor,
                        placement.columns,
                        placement.rows,
                    )
                    .is_some(),
                    (true, Some(record)) => ImagePlacementSnapshot::with_content_id(
                        placement.id,
                        placement.content_id,
                        Arc::clone(record),
                        placement.anchor,
                        placement.columns,
                        placement.rows,
                    )
                    .is_some(),
                    (true, None) => ImagePlacementSnapshot::unavailable(
                        placement.id,
                        placement.content_id,
                        placement.anchor,
                        placement.columns,
                        placement.rows,
                    )
                    .is_some(),
                };
                if !valid {
                    return Err(ImageAssemblyError::InvalidPlacement);
                }
                if placement.available {
                    content_ids.insert(placement.content_id);
                }
            }
        }

        self.images.retain(|id, _| content_ids.contains(id));
        self.retained_bytes = retained_image_bytes(&self.images)?;
        self.missing = content_ids
            .into_iter()
            .filter(|id| !self.images.contains_key(id))
            .collect();
        self.pending = None;
        self.transfer_count = 0;
        self.frame = Some(frame);
        self.snapshot()
    }

    /// Start receiving one record needed by the newest painted frame.
    pub(crate) fn start(&mut self, transfer: FrameImageTransfer) -> Result<(), ImageAssemblyError> {
        if self.frame.is_none() {
            return Err(ImageAssemblyError::MissingBaseFrame);
        }
        if self.pending.is_some() {
            return Err(ImageAssemblyError::TransferAlreadyOpen);
        }
        if self.images.contains_key(&transfer.id) {
            return Err(ImageAssemblyError::TransferAlreadyComplete(transfer.id));
        }
        if !self.missing.contains(&transfer.id) {
            return Err(ImageAssemblyError::UnknownTransfer(transfer.id));
        }
        if self.transfer_count >= MAX_FRAME_IMAGE_TRANSFERS {
            return Err(ImageAssemblyError::TransferCountExceedsFrame);
        }
        let expected_bytes = u64::from(transfer.record.width)
            .checked_mul(u64::from(transfer.record.height))
            .and_then(|pixels| pixels.checked_mul(4));
        if expected_bytes != Some(transfer.byte_len) || transfer.byte_len == 0 {
            return Err(ImageAssemblyError::InvalidTransferLength);
        }
        let total_bytes = self
            .retained_bytes
            .checked_add(transfer.byte_len)
            .ok_or(ImageAssemblyError::TransferBytesExceedFrame)?;
        if total_bytes > MAX_FRAME_IMAGE_TRANSFER_BYTES {
            return Err(ImageAssemblyError::TransferBytesExceedFrame);
        }
        let capacity = usize::try_from(transfer.byte_len)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(capacity)
            .map_err(|_| ImageAssemblyError::ByteLengthDoesNotFit)?;
        self.pending = Some(PendingImage {
            transfer,
            rgba,
            received: 0,
        });
        self.transfer_count += 1;
        Ok(())
    }

    /// Accept one chunk and return a complete frame when every missing record arrived.
    pub(crate) fn accept(
        &mut self,
        chunk: FrameImageChunk,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        let result = self.accept_inner(chunk);
        if result.is_err() {
            self.pending = None;
        }
        result
    }

    /// Validate and append one chunk to the open transfer.
    fn accept_inner(
        &mut self,
        chunk: FrameImageChunk,
    ) -> Result<Option<RenderSnapshot>, ImageAssemblyError> {
        if chunk.bytes.is_empty() {
            return Err(ImageAssemblyError::FinalMarkerMismatch);
        }
        if chunk.bytes.len() > MAX_FRAME_IMAGE_CHUNK_BYTES {
            return Err(ImageAssemblyError::ChunkTooLarge);
        }
        let image = self
            .pending
            .as_mut()
            .filter(|image| image.transfer.id == chunk.transfer_id)
            .ok_or(ImageAssemblyError::UnknownTransfer(chunk.transfer_id))?;
        if chunk.offset != image.received {
            return Err(ImageAssemblyError::WrongOffset {
                transfer_id: chunk.transfer_id,
                expected: image.received,
                actual: chunk.offset,
            });
        }
        let chunk_len =
            u64::try_from(chunk.bytes.len()).map_err(|_| ImageAssemblyError::ChunkExceedsImage)?;
        let end = chunk
            .offset
            .checked_add(chunk_len)
            .ok_or(ImageAssemblyError::ChunkExceedsImage)?;
        if end > image.transfer.byte_len {
            return Err(ImageAssemblyError::ChunkExceedsImage);
        }
        if chunk.last != (end == image.transfer.byte_len) {
            return Err(ImageAssemblyError::FinalMarkerMismatch);
        }
        image.rgba.extend_from_slice(&chunk.bytes);
        image.received = end;
        if end != image.transfer.byte_len {
            return Ok(None);
        }

        let image = self
            .pending
            .take()
            .ok_or(ImageAssemblyError::UnknownTransfer(chunk.transfer_id))?;
        let id = image.transfer.id;
        let record = Arc::new(to_image_record(&image.transfer.record, image.rgba));
        self.validate_record(id, &record)?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(image.transfer.byte_len)
            .ok_or(ImageAssemblyError::TransferBytesExceedFrame)?;
        self.images.insert(id, record);
        self.missing.remove(&id);
        if !self.missing.is_empty() {
            return Ok(None);
        }
        self.snapshot().map(Some)
    }

    /// Check a complete record against every placement that names it.
    fn validate_record(
        &self,
        content_id: u64,
        record: &Arc<ImageRecord>,
    ) -> Result<(), ImageAssemblyError> {
        let frame = self
            .frame
            .as_ref()
            .ok_or(ImageAssemblyError::MissingBaseFrame)?;
        let valid = frame.panes.iter().all(|pane| {
            pane.image_placements
                .iter()
                .filter(|placement| placement.available && placement.content_id == content_id)
                .all(|placement| {
                    ImagePlacementSnapshot::with_content_id(
                        placement.id,
                        placement.content_id,
                        Arc::clone(record),
                        placement.anchor,
                        placement.columns,
                        placement.rows,
                    )
                    .is_some()
                })
        });
        if valid {
            Ok(())
        } else {
            Err(ImageAssemblyError::InvalidPlacement)
        }
    }

    /// Rebuild the newest painted frame with each available cached record.
    fn snapshot(&self) -> Result<RenderSnapshot, ImageAssemblyError> {
        self.frame
            .as_deref()
            .map(|frame| to_snapshot_with_images(frame, &self.images))
            .ok_or(ImageAssemblyError::MissingBaseFrame)
    }
}

/// Count the RGBA bytes in complete retained image records.
fn retained_image_bytes(
    images: &HashMap<u64, Arc<ImageRecord>>,
) -> Result<u64, ImageAssemblyError> {
    images.values().try_fold(0u64, |total, record| {
        let len = u64::try_from(record.image.rgba.len())
            .map_err(|_| ImageAssemblyError::TransferBytesExceedFrame)?;
        total
            .checked_add(len)
            .ok_or(ImageAssemblyError::TransferBytesExceedFrame)
    })
}

/// One solved pane placement, as the renderer reads it.
fn to_slot(slot: &FrameSlot) -> PaneSlot {
    PaneSlot {
        pane_id: slot.pane_id,
        rect: slot.rect,
        inner_rect: slot.inner_rect,
        kind: slot.kind,
        visible: slot.visible,
        suppressed: slot.suppressed,
        dead: slot.dead,
    }
}

/// One tab-bar entry, as the renderer reads it. The name is filtered by
/// [`sanitize_reported_text`].
fn to_tab_meta(meta: &FrameTabMeta) -> TabMeta {
    TabMeta {
        id: meta.id,
        name: sanitize_reported_text(&meta.name),
        index: meta.index,
        active: meta.active,
    }
}

/// One pane's content, as the renderer reads it. A pane that sent no window has
/// no grid. The title is filtered by [`sanitize_reported_text`]; the cells are
/// not.
fn to_pane(pane: &FramePane, images: &HashMap<u64, Arc<ImageRecord>>) -> PaneSnapshot {
    PaneSnapshot {
        id: pane.id,
        title: pane.title.as_deref().map(sanitize_reported_text),
        cursor: CursorSnapshot {
            row: pane.cursor.row,
            col: pane.cursor.col,
            visible: pane.cursor.visible,
            blink: pane.cursor.blink,
            shape: pane.cursor.shape.as_ref().map(to_cursor_shape),
        },
        grid_view: pane.window.as_ref().map(to_grid_view),
        image_placements: pane
            .image_placements
            .iter()
            .filter_map(|placement| to_image_placement(placement, images))
            .collect(),
        reverse_video: pane.reverse_video,
        mouse_tracking: pane.mouse_tracking,
        alt_scroll: pane.alt_scroll,
        on_alt_screen: pane.on_alt_screen,
        view_top_row: pane.view_top_row,
        selection: pane.selection.as_ref().map(|selection| SelectionSpans {
            rows: selection.rows.clone(),
        }),
        has_selection: pane.has_selection,
        scrollback: ScrollbackMeta {
            truncated: pane.scrollback.truncated,
            retained_lines: pane.scrollback.retained_lines,
        },
    }
}

/// One wire image placement, with its cached record when the transfer completed.
fn to_image_placement(
    placement: &FrameImagePlacement,
    images: &HashMap<u64, Arc<ImageRecord>>,
) -> Option<ImagePlacementSnapshot> {
    if !placement.available {
        return ImagePlacementSnapshot::unavailable(
            placement.id,
            placement.content_id,
            placement.anchor,
            placement.columns,
            placement.rows,
        );
    }
    images
        .get(&placement.content_id)
        .and_then(|record| {
            ImagePlacementSnapshot::with_content_id(
                placement.id,
                placement.content_id,
                Arc::clone(record),
                placement.anchor,
                placement.columns,
                placement.rows,
            )
        })
        .or_else(|| {
            ImagePlacementSnapshot::unavailable(
                placement.id,
                placement.content_id,
                placement.anchor,
                placement.columns,
                placement.rows,
            )
        })
}

/// One complete image record rebuilt from transfer metadata and RGBA bytes.
fn to_image_record(record: &FrameImageRecordHeader, rgba: Vec<u8>) -> ImageRecord {
    ImageRecord {
        protocol: to_graphics_protocol(record.protocol),
        image: DecodedImage {
            width: record.width,
            height: record.height,
            rgba,
        },
        action: to_image_action(record.action),
        display: to_image_display(&record.display),
        anchor: record.anchor,
    }
}

/// The source image protocol restored from the wire.
fn to_graphics_protocol(protocol: FrameGraphicsProtocol) -> GraphicsProtocol {
    match protocol {
        FrameGraphicsProtocol::Sixel => GraphicsProtocol::Sixel,
        FrameGraphicsProtocol::Kitty => GraphicsProtocol::Kitty,
        FrameGraphicsProtocol::Iterm2 => GraphicsProtocol::Iterm2,
    }
}

/// The wire image operation restored from the wire.
fn to_image_action(action: FrameImageAction) -> ImageAction {
    match action {
        FrameImageAction::Transmit => ImageAction::Transmit,
        FrameImageAction::Display => ImageAction::Display,
        FrameImageAction::TransmitAndDisplay => ImageAction::TransmitAndDisplay,
    }
}

/// One wire dimension restored as terminal image metadata.
fn to_image_dimension(dimension: FrameImageDimension) -> ImageDimension {
    match dimension {
        FrameImageDimension::Cells(value) => ImageDimension::Cells(value),
        FrameImageDimension::Pixels(value) => ImageDimension::Pixels(value),
        FrameImageDimension::Percent(value) => ImageDimension::Percent(value),
        FrameImageDimension::Auto => ImageDimension::Auto,
    }
}

/// One wire Sixel background rule restored as terminal image metadata.
fn to_sixel_background(background: FrameSixelBackground) -> SixelBackground {
    match background {
        FrameSixelBackground::Terminal => SixelBackground::Terminal,
        FrameSixelBackground::Preserve => SixelBackground::Preserve,
    }
}

/// Display metadata restored from the wire.
fn to_image_display(display: &FrameImageDisplay) -> ImageDisplay {
    ImageDisplay {
        width: display.width.map(to_image_dimension),
        height: display.height.map(to_image_dimension),
        preserve_aspect_ratio: display.preserve_aspect_ratio,
        sixel_background: display.sixel_background.map(to_sixel_background),
        image_id: display.image_id,
        image_number: display.image_number,
        placement_id: display.placement_id,
        usage_hints: display.usage_hints,
        unicode_placeholder: display.unicode_placeholder,
        z_index: display.z_index,
        cell_columns: display.cell_columns,
        cell_rows: display.cell_rows,
        source_offset_x: display.source_offset_x,
        source_offset_y: display.source_offset_y,
        cell_offset_x: display.cell_offset_x,
        cell_offset_y: display.cell_offset_y,
        move_cursor: display.move_cursor,
    }
}

/// The pane's visible cells as one grid, plus how far its view is scrolled
/// back.
fn to_grid_view(window: &FrameWindow) -> GridView {
    let rows: Vec<Vec<Cell>> = window.rows.iter().map(to_row).collect();
    // `from_rows` starts every row `Hard`; each row the wire ends another way
    // is set back afterwards.
    let mut grid = Grid::from_rows(rows, window.cols, Style::default());
    for (index, row) in window.rows.iter().enumerate() {
        let end = to_row_end(row.end);
        if end != RowEnd::Hard {
            grid.set_row_end(u16::try_from(index).unwrap_or(u16::MAX), end);
        }
    }
    GridView {
        grid: Arc::new(grid),
        view_offset: window.view_offset,
    }
}

/// One row's line-continuation state, read back from the wire.
fn to_row_end(end: FrameRowEnd) -> RowEnd {
    match end {
        FrameRowEnd::Hard => RowEnd::Hard,
        FrameRowEnd::Soft => RowEnd::Soft,
        FrameRowEnd::SoftWide => RowEnd::SoftWide,
    }
}

/// One row's runs, expanded back into cells: each run's cell is built once and
/// repeated `count` times. A run with `count: 80` yields 80 equal cells.
fn to_row(row: &FrameRow) -> Vec<Cell> {
    let width = row.runs.iter().map(|run| usize::from(run.count)).sum();
    let mut cells = Vec::with_capacity(width);
    for run in &row.runs {
        cells.extend(std::iter::repeat_n(
            to_cell(&run.cell),
            usize::from(run.count),
        ));
    }
    cells
}

/// One cell: its character, the rest of its grapheme cluster layered back on in
/// arrival order, its display width, and its style.
fn to_cell(cell: &FrameCell) -> Cell {
    let mut built = Cell::new(cell.ch, cell.width, to_style(&cell.style));
    for mark in &cell.combining {
        built.push_combining(*mark);
    }
    built
}

/// One cell's colors and text attributes.
fn to_style(style: &FrameStyle) -> Style {
    let mut built = Style::default();
    built.set_fg(to_color(&style.fg));
    built.set_bg(to_color(&style.bg));
    built.set_underline_color(style.underline_color.as_ref().map(to_color));
    built.set_bold(style.attrs.bold);
    built.set_italic(style.attrs.italic);
    built.set_underline(to_underline(&style.attrs.underline));
    built.set_reverse(style.attrs.reverse);
    built.set_faint(style.attrs.faint);
    built.set_blink(style.attrs.blink);
    built.set_conceal(style.attrs.conceal);
    built.set_strike(style.attrs.strike);
    built.set_overline(style.attrs.overline);
    built
}

/// One foreground, background or underline color.
fn to_color(color: &FrameColor) -> Color {
    match color {
        FrameColor::Default => Color::Default,
        FrameColor::Indexed(index) => Color::Indexed(*index),
        FrameColor::Rgb(red, green, blue) => Color::Rgb(*red, *green, *blue),
    }
}

/// One cell's underline style.
fn to_underline(underline: &FrameUnderline) -> UnderlineStyle {
    match underline {
        FrameUnderline::None => UnderlineStyle::None,
        FrameUnderline::Single => UnderlineStyle::Single,
        FrameUnderline::Double => UnderlineStyle::Double,
        FrameUnderline::Curly => UnderlineStyle::Curly,
        FrameUnderline::Dotted => UnderlineStyle::Dotted,
        FrameUnderline::Dashed => UnderlineStyle::Dashed,
    }
}

/// The shape a pane asked its cursor to be drawn as.
fn to_cursor_shape(shape: &FrameCursorShape) -> CursorShape {
    match shape {
        FrameCursorShape::Block => CursorShape::Block,
        FrameCursorShape::Underline => CursorShape::Underline,
        FrameCursorShape::Bar => CursorShape::Bar,
    }
}
