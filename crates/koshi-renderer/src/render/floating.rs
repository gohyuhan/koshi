//! The chrome every floating component draws, and the floating panes over
//! the tiled layer: where each one this viewer draws sits in the frame, and
//! how one is drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use koshi_core::constant::FLOATING_CHROME_SIZE;
use koshi_core::geometry::{Rect, Size};

use crate::floating_chrome::{
    compute_floating_content_rect, compute_floating_titlebar, FloatingTitlebar,
    FloatingTitlebarControls,
};
use crate::snapshot::{
    CommittedRegions, FloatingPaneSlot, PlacementHandleHover, RenderSnapshot, ViewerChrome,
};
use crate::theme::Theme;

use super::{
    clear_drawn_rows, compute_pane_area, compute_pane_border_style, compute_symbol_display_width,
    draw_grid, draw_pane_border, draw_pane_scroll_position, draw_placement_hover_tint,
    get_area_origin, place_cell_rect, set_line_clipped, write_text_clipped, DrawnRows,
};

/// The grab handle a floating pane draws in its titlebar.
const GRAB_HANDLE_TEXT: &str = "⣿⣿";

/// The pin toggle of a floating pane the viewing client has not pinned.
const UNPINNED_TOGGLE_TEXT: &str = "[ ]";

/// The pin toggle of a floating pane the viewing client pinned.
const PINNED_TOGGLE_TEXT: &str = "[⚪]";

/// The minimize button of a floating pane.
const MINIMIZE_BUTTON_TEXT: &str = "–";

/// The close button on the titlebar of every floating component.
pub(super) const CLOSE_BUTTON_TEXT: &str = "✕";

