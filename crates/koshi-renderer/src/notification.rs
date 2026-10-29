//! Where the notification center, the toast and the tabline's notification
//! button sit on screen, and the rows an entry's text wraps into.
//!
//! The center is docked on the right side of the pane area, as tall as the
//! pane area. It draws the floating chrome ([`crate::floating_chrome`]) with
//! the close button only, titled `Notifications`. Its body lists one card per
//! entry, newest first, and its last content row holds the label
//! `Clear all`. Each card's top border holds its dismiss button `✕`. A
//! `24`-column center:
//!
//! ```text
//! ┌──────────────────────┐   top border
//! │ Notifications     ✕  │   titlebar: title and close button
//! ├──────────────────────┤   separator
//! │┌──────────────── ✕ ─┐│   a card's top border and its dismiss button
//! ││ [Koshi] Error ×2   ││   the card's header
//! ││ pane …a1b2 killed  ││   the entry's text, wrapped
//! ││ by signal 9        ││
//! │└────────────────────┘│   the card's bottom border
//! │ 3 older              │   the dropped-entries text
//! │ notifications were   │
//! │ dropped              │
//! │      Clear all       │   the clear-all label
//! └──────────────────────┘   bottom border
//! ```
//!
//! The center slides in from past the pane area's right edge when it opens,
//! and slides out again when it closes.
//!
//! The toast is a box of the same width in one corner of the pane area. Its
//! top border holds the close control, its first body row the entry's header,
//! and up to [`MAX_TOAST_TEXT_ROW_COUNT`] rows of its text sit under that row:
//!
//! ```text
//! ┌─────────────────────── ✕ ─┐
//! │ [Koshi] Error ×2          │
//! │ pane …a1b2 killed by      │
//! │ signal 9                  │
//! └───────────────────────────┘
//! ```
//!
//! Every rect here is in screen cells, `(0, 0)` at the viewport's top-left
//! cell.

use std::borrow::Cow;
use std::cell::{Ref, RefCell, RefMut};
use std::collections::{vec_deque, VecDeque};
use std::ops::{Index, Range};
use std::sync::Arc;

use koshi_config::types::ToastPosition;
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::notification::{
    NotificationEntry, NotificationId, NotificationSeverity, NotificationSource,
};
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;
use ratatui::text::Span;

use crate::floating_chrome::{
    compute_floating_content_rect, compute_floating_titlebar, FloatingTitlebarControls,
};
use crate::hit_test::{compute_frame_pane_area, compute_viewport_area};
use crate::render::{compute_symbol_display_width, get_text_width};
use crate::snapshot::{FrameLayout, NotificationButton};

/// The widest the center and the toast are: 48 columns.
const MAXIMUM_NOTIFICATION_PANEL_COLUMN_COUNT: u16 = 48;

/// The narrowest the center and the toast are on a pane area at least this
/// wide: 24 columns. A narrower pane area gives them its full width.
const MINIMUM_NOTIFICATION_PANEL_COLUMN_COUNT: u16 = 24;

/// The share of the pane area's width the center and the toast take, as the
/// divisor of that width: a tenth.
const NOTIFICATION_PANEL_PANE_AREA_DIVISOR: u16 = 10;

/// The smallest box the center is drawn in: 10 columns, the narrowest box
/// that draws its close button and a card, and 6 rows, its chrome, one body
/// row and the clear-all row.
const MINIMUM_NOTIFICATION_CENTER_SIZE: Size = Size {
    column_count: 10,
    row_count: 6,
};

/// The narrowest box the toast is drawn in: 10 columns.
const MINIMUM_NOTIFICATION_TOAST_COLUMN_COUNT: u16 = 10;

/// The most text rows a toast holds under its header row: 2.
pub const MAX_TOAST_TEXT_ROW_COUNT: u16 = 2;

/// The rows of a toast that hold no text: its top border, its header row and
/// its bottom border.
const TOAST_FRAME_ROW_COUNT: u16 = 3;

/// The border cells at each end of the toast's top border before its close
/// control: a corner and one `─`.
const BORDER_END_COLUMN_COUNT: u16 = 2;

/// The columns on each side of a toast's body row, and of a card's text row:
/// the border and one blank column.
const BODY_SIDE_COLUMN_COUNT: u16 = 2;

/// The rows of a card that hold no text: its top border, its header row and
/// its bottom border.
const CARD_FRAME_ROW_COUNT: usize = 3;

/// The column of a card's dismiss button `✕`, counted back from the card's
/// right edge: ` ✕ `, one `─` and the corner sit right of it.
const CARD_DISMISS_BUTTON_RIGHT_COLUMN_OFFSET: u16 = 4;

/// The center's title on its titlebar.
pub(crate) const NOTIFICATION_CENTER_TITLE_TEXT: &str = "Notifications";

/// The close control on the top border of the toast.
pub(crate) const NOTIFICATION_TOAST_CLOSE_CONTROL_TEXT: &str = " ✕ ";

/// The cells [`NOTIFICATION_TOAST_CLOSE_CONTROL_TEXT`] takes.
const NOTIFICATION_TOAST_CLOSE_CONTROL_COLUMN_COUNT: u16 = 3;

/// The label on the center's last content row that clears the center.
pub(crate) const NOTIFICATION_CLEAR_ALL_LABEL_TEXT: &str = "Clear all";

/// The body row of an empty center.
pub(crate) const NO_NOTIFICATIONS_TEXT: &str = "No notifications";

/// The columns of the center and the toast in a pane area
/// `pane_area_column_count` columns wide: a tenth of it, from 24 to 48
/// columns, and the full width below 24. `80` gives `24`, `300` gives `30`,
/// `600` gives `48`, and `20` gives `20`.
#[must_use]
pub fn compute_notification_panel_column_count(pane_area_column_count: u16) -> u16 {
    if pane_area_column_count < MINIMUM_NOTIFICATION_PANEL_COLUMN_COUNT {
        return pane_area_column_count;
    }
    (pane_area_column_count / NOTIFICATION_PANEL_PANE_AREA_DIVISOR).clamp(
        MINIMUM_NOTIFICATION_PANEL_COLUMN_COUNT,
        MAXIMUM_NOTIFICATION_PANEL_COLUMN_COUNT,
    )
}

/// Where the parts of an open notification center sit, in screen cells.
///
/// The parts are placed in `center_rect`, where the center rests. While the
/// center slides, it covers `drawn_rect`: the same box moved right, cut at
/// the pane area's right edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationCenterLayout {
    /// The center's box where it rests, docked at the pane area's right edge,
    /// its floating chrome included.
    pub center_rect: Rect,
    /// The screen cells the center covers this frame: `center_rect` moved
    /// right by the columns the slide hides, cut at the pane area's right
    /// edge. Equal to `center_rect` while the center is in place.
    pub drawn_rect: Rect,
    /// The titlebar cells the title `Notifications` may take. `None` when the
    /// titlebar has no title cell.
    pub title_rect: Option<Rect>,
    /// The cells the cards and the center's own text take: the content rows
    /// above the clear-all row, between the side borders.
    pub body_rect: Rect,
    /// The label `Clear all`, centered on the last content row and cut to
    /// it.
    pub clear_all_rect: Rect,
}

