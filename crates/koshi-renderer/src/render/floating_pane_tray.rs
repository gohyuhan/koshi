//! The viewer's open floating pane tray: its floating chrome, its titlebar,
//! and one tile per slot, drawn over the floating panes.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use koshi_core::geometry::{Point, Rect, Size};

use crate::floating_pane_tray::{
    compute_tray_tile_count, select_toggle_minimize_all_label_text, FloatingPaneTrayLayout,
};
use crate::snapshot::{FloatingPane, FloatingPaneTray, RenderSnapshot};
use crate::theme::Theme;

use crate::floating_chrome::FloatingTitlebarControls;

use super::floating::draw_floating_frame;
use super::style::{
    compute_focused_border_style, compute_hover_border_style, compute_placement_hover_border_style,
    compute_unfocused_border_style,
};
use super::{
    draw_pane_border, get_text_width, place_screen_rect, set_line_clipped, write_text_clipped,
    write_text_fitted, DrawnRows,
};

/// The text of the tile that creates a floating pane.
const NEW_FLOATING_PANE_TILE_TEXT: &str = "+ new";

/// Draw the open tray `tray_layout` over every cell drawn before it, in this
/// order:
///
/// 1. Draws the floating chrome ([`draw_floating_frame`]) with the close
///    button only, in the focused border style.
/// 2. Writes the title of the tile under the pointer
///    (`floating_pane_tray.hovered_tile_index`), else of the selected tile
///    ([`find_floating_pane_title`]), in the titlebar's tile title cells, and
///    the label ([`select_toggle_minimize_all_label_text`]) in the label's
///    cells, each fitted to its cells ([`write_text_fitted`]) in the focused
///    border style. The `+ new` tile has no title.
/// 3. Draws each slot's border ring. Slot `i` shows the session's floating
///    pane `i`: its number `i + 1` on the top border as ` <number> `, and its
///    title on the row under it ([`write_pane_title_clipped`]), each in the
///    eight cells inside the ring. A tile whose pane has no title shows its
///    number alone. The slot right after the last floating pane, while a slot
///    is free ([`compute_tray_tile_count`]), is the `+ new` tile: no number,
///    and ` + new` on the row under the top border. Every other slot draws its
///    ring alone.
///
/// A tile is lit while the viewer did not minimize its pane, and gray while
/// the viewer minimized it ([`FloatingPane::is_minimized`]). The `+ new` tile
/// is lit. A lit tile's text takes the placement hover style, and a gray
/// tile's text and an empty slot's ring the unfocused border style. The
/// selected tile's ring takes the hover border style, and every other tile's
/// ring its text style.
///
/// Every step runs only over the rows `drawn_rows` draws. A tray on no drawn
/// row is not drawn.
///
/// Example: tile `0` of `htop`, lit and selected, at `(7, 4)` reads
/// `┌ 1 ─────┐`, `│ htop   │` and `└────────┘`. Tile `1` of `logserver`
/// reads `┌ 2 ─────┐`, `│ logser…│` and `└────────┘`.
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
    draw_floating_frame(
        tray_area,
        FloatingTitlebarControls::CLOSE_BUTTON_ONLY,
        tray_style,
        drawn_rows,
        screen_buffer,
    );

    if drawn_rows.is_row_drawn(tray_area.y.saturating_add(1)) {
        let header_tile_index = floating_pane_tray
            .hovered_tile_index
            .unwrap_or(floating_pane_tray.selected_tile_index);
        if let (Some(tile_title_rect), Some(pane_title)) = (
            tray_layout.tile_title_rect,
            session_snapshot
                .floating_panes
                .get(header_tile_index)
                .and_then(|floating_pane| find_floating_pane_title(render_snapshot, floating_pane)),
        ) {
            write_text_fitted(screen_buffer, tile_title_rect, pane_title, tray_style);
        }
        if let (Some(toggle_minimize_all_rect), Some(label_text)) = (
            tray_layout.toggle_minimize_all_rect,
            select_toggle_minimize_all_label_text(session_snapshot),
        ) {
            write_text_fitted(
                screen_buffer,
                toggle_minimize_all_rect,
                label_text,
                tray_style,
            );
        }
    }

    let tile_count = compute_tray_tile_count(session_snapshot.floating_panes.len());
    for (slot_index, slot_rect) in tray_layout.slot_rects.iter().enumerate() {
        let slot_area = place_screen_rect(*slot_rect);
        if slot_index >= tile_count {
            draw_pane_border(
                slot_area,
                compute_unfocused_border_style(theme),
                drawn_rows,
                screen_buffer,
            );
            continue;
        }
        let floating_pane = session_snapshot.floating_panes.get(slot_index);
        let tile_text_style = if floating_pane.is_some_and(FloatingPane::is_minimized) {
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
        if floating_pane.is_some() && drawn_rows.is_row_drawn(slot_area.y) {
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
        let tile_title = match floating_pane {
            Some(floating_pane) => find_floating_pane_title(render_snapshot, floating_pane),
            None => Some(NEW_FLOATING_PANE_TILE_TEXT),
        };
        if let Some(tile_title) = tile_title {
            if drawn_rows.is_row_drawn(slot_area.y + 1) {
                write_pane_title_clipped(
                    screen_buffer,
                    title_row_rect,
                    tile_title,
                    tile_text_style,
                );
            }
        }
    }
}

/// The title of `floating_pane` in `render_snapshot`: the title of the pane's
/// [`PaneSnapshot`](crate::snapshot::PaneSnapshot) while the viewer shows it,
/// and the title [`FloatingPane::Minimized`] carries while the viewer
/// minimized it. `None` when the pane reported no title.
fn find_floating_pane_title<'a>(
    render_snapshot: &'a RenderSnapshot,
    floating_pane: &'a FloatingPane,
) -> Option<&'a str> {
    match floating_pane {
        FloatingPane::Shown { pane_id } => render_snapshot
            .find_pane_snapshot(*pane_id)
            .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref()),
        FloatingPane::Minimized { pane_title, .. } => pane_title.as_deref(),
    }
}

/// Write ` <pane_title> ` in `text_style` from the top-left cell of the
/// one-row `text_rect` of at least one cell. A title that does not fit the
/// cells after the leading space keeps the cells that fit before the rect's
/// last cell, and `…` follows them.
///
/// In 8 cells, `htop` reads ` htop   `, `logserv` reads ` logserv`, `postgres`
/// reads ` postgr…`, and `a日本語テ` reads ` a日本…`.
fn write_pane_title_clipped(
    screen_buffer: &mut Buffer,
    text_rect: Rect,
    pane_title: &str,
    text_style: Style,
) {
    let column_count = text_rect.size.column_count;
    if get_text_width(pane_title) < column_count {
        write_text_clipped(
            screen_buffer,
            text_rect,
            &format!(" {pane_title} "),
            text_style,
        );
        return;
    }
    let ellipsis_column = set_line_clipped(
        screen_buffer,
        text_rect.origin.column,
        text_rect.origin.row,
        &Line::from(Span::styled(format!(" {pane_title}"), text_style)),
        column_count - 1,
    );
    set_line_clipped(
        screen_buffer,
        ellipsis_column,
        text_rect.origin.row,
        &Line::from(Span::styled("…", text_style)),
        1,
    );
}

#[cfg(test)]
mod tests;
