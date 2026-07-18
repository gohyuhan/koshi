//! Stock (plugin-free) frame composition.
//!
//! [`render_frame`] paints one [`RenderSnapshot`] into a ratatui [`Buffer`] as
//! three fixed zones: a **tabline** on the top row (session name and tab list on
//! the left, a right-aligned status section — scroll position and mode tag), the
//! **pane area** in the middle (a bordered box per visible pane, the focused
//! pane's border highlighted), and the **keybinding hint bar** on the bottom row
//! — a koshi-owned row painted by [`crate::statusline_hints`] from the
//! snapshot's per-mode keybinding data.
//!
//! Collapsed members of a stacked pane group are drawn as one-row title strips
//! in the pane area, and each visible terminal pane's cells are painted into its
//! content rect. The focused pane's cursor cell is reported separately by
//! [`cursor_position`] for the caller to place the terminal's hardware cursor,
//! since the buffer itself carries no cursor. When the active tab has no room
//! for any pane, a centered "terminal too small" overlay replaces the pane
//! render for that frame. When the client's viewport is larger than the size
//! the layout was solved for, the whole frame is centered and the surrounding
//! margin is filled with a dim letterbox. Plugin-contributed segments (empty
//! here) are injected once the plugin host lands.

pub mod state;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::PaneId;
use koshi_core::lock::LockMode;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::{Color as CellColor, Style as CellStyle, UnderlineStyle};

use crate::snapshot::{ClientSnapshot, CursorStyle, PaneSnapshot, RenderSnapshot, SelectionSpans};
use crate::statusline_hints::draw_hint_bar;
use crate::theme::Theme;

/// Paint `snapshot` into `buf` over `area` (the client's full viewport).
///
/// Blanks `area` first so a buffer reused across frames shows no stale cells,
/// then draws the pane borders, each visible pane's terminal cells, and the
/// collapsed stack-member strips, then the tabline over the top row and the
/// keybinding hint bar over the bottom row (skipped when the content area is a
/// single row — the tabline owns it). When the active tab has no room for any
/// pane (`all_suppressed`), draws only a centered too-small overlay and
/// returns, skipping the panes and both chrome rows. Does nothing for a
/// zero-size area.
pub fn render_frame(snapshot: &RenderSnapshot, area: RatatuiRect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // A per-client snapshot solves the tab that client is viewing into
    // `session.active_tab`, so its id must match the client's viewed tab.
    debug_assert_eq!(
        snapshot.client.active_tab, snapshot.session.active_tab.id,
        "snapshot builder must solve the client's active tab into session.active_tab"
    );

    // Blank the viewport first: ratatui reuses the previous frame's buffer, and
    // this clears stale cells in the tabline gap, the reserved hint row, and any
    // pane interior not painted this frame.
    Clear.render(area, buf);

    // No room for any pane: the whole frame becomes the too-small overlay.
    if snapshot.session.active_tab.all_suppressed {
        draw_too_small_overlay(area, buf);
        return;
    }

    // Center the solved layout inside this client's viewport. The layout was
    // solved for the tab's effective (smallest-client) size, so a larger client
    // has margin: `content` is that effective-sized rect centered in `area`, and
    // `offset` shifts each effective-space layout rect into it.
    let content = content_rect(area, snapshot.session.active_tab.effective_size);
    let offset = Point {
        x: content.x,
        y: content.y,
    };

    draw_panes(snapshot, offset, buf);
    draw_pane_contents(snapshot, offset, buf);
    draw_stack_headers(snapshot, offset, buf);

    // Fill multi-client margins before chrome; the tabline and hint bar own
    // the outer rows and must remain visible over the letterbox.
    draw_letterbox(area, content, &snapshot.theme, buf);

    let tabline = RatatuiRect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    draw_tabline(snapshot, tabline, buf);

    if area.height >= 2 {
        let hint_bar = RatatuiRect {
            x: area.x,
            y: area.bottom() - 1,
            width: area.width,
            height: 1,
        };
        draw_hint_bar(snapshot, hint_bar, buf);
    }
}