impl NotificationCenterLayout {
    /// The cell of the center at rest that the slide draws at `screen_point`,
    /// or `None` when `screen_point` lies outside `drawn_rect`. With the
    /// center at rest at `(56, 1)` and drawn 8 columns right of it,
    /// `(70, 5)` gives `(62, 5)`.
    #[must_use]
    pub fn find_resting_point(&self, screen_point: Point) -> Option<Point> {
        if !self.drawn_rect.is_point_inside(screen_point) {
            return None;
        }
        Some(Point {
            column: screen_point.column
                - (self.drawn_rect.origin.column - self.center_rect.origin.column),
            row: screen_point.row,
        })
    }
}

/// Where the open center of `frame_layout.viewer_chrome.notification_center`
/// sits.
///
/// The center rests on the right side of the pane area: it is
/// [`compute_notification_panel_column_count`] of the pane area's width wide,
/// and as tall as the pane area. Its titlebar holds the title cells and the
/// close button of the floating chrome ([`crate::floating_chrome`]). Its
/// content rows above the last one are the body, and the last content row
/// holds `Clear all`. Of its width, `hidden_per_mille` thousandths, rounded to
/// the nearest column, lie past the pane area's right edge and are not drawn.
///
/// `None` while the viewer's center is closed, when the viewport has no cell,
/// every pane is suppressed, or the box would be narrower than 10 columns or
/// shorter than 6 rows.
///
/// An `80x24` viewport with a pane area at `(0, 1)` `80x22` gives the center
/// at `(56, 1)` `24x22`, its title at `(58, 2)` `16x1`, its body at
/// `(57, 4)` `22x17` and `Clear all` at `(63, 21)` `9x1`. Half hidden, it draws its 12 left columns at `(68, 1)`.
#[must_use]
pub fn find_drawn_notification_center_layout(
    frame_layout: FrameLayout<'_>,
) -> Option<NotificationCenterLayout> {
    let notification_center = frame_layout.viewer_chrome.notification_center?;
    let pane_area = find_notification_pane_area(frame_layout)?;
    let center_column_count = compute_notification_panel_column_count(pane_area.width);
    if center_column_count < MINIMUM_NOTIFICATION_CENTER_SIZE.column_count
        || pane_area.height < MINIMUM_NOTIFICATION_CENTER_SIZE.row_count
    {
        return None;
    }
    let center_rect = Rect::from_origin_and_size(
        Point {
            column: pane_area.right() - center_column_count,
            row: pane_area.y,
        },
        Size {
            column_count: center_column_count,
            row_count: pane_area.height,
        },
    );
    let hidden_column_count =
        compute_hidden_column_count(center_column_count, notification_center.hidden_per_mille);
    let drawn_rect = Rect::from_origin_and_size(
        Point {
            column: center_rect.origin.column + hidden_column_count,
            row: center_rect.origin.row,
        },
        Size {
            column_count: center_column_count - hidden_column_count,
            row_count: center_rect.size.row_count,
        },
    );
    let titlebar =
        compute_floating_titlebar(center_rect, FloatingTitlebarControls::CLOSE_BUTTON_ONLY);
    let content_rect = compute_floating_content_rect(center_rect);
    let clear_all_row = content_rect.origin.row + content_rect.size.row_count - 1;
    let clear_all_column_count =
        get_text_width(NOTIFICATION_CLEAR_ALL_LABEL_TEXT).min(content_rect.size.column_count);
    Some(NotificationCenterLayout {
        center_rect,
        drawn_rect,
        title_rect: titlebar.title_rect,
        body_rect: Rect::from_origin_and_size(
            content_rect.origin,
            Size {
                column_count: content_rect.size.column_count,
                row_count: content_rect.size.row_count - 1,
            },
        ),
        clear_all_rect: build_row_rect(
            content_rect.origin.column
                + (content_rect.size.column_count - clear_all_column_count) / 2,
            clear_all_row,
            clear_all_column_count,
        ),
    })
}

/// Where a toast sits and which of its cells one frame draws, in screen
/// cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationToastLayout {
    /// The toast's box once it has slid in, border ring included.
    pub toast_rect: Rect,
    /// The screen cells the toast covers this frame: the part of the box,
    /// moved toward its side edge by the hidden columns, that lies inside the
    /// pane area.
    pub drawn_rect: Rect,
    /// The column of the box, counted from its left edge, that this frame
    /// draws on `drawn_rect`'s first column.
    pub first_drawn_box_column: u16,
    /// The close control ` ✕ ` on the top border, where this frame draws it.
    /// `None` while any of its cells lies past the side edge.
    pub close_rect: Option<Rect>,
}

/// Whether `frame_layout` has room for the box of the toast
/// `frame_layout.viewer_chrome.notification_toast` in any slide position:
/// the viewport has a cell, a pane shows, and the pane area holds a box
/// [`compute_notification_panel_column_count`] columns wide, at least 10,
/// and tall enough for the toast's border rows, its header row and the first
/// row of its text. That is 4 rows for a toast with a text row, and 3 rows
/// for a toast with none and while no toast shows. The toast's clock runs
/// only while this holds.
///
/// Example: an `80x5` viewport, a pane area 3 rows tall, has room for a
/// toast of no text row and no room for a toast of 1 text row.
#[must_use]
pub fn has_notification_toast_room(frame_layout: FrameLayout<'_>) -> bool {
    let first_text_row_count = frame_layout
        .viewer_chrome
        .notification_toast
        .map_or(0, |notification_toast| {
            notification_toast.text_row_count.min(1)
        });
    find_notification_pane_area(frame_layout).is_some_and(|pane_area| {
        compute_notification_panel_column_count(pane_area.width)
            >= MINIMUM_NOTIFICATION_TOAST_COLUMN_COUNT
            && pane_area.height >= TOAST_FRAME_ROW_COUNT + first_text_row_count
    })
}

