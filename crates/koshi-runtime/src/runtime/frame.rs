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

use koshi_ipc::frame::{
    FrameAttrs, FrameCell, FrameClient, FrameColor, FrameCursor, FrameCursorShape,
    FrameGraphicsProtocol, FrameImageAction, FrameImageDimension, FrameImageDisplay,
    FrameImagePlacement, FrameImageRecordHeader, FrameImageTransfer, FramePane, FrameRow,
    FrameRowEnd, FrameRun, FrameScrollback, FrameSelection, FrameSession, FrameSixelBackground,
    FrameSlot, FrameStyle, FrameTab, FrameTabMeta, FrameUnderline, FrameWindow, PaintedFrame,
    MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT,
};
use koshi_renderer::snapshot::{
    GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot, RenderSnapshot, TabMeta,
};
use koshi_terminal::graphics::{
    GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord, SixelBackground,
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
pub fn wire_frame(render_snapshot: &RenderSnapshot) -> PaintedFrame {
    let mut next_image_content_id = 1u64;
    wire_frame_with_content_ids(render_snapshot, |_, _| {
        let image_content_id = next_image_content_id;
        next_image_content_id = next_image_content_id.saturating_add(1);
        image_content_id
    })
}

/// Turn one painted frame into wire form with connection-selected image ids.
#[must_use]
pub(crate) fn wire_frame_with_content_ids(
    render_snapshot: &RenderSnapshot,
    mut assign_image_content_id: impl FnMut(koshi_core::ids::PaneId, &ImagePlacementSnapshot) -> u64,
) -> PaintedFrame {
    let session_snapshot = &render_snapshot.session_snapshot;
    let active_tab_snapshot = &session_snapshot.active_tab_snapshot;
    let client_snapshot = &render_snapshot.client_snapshot;
    PaintedFrame {
        session_snapshot: FrameSession {
            session_id: session_snapshot.session_id,
            session_name: session_snapshot.session_name.clone(),
            active_tab_snapshot: FrameTab {
                tab_id: active_tab_snapshot.tab_id,
                tab_name: active_tab_snapshot.tab_name.clone(),
                pane_slots: active_tab_snapshot
                    .pane_slots
                    .iter()
                    .map(wire_slot)
                    .collect(),
                effective_cell_size: active_tab_snapshot.effective_cell_size,
                stack_headers: active_tab_snapshot.stack_headers.clone(),
                layout_mode: active_tab_snapshot.layout_mode,
                is_every_pane_suppressed: active_tab_snapshot.are_all_panes_suppressed,
                gap_cell_count: active_tab_snapshot.gap_cell_count,
            },
            tab_snapshots: session_snapshot
                .tabs_metadata
                .iter()
                .map(wire_tab_meta)
                .collect(),
        },
        pane_snapshots: render_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| wire_pane(pane_snapshot, &mut assign_image_content_id))
            .collect(),
        client_snapshot: FrameClient {
            client_id: client_snapshot.client_id,
            viewport_size: client_snapshot.viewport_size,
            active_tab_id: client_snapshot.active_tab_id,
            focused_pane_id: client_snapshot.focused_pane_id,
            lock_mode: client_snapshot.lock_mode,
            is_mouse_selection_enabled: client_snapshot.is_mouse_selection_enabled,
        },
    }
}

/// Build the metadata sent before one image record's RGBA chunks.
#[must_use]
pub(crate) fn wire_image_transfer(
    image_content_id: u64,
    image_record: &ImageRecord,
) -> FrameImageTransfer {
    FrameImageTransfer {
        image_content_id,
        image_record: FrameImageRecordHeader {
            protocol: wire_graphics_protocol(image_record.protocol),
            pixel_width: image_record.image.pixel_width,
            pixel_height: image_record.image.pixel_height,
            image_action: wire_image_action(image_record.action),
            display: wire_image_display(&image_record.display),
            anchor_cell: image_record.anchor,
        },
        image_byte_count: u64::try_from(image_record.image.rgba_bytes.len())
            .expect("an image byte count fits in a frame transfer"),
    }
}

