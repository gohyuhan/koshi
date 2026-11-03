//! The chrome of the floating pane tray and of the notification center, and
//! the floating panes over the tiled layer: where each one this viewer draws
//! sits in the frame, and how one is drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use koshi_core::geometry::{Point, Rect};
use koshi_core::ids::PaneId;

use crate::floating_chrome::{
    compute_floating_pane_layout_origin, compute_floating_pane_strip, compute_floating_titlebar,
    FloatingTitlebar, FLOATING_CHROME_SIZE,
};
use crate::snapshot::{
    CommittedRegions, DrawnFloatingPaneSlot, PaneSnapshot, PlacementHandleHover, RenderSnapshot,
    ViewerChrome,
};
use crate::theme::Theme;

use super::{
    clear_drawn_rows, compute_pane_area, compute_pane_border_style, compute_symbol_display_width,
    draw_pane_border, draw_pane_contents, draw_panes, draw_placement_hover_tints,
    draw_stack_headers, draw_too_small_overlay, get_area_origin, get_area_rect,
    list_visible_layout_area_panes, place_cell_rect, place_screen_rect, set_line_clipped,
    write_text_clipped, DrawnRows, LayoutAreaSurface, PlacedLayoutArea,
};

/// The grab handle a floating pane draws on its titlebar strip.
const GRAB_HANDLE_TEXT: &str = "⣿⣿";

/// The pin toggle of a floating pane the viewing client has not pinned.
const UNPINNED_TOGGLE_TEXT: &str = "[ ]";

/// The pin toggle of a floating pane the viewing client pinned.
const PINNED_TOGGLE_TEXT: &str = "[⚪]";

/// The minimize button of a floating pane.
const MINIMIZE_BUTTON_TEXT: &str = "–";

/// The close button of a floating pane, of the tray and of the notification
/// center.
pub(super) const CLOSE_BUTTON_TEXT: &str = "✕";

/// One floating pane this viewer draws, placed in the frame: its drawn slot,
/// its box moved from pane-area cells to frame cells, and where its layout
/// area starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedFloatingPane<'a> {
    /// The drawn slot this placement comes from.
    pub(crate) drawn_floating_pane_slot: DrawnFloatingPaneSlot<'a>,
    /// The floating pane's whole box: the strip row and the layout area.
    pub(crate) outer_area: RatatuiRect,
    /// The frame cell where the layout area's `(0, 0)` cell lands: the first
    /// cell below the strip.
    pub(crate) layout_origin: Point,
    /// `outer_area` cut to the pane area. The cells, images and cursor of the
    /// floating pane's panes show only here.
    pub(crate) visible_area: RatatuiRect,
}

impl<'a> PlacedFloatingPane<'a> {
    /// Each pane of this floating pane's tree that is visible, has a content
    /// rect and has a pane snapshot in `render_snapshot`, in pane slot order
    /// ([`list_visible_layout_area_panes`]): its snapshot, its content rect
    /// placed at the layout origin, and that rect cut to the visible area.
    pub(crate) fn list_visible_panes(
        &self,
        render_snapshot: &'a RenderSnapshot,
    ) -> impl Iterator<Item = (&'a PaneSnapshot, RatatuiRect, RatatuiRect)> + 'a {
        let visible_area = self.visible_area;
        list_visible_layout_area_panes(
            render_snapshot,
            &self.drawn_floating_pane_slot.floating_pane_slot.layout_area,
            self.layout_origin,
        )
        .map(move |(pane_snapshot, content_area)| {
            (
                pane_snapshot,
                content_area,
                content_area.intersection(visible_area),
            )
        })
    }

    /// The content rect of `pane_id`, a visible pane of this floating pane's
    /// tree, placed at the layout origin, and that rect cut to the visible
    /// area. `None` when the tree holds no visible slot of `pane_id`.
    pub(crate) fn find_pane_content_areas(
        &self,
        pane_id: PaneId,
    ) -> Option<(RatatuiRect, RatatuiRect)> {
        let content_rect = self
            .drawn_floating_pane_slot
            .floating_pane_slot
            .layout_area
            .find_visible_pane_slot(pane_id)?
            .content_rect?;
        let content_area = place_cell_rect(content_rect, self.layout_origin);
        Some((content_area, content_area.intersection(self.visible_area)))
    }
}

/// The floating panes this viewer draws in this frame, back to front
/// ([`FrameLayout::list_drawn_floating_pane_slots`](crate::snapshot::FrameLayout::list_drawn_floating_pane_slots)),
/// each moved from pane-area cells by the origin of the pane area that
/// `committed_regions` leave inside `viewport_area`.
///
/// A slot with outer rect `(150, 4)` `40x12` on a viewport whose pane area
/// starts at `(0, 1)` has its outer area at `(150, 5)` and its layout origin
/// at `(150, 6)`.
pub(crate) fn list_placed_floating_panes<'a>(
    render_snapshot: &'a RenderSnapshot,
    committed_regions: &'a CommittedRegions,
    viewer_chrome: ViewerChrome,
    viewport_area: RatatuiRect,
) -> Vec<PlacedFloatingPane<'a>> {
    let pane_area = compute_pane_area(committed_regions, viewport_area);
    let pane_area_origin = get_area_origin(pane_area);
    render_snapshot
        .build_painted_frame_layout(viewer_chrome, committed_regions)
        .list_drawn_floating_pane_slots()
        .map(|drawn_floating_pane_slot| {
            let outer_area = place_cell_rect(drawn_floating_pane_slot.outer_rect, pane_area_origin);
            PlacedFloatingPane {
                drawn_floating_pane_slot,
                outer_area,
                layout_origin: compute_floating_pane_layout_origin(get_area_rect(outer_area)),
                visible_area: outer_area.intersection(pane_area),
            }
        })
        .collect()
}

