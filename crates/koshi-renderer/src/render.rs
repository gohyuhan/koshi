//! Stock (plugin-free) frame composition.
//!
//! [`render_frame`] paints one [`RenderSnapshot`] into a ratatui
//! [`Buffer`] as three stock zones: a **tabline** (session name, the running
//! koshi version, and the tab list on the left, the right-aligned mode tag),
//! the **pane area** (a bordered box per visible pane, the focused pane's
//! border highlighted), and the **statusline** — a koshi-owned keybinding row
//! painted from the per-mode keybinding data the caller passes in. The
//! committed region solve supplies all three zones.
//!
//! Collapsed members of a stacked pane group are drawn as one-row title strips
//! in the pane area, and each visible terminal pane's cells are painted into its
//! content rect. The focused pane's cursor cell is reported separately by
//! [`get_cursor_position`] for the caller to place the terminal's hardware cursor;
//! the buffer itself carries no cursor. When the active tab has no room
//! for any pane, a centered "terminal too small" overlay replaces the pane
//! render for that frame. When the pane area is larger than the size the
//! layout was solved for, the layout is centered inside that pane area and the
//! surrounding margin is filled with a dim letterbox. Nothing here draws
//! plugin-contributed segments.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::PaneId;
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::{Color as CellColor, Style as CellStyle, UnderlineStyle};

use crate::images::{
    compute_image_placeholder_rects, compute_selected_image_placeholder_rects,
    draw_image_placeholders, ImagePlacementKey, ImageRenderMode,
};
use crate::region::StatuslineInputs;
use crate::snapshot::{
    CommittedRegions, CursorStyle, KeymapHints, PaneSnapshot, Reconnecting, RenderSnapshot,
    SelectionSpans, ViewerChrome,
};
use crate::statusline_hints::draw_statusline;
use crate::theme::Theme;

/// Paint `render_snapshot` into `buffer` over `viewport_area` with the selected image mode.
///
/// It does nothing for a zero-size area. When the active tab has no room for any
/// pane (`are_all_panes_suppressed`), it blanks `viewport_area`, draws a centered too-small
/// overlay, and returns, skipping the panes and both chrome rows.
///
/// Otherwise paints in this order:
///
/// 1. Blanks every cell of `viewport_area`, so a buffer reused across frames shows no
///    stale cells.
/// 2. Draws one bordered box per visible pane, its title in the top border and
///    its scroll position in the bottom border when it is scrolled back.
/// 3. Draws each visible terminal pane's cells into its content rect.
/// 4. Keeps pane cells under native images or writes the unsupported-image
///    text over unavailable coverage.
/// 5. Draws the one-row title strip for every collapsed stack member.
/// 6. Fills the letterbox margin: every cell of `viewport_area` outside the centered
///    layout, the chrome rows included.
/// 7. Draws the tabline in the first committed region, over that margin.
/// 8. Draws the statusline in the second committed region, over that margin.
///
/// `theme`, `hints`, `pending_key_sequence`, and `viewer_chrome` come from the viewer: the colors
/// it paints koshi's chrome in, the statusline data for the mode it is in, the
/// multi-chord sequence it has open, and the pane its pointer is over together
/// with where its tab strip is scrolled and whether it is dialing the session
/// again.
/// The attached client passes the same [`CommittedRegions`] to this function
/// and to [`get_cursor_position`]. For example, a left region of 20 columns on a
/// `120x40` viewport leaves the pane rectangle at `x = 20`.
///
/// # Panics
///
/// In a debug build, when `render_snapshot.client_snapshot.active_tab_id` is not
/// `render_snapshot.session_snapshot.active_tab_snapshot.tab_id`.
#[allow(clippy::too_many_arguments)]
pub fn render_frame(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    theme: &Theme,
    hints: &KeymapHints,
    pending_key_sequence: Option<&KeySequence>,
    viewer_chrome: ViewerChrome,
    viewport_area: RatatuiRect,
    buffer: &mut Buffer,
) {
    render_frame_with_images(
        render_snapshot,
        committed_regions,
        theme,
        hints,
        pending_key_sequence,
        viewer_chrome,
        ImageRenderMode::Placeholder,
        viewport_area,
        buffer,
    );
}

