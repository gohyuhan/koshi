//! Floating panes over the tiled layer: where each one this viewer draws sits
//! in the frame, and how one is drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};

use koshi_core::constant::FLOATING_PANE_CHROME_SIZE;
use koshi_core::geometry::{Rect, Size};

use crate::floating_pane_chrome::compute_floating_pane_titlebar;
use crate::snapshot::{CommittedRegions, FloatingPaneSlot, RenderSnapshot, ViewerChrome};
use crate::theme::Theme;

use super::{
    compute_pane_area, compute_pane_border_style, draw_grid, draw_pane_scroll_position,
    draw_placement_hover_tint, get_area_origin, is_placement_handle_hovered, place_cell_rect,
    set_line_clipped,
};

/// The grab handle a floating pane draws in its titlebar.
const GRAB_HANDLE_TEXT: &str = "⣿⣿";

/// The pin toggle of a floating pane the viewing client has not pinned.
const UNPINNED_TOGGLE_TEXT: &str = "[ ]";

/// The pin toggle of a floating pane the viewing client pinned.
const PINNED_TOGGLE_TEXT: &str = "[⚪]";

/// The minimize button of a floating pane.
const MINIMIZE_BUTTON_TEXT: &str = "–";

/// The close button of a floating pane.
const CLOSE_BUTTON_TEXT: &str = "✕";

/// One floating pane this viewer draws, placed in the frame: its slot, and
/// its rects moved from pane-area cells to frame cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFloatingPane<'a> {
    /// The slot this placement comes from.
    pub(crate) floating_pane_slot: &'a FloatingPaneSlot,
    /// The pane's box with its chrome.
    pub(crate) outer_area: RatatuiRect,
    /// The cells inside the chrome, where the pane's grid starts.
    pub(crate) content_area: RatatuiRect,
    /// `content_area` cut to the pane area: the same top-left cell, with the
    /// columns and rows past the pane area's right and bottom edges removed.
    /// The pane's cells, images and cursor show only here.
    pub(crate) visible_content_area: RatatuiRect,
}

/// The floating panes this viewer draws in this frame, back to front
/// ([`FrameLayout::list_drawn_floating_pane_slots`](crate::snapshot::FrameLayout::list_drawn_floating_pane_slots)),
/// each moved from pane-area cells by the origin of the pane area that
/// `committed_regions` leave inside `viewport_area`.
///
/// A slot with outer rect `(150, 4)` `40x12` on a viewport whose pane area
/// starts at `(0, 1)` has its outer area at `(150, 5)` and its content area
/// at `(151, 8)` `38x8`.
pub(crate) fn list_placed_floating_panes<'a>(
    render_snapshot: &'a RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
    viewport_area: RatatuiRect,
) -> Vec<PlacedFloatingPane<'a>> {
    let pane_area = compute_pane_area(committed_regions, viewport_area);
    let pane_area_origin = get_area_origin(pane_area);
    render_snapshot
        .build_frame_layout(viewer_chrome)
        .list_drawn_floating_pane_slots()
        .iter()
        .map(|floating_pane_slot| {
            let content_area = place_cell_rect(floating_pane_slot.content_rect, pane_area_origin);
            PlacedFloatingPane {
                floating_pane_slot,
                outer_area: place_cell_rect(floating_pane_slot.outer_rect, pane_area_origin),
                content_area,
                visible_content_area: content_area.intersection(pane_area),
            }
        })
        .collect()
}