/// One floating pane this viewer draws, placed in the frame: its slot, and
/// its rects moved from pane-area cells to frame cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFloatingPane {
    /// The slot this placement comes from.
    pub(crate) floating_pane_slot: FloatingPaneSlot,
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
pub(crate) fn list_placed_floating_panes(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
    viewport_area: RatatuiRect,
) -> Vec<PlacedFloatingPane> {
    let pane_area = compute_pane_area(committed_regions, viewport_area);
    let pane_area_origin = get_area_origin(pane_area);
    render_snapshot
        .build_painted_frame_layout(viewer_chrome, committed_regions)
        .list_drawn_floating_pane_slots()
        .map(|floating_pane_slot| {
            let content_area = place_cell_rect(
                compute_floating_content_rect(floating_pane_slot.outer_rect),
                pane_area_origin,
            );
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
/// 1. Draws the floating chrome ([`draw_floating_frame`]) with the floating
///    pane's controls ([`FloatingTitlebarControls::FLOATING_PANE`]), in the
///    style [`compute_pane_border_style`] gives the pane: the placement hover
///    style while `viewer_chrome.placement_handle_hover` is
///    [`FloatingHandle`](PlacementHandleHover::FloatingHandle) of the pane
///    with its `is_pinned`.
/// 2. Writes the grab handle `⣿⣿`, the pane title cut to its cells, the pin
///    toggle `[ ]` (`[⚪]` while `is_pinned`) and the minimize button `–` on
///    the titlebar row, each where [`compute_floating_titlebar`] places it, in
///    the chrome's style.
/// 3. Draws the pane's cells into its visible content area.
/// 4. Tints the visible content area while that hover is `FloatingHandle` of
///    the pane with its `is_pinned`.
/// 5. Writes the pane's scroll position ` up/total ` into its bottom border
///    while it is scrolled back.
///
/// A pane with no content snapshot draws its chrome with no title, no cells
/// and no scroll position.
///
/// Steps 1, 3 and 4 run only over the rows `drawn_rows` draws. The titlebar
/// parts and the scroll position are drawn only when `drawn_rows` draws their
/// row. A box on no drawn row is not drawn.
pub(super) fn draw_floating_pane(
    render_snapshot: &RenderSnapshot,
    placed_floating_pane: &PlacedFloatingPane,
    viewer_chrome: ViewerChrome,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let floating_pane_slot = placed_floating_pane.floating_pane_slot;
    let outer_area = placed_floating_pane.outer_area;
    if !drawn_rows.is_area_drawn(outer_area) {
        return;
    }
    let is_grab_handle_hovered = viewer_chrome.placement_handle_hover
        == Some(PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_slot.pane_id,
            is_pinned: floating_pane_slot.is_pinned,
        });
    let border_style = compute_pane_border_style(
        render_snapshot,
        floating_pane_slot.pane_id,
        is_grab_handle_hovered,
        viewer_chrome,
        theme,
    );
    let titlebar = draw_floating_frame(
        outer_area,
        FloatingTitlebarControls::FLOATING_PANE,
        border_style,
        drawn_rows,
        screen_buffer,
    );

    let pane_snapshot = render_snapshot.find_pane_snapshot(floating_pane_slot.pane_id);
    if drawn_rows.is_row_drawn(outer_area.y.saturating_add(1)) {
        let pane_title = pane_snapshot
            .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref())
            .unwrap_or("");
        let pin_toggle_text = if floating_pane_slot.is_pinned {
            PINNED_TOGGLE_TEXT
        } else {
            UNPINNED_TOGGLE_TEXT
        };
        let titlebar_parts = [
            (titlebar.grab_handle_rect, GRAB_HANDLE_TEXT),
            (titlebar.title_rect, pane_title),
            (titlebar.pin_toggle_rect, pin_toggle_text),
            (titlebar.minimize_button_rect, MINIMIZE_BUTTON_TEXT),
        ];
        for (part_rect, part_text) in titlebar_parts {
            if let Some(part_rect) = part_rect {
                write_text_clipped(screen_buffer, part_rect, part_text, border_style);
            }
        }
    }

    let Some(pane_snapshot) = pane_snapshot else {
        return;
    };
    if let Some(terminal_grid_view) = &pane_snapshot.terminal_grid_view {
        draw_grid(
            terminal_grid_view,
            placed_floating_pane.visible_content_area,
            pane_snapshot.is_reverse_video,
            pane_snapshot.selection_spans.as_ref(),
            drawn_rows,
            screen_buffer,
        );
    }
    if is_grab_handle_hovered {
        draw_placement_hover_tint(
            placed_floating_pane.visible_content_area,
            drawn_rows,
            screen_buffer,
        );
    }
    if drawn_rows.is_row_drawn(outer_area.bottom().saturating_sub(1)) {
        draw_pane_scroll_position(pane_snapshot, outer_area, border_style, screen_buffer);
    }
}

/// Draw the chrome of a floating component whose box is `outer_area` over
/// every cell drawn before it, in `chrome_style`, and return where its
/// titlebar parts sit ([`compute_floating_titlebar`] with
/// `titlebar_controls`). In this order:
///
/// 1. Blanks the box and the glyph that reaches into it from the left
///    ([`clear_overlay_area`]).
/// 2. Draws the border ring.
/// 3. On a box of 4 rows or more, draws the separator `├──┤` on the third row
///    and the close button `✕` on the titlebar row.
///
/// The component writes its title and its own controls into the returned
/// rects. Every step runs only over the rows `drawn_rows` draws.
///
/// Example: a `24`-column box at `(0, 0)` with the close button only reads
/// `┌──────────────────────┐`, `│                   ✕  │` and
/// `├──────────────────────┤` on its first three rows.
pub(super) fn draw_floating_frame(
    outer_area: RatatuiRect,
    titlebar_controls: FloatingTitlebarControls,
    chrome_style: Style,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) -> FloatingTitlebar {
    clear_overlay_area(outer_area, drawn_rows, screen_buffer);
    draw_pane_border(outer_area, chrome_style, drawn_rows, screen_buffer);
    if outer_area.width >= 2
        && outer_area.height >= FLOATING_CHROME_SIZE.row_count
        && drawn_rows.is_row_drawn(outer_area.y.saturating_add(2))
    {
        let separator_text = format!("├{}┤", "─".repeat(usize::from(outer_area.width - 2)));
        set_line_clipped(
            screen_buffer,
            outer_area.x,
            outer_area.y.saturating_add(2),
            &Line::from(Span::styled(separator_text, chrome_style)),
            outer_area.width,
        );
    }
    let titlebar = compute_floating_titlebar(
        Rect::from_origin_and_size(
            get_area_origin(outer_area),
            Size {
                column_count: outer_area.width,
                row_count: outer_area.height,
            },
        ),
        titlebar_controls,
    );
    if let Some(close_button_rect) = titlebar.close_button_rect {
        if drawn_rows.is_row_drawn(close_button_rect.origin.row) {
            write_text_clipped(
                screen_buffer,
                close_button_rect,
                CLOSE_BUTTON_TEXT,
                chrome_style,
            );
        }
    }
    titlebar
}

/// Blank every cell of `overlay_area` on each row `drawn_rows` draws, and the
/// glyph that starts left of `overlay_area` and reaches into it. The row is
/// read from its first column, and each glyph covers the number of columns
/// that [`compute_symbol_display_width`] gives its symbol, 1 at least.
/// Example: `世` at column `5` beside an area that starts at column `6`
/// becomes a blank at column `5` with the style `世` had, and `U+17D8` at
/// column `5` stays.
pub(super) fn clear_overlay_area(
    overlay_area: RatatuiRect,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    clear_drawn_rows(overlay_area, drawn_rows, screen_buffer);
    for row_index in drawn_rows.list_rows_between(overlay_area.top(), overlay_area.bottom()) {
        let mut column_index = screen_buffer.area.left();
        while column_index < overlay_area.x {
            let Some(glyph_cell) = screen_buffer.cell_mut((column_index, row_index)) else {
                break;
            };
            let glyph_width = compute_symbol_display_width(glyph_cell.symbol()).max(1);
            if column_index.saturating_add(glyph_width) > overlay_area.x {
                glyph_cell.set_symbol(" ");
            }
            column_index = column_index.saturating_add(glyph_width);
        }
    }
}

#[cfg(test)]
mod tests;
