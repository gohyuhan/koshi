//! Per-pane terminal state: screen buffers, cursor, pen style, modes, title,
//! and scrollback.
//!
//! One [`TerminalState`] backs a single terminal pane; panes never share
//! buffers. The runtime owns the `PaneId → TerminalState` map, so the state
//! itself carries no identity. The VTE performer (see the `perform` submodule)
//! mutates this model as PTY output arrives.

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
    /// Deferred-wrap latch (xterm-style): set when a glyph is printed into the
    /// last column, leaving the cursor parked there instead of advancing. The
    /// next printable glyph first wraps to the following line, so a row that
    /// exactly fills the width does not scroll early. Any cursor-moving
    /// operation clears it.
    pending_wrap: bool,
}

/// Terminal mode flags (bracketed paste, mouse tracking, …).
///
/// Placeholder: the individual mode fields are added later; it exists now so
/// [`TerminalState`] can own it.
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
    /// Scroll-region margins as 0-based inclusive `(top, bottom)` rows set by
    /// DECSTBM; `None` scrolls the whole screen. Line feed, reverse index,
    /// IL/DL, and SU/SD all clamp to this band.
    scroll_region: Option<(u16, u16)>,
    /// The window/tab title set via OSC 0/1/2; `None` until the app sets one.
    title: Option<String>,
    /// Lines that have scrolled off the top of the primary screen.
    scrollback: Scrollback,
}

impl TerminalState {
    /// Create per-pane state for a terminal of `size`: both screen buffers
    /// blank, the cursor at the top-left and visible, default pen, no title.
    pub fn new(size: PtySize) -> Self {
        let terminal_size = Grid::blank(size.rows, size.cols, Style::default());
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
            scroll_region: None,
            title: None,
            scrollback: Scrollback {},
        }
    }

    /// Resize both screen buffers to `size` and clamp the cursor into the new
    /// bounds. Existing cell contents are discarded — reflow is not done here.
    pub fn resize(&mut self, size: PtySize) {
        let fill = self.style.bg_fill();
        let resized_terminal_size = Grid::blank(size.rows, size.cols, fill);
        self.primary = resized_terminal_size.clone();
        self.alternate = resized_terminal_size.clone();

        self.cursor.row = min(self.cursor.row, size.rows.saturating_sub(1));
        self.cursor.col = min(self.cursor.col, size.cols.saturating_sub(1));
        // The deferred-wrap latch refers to the old right edge; the new grid is
        // blank and the cursor was just clamped, so drop it.
        self.cursor.pending_wrap = false;
        // Margins index the old geometry; drop the region so the resized screen
        // scrolls in full until the app issues DECSTBM again.
        self.scroll_region = None;
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