/// The buffer cell where the client's focused pane wants the hardware cursor, or
/// `None` when no cursor should show this frame.
///
/// Companion to [`render_frame`]: the buffer carries no cursor, so the caller
/// reads this alongside the paint — passing the same `area` — and places the
/// terminal's cursor at the returned [`Position`] (or hides it on `None`). The
/// position is the focused pane's cursor cell — its row and column within the
/// content area, shifted by the same letterbox offset `render_frame` centers the
/// layout with and clamped inside the area — in the same absolute buffer
/// coordinates the panes are drawn in.
///
/// Returns `None` when the client has no focused pane; that pane has no placed
/// slot or no content snapshot; it is not visible or has no content area
/// (suppressed, hidden, or a collapsed stack member); it has no terminal grid
/// (a plugin pane, or a slot showing nothing this frame); its view is scrolled
/// back into history (no hardware cursor is placed while scrolled); or the
/// application has hidden its cursor.
pub fn cursor_position(snapshot: &RenderSnapshot, area: RatatuiRect) -> Option<Position> {
    let focused = snapshot.client.focused_pane?;

    let slot = snapshot
        .session
        .active_tab
        .layout_solved
        .iter()
        .find(|slot| slot.pane_id == focused)?;
    if !slot.visible {
        return None;
    }
    let inner = slot.inner_rect?;

    let pane = find_pane(snapshot, focused)?;
    // A plugin pane (no grid) gets a cursor only when the plugin asks for one.
    let view = pane.grid_view.as_ref()?;
    // A view scrolled back into history shows no hardware cursor: the cursor
    // belongs to the live tail the view has scrolled away from.
    if view.view_offset > 0 {
        return None;
    }
    if !pane.cursor.visible {
        return None;
    }

    // Map the pane-local cursor (col/row counted from the content area's own
    // top-left) to a screen cell. `inner` is the content rect in effective-layout
    // space; `place` shifts it by the same letterbox offset `render_frame` centers
    // with, so the cursor lands on the cell the panes drew. Adding the local
    // col/row to the placed origin gives the screen position; clamp inside the
    // rect since a dead pane keeps a frozen cursor while its content rect can
    // shrink, so the raw sum may fall past the edge.
    let content = content_rect(area, snapshot.session.active_tab.effective_size);
    let inner = place(
        inner,
        Point {
            x: content.x,
            y: content.y,
        },
    );
    let x = (inner.x + pane.cursor.col).min(inner.right().saturating_sub(1));
    let y = (inner.y + pane.cursor.row).min(inner.bottom().saturating_sub(1));
    Some(Position::new(x, y))
}

/// How the outer terminal's cursor should look this frame:
/// [`Shaped`](CursorStyle::Shaped) with what the focused pane asked for via
/// DECSCUSR, or [`UserDefault`](CursorStyle::UserDefault) when it asked for
/// nothing — a plain shell never sends DECSCUSR, and its cursor is whatever the
/// user configured, not a block koshi invented.
///
/// `None` — meaning "leave the cursor as it is" — only when there is no focused
/// terminal pane to speak for it: no focused pane at all, or a plugin pane,
/// which has no terminal and so no opinion.
///
/// Companion to [`cursor_position`], which says *where* the cursor goes; this
/// says what it looks like once it is there. The caller applies it to the outer
/// terminal (crossterm's `SetCursorStyle`), which is what makes vim's
/// insert-mode bar show as a bar instead of a block.
///
/// Deliberately not gated on the cursor being visible or the view being scrolled
/// back: a cursor that is not drawn has no look to get wrong, and re-deriving
/// [`cursor_position`]'s guard chain here would be a second copy of it to keep in
/// step.
#[must_use]
pub fn cursor_style(snapshot: &RenderSnapshot) -> Option<CursorStyle> {
    let pane = find_pane(snapshot, snapshot.client.focused_pane?)?;
    pane.grid_view.as_ref()?;
    let style = match pane.cursor.shape {
        Some(shape) => CursorStyle::Shaped {
            shape,
            blink: pane.cursor.blink,
        },
        None => CursorStyle::UserDefault,
    };
    Some(style)
}

/// Find the [`PaneSnapshot`] with the given id in this frame.
fn find_pane(snapshot: &RenderSnapshot, id: PaneId) -> Option<&PaneSnapshot> {
    snapshot.panes.iter().find(|pane| pane.id == id)
}