/// Paint `render_snapshot` into `buffer` with a selected terminal-image mode.
///
/// `Placeholder` writes `terminal image unavailable` into visible image
/// rectangles. `Native` keeps the pane cells beneath available image pixels
/// and writes the same marker while a record is missing. A four-column image
/// at `(12, 6)` in placeholder mode writes `term` across the first four cells.
#[allow(clippy::too_many_arguments)]
pub fn render_frame_with_images(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    theme: &Theme,
    hints: &KeymapHints,
    pending_key_sequence: Option<&KeySequence>,
    viewer_chrome: ViewerChrome,
    image_mode: ImageRenderMode,
    viewport_area: RatatuiRect,
    buffer: &mut Buffer,
) {
    render_frame_with_image_availability(
        render_snapshot,
        committed_regions,
        theme,
        hints,
        pending_key_sequence,
        viewer_chrome,
        image_mode,
        None,
        viewport_area,
        buffer,
    );
}

/// Paint one frame with a selected set of image placements kept beneath native
/// output.
#[allow(clippy::too_many_arguments)]
pub fn render_frame_with_image_availability(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    theme: &Theme,
    hints: &KeymapHints,
    pending_key_sequence: Option<&KeySequence>,
    viewer_chrome: ViewerChrome,
    image_mode: ImageRenderMode,
    available_image_keys: Option<&[ImagePlacementKey]>,
    viewport_area: RatatuiRect,
    buffer: &mut Buffer,
) {
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return;
    }

    // A per-client snapshot solves the tab that client is viewing into
    // `session_snapshot.active_tab_snapshot`, so its id must match the client's viewed tab.
    debug_assert_eq!(
        render_snapshot.client_snapshot.active_tab_id,
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_id,
        "snapshot builder must solve the client's active tab into session_snapshot.active_tab_snapshot"
    );

    // Reset every cell of `viewport_area` first, so a buffer carried over from the
    // previous frame keeps no cell in the tabline gap, the reserved statusline row,
    // or a pane interior this frame does not paint.
    Clear.render(viewport_area, buffer);

    // No room for any pane: the whole frame becomes the too-small overlay.
    if render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .are_all_panes_suppressed
    {
        draw_too_small_overlay(viewport_area, buffer);
        return;
    }

    // Center the solved layout inside this client's viewport. The layout was
    // solved for the tab's effective (smallest-client) size, so a larger client
    // has margin: `effective_layout_rect` is that effective-sized rect centered
    // in the pane area left by the committed regions, and `layout_origin` shifts each
    // effective-space layout rect into it.
    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, viewport_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
    );
    let layout_origin = get_area_origin(effective_layout_rect);

    draw_panes(
        render_snapshot,
        theme,
        viewer_chrome.hovered_pane_id,
        layout_origin,
        buffer,
    );
    draw_pane_contents(render_snapshot, layout_origin, buffer);
    match image_mode {
        ImageRenderMode::Placeholder => {
            let placeholder_rects = compute_image_placeholder_rects(
                render_snapshot,
                committed_regions,
                viewport_area,
                false,
            );
            draw_image_placeholders(&placeholder_rects, buffer);
        }
        ImageRenderMode::Native => {
            let placeholder_rects = match available_image_keys {
                Some(available_image_keys) => compute_selected_image_placeholder_rects(
                    render_snapshot,
                    committed_regions,
                    viewport_area,
                    Some(available_image_keys),
                ),
                None => compute_image_placeholder_rects(
                    render_snapshot,
                    committed_regions,
                    viewport_area,
                    true,
                ),
            };
            draw_image_placeholders(&placeholder_rects, buffer);
        }
    }
    draw_stack_headers(render_snapshot, theme, layout_origin, buffer);

    // The margin fills first; the tabline and statusline paint over it.
    draw_letterbox(viewport_area, effective_layout_rect, theme, buffer);

    // The same tab-row facts hit-testing reads, so the tabline drawn is the
    // tabline classified.
    let tabline_inputs = render_snapshot
        .build_frame_layout(viewer_chrome)
        .get_tabline_inputs();
    if let Some(tabline_rect) = find_region_area(committed_regions, 0, viewport_area) {
        draw_tabline(tabline_inputs, theme, tabline_rect, buffer);
    }

    if let Some(statusline_rect) = find_region_area(committed_regions, 1, viewport_area) {
        draw_statusline(
            StatuslineInputs {
                keymap_hints: hints,
                pending_key_sequence,
            },
            theme,
            statusline_rect,
            buffer,
        );
    }
}

