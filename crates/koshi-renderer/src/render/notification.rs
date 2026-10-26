//! The viewer's notification center and its toast, each drawn over every
//! cell drawn before it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;

use koshi_core::geometry::Rect;

use crate::floating_chrome::FloatingTitlebarControls;
use crate::notification::{
    build_row_rect, compute_card_dismiss_button_column, compute_card_text_column_count,
    compute_toast_close_rect, format_notification_header, list_notification_center_rows,
    NotificationCenterLayout, NotificationCenterRow, NotificationToastLayout,
    NotificationToastView, NotificationView, NOTIFICATION_CENTER_TITLE_TEXT,
    NOTIFICATION_CLEAR_ALL_LABEL_TEXT, NOTIFICATION_TOAST_CLOSE_CONTROL_TEXT,
};
use crate::snapshot::NotificationCenter;
use crate::theme::Theme;

use super::floating::{clear_overlay_area, draw_floating_frame, CLOSE_BUTTON_TEXT};
use super::style::{
    compute_focused_border_style, compute_hover_border_style, compute_notification_severity_style,
    compute_unfocused_border_style,
};
use super::{
    compute_symbol_display_width, draw_pane_border, place_screen_rect, write_text_clipped,
    write_text_ending_in_ellipsis, write_text_fitted, DrawnRows,
};

/// Draw the center `center_layout` over every cell drawn before it, on the
/// cells of its `drawn_rect`.
///
/// A center in place ([`NotificationCenterLayout::drawn_rect`] equal to its
/// `center_rect`) is drawn straight over the rows `drawn_rows` draws
/// ([`draw_notification_center_box`]). A sliding center is drawn whole into
/// a box of its own, and the columns its `drawn_rect` covers are copied
/// over the rows `drawn_rows` draws ([`copy_slid_box`]). A center on no drawn
/// row is not drawn.
pub(super) fn draw_notification_center(
    center_layout: &NotificationCenterLayout,
    notification_center: NotificationCenter,
    notification_view: NotificationView<'_>,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let drawn_area = place_screen_rect(center_layout.drawn_rect);
    if !drawn_rows.is_area_drawn(drawn_area) {
        return;
    }
    if center_layout.drawn_rect == center_layout.center_rect {
        draw_notification_center_box(
            center_layout,
            notification_center,
            notification_view,
            theme,
            drawn_rows,
            screen_buffer,
        );
        return;
    }
    let mut box_buffer = Buffer::empty(place_screen_rect(center_layout.center_rect));
    draw_notification_center_box(
        center_layout,
        notification_center,
        notification_view,
        theme,
        DrawnRows::Every,
        &mut box_buffer,
    );
    copy_slid_box(&box_buffer, drawn_area, 0, drawn_rows, screen_buffer);
}

/// Draw the center at rest, `center_layout.center_rect`, into
/// `screen_buffer`, in this order:
///
/// 1. Draws the floating chrome ([`draw_floating_frame`]) with the close
///    button only, in the focused border style, and writes `Notifications`
///    in the title cells ([`write_text_fitted`]).
/// 2. Writes `Clear all` in its cells on the last content row.
/// 3. Writes the body rows ([`list_notification_center_rows`]) from where
///    the body starts at scroll `notification_center.scroll_row_offset`
///    ([`NotificationView::compute_center_body_rows`]), one per body row
///    ([`draw_notification_center_row`]).
///
/// Every step runs only over the rows `drawn_rows` draws.
///
/// Example: a center whose newest entry is the `Error`
/// `pane …a1b2 killed by signal 9` repeated twice, in a `22`-column body,
/// reads `┌──────────────── ✕ ─┐`, `│ [Koshi] Error ×2   │`,
/// `│ pane …a1b2 killed  │`, `│ by signal 9        │` and
/// `└────────────────────┘` on its first five body rows.
fn draw_notification_center_box(
    center_layout: &NotificationCenterLayout,
    notification_center: NotificationCenter,
    notification_view: NotificationView<'_>,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let frame_style = compute_focused_border_style(theme);
    draw_floating_frame(
        place_screen_rect(center_layout.center_rect),
        FloatingTitlebarControls::CLOSE_BUTTON_ONLY,
        frame_style,
        drawn_rows,
        screen_buffer,
    );
    if let Some(title_rect) = center_layout.title_rect {
        if drawn_rows.is_row_drawn(title_rect.origin.row) {
            write_text_fitted(
                screen_buffer,
                title_rect,
                NOTIFICATION_CENTER_TITLE_TEXT,
                frame_style,
            );
        }
    }
    let clear_all_rect = center_layout.clear_all_rect;
    if drawn_rows.is_row_drawn(clear_all_rect.origin.row) {
        write_text_clipped(
            screen_buffer,
            clear_all_rect,
            NOTIFICATION_CLEAR_ALL_LABEL_TEXT,
            frame_style,
        );
    }

    let body_rect = center_layout.body_rect;
    let row_start = notification_view
        .compute_center_body_rows(body_rect, notification_center.scroll_row_offset)
        .row_start;
    let center_rows =
        list_notification_center_rows(notification_view, body_rect.size.column_count, row_start)
            .take(usize::from(body_rect.size.row_count));
    for (body_row_index, center_row) in center_rows.enumerate() {
        let screen_row = body_rect.origin.row + body_row_index as u16;
        if drawn_rows.is_row_drawn(screen_row) {
            draw_notification_center_row(
                &center_row,
                build_row_rect(
                    body_rect.origin.column,
                    screen_row,
                    body_rect.size.column_count,
                ),
                notification_center,
                theme,
                screen_buffer,
            );
        }
    }
}