/// Where the toast of `frame_layout.viewer_chrome.notification_toast` sits.
///
/// The box is as wide as the center, and as tall as its text rows, its header
/// row and its two border rows. A pane area too short for every text row
/// keeps the text rows that fit. The box sits in the corner of the pane area
/// its `toast_position` names. Of its width, `hidden_per_mille` thousandths,
/// rounded to the nearest column, lie past the side edge of that corner: past
/// the right edge for a right corner, and past the left edge for a left
/// corner. Those columns are not drawn.
///
/// `None` while the viewer shows no toast, when the frame has no room for the
/// toast ([`has_notification_toast_room`]), or every column of the box lies
/// past the edge.
///
/// A toast of no text row at `top-right` over a pane area at `(0, 1)` `80x22`
/// sits at `(56, 1)` `24x3`. Half hidden, it draws its 12 left columns at
/// `(68, 1)`, and its close control lies past the edge. A toast of 2 text rows
/// over a pane area 4 rows tall keeps 1 text row.
#[must_use]
pub fn find_drawn_notification_toast_layout(
    frame_layout: FrameLayout<'_>,
) -> Option<NotificationToastLayout> {
    let notification_toast = frame_layout.viewer_chrome.notification_toast?;
    if !has_notification_toast_room(frame_layout) {
        return None;
    }
    let pane_area = find_notification_pane_area(frame_layout)?;
    let toast_column_count = compute_notification_panel_column_count(pane_area.width);
    let toast_row_count = notification_toast
        .text_row_count
        .min(pane_area.height - TOAST_FRAME_ROW_COUNT)
        + TOAST_FRAME_ROW_COUNT;
    let is_right_corner = matches!(
        notification_toast.toast_position,
        ToastPosition::TopRight | ToastPosition::BottomRight
    );
    let is_top_corner = matches!(
        notification_toast.toast_position,
        ToastPosition::TopRight | ToastPosition::TopLeft
    );
    let toast_rect = Rect::from_origin_and_size(
        Point {
            column: if is_right_corner {
                pane_area.right() - toast_column_count
            } else {
                pane_area.x
            },
            row: if is_top_corner {
                pane_area.y
            } else {
                pane_area.bottom() - toast_row_count
            },
        },
        Size {
            column_count: toast_column_count,
            row_count: toast_row_count,
        },
    );
    let hidden_column_count =
        compute_hidden_column_count(toast_column_count, notification_toast.hidden_per_mille);
    let drawn_column_count = toast_column_count - hidden_column_count;
    if drawn_column_count == 0 {
        return None;
    }
    let (drawn_start_column, first_drawn_box_column) = if is_right_corner {
        (toast_rect.origin.column + hidden_column_count, 0)
    } else {
        (toast_rect.origin.column, hidden_column_count)
    };
    let drawn_rect = Rect::from_origin_and_size(
        Point {
            column: drawn_start_column,
            row: toast_rect.origin.row,
        },
        Size {
            column_count: drawn_column_count,
            row_count: toast_row_count,
        },
    );
    let box_close_rect = compute_toast_close_rect(toast_rect);
    let box_close_start_column = box_close_rect.origin.column - toast_rect.origin.column;
    let is_close_drawn = box_close_start_column >= first_drawn_box_column
        && box_close_start_column + NOTIFICATION_TOAST_CLOSE_CONTROL_COLUMN_COUNT
            <= first_drawn_box_column + drawn_column_count;
    Some(NotificationToastLayout {
        toast_rect,
        drawn_rect,
        first_drawn_box_column,
        close_rect: is_close_drawn.then(|| {
            build_row_rect(
                drawn_start_column + box_close_start_column - first_drawn_box_column,
                toast_rect.origin.row,
                NOTIFICATION_TOAST_CLOSE_CONTROL_COLUMN_COUNT,
            )
        }),
    })
}

/// The columns of a box `box_column_count` columns wide that lie past its
/// side edge when `hidden_per_mille` thousandths of it do, rounded to the
/// nearest column. A value above `1000` counts as `1000`. `24` columns at
/// `500` gives `12`, and at `521` gives `13`.
fn compute_hidden_column_count(box_column_count: u16, hidden_per_mille: u16) -> u16 {
    ((u32::from(box_column_count) * u32::from(hidden_per_mille.min(1000)) + 500) / 1000) as u16
}

/// The tabline's notification button text: ` ● ` with no unread entry, ` ●3 `
/// with 3, and ` ●99+ ` with more than 99. `●` (`U+25CF`) has East Asian
/// ambiguous width, and koshi draws it one cell wide.
#[must_use]
pub fn format_notification_button_text(
    notification_button: NotificationButton,
) -> Cow<'static, str> {
    match notification_button.unread_entry_count {
        0 => Cow::Borrowed(" ● "),
        unread_entry_count @ 1..=99 => Cow::Owned(format!(" ●{unread_entry_count} ")),
        _ => Cow::Borrowed(" ●99+ "),
    }
}

/// The label of the source that wrote an entry: `[Koshi]` for koshi itself.
#[must_use]
pub fn format_notification_source_label(notification_source: NotificationSource) -> &'static str {
    match notification_source {
        NotificationSource::Core => "[Koshi]",
    }
}

/// The word for a severity: `Info`, `Warning` or `Error`.
#[must_use]
pub fn format_notification_severity(notification_severity: NotificationSeverity) -> &'static str {
    match notification_severity {
        NotificationSeverity::Info => "Info",
        NotificationSeverity::Warning => "Warning",
        NotificationSeverity::Error => "Error",
    }
}

/// An entry's header: its source label and severity, then `×N` when `N`
/// posts merged into it. `[Koshi] Error` for one post, and
/// `[Koshi] Warning ×3` for three.
#[must_use]
pub fn format_notification_header(notification_entry: &NotificationEntry) -> String {
    let header_text = format!(
        "{} {}",
        format_notification_source_label(notification_entry.source),
        format_notification_severity(notification_entry.severity)
    );
    if notification_entry.repeat_count > 1 {
        format!("{header_text} ×{}", notification_entry.repeat_count)
    } else {
        header_text
    }
}

/// The text of a toast that counts `notification_count` entries the toast
/// queue did not preview: `36 more notifications in the center`, and
/// `1 more notification in the center`.
#[must_use]
pub fn format_toast_summary_text(notification_count: usize) -> String {
    if notification_count == 1 {
        "1 more notification in the center".to_string()
    } else {
        format!("{notification_count} more notifications in the center")
    }
}

/// The text of the center's row that counts `dropped_entry_count` entries the
/// viewer can no longer show: `3 older notifications were dropped`, and
/// `1 older notification was dropped`.
#[must_use]
pub fn format_dropped_notifications_text(dropped_entry_count: u64) -> String {
    if dropped_entry_count == 1 {
        "1 older notification was dropped".to_string()
    } else {
        format!("{dropped_entry_count} older notifications were dropped")
    }
}

/// The rows `text` takes in rows `column_count` cells wide, top first.
///
/// The words of `text` are its runs of characters that are neither a space nor
/// a control character. Each row holds the most words that fit, one space
/// between two words. A word that does not fit after the words of its row
/// starts the next row. A word wider than a row is cut between two graphemes
/// where the row is full, and a grapheme wider than a whole row takes a row
/// alone. Text with no word takes no row.
///
/// In 10 columns, `pane …a1b2 killed by signal 9` gives `pane …a1b2`,
/// `killed by`, `signal 9`, and `abcdefghijkl` gives `abcdefghij`, `kl`. In 1
/// column, `世界` gives `世`, `界`, and `e◌́x` (`e` `U+0301` `x`) gives `é`,
/// `x`.
#[must_use]
pub fn wrap_text_rows(text: &str, column_count: u16) -> Vec<String> {
    wrap_text_rows_up_to(text, column_count, usize::MAX)
}

