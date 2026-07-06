//! Stock (plugin-free) frame composition.
//!
//! [`render_frame`] paints one [`RenderSnapshot`] into a ratatui [`Buffer`] as
//! three fixed zones: a **tabline** on the top row (session name and tab list on
//! the left, a right-aligned status section — scroll position and mode tag), the
//! **pane area** in the middle (a bordered box per visible pane, the focused
//! pane's border highlighted), and the **keybinding hint bar** on the bottom row
//! — a koshi-owned row reserved here and left blank until config and action
//! metadata are available to fill it.
//!
//! Collapsed members of a stacked pane group are drawn as one-row title strips
//! in the pane area, and each visible terminal pane's cells are painted into its
//! content rect. The focused pane's cursor cell is reported separately by
//! [`cursor_position`] for the caller to place the terminal's hardware cursor,
//! since the buffer itself carries no cursor. When the active tab has no room
//! for any pane, a centered "terminal too small" overlay replaces the pane
//! render for that frame. When the client's viewport is larger than the size
//! the layout was solved for, the whole frame is centered and the surrounding
//! margin is filled with a dim letterbox. The keybinding hints are painted by a
//! later task over the same buffer; plugin-contributed segments (empty here) are
//! injected once the plugin host lands.

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

use crate::snapshot::{PaneSnapshot, RenderSnapshot};

/// Paint `snapshot` into `buf` over `area` (the client's full viewport).
///
/// Blanks `area` first so a buffer reused across frames shows no stale cells,
/// then draws the pane borders, each visible pane's terminal cells, and the
/// collapsed stack-member strips, then the tabline over the top row. The bottom
/// row is the koshi-owned keybinding hint bar: reserved and left blank here,
/// filled by a later task. When the active tab has no room for any pane
/// (`all_suppressed`), draws only a centered too-small overlay and returns,
/// skipping the panes and the tabline. Does nothing for a zero-size area.
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

    // Blank the viewport first: ratatui double-buffers without clearing, so a
    // reused buffer would otherwise keep leftover cells in the tabline gap, the
    // reserved hint row, and any pane interior not painted this frame.
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

    let tabline = RatatuiRect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: 1,
    };
    draw_tabline(snapshot, tabline, buf);

    draw_letterbox(area, content, buf);
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

/// Find the [`PaneSnapshot`] with the given id in this frame.
fn find_pane(snapshot: &RenderSnapshot, id: PaneId) -> Option<&PaneSnapshot> {
    snapshot.panes.iter().find(|pane| pane.id == id)
}

/// Draw a bordered box for every visible pane in the active tab, highlighting
/// the client's focused pane's border. `offset` shifts each pane into the
/// centered content rect.
fn draw_panes(snapshot: &RenderSnapshot, offset: Point, buf: &mut Buffer) {
    let focused = snapshot.client.focused_pane;
    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        let style = if Some(slot.pane_id) == focused {
            border_focused_style()
        } else {
            border_unfocused_style()
        };
        Block::new()
            .borders(Borders::ALL)
            .border_style(style)
            .render(place(slot.rect, offset), buf);
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
        draw_grid(&view.grid, place(inner, offset), pane.reverse_video, buf);
    }
}

/// Paint one terminal `grid` into `area`, one buffer cell per grid cell.
///
/// Each grid cell is placed at its own column, so the on-screen columns follow
/// the grid rather than a re-measured glyph width. The continuation half of a
/// wide glyph (width 0) is skipped — the wide base already covers it. A wide
/// glyph whose second half falls outside the content area is replaced by a blank
/// so no half-glyph bleeds past the edge. `reverse_video` (DECSCNM) toggles
/// reverse for every cell. `area` is clipped to the buffer so an oversized rect
/// cannot index out of bounds.
fn draw_grid(grid: &Grid, area: RatatuiRect, reverse_video: bool, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    let (grid_rows, grid_cols) = grid.dimensions();
    let rows = grid_rows.min(area.height);
    let cols = grid_cols.min(area.width);
    for row in 0..rows {
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
            let style = cell_style(cell.style(), reverse_video);
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
/// right-aligned, over an inverted-background row that marks the strip as
/// koshi-owned. `offset` shifts each strip into the centered content rect.
fn draw_stack_headers(snapshot: &RenderSnapshot, offset: Point, buf: &mut Buffer) {
    let style = stack_header_style();
    for header in &snapshot.session.active_tab.stack_headers {
        let rect = place(header.rect, offset);
        if rect.width == 0 || rect.height == 0 {
            continue;
        }

        // Invert the whole row first so the gap between the title and the
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

/// Draw the tabline: session name and tab list on the left, an optional scroll
/// indicator and the mode tag right-aligned.
fn draw_tabline(snapshot: &RenderSnapshot, area: RatatuiRect, buf: &mut Buffer) {
    let mut left = vec![Span::raw(snapshot.session.name.clone()), Span::raw(" │ ")];
    for (i, meta) in snapshot.session.tabs_metadata.iter().enumerate() {
        if i > 0 {
            left.push(Span::raw(" "));
        }
        let label = format!("{}:{}", meta.index + 1, meta.name);
        let style = if meta.active {
            tab_active_style()
        } else {
            Style::default()
        };
        left.push(Span::styled(label, style));
    }
    set_line_clipped(buf, area.x, area.y, &Line::from(left), area.width);

    let mut right = Vec::new();
    if let Some((offset, total)) = focused_scroll(snapshot) {
        right.push(Span::raw(format!("SCROLL {offset}/{total} ")));
    }
    right.push(Span::styled(
        format!("[{}]", mode_tag(snapshot.client.lock_mode)),
        mode_style(),
    ));
    let right = Line::from(right);
    let width = right.width() as u16;
    set_line_clipped(
        buf,
        area.right().saturating_sub(width),
        area.y,
        &right,
        width,
    );
}

/// The short mode tag shown in the tabline for each input mode.
fn mode_tag(mode: LockMode) -> &'static str {
    match mode {
        LockMode::Normal => "BASE",
        LockMode::Locked => "LOCK",
        LockMode::Resize => "RESIZE",
        LockMode::PaneMode => "PANE",
        LockMode::TabMode => "TAB",
        LockMode::ScrollMode => "SCROLL",
        LockMode::SearchMode => "SEARCH",
    }
}

/// The focused pane's scroll position as `(lines scrolled up, retained lines)`,
/// or `None` when the pane is at the live tail (nothing to indicate).
fn focused_scroll(snapshot: &RenderSnapshot) -> Option<(usize, usize)> {
    let focused = snapshot.client.focused_pane?;
    let pane = find_pane(snapshot, focused)?;
    let offset = pane.grid_view.as_ref().map_or(0, |view| view.view_offset);
    if offset == 0 {
        return None;
    }
    Some((offset, pane.scrollback.retained_lines))
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
fn content_rect(area: RatatuiRect, effective: Size) -> RatatuiRect {
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
fn draw_letterbox(area: RatatuiRect, content: RatatuiRect, buf: &mut Buffer) {
    if content == area {
        return;
    }
    let style = letterbox_style();
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

/// Inverted style marking the active tab in the tab list.
fn tab_active_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Inverted style marking a collapsed stack member's koshi-owned header strip.
fn stack_header_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Bold style for the tabline mode tag.
fn mode_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Bold style for the terminal-too-small overlay message.
fn too_small_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Dim backdrop style for the letterbox margin around a centered layout.
fn letterbox_style() -> Style {
    Style::default().bg(Color::DarkGray)
}

/// Highlighted border style for the focused pane.
fn border_focused_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// Dim border style for unfocused panes.
fn border_unfocused_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests;