/// The buffer cell where the client's focused pane wants the hardware cursor, or
/// `None` when no cursor should show this frame.
///
/// Companion to [`render_frame`]: the buffer carries no cursor, so the caller
/// reads this alongside the paint — passing the same `viewport_area` and committed
/// regions — and places the terminal's cursor at the returned [`Position`] (or
/// hides it on `None`). The
/// position is the focused pane's cursor cell — its row and column within the
/// pane's content area, shifted by the same letterbox offset `render_frame`
/// centers the layout with and clamped to that content area's last cell — in
/// the same absolute buffer coordinates the panes are drawn in.
///
/// Returns `None` when the client has no focused pane; that pane has no placed
/// slot or no content snapshot; it is not visible or has no content area
/// (suppressed, hidden, a collapsed stack member, or a slot of two or fewer
/// columns or rows, whose content rect holds no cells); it has no terminal grid
/// (a plugin pane, or a slot showing nothing this frame); its view is scrolled
/// back into history (no hardware cursor is placed while scrolled); or the
/// application has hidden its cursor.
///
/// A `120x40` viewport with a 20-column left region places the same pane cursor
/// 20 columns farther right than a whole-area layout.
pub fn get_cursor_position(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewport_area: RatatuiRect,
) -> Option<Position> {
    let focused_pane_id = render_snapshot.client_snapshot.focused_pane_id?;

    let pane_slot = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.pane_id == focused_pane_id)?;
    if !pane_slot.is_visible {
        return None;
    }
    let content_rect = pane_slot.content_rect?;
    if content_rect.cell_size.column_count == 0 || content_rect.cell_size.row_count == 0 {
        return None;
    }

    let pane_snapshot = find_pane_snapshot(render_snapshot, focused_pane_id)?;
    // A pane with no grid — a plugin pane — places no cursor.
    let terminal_grid_view = pane_snapshot.terminal_grid_view.as_ref()?;
    // A view scrolled back into history shows no hardware cursor.
    if terminal_grid_view.view_row_offset > 0 {
        return None;
    }
    if !pane_snapshot.cursor_snapshot.is_visible {
        return None;
    }

    // Map the pane-local cursor (column/row counted from the content area's own
    // top-left) to a screen cell. `content_rect` is the content rect in
    // effective-layout space; `place_cell_rect` shifts it by the same letterbox
    // offset `render_frame` centers with. The placed origin plus the local
    // column/row is
    // the screen position, clamped to the rect's last cell: a dead pane keeps a
    // frozen col/row while its content rect shrinks, so the sum can land past
    // the edge.
    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, viewport_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
    );
    let placed_content_rect = place_cell_rect(content_rect, get_area_origin(effective_layout_rect));
    let screen_column = (placed_content_rect.x + pane_snapshot.cursor_snapshot.column_index)
        .min(placed_content_rect.right().saturating_sub(1));
    let screen_row = (placed_content_rect.y + pane_snapshot.cursor_snapshot.row_index)
        .min(placed_content_rect.bottom().saturating_sub(1));
    Some(Position::new(screen_column, screen_row))
}

/// How the outer terminal's cursor should look this frame:
/// [`Shaped`](CursorStyle::Shaped) with what the focused pane asked for via
/// DECSCUSR, or [`UserDefault`](CursorStyle::UserDefault) when it asked for
/// nothing — a plain shell never sends DECSCUSR, so its cursor stays whatever
/// the user configured.
///
/// `None` — meaning "leave the cursor as it is" — only when there is no focused
/// terminal pane to speak for it: no focused pane at all, or a plugin pane,
/// which has no terminal and so no opinion.
///
/// Companion to [`get_cursor_position`], which says where the cursor goes; this
/// says what it looks like once it is there. The caller applies it to the outer
/// terminal (crossterm's `SetCursorStyle`), which is what makes vim's
/// insert-mode bar show as a bar instead of a block.
///
/// Not gated on the cursor being visible or the view being scrolled back.
#[must_use]
pub fn get_cursor_style(render_snapshot: &RenderSnapshot) -> Option<CursorStyle> {
    let pane_snapshot = find_pane_snapshot(
        render_snapshot,
        render_snapshot.client_snapshot.focused_pane_id?,
    )?;
    pane_snapshot.terminal_grid_view.as_ref()?;
    let cursor_style = match pane_snapshot.cursor_snapshot.shape {
        Some(cursor_shape) => CursorStyle::Shaped {
            shape: cursor_shape,
            blink: pane_snapshot.cursor_snapshot.is_blinking,
        },
        None => CursorStyle::UserDefault,
    };
    Some(cursor_style)
}

