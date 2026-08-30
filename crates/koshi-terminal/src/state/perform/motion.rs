//! Cursor motion, scrolling, and the scroll region: line feed and reverse
//! index, save / restore cursor, absolute placement, the deferred-wrap latch,
//! and tab-stop math.

use crate::grid::state::{RowEnd, RowMeta};
use crate::state::{RenderState, SavedCursor, Screen, TerminalState};
use crate::style::Style;

impl TerminalState {
    /// The scroll-region margins as 0-based inclusive `(top, bottom)` rows,
    /// resolving `None` to the whole active grid.
    pub(super) fn region_bounds(&self) -> (u16, u16) {
        let last_row = self.active_grid().dimensions().0.saturating_sub(1);
        self.scroll_region().unwrap_or((0, last_row))
    }

    /// Delete `n` lines starting at `first`, scrolling the band `first..=bottom`
    /// up and filling the vacated bottom rows with `fill`.
    ///
    /// When `first == 0` on the primary screen — a line feed at the bottom
    /// margin of a region starting at row 0, an SU whose region starts at row
    /// 0, a DL with the cursor on row 0 — the departing rows `0..min(n, bottom + 1)`
    /// go into scrollback first, oldest first, each with its row end and prompt
    /// mark. The alternate screen, and a delete with `first > 0`, feed nothing:
    /// the removed lines are discarded.
    pub(super) fn delete_lines_into_scrollback(
        &mut self,
        first: u16,
        bottom: u16,
        n: u16,
        fill: Style,
    ) {
        if self.active == Screen::Primary && first == 0 {
            let band_height = bottom.saturating_add(1);
            let removed = n.min(band_height);
            for row in 0..removed {
                if let Some(scrolled_off) = self.primary.rows().get(row as usize) {
                    let meta = RowMeta {
                        end: self.primary.row_end(row),
                        prompt: self.primary.prompt_mark(row),
                    };
                    self.scrollback.push_row_with_meta(scrolled_off, meta);
                }
            }
        }
        self.active_grid_mut().delete_lines(first, bottom, n, fill);
    }

    /// Move the cursor down one line. At the scroll region's bottom margin the
    /// cursor stays and the region scrolls up one line; on any other row the
    /// cursor moves down, stopping at the last grid row. The column does not
    /// change.
    pub(super) fn linefeed(&mut self) {
        let (top, bottom) = self.region_bounds();
        if self.active_cursor().row == bottom {
            let fill = self.active_render().style.bg_fill();
            self.delete_lines_into_scrollback(top, bottom, 1, fill);
        } else {
            let last_row = self.active_grid().dimensions().0.saturating_sub(1);
            if self.active_cursor().row < last_row {
                self.active_cursor_mut().row += 1;
            }
        }
    }

    /// Record `end` on the row the line feed leaves behind, then
    /// [`linefeed`](Self::linefeed).
    ///
    /// The cursor moved down: its old row gets `end`. The region scrolled: the
    /// cursor row is marked before the scroll, so a row leaving the top carries
    /// `end` and its prompt mark into scrollback, and the row directly above
    /// the cursor gets `end` afterwards (`delete_lines` reset it to
    /// [`RowEnd::Hard`]). The cursor neither moved nor scrolled (last grid row
    /// outside the region): no row is marked, since no row continues it.
    pub(super) fn wrap_linefeed(&mut self, end: RowEnd) {
        let before = self.active_cursor().row;
        let at_scroll_bottom = before == self.region_bounds().1;
        if at_scroll_bottom {
            self.active_grid_mut().set_row_end(before, end);
        }
        self.linefeed();

        let after = self.active_cursor().row;
        let continued_row = if after > before {
            Some(before)
        } else if at_scroll_bottom {
            after.checked_sub(1)
        } else {
            None
        };
        if let Some(row) = continued_row {
            self.active_grid_mut().set_row_end(row, end);
        }
    }

    /// Reverse index (RI): move the cursor up one line. At the scroll region's
    /// top margin the cursor stays and the region scrolls down one line; on row
    /// 0 outside a region the cursor stays. Clears the deferred-wrap latch.
    pub(super) fn reverse_index(&mut self) {
        let (top, bottom) = self.region_bounds();
        if self.active_cursor().row == top {
            let fill = self.active_render().style.bg_fill();
            self.active_grid_mut().insert_lines(top, bottom, 1, fill);
        } else if self.active_cursor().row > 0 {
            self.active_cursor_mut().row -= 1;
        }
        self.clear_wrap_latch();
    }

