//! The wire-frame builder: turning the in-process [`RenderSnapshot`] into the
//! [`PaintedFrame`] a client in another process draws.
//!
//! [`wire_frame`] is a plain field-for-field mapping. The session, tab, slot,
//! tab-bar and client parts already hold shared types
//! ([`Rect`](koshi_core::geometry::Rect),
//! [`Size`](koshi_core::geometry::Size),
//! [`StackHeader`](koshi_layout::solver::StackHeader),
//! [`LayoutMode`](koshi_layout::mode::LayoutMode),
//! [`PaneKind`](koshi_pane::pane::state::PaneKind),
//! [`MouseTracking`](koshi_core::mouse::MouseTracking)), so they copy straight
//! across. The terminal types do not travel, so each pane's
//! [`Grid`] is read cell by cell into a [`FrameWindow`], and
//! [`Style`], [`Color`], [`UnderlineStyle`] and
//! [`CursorShape`] are re-spelled in koshi-ipc's own enums.
//!
//! Rows are run-length encoded from source [`Cell`] values into [`FrameRun`]
//! records: a 1×3 row holding a styled `e` then two default blanks travels as
//! two runs — `count: 1` for the `e`, `count: 2` for the blanks.
//!
//! Plugin UI does not travel.
//! [`build_snapshot`](crate::server::Server::build_snapshot) always sets the
//! default, and [`PaintedFrame`] has no slot for it.

use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{
    FrameAttrs, FrameCell, FrameClient, FrameColor, FrameCursor, FrameCursorShape,
    FrameDecodedImage, FrameGraphicsProtocol, FrameImageAction, FrameImageDimension,
    FrameImageDisplay, FrameImagePlacement, FrameImageRecord, FrameImageRecordHeader,
    FrameImageTransfer, FramePane, FrameRow, FrameRowEnd, FrameRun, FrameScrollback,
    FrameSelection, FrameSession, FrameSixelBackground, FrameSlot, FrameStyle, FrameTab,
    FrameTabMeta, FrameUnderline, FrameWindow, PaintedFrame, MAX_FRAME_IMAGE_CHUNK_BYTES,
};
use koshi_renderer::snapshot::{
    GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot, RenderSnapshot, TabMeta,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};
use koshi_terminal::grid::state::{Cell, Grid, RowEnd};
use koshi_terminal::state::CursorShape;
use koshi_terminal::style::{Color, Style, UnderlineStyle};

/// Turn one painted frame into the form it travels in.
///
/// Carries the session's identity, its solved active tab, the tab-bar entries,
/// every pane's content, and the viewing client's own state. Carries no plugin
/// UI.
#[must_use]
pub fn wire_frame(snapshot: &RenderSnapshot) -> PaintedFrame {
    let tab = &snapshot.session.active_tab;
    PaintedFrame {
        session: FrameSession {
            id: snapshot.session.id,
            name: snapshot.session.name.clone(),
            active_tab: FrameTab {
                id: tab.id,
                name: tab.name.clone(),
                slots: tab.layout_solved.iter().map(wire_slot).collect(),
                effective_size: tab.effective_size,
                stack_headers: tab.stack_headers.clone(),
                layout_mode: tab.layout_mode,
                all_suppressed: tab.all_suppressed,
                gap: tab.gap,
            },
            tabs: snapshot
                .session
                .tabs_metadata
                .iter()
                .map(wire_tab_meta)
                .collect(),
        },
        panes: snapshot.panes.iter().map(wire_pane).collect(),
        client: FrameClient {
            id: snapshot.client.id,
            viewport: snapshot.client.viewport,
            active_tab: snapshot.client.active_tab,
            focused_pane: snapshot.client.focused_pane,
            lock_mode: snapshot.client.lock_mode,
            mouse_select: snapshot.client.mouse_select,
        },
    }
}