/// Draw a bordered box for every visible pane in the active tab, coloring the
/// focused pane's border (and an unfocused hovered pane's), writing the pane's
/// resolved title into its top border line, and — when the pane is scrolled
/// back — its scroll position into its bottom border. `offset` shifts each pane
/// into the centered content rect.
fn draw_panes(snapshot: &RenderSnapshot, offset: Point, buf: &mut Buffer) {
    let focused = snapshot.client.focused_pane;
    let hovered = snapshot.client.hovered_pane;
    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        // Focus keeps its own color; the hover color marks only an unfocused
        // pane the wheel would scroll, so the focused pane never turns purple.
        let style = if Some(slot.pane_id) == focused {
            border_focused_style(&snapshot.theme)
        } else if Some(slot.pane_id) == hovered {
            border_hover_style(&snapshot.theme)
        } else {
            border_unfocused_style(&snapshot.theme)
        };
        let rect = place(slot.rect, offset);
        Block::new()
            .borders(Borders::ALL)
            .border_style(style)
            .render(rect, buf);

        let pane = find_pane(snapshot, slot.pane_id);

        // The pane's title sits in the top border, zellij-style: ` title `
        // over the line, clipped so the corner glyphs always survive.
        if let Some(title) = pane.and_then(|pane| pane.title.as_deref()) {
            if !title.is_empty() && rect.width > 4 {
                let line = Line::from(Span::styled(format!(" {title} "), style));
                set_line_clipped(buf, rect.x + 2, rect.y, &line, rect.width - 4);
            }
        }

        // When this pane is scrolled back, its position sits in the bottom
        // border, right-aligned: ` up/total `. A pane at the live tail shows
        // nothing. Each pane carries its own offset, so several can show at once.
        if let Some((up, total)) = pane.and_then(pane_scroll) {
            let text = format!(" {up}/{total} ");
            let width = text.len() as u16;
            if rect.width >= width + 2 {
                let line = Line::from(Span::styled(text, style));
                let x = rect.right() - 1 - width;
                set_line_clipped(buf, x, rect.bottom() - 1, &line, width);
            }
        }
    }
}

/// Draw the "terminal too small" overlay: one centered, bold line telling the
/// user to enlarge the window, shown when the tab has no room for any pane.
///
/// Centered on the middle row of `area` and horizontally within it. A message
/// wider than the viewport is clipped to the right edge, so nothing is written
/// out of bounds on a very narrow screen.
fn draw_too_small_overlay(area: RatatuiRect, buf: &mut Buffer) {
    let message = Line::from(Span::styled(
        "Terminal too small — enlarge window",
        too_small_style(),
    ));
    let width = message.width() as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height / 2;
    set_line_clipped(buf, x, y, &message, area.right().saturating_sub(x));
}

/// Paint each visible terminal pane's cells into its content rect.
///
/// For every visible pane slot that has a content rect and a terminal grid,
/// draws the grid into that rect. Plugin panes (no grid) and panes with no
/// content rect (suppressed, hidden, or a collapsed stack member) draw nothing.
/// `offset` shifts each content rect into the centered content area.
fn draw_pane_contents(snapshot: &RenderSnapshot, offset: Point, buf: &mut Buffer) {
    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        let Some(inner) = slot.inner_rect else {
            continue;
        };
        let Some(pane) = find_pane(snapshot, slot.pane_id) else {
            continue;
        };
        let Some(view) = &pane.grid_view else {
            continue;
        };
        draw_grid(
            &view.grid,
            place(inner, offset),
            pane.reverse_video,
            pane.selection.as_ref(),
            buf,
        );
    }
}

/// Paint one terminal `grid` into `area`, one buffer cell per grid cell.
///
/// Each grid cell is placed at its own column, so on-screen column positions
/// always match grid column positions. The continuation half of a
/// wide glyph (width 0) is skipped — the wide base already covers it. A wide
/// glyph whose second half falls outside the content area is replaced by a blank
/// so no half-glyph bleeds past the edge. `reverse_video` (DECSCNM) toggles
/// reverse for every cell. `area` is clipped to the buffer so an oversized rect
/// cannot index out of bounds.
///
/// A highlighted cell (`selection`) is drawn in reverse, the way a terminal has
/// always shown selected text. It combines with the cell's own reverse and with
/// `reverse_video` by exclusive-or, so highlighting text that is already reverse
/// swaps it back and the highlight still reads against its surroundings.
fn draw_grid(
    grid: &Grid,
    area: RatatuiRect,
    reverse_video: bool,
    selection: Option<&SelectionSpans>,
    buf: &mut Buffer,
) {
    let area = area.intersection(buf.area);
    let (grid_rows, grid_cols) = grid.dimensions();
    let rows = grid_rows.min(area.height);
    let cols = grid_cols.min(area.width);
    for row in 0..rows {
        // Once per row, not once per cell: a highlight is a column range on a
        // row, so the row's range is looked up before walking its cells.
        let span = selection.and_then(|spans| spans.row_span(row));
        for col in 0..cols {
            let Some(cell) = grid.cell(row, col) else {
                continue;
            };
            let width = cell.width();
            if width == 0 {
                continue;
            }
            let x = area.x + col;
            let y = area.y + row;
            let selected = span.is_some_and(|(start, end)| col >= start && col <= end);
            let style = cell_style(cell.style(), reverse_video ^ selected);
            if width >= 2 && col + 1 >= cols {
                buf[(x, y)].set_char(' ').set_style(style);
                continue;
            }
            if cell.combining().is_empty() {
                buf[(x, y)].set_char(cell.ch()).set_style(style);
            } else {
                buf[(x, y)].set_symbol(&cell_symbol(cell)).set_style(style);
            }
        }
    }
}