/// Draw `center_row` on the one-row `row_rect`, which spans the center's
/// body, at least 8 columns wide.
///
/// - A card's top border reads `┌──── ✕ ─┐` with its dismiss button at
///   [`compute_card_dismiss_button_column`], and its bottom border `└──────┘`.
/// - A card's header and text rows read `│ <text> │`: the header
///   ([`format_notification_header`]) fitted to the card's text cells
///   ([`compute_card_text_column_count`]) in its severity's style
///   ([`compute_notification_severity_style`]), and a text row in the
///   default style.
/// - The center's own text sits one blank column in from each side, in the
///   unfocused border style.
///
/// The card of `notification_center.selected_notification_id` draws its
/// border in the hover border style, and every other card in the unfocused
/// border style.
fn draw_notification_center_row(
    center_row: &NotificationCenterRow<'_>,
    row_rect: Rect,
    notification_center: NotificationCenter,
    theme: &Theme,
    screen_buffer: &mut Buffer,
) {
    let card_column_count = row_rect.size.column_count;
    let text_rect = build_row_rect(
        row_rect.origin.column + 2,
        row_rect.origin.row,
        compute_card_text_column_count(card_column_count),
    );
    let compute_card_border_style = |notification_id| {
        if notification_center.selected_notification_id == Some(notification_id) {
            compute_hover_border_style(theme)
        } else {
            compute_unfocused_border_style(theme)
        }
    };
    match center_row {
        NotificationCenterRow::CardTopBorder(notification_entry) => {
            let border_style = compute_card_border_style(notification_entry.notification_id);
            let dash_text = "─".repeat(usize::from(card_column_count - 2));
            write_text_clipped(
                screen_buffer,
                row_rect,
                &format!("┌{dash_text}┐"),
                border_style,
            );
            write_text_clipped(
                screen_buffer,
                build_row_rect(
                    compute_card_dismiss_button_column(row_rect) - 1,
                    row_rect.origin.row,
                    3,
                ),
                &format!(" {CLOSE_BUTTON_TEXT} "),
                border_style,
            );
        }
        NotificationCenterRow::CardHeader(notification_entry) => {
            draw_card_side_borders(
                screen_buffer,
                row_rect,
                compute_card_border_style(notification_entry.notification_id),
            );
            write_text_fitted(
                screen_buffer,
                text_rect,
                &format_notification_header(notification_entry),
                compute_notification_severity_style(theme, notification_entry.severity),
            );
        }
        NotificationCenterRow::CardText {
            center_entry,
            text_row_index,
        } => {
            draw_card_side_borders(
                screen_buffer,
                row_rect,
                compute_card_border_style(center_entry.get_notification_entry().notification_id),
            );
            let text_rows = center_entry.list_text_rows(text_rect.size.column_count);
            if let Some(text_row) = text_rows.get(*text_row_index) {
                write_text_clipped(screen_buffer, text_rect, text_row, Style::default());
            }
        }
        NotificationCenterRow::CardBottomBorder(notification_entry) => {
            let dash_text = "─".repeat(usize::from(card_column_count - 2));
            write_text_clipped(
                screen_buffer,
                row_rect,
                &format!("└{dash_text}┘"),
                compute_card_border_style(notification_entry.notification_id),
            );
        }
        NotificationCenterRow::StatusText(text_row) => {
            write_text_clipped(
                screen_buffer,
                build_row_rect(
                    row_rect.origin.column + 1,
                    row_rect.origin.row,
                    card_column_count - 2,
                ),
                text_row,
                compute_unfocused_border_style(theme),
            );
        }
    }
}

