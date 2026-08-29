//! The frame reader: turning the [`PaintedFrame`](koshi_ipc::frame::PaintedFrame)
//! a session sends into the
//! [`RenderSnapshot`](koshi_renderer::snapshot::RenderSnapshot) this process
//! paints.
//!
//! [`to_snapshot`] is the exact inverse of
//! [`wire_frame`](koshi_runtime::runtime::frame::wire_frame). The session, tab,
//! slot, tab-bar and client parts already hold shared types, so they copy
//! straight back. Each pane's cells are rebuilt: every
//! [`FrameRow`](koshi_ipc::frame::FrameRow) expands its runs back into cells,
//! each cell becomes a [`Cell`](koshi_terminal::grid::state::Cell), and the rows
//! become one [`Grid`](koshi_terminal::grid::state::Grid) behind an
//! [`Arc`](std::sync::Arc).
//!
//! A run travels once and expands back into as many cells as it stood for: a
//! blank 80-column row arrives as one run with `count: 80` and rebuilds into 80
//! blank cells.
//!
//! Plugin UI does not travel, so every frame read here carries the default
//! [`PluginUiSnapshot`](koshi_renderer::snapshot::PluginUiSnapshot).

use std::sync::Arc;

use koshi_ipc::frame::{
    FrameCell, FrameColor, FrameCursorShape, FramePane, FrameRow, FrameRowEnd, FrameSlot,
    FrameStyle, FrameTabMeta, FrameUnderline, FrameWindow, PaintedFrame,
};
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, PaneSlot, PaneSnapshot, PluginUiSnapshot,
    RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta, TabSnapshot,
};
use koshi_terminal::grid::state::{Cell, Grid, RowEnd};
use koshi_terminal::state::CursorShape;
use koshi_terminal::style::{Color, Style, UnderlineStyle};

#[cfg(test)]
mod tests;

/// Turn one frame read off the event stream into the snapshot the renderer
/// draws.
#[must_use]
pub fn to_snapshot(frame: &PaintedFrame) -> RenderSnapshot {
    let tab = &frame.session.active_tab;
    RenderSnapshot {
        session: SessionSnapshot {
            id: frame.session.id,
            name: frame.session.name.clone(),
            active_tab: TabSnapshot {
                id: tab.id,
                name: tab.name.clone(),
                layout_solved: tab.slots.iter().map(to_slot).collect(),
                effective_size: tab.effective_size,
                stack_headers: tab.stack_headers.clone(),
                layout_mode: tab.layout_mode,
                all_suppressed: tab.all_suppressed,
                gap: tab.gap,
            },
            tabs_metadata: frame.session.tabs.iter().map(to_tab_meta).collect(),
        },
        panes: frame.panes.iter().map(to_pane).collect(),
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

/// One tab-bar entry, as the renderer reads it.
fn to_tab_meta(meta: &FrameTabMeta) -> TabMeta {
    TabMeta {
        id: meta.id,
        name: meta.name.clone(),
        index: meta.index,
        active: meta.active,
    }
}

/// One pane's content, as the renderer reads it. A pane that sent no window has
/// no grid.
fn to_pane(pane: &FramePane) -> PaneSnapshot {
    PaneSnapshot {
        id: pane.id,
        title: pane.title.clone(),
        cursor: CursorSnapshot {
            row: pane.cursor.row,
            col: pane.cursor.col,
            visible: pane.cursor.visible,
            blink: pane.cursor.blink,
            shape: pane.cursor.shape.as_ref().map(to_cursor_shape),
        },
        grid_view: pane.window.as_ref().map(to_grid_view),
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
