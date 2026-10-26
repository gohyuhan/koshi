//! Where the parts of a floating component's chrome sit inside its outer
//! rect. Every floating component draws this chrome: a floating pane, the
//! floating pane tray and the notification center.
//!
//! The chrome is a one-cell border ring, a titlebar row under the top border,
//! and a separator row under the titlebar. The content fills the rest. The
//! titlebar holds a title and the close button `✕`, and each component adds
//! the controls it declares (`FloatingTitlebarControls`). A floating pane adds
//! the grab handle, the pin toggle and the minimize button:
//!
//! ```text
//! ┌──────────────────────────────────────────────┐   row 0: top border
//! │ ⣿⣿  htop                          [ ]  –  ✕  │   row 1: titlebar
//! ├──────────────────────────────────────────────┤   row 2: separator
//! │ content                                      │   rows 3 to the last but one
//! └──────────────────────────────────────────────┘   last row: bottom border
//! ```
//!
//! A component that adds no control has the title and the close button only:
//!
//! ```text
//! ┌──────────────────────┐
//! │ Notifications     ✕  │
//! ├──────────────────────┤
//! ```
//!
//! Every rect here counts the same cells as the outer rect it comes from.

use koshi_core::constant::FLOATING_CHROME_SIZE;
use koshi_core::geometry::{Point, Rect, Size};

/// The chrome rows above a floating component's content: the top border, the
/// titlebar and the separator.
const TOP_CHROME_ROW_COUNT: u16 = 3;

/// The column, counted from the outer rect's left edge, of the grab handle
/// `⣿⣿`: one blank cell after the left border.
const GRAB_HANDLE_START_COLUMN: u16 = 2;

/// The cells the grab handle `⣿⣿` takes.
const GRAB_HANDLE_COLUMN_COUNT: u16 = 2;

/// The column, counted from the outer rect's left edge, where the title
/// starts on a titlebar with a grab handle: two blank cells after the
/// handle.
const TITLE_AFTER_GRAB_HANDLE_START_COLUMN: u16 = 6;

/// The column, counted from the outer rect's left edge, where the title
/// starts on a titlebar with no grab handle: one blank cell after the left
/// border.
const TITLE_START_COLUMN: u16 = 2;

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

/// The controls a floating component adds to its titlebar, beside the title
/// and the close button that every titlebar holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingTitlebarControls {
    /// Whether the titlebar holds the grab handle `⣿⣿` that drags the
    /// component.
    pub(crate) has_grab_handle: bool,
    /// Whether the titlebar holds the pin toggle `[ ]` or `[⚪]`.
    pub(crate) has_pin_toggle: bool,
    /// Whether the titlebar holds the minimize button `–`.
    pub(crate) has_minimize_button: bool,
}

impl FloatingTitlebarControls {
    /// The controls of a floating pane: the grab handle, the pin toggle and
    /// the minimize button.
    pub(crate) const FLOATING_PANE: FloatingTitlebarControls = FloatingTitlebarControls {
        has_grab_handle: true,
        has_pin_toggle: true,
        has_minimize_button: true,
    };

    /// No control beside the title and the close button: the controls of the
    /// floating pane tray and of the notification center.
    pub(crate) const CLOSE_BUTTON_ONLY: FloatingTitlebarControls = FloatingTitlebarControls {
        has_grab_handle: false,
        has_pin_toggle: false,
        has_minimize_button: false,
    };
}

/// The content size of a floating component whose outer size is
/// `outer_size`: the outer size less [`FLOATING_CHROME_SIZE`] on each axis,
/// never below `0`. `40x12` → `38x8`. A floating pane's PTY takes this size.
#[must_use]
pub fn compute_floating_content_size(outer_size: Size) -> Size {
    Size {
        column_count: outer_size
            .column_count
            .saturating_sub(FLOATING_CHROME_SIZE.column_count),
        row_count: outer_size
            .row_count
            .saturating_sub(FLOATING_CHROME_SIZE.row_count),
    }
}

/// The content rect of a floating component whose outer rect is
/// `outer_rect`: one column right of the left border, three rows down past
/// the top border, the titlebar and the separator, and
/// [`compute_floating_content_size`] cells in size. Outer `(24, 8)` `72x22`
/// → content `(25, 11)` `70x18`.
#[must_use]
pub fn compute_floating_content_rect(outer_rect: Rect) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: outer_rect.origin.column.saturating_add(1),
            row: outer_rect.origin.row.saturating_add(TOP_CHROME_ROW_COUNT),
        },
        compute_floating_content_size(outer_rect.size),
    )
}

