//! Where the notification center, the toast and the tabline's notification
//! button sit on screen, and the rows an entry's text wraps into.
//!
//! The center is a panel on the right side of the pane area, as tall as the
//! pane area. Its top border names it and holds the close control ` × `, its
//! bottom border holds the label ` clear `, and its body lists the entries
//! newest first. A `32`-column center:
//!
//! ```text
//! ┌─ Notifications ────────── × ─┐   top border: title and close control
//! │ [Koshi] Error ×2             │   an entry's header row
//! │ pane …a1b2 killed by signal  │   the entry's text, wrapped
//! │ 9                            │
//! │ [Koshi] Warning              │
//! │ koshi.kdl: unknown key       │
//! │ 3 older notifications were   │   the dropped-entries row
//! │ dropped                      │
//! └─ clear ──────────────────────┘   bottom border: the clear label
//! ```
//!
//! The toast is a box of the same width in one corner of the pane area. Its
//! top border holds the close control, its first body row the entry's header,
//! and up to [`MAX_TOAST_TEXT_ROW_COUNT`] rows of its text sit under that row:
//!
//! ```text
//! ┌─────────────────────── × ─┐
//! │ [Koshi] Error ×2          │
//! │ pane …a1b2 killed by      │
//! │ signal 9                  │
//! └───────────────────────────┘
//! ```
//!
//! Every rect here is in screen cells, `(0, 0)` at the viewport's top-left
//! cell.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ops::Range;

use koshi_config::types::ToastPosition;
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::notification::{NotificationEntry, NotificationSeverity, NotificationSource};
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;
use ratatui::text::Span;

use crate::hit_test::{compute_frame_pane_area, compute_viewport_area};
use crate::render::{compute_symbol_display_width, get_text_width};
use crate::snapshot::{FrameLayout, NotificationButton};

/// The widest the center and the toast are: 48 columns.
const MAXIMUM_NOTIFICATION_PANEL_COLUMN_COUNT: u16 = 48;

/// The narrowest the center and the toast are on a viewport at least this
/// wide: 24 columns. A narrower viewport gives them its full width.
const MINIMUM_NOTIFICATION_PANEL_COLUMN_COUNT: u16 = 24;

/// The narrowest box the center and the toast are drawn in: 10 columns.
const MINIMUM_NOTIFICATION_BOX_COLUMN_COUNT: u16 = 10;

/// The most text rows a toast holds under its header row: 2.
pub const MAX_TOAST_TEXT_ROW_COUNT: u16 = 2;

/// The rows of a toast that hold no text: its top border, its header row and
/// its bottom border.
const TOAST_FRAME_ROW_COUNT: u16 = 3;

/// The border cells at each end of a top or bottom border before a label: a
/// corner and one `─`.
const BORDER_END_COLUMN_COUNT: u16 = 2;

/// The columns on each side of a body row: the border and one blank column.
const BODY_SIDE_COLUMN_COUNT: u16 = 2;

/// The center's title on its top border.
pub(crate) const NOTIFICATION_CENTER_TITLE_TEXT: &str = " Notifications ";

/// The close control on the top border of the center and of the toast.
pub(crate) const NOTIFICATION_CLOSE_CONTROL_TEXT: &str = " × ";

/// The cells [`NOTIFICATION_CLOSE_CONTROL_TEXT`] takes.
const NOTIFICATION_CLOSE_CONTROL_COLUMN_COUNT: u16 = 3;

/// The label on the center's bottom border that clears the center.
pub(crate) const NOTIFICATION_CLEAR_LABEL_TEXT: &str = " clear ";

/// The body row of an empty center.
pub(crate) const NO_NOTIFICATIONS_TEXT: &str = "No notifications";

/// The columns of the center and the toast on a viewport `viewport_column_count`
/// columns wide: a third of it, from 24 to 48 columns, and the full width below
/// 24. `80` gives `26`, `200` gives `48`, `60` gives `24`, and `20` gives `20`.
#[must_use]
pub fn compute_notification_panel_column_count(viewport_column_count: u16) -> u16 {
    if viewport_column_count < MINIMUM_NOTIFICATION_PANEL_COLUMN_COUNT {
        return viewport_column_count;
    }
    (viewport_column_count / 3).clamp(
        MINIMUM_NOTIFICATION_PANEL_COLUMN_COUNT,
        MAXIMUM_NOTIFICATION_PANEL_COLUMN_COUNT,
    )
}