/// The first `max_row_count` rows [`wrap_text_rows`] gives for `text` in rows
/// `column_count` cells wide. The wrap stops at the row `max_row_count` ends,
/// and reads none of the text after it. With a `max_row_count` of 2, in 10
/// columns, `pane …a1b2 killed by signal 9` gives `pane …a1b2`, `killed by`.
fn wrap_text_rows_up_to(text: &str, column_count: u16, max_row_count: usize) -> Vec<String> {
    let mut text_rows = Vec::new();
    let mut row_text = String::new();
    let mut row_column_count: u16 = 0;
    let words = text.split(|character: char| character == ' ' || character.is_control());
    for word in words.filter(|word| !word.is_empty()) {
        let word_column_count = get_text_width(word);
        if !row_text.is_empty() {
            if row_column_count.saturating_add(1 + word_column_count) <= column_count {
                row_text.push(' ');
                row_text.push_str(word);
                row_column_count += 1 + word_column_count;
                continue;
            }
            text_rows.push(std::mem::take(&mut row_text));
            if text_rows.len() == max_row_count {
                return text_rows;
            }
            row_column_count = 0;
        }
        for styled_grapheme in Span::raw(word).styled_graphemes(Style::default()) {
            let grapheme_column_count = compute_symbol_display_width(styled_grapheme.symbol);
            if !row_text.is_empty()
                && row_column_count.saturating_add(grapheme_column_count) > column_count
            {
                text_rows.push(std::mem::take(&mut row_text));
                if text_rows.len() == max_row_count {
                    return text_rows;
                }
                row_column_count = 0;
            }
            row_text.push_str(styled_grapheme.symbol);
            row_column_count = row_column_count.saturating_add(grapheme_column_count);
        }
    }
    if !row_text.is_empty() {
        text_rows.push(row_text);
    }
    text_rows
}

/// The rows a toast's text takes under its header row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToastTextRows {
    /// The first rows [`wrap_text_rows`] gives in the toast's body columns,
    /// top first: at most [`MAX_TOAST_TEXT_ROW_COUNT`].
    pub text_rows: Vec<String>,
    /// Whether the text wraps to more rows than `text_rows` holds. The toast
    /// then ends its last row in `…`.
    pub has_more_rows: bool,
}

impl ToastTextRows {
    /// The rows `text` takes under the header row of a toast
    /// `toast_column_count` columns wide. The wrap reads `text` only up to
    /// the end of the row after the last row the toast holds.
    ///
    /// Example: in a `14`-column toast, `pane …a1b2 killed by signal 9` gives
    /// `pane …a1b2`, `killed by` and more rows, and `pane …a1b2 exited 2`
    /// gives `pane …a1b2`, `exited 2` and no more row.
    #[must_use]
    pub fn from_text(text: &str, toast_column_count: u16) -> Self {
        let body_column_count = toast_column_count.saturating_sub(2 * BODY_SIDE_COLUMN_COUNT);
        let shown_row_count = usize::from(MAX_TOAST_TEXT_ROW_COUNT);
        let mut text_rows = wrap_text_rows_up_to(text, body_column_count, shown_row_count + 1);
        let has_more_rows = text_rows.len() > shown_row_count;
        text_rows.truncate(shown_row_count);
        ToastTextRows {
            text_rows,
            has_more_rows,
        }
    }
}

/// The text, toast width and rows of the last ask of a [`ToastTextRowCache`].
#[derive(Debug, Clone, Default)]
struct CachedToastTextRows {
    /// The text asked for.
    text: String,
    /// The toast width asked for, in columns.
    toast_column_count: u16,
    /// The rows of `text` in a toast `toast_column_count` columns wide.
    toast_text_rows: ToastTextRows,
}

/// The rows of the last toast text asked for, kept for the next ask with the
/// same text and toast width. A new cache holds the rows of an empty text,
/// which takes no row.
#[derive(Debug, Clone, Default)]
pub struct ToastTextRowCache {
    /// The last ask and its rows.
    cached_text_rows: RefCell<CachedToastTextRows>,
}

impl ToastTextRowCache {
    /// The rows `text` takes under the header row of a toast
    /// `toast_column_count` columns wide ([`ToastTextRows::from_text`]). The
    /// text is wrapped only when the last ask had another text or another
    /// width.
    ///
    /// Example: a toast that slides for one second asks 20 times for the same
    /// text at the same width, and the text is wrapped once.
    #[must_use]
    pub fn compute_toast_text_rows(
        &self,
        text: &str,
        toast_column_count: u16,
    ) -> Ref<'_, ToastTextRows> {
        let is_cached = {
            let cached_text_rows = self.cached_text_rows.borrow();
            cached_text_rows.toast_column_count == toast_column_count
                && cached_text_rows.text == text
        };
        if !is_cached {
            *self.cached_text_rows.borrow_mut() = CachedToastTextRows {
                text: text.to_owned(),
                toast_column_count,
                toast_text_rows: ToastTextRows::from_text(text, toast_column_count),
            };
        }
        Ref::map(self.cached_text_rows.borrow(), |cached_text_rows| {
            &cached_text_rows.toast_text_rows
        })
    }
}

/// One entry the notification center lists, and the rows its text wraps to
/// ([`wrap_text_rows`]) in the text width last asked for.
///
/// The entry does not change after this type takes it. The rows are kept for
/// one text width: asking again for that width reads them, and asking for
/// another width wraps the text again and keeps the new rows.
#[derive(Debug, Clone)]
pub struct NotificationCenterEntry {
    /// The entry.
    notification_entry: NotificationEntry,
    /// The last rows taken, or `None` before any were taken.
    wrapped_text: RefCell<Option<WrappedEntryText>>,
}

/// Two entries are equal when their entries are equal, whatever rows each one
/// keeps.
impl PartialEq for NotificationCenterEntry {
    fn eq(&self, other: &Self) -> bool {
        self.notification_entry == other.notification_entry
    }
}

impl Eq for NotificationCenterEntry {}

/// The rows an entry's text wraps to in rows `text_column_count` cells wide.
#[derive(Debug, Clone)]
struct WrappedEntryText {
    /// The text width the text was wrapped at.
    text_column_count: u16,
    /// The rows the text wrapped to, top first.
    text_rows: Vec<String>,
}

impl NotificationCenterEntry {
    /// `notification_entry`, with no rows taken.
    #[must_use]
    pub fn from_notification_entry(notification_entry: NotificationEntry) -> Self {
        NotificationCenterEntry {
            notification_entry,
            wrapped_text: RefCell::new(None),
        }
    }

    /// The entry.
    #[must_use]
    pub fn get_notification_entry(&self) -> &NotificationEntry {
        &self.notification_entry
    }

