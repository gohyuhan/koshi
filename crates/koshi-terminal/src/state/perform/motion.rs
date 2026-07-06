//! Cursor motion, scrolling, and the scroll region: line feed and reverse
//! index, save / restore cursor, absolute placement, the deferred-wrap latch,
//! and tab-stop math.

use crate::state::{RenderState, SavedCursor, Screen, TerminalState};
use crate::style::Style;

impl TerminalState {
    /// The scroll-region margins for the active screen.
    fn active_scroll_region(&self) -> Option<(u16, u16)> {
        match self.active {
            Screen::Primary => self.primary_scroll_region,
            Screen::Alternate => self.alternate_scroll_region,
        }
    }

    /// The scroll-region margins as 0-based inclusive `(top, bottom)` rows,
    /// resolving `None` to the whole active grid.
    pub(super) fn region_bounds(&self) -> (u16, u16) {
        let last_row = self.active_grid().dimensions().0.saturating_sub(1);
        self.active_scroll_region().unwrap_or((0, last_row))
    }

    /// Delete `n` lines starting at `first` (scrolling the band `first..=bottom`
    /// up), first preserving into scrollback any rows that leave the *top* of the
    /// primary screen.
    ///
    /// Rows leave the top only when `first == 0` on the primary screen — i.e. a
    /// line feed at a top-anchored region's bottom margin, an SU whose region
    /// starts at row 0, or a DL with the cursor on row 0. The alternate screen
    /// never feeds history, and an interior delete (`first > 0`, e.g. DL below
    /// the top or a scroll region whose top margin is below row 0) discards its
    /// removed lines rather than retaining them. This matches xterm/alacritty,
    /// where history is fed only when the scrolled region begins at row 0.
    ///
    /// The departing rows — `rows[0..min(n, bottom + 1)]`, exactly the rows
    /// `delete_lines` removes — are pushed oldest-first so the topmost lands
    /// deepest in history. Capture happens before the delete, which overwrites
    /// them.
    pub(super) fn delete_lines_into_scrollback(
        &mut self,
        first: u16,
        bottom: u16,
        n: u16,
        fill: Style,
    ) {
        if self.active == Screen::Primary && first == 0 {
            let removed = n.min(bottom.saturating_sub(first).saturating_add(1));
            for row in 0..removed {
                if let Some(scrolled_off) = self.primary.rows().get(row as usize) {
                    let scrolled_off = scrolled_off.clone();
                    self.scrollback.push_line(scrolled_off);
                }
            }
        }
        self.active_grid_mut().delete_lines(first, bottom, n, fill);
    }

    /// Move the cursor down one line. At the scroll region's bottom margin the
    /// region scrolls up instead of the cursor advancing; below the margin the
    /// cursor just descends to the last grid row. The column is left unchanged
    /// (LNM is off, so a line feed is a pure vertical move).
    pub(super) fn linefeed(&mut self) {
        let fill = self.active_render().style.bg_fill();
        let (top, bottom) = self.region_bounds();
        if self.active_cursor().row == bottom {
            self.delete_lines_into_scrollback(top, bottom, 1, fill);
        } else {
            let last_row = self.active_grid().dimensions().0.saturating_sub(1);
            if self.active_cursor().row < last_row {
                self.active_cursor_mut().row += 1;
            }
        }
    }

    /// Reverse index (RI): move the cursor up one line. At the scroll region's
    /// top margin the region scrolls down instead.
    pub(super) fn reverse_index(&mut self) {
        let fill = self.active_render().style.bg_fill();
        let (top, bottom) = self.region_bounds();
        if self.active_cursor().row == top {
            self.active_grid_mut().insert_lines(top, bottom, 1, fill);
        } else if self.active_cursor().row > 0 {
            self.active_cursor_mut().row -= 1;
        }
        self.active_cursor_mut().pending_wrap = false;
    }