/// Build one metadata event per image whose pixels need separate events.
#[must_use]
pub fn wire_chunked_frame_starts(frame: &PaintedFrame, frame_id: u64) -> Option<Vec<SessionEvent>> {
    if frame_id == 0 {
        return None;
    }
    let images: Vec<FrameImageTransfer> = frame
        .panes
        .iter()
        .flat_map(|pane| {
            pane.image_placements
                .iter()
                .map(move |placement| (pane.id, placement))
        })
        .enumerate()
        .map(|(index, (pane_id, placement))| {
            wire_image_transfer(transfer_id(index), pane_id, placement)
        })
        .collect();
    if images.is_empty() {
        return None;
    }

    Some(
        images
            .into_iter()
            .map(|image| SessionEvent::PaintedImageStart {
                frame_id,
                images: vec![image],
            })
            .collect(),
    )
}

/// Build the ordinary frame that older clients paint before image chunks.
#[must_use]
pub fn wire_chunked_frame_base(frame: &PaintedFrame) -> PaintedFrame {
    frame.without_image_placements()
}

/// Return the raw image chunks in the transfer order named by the start event.
pub fn wire_image_chunk_sources<'a>(
    frame: &'a PaintedFrame,
) -> impl Iterator<Item = (u64, u64, bool, &'a [u8])> + 'a {
    frame
        .panes
        .iter()
        .flat_map(|pane| pane.image_placements.iter())
        .enumerate()
        .flat_map(|(index, placement)| {
            let transfer_id = transfer_id(index);
            let total = placement.record.image.rgba.len();
            placement
                .record
                .image
                .rgba
                .chunks(MAX_FRAME_IMAGE_CHUNK_BYTES)
                .enumerate()
                .map(move |(chunk_index, bytes)| {
                    let chunk_index = u64::try_from(chunk_index)
                        .expect("an image chunk index fits in a frame identity");
                    let chunk_size = u64::try_from(MAX_FRAME_IMAGE_CHUNK_BYTES)
                        .expect("the image chunk size fits in a frame offset");
                    let offset = chunk_index * chunk_size;
                    let last = offset.checked_add(
                        u64::try_from(bytes.len())
                            .expect("an image chunk length fits in a frame offset"),
                    ) == Some(
                        u64::try_from(total).expect("an image byte count fits in a frame"),
                    );
                    (transfer_id, offset, last, bytes)
                })
        })
}

/// Turn one complete wire placement into the metadata a chunked transfer names.
fn wire_image_transfer(
    id: u64,
    pane_id: koshi_core::ids::PaneId,
    placement: &FrameImagePlacement,
) -> FrameImageTransfer {
    FrameImageTransfer {
        id,
        pane_id,
        placement_id: placement.id,
        record: FrameImageRecordHeader {
            protocol: placement.record.protocol,
            width: placement.record.image.width,
            height: placement.record.image.height,
            action: placement.record.action,
            display: placement.record.display.clone(),
            anchor: placement.record.anchor,
        },
        anchor: placement.anchor,
        columns: placement.columns,
        rows: placement.rows,
        byte_len: u64::try_from(placement.record.image.rgba.len())
            .expect("an image byte count fits in a frame transfer"),
    }
}

/// Number the image in its flattened pane-order position.
fn transfer_id(index: usize) -> u64 {
    u64::try_from(index)
        .expect("an image placement index fits in a frame identity")
        .saturating_add(1)
}

/// One solved pane placement, as it travels.
fn wire_slot(slot: &PaneSlot) -> FrameSlot {
    FrameSlot {
        pane_id: slot.pane_id,
        rect: slot.rect,
        inner_rect: slot.inner_rect,
        kind: slot.kind,
        visible: slot.visible,
        suppressed: slot.suppressed,
        dead: slot.dead,
    }
}

/// One tab-bar entry, as it travels.
fn wire_tab_meta(meta: &TabMeta) -> FrameTabMeta {
    FrameTabMeta {
        id: meta.id,
        name: meta.name.clone(),
        index: meta.index,
        active: meta.active,
    }
}

/// One pane's content, as it travels. A pane with no terminal content sends no
/// window.
fn wire_pane(pane: &PaneSnapshot) -> FramePane {
    FramePane {
        id: pane.id,
        title: pane.title.clone(),
        cursor: FrameCursor {
            row: pane.cursor.row,
            col: pane.cursor.col,
            visible: pane.cursor.visible,
            blink: pane.cursor.blink,
            shape: pane.cursor.shape.map(wire_cursor_shape),
        },
        window: pane.grid_view.as_ref().map(wire_window),
        image_placements: pane
            .image_placements
            .iter()
            .map(wire_image_placement)
            .collect(),
        reverse_video: pane.reverse_video,
        mouse_tracking: pane.mouse_tracking,
        alt_scroll: pane.alt_scroll,
        on_alt_screen: pane.on_alt_screen,
        view_top_row: pane.view_top_row,
        selection: pane.selection.as_ref().map(|selection| FrameSelection {
            rows: selection.rows.clone(),
        }),
        has_selection: pane.has_selection,
        scrollback: FrameScrollback {
            truncated: pane.scrollback.truncated,
            retained_lines: pane.scrollback.retained_lines,
        },
    }
}

/// One validated image placement, as it travels with its pane.
fn wire_image_placement(placement: &ImagePlacementSnapshot) -> FrameImagePlacement {
    FrameImagePlacement {
        id: placement.id(),
        record: wire_image_record(placement.record()),
        anchor: placement.anchor(),
        columns: placement.dimensions().1,
        rows: placement.dimensions().0,
    }
}

/// One complete image record, with terminal types re-spelled in wire enums.
fn wire_image_record(record: &ImageRecord) -> FrameImageRecord {
    FrameImageRecord {
        protocol: wire_graphics_protocol(record.protocol),
        image: wire_decoded_image(&record.image),
        action: wire_image_action(record.action),
        display: wire_image_display(&record.display),
        anchor: record.anchor,
    }
}

/// The validated RGBA image carried by a record.
fn wire_decoded_image(image: &DecodedImage) -> FrameDecodedImage {
    FrameDecodedImage {
        width: image.width,
        height: image.height,
        rgba: image.rgba.clone(),
    }
}

/// The source image protocol in wire form.
fn wire_graphics_protocol(protocol: GraphicsProtocol) -> FrameGraphicsProtocol {
    match protocol {
        GraphicsProtocol::Sixel => FrameGraphicsProtocol::Sixel,
        GraphicsProtocol::Kitty => FrameGraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2 => FrameGraphicsProtocol::Iterm2,
    }
}

/// The image operation in wire form.
fn wire_image_action(action: ImageAction) -> FrameImageAction {
    match action {
        ImageAction::Transmit => FrameImageAction::Transmit,
        ImageAction::Display => FrameImageAction::Display,
        ImageAction::TransmitAndDisplay => FrameImageAction::TransmitAndDisplay,
    }
}

/// One protocol dimension in wire form.
fn wire_image_dimension(dimension: ImageDimension) -> FrameImageDimension {
    match dimension {
        ImageDimension::Cells(value) => FrameImageDimension::Cells(value),
        ImageDimension::Pixels(value) => FrameImageDimension::Pixels(value),
        ImageDimension::Percent(value) => FrameImageDimension::Percent(value),
        ImageDimension::Auto => FrameImageDimension::Auto,
    }
}

/// One Sixel background rule in wire form.
fn wire_sixel_background(background: SixelBackground) -> FrameSixelBackground {
    match background {
        SixelBackground::Terminal => FrameSixelBackground::Terminal,
        SixelBackground::Preserve => FrameSixelBackground::Preserve,
    }
}

/// Display metadata in wire form.
fn wire_image_display(display: &ImageDisplay) -> FrameImageDisplay {
    FrameImageDisplay {
        width: display.width.map(wire_image_dimension),
        height: display.height.map(wire_image_dimension),
        preserve_aspect_ratio: display.preserve_aspect_ratio,
        sixel_background: display.sixel_background.map(wire_sixel_background),
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

/// The pane's visible cells, row by row, each row folded into runs.
fn wire_window(view: &GridView) -> FrameWindow {
    let (rows, cols) = view.grid.dimensions();
    FrameWindow {
        cols,
        rows: (0..rows)
            .map(|row| wire_row(&view.grid, row, cols))
            .collect(),
        view_offset: view.view_offset,
    }
}

/// Row `row`, always exactly `cols` cells wide, folded into runs of equal
/// neighbours, with how the row ends its logical line. A cell the grid does not
/// hold travels as a blank, so the row keeps its width.
fn wire_row(grid: &Grid, row: u16, cols: u16) -> FrameRow {
    let row_cells = grid
        .rows()
        .get(row as usize)
        .map(Vec::as_slice)
        .unwrap_or_default();
    debug_assert_eq!(
        row_cells.len(),
        cols as usize,
        "every grid row is the grid's width"
    );
    let blank = Cell::blank();
    let mut runs: Vec<FrameRun> = Vec::new();
    let mut source_run: Option<(&Cell, u16)> = None;
    for cell in (0..cols).map(|col| row_cells.get(col as usize).unwrap_or(&blank)) {
        match source_run {
            Some((source, count))
                if count < u16::MAX
                    && source.ch() == cell.ch()
                    && source.combining() == cell.combining()
                    && source.width() == cell.width()
                    && source.style() == cell.style() =>
            {
                source_run = Some((source, count + 1));
            }
            Some((source, count)) => {
                runs.push(FrameRun {
                    count,
                    cell: wire_cell(source),
                });
                source_run = Some((cell, 1));
            }
            None => source_run = Some((cell, 1)),
        }
    }
    if let Some((source, count)) = source_run {
        runs.push(FrameRun {
            count,
            cell: wire_cell(source),
        });
    }
    FrameRow {
        runs,
        end: wire_row_end(grid.row_end(row)),
    }
}

/// The wire form of one row's line-continuation state.
fn wire_row_end(end: RowEnd) -> FrameRowEnd {
    match end {
        RowEnd::Hard => FrameRowEnd::Hard,
        RowEnd::Soft => FrameRowEnd::Soft,
        RowEnd::SoftWide => FrameRowEnd::SoftWide,
    }
}

/// One cell: its character, the rest of its grapheme cluster, its display
/// width, and its style.
fn wire_cell(cell: &Cell) -> FrameCell {
    FrameCell {
        ch: cell.ch(),
        combining: cell.combining().to_vec(),
        width: cell.width(),
        style: wire_style(cell.style()),
    }
}

/// One cell's colors and text attributes.
fn wire_style(style: Style) -> FrameStyle {
    let attrs = style.attrs();
    FrameStyle {
        fg: wire_color(style.fg()),
        bg: wire_color(style.bg()),
        underline_color: style.underline_color().map(wire_color),
        attrs: FrameAttrs {
            bold: attrs.bold(),
            italic: attrs.italic(),
            reverse: attrs.reverse(),
            faint: attrs.faint(),
            blink: attrs.blink(),
            conceal: attrs.conceal(),
            strike: attrs.strike(),
            overline: attrs.overline(),
            underline: wire_underline(attrs.underline()),
        },
    }
}

/// One foreground, background or underline color.
fn wire_color(color: Color) -> FrameColor {
    match color {
        Color::Default => FrameColor::Default,
        Color::Indexed(index) => FrameColor::Indexed(index),
        Color::Rgb(red, green, blue) => FrameColor::Rgb(red, green, blue),
    }
}

/// One cell's underline style.
fn wire_underline(underline: UnderlineStyle) -> FrameUnderline {
    match underline {
        UnderlineStyle::None => FrameUnderline::None,
        UnderlineStyle::Single => FrameUnderline::Single,
        UnderlineStyle::Double => FrameUnderline::Double,
        UnderlineStyle::Curly => FrameUnderline::Curly,
        UnderlineStyle::Dotted => FrameUnderline::Dotted,
        UnderlineStyle::Dashed => FrameUnderline::Dashed,
    }
}

/// The shape a pane asked its cursor to be drawn as.
fn wire_cursor_shape(shape: CursorShape) -> FrameCursorShape {
    match shape {
        CursorShape::Block => FrameCursorShape::Block,
        CursorShape::Underline => FrameCursorShape::Underline,
        CursorShape::Bar => FrameCursorShape::Bar,
    }
}

#[cfg(test)]
mod tests;