/// Find the [`PaneSnapshot`] with the given pane id in this frame.
pub(crate) fn find_pane_snapshot(
    render_snapshot: &RenderSnapshot,
    pane_id: PaneId,
) -> Option<&PaneSnapshot> {
    render_snapshot
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
}

/// Draw a bordered box for every visible pane in the active tab, coloring the
/// focused pane's border (and an unfocused hovered pane's), writing the pane's
/// resolved title into its top border line, and — when the pane is scrolled
/// back — its scroll position into its bottom border. `hovered_pane_id` is the
/// pane the viewer's pointer is over; `layout_origin` shifts each pane into the centered
/// content rect.
fn draw_panes(
    render_snapshot: &RenderSnapshot,
    theme: &Theme,
    hovered_pane_id: Option<PaneId>,
    layout_origin: Point,
    buffer: &mut Buffer,
) {
    let focused_pane_id = render_snapshot.client_snapshot.focused_pane_id;
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible {
            continue;
        }
        // The focus color wins over the hover color: the hover color marks
        // only an unfocused pane, the one the wheel scrolls.
        let border_style = if Some(pane_slot.pane_id) == focused_pane_id {
            compute_focused_border_style(theme)
        } else if Some(pane_slot.pane_id) == hovered_pane_id {
            compute_hover_border_style(theme)
        } else {
            compute_unfocused_border_style(theme)
        };
        let pane_rect = place_cell_rect(pane_slot.outer_rect, layout_origin);
        Block::new()
            .borders(Borders::ALL)
            .border_style(border_style)
            .render(pane_rect, buffer);

        let pane_snapshot = find_pane_snapshot(render_snapshot, pane_slot.pane_id);

        // The pane's title sits in the top border as ` title `, starting two
        // cells in and clipped four cells short of the box width, which leaves
        // the corner glyphs.
        if let Some(pane_title) =
            pane_snapshot.and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref())
        {
            if !pane_title.is_empty() && pane_rect.width > 4 {
                let title_line = Line::from(Span::styled(format!(" {pane_title} "), border_style));
                set_line_clipped(
                    buffer,
                    pane_rect.x + 2,
                    pane_rect.y,
                    &title_line,
                    pane_rect.width - 4,
                );
            }
        }

        // When this pane is scrolled back, its position sits in the bottom
        // border, right-aligned: ` up/total `. A pane at the live tail shows
        // nothing. Each pane carries its own offset, so several can show at once.
        if let Some((scrolled_line_count, retained_line_count)) =
            pane_snapshot.and_then(get_pane_scroll)
        {
            let scroll_text = format!(" {scrolled_line_count}/{retained_line_count} ");
            let scroll_text_width = get_text_width(&scroll_text);
            if pane_rect.width >= scroll_text_width + 2 {
                let scroll_line = Line::from(Span::styled(scroll_text, border_style));
                let scroll_start_column = pane_rect.right() - 1 - scroll_text_width;
                set_line_clipped(
                    buffer,
                    scroll_start_column,
                    pane_rect.bottom() - 1,
                    &scroll_line,
                    scroll_text_width,
                );
            }
        }
    }
}

/// Draw the "terminal too small" overlay: one centered, bold line telling the
/// user to enlarge the window, shown when the tab has no room for any pane.
///
/// Centered on the middle row of `area` and horizontally within it. A message
/// wider than `area` is clipped at its right edge.
fn draw_too_small_overlay(viewport_area: RatatuiRect, buffer: &mut Buffer) {
    let too_small_message = Line::from(Span::styled(
        "Terminal too small — enlarge window",
        compute_too_small_overlay_style(),
    ));
    let too_small_message_width = get_line_width(&too_small_message);
    let too_small_message_start_column =
        viewport_area.x + viewport_area.width.saturating_sub(too_small_message_width) / 2;
    let message_row = viewport_area.y + viewport_area.height / 2;
    set_line_clipped(
        buffer,
        too_small_message_start_column,
        message_row,
        &too_small_message,
        viewport_area
            .right()
            .saturating_sub(too_small_message_start_column),
    );
}

