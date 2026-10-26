//! The viewer's open notification center and its toast, each drawn over every
//! cell drawn before it.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use koshi_core::geometry::Rect;

use crate::notification::{
    build_row_rect, compute_border_label_rects, compute_maximum_center_scroll_row_offset,
    format_notification_header, list_notification_center_rows, wrap_text_rows,
    NotificationCenterLayout, NotificationCenterRow, NotificationToastContent,
    NotificationToastLayout, NotificationView, NOTIFICATION_CENTER_TITLE_TEXT,
    NOTIFICATION_CLEAR_LABEL_TEXT, NOTIFICATION_CLOSE_CONTROL_TEXT,
};
use crate::snapshot::NotificationCenter;
use crate::theme::Theme;

use super::floating::clear_overlay_area;
use super::style::{
    compute_focused_border_style, compute_notification_severity_style,
    compute_unfocused_border_style,
};
use super::{
    compute_symbol_display_width, draw_pane_border, get_text_width, place_screen_rect,
    set_line_clipped, DrawnRows,
};

/// Draw the open center `center_layout` over every cell drawn before it, in
/// this order:
///
/// 1. Blanks the center's box ([`clear_overlay_area`]) and draws its border
///    ring: in the focused border style while the center holds the viewer's
///    keyboard (`notification_center.is_focused`), and in the unfocused border
///    style otherwise.
/// 2. Writes ` Notifications ` in the title cells ([`write_text_fitted`]) and
///    ` × ` in the close control's cells of the top border, and ` clear ` in the
///    clear label's cells of the bottom border, cut to them, each in the
///    border's style.
/// 3. Writes the body rows ([`list_notification_center_rows`]) from row
///    `notification_center.scroll_row_offset`, cut to
///    [`compute_maximum_center_scroll_row_offset`], one per body row
///    ([`write_text_fitted`]). An entry's header takes its severity's style
///    ([`compute_notification_severity_style`]), its text the default style,
///    and the status text the unfocused border style.
///
/// Every step runs only over the rows `drawn_rows` draws. A center on no
/// drawn row is not drawn.
///
/// Example: a focused center whose newest entry is the `Error`
/// `pane …a1b2 killed by signal 9` repeated twice reads `[Koshi] Error ×2`
/// on its first body row and `pane …a1b2 killed by signal 9` under it.
pub(super) fn draw_notification_center(
    center_layout: &NotificationCenterLayout,
    notification_center: NotificationCenter,
    notification_view: NotificationView<'_>,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let center_area = place_screen_rect(center_layout.center_rect);
    if !drawn_rows.is_area_drawn(center_area) {
        return;
    }
    let border_style = if notification_center.is_focused {
        compute_focused_border_style(theme)
    } else {
        compute_unfocused_border_style(theme)
    };
    clear_overlay_area(center_area, drawn_rows, screen_buffer);
    draw_pane_border(center_area, border_style, drawn_rows, screen_buffer);
    if drawn_rows.is_row_drawn(center_area.y) {
        write_text_fitted(
            screen_buffer,
            center_layout.title_rect,
            NOTIFICATION_CENTER_TITLE_TEXT,
            border_style,
        );
        write_text_clipped(
            screen_buffer,
            center_layout.close_rect,
            NOTIFICATION_CLOSE_CONTROL_TEXT,
            border_style,
        );
    }
    if drawn_rows.is_row_drawn(center_layout.clear_rect.origin.row) {
        write_text_clipped(
            screen_buffer,
            center_layout.clear_rect,
            NOTIFICATION_CLEAR_LABEL_TEXT,
            border_style,
        );
    }

    let body_rect = center_layout.body_rect;
    let drawn_scroll_row_offset = match notification_center.scroll_row_offset {
        0 => 0,
        scroll_row_offset => scroll_row_offset.min(compute_maximum_center_scroll_row_offset(
            notification_view,
            body_rect,
        )),
    };
    let center_rows = list_notification_center_rows(
        notification_view,
        body_rect.size.column_count,
        drawn_scroll_row_offset,
    )
    .take(usize::from(body_rect.size.row_count));
    for (body_row_index, center_row) in center_rows.enumerate() {
        let screen_row = body_rect.origin.row + body_row_index as u16;
        if !drawn_rows.is_row_drawn(screen_row) {
            continue;
        }
        let (row_text, row_style) = match center_row {
            NotificationCenterRow::EntryHeader(entry) => (
                format_notification_header(entry),
                compute_notification_severity_style(theme, entry.severity),
            ),
            NotificationCenterRow::EntryText(text_row) => (text_row, Style::default()),
            NotificationCenterRow::StatusText(text_row) => {
                (text_row, compute_unfocused_border_style(theme))
            }
        };
        write_text_fitted(
            screen_buffer,
            build_row_rect(
                body_rect.origin.column,
                screen_row,
                body_rect.size.column_count,
            ),
            &row_text,
            row_style,
        );
    }
}