    /// Save the cursor position and the active screen's render state (DECSC /
    /// SCOSC) into the active screen's cursor, so the primary and alternate
    /// screens snapshot separately.
    pub(super) fn save_cursor(&mut self) {
        let row = self.active_cursor().row;
        let col = self.active_cursor().col;
        let pending_wrap = self.active_cursor().pending_wrap;
        let render = *self.active_render();
        self.active_cursor_mut().saved = Some(SavedCursor {
            row,
            col,
            pending_wrap,
            render,
        });
    }

    /// Restore the cursor position and pen style saved by `save_cursor` (DECRC /
    /// SCORC). With no prior save, xterm homes the cursor and resets the pen to
    /// defaults; the restored position is clamped into the current grid in case
    /// it shrank since the save.
    pub(super) fn restore_cursor(&mut self) {
        let (rows, cols) = self.active_grid().dimensions();
        let (last_row, last_col) = (rows.saturating_sub(1), cols.saturating_sub(1));
        match self.active_cursor().saved {
            Some(saved) => {
                self.active_cursor_mut().row = saved.row.min(last_row);
                self.active_cursor_mut().col = saved.col.min(last_col);
                self.active_cursor_mut().pending_wrap = saved.pending_wrap;
                *self.active_render_mut() = saved.render;
            }
            None => {
                self.active_cursor_mut().row = 0;
                self.active_cursor_mut().col = 0;
                self.active_cursor_mut().pending_wrap = false;
                *self.active_render_mut() = RenderState::fresh();
            }
        }
    }

    /// Move the cursor to an absolute (`row`, `col`), clamped into the active
    /// grid, and clear the deferred-wrap latch — the single chokepoint every
    /// absolute cursor placement (CUP/HVP, CHA/HPA, VPA, CNL, CPL) routes
    /// through. Centralizing here is deliberate: origin mode (DECOM) and
    /// left/right margins (DECSLRM) become a one-place change when they land,
    /// the same way alacritty funnels its absolute moves through one `goto`.
    pub(super) fn goto(&mut self, row: u16, col: u16) {
        let (rows, cols) = self.active_grid().dimensions();
        let cursor = self.active_cursor_mut();
        cursor.row = row.min(rows.saturating_sub(1));
        cursor.col = col.min(cols.saturating_sub(1));
        cursor.pending_wrap = false;
    }

    /// Park the cursor on `last_col` and arm the deferred-wrap latch — but ONLY
    /// when autowrap (DECAWM `?7`) is on. The latch is purely an autowrap
    /// mechanism: with autowrap off, a glyph landing on the last column leaves the
    /// cursor resting there with no wrap pending, so the next glyph overwrites in
    /// place (DEC: a character at the right margin replaces when autowrap is
    /// reset). Re-enabling autowrap afterward does not retroactively arm a wrap.
    /// Every site where a glyph lands on the last column funnels through here, so
    /// the arm-iff-autowrap rule lives in one place.
    pub(super) fn arm_wrap_latch(&mut self, last_col: u16) {
        let armed = self.modes.autowrap;
        let cursor = self.active_cursor_mut();
        cursor.col = last_col;
        cursor.pending_wrap = armed;
    }
}

/// The next 8-column tab stop strictly after `col`, clamped to `last_col`. With
/// stops at every multiple of 8, this rounds `col` up to the next multiple (a
/// column already on a stop still advances a full 8), bounded by the last
/// column. A later task will replace the fixed 8-grid with a configurable stop
/// table; this is the single place the forward-stop math lives (HT and CHT share it).
pub(super) fn next_tab_stop(col: u16, last_col: u16) -> u16 {
    col.saturating_add(8 - col % 8).min(last_col)
}

/// The previous 8-column tab stop strictly before `col`, floored at column 0
/// (itself a stop). A column already on a stop retreats a full 8.
pub(super) fn prev_tab_stop(col: u16) -> u16 {
    col.saturating_sub(1) / 8 * 8
}