/// Paint each visible terminal pane's cells into its content rect.
///
/// For every visible pane slot that has a content rect and a terminal grid,
/// draws the grid into that rect. Plugin panes (no grid) and panes with no
/// content rect (suppressed, hidden, or a collapsed stack member) draw nothing.
/// `layout_origin` shifts each content rect into the centered content area.
fn draw_pane_contents(render_snapshot: &RenderSnapshot, layout_origin: Point, buffer: &mut Buffer) {
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible {
            continue;
        }
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(pane_snapshot) = find_pane_snapshot(render_snapshot, pane_slot.pane_id) else {
            continue;
        };
        let Some(terminal_grid_view) = &pane_snapshot.terminal_grid_view else {
            continue;
        };
        draw_grid(
            &terminal_grid_view.grid,
            place_cell_rect(content_rect, layout_origin),
            pane_snapshot.is_reverse_video,
            pane_snapshot.selection_spans.as_ref(),
            buffer,
        );
    }
}

/// Paint one terminal `terminal_grid` into `target_area`, one buffer cell per grid cell.
///
/// Each grid cell is placed at its own column, so on-screen column positions
/// always match grid column positions. The continuation half of a
/// wide glyph (width 0) is skipped — the wide base already covers it. A wide
/// glyph whose second half falls outside the content area is replaced by a blank
/// so no half-glyph bleeds past the edge. `is_reverse_video` (DECSCNM) toggles
/// reverse for every cell. `target_area` is clipped to the buffer so an oversized rect
/// cannot index out of bounds.
///
/// A highlighted cell in `selection_spans` is drawn in reverse. The highlight
/// combines with the cell's own reverse and with `is_reverse_video` by exclusive-or, so
/// highlighting a cell that is already reverse swaps it back to normal.
fn draw_grid(
    terminal_grid: &Grid,
    target_area: RatatuiRect,
    is_reverse_video: bool,
    selection_spans: Option<&SelectionSpans>,
    buffer: &mut Buffer,
) {
    let clipped_area = target_area.intersection(buffer.area);
    let (grid_row_count, grid_column_count) = terminal_grid.get_grid_dimensions();
    let visible_row_count = grid_row_count.min(clipped_area.height);
    let visible_column_count = grid_column_count.min(clipped_area.width);
    for (row_index, cell_row) in (0..visible_row_count).zip(terminal_grid.list_rows()) {
        // A highlight is one column range per row, resolved before the row's
        // cells are walked.
        let row_span = selection_spans.and_then(|spans| spans.find_row_span(row_index));
        let screen_row = clipped_area.y + row_index;
        for column_index in 0..visible_column_count {
            let Some(cell) = cell_row.get(column_index as usize) else {
                continue;
            };
            let cell_width = cell.get_display_width();
            if cell_width == 0 {
                continue;
            }
            let screen_column = clipped_area.x + column_index;
            let is_selected = row_span.is_some_and(|(start_column, end_column)| {
                column_index >= start_column && column_index <= end_column
            });
            let cell_style = get_cell_style(cell.get_style(), is_reverse_video ^ is_selected);
            if cell_width >= 2 && column_index + 1 >= visible_column_count {
                buffer[(screen_column, screen_row)]
                    .set_char(' ')
                    .set_style(cell_style);
                continue;
            }
            if cell.list_combining_characters().is_empty() {
                buffer[(screen_column, screen_row)]
                    .set_char(cell.get_character())
                    .set_style(cell_style);
            } else {
                buffer[(screen_column, screen_row)]
                    .set_symbol(&get_cell_symbol(cell))
                    .set_style(cell_style);
            }
        }
    }
}

/// The glyph a cell draws: its base character followed by any combining marks
/// and joined code points, as one string.
fn get_cell_symbol(cell: &Cell) -> String {
    let mut cell_symbol = String::with_capacity(1 + cell.list_combining_characters().len());
    cell_symbol.push(cell.get_character());
    cell_symbol.extend(cell.list_combining_characters().iter().copied());
    cell_symbol
}