/// Draw the toast `toast_content` at `toast_layout` over every cell drawn
/// before it: the part of the box in `toast_layout.drawn_rect`, from box
/// column `toast_layout.first_drawn_box_column`.
///
/// The box has a border ring in the focused border style, and its top border
/// holds ` × ` in the close control's cells. Its first body row holds the
/// header ([`NotificationToastContent::format_header`]) in the header's
/// severity style ([`compute_notification_severity_style`]), fitted to the
/// body's columns ([`write_text_fitted`]). The rows under it hold the text
/// ([`NotificationToastContent::format_text`]) wrapped to the body's columns
/// ([`wrap_text_rows`]). Text that needs more rows than the box holds keeps
/// the rows that fit, and its last row ends in `…`.
///
/// A glyph that the drawn part cuts at its last column is drawn as a blank
/// with the glyph's style, and a glyph it cuts at its first column leaves a
/// blank. Every cell runs only over the rows `drawn_rows` draws.
///
/// Example: a `26`-column toast of the `Error` `pane …a1b2 killed by signal 9`
/// reads `┌──────────────────── × ─┐`, `│ [Koshi] Error          │`,
/// `│ pane …a1b2 killed by   │`, `│ signal 9               │` and
/// `└────────────────────────┘`.
pub(super) fn draw_notification_toast(
    toast_layout: &NotificationToastLayout,
    toast_content: NotificationToastContent<'_>,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let drawn_area = place_screen_rect(toast_layout.drawn_rect);
    if !drawn_rows.is_area_drawn(drawn_area) {
        return;
    }
    let box_size = toast_layout.toast_rect.size;
    let box_rect = Rect::from_size_at_origin(box_size);
    let box_area = place_screen_rect(box_rect);
    let mut box_buffer = Buffer::empty(box_area);
    let border_style = compute_focused_border_style(theme);
    draw_pane_border(box_area, border_style, DrawnRows::Every, &mut box_buffer);
    write_text_clipped(
        &mut box_buffer,
        compute_border_label_rects(box_rect).close_rect,
        NOTIFICATION_CLOSE_CONTROL_TEXT,
        border_style,
    );
    let body_column_count = box_size.column_count - 4;
    write_text_fitted(
        &mut box_buffer,
        build_row_rect(2, 1, body_column_count),
        &toast_content.format_header(),
        compute_notification_severity_style(theme, toast_content.get_severity()),
    );
    let text_row_count = usize::from(box_size.row_count - 3);
    let text_rows = wrap_text_rows(&toast_content.format_text(), body_column_count);
    for (text_row_index, text_row) in text_rows.iter().take(text_row_count).enumerate() {
        let row_rect = build_row_rect(2, 2 + text_row_index as u16, body_column_count);
        let is_cut_row = text_row_index + 1 == text_row_count && text_rows.len() > text_row_count;
        if is_cut_row {
            write_text_ending_in_ellipsis(&mut box_buffer, row_rect, text_row, Style::default());
        } else {
            write_text_clipped(&mut box_buffer, row_rect, text_row, Style::default());
        }
    }

    clear_overlay_area(drawn_area, drawn_rows, screen_buffer);
    for screen_row in drawn_rows.list_rows_between(drawn_area.top(), drawn_area.bottom()) {
        let box_row = screen_row - drawn_area.y;
        for drawn_column_index in 0..drawn_area.width {
            let Some(screen_cell) =
                screen_buffer.cell_mut((drawn_area.x + drawn_column_index, screen_row))
            else {
                continue;
            };
            *screen_cell = box_buffer[(
                toast_layout.first_drawn_box_column + drawn_column_index,
                box_row,
            )]
                .clone();
            let glyph_column_count = compute_symbol_display_width(screen_cell.symbol()).max(1);
            if drawn_column_index + glyph_column_count > drawn_area.width {
                screen_cell.set_symbol(" ");
            }
        }
    }
}

/// Write `text` in `text_style` from the top-left cell of the one-row
/// `text_rect` of at least one cell: whole when it fits
/// ([`write_text_clipped`]), and ending in `…` when it is wider
/// ([`write_text_ending_in_ellipsis`]). In 8 cells, `pane` reads `pane`, and
/// `pane exited` reads `pane ex…`.
fn write_text_fitted(screen_buffer: &mut Buffer, text_rect: Rect, text: &str, text_style: Style) {
    if get_text_width(text) <= text_rect.size.column_count {
        write_text_clipped(screen_buffer, text_rect, text, text_style);
    } else {
        write_text_ending_in_ellipsis(screen_buffer, text_rect, text, text_style);
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

/// Write `text` in `text_style` from the top-left cell of the one-row
/// `text_rect` of at least one cell, then `…`: the characters that fit before
/// the rect's last cell, and `…` right after them. In 8 cells, `pane exited`
/// reads `pane ex…`, and `abc` reads `abc…`.
fn write_text_ending_in_ellipsis(
    screen_buffer: &mut Buffer,
    text_rect: Rect,
    text: &str,
    text_style: Style,
) {
    let column_count = text_rect.size.column_count;
    let text_column_count = get_text_width(text).min(column_count - 1);
    let ellipsis_column = set_line_clipped(
        screen_buffer,
        text_rect.origin.column,
        text_rect.origin.row,
        &Line::from(Span::styled(text, text_style)),
        text_column_count,
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