/// The glyph a cell draws: its base character followed by any combining marks
/// and joined code points, as one string.
fn cell_symbol(cell: &Cell) -> String {
    let mut symbol = String::with_capacity(1 + cell.combining().len());
    symbol.push(cell.ch());
    symbol.extend(cell.combining().iter().copied());
    symbol
}

/// Map a terminal cell style to a ratatui [`Style`].
///
/// Colors map directly, the terminal default becoming ratatui's reset. Each
/// boolean attribute maps to its modifier; every underline variant collapses to
/// a single underline, and overline and underline color have no ratatui modifier
/// and are not drawn. `reverse_video` (DECSCNM) combines with the cell's own
/// reverse by exclusive-or, so a screen-wide reverse cancels a cell already in
/// reverse.
fn cell_style(style: CellStyle, reverse_video: bool) -> Style {
    let attrs = style.attrs();
    let mut modifier = Modifier::empty();
    if attrs.bold() {
        modifier |= Modifier::BOLD;
    }
    if attrs.faint() {
        modifier |= Modifier::DIM;
    }
    if attrs.italic() {
        modifier |= Modifier::ITALIC;
    }
    if attrs.underline() != UnderlineStyle::None {
        modifier |= Modifier::UNDERLINED;
    }
    if attrs.blink() {
        modifier |= Modifier::SLOW_BLINK;
    }
    if attrs.conceal() {
        modifier |= Modifier::HIDDEN;
    }
    if attrs.strike() {
        modifier |= Modifier::CROSSED_OUT;
    }
    if attrs.reverse() ^ reverse_video {
        modifier |= Modifier::REVERSED;
    }
    Style::default()
        .fg(cell_color(style.fg()))
        .bg(cell_color(style.bg()))
        .add_modifier(modifier)
}

