//! Per-pane terminal state: screen buffers, cursor, pen style, modes, title,
//! and scrollback.
//!
//! One [`TerminalState`] backs a single terminal pane; panes never share
//! buffers. The runtime owns the `PaneId → TerminalState` map, so the state
//! itself carries no identity. The VTE performer (see the `perform` submodule)
//! mutates this model as PTY output arrives.

use std::cmp::min;

use tile_core::process::PtySize;

use crate::grid::state::{Cell, Grid};
use crate::scrollback::{Scrollback, ScrollbackLimit};
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
    /// The deferred-wrap latch at save time, restored alongside the position so
    /// a glyph parked at the last column still wraps after a save/restore.
    pending_wrap: bool,
}

/// The text cursor: position, visibility, and the deferred-wrap latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Zero-based row within the active grid (internally 0-based despite
    /// 1-based ANSI addressing).
    row: u16,
    /// Zero-based column within the active grid.
    col: u16,
    /// Whether the cursor is currently shown (toggled by DEC mode `?25`).
    is_visible: bool,
    /// Deferred-wrap latch (xterm-style): set when a glyph is printed into the
    /// last column, leaving the cursor parked there instead of advancing. The
    /// next printable glyph first wraps to the following line, so a row that
    /// exactly fills the width does not scroll early. Any cursor-moving
    /// operation clears it.
    pending_wrap: bool,
    /// Saved cursor position and style from DECSC/DECRC (xterm form) or
    /// SCOSC/SCORC (ANSI form), kept per screen so each screen buffer has its
    /// own snapshot independent of the other.
    saved: Option<SavedCursor>,
}

/// One screen row trimmed to the renderer's inner width, with a flag for a
/// wide glyph clipped at the right edge.
///
/// Produced by [`TerminalState::clip_row`]. Borrows the live grid row, so it
/// lives only as long as that borrow. A wide glyph (CJK, emoji) occupies two
/// columns; when the inner rect ends between its halves, drawing only the left
/// half would show a broken glyph. `clip_row` instead drops that base from
/// `cells` and sets `right_pad`, telling the renderer to fill the freed column
/// with a blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClippedRow<'a> {
    /// The visible cells, left to right. When `right_pad` is set this stops one
    /// column short of the inner width — the clipped wide base is excluded.
    cells: &'a [Cell],
    /// `true` when the last visible column would have shown only the left half
    /// of a wide glyph; the renderer draws one blank pad cell there instead.
    right_pad: bool,
}

