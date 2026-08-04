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
//! Rows are run-length encoded by [`FrameRow::from_cells`]: a 1×3 row holding a
//! styled `e` then two default blanks travels as two runs — `count: 1` for the
//! `e`, `count: 2` for the blanks.
//!
//! Plugin UI does not travel.
//! [`build_snapshot`](crate::server::Server::build_snapshot) always sets the
//! default, and [`PaintedFrame`] has no slot for it.

use koshi_ipc::frame::{
    FrameAttrs, FrameCell, FrameClient, FrameColor, FrameCursor, FrameCursorShape, FramePane,
    FrameRow, FrameRowEnd, FrameScrollback, FrameSelection, FrameSession, FrameSlot, FrameStyle,
    FrameTab, FrameTabMeta, FrameUnderline, FrameWindow, PaintedFrame,
};
use koshi_renderer::snapshot::{GridView, PaneSlot, PaneSnapshot, RenderSnapshot, TabMeta};
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

/// One solved pane placement, as it travels.
fn wire_slot(slot: &PaneSlot) -> FrameSlot {
    FrameSlot {
        pane_id: slot.pane_id,
        rect: slot.rect,
        inner_rect: slot.inner_rect,
        kind: slot.kind.clone(),
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
    debug_assert_eq!(
        grid.rows().get(row as usize).map_or(0, Vec::len),
        cols as usize,
        "every grid row is the grid's width"
    );
    let blank = Cell::blank();
    let cells: Vec<FrameCell> = (0..cols)
        .map(|col| wire_cell(grid.cell(row, col).unwrap_or(&blank)))
        .collect();
    FrameRow::from_cells(&cells, wire_row_end(grid.row_end(row)))
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