/// Map a terminal color to a ratatui [`Color`]; the terminal default becomes
/// ratatui's reset (the outer terminal's own default).
fn cell_color(color: CellColor) -> Color {
    match color {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(index) => Color::Indexed(index),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Draw the one-row title strip for every collapsed stack member: a collapse
/// arrow and the pane title on the left, a `[position/total]` indicator
/// right-aligned, over a theme-filled row that marks the strip as
/// koshi-owned. `offset` shifts each strip into the centered content rect.
fn draw_stack_headers(snapshot: &RenderSnapshot, offset: Point, buf: &mut Buffer) {
    let style = stack_header_style(&snapshot.theme);
    for header in &snapshot.session.active_tab.stack_headers {
        let rect = place(header.rect, offset);
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        // Fill the whole row first so the gap between the title and the
        // indicator carries the strip background too.
        buf.set_style(rect, style);

        let title = header_title(snapshot, header.pane);
        let left = Line::from(format!("▸ {title}"));
        set_line_clipped(buf, rect.x, rect.y, &left, rect.width);

        // Right-align `[N/total]`, clamped inside the strip so a stack narrower
        // than the indicator never writes into a neighbouring pane.
        let indicator = Line::from(format!("[{}/{}]", header.position + 1, header.total));
        let width = indicator.width() as u16;
        let x = rect.right().saturating_sub(width).max(rect.x);
        set_line_clipped(buf, x, rect.y, &indicator, rect.right() - x);
    }
}

/// The title drawn on a stack member's header strip: the pane's terminal title,
/// or empty when the pane has none.
fn header_title(snapshot: &RenderSnapshot, pane: PaneId) -> String {
    find_pane(snapshot, pane)
        .and_then(|snap| snap.title.clone())
        .unwrap_or_default()
}

/// The mode indicator shown in the tabline: every active mode label joined with
/// ` · `, or `BASE` when the client is in plain mode with the mouse ungrabbed.
///
/// The labels compose from independent axes: the `lock_mode` layer contributes
/// at most one tag (nothing when `Normal`), and `mouse_select` adds `SELECT`.
/// So a locked client grabbing the mouse reads `LOCK · SELECT`, and a plain one
/// grabbing it reads `SELECT`.
fn mode_tags(client: &ClientSnapshot) -> String {
    let mut tags: Vec<&'static str> = Vec::new();
    if let Some(tag) = lock_mode_tag(client.lock_mode) {
        tags.push(tag);
    }
    if client.mouse_select {
        tags.push("SELECT");
    }
    if tags.is_empty() {
        "BASE".to_string()
    } else {
        tags.join(" · ")
    }
}

/// The tag for a non-plain lock mode, or `None` for `Normal` — which shows as
/// `BASE` only when no other mode is active.
fn lock_mode_tag(mode: LockMode) -> Option<&'static str> {
    match mode {
        LockMode::Normal => None,
        LockMode::Locked => Some("LOCK"),
        LockMode::Resize => Some("RESIZE"),
        LockMode::PaneMode => Some("PANE"),
        LockMode::TabMode => Some("TAB"),
        LockMode::ScrollMode => Some("SCROLL"),
    }
}

/// A pane's scroll position as `(lines scrolled up, retained lines)`, or `None`
/// when the pane is at the live tail (nothing to indicate).
fn pane_scroll(pane: &PaneSnapshot) -> Option<(usize, usize)> {
    let offset = pane.grid_view.as_ref().map_or(0, |view| view.view_offset);
    (offset > 0).then_some((offset, pane.scrollback.retained_lines))
}

/// Place an effective-space layout [`Rect`] onto the screen: convert its
/// koshi-core cell rect to a ratatui rect and shift its origin by `offset`, the
/// origin of the centered content rect. A zero offset (a client at the effective
/// size) leaves the rect where the solver put it.
fn place(rect: Rect, offset: Point) -> RatatuiRect {
    RatatuiRect {
        x: rect.origin.x + offset.x,
        y: rect.origin.y + offset.y,
        width: rect.size.cols,
        height: rect.size.rows,
    }
}

/// Draw a line, skipping it when its row lies outside the buffer.
///
/// [`Buffer::set_line`] clips a line horizontally but writes its row with no
/// vertical bound, so a row past the buffer's height panics. A resize can leave
/// the buffer shorter than the laid-out frame (its rows solved for a taller
/// size), which places chrome rows below the buffer; this guards that row.
fn set_line_clipped(buf: &mut Buffer, x: u16, y: u16, line: &Line<'_>, max_width: u16) {
    if y < buf.area.top() || y >= buf.area.bottom() {
        return;
    }
    buf.set_line(x, y, line, max_width);
}

/// The centered rect of the effective (solved) size within the client's `area`.
///
/// The layout was solved for `effective`; a client whose viewport is larger
/// centers that rect and letterboxes the margin, while a client at exactly the
/// effective size fills `area`. The size is clamped to `area` so it never
/// exceeds the viewport (and the centering subtraction never underflows).
pub(crate) fn content_rect(area: RatatuiRect, effective: Size) -> RatatuiRect {
    let width = effective.cols.min(area.width);
    let height = effective.rows.min(area.height);
    RatatuiRect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Fill the letterbox margin — the cells of `area` outside the centered
/// `content` rect — with a dim backdrop, so the space around a layout smaller
/// than the viewport reads as an intentional letterbox. Does nothing when the
/// content fills the whole area.
///
/// The margin is the four bands around `content`; [`render_frame`] already
/// blanked every cell with `Clear`, so restyling is enough. [`Buffer::set_style`]
/// clips to the buffer, so an `area` larger than `buf` (a resize race can report
/// a viewport bigger than the current buffer) never indexes out of bounds.
fn draw_letterbox(area: RatatuiRect, content: RatatuiRect, theme: &Theme, buf: &mut Buffer) {
    if content == area {
        return;
    }
    let style = letterbox_style(theme);
    let bands = [
        // Above the content, full width.
        RatatuiRect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: content.y - area.y,
        },
        // Below the content, full width.
        RatatuiRect {
            x: area.x,
            y: content.bottom(),
            width: area.width,
            height: area.bottom() - content.bottom(),
        },
        // Left of the content, its own height.
        RatatuiRect {
            x: area.x,
            y: content.y,
            width: content.x - area.x,
            height: content.height,
        },
        // Right of the content, its own height.
        RatatuiRect {
            x: content.right(),
            y: content.y,
            width: area.right() - content.right(),
            height: content.height,
        },
    ];
    for band in bands {
        buf.set_style(band, style);
    }
}

mod style;
mod tabline;

use style::*;
use tabline::draw_tabline;
pub(crate) use tabline::tabline_layout;

#[cfg(test)]
mod tests;