impl<'a> ClippedRow<'a> {
    /// The visible cells, left to right. The renderer draws these, then one
    /// blank pad cell when `right_pad` is set. The slice borrows the underlying
    /// grid row, so it outlives this `ClippedRow`.
    pub fn cells(&self) -> &'a [Cell] {
        self.cells
    }

    /// Whether the renderer should draw one blank pad cell after `cells` to
    /// fill the column a clipped wide glyph would have half-occupied.
    pub fn right_pad(&self) -> bool {
        self.right_pad
    }
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
    /// The cursor for the primary screen, holding its own position, visibility,
    /// wrap latch, and saved snapshot.
    primary_cursor: Cursor,
    /// The cursor for the alternate screen, independent of the primary cursor
    /// so that position and wrap state do not leak across screen switches.
    alternate_cursor: Cursor,
    /// The current pen style applied to printed cells.
    style: Style,
    /// Active terminal modes (bracketed paste, mouse tracking, …).
    modes: TerminalModes,
    /// The window/tab title set via OSC 0/1/2; `None` until the app sets one.
    title: Option<String>,
    /// Lines that have scrolled off the top of the primary screen.
    scrollback: Scrollback,
    /// Primary screen's DECSTBM scroll-region margins, 0-based inclusive
    /// `(top, bottom)`; `None` scrolls the whole screen. Kept per screen (not
    /// shared) so an alt-screen app's margins do not leak onto the primary
    /// after it exits.
    primary_scroll_region: Option<(u16, u16)>,
    /// Alternate screen's scroll-region margins; see `primary_scroll_region`.
    alternate_scroll_region: Option<(u16, u16)>,
    /// The grapheme cluster currently being built at the cursor — the run of
    /// printed code points that fold into one cell (a base plus its combining
    /// marks and any emoji continuation: ZWJ-joined parts, variation selectors,
    /// skin-tone modifiers, regional-indicator flags). Empty when no run is
    /// active; any non-printing event resets it.
    cluster: String,
    /// The `(row, col)` of the cell holding `cluster`'s base, or `None` when no
    /// run is active. Continuations attach here and width promotion widens it.
    cluster_base: Option<(u16, u16)>,
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
            pending_wrap: false,
            saved: None,
        };
        TerminalState {
            primary: terminal_size.clone(),
            alternate: terminal_size.clone(),
            active: Screen::Primary,
            primary_cursor: terminal_cursor,
            alternate_cursor: terminal_cursor,
            style: Style::default(),
            modes: TerminalModes {},
            title: None,
            scrollback: Scrollback::new(ScrollbackLimit::default()),
            primary_scroll_region: None,
            alternate_scroll_region: None,
            cluster: String::new(),
            cluster_base: None,
        }
    }

    /// Resize both screen buffers to `size` and clamp the cursor into the new
    /// bounds. Existing cell contents are discarded — reflow is not done here.
    pub fn resize(&mut self, size: PtySize) {
        let fill = self.style.bg_fill();
        let resized_terminal_size = Grid::blank(size.rows, size.cols, fill);
        self.primary = resized_terminal_size.clone();
        self.alternate = resized_terminal_size.clone();

        // Clamp both cursors to the new bounds.
        self.primary_cursor.row = min(self.primary_cursor.row, size.rows.saturating_sub(1));
        self.primary_cursor.col = min(self.primary_cursor.col, size.cols.saturating_sub(1));
        self.primary_cursor.pending_wrap = false;

        self.alternate_cursor.row = min(self.alternate_cursor.row, size.rows.saturating_sub(1));
        self.alternate_cursor.col = min(self.alternate_cursor.col, size.cols.saturating_sub(1));
        self.alternate_cursor.pending_wrap = false;

        // Margins index the old geometry; drop the region so the resized screen
        // scrolls in full until the app issues DECSTBM again.
        self.primary_scroll_region = None;
        self.alternate_scroll_region = None;

        // The resized buffers discard their cells, so any in-progress cluster's
        // base cell is gone; drop the run.
        self.cluster.clear();
        self.cluster_base = None;
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

    /// The window/tab title set by OSC 0/1/2, or `None` if the app has not set
    /// one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Whether the cursor should be drawn — toggled by DECTCEM (`?25`).
    pub fn cursor_visible(&self) -> bool {
        self.active_cursor().is_visible
    }

    /// The pane's scrollback history. The runtime reads its truncation tallies
    /// to emit `PaneScrollbackTruncated`, and the renderer reads its lines to
    /// compose a scrolled-back view.
    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    pub fn scroll_region(&self) -> Option<(u16, u16)> {
        match self.active {
            Screen::Primary => self.primary_scroll_region,
            Screen::Alternate => self.alternate_scroll_region,
        }
    }

    pub fn scroll_region_mut(&mut self) -> &mut Option<(u16, u16)> {
        match self.active {
            Screen::Primary => &mut self.primary_scroll_region,
            Screen::Alternate => &mut self.alternate_scroll_region,
        }
    }

    /// The cursor for the active screen.
    fn active_cursor(&self) -> &Cursor {
        match self.active {
            Screen::Primary => &self.primary_cursor,
            Screen::Alternate => &self.alternate_cursor,
        }
    }

    /// Mutable access to the cursor for the active screen.
    fn active_cursor_mut(&mut self) -> &mut Cursor {
        match self.active {
            Screen::Primary => &mut self.primary_cursor,
            Screen::Alternate => &mut self.alternate_cursor,
        }
    }

    /// Trim the active screen's `row` to the first `inner_width` columns for
    /// rendering, guarding the right edge against a half-drawn wide glyph.
    ///
    /// Returns the visible cells plus a `right_pad` flag. When the last visible
    /// column holds the left half of a wide glyph (its continuation falls
    /// outside the inner rect), that base is dropped from the returned cells and
    /// `right_pad` is set so the renderer blanks the freed column rather than
    /// drawing half a glyph. An out-of-range `row`, a zero `inner_width`, or an
    /// empty row yields no cells and no pad. `inner_width` is clamped to the
    /// row length, so a width past the grid is harmless.
    pub fn clip_row(&self, row: u16, inner_width: u16) -> ClippedRow<'_> {
        let rows = self.active_grid().rows();
        let Some(r) = rows.get(row as usize) else {
            return ClippedRow {
                cells: &[],
                right_pad: false,
            };
        };

        let w = min(inner_width as usize, r.len());

        if w > 0 && r[w - 1].width() > 1 {
            ClippedRow {
                cells: &r[..w - 1],
                right_pad: true,
            }
        } else {
            ClippedRow {
                cells: &r[..w],
                right_pad: false,
            }
        }
    }
}

#[cfg(test)]
mod tests;