/// Draw one placed floating pane over every cell drawn before it, in this
/// order:
///
/// 1. Blanks its box and the glyph that reaches into it from the left
///    ([`clear_overlay_area`]).
/// 2. Writes the grab handle `⣿⣿`, the pin toggle `[ ]` (`[⚪]` while the slot
///    is pinned), the minimize button `–` and the close button `✕` on the
///    strip row, each where [`compute_floating_pane_strip`] places it, in the
///    style [`compute_pane_border_style`] gives the slot's input pane: the
///    placement hover style while `viewer_chrome.placement_handle_hover` is
///    [`FloatingHandle`](PlacementHandleHover::FloatingHandle) of that pane
///    with its pin state.
/// 3. Draws the layout area at the layout origin as the tiled layer draws a
///    tab ([`LayoutAreaSurface::FloatingPane`]): the pane boxes with their
///    labels and scroll positions, the pane cells, the placement hover tint
///    and the stack headers. The pane cells and the tint are cut to the
///    visible area. No pane draws a placement handle, and while that hover
///    is `FloatingHandle`, every pane takes the placement hover style and
///    tint. While every pane of the layout area is suppressed, it draws
///    `Terminal too small — enlarge window` centered in the visible cells
///    below the strip instead ([`draw_too_small_overlay`]).
///
/// Every step runs only over the rows `drawn_rows` draws. A box on no drawn
/// row is not drawn.
pub(super) fn draw_floating_pane(
    render_snapshot: &RenderSnapshot,
    placed_floating_pane: &PlacedFloatingPane<'_>,
    viewer_chrome: ViewerChrome,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let floating_pane_slot = placed_floating_pane
        .drawn_floating_pane_slot
        .floating_pane_slot;
    let outer_area = placed_floating_pane.outer_area;
    if !drawn_rows.is_area_drawn(outer_area) {
        return;
    }
    clear_overlay_area(outer_area, drawn_rows, screen_buffer);
    let is_grab_handle_hovered = viewer_chrome.placement_handle_hover
        == Some(PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_slot.pane_id,
            is_pinned: floating_pane_slot.is_pinned,
        });
    if drawn_rows.is_row_drawn(outer_area.y) {
        let strip_style = compute_pane_border_style(
            render_snapshot,
            floating_pane_slot.pane_id,
            is_grab_handle_hovered,
            viewer_chrome,
            theme,
        );
        let strip = compute_floating_pane_strip(get_area_rect(outer_area));
        let pin_toggle_text = if floating_pane_slot.is_pinned {
            PINNED_TOGGLE_TEXT
        } else {
            UNPINNED_TOGGLE_TEXT
        };
        let strip_parts = [
            (strip.grab_handle_rect, GRAB_HANDLE_TEXT),
            (strip.pin_toggle_rect, pin_toggle_text),
            (strip.minimize_button_rect, MINIMIZE_BUTTON_TEXT),
            (strip.close_button_rect, CLOSE_BUTTON_TEXT),
        ];
        for (part_rect, part_text) in strip_parts {
            if let Some(part_rect) = part_rect {
                write_text_clipped(screen_buffer, part_rect, part_text, strip_style);
            }
        }
    }

    let layout_area = &floating_pane_slot.layout_area;
    if layout_area.is_every_pane_suppressed {
        let below_strip_area = place_screen_rect(Rect::from_origin_and_size(
            placed_floating_pane.layout_origin,
            layout_area.solved_size,
        ))
        .intersection(placed_floating_pane.visible_area);
        draw_too_small_overlay(below_strip_area, drawn_rows, screen_buffer);
        return;
    }
    let placed_layout_area = PlacedLayoutArea {
        layout_area,
        layout_area_surface: LayoutAreaSurface::FloatingPane {
            is_grab_handle_hovered,
        },
        layout_origin: placed_floating_pane.layout_origin,
        content_clip_area: placed_floating_pane.visible_area,
    };
    draw_panes(
        render_snapshot,
        placed_layout_area,
        viewer_chrome,
        None,
        theme,
        drawn_rows,
        screen_buffer,
    );
    draw_pane_contents(
        render_snapshot,
        placed_layout_area,
        drawn_rows,
        screen_buffer,
    );
    draw_placement_hover_tints(placed_layout_area, viewer_chrome, drawn_rows, screen_buffer);
    draw_stack_headers(
        render_snapshot,
        placed_layout_area,
        viewer_chrome,
        None,
        theme,
        drawn_rows,
        screen_buffer,
    );
}

/// Draw the chrome of the tray or of the notification center whose box is
/// `outer_area` over every cell drawn before it, in `chrome_style`, and return
/// where its titlebar parts sit ([`compute_floating_titlebar`]). In this
/// order:
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
/// Example: a `24`-column box at `(0, 0)` reads
/// `┌──────────────────────┐`, `│                   ✕  │` and
/// `├──────────────────────┤` on its first three rows.
pub(super) fn draw_floating_frame(
    outer_area: RatatuiRect,
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
    let titlebar = compute_floating_titlebar(get_area_rect(outer_area));
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
