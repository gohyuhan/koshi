//! [`vte::Perform`] implementation that drives [`TerminalState`] from parsed
//! PTY output: printable glyphs land in the active grid at the cursor, and the
//! basic C0 control bytes move the cursor and scroll.
//!
//! Only `print` and `execute` are implemented; every other `Perform` method
//! keeps its default no-op so later tasks (CSI cursor/erase, SGR, OSC, …) add
//! them without re-touching this file. `vte` decodes UTF-8 upstream, so `print`
//! receives a ready `char`.

use crate::grid::state::Cell;
use crate::state::TerminalState;

impl TerminalState {
    /// Move the cursor down one line, scrolling the active grid up when it is
    /// already on the last row. The column is left unchanged (LNM is off, so a
    /// line feed is a pure vertical move).
    fn linefeed(&mut self) {
        let (rows, _) = self.active_grid().dimensions();
        let last_row = rows.saturating_sub(1);
        if self.cursor.row >= last_row {
            self.active_grid_mut().scroll_up();
        } else {
            self.cursor.row += 1;
        }
    }
}

impl vte::Perform for TerminalState {
    fn print(&mut self, c: char) {
        // Deferred wrap: a prior print parked on the last column. Wrap to the
        // next line before placing this glyph, so a row that exactly fills the
        // width is not scrolled early.
        if self.cursor.pending_wrap {
            self.linefeed();
            self.cursor.col = 0;
            self.cursor.pending_wrap = false;
        }

        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);
        let row = self.cursor.row;
        let col = self.cursor.col;
        let style = self.style;

        if let Some(cell) = self.active_grid_mut().cell_mut(row, col) {
            *cell = Cell::new(c, 1, style);
        }

        if col >= last_col {
            // Park on the last column; the next print performs the wrap.
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.col += 1;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // LF, VT, FF: line feed (VT/FF treated as LF).
            0x0A..=0x0C => {
                self.linefeed();
                self.cursor.pending_wrap = false;
            }
            // CR: carriage return to column 0.
            0x0D => {
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            // BS: backspace one column (no erase).
            0x08 => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            // HT: advance to the next 8-column tab stop, clamped to the grid.
            0x09 => {
                let (_, cols) = self.active_grid().dimensions();
                let last_col = cols.saturating_sub(1);
                let to_next_stop = 8 - (self.cursor.col % 8);
                let next_tab = self.cursor.col.saturating_add(to_next_stop);
                self.cursor.col = next_tab.min(last_col);
                self.cursor.pending_wrap = false;
            }
            // BEL: discarded.
            0x07 => {}
            // Any other control byte: trace and ignore, never raw-rendered.
            _ => {
                tracing::trace!(byte, "unhandled control byte; ignored");
            }
        }
    }
}

#[cfg(test)]
mod tests;