    /// The rows the entry's text wraps to in rows `text_column_count` cells
    /// wide ([`wrap_text_rows`]), top first. The text is wrapped only when the
    /// last rows were taken at another width or none were taken.
    ///
    /// Example: `pane …a1b2 killed by signal 9` gives `pane …a1b2`,
    /// `killed by` and `signal 9` at `10` columns. A second ask at `10` reads
    /// those rows with no wrap, and an ask at `40` wraps again and gives one
    /// row.
    #[must_use]
    pub fn list_text_rows(&self, text_column_count: u16) -> Ref<'_, [String]> {
        let is_wrapped = self
            .wrapped_text
            .borrow()
            .as_ref()
            .is_some_and(|wrapped_text| wrapped_text.text_column_count == text_column_count);
        if !is_wrapped {
            *self.wrapped_text.borrow_mut() = Some(WrappedEntryText {
                text_column_count,
                text_rows: wrap_text_rows(&self.notification_entry.text, text_column_count),
            });
        }
        Ref::map(
            self.wrapped_text.borrow(),
            |wrapped_text| match wrapped_text {
                Some(wrapped_text) => wrapped_text.text_rows.as_slice(),
                None => &[],
            },
        )
    }

    /// The rows the entry's card takes in a body `body_column_count` columns
    /// wide: its top border, its header row, its text rows
    /// ([`list_text_rows`](Self::list_text_rows) in the card's text width) and
    /// its bottom border. `pane …a1b2 killed by signal 9` gives `5` in a
    /// `22`-column body.
    #[must_use]
    pub fn count_card_rows(&self, body_column_count: u16) -> usize {
        CARD_FRAME_ROW_COUNT
            + self
                .list_text_rows(compute_card_text_column_count(body_column_count))
                .len()
    }
}

/// Every entry the notification center lists, oldest first, with the rows the
/// center counted and drew from them.
///
/// [`add_newest_center_entry`](Self::add_newest_center_entry),
/// [`remove_center_entry_at`](Self::remove_center_entry_at) and
/// [`clear_center_entries`](Self::clear_center_entries) are the only changes
/// to the entries. Each one drops the counted rows and the kept body rows.
#[derive(Debug, Clone, Default)]
pub struct NotificationCenterEntries {
    /// The entries, oldest first.
    center_entries: VecDeque<NotificationCenterEntry>,
    /// The rows of the newest cards and of the center's own text, counted as
    /// far as an ask needed.
    counted_center_rows: RefCell<CountedCenterRows>,
    /// The body rows last worked out, or `None` before the first and after
    /// each change to the entries.
    center_body_rows: RefCell<Option<NotificationCenterBodyRows>>,
}

/// The rows the center counted in a body `body_column_count` columns wide:
/// the newest cards, newest first, and the center's own text.
///
/// Example: in a `22`-column body, cards of 4, 5 and 4 rows, newest first,
/// counted in full give `card_row_ends` `[4, 9, 13]`.
#[derive(Debug, Clone, Default)]
struct CountedCenterRows {
    /// The body width the rows were counted in.
    body_column_count: u16,
    /// The body row after each counted card, newest card first: item `n`
    /// holds the rows of the newest `n + 1` cards.
    card_row_ends: Vec<usize>,
    /// The rows of the center's own text, or `None` before they were
    /// counted.
    status_rows: Option<CountedStatusRows>,
}

/// The rows the center's own text takes for one dropped count.
#[derive(Debug, Clone, Copy)]
struct CountedStatusRows {
    /// The dropped count the text was counted for.
    dropped_entry_count: u64,
    /// The rows the text wraps to.
    status_row_count: usize,
}

impl NotificationCenterEntries {
    /// How many entries the center lists.
    #[must_use]
    pub fn count_center_entries(&self) -> usize {
        self.center_entries.len()
    }

    /// Whether the center lists no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.center_entries.is_empty()
    }

    /// The entries, oldest first.
    pub fn list_center_entries(&self) -> vec_deque::Iter<'_, NotificationCenterEntry> {
        self.center_entries.iter()
    }

    /// The entry at `entry_index`, oldest first, or `None` past the newest
    /// entry.
    #[must_use]
    pub fn find_center_entry_at(&self, entry_index: usize) -> Option<&NotificationCenterEntry> {
        self.center_entries.get(entry_index)
    }

    /// The newest entry, or `None` with no entry.
    #[must_use]
    pub fn find_newest_center_entry(&self) -> Option<&NotificationCenterEntry> {
        self.center_entries.back()
    }

    /// Add `center_entry` as the newest entry, and drop the counted rows and
    /// the kept body rows.
    pub fn add_newest_center_entry(&mut self, center_entry: NotificationCenterEntry) {
        self.center_entries.push_back(center_entry);
        self.clear_counted_rows();
    }

    /// Remove the entry at `entry_index`, oldest first, and return it, or
    /// `None` past the newest entry. A removed entry drops the counted rows
    /// and the kept body rows.
    pub fn remove_center_entry_at(
        &mut self,
        entry_index: usize,
    ) -> Option<NotificationCenterEntry> {
        let removed_entry = self.center_entries.remove(entry_index)?;
        self.clear_counted_rows();
        Some(removed_entry)
    }

    /// Remove every entry, and drop the counted rows and the kept body rows.
    pub fn clear_center_entries(&mut self) {
        self.center_entries.clear();
        self.clear_counted_rows();
    }

    /// Drop the counted rows and the kept body rows.
    fn clear_counted_rows(&mut self) {
        *self.counted_center_rows.get_mut() = CountedCenterRows::default();
        *self.center_body_rows.get_mut() = None;
    }

    /// The counted rows for a body `body_column_count` columns wide. Rows
    /// counted in another width are dropped first.
    fn borrow_counted_center_rows(&self, body_column_count: u16) -> RefMut<'_, CountedCenterRows> {
        let mut counted_center_rows = self.counted_center_rows.borrow_mut();
        if counted_center_rows.body_column_count != body_column_count {
            *counted_center_rows = CountedCenterRows {
                body_column_count,
                ..CountedCenterRows::default()
            };
        }
        counted_center_rows
    }

    /// The body row after each of the newest cards in a body
    /// `body_column_count` columns wide, newest card first
    /// ([`NotificationCenterEntry::count_card_rows`]).
    ///
    /// Cards are counted, newest first, while fewer than `card_count` cards
    /// are counted and the counted cards take fewer than `row_count` rows.
    /// Counted cards stay counted until the entries or the body width change,
    /// and the next ask counts only the cards past them.
    ///
    /// Example: with cards of 4 rows, an ask for `9` rows counts the newest 3
    /// cards and gives `[4, 8, 12]`.
    fn list_card_row_ends(
        &self,
        body_column_count: u16,
        card_count: usize,
        row_count: usize,
    ) -> Ref<'_, [usize]> {
        {
            let mut counted_center_rows = self.borrow_counted_center_rows(body_column_count);
            let card_row_ends = &mut counted_center_rows.card_row_ends;
            let card_count = card_count.min(self.center_entries.len());
            let mut last_card_row_end = card_row_ends.last().copied().unwrap_or(0);
            while card_row_ends.len() < card_count && last_card_row_end < row_count {
                let entry_index = self.center_entries.len() - 1 - card_row_ends.len();
                last_card_row_end +=
                    self.center_entries[entry_index].count_card_rows(body_column_count);
                card_row_ends.push(last_card_row_end);
            }
        }
        Ref::map(self.counted_center_rows.borrow(), |counted_center_rows| {
            counted_center_rows.card_row_ends.as_slice()
        })
    }

    /// Where the center's body starts when it draws from body row
    /// `first_row_index` in a body `body_column_count` columns wide: after
    /// the cards that end on or above that row, newest first, and after the
    /// rows of the next card, or of the center's own text, above it. The
    /// newest cards are counted only through the card that holds that row
    /// ([`list_card_row_ends`](Self::list_card_row_ends)).
    ///
    /// Example: from row `7`, a body whose newest card takes 5 rows skips that
    /// card and the first 2 rows of the card after it.
    fn compute_center_row_start(
        &self,
        body_column_count: u16,
        first_row_index: usize,
    ) -> NotificationCenterRowStart {
        let card_row_ends = self.list_card_row_ends(
            body_column_count,
            self.center_entries.len(),
            first_row_index.saturating_add(1),
        );
        let skipped_entry_count =
            card_row_ends.partition_point(|&card_row_end| card_row_end <= first_row_index);
        let skipped_card_row_count = if skipped_entry_count == 0 {
            0
        } else {
            card_row_ends[skipped_entry_count - 1]
        };
        NotificationCenterRowStart {
            skipped_entry_count,
            skipped_row_count: first_row_index - skipped_card_row_count,
        }
    }
}