/// Where the parts of an open notification center sit, in screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationCenterLayout {
    /// The center's box, border ring included.
    pub center_rect: Rect,
    /// The top border cells that the title ` Notifications ` may take, at
    /// least one.
    pub title_rect: Rect,
    /// The close control ` × ` on the top border.
    pub close_rect: Rect,
    /// The label ` clear ` on the bottom border, cut to the border cells left
    /// of its last `─`.
    pub clear_rect: Rect,
    /// The cells the body rows take: the rows between the borders, and the
    /// columns between the blank column inside each side border.
    pub body_rect: Rect,
}

/// Where the open center of `frame_layout.viewer_chrome.notification_center`
/// sits.
///
/// The center takes the right side of the pane area: it is
/// [`compute_notification_panel_column_count`] columns wide, cut to the pane
/// area, and as tall as the pane area. `None` while the viewer's center is
/// closed, when the viewport has no cell, every pane is suppressed, or the box
/// would be narrower than 10 columns or shorter than 3 rows.
///
/// An `80x24` viewport with a pane area at `(0, 1)` `80x22` gives the center
/// at `(54, 1)` `26x22`, its close control at `(75, 1)` `3x1`, its clear label
/// at `(56, 22)` `7x1`, and its body at `(56, 2)` `22x20`.
#[must_use]
pub fn find_drawn_notification_center_layout(
    frame_layout: FrameLayout<'_>,
) -> Option<NotificationCenterLayout> {
    frame_layout.viewer_chrome.notification_center?;
    let pane_area = find_notification_pane_area(frame_layout)?;
    let viewport_area = compute_viewport_area(frame_layout);
    let center_column_count =
        compute_notification_panel_column_count(viewport_area.width).min(pane_area.width);
    if center_column_count < MINIMUM_NOTIFICATION_BOX_COLUMN_COUNT || pane_area.height < 3 {
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
    let border_parts = compute_border_label_rects(center_rect);
    let bottom_row = center_rect.origin.row + center_rect.size.row_count - 1;
    let clear_column_count = get_text_width(NOTIFICATION_CLEAR_LABEL_TEXT)
        .min(center_column_count - 2 * BORDER_END_COLUMN_COUNT);
    Some(NotificationCenterLayout {
        center_rect,
        title_rect: border_parts.title_rect,
        close_rect: border_parts.close_rect,
        clear_rect: build_row_rect(
            center_rect.origin.column + BORDER_END_COLUMN_COUNT,
            bottom_row,
            clear_column_count,
        ),
        body_rect: Rect::from_origin_and_size(
            Point {
                column: center_rect.origin.column + BODY_SIDE_COLUMN_COUNT,
                row: center_rect.origin.row + 1,
            },
            Size {
                column_count: center_column_count - 2 * BODY_SIDE_COLUMN_COUNT,
                row_count: center_rect.size.row_count - 2,
            },
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
    /// The close control ` × ` on the top border, where this frame draws it.
    /// `None` while any of its cells lies past the side edge.
    pub close_rect: Option<Rect>,
}

/// Where the toast of `frame_layout.viewer_chrome.notification_toast` sits.
///
/// The box is as wide as the center, and as tall as its text rows, its header
/// row and its two border rows. It sits in the corner of the pane area its `toast_position`
/// names. Of its width, `hidden_per_mille` thousandths, rounded to the nearest
/// column, lie past the side edge of that corner: past the right edge for a
/// right corner, and past the left edge for a left corner. Those columns are
/// not drawn.
///
/// `None` while the viewer shows no toast, when the viewport has no cell,
/// every pane is suppressed, the box would be narrower than 10 columns or
/// taller than the pane area, or every column of the box lies past the edge.
///
/// A toast of no text row at `top-right` over a pane area at `(0, 1)` `80x22`
/// sits at `(54, 1)` `26x3`. Half hidden, it draws its 13 left columns at
/// `(67, 1)`, and its close control lies past the edge.
#[must_use]
pub fn find_drawn_notification_toast_layout(
    frame_layout: FrameLayout<'_>,
) -> Option<NotificationToastLayout> {
    let notification_toast = frame_layout.viewer_chrome.notification_toast?;
    let pane_area = find_notification_pane_area(frame_layout)?;
    let viewport_area = compute_viewport_area(frame_layout);
    let toast_column_count =
        compute_notification_panel_column_count(viewport_area.width).min(pane_area.width);
    let toast_row_count = notification_toast.text_row_count + TOAST_FRAME_ROW_COUNT;
    if toast_column_count < MINIMUM_NOTIFICATION_BOX_COLUMN_COUNT
        || toast_row_count > pane_area.height
    {
        return None;
    }
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
    let hidden_column_count = ((u32::from(toast_column_count)
        * u32::from(notification_toast.hidden_per_mille.min(1000))
        + 500)
        / 1000) as u16;
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
    let box_close_rect = compute_border_label_rects(toast_rect).close_rect;
    let box_close_start_column = box_close_rect.origin.column - toast_rect.origin.column;
    let is_close_drawn = box_close_start_column >= first_drawn_box_column
        && box_close_start_column + NOTIFICATION_CLOSE_CONTROL_COLUMN_COUNT
            <= first_drawn_box_column + drawn_column_count;
    Some(NotificationToastLayout {
        toast_rect,
        drawn_rect,
        first_drawn_box_column,
        close_rect: is_close_drawn.then(|| {
            build_row_rect(
                drawn_start_column + box_close_start_column - first_drawn_box_column,
                toast_rect.origin.row,
                NOTIFICATION_CLOSE_CONTROL_COLUMN_COUNT,
            )
        }),
    })
}

/// The tabline's notification button text: ` ● ` with no unread entry, ` ●3 `
/// with 3, and ` ●99+ ` with more than 99. `●` (`U+25CF`) has East Asian
/// ambiguous width, and koshi draws it one cell wide.
#[must_use]
pub fn format_notification_button_text(notification_button: NotificationButton) -> String {
    match notification_button.unread_entry_count {
        0 => " ● ".to_string(),
        unread_entry_count @ 1..=99 => format!(" ●{unread_entry_count} "),
        _ => " ●99+ ".to_string(),
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
pub fn format_notification_header(entry: &NotificationEntry) -> String {
    let header_text = format!(
        "{} {}",
        format_notification_source_label(entry.source),
        format_notification_severity(entry.severity)
    );
    if entry.repeat_count > 1 {
        format!("{header_text} ×{}", entry.repeat_count)
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
            row_column_count = 0;
        }
        for styled_grapheme in Span::raw(word).styled_graphemes(Style::default()) {
            let grapheme_column_count = compute_symbol_display_width(styled_grapheme.symbol);
            if !row_text.is_empty()
                && row_column_count.saturating_add(grapheme_column_count) > column_count
            {
                text_rows.push(std::mem::take(&mut row_text));
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

/// The rows a toast of `text` holds under its header row in a box
/// `toast_column_count` columns wide: the rows [`wrap_text_rows`] gives in the
/// box's body columns, at most [`MAX_TOAST_TEXT_ROW_COUNT`].
#[must_use]
pub fn count_toast_text_rows(text: &str, toast_column_count: u16) -> u16 {
    let body_column_count = toast_column_count.saturating_sub(2 * BODY_SIDE_COLUMN_COUNT);
    let text_row_count = wrap_text_rows(text, body_column_count).len();
    text_row_count.min(usize::from(MAX_TOAST_TEXT_ROW_COUNT)) as u16
}

/// One entry the notification center lists, and the count of rows its text
/// wraps to ([`wrap_text_rows`]) in the body width last asked for.
///
/// The entry does not change after this type takes it. The count is kept for
/// one body width: asking again for that width reads it, and asking for
/// another width wraps the text again and keeps the new count.
#[derive(Debug, Clone)]
pub struct NotificationCenterEntry {
    /// The entry.
    entry: NotificationEntry,
    /// The last count taken, or `None` before any count.
    text_row_count: Cell<Option<TextRowCount>>,
}

/// The rows an entry's text wraps to in a body `body_column_count` columns
/// wide.
#[derive(Debug, Clone, Copy)]
struct TextRowCount {
    /// The body width the text was wrapped at.
    body_column_count: u16,
    /// The rows the text wrapped to.
    row_count: usize,
}

impl NotificationCenterEntry {
    /// `entry`, with no count taken.
    #[must_use]
    pub fn from_entry(entry: NotificationEntry) -> Self {
        NotificationCenterEntry {
            entry,
            text_row_count: Cell::new(None),
        }
    }

    /// The entry.
    #[must_use]
    pub fn get_entry(&self) -> &NotificationEntry {
        &self.entry
    }

    /// The rows the entry's text wraps to in a body `body_column_count`
    /// columns wide ([`wrap_text_rows`]). The text is wrapped only when the
    /// last count was taken at another width or none was taken.
    ///
    /// Example: `pane …a1b2 killed by signal 9` gives `3` at `10` columns.
    /// A second ask at `10` reads `3` with no wrap, and an ask at `40` wraps
    /// again and gives `1`.
    #[must_use]
    pub fn count_text_rows(&self, body_column_count: u16) -> usize {
        if let Some(text_row_count) = self.text_row_count.get() {
            if text_row_count.body_column_count == body_column_count {
                return text_row_count.row_count;
            }
        }
        let row_count = wrap_text_rows(&self.entry.text, body_column_count).len();
        self.text_row_count.set(Some(TextRowCount {
            body_column_count,
            row_count,
        }));
        row_count
    }
}

/// What one viewer's notification center and toast draw.
#[derive(Debug, Clone, Copy)]
pub struct NotificationView<'a> {
    /// Every entry the center lists, oldest first.
    pub entries: &'a VecDeque<NotificationCenterEntry>,
    /// How many entries the viewer can no longer show: the center's
    /// dropped-entries row counts them.
    pub dropped_entry_count: u64,
    /// What the toast shows, or `None` while no toast shows.
    pub toast_content: Option<NotificationToastContent<'a>>,
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
            NotificationToastContent::Entry(entry) => format_notification_header(entry),
            NotificationToastContent::Summary { severity, .. } => format!(
                "{} {}",
                format_notification_source_label(NotificationSource::Core),
                format_notification_severity(*severity)
            ),
        }
    }

    /// The toast's text: the entry's text, and the
    /// [`format_toast_summary_text`] of a summary.
    #[must_use]
    pub fn format_text(&self) -> String {
        match self {
            NotificationToastContent::Entry(entry) => entry.text.clone(),
            NotificationToastContent::Summary {
                notification_count, ..
            } => format_toast_summary_text(*notification_count),
        }
    }

    /// The severity whose style the toast's header takes.
    #[must_use]
    pub fn get_severity(&self) -> NotificationSeverity {
        match self {
            NotificationToastContent::Entry(entry) => entry.severity,
            NotificationToastContent::Summary { severity, .. } => *severity,
        }
    }
}

/// One row of the center's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationCenterRow<'a> {
    /// An entry's header ([`format_notification_header`]), drawn in its
    /// severity's style.
    EntryHeader(&'a NotificationEntry),
    /// One row of an entry's text.
    EntryText(String),
    /// One row of the center's own text, drawn dim: the count of the entries
    /// the viewer can no longer show ([`format_dropped_notifications_text`]),
    /// or `No notifications` in a center with no entry and nothing dropped.
    StatusText(String),
}

/// The rows of the center's body from row `first_row_index`, top first.
///
/// The body lists each entry of `notification_view`, newest first, as its
/// header row and its text rows ([`wrap_text_rows`] in `body_column_count`
/// columns). The status text follows, wrapped as an entry's text is: the
/// dropped-entries text ([`format_dropped_notifications_text`]) while the
/// view counts a dropped entry, and `No notifications` with no entry and
/// nothing dropped.
///
/// The entries above row `first_row_index` are skipped by their row counts
/// ([`NotificationCenterEntry::count_text_rows`]), not wrapped. Each listed
/// entry is wrapped only when the iteration reaches it.
///
/// Example: from row `3`, a body whose newest entry takes a header and one
/// text row starts at the first text row of the entry after it.
pub fn list_notification_center_rows<'a>(
    notification_view: NotificationView<'a>,
    body_column_count: u16,
    first_row_index: usize,
) -> impl Iterator<Item = NotificationCenterRow<'a>> + 'a {
    let mut skipped_row_count = first_row_index;
    let mut skipped_entry_count = 0;
    for center_entry in notification_view.entries.iter().rev() {
        let entry_row_count = 1 + center_entry.count_text_rows(body_column_count);
        if skipped_row_count < entry_row_count {
            break;
        }
        skipped_row_count -= entry_row_count;
        skipped_entry_count += 1;
    }
    let entry_rows = notification_view
        .entries
        .iter()
        .rev()
        .skip(skipped_entry_count)
        .flat_map(move |center_entry| {
            let entry = center_entry.get_entry();
            std::iter::once(NotificationCenterRow::EntryHeader(entry)).chain(
                wrap_text_rows(&entry.text, body_column_count)
                    .into_iter()
                    .map(NotificationCenterRow::EntryText),
            )
        });
    let status_rows = wrap_text_rows(
        &format_center_status_text(notification_view),
        body_column_count,
    );
    entry_rows
        .chain(
            status_rows
                .into_iter()
                .map(NotificationCenterRow::StatusText),
        )
        .skip(skipped_row_count)
}

/// The text under the center's entries: the dropped-entries text while
/// `notification_view` counts a dropped entry, `No notifications` with no
/// entry and nothing dropped, and nothing otherwise.
fn format_center_status_text(notification_view: NotificationView<'_>) -> String {
    if notification_view.dropped_entry_count > 0 {
        format_dropped_notifications_text(notification_view.dropped_entry_count)
    } else if notification_view.entries.is_empty() {
        NO_NOTIFICATIONS_TEXT.to_string()
    } else {
        String::new()
    }
}

/// The highest body row the open center can draw from: the row that puts the
/// last of its rows ([`list_notification_center_rows`] of `notification_view`
/// in `body_rect`'s columns) on `body_rect`'s last row, or `0` while every row
/// fits. Each entry adds its header row and its text rows
/// ([`NotificationCenterEntry::count_text_rows`]). 30 entries of one text row
/// each in a `22x20` body give 60 rows and `40`.
#[must_use]
pub fn compute_maximum_center_scroll_row_offset(
    notification_view: NotificationView<'_>,
    body_rect: Rect,
) -> usize {
    let body_column_count = body_rect.size.column_count;
    let entry_row_count: usize = notification_view
        .entries
        .iter()
        .map(|center_entry| 1 + center_entry.count_text_rows(body_column_count))
        .sum();
    let status_row_count = wrap_text_rows(
        &format_center_status_text(notification_view),
        body_column_count,
    )
    .len();
    (entry_row_count + status_row_count).saturating_sub(usize::from(body_rect.size.row_count))
}

/// The rects of a box's top border labels: the title cells from the cell after
/// the corner and one `─`, and the close control ` × ` that ends before the
/// last `─` and the corner. The title cells end one `─` before the close
/// control.
pub(crate) struct BorderLabelRects {
    /// The title cells.
    pub(crate) title_rect: Rect,
    /// The close control's cells.
    pub(crate) close_rect: Rect,
}

/// The [`BorderLabelRects`] of `box_rect`, at least 10 columns wide. A
/// `26`-column box at `(54, 1)` gives the title at `(56, 1)` `18x1` and the
/// close control at `(75, 1)` `3x1`.
pub(crate) fn compute_border_label_rects(box_rect: Rect) -> BorderLabelRects {
    let box_end_column = box_rect.origin.column + box_rect.size.column_count;
    let close_start_column =
        box_end_column - BORDER_END_COLUMN_COUNT - NOTIFICATION_CLOSE_CONTROL_COLUMN_COUNT;
    let title_start_column = box_rect.origin.column + BORDER_END_COLUMN_COUNT;
    BorderLabelRects {
        title_rect: build_row_rect(
            title_start_column,
            box_rect.origin.row,
            close_start_column - 1 - title_start_column,
        ),
        close_rect: build_row_rect(
            close_start_column,
            box_rect.origin.row,
            NOTIFICATION_CLOSE_CONTROL_COLUMN_COUNT,
        ),
    }
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
/// before `button_end_column`, the tray button's column or, with no tray
/// button, the mode tag's column. `None` when `notification_button` is
/// `None`, or when fewer cells of the row than the button takes lie left of
/// `button_end_column`. A ` ●3 ` button before a mode tag at column `74`
/// gives `70..74`.
pub(crate) fn find_notification_button_columns(
    notification_button: Option<NotificationButton>,
    tabline_area: RatatuiRect,
    button_end_column: u16,
) -> Option<Range<u16>> {
    let button_column_count =
        get_text_width(&format_notification_button_text(notification_button?));
    let has_room = button_end_column.saturating_sub(tabline_area.x) >= button_column_count;
    has_room.then(|| (button_end_column - button_column_count)..button_end_column)
}

#[cfg(test)]
pub(crate) mod tests;