/// Draw one placed floating pane over every cell drawn before it, in this
/// order:
///
/// 1. Blanks every cell of its outer area.
/// 2. Draws its border ring in the style [`compute_pane_border_style`] gives
///    it.
/// 3. On an outer area of 4 rows or more, draws the separator `├──┤` on the
///    third row and the titlebar on the second row, each part where
///    [`compute_floating_pane_titlebar`] places it: the grab handle `⣿⣿`, the
///    pane title cut to its cells, the pin toggle `[ ]` (`[⚪]` while
///    `is_pinned`), the minimize button `–` and the close button `✕`.
/// 4. Draws the pane's cells into its visible content area.
/// 5. Tints the visible content area while `viewer_chrome.placement_handle_pane_id`
///    names the pane ([`is_placement_handle_hovered`]).
/// 6. Writes the pane's scroll position ` up/total ` into its bottom border
///    while it is scrolled back.
///
/// The chrome takes the border style. A pane with no content snapshot draws
/// its chrome with no title, no cells and no scroll position.
pub(super) fn draw_floating_pane(
    render_snapshot: &RenderSnapshot,
    placed_floating_pane: &PlacedFloatingPane<'_>,
    viewer_chrome: ViewerChrome,
    theme: &Theme,
    screen_buffer: &mut Buffer,
) {
    let floating_pane_slot = placed_floating_pane.floating_pane_slot;
    let outer_area = placed_floating_pane.outer_area;
    let border_style = compute_pane_border_style(
        render_snapshot,
        floating_pane_slot.pane_id,
        false,
        viewer_chrome,
        theme,
    );
    Clear.render(outer_area, screen_buffer);
    Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .render(outer_area, screen_buffer);

    let pane_snapshot = render_snapshot.find_pane_snapshot(floating_pane_slot.pane_id);
    if outer_area.width >= 2 && outer_area.height >= FLOATING_PANE_CHROME_SIZE.row_count {
        let separator_text = format!("├{}┤", "─".repeat(usize::from(outer_area.width - 2)));
        set_line_clipped(
            screen_buffer,
            outer_area.x,
            outer_area.y.saturating_add(2),
            &Line::from(Span::styled(separator_text, border_style)),
            outer_area.width,
        );
    }
    let pane_title = pane_snapshot
        .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref())
        .unwrap_or("");
    draw_floating_pane_titlebar(
        outer_area,
        pane_title,
        floating_pane_slot.is_pinned,
        border_style,
        screen_buffer,
    );

    let Some(pane_snapshot) = pane_snapshot else {
        return;
    };
    if let Some(terminal_grid_view) = &pane_snapshot.terminal_grid_view {
        draw_grid(
            &terminal_grid_view.grid,
            placed_floating_pane.visible_content_area,
            pane_snapshot.is_reverse_video,
            pane_snapshot.selection_spans.as_ref(),
            screen_buffer,
        );
    }
    if is_placement_handle_hovered(floating_pane_slot.pane_id, viewer_chrome) {
        draw_placement_hover_tint(placed_floating_pane.visible_content_area, screen_buffer);
    }
    draw_pane_scroll_position(pane_snapshot, outer_area, border_style, screen_buffer);
}

/// Write the titlebar parts of the floating pane whose box is `outer_area`,
/// each cut to the cells [`compute_floating_pane_titlebar`] gives it, in
/// `chrome_style`. An empty `pane_title` writes no title.
///
/// In a `48`-column box at `(0, 0)` titled `htop`, row `1` reads
/// `│ ⣿⣿  htop                          [ ]  –  ✕  │` once the border is
/// drawn.
fn draw_floating_pane_titlebar(
    outer_area: RatatuiRect,
    pane_title: &str,
    is_pinned: bool,
    chrome_style: Style,
    screen_buffer: &mut Buffer,
) {
    let titlebar = compute_floating_pane_titlebar(Rect::from_origin_and_size(
        get_area_origin(outer_area),
        Size {
            column_count: outer_area.width,
            row_count: outer_area.height,
        },
    ));
    let pin_toggle_text = if is_pinned {
        PINNED_TOGGLE_TEXT
    } else {
        UNPINNED_TOGGLE_TEXT
    };
    let titlebar_parts = [
        (titlebar.grab_handle_rect, GRAB_HANDLE_TEXT),
        (titlebar.title_rect, pane_title),
        (titlebar.pin_toggle_rect, pin_toggle_text),
        (titlebar.minimize_button_rect, MINIMIZE_BUTTON_TEXT),
        (titlebar.close_button_rect, CLOSE_BUTTON_TEXT),
    ];
    for (part_rect, part_text) in titlebar_parts {
        let Some(part_rect) = part_rect else {
            continue;
        };
        set_line_clipped(
            screen_buffer,
            part_rect.origin.column,
            part_rect.origin.row,
            &Line::from(Span::styled(part_text, chrome_style)),
            part_rect.size.column_count,
        );
    }
}

#[cfg(test)]
mod tests;