/// Write the side borders `│` of a card on the first and the last cell of
/// the one-row `row_rect`, in `border_style`.
fn draw_card_side_borders(screen_buffer: &mut Buffer, row_rect: Rect, border_style: Style) {
    let last_column = row_rect.origin.column + row_rect.size.column_count - 1;
    for border_column in [row_rect.origin.column, last_column] {
        write_text_clipped(
            screen_buffer,
            build_row_rect(border_column, row_rect.origin.row, 1),
            "│",
            border_style,
        );
    }
}

/// Copy the cells of `box_buffer` that a slid box shows onto `drawn_area` of
/// `screen_buffer`, over the rows `drawn_rows` draws: box column
/// `first_drawn_box_column`, counted from the box buffer's left edge, lands
/// on `drawn_area`'s first column.
///
/// It first blanks `drawn_area` and the glyph that reaches into it from the
/// left ([`clear_overlay_area`]). A glyph that `drawn_area` cuts at its last
/// column is copied as a blank with the glyph's style, and a glyph it cuts at
/// its first column leaves a blank.
fn copy_slid_box(
    box_buffer: &Buffer,
    drawn_area: RatatuiRect,
    first_drawn_box_column: u16,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    clear_overlay_area(drawn_area, drawn_rows, screen_buffer);
    for screen_row in drawn_rows.list_rows_between(drawn_area.top(), drawn_area.bottom()) {
        let box_row = box_buffer.area.y + (screen_row - drawn_area.y);
        for drawn_column_index in 0..drawn_area.width {
            let Some(screen_cell) =
                screen_buffer.cell_mut((drawn_area.x + drawn_column_index, screen_row))
            else {
                continue;
            };
            *screen_cell = box_buffer[(
                box_buffer.area.x + first_drawn_box_column + drawn_column_index,
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

/// Draw the toast `shown_toast` at `toast_layout` over every cell drawn
/// before it: the part of the box in `toast_layout.drawn_rect`, from box
/// column `toast_layout.first_drawn_box_column`.
///
/// The box has a border ring in the focused border style, and its top border
/// holds ` ✕ ` in the close control's cells. Its first body row holds the
/// header ([`NotificationToastContent::format_header`](crate::notification::NotificationToastContent::format_header)) in the header's
/// severity style ([`compute_notification_severity_style`]), fitted to the
/// body's columns ([`write_text_fitted`]). The rows under it hold the rows
/// the text takes in a toast of the box's width
/// ([`NotificationToastView::compute_toast_text_rows`]). Text that needs more
/// rows than the box holds keeps the rows that fit, and its last row ends in
/// `…`.
///
/// The box is drawn whole into a box of its own, and the drawn part is copied
/// over the rows `drawn_rows` draws ([`copy_slid_box`]).
///
/// Example: a `26`-column toast of the `Error` `pane …a1b2 killed by signal 9`
/// reads `┌──────────────────── ✕ ─┐`, `│ [Koshi] Error          │`,
/// `│ pane …a1b2 killed by   │`, `│ signal 9               │` and
/// `└────────────────────────┘`.
pub(super) fn draw_notification_toast(
    toast_layout: &NotificationToastLayout,
    shown_toast: NotificationToastView<'_>,
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
        compute_toast_close_rect(box_rect),
        NOTIFICATION_TOAST_CLOSE_CONTROL_TEXT,
        border_style,
    );
    let body_column_count = box_size.column_count - 4;
    let toast_content = shown_toast.toast_content;
    write_text_fitted(
        &mut box_buffer,
        build_row_rect(2, 1, body_column_count),
        &toast_content.format_header(),
        compute_notification_severity_style(theme, toast_content.get_severity()),
    );
    let text_row_count = usize::from(box_size.row_count - 3);
    let toast_text_rows = shown_toast.compute_toast_text_rows(box_size.column_count);
    let text_rows = &toast_text_rows.text_rows;
    let has_more_rows = toast_text_rows.has_more_rows || text_rows.len() > text_row_count;
    for (text_row_index, text_row) in text_rows.iter().take(text_row_count).enumerate() {
        let row_rect = build_row_rect(2, 2 + text_row_index as u16, body_column_count);
        let is_cut_row = text_row_index + 1 == text_row_count && has_more_rows;
        if is_cut_row {
            write_text_ending_in_ellipsis(&mut box_buffer, row_rect, text_row, Style::default());
        } else {
            write_text_clipped(&mut box_buffer, row_rect, text_row, Style::default());
        }
    }

    copy_slid_box(
        &box_buffer,
        drawn_area,
        toast_layout.first_drawn_box_column,
        drawn_rows,
        screen_buffer,
    );
}

#[cfg(test)]
mod tests;