/// Map a terminal cell style to a ratatui [`Style`].
///
/// Colors map directly, the terminal default becoming ratatui's reset. Each
/// boolean attribute maps to its modifier; every underline variant collapses to
/// a single underline, and overline and underline color have no ratatui modifier
/// and are not drawn. `reverse_video` (DECSCNM) combines with the cell's own
/// reverse by exclusive-or, so a screen-wide reverse cancels a cell already in
/// reverse.
fn get_cell_style(cell_style: CellStyle, is_reverse_video: bool) -> Style {
    let cell_attributes = cell_style.get_attributes();
    let mut text_modifier = Modifier::empty();
    if cell_attributes.is_bold() {
        text_modifier |= Modifier::BOLD;
    }
    if cell_attributes.is_faint() {
        text_modifier |= Modifier::DIM;
    }
    if cell_attributes.is_italic() {
        text_modifier |= Modifier::ITALIC;
    }
    if cell_attributes.get_underline_style() != UnderlineStyle::None {
        text_modifier |= Modifier::UNDERLINED;
    }
    if cell_attributes.is_blinking() {
        text_modifier |= Modifier::SLOW_BLINK;
    }
    if cell_attributes.is_concealed() {
        text_modifier |= Modifier::HIDDEN;
    }
    if cell_attributes.is_strikethrough() {
        text_modifier |= Modifier::CROSSED_OUT;
    }
    if cell_attributes.is_reverse() ^ is_reverse_video {
        text_modifier |= Modifier::REVERSED;
    }
    Style::default()
        .fg(get_cell_color(cell_style.get_foreground_color()))
        .bg(get_cell_color(cell_style.get_background_color()))
        .add_modifier(text_modifier)
}