/// Return bounded RGBA chunks for one connection-local image record.
pub(crate) fn wire_image_chunk_sources(
    image_record: &ImageRecord,
) -> impl Iterator<Item = (u64, bool, &[u8])> {
    let image_byte_count = image_record.image.rgba_bytes.len();
    image_record
        .image
        .rgba_bytes
        .chunks(MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
        .enumerate()
        .map(move |(chunk_index, bytes)| {
            let chunk_index =
                u64::try_from(chunk_index).expect("an image chunk index fits in a transfer offset");
            let chunk_size = u64::try_from(MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
                .expect("the image chunk size fits in a transfer offset");
            let byte_offset = chunk_index * chunk_size;
            let is_last_chunk = byte_offset.checked_add(
                u64::try_from(bytes.len()).expect("an image chunk length fits in an offset"),
            ) == Some(
                u64::try_from(image_byte_count).expect("an image byte count fits in a transfer"),
            );
            (byte_offset, is_last_chunk, bytes)
        })
}

/// One solved pane placement, as it travels.
fn wire_slot(pane_slot: &PaneSlot) -> FrameSlot {
    FrameSlot {
        pane_id: pane_slot.pane_id,
        outer_rect: pane_slot.outer_rect,
        content_rect: pane_slot.content_rect,
        pane_kind: pane_slot.pane_kind,
        is_visible: pane_slot.is_visible,
        is_suppressed: pane_slot.is_suppressed,
        is_dead: pane_slot.is_dead,
    }
}

/// One tab-bar entry, as it travels.
fn wire_tab_meta(tab_metadata: &TabMeta) -> FrameTabMeta {
    FrameTabMeta {
        tab_id: tab_metadata.tab_id,
        tab_name: tab_metadata.tab_name.clone(),
        tab_index: tab_metadata.tab_index,
        is_active: tab_metadata.is_active,
    }
}

/// One pane's content, as it travels. A pane with no terminal content sends no
/// window.
fn wire_pane(
    pane_snapshot: &PaneSnapshot,
    assign_image_content_id: &mut impl FnMut(koshi_core::ids::PaneId, &ImagePlacementSnapshot) -> u64,
) -> FramePane {
    FramePane {
        pane_id: pane_snapshot.pane_id,
        pane_title: pane_snapshot.pane_title.clone(),
        cursor_snapshot: FrameCursor {
            row_index: pane_snapshot.cursor_snapshot.row_index,
            column_index: pane_snapshot.cursor_snapshot.column_index,
            is_visible: pane_snapshot.cursor_snapshot.is_visible,
            is_blinking: pane_snapshot.cursor_snapshot.is_blinking,
            shape: pane_snapshot.cursor_snapshot.shape.map(wire_cursor_shape),
        },
        terminal_window: pane_snapshot.terminal_grid_view.as_ref().map(wire_window),
        image_placement_snapshots: pane_snapshot
            .image_placement_snapshots
            .iter()
            .map(|image_placement_snapshot| {
                wire_image_placement(
                    pane_snapshot.pane_id,
                    image_placement_snapshot,
                    assign_image_content_id,
                )
            })
            .collect(),
        is_reverse_video: pane_snapshot.is_reverse_video,
        mouse_tracking: pane_snapshot.mouse_tracking,
        is_alt_scroll_enabled: pane_snapshot.is_alternate_scroll_enabled,
        is_on_alt_screen: pane_snapshot.is_on_alternate_screen,
        view_top_row_index: pane_snapshot.view_top_row_index,
        selection_spans: pane_snapshot
            .selection_spans
            .as_ref()
            .map(|selection_spans| FrameSelection {
                row_spans: selection_spans.row_spans.clone(),
            }),
        has_selection: pane_snapshot.has_selection,
        scrollback_meta: FrameScrollback {
            is_truncated: pane_snapshot.scrollback_meta.is_truncated,
            retained_line_count: pane_snapshot.scrollback_meta.retained_line_count,
        },
    }
}

/// One validated image placement, as it travels with its pane.
fn wire_image_placement(
    pane_id: koshi_core::ids::PaneId,
    image_placement_snapshot: &ImagePlacementSnapshot,
    assign_image_content_id: &mut impl FnMut(koshi_core::ids::PaneId, &ImagePlacementSnapshot) -> u64,
) -> FrameImagePlacement {
    let (row_count, column_count) = image_placement_snapshot.get_cell_dimensions();
    FrameImagePlacement {
        placement_id: image_placement_snapshot.get_placement_id(),
        cell_geometry: Some(image_placement_snapshot.get_cell_geometry()),
        image_record: image_placement_snapshot
            .get_image_record()
            .map(|image_record| wire_image_transfer(1, image_record).image_record),
        image_content_id: assign_image_content_id(pane_id, image_placement_snapshot),
        is_available: image_placement_snapshot.get_image_record().is_some(),
        anchor_cell: image_placement_snapshot.get_anchor_cell(),
        column_count,
        row_count,
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
        ImageDimension::Cells(cell_count) => FrameImageDimension::Cells(cell_count),
        ImageDimension::Pixels(pixel_count) => FrameImageDimension::Pixels(pixel_count),
        ImageDimension::Percent(percent) => FrameImageDimension::Percent(percent),
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
        response_suppression_level: display.response_suppression_level,
        requested_width: display.requested_width.map(wire_image_dimension),
        requested_height: display.requested_height.map(wire_image_dimension),
        is_aspect_ratio_preserved: display.is_aspect_ratio_preserved,
        sixel_background: display.sixel_background.map(wire_sixel_background),
        image_id: display.image_id,
        image_number: display.image_number,
        placement_id: display.placement_id,
        usage_hints: display.usage_hints,
        is_unicode_placeholder: display.is_unicode_placeholder,
        z_index: display.z_index,
        relative_image_id: display.relative_image_id,
        relative_placement_id: display.relative_placement_id,
        relative_column_offset: display.relative_column_offset,
        relative_row_offset: display.relative_row_offset,
        requested_column_count: display.requested_column_count,
        requested_row_count: display.requested_row_count,
        source_pixel_offset_x: display.source_pixel_offset_x,
        source_pixel_offset_y: display.source_pixel_offset_y,
        cell_pixel_offset_x: display.cell_pixel_offset_x,
        cell_pixel_offset_y: display.cell_pixel_offset_y,
        should_move_cursor: display.should_move_cursor,
    }
}

/// The pane's visible cells, row by row, each row folded into runs.
fn wire_window(view: &GridView) -> FrameWindow {
    let (row_count, column_count) = view.grid.get_grid_dimensions();
    FrameWindow {
        column_count,
        row_snapshots: (0..row_count)
            .map(|row_index| wire_row(&view.grid, row_index, column_count))
            .collect(),
        view_row_offset: view.view_row_offset,
    }
}

/// Row `row_index`, always exactly `column_count` cells wide, folded into runs of equal
/// neighbours, with how the row ends its logical line. A cell the grid does not
/// hold travels as a blank, so the row keeps its width.
fn wire_row(terminal_grid: &Grid, row_index: u16, column_count: u16) -> FrameRow {
    let row_cells = terminal_grid
        .list_rows()
        .get(row_index as usize)
        .map(Vec::as_slice)
        .unwrap_or_default();
    debug_assert_eq!(
        row_cells.len(),
        column_count as usize,
        "every grid row is the grid's width"
    );
    let blank_cell = Cell::blank();
    let mut frame_runs: Vec<FrameRun> = Vec::new();
    let mut source_frame_run: Option<(&Cell, u16)> = None;
    for cell in (0..column_count)
        .map(|column_index| row_cells.get(column_index as usize).unwrap_or(&blank_cell))
    {
        match source_frame_run {
            Some((source_cell, run_cell_count))
                if run_cell_count < u16::MAX
                    && source_cell.get_character() == cell.get_character()
                    && source_cell.list_combining_characters()
                        == cell.list_combining_characters()
                    && source_cell.get_display_width() == cell.get_display_width()
                    && source_cell.get_style() == cell.get_style() =>
            {
                source_frame_run = Some((source_cell, run_cell_count + 1));
            }
            Some((source_cell, run_cell_count)) => {
                frame_runs.push(FrameRun {
                    repeat_count: run_cell_count,
                    cell: wire_cell(source_cell),
                });
                source_frame_run = Some((cell, 1));
            }
            None => source_frame_run = Some((cell, 1)),
        }
    }
    if let Some((source_cell, run_cell_count)) = source_frame_run {
        frame_runs.push(FrameRun {
            repeat_count: run_cell_count,
            cell: wire_cell(source_cell),
        });
    }
    FrameRow {
        cell_runs: frame_runs,
        row_end: wire_row_end(terminal_grid.get_row_end(row_index)),
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
fn wire_cell(terminal_cell: &Cell) -> FrameCell {
    FrameCell {
        character: terminal_cell.get_character(),
        combining_characters: if terminal_cell.has_image_placeholder() {
            Vec::new()
        } else {
            terminal_cell.list_combining_characters().to_vec()
        },
        cell_width: terminal_cell.get_display_width(),
        style: wire_style(terminal_cell.get_style()),
    }
}

/// One cell's colors and text attributes.
fn wire_style(style: Style) -> FrameStyle {
    let text_attributes = style.get_attributes();
    FrameStyle {
        foreground_color: wire_color(style.get_foreground_color()),
        background_color: wire_color(style.get_background_color()),
        underline_color: style.get_underline_color().map(wire_color),
        text_attributes: FrameAttrs {
            is_bold: text_attributes.is_bold(),
            is_italic: text_attributes.is_italic(),
            is_reverse: text_attributes.is_reverse(),
            is_faint: text_attributes.is_faint(),
            is_blinking: text_attributes.is_blinking(),
            is_concealed: text_attributes.is_concealed(),
            is_struck_through: text_attributes.is_strikethrough(),
            is_overlined: text_attributes.is_overlined(),
            underline_style: wire_underline(text_attributes.get_underline_style()),
        },
    }
}

/// One foreground, background or underline color.
fn wire_color(color: Color) -> FrameColor {
    match color {
        Color::Default => FrameColor::Default,
        Color::Indexed(color_index) => FrameColor::Indexed(color_index),
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
