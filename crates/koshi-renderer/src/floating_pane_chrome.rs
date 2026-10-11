//! Where the parts of a floating pane's chrome sit inside its outer rect.
//!
//! A floating pane draws a one-cell border ring, a titlebar row under the top
//! border, and a separator row under the titlebar. Its content fills the rest:
//!
//! ```text
//! ┌──────────────────────────────────────────────┐   row 0: top border
//! │ ⣿⣿  htop                          [ ]  –  ✕  │   row 1: titlebar
//! ├──────────────────────────────────────────────┤   row 2: separator
//! │ content                                      │   rows 3 to the last but one
//! └──────────────────────────────────────────────┘   last row: bottom border
//! ```
//!
//! Every rect here counts the same cells as the outer rect it comes from.

use koshi_core::constant::FLOATING_PANE_CHROME_SIZE;
use koshi_core::geometry::{Point, Rect, Size};

/// The chrome rows above a floating pane's content: the top border, the
/// titlebar and the separator.
const TOP_CHROME_ROW_COUNT: u16 = 3;

/// The column, counted from the outer rect's left edge, of the grab handle
/// `⣿⣿`: one blank cell after the left border.
const GRAB_HANDLE_START_COLUMN: u16 = 2;

/// The cells the grab handle `⣿⣿` takes.
const GRAB_HANDLE_COLUMN_COUNT: u16 = 2;

/// The column, counted from the outer rect's left edge, where the title
/// starts: two blank cells after the grab handle. No control starts left of
/// it.
const TITLE_START_COLUMN: u16 = 6;

/// The blank cells between the title and the first control.
const TITLE_CONTROL_GAP_COLUMN_COUNT: u16 = 2;

/// The column of the close button `✕`, counted back from the outer rect's
/// right edge: two blank cells sit between it and the right border.
const CLOSE_BUTTON_RIGHT_COLUMN_OFFSET: u16 = 4;

/// The column of the minimize button `–`, counted back from the outer rect's
/// right edge.
const MINIMIZE_BUTTON_RIGHT_COLUMN_OFFSET: u16 = 7;

/// The first column of the pin toggle, counted back from the outer rect's
/// right edge.
const PIN_TOGGLE_RIGHT_COLUMN_OFFSET: u16 = 12;

/// The cells the pin toggle takes: `[ ]` and one blank cell, or `[⚪]`.
const PIN_TOGGLE_COLUMN_COUNT: u16 = 4;

/// The content size of a floating pane whose outer size is `outer_size`: the
/// outer size less [`FLOATING_PANE_CHROME_SIZE`] on each axis, never below
/// `0`. `40x12` → `38x8`. The pane's PTY takes this size.
#[must_use]
pub fn compute_floating_pane_content_size(outer_size: Size) -> Size {
    Size {
        column_count: outer_size
            .column_count
            .saturating_sub(FLOATING_PANE_CHROME_SIZE.column_count),
        row_count: outer_size
            .row_count
            .saturating_sub(FLOATING_PANE_CHROME_SIZE.row_count),
    }
}

/// The content rect of a floating pane whose outer rect is `outer_rect`: one
/// column right of the left border, three rows down past the top border, the
/// titlebar and the separator, and
/// [`compute_floating_pane_content_size`] cells in size. Outer `(24, 8)`
/// `72x22` → content `(25, 11)` `70x18`.
#[must_use]
pub fn compute_floating_pane_content_rect(outer_rect: Rect) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: outer_rect.origin.column.saturating_add(1),
            row: outer_rect.origin.row.saturating_add(TOP_CHROME_ROW_COUNT),
        },
        compute_floating_pane_content_size(outer_rect.size),
    )
}