/// The entry at `entry_index`, oldest first. Panics past the newest entry.
impl Index<usize> for NotificationCenterEntries {
    type Output = NotificationCenterEntry;

    fn index(&self, entry_index: usize) -> &NotificationCenterEntry {
        &self.center_entries[entry_index]
    }
}

/// What one viewer's notification center and toast draw.
#[derive(Debug, Clone, Copy)]
pub struct NotificationView<'a> {
    /// Every entry the center lists, oldest first, with the rows the center
    /// counted and drew from them.
    pub center_entries: &'a NotificationCenterEntries,
    /// How many entries the viewer can no longer show: the center's
    /// dropped-entries row counts them.
    pub dropped_entry_count: u64,
    /// The toast the viewer shows, or `None` while no toast shows.
    pub shown_toast: Option<NotificationToastView<'a>>,
}

impl NotificationView<'_> {
    /// The body row the center draws from when its body is `body_rect`'s size
    /// and its scroll is `scroll_row_offset`: `scroll_row_offset`, cut to the
    /// row that puts the center's last row on the body's last row. A
    /// `scroll_row_offset` of `0` gives `0` and counts no row.
    ///
    /// The cards are counted, newest first, only until they fill the body
    /// below `scroll_row_offset`, and they stay counted until the entries or
    /// the body width change ([`NotificationCenterEntries`]). The center's own
    /// text is counted only when every card is counted and the cards do not
    /// fill the body.
    ///
    /// Example: 30 cards of 4 rows in a `22x17` body give `7` for `7` and
    /// count the newest 6 cards, and give `103` for `500`.
    #[must_use]
    pub fn compute_drawn_center_scroll_row_offset(
        &self,
        body_rect: Rect,
        scroll_row_offset: usize,
    ) -> usize {
        if scroll_row_offset == 0 {
            return 0;
        }
        let body_column_count = body_rect.size.column_count;
        let body_row_count = usize::from(body_rect.size.row_count);
        let filled_row_count = scroll_row_offset.saturating_add(body_row_count);
        let card_row_count = self
            .center_entries
            .list_card_row_ends(
                body_column_count,
                self.center_entries.count_center_entries(),
                filled_row_count,
            )
            .last()
            .copied()
            .unwrap_or(0);
        if card_row_count >= filled_row_count {
            return scroll_row_offset;
        }
        let center_row_count = card_row_count + self.count_center_status_rows(body_column_count);
        scroll_row_offset.min(center_row_count.saturating_sub(body_row_count))
    }

    /// The rows the center's own text ([`format_center_status_text`]) wraps to
    /// in a body `body_column_count` columns wide. The text is wrapped only
    /// when the last count had another body width or dropped count, or when
    /// the entries changed after it.
    ///
    /// Example: `3 older notifications were dropped` takes 3 rows in a
    /// `22`-column body.
    fn count_center_status_rows(&self, body_column_count: u16) -> usize {
        let mut counted_center_rows = self
            .center_entries
            .borrow_counted_center_rows(body_column_count);
        if let Some(status_rows) = counted_center_rows.status_rows {
            if status_rows.dropped_entry_count == self.dropped_entry_count {
                return status_rows.status_row_count;
            }
        }
        let status_row_count = wrap_text_rows(
            &format_center_status_text(*self),
            compute_status_text_column_count(body_column_count),
        )
        .len();
        counted_center_rows.status_rows = Some(CountedStatusRows {
            dropped_entry_count: self.dropped_entry_count,
            status_row_count,
        });
        status_row_count
    }

    /// The rows the center's body draws when the body is `body_rect`'s size
    /// and its scroll is `scroll_row_offset`, from the body row it draws from
    /// ([`compute_drawn_center_scroll_row_offset`](Self::compute_drawn_center_scroll_row_offset)).
    ///
    /// The rows are worked out only when the last ask had another body size,
    /// scroll or dropped count, or when the entries changed after it
    /// ([`NotificationCenterEntries`]). Every other ask reads the kept rows.
    ///
    /// Example: pane output that redraws an open center 120 times a second,
    /// with no change to its entries, its body or its scroll, asks 120 times
    /// and works out the rows once.
    #[must_use]
    pub fn compute_center_body_rows(
        &self,
        body_rect: Rect,
        scroll_row_offset: usize,
    ) -> NotificationCenterBodyRows {
        if let Some(body_rows) = self.center_entries.center_body_rows.borrow().as_ref() {
            if body_rows.body_size == body_rect.size
                && body_rows.scroll_row_offset == scroll_row_offset
                && body_rows.dropped_entry_count == self.dropped_entry_count
            {
                return body_rows.clone();
            }
        }
        let drawn_scroll_row_offset =
            self.compute_drawn_center_scroll_row_offset(body_rect, scroll_row_offset);
        let body_column_count = body_rect.size.column_count;
        let row_start = self
            .center_entries
            .compute_center_row_start(body_column_count, drawn_scroll_row_offset);
        let card_rows = list_notification_center_rows(*self, body_column_count, row_start)
            .take(usize::from(body_rect.size.row_count))
            .map(|center_row| match center_row {
                NotificationCenterRow::CardTopBorder(notification_entry) => {
                    Some(NotificationCardRow {
                        notification_id: notification_entry.notification_id,
                        is_top_border: true,
                    })
                }
                NotificationCenterRow::CardHeader(notification_entry)
                | NotificationCenterRow::CardBottomBorder(notification_entry) => {
                    Some(NotificationCardRow {
                        notification_id: notification_entry.notification_id,
                        is_top_border: false,
                    })
                }
                NotificationCenterRow::CardText { center_entry, .. } => Some(NotificationCardRow {
                    notification_id: center_entry.get_notification_entry().notification_id,
                    is_top_border: false,
                }),
                NotificationCenterRow::StatusText(_) => None,
            })
            .collect();
        let body_rows = NotificationCenterBodyRows {
            body_size: body_rect.size,
            scroll_row_offset,
            dropped_entry_count: self.dropped_entry_count,
            row_start,
            card_rows,
        };
        *self.center_entries.center_body_rows.borrow_mut() = Some(body_rows.clone());
        body_rows
    }
}

