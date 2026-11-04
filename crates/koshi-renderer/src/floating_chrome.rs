//! Where the parts of floating chrome sit inside an outer rect.
//!
//! The floating pane tray and the notification center draw a one-cell border
//! ring, a titlebar row under the top border, and a separator row under the
//! titlebar. The content fills the rest. The titlebar holds a title and the
//! close button `✕`:
//!
//! ```text
//! ┌──────────────────────┐   row 0: top border
//! │ Notifications     ✕  │   row 1: titlebar
//! ├──────────────────────┤   row 2: separator
//! │ content              │   rows 3 to the last but one
//! └──────────────────────┘   last row: bottom border
//! ```
//!
//! A floating pane draws a titlebar strip on its first row: the grab handle
//! `⣿⣿`, the pin toggle, the minimize button and the close button, with no
//! border and no title. Its layout tree fills the rows below the strip, each
//! pane in its own box:
//!
//! ```text
//!  ⣿⣿                     [ ]  –  ✕     row 0: titlebar strip
//! ┌─── vim ───────┐┌─── htop ──────┐    rows 1 to the last: layout area
//! │               ││               │
//! └───────────────┘└───────────────┘
//! ```
//!
//! Every rect here counts the same cells as the outer rect it comes from.

use koshi_core::constant::FLOATING_TITLEBAR_STRIP_ROW_COUNT;
use koshi_core::geometry::{Point, Rect, Size};

/// The cells the chrome of the floating pane tray and of the notification
/// center takes from its outer size: one border column on each side, and the
/// top border, titlebar, separator and bottom border rows. A `40x12` box holds
/// `38x8` cells of content.
pub const FLOATING_CHROME_SIZE: Size = Size {
    column_count: 2,
    row_count: 4,
};

/// The chrome rows above the content of the tray and of the notification
/// center: the top border, the titlebar and the separator.
const TOP_CHROME_ROW_COUNT: u16 = 3;

/// The column, counted from the outer rect's left edge, where the title of
/// the tray or of the notification center starts: one blank cell after the
/// left border.
const TITLE_START_COLUMN: u16 = 2;

/// The blank cells between the title and the close button.
const TITLE_CONTROL_GAP_COLUMN_COUNT: u16 = 2;

/// The column of the close button `✕` of the tray and of the notification
/// center, counted back from the outer rect's right edge: two blank cells sit
/// between it and the right border.
const CLOSE_BUTTON_RIGHT_COLUMN_OFFSET: u16 = 4;

/// The column, counted from a floating pane's left edge, of the grab handle
/// `⣿⣿` on its titlebar strip: one blank cell after the edge.
const STRIP_GRAB_HANDLE_START_COLUMN: u16 = 1;

/// The cells the grab handle `⣿⣿` takes.
const STRIP_GRAB_HANDLE_COLUMN_COUNT: u16 = 2;

/// The first column, counted from a floating pane's left edge, where a strip
/// control may start: two blank cells after the grab handle.
const STRIP_FIRST_CONTROL_COLUMN: u16 = 5;

/// The column of the close button `✕` on a floating pane's strip, counted
/// back from its right edge: one blank cell sits after it.
const STRIP_CLOSE_BUTTON_RIGHT_COLUMN_OFFSET: u16 = 2;

/// The column of the minimize button `–` on a floating pane's strip, counted
/// back from its right edge.
const STRIP_MINIMIZE_BUTTON_RIGHT_COLUMN_OFFSET: u16 = 5;

/// The first column of the pin toggle on a floating pane's strip, counted back
/// from its right edge.
const STRIP_PIN_TOGGLE_RIGHT_COLUMN_OFFSET: u16 = 10;

/// The cells the pin toggle takes: `[ ]` and one blank cell, or `[⚪]`.
const STRIP_PIN_TOGGLE_COLUMN_COUNT: u16 = 4;

/// The content rect of the tray or of the notification center whose outer
/// rect is `outer_rect`: one column right of the left border, three rows down
/// past the top border, the titlebar and the separator, and the outer size
/// less [`FLOATING_CHROME_SIZE`] on each axis, never below `0`. Outer
/// `(24, 8)` `72x22` → content `(25, 11)` `70x18`.
#[must_use]
pub fn compute_floating_content_rect(outer_rect: Rect) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: outer_rect.origin.column.saturating_add(1),
            row: outer_rect.origin.row.saturating_add(TOP_CHROME_ROW_COUNT),
        },
        Size {
            column_count: outer_rect
                .size
                .column_count
                .saturating_sub(FLOATING_CHROME_SIZE.column_count),
            row_count: outer_rect
                .size
                .row_count
                .saturating_sub(FLOATING_CHROME_SIZE.row_count),
        },
    )
}

/// The cells of the parts of the titlebar of the tray or of the notification
/// center, each one row tall on the titlebar row. A part with no room is
/// `None`, and its cells are plain titlebar cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingTitlebar {
    /// The cells the title may take, at least one.
    pub(crate) title_rect: Option<Rect>,
    /// The close button `✕`, one cell.
    pub(crate) close_button_rect: Option<Rect>,
}

