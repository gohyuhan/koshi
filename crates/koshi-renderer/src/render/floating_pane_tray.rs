//! The viewer's open floating pane tray: its box, its header, and one tile per
//! slot, drawn over the floating panes.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use koshi_core::geometry::{Point, Rect, Size};

use crate::floating_pane_tray::{select_toggle_minimize_all_label_text, FloatingPaneTrayLayout};
use crate::snapshot::{FloatingPaneTray, RenderSnapshot};
use crate::theme::Theme;

use super::floating::clear_overlay_area;
use super::style::{
    compute_focused_border_style, compute_hover_border_style, compute_placement_hover_border_style,
    compute_unfocused_border_style,
};
use super::{draw_pane_border, place_screen_rect, set_line_clipped, DrawnRows};

/// Draw the open tray `tray_layout` over every cell drawn before it, in this
/// order:
///
/// 1. Blanks the tray's box ([`clear_overlay_area`]) and draws its border ring
///    in the focused border style.
/// 2. Writes ` <title> ` of the tile under the pointer
///    (`floating_pane_tray.hovered_tile_index`), else of the selected tile, cut
///    to the header's title cells, and the header label
///    ([`select_toggle_minimize_all_label_text`]) in the header's label cells.
///    Both take the focused border style.
/// 3. Draws each slot's border ring. Slot `i` shows the session's floating
///    pane `i`: its number `i + 1` on the top border as ` <number> `, and
///    ` <title> ` on the row under it, each cut to the six cells inside the
///    ring. A slot with no pane draws its ring alone.
///
/// A tile is lit while the viewer did not minimize its pane, and gray while
/// the viewer minimized it (`FloatingPane::is_minimized`). A
/// lit tile's text takes the placement hover style, and a gray tile's text and
/// an empty slot's ring the unfocused border style. The selected tile's ring
/// takes the hover border style, and every other tile's ring its text style.
///
/// Every step runs only over the rows `drawn_rows` draws. A tray on no drawn
/// row is not drawn.
///
/// Example: tile `0` of `htop`, lit and selected, at `(19, 2)` reads `┌ 1 ───┐`,
/// `│ htop │` and `└──────┘`.
pub(super) fn draw_floating_pane_tray(
    render_snapshot: &RenderSnapshot,
    tray_layout: &FloatingPaneTrayLayout,
    floating_pane_tray: FloatingPaneTray,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let tray_area = place_screen_rect(tray_layout.tray_rect);
    if !drawn_rows.is_area_drawn(tray_area) {
        return;
    }
    let session_snapshot = &render_snapshot.session_snapshot;
    let tray_style = compute_focused_border_style(theme);
    clear_overlay_area(tray_area, drawn_rows, screen_buffer);
    draw_pane_border(tray_area, tray_style, drawn_rows, screen_buffer);

    if drawn_rows.is_row_drawn(tray_area.y) {
        let header_tile_index = floating_pane_tray
            .hovered_tile_index
            .unwrap_or(floating_pane_tray.selected_tile_index);
        if let Some(pane_title) = session_snapshot
            .floating_panes
            .get(header_tile_index)
            .and_then(|floating_pane| floating_pane.pane_title.as_deref())
        {
            write_text_clipped(
                screen_buffer,
                tray_layout.header_title_rect,
                &format!(" {pane_title} "),
                tray_style,
            );
        }
        if let Some(toggle_minimize_all_rect) = tray_layout.toggle_minimize_all_rect {
            write_text_clipped(
                screen_buffer,
                toggle_minimize_all_rect,
                select_toggle_minimize_all_label_text(session_snapshot),
                tray_style,
            );
        }
    }

    for (slot_index, slot_rect) in tray_layout.slot_rects.iter().enumerate() {
        let slot_area = place_screen_rect(*slot_rect);
        let Some(floating_pane) = session_snapshot.floating_panes.get(slot_index) else {
            draw_pane_border(
                slot_area,
                compute_unfocused_border_style(theme),
                drawn_rows,
                screen_buffer,
            );
            continue;
        };
        let tile_text_style = if floating_pane.is_minimized {
            compute_unfocused_border_style(theme)
        } else {
            compute_placement_hover_border_style(theme)
        };
        let tile_border_style = if slot_index == floating_pane_tray.selected_tile_index {
            compute_hover_border_style(theme)
        } else {
            tile_text_style
        };
        draw_pane_border(slot_area, tile_border_style, drawn_rows, screen_buffer);
        let number_rect = Rect::from_origin_and_size(
            Point {
                column: slot_rect.origin.column + 1,
                row: slot_rect.origin.row,
            },
            Size {
                column_count: slot_rect.size.column_count - 2,
                row_count: 1,
            },
        );
        if drawn_rows.is_row_drawn(slot_area.y) {
            write_text_clipped(
                screen_buffer,
                number_rect,
                &format!(" {} ", slot_index + 1),
                tile_border_style,
            );
        }
        let title_row_rect = Rect::from_origin_and_size(
            Point {
                column: number_rect.origin.column,
                row: number_rect.origin.row + 1,
            },
            number_rect.size,
        );
        if let Some(pane_title) = floating_pane.pane_title.as_deref() {
            if drawn_rows.is_row_drawn(slot_area.y + 1) {
                write_text_clipped(
                    screen_buffer,
                    title_row_rect,
                    &format!(" {pane_title} "),
                    tile_text_style,
                );
            }
        }
    }
}

/// Write `text` in `text_style` from the top-left cell of the one-row
/// `text_rect`, cut to its columns ([`set_line_clipped`]).
fn write_text_clipped(screen_buffer: &mut Buffer, text_rect: Rect, text: &str, text_style: Style) {
    set_line_clipped(
        screen_buffer,
        text_rect.origin.column,
        text_rect.origin.row,
        &Line::from(Span::styled(text, text_style)),
        text_rect.size.column_count,
    );
}

#[cfg(test)]
mod tests;