/// One toast a viewer shows: what it shows, and the cache of the rows its
/// text takes.
#[derive(Debug, Clone, Copy)]
pub struct NotificationToastView<'a> {
    /// What the toast shows.
    pub toast_content: NotificationToastContent<'a>,
    /// The rows of the toast's text last asked for.
    pub toast_text_row_cache: &'a ToastTextRowCache,
}

impl NotificationToastView<'_> {
    /// The rows the toast's text ([`NotificationToastContent::format_text`])
    /// takes under its header row in a toast `toast_column_count` columns
    /// wide, through the toast's cache
    /// ([`ToastTextRowCache::compute_toast_text_rows`]).
    #[must_use]
    pub fn compute_toast_text_rows(&self, toast_column_count: u16) -> Ref<'_, ToastTextRows> {
        self.toast_text_row_cache
            .compute_toast_text_rows(&self.toast_content.format_text(), toast_column_count)
    }
}

/// What one toast shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationToastContent<'a> {
    /// One entry: its header and its text.
    Entry(&'a NotificationEntry),
    /// A count of entries that took no toast of their own, as `[Koshi]` at the
    /// loudest severity among them: `36 more notifications in the center`.
    Summary {
        /// How many entries the summary counts.
        notification_count: usize,
        /// The loudest severity among those entries.
        severity: NotificationSeverity,
    },
}

impl NotificationToastContent<'_> {
    /// The toast's header: the entry's [`format_notification_header`], and
    /// `[Koshi] Error` for a summary at `Error`.
    #[must_use]
    pub fn format_header(&self) -> String {
        match self {
            NotificationToastContent::Entry(toast_entry) => format_notification_header(toast_entry),
            NotificationToastContent::Summary { severity, .. } => format!(
                "{} {}",
                format_notification_source_label(NotificationSource::Core),
                format_notification_severity(*severity)
            ),
        }
    }

    /// The toast's text: the entry's text, borrowed, and the
    /// [`format_toast_summary_text`] of a summary.
    #[must_use]
    pub fn format_text(&self) -> Cow<'_, str> {
        match self {
            NotificationToastContent::Entry(toast_entry) => Cow::Borrowed(&toast_entry.text),
            NotificationToastContent::Summary {
                notification_count, ..
            } => Cow::Owned(format_toast_summary_text(*notification_count)),
        }
    }

    /// The severity whose style the toast's header takes.
    #[must_use]
    pub fn get_severity(&self) -> NotificationSeverity {
        match self {
            NotificationToastContent::Entry(toast_entry) => toast_entry.severity,
            NotificationToastContent::Summary { severity, .. } => *severity,
        }
    }
}

/// The cells of a card's text, and of its header, in a body
/// `body_column_count` columns wide: the body less the card's border and one
/// blank column on each side. `22` gives `18`.
#[must_use]
pub fn compute_card_text_column_count(body_column_count: u16) -> u16 {
    body_column_count.saturating_sub(2 * BODY_SIDE_COLUMN_COUNT)
}

/// The cells of the center's own text in a body `body_column_count` columns
/// wide: the body less one blank column on each side. `22` gives `20`.
fn compute_status_text_column_count(body_column_count: u16) -> u16 {
    body_column_count.saturating_sub(2)
}

/// One row of the center's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationCenterRow<'a> {
    /// The top border of an entry's card, which holds its dismiss button.
    CardTopBorder(&'a NotificationEntry),
    /// The header row of an entry's card ([`format_notification_header`]).
    CardHeader(&'a NotificationEntry),
    /// One row of the text of an entry's card: the row at `text_row_index`
    /// of [`NotificationCenterEntry::list_text_rows`].
    CardText {
        /// The entry whose text the row holds.
        center_entry: &'a NotificationCenterEntry,
        /// The row's index among the entry's text rows, from `0`.
        text_row_index: usize,
    },
    /// The bottom border of an entry's card.
    CardBottomBorder(&'a NotificationEntry),
    /// One row of the center's own text, drawn dim: the count of the entries
    /// the viewer can no longer show ([`format_dropped_notifications_text`]),
    /// or `No notifications` in a center with no entry and nothing dropped.
    StatusText(String),
}

/// Where the center's body starts drawing: after the whole cards above its
/// first row, and after the rows of the next card, or of the center's own
/// text, above that row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationCenterRowStart {
    /// How many whole cards lie above the body's first row, newest first.
    skipped_entry_count: usize,
    /// How many rows of the next card, or of the center's own text after
    /// the last card, lie above the body's first row.
    skipped_row_count: usize,
}

/// The rows of the center's body from `row_start`, top first.
///
/// The body lists one card per entry of `notification_view`, newest first:
/// its top border, its header row, its text rows
/// ([`NotificationCenterEntry::list_text_rows`] in
/// [`compute_card_text_column_count`] of `body_column_count`) and its bottom
/// border. The center's own text follows, wrapped in the body less one blank
/// column on each side: the dropped-entries text
/// ([`format_dropped_notifications_text`]) while the view counts a dropped
/// entry, and `No notifications` with no entry and nothing dropped.
///
/// The rows start after the `row_start` cards and rows. The center's own
/// text is wrapped only when the iteration reaches it.
///
/// Example: from the start of row `5`, a body whose newest card takes 5
/// rows starts at the top border of the card after it.
pub fn list_notification_center_rows<'a>(
    notification_view: NotificationView<'a>,
    body_column_count: u16,
    row_start: NotificationCenterRowStart,
) -> impl Iterator<Item = NotificationCenterRow<'a>> + 'a {
    let NotificationCenterRowStart {
        skipped_entry_count,
        skipped_row_count,
    } = row_start;
    let text_column_count = compute_card_text_column_count(body_column_count);
    let card_rows = notification_view
        .center_entries
        .list_center_entries()
        .rev()
        .skip(skipped_entry_count)
        .flat_map(move |center_entry| {
            let notification_entry = center_entry.get_notification_entry();
            let text_row_count = center_entry.list_text_rows(text_column_count).len();
            [
                NotificationCenterRow::CardTopBorder(notification_entry),
                NotificationCenterRow::CardHeader(notification_entry),
            ]
            .into_iter()
            .chain(
                (0..text_row_count).map(move |text_row_index| NotificationCenterRow::CardText {
                    center_entry,
                    text_row_index,
                }),
            )
            .chain(std::iter::once(NotificationCenterRow::CardBottomBorder(
                notification_entry,
            )))
        });
    let status_rows = std::iter::once(()).flat_map(move |()| {
        wrap_text_rows(
            &format_center_status_text(notification_view),
            compute_status_text_column_count(body_column_count),
        )
        .into_iter()
        .map(NotificationCenterRow::StatusText)
    });
    card_rows.chain(status_rows).skip(skipped_row_count)
}