/// The titlebar parts of the tray or of the notification center whose outer
/// rect is `outer_rect`.
///
/// The titlebar row reads `│ <title>  ✕  │`: the title from column 2, and the
/// close button 4 columns in from the right border. The close button is drawn
/// only when it starts at column 2 or further right: from a width of 6
/// columns. Two blank cells separate the title from the close button: a title
/// shows beside it from a width of 9 columns. With no close button drawn, one blank cell
/// separates the title from the right border.
///
/// An outer rect shorter than 4 rows has no titlebar: every part is `None`.
///
/// A `24x22` outer rect at `(0, 0)` gives the title at `(2, 1)` `16x1` and
/// close at `(20, 1)`.
#[must_use]
pub(crate) fn compute_floating_titlebar(outer_rect: Rect) -> FloatingTitlebar {
    let outer_column_count = outer_rect.size.column_count;
    if outer_rect.size.row_count < FLOATING_CHROME_SIZE.row_count {
        return FloatingTitlebar {
            title_rect: None,
            close_button_rect: None,
        };
    }
    let build_titlebar_rect = |start_column: u16, part_column_count: u16| {
        Rect::from_origin_and_size(
            Point {
                column: outer_rect.origin.column.saturating_add(start_column),
                row: outer_rect.origin.row.saturating_add(1),
            },
            Size {
                column_count: part_column_count,
                row_count: 1,
            },
        )
    };
    let close_button_column = (outer_column_count
        >= CLOSE_BUTTON_RIGHT_COLUMN_OFFSET + TITLE_START_COLUMN)
        .then(|| outer_column_count - CLOSE_BUTTON_RIGHT_COLUMN_OFFSET);
    let title_end_column = match close_button_column {
        Some(close_button_column) => close_button_column - TITLE_CONTROL_GAP_COLUMN_COUNT,
        None => outer_column_count.saturating_sub(2),
    };
    FloatingTitlebar {
        title_rect: (title_end_column > TITLE_START_COLUMN).then(|| {
            build_titlebar_rect(TITLE_START_COLUMN, title_end_column - TITLE_START_COLUMN)
        }),
        close_button_rect: close_button_column
            .map(|start_column| build_titlebar_rect(start_column, 1)),
    }
}

/// The cells of the parts of a floating pane's titlebar strip, each one row
/// tall on the strip row. A part with no room is `None`, and its cells are
/// plain strip cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingPaneStrip {
    /// The grab handle `⣿⣿`, two cells.
    pub(crate) grab_handle_rect: Option<Rect>,
    /// The pin toggle, four cells: `[ ]` and one blank cell, or `[⚪]`.
    pub(crate) pin_toggle_rect: Option<Rect>,
    /// The minimize button `–`, one cell.
    pub(crate) minimize_button_rect: Option<Rect>,
    /// The close button `✕`, one cell.
    pub(crate) close_button_rect: Option<Rect>,
}

/// The strip parts of a floating pane whose outer rect is `outer_rect`.
///
/// The strip row reads ` ⣿⣿  …  [ ]  –  ✕ `: the grab handle in columns 1
/// and 2, and the controls counted back from the right edge: the close button
/// 2 columns in, the minimize button 5 and the 4-cell pin toggle 10. A control
/// is drawn only when it starts at column 5 or further right, two blank cells
/// after the handle. A narrow strip drops the pin toggle first, then the
/// minimize button, then the close button: the close button shows from 7
/// columns, the minimize button from 10 and the pin toggle from 15. The grab
/// handle shows from 3 columns.
///
/// An outer rect with no row has no strip: every part is `None`.
///
/// A `38x12` outer rect at `(0, 0)` gives the handle at `(1, 0)`, the pin
/// toggle at `(28, 0)`, minimize at `(33, 0)` and close at `(36, 0)`.
#[must_use]
pub(crate) fn compute_floating_pane_strip(outer_rect: Rect) -> FloatingPaneStrip {
    let outer_column_count = outer_rect.size.column_count;
    if outer_rect.size.row_count < FLOATING_TITLEBAR_STRIP_ROW_COUNT {
        return FloatingPaneStrip {
            grab_handle_rect: None,
            pin_toggle_rect: None,
            minimize_button_rect: None,
            close_button_rect: None,
        };
    }
    let build_strip_rect = |start_column: u16, part_column_count: u16| {
        Rect::from_origin_and_size(
            Point {
                column: outer_rect.origin.column.saturating_add(start_column),
                row: outer_rect.origin.row,
            },
            Size {
                column_count: part_column_count,
                row_count: 1,
            },
        )
    };
    let find_control_column = |right_column_offset: u16| {
        (outer_column_count >= right_column_offset + STRIP_FIRST_CONTROL_COLUMN)
            .then(|| outer_column_count - right_column_offset)
    };
    let has_grab_handle_room =
        outer_column_count >= STRIP_GRAB_HANDLE_START_COLUMN + STRIP_GRAB_HANDLE_COLUMN_COUNT;
    FloatingPaneStrip {
        grab_handle_rect: has_grab_handle_room.then(|| {
            build_strip_rect(
                STRIP_GRAB_HANDLE_START_COLUMN,
                STRIP_GRAB_HANDLE_COLUMN_COUNT,
            )
        }),
        pin_toggle_rect: find_control_column(STRIP_PIN_TOGGLE_RIGHT_COLUMN_OFFSET)
            .map(|start_column| build_strip_rect(start_column, STRIP_PIN_TOGGLE_COLUMN_COUNT)),
        minimize_button_rect: find_control_column(STRIP_MINIMIZE_BUTTON_RIGHT_COLUMN_OFFSET)
            .map(|start_column| build_strip_rect(start_column, 1)),
        close_button_rect: find_control_column(STRIP_CLOSE_BUTTON_RIGHT_COLUMN_OFFSET)
            .map(|start_column| build_strip_rect(start_column, 1)),
    }
}

/// The cell where a floating pane's layout area starts: the first cell of the
/// row below its titlebar strip. Outer origin `(24, 8)` → `(24, 9)`.
#[must_use]
pub fn compute_floating_pane_layout_origin(outer_rect: Rect) -> Point {
    Point {
        column: outer_rect.origin.column,
        row: outer_rect
            .origin
            .row
            .saturating_add(FLOATING_TITLEBAR_STRIP_ROW_COUNT),
    }
}

#[cfg(test)]
mod tests;