/// Map a terminal color to a ratatui [`Color`]; the terminal default becomes
/// ratatui's reset (the outer terminal's own default).
fn get_cell_color(cell_color: CellColor) -> Color {
    match cell_color {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(color_index) => Color::Indexed(color_index),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Draw the one-row title strip for every collapsed stack member: a collapse
/// arrow and the pane title on the left, a `[position/total]` indicator
/// right-aligned, over a theme-filled row that marks the strip as
/// koshi-owned. `layout_origin` shifts each strip into the centered content rect.
fn draw_stack_headers(
    render_snapshot: &RenderSnapshot,
    theme: &Theme,
    layout_origin: Point,
    buffer: &mut Buffer,
) {
    let stack_header_style = compute_stack_header_style(theme);
    for stack_header in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers
    {
        let header_rect = place_cell_rect(stack_header.header_rect, layout_origin);
        if header_rect.width == 0 || header_rect.height == 0 {
            continue;
        }

        // Fill the whole row first: the gap between the title and the indicator
        // carries the strip background too.
        buffer.set_style(header_rect, stack_header_style);

        let pane_title = get_stack_header_title(render_snapshot, stack_header.pane_id);
        let title_line = Line::from(format!("▸ {pane_title}"));
        set_line_clipped(
            buffer,
            header_rect.x,
            header_rect.y,
            &title_line,
            header_rect.width,
        );

        // Right-align `[N/total]`, with its start column clamped to `rect.x`. A
        // strip narrower than the indicator writes only inside the strip.
        let indicator_line = Line::from(format!(
            "[{}/{}]",
            stack_header.member_index + 1,
            stack_header.member_count
        ));
        let indicator_width = get_line_width(&indicator_line);
        let indicator_start_column = header_rect
            .right()
            .saturating_sub(indicator_width)
            .max(header_rect.x);
        set_line_clipped(
            buffer,
            indicator_start_column,
            header_rect.y,
            &indicator_line,
            header_rect.right() - indicator_start_column,
        );
    }
}

/// The title drawn on a stack member's header strip: the pane's terminal title,
/// or empty when the pane has none.
fn get_stack_header_title(render_snapshot: &RenderSnapshot, pane_id: PaneId) -> &str {
    find_pane_snapshot(render_snapshot, pane_id)
        .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref())
        .unwrap_or_default()
}

/// The mode indicator shown in the tabline: every active mode label joined with
/// ` · `, or `BASE` when the client is in plain mode with the mouse ungrabbed
/// and `reconnecting` is `None`.
///
/// The labels compose from independent axes, always in this order:
/// `reconnecting` adds `RECONNECTING (attempt 4, retry in 8s)` from a
/// `Reconnecting { attempt: 4, retry_in_seconds: 8 }`, the `lock_mode` layer
/// contributes at most one tag (nothing when `Normal`), and mouse-selection mode adds
/// `SELECT`. So that same client, locked and grabbing the mouse, reads
/// `RECONNECTING (attempt 4, retry in 8s) · LOCK · SELECT`, and a plain one
/// grabbing it reads `SELECT`. A client with `reconnecting` set never reads
/// `BASE`.
fn build_mode_tags(
    lock_mode: LockMode,
    is_mouse_selection_enabled: bool,
    reconnecting: Option<Reconnecting>,
) -> String {
    let reconnect_tag = reconnecting.map(|reconnecting_state| {
        format!(
            "RECONNECTING (attempt {}, retry in {}s)",
            reconnecting_state.attempt, reconnecting_state.retry_in_seconds
        )
    });
    let mut mode_tag_labels: Vec<&str> = Vec::new();
    if let Some(mode_tag) = reconnect_tag.as_deref() {
        mode_tag_labels.push(mode_tag);
    }
    if let Some(mode_tag) = lock_mode_tag(lock_mode) {
        mode_tag_labels.push(mode_tag);
    }
    if is_mouse_selection_enabled {
        mode_tag_labels.push("SELECT");
    }
    if mode_tag_labels.is_empty() {
        "BASE".to_string()
    } else {
        mode_tag_labels.join(" · ")
    }
}

/// The tag for a non-plain lock mode, or `None` for `Normal` — which shows as
/// `BASE` only when no other mode is active.
fn lock_mode_tag(lock_mode: LockMode) -> Option<&'static str> {
    match lock_mode {
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
fn get_pane_scroll(pane_snapshot: &PaneSnapshot) -> Option<(usize, usize)> {
    let view_row_offset = pane_snapshot
        .terminal_grid_view
        .as_ref()
        .map_or(0, |grid_view| grid_view.view_row_offset);
    (view_row_offset > 0).then_some((
        view_row_offset,
        pane_snapshot.scrollback_meta.retained_line_count,
    ))
}

/// The top-left cell of a ratatui rect, as the layout origin [`place_cell_rect`]
/// shifts by. An
/// area at `column: 20, row: 1` gives `Point { column: 20, row: 1 }`.
fn get_area_origin(screen_area: RatatuiRect) -> Point {
    Point {
        column: screen_area.x,
        row: screen_area.y,
    }
}

/// Place a koshi-core cell rect into a ratatui area by shifting its origin.
///
/// A region solution starts at `(0, 0)`; a ratatui frame area can start elsewhere.
pub(crate) fn place_cell_rect(cell_rect: Rect, layout_origin: Point) -> RatatuiRect {
    RatatuiRect {
        x: cell_rect.origin.column + layout_origin.column,
        y: cell_rect.origin.row + layout_origin.row,
        width: cell_rect.cell_size.column_count,
        height: cell_rect.cell_size.row_count,
    }
}

/// Return one committed region in the ratatui frame's coordinate space.
pub(crate) fn find_region_area(
    committed_regions: &CommittedRegions,
    region_index: usize,
    viewport_area: RatatuiRect,
) -> Option<RatatuiRect> {
    committed_regions
        .solved_regions
        .region_rects
        .get(region_index)
        .map(|&region_rect| place_cell_rect(region_rect, get_area_origin(viewport_area)))
}

/// Return the pane rectangle left by the committed regions in frame coordinates.
pub(crate) fn compute_pane_area(
    committed_regions: &CommittedRegions,
    viewport_area: RatatuiRect,
) -> RatatuiRect {
    place_cell_rect(
        committed_regions.solved_regions.pane_rect,
        get_area_origin(viewport_area),
    )
}

/// The cells `text` occupies when drawn, counted as Unicode display width and
/// never from its bytes or chars: `漢字` is 4, `🦀` is 2, `e` plus a combining
/// acute is 1.
///
/// Text wider than `u16::MAX` cells gives `u16::MAX`, which every width
/// comparison in this crate reads as wider than the row.
pub(crate) fn get_text_width(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

/// The cells `line` occupies when drawn, summed over its spans and never taken
/// from their styles.
///
/// A line wider than `u16::MAX` cells gives `u16::MAX`, which every width
/// comparison in this crate reads as wider than the row.
pub(crate) fn get_line_width(line: &Line<'_>) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

/// Draw a line, writing nothing when `row_index` lies outside the buffer's rows.
///
/// [`Buffer::set_line`] clips a line horizontally but writes its row with no
/// vertical bound: a row past the buffer's height panics. A buffer shorter than
/// the laid-out frame — a resize left the frame's rows solved for a taller
/// size — places chrome rows below it, and those rows are skipped.
pub(crate) fn set_line_clipped(
    buffer: &mut Buffer,
    start_column: u16,
    row_index: u16,
    line: &Line<'_>,
    maximum_width: u16,
) {
    if row_index < buffer.area.top() || row_index >= buffer.area.bottom() {
        return;
    }
    buffer.set_line(start_column, row_index, line, maximum_width);
}

/// The centered rect of the effective (solved) size within the pane area.
///
/// A pane area larger than `effective` leaves a letterbox margin around the
/// rect. Each dimension is clamped to the pane area's own.
pub(crate) fn compute_content_rect(
    pane_area: RatatuiRect,
    effective_cell_size: Size,
) -> RatatuiRect {
    let column_count = effective_cell_size.column_count.min(pane_area.width);
    let row_count = effective_cell_size.row_count.min(pane_area.height);
    RatatuiRect {
        x: pane_area.x + (pane_area.width - column_count) / 2,
        y: pane_area.y + (pane_area.height - row_count) / 2,
        width: column_count,
        height: row_count,
    }
}

/// Fill the letterbox margin — the cells of `viewport_area` outside the centered
/// `effective_layout_rect` — with a dim backdrop. Does nothing when the content fills the
/// whole area.
///
/// The margin is the four bands around `effective_layout_rect` once it is cut
/// to `viewport_area`: a layout solved for a larger viewport reaches past the
/// painted area, and the part outside it is not a band. Each band is restyled
/// in place, never blanked: [`render_frame`] clears every cell of `viewport_area`
/// before this runs. [`Buffer::set_style`] clips to the buffer, so a viewport
/// area larger than `buffer` writes only inside `buffer`.
fn draw_letterbox(
    viewport_area: RatatuiRect,
    effective_layout_rect: RatatuiRect,
    theme: &Theme,
    buffer: &mut Buffer,
) {
    let effective_layout_rect = viewport_area.intersection(effective_layout_rect);
    if effective_layout_rect == viewport_area {
        return;
    }
    let letterbox_style = compute_letterbox_style(theme);
    let letterbox_bands = [
        // Above the content, full width.
        RatatuiRect {
            x: viewport_area.x,
            y: viewport_area.y,
            width: viewport_area.width,
            height: effective_layout_rect.y - viewport_area.y,
        },
        // Below the content, full width.
        RatatuiRect {
            x: viewport_area.x,
            y: effective_layout_rect.bottom(),
            width: viewport_area.width,
            height: viewport_area
                .bottom()
                .saturating_sub(effective_layout_rect.bottom()),
        },
        // Left of the content, its own height.
        RatatuiRect {
            x: viewport_area.x,
            y: effective_layout_rect.y,
            width: effective_layout_rect.x - viewport_area.x,
            height: effective_layout_rect.height,
        },
        // Right of the content, its own height.
        RatatuiRect {
            x: effective_layout_rect.right(),
            y: effective_layout_rect.y,
            width: viewport_area
                .right()
                .saturating_sub(effective_layout_rect.right()),
            height: effective_layout_rect.height,
        },
    ];
    for letterbox_band in letterbox_bands {
        buffer.set_style(letterbox_band, letterbox_style);
    }
}

mod style;
mod tabline;

use style::*;
use tabline::draw_tabline;
// The statusline fills its row with the same bar background as the tabline.
pub(crate) use style::compute_bar_style;
pub(crate) use tabline::solve_tabline_layout;
// The badge text, reachable from the sibling test modules.
#[cfg(test)]
pub(crate) use tabline::create_version_badge_text;

#[cfg(test)]
mod tests;