/// The text under the center's cards: the dropped-entries text while
/// `notification_view` counts a dropped entry, `No notifications` with no
/// entry and nothing dropped, and nothing otherwise.
fn format_center_status_text(notification_view: NotificationView<'_>) -> String {
    if notification_view.dropped_entry_count > 0 {
        format_dropped_notifications_text(notification_view.dropped_entry_count)
    } else if notification_view.center_entries.is_empty() {
        NO_NOTIFICATIONS_TEXT.to_string()
    } else {
        String::new()
    }
}

/// The body rows the card of the entry `notification_id` takes in a body
/// `body_column_count` columns wide, counted from the body's first row at
/// scroll row `0`. `None` when `notification_view` lists no such entry.
///
/// The cards are counted, newest first, only through that entry's card, and
/// they stay counted until the entries or the body width change
/// ([`NotificationCenterEntries`]).
///
/// Example: in a body whose newest card takes 5 rows, the second newest
/// card of one text row takes `5..9`.
#[must_use]
pub fn find_notification_card_row_range(
    notification_view: NotificationView<'_>,
    body_column_count: u16,
    notification_id: NotificationId,
) -> Option<Range<usize>> {
    let center_entries = notification_view.center_entries;
    let newest_first_entry_index =
        center_entries
            .list_center_entries()
            .rev()
            .position(|center_entry| {
                center_entry.get_notification_entry().notification_id == notification_id
            })?;
    let card_row_ends = center_entries.list_card_row_ends(
        body_column_count,
        newest_first_entry_index + 1,
        usize::MAX,
    );
    let card_start_row = if newest_first_entry_index == 0 {
        0
    } else {
        card_row_ends[newest_first_entry_index - 1]
    };
    Some(card_start_row..card_row_ends[newest_first_entry_index])
}

/// The card under one cell of the center's body, and whether the cell is its
/// dismiss button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationCardHit {
    /// The entry whose card is under the cell.
    pub notification_id: NotificationId,
    /// Whether the cell is the card's dismiss button `✕`.
    pub is_on_dismiss_button: bool,
}

/// The card one body row of the center draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationCardRow {
    /// The entry whose card the row is part of.
    pub notification_id: NotificationId,
    /// Whether the row is the card's top border, which holds its dismiss
    /// button `✕`.
    pub is_top_border: bool,
}

/// The rows the center's body draws for one body size, scroll and dropped
/// count ([`NotificationView::compute_center_body_rows`]).
///
/// Example: in a `22x17` body drawn from row `0`, a newest card of 5 rows
/// gives `row_start` at no skipped card and no skipped row, and its entry on
/// `card_rows` items `0` to `4`, with `is_top_border` on item `0` only.
#[derive(Debug, Clone)]
pub struct NotificationCenterBodyRows {
    /// The body size the rows are for.
    body_size: Size,
    /// The scroll the rows are for, before the cut to the body row the
    /// center draws from
    /// ([`NotificationView::compute_drawn_center_scroll_row_offset`]).
    scroll_row_offset: usize,
    /// The dropped count the rows are for.
    dropped_entry_count: u64,
    /// Where the body starts drawing.
    pub row_start: NotificationCenterRowStart,
    /// The card each body row draws: one item per drawn body row, top
    /// first, and `None` on a row of the center's own text. The rows under
    /// the last drawn row have no item.
    pub card_rows: Arc<[Option<NotificationCardRow>]>,
}

/// The card under `resting_point`, a cell of the center at rest at
/// `center_layout` ([`NotificationCenterLayout::find_resting_point`]), as
/// `card_rows` ([`NotificationCenterBodyRows::card_rows`]) lists the cards
/// each body row drew. `None` off the body, on a row of no card, and on a
/// row `card_rows` has no item for.
///
/// The dismiss button is the cell 4 columns in from the card's right edge on
/// its top border: column `18` of a `22`-column card.
#[must_use]
pub fn find_notification_card_at(
    card_rows: &[Option<NotificationCardRow>],
    center_layout: &NotificationCenterLayout,
    resting_point: Point,
) -> Option<NotificationCardHit> {
    let body_rect = center_layout.body_rect;
    if !body_rect.is_point_inside(resting_point) {
        return None;
    }
    let body_row_index = usize::from(resting_point.row - body_rect.origin.row);
    let card_row = card_rows.get(body_row_index).copied().flatten()?;
    Some(NotificationCardHit {
        notification_id: card_row.notification_id,
        is_on_dismiss_button: card_row.is_top_border
            && resting_point.column == compute_card_dismiss_button_column(body_rect),
    })
}

/// The screen column of the dismiss button `✕` on the top border of a card
/// that spans `body_rect`'s columns:
/// [`CARD_DISMISS_BUTTON_RIGHT_COLUMN_OFFSET`] columns in from its right
/// edge. The body of a drawn center is at least 8 columns wide, which leaves
/// two `─` left of the button. A body at `(57, 4)` `22x17` gives `75`.
pub(crate) fn compute_card_dismiss_button_column(body_rect: Rect) -> u16 {
    (body_rect.origin.column + body_rect.size.column_count)
        .saturating_sub(CARD_DISMISS_BUTTON_RIGHT_COLUMN_OFFSET)
}

/// The close control ` ✕ ` on the top border of the toast whose box is
/// `toast_rect`, at least 10 columns wide: it ends before the last `─` and
/// the corner. A `24`-column box at `(56, 1)` gives `(75, 1)` `3x1`.
pub(crate) fn compute_toast_close_rect(toast_rect: Rect) -> Rect {
    build_row_rect(
        toast_rect.origin.column + toast_rect.size.column_count
            - BORDER_END_COLUMN_COUNT
            - NOTIFICATION_TOAST_CLOSE_CONTROL_COLUMN_COUNT,
        toast_rect.origin.row,
        NOTIFICATION_TOAST_CLOSE_CONTROL_COLUMN_COUNT,
    )
}

/// A one-row rect at `(start_column, row)`, `column_count` columns wide.
pub(crate) fn build_row_rect(start_column: u16, row: u16, column_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: start_column,
            row,
        },
        Size {
            column_count,
            row_count: 1,
        },
    )
}

/// The pane area of `frame_layout`, or `None` when the viewport has no cell
/// or every pane is suppressed.
fn find_notification_pane_area(frame_layout: FrameLayout<'_>) -> Option<RatatuiRect> {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0
        || viewport_area.height == 0
        || frame_layout
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    {
        return None;
    }
    Some(compute_frame_pane_area(frame_layout, viewport_area))
}

/// The columns the tabline's notification button takes: the cells right
/// before `button_end_column`, the tray button's column. `None` when fewer
/// cells of the row than the button takes lie left of `button_end_column`. A
/// ` ●3 ` button before a tray button at column `71` gives `67..71`.
pub(crate) fn find_notification_button_columns(
    notification_button: NotificationButton,
    tabline_area: RatatuiRect,
    button_end_column: u16,
) -> Option<Range<u16>> {
    let button_column_count = get_text_width(&format_notification_button_text(notification_button));
    let has_room = button_end_column.saturating_sub(tabline_area.x) >= button_column_count;
    has_room.then(|| (button_end_column - button_column_count)..button_end_column)
}

#[cfg(test)]
pub(crate) mod tests;