    /// Save the cursor position, wrap latch, and the active screen's render
    /// state (DECSC / SCOSC) into the active screen's cursor. Each screen keeps
    /// its own snapshot.
    pub(super) fn save_cursor(&mut self) {
        let cursor = *self.active_cursor();
        let render = *self.active_render();
        self.active_cursor_mut().saved = Some(SavedCursor {
            row: cursor.row,
            col: cursor.col,
            pending_wrap: cursor.pending_wrap,
            render,
        });
    }

    /// Restore the cursor position, wrap latch, and render state saved by
    /// [`save_cursor`](Self::save_cursor) (DECRC / SCORC), clamping the position
    /// into the current grid. With no saved cursor: home the cursor, clear the
    /// wrap latch, and reset the render state to
    /// [`RenderState::fresh`]. The saved snapshot stays.
    pub(super) fn restore_cursor(&mut self) {
        match self.active_cursor().saved {
            Some(saved) => {
                let (rows, cols) = self.active_grid().dimensions();
                let cursor = self.active_cursor_mut();
                cursor.row = saved.row.min(rows.saturating_sub(1));
                cursor.col = saved.col.min(cols.saturating_sub(1));
                cursor.pending_wrap = saved.pending_wrap;
                *self.active_render_mut() = saved.render;
            }
            None => {
                let cursor = self.active_cursor_mut();
                cursor.row = 0;
                cursor.col = 0;
                cursor.pending_wrap = false;
                *self.active_render_mut() = RenderState::fresh();
            }
        }
    }

    /// Move the cursor to an absolute (`row`, `col`), clamped into the active
    /// grid, and clear the deferred-wrap latch. Every absolute cursor
    /// placement — CUP/HVP, CHA/HPA, VPA, CNL, CPL — routes through here.
    pub(super) fn goto(&mut self, row: u16, col: u16) {
        let (rows, cols) = self.active_grid().dimensions();
        let cursor = self.active_cursor_mut();
        cursor.row = row.min(rows.saturating_sub(1));
        cursor.col = col.min(cols.saturating_sub(1));
        cursor.pending_wrap = false;
    }

    /// Park the cursor on `last_col`. With autowrap (DECAWM `?7`) on, arm the
    /// deferred-wrap latch: the next glyph wraps before printing. With autowrap
    /// off, clear the latch: the next glyph overwrites `last_col` in place,
    /// and turning autowrap on afterward does not arm the latch. Every
    /// site where a glyph lands on the last column funnels through here.
    pub(super) fn arm_wrap_latch(&mut self, last_col: u16) {
        let armed = self.modes.autowrap;
        let cursor = self.active_cursor_mut();
        cursor.col = last_col;
        cursor.pending_wrap = armed;
    }

    /// Clear the active cursor's deferred-wrap latch: the next glyph prints at
    /// the cursor's column instead of wrapping first. Counterpart of
    /// [`arm_wrap_latch`](Self::arm_wrap_latch).
    pub(super) fn clear_wrap_latch(&mut self) {
        self.active_cursor_mut().pending_wrap = false;
    }

    /// Set a horizontal tab stop at the active cursor column. A column past the
    /// tab-stop table is a no-op.
    pub(super) fn set_tab_stop(&mut self) {
        let col = self.active_cursor().col;
        if let Some(stop) = self.tab_stops.get_mut(col as usize) {
            *stop = true;
        }
    }

    /// Clear the horizontal tab stop at the active cursor column. A column past
    /// the tab-stop table is a no-op.
    pub(super) fn clear_tab_stop(&mut self) {
        let col = self.active_cursor().col;
        if let Some(stop) = self.tab_stops.get_mut(col as usize) {
            *stop = false;
        }
    }

    /// Clear every horizontal tab stop.
    pub(super) fn clear_all_tab_stops(&mut self) {
        self.tab_stops.fill(false);
    }
}

/// The first tab stop strictly after `col`, or `last_col` when there is none
/// or `col >= last_col`. A column past the end of `tab_stops` holds no stop.
pub(super) fn next_tab_stop(tab_stops: &[bool], col: u16, last_col: u16) -> u16 {
    if col >= last_col {
        return last_col;
    }
    (col + 1..=last_col)
        .find(|&next| tab_stops.get(next as usize).copied().unwrap_or(false))
        .unwrap_or(last_col)
}

/// The first tab stop strictly before `col`, or column `0` when there is none.
/// A column past the end of `tab_stops` holds no stop.
pub(super) fn prev_tab_stop(tab_stops: &[bool], col: u16) -> u16 {
    (0..col)
        .rev()
        .find(|&previous| tab_stops.get(previous as usize).copied().unwrap_or(false))
        .unwrap_or(0)
}