/// The cells of the parts of a floating component's titlebar, each one row
/// tall on the titlebar row. A part the component does not declare, or with
/// no room, is `None`, and its cells are plain titlebar cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FloatingTitlebar {
    /// The grab handle `⣿⣿`, two cells.
    pub(crate) grab_handle_rect: Option<Rect>,
    /// The cells the title may take, at least one.
    pub(crate) title_rect: Option<Rect>,
    /// The pin toggle, four cells: `[ ]` and one blank cell, or `[⚪]`.
    pub(crate) pin_toggle_rect: Option<Rect>,
    /// The minimize button `–`, one cell.
    pub(crate) minimize_button_rect: Option<Rect>,
    /// The close button `✕`, one cell.
    pub(crate) close_button_rect: Option<Rect>,
}

/// The titlebar parts of a floating component whose outer rect is
/// `outer_rect` and whose titlebar holds `titlebar_controls`.
///
/// With a grab handle, the titlebar row reads
/// `│ ⣿⣿  <title>  [ ]  –  ✕  │`: the handle in columns 2 and 3, and the
/// title from column 6. With no grab handle, the title starts at column 2.
/// The controls are counted back from the right border: the close button 4
/// columns in, the minimize button 7 and the 4-cell pin toggle 12. A control
/// is drawn only when it starts at the title's first column or further right.
/// A narrow component drops the pin toggle first, then the minimize button,
/// then the close button. The grab handle needs 5 columns. Two blank cells
/// separate the title from the first control. With no control drawn, one
/// blank cell separates the title from the right border.
///
/// A floating pane ([`FloatingTitlebarControls::FLOATING_PANE`]) draws the
/// pin toggle from 18 columns, the minimize button from 13 and the close
/// button from 10, and shows a title with every control from 21 columns. A
/// component with the close button only
/// ([`FloatingTitlebarControls::CLOSE_BUTTON_ONLY`]) draws it from 6 columns,
/// and shows a title beside it from 9 columns.
///
/// An outer rect shorter than 4 rows has no titlebar: every part is `None`.
///
/// A floating pane's `48x12` outer rect at `(0, 0)` gives the handle at
/// `(2, 1)`, the title at `(6, 1)` `28x1`, the pin toggle at `(36, 1)`,
/// minimize at `(41, 1)` and close at `(44, 1)`. A `24x22` outer rect with the
/// close button only gives the title at `(2, 1)` `16x1` and close at
/// `(20, 1)`.
#[must_use]
pub(crate) fn compute_floating_titlebar(
    outer_rect: Rect,
    titlebar_controls: FloatingTitlebarControls,
) -> FloatingTitlebar {
    let outer_column_count = outer_rect.size.column_count;
    if outer_rect.size.row_count < FLOATING_CHROME_SIZE.row_count {
        return FloatingTitlebar {
            grab_handle_rect: None,
            title_rect: None,
            pin_toggle_rect: None,
            minimize_button_rect: None,
            close_button_rect: None,
        };
    }
    let title_start_column = if titlebar_controls.has_grab_handle {
        TITLE_AFTER_GRAB_HANDLE_START_COLUMN
    } else {
        TITLE_START_COLUMN
    };
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
    let find_control_column = |is_declared: bool, right_column_offset: u16| {
        (is_declared && outer_column_count >= right_column_offset + title_start_column)
            .then(|| outer_column_count - right_column_offset)
    };
    let close_button_column = find_control_column(true, CLOSE_BUTTON_RIGHT_COLUMN_OFFSET);
    let minimize_button_column = find_control_column(
        titlebar_controls.has_minimize_button,
        MINIMIZE_BUTTON_RIGHT_COLUMN_OFFSET,
    );
    let pin_toggle_column = find_control_column(
        titlebar_controls.has_pin_toggle,
        PIN_TOGGLE_RIGHT_COLUMN_OFFSET,
    );
    let title_end_column = match pin_toggle_column
        .or(minimize_button_column)
        .or(close_button_column)
    {
        Some(first_control_column) => first_control_column - TITLE_CONTROL_GAP_COLUMN_COUNT,
        None => outer_column_count.saturating_sub(2),
    };
    let has_grab_handle_room = titlebar_controls.has_grab_handle
        && outer_column_count > GRAB_HANDLE_START_COLUMN + GRAB_HANDLE_COLUMN_COUNT;
    FloatingTitlebar {
        grab_handle_rect: has_grab_handle_room
            .then(|| build_titlebar_rect(GRAB_HANDLE_START_COLUMN, GRAB_HANDLE_COLUMN_COUNT)),
        title_rect: (title_end_column > title_start_column).then(|| {
            build_titlebar_rect(title_start_column, title_end_column - title_start_column)
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
