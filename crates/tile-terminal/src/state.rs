//! Per-pane terminal state: screen buffers, cursor, pen style, modes, title,
//! and scrollback.
//!
//! One [`TerminalState`] backs a single terminal pane; panes never share
//! buffers. The runtime owns the `PaneId → TerminalState` map, so the state
//! itself carries no identity. The VTE performer (added later) mutates this
//! model as PTY output arrives.

use std::cmp::min;

use tile_core::process::PtySize;

use crate::grid::state::Grid;
use crate::scrollback::Scrollback;
use crate::style::Style;
mod perform;

/// Which of the two screen buffers is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The normal, scrolling screen.
    #[default]
    Primary,
    /// The alternate screen used by full-screen apps (e.g. vim, htop).
    Alternate,
}

/// A cursor position and pen style captured by DECSC, restored by DECRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedCursor {
    /// Saved zero-based row within the grid.
    row: u16,
    /// Saved zero-based column within the grid.
    col: u16,
    /// The pen style in effect when the cursor was saved.
    style: Style,
}

/// The text cursor: position, visibility, and any saved state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Zero-based row within the active grid (internally 0-based despite
    /// 1-based ANSI addressing).
    row: u16,
    /// Zero-based column within the active grid.
    col: u16,
    /// Whether the cursor is currently shown (toggled by DEC mode `?25`).
    is_visible: bool,
    /// Position and style snapshot from DECSC, restored by DECRC; `None` until
    /// a save has happened.
    saved: Option<SavedCursor>,

    pending_wrap: bool,
}

/// Terminal mode flags (bracketed paste, mouse tracking, …).
///
/// Placeholder: the individual mode fields are added in a later task; it exists
/// now so [`TerminalState`] can own it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalModes {}

/// The full emulation state of one terminal pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalState {
    /// The primary (normal, scrolling) screen buffer.
    primary: Grid,
    /// The alternate screen buffer used by full-screen apps; swapped in via DEC
    /// mode `?1049`/`?47` and never appended to the `scrollback`.
    alternate: Grid,
    /// Which buffer — `primary` or `alternate` — output currently writes to and
    /// the renderer displays.
    active: Screen,
    /// The text cursor, addressing the active grid.
    cursor: Cursor,
    /// The current pen style applied to printed cells.
    style: Style,
    /// Active terminal modes (bracketed paste, mouse tracking, …).
    modes: TerminalModes,
    /// The window/tab title set via OSC 0/1/2; `None` until the app sets one.
    title: Option<String>,
    /// Lines that have scrolled off the top of the primary screen.
    scrollback: Scrollback,
}

impl TerminalState {
    /// Create per-pane state for a terminal of `size`: both screen buffers
    /// blank, the cursor at the top-left and visible, default pen, no title.
    pub fn new(size: PtySize) -> Self {
        let terminal_size = Grid::blank(size.rows, size.cols);
        let terminal_cursor = Cursor {
            row: 0,
            col: 0,
            is_visible: true,
            saved: None,
            pending_wrap: false,
        };
        TerminalState {
            primary: terminal_size.clone(),
            alternate: terminal_size.clone(),
            active: Screen::Primary,
            cursor: terminal_cursor,
            style: Style::default(),
            modes: TerminalModes {},
            title: None,
            scrollback: Scrollback {},
        }
    }

    /// Resize both screen buffers to `size` and clamp the cursor into the new
    /// bounds. Existing cell contents are discarded — reflow is not done here.
    pub fn resize(&mut self, size: PtySize) {
        let resized_terminal_size = Grid::blank(size.rows, size.cols);
        self.primary = resized_terminal_size.clone();
        self.alternate = resized_terminal_size.clone();

        self.cursor.row = min(self.cursor.row, size.rows.saturating_sub(1));
        self.cursor.col = min(self.cursor.col, size.cols.saturating_sub(1));
    }

    /// The screen buffer currently displayed and written to — `primary` or
    /// `alternate`, per the active screen.
    pub fn active_grid(&self) -> &Grid {
        match self.active {
            Screen::Primary => &self.primary,
            Screen::Alternate => &self.alternate,
        }
    }

    /// Mutable access to the active screen buffer, for writing cells.
    pub fn active_grid_mut(&mut self) -> &mut Grid {
        match self.active {
            Screen::Primary => &mut self.primary,
            Screen::Alternate => &mut self.alternate,
        }
    }
}

#[cfg(test)]
mod tests;