/// The cells of the parts of a floating pane's titlebar, each one row tall
/// on the titlebar row. A part with no room is `None`, and its cells are
/// plain titlebar cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingPaneTitlebar {
    /// The grab handle `⣿⣿`, two cells.
    pub(crate) grab_handle_rect: Option<Rect>,
    /// The cells the pane title may take, at least one.
    pub(crate) title_rect: Option<Rect>,
    /// The pin toggle, four cells: `[ ]` and one blank cell, or `[⚪]`.
    pub(crate) pin_toggle_rect: Option<Rect>,
    /// The minimize button `–`, one cell.
    pub(crate) minimize_button_rect: Option<Rect>,
    /// The close button `✕`, one cell.
    pub(crate) close_button_rect: Option<Rect>,
}

/// The titlebar parts of a floating pane whose outer rect is `outer_rect`.
///
/// The titlebar row reads `│ ⣿⣿  <title>  [ ]  –  ✕  │`: the grab handle in
/// columns 2 and 3, the title from column 6, and the controls counted back
/// from the right border. A control is drawn only when it starts at column 6
/// or further right. A narrow pane drops the pin toggle first (below 18
/// columns), then the minimize button (below 13), then the close button
/// (below 10). The grab handle needs 5 columns. Two blank cells separate the
/// title from the first control. With no control, one blank cell separates
/// the title from the right border. A pane with every control shows a title
/// from 21 columns.
///
/// An outer rect shorter than 4 rows has no titlebar: every part is `None`.
///
/// A `48x12` outer rect at `(0, 0)` gives the handle at `(2, 1)`, the title
/// at `(6, 1)` `28x1`, the pin toggle at `(36, 1)`, minimize at `(41, 1)` and
/// close at `(44, 1)`. A `10x12` one gives the handle at `(2, 1)`, the close
/// button at `(6, 1)`, and no title.
#[must_use]
pub(crate) fn compute_floating_pane_titlebar(outer_rect: Rect) -> FloatingPaneTitlebar {
    let outer_column_count = outer_rect.size.column_count;
    if outer_rect.size.row_count < FLOATING_PANE_CHROME_SIZE.row_count {
        return FloatingPaneTitlebar {
            grab_handle_rect: None,
            title_rect: None,
            pin_toggle_rect: None,
            minimize_button_rect: None,
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
    let find_control_column = |right_column_offset: u16| {
        (outer_column_count >= right_column_offset + TITLE_START_COLUMN)
            .then(|| outer_column_count - right_column_offset)
    };
    let close_button_column = find_control_column(CLOSE_BUTTON_RIGHT_COLUMN_OFFSET);
    let minimize_button_column = find_control_column(MINIMIZE_BUTTON_RIGHT_COLUMN_OFFSET);
    let pin_toggle_column = find_control_column(PIN_TOGGLE_RIGHT_COLUMN_OFFSET);
    let title_end_column = match pin_toggle_column
        .or(minimize_button_column)
        .or(close_button_column)
    {
        Some(first_control_column) => first_control_column - TITLE_CONTROL_GAP_COLUMN_COUNT,
        None => outer_column_count.saturating_sub(2),
    };
    let has_grab_handle_room =
        outer_column_count > GRAB_HANDLE_START_COLUMN + GRAB_HANDLE_COLUMN_COUNT;
    FloatingPaneTitlebar {
        grab_handle_rect: has_grab_handle_room
            .then(|| build_titlebar_rect(GRAB_HANDLE_START_COLUMN, GRAB_HANDLE_COLUMN_COUNT)),
        title_rect: (title_end_column > TITLE_START_COLUMN).then(|| {
            build_titlebar_rect(TITLE_START_COLUMN, title_end_column - TITLE_START_COLUMN)
        }),
        pin_toggle_rect: pin_toggle_column
            .map(|start_column| build_titlebar_rect(start_column, PIN_TOGGLE_COLUMN_COUNT)),
        minimize_button_rect: minimize_button_column
            .map(|start_column| build_titlebar_rect(start_column, 1)),
        close_button_rect: close_button_column
            .map(|start_column| build_titlebar_rect(start_column, 1)),
    }
}

#[cfg(test)]
mod tests;
