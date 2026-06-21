//! [`vte::Perform`] implementation that drives [`TerminalState`] from parsed
//! PTY output: printable glyphs land in the active grid at the cursor, and the
//! basic C0 control bytes move the cursor and scroll.
//!
//! Implemented so far: `print` (printable glyphs), `execute` (C0 control
//! bytes), and `csi_dispatch` (CSI cursor moves + erase). The remaining
//! `Perform` methods (`osc_dispatch`, `hook`/`put`/`unhook`, `esc_dispatch`)
//! keep their default no-op until later tasks fill them in; SGR styling is a
//! CSI final byte, so it will extend `csi_dispatch` rather than add a method.
//! `vte` decodes UTF-8 upstream, so `print` receives a ready `char`.

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

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        // Plain cursor/erase sequences carry no intermediate or private-marker
        // bytes. Private markers (`?`/`>`/`<`/`=`) are collected into
        // `intermediates` by vte, so a non-empty slice means a mode-set or
        // device query owned by a later task — skip it here. `ignore` flags a
        // sequence with too many params/intermediates to have been kept intact.
        if ignore || !intermediates.is_empty() {
            return;
        }

        let (rows, cols) = self.active_grid().dimensions();
        let last_row = rows.saturating_sub(1);
        let last_col = cols.saturating_sub(1);

        match action {
            // CUU — cursor up; absent/zero count means one.
            'A' => {
                self.cursor.row = self.cursor.row.saturating_sub(move_count(params));
                self.cursor.pending_wrap = false;
            }
            // CUD — cursor down, clamped to the last row.
            'B' => {
                let n = move_count(params);
                self.cursor.row = self.cursor.row.saturating_add(n).min(last_row);
                self.cursor.pending_wrap = false;
            }
            // CUF — cursor forward, clamped to the last column.
            'C' => {
                let n = move_count(params);
                self.cursor.col = self.cursor.col.saturating_add(n).min(last_col);
                self.cursor.pending_wrap = false;
            }
            // CUB — cursor back.
            'D' => {
                self.cursor.col = self.cursor.col.saturating_sub(move_count(params));
                self.cursor.pending_wrap = false;
            }
            // CUP / HVP — absolute position; 1-based row;col arguments mapped to
            // 0-based coordinates and clamped into the grid.
            'H' | 'f' => {
                self.cursor.row = coord_param(params, 0).min(last_row);
                self.cursor.col = coord_param(params, 1).min(last_col);
                self.cursor.pending_wrap = false;
            }
            // ED — erase in display (cursor unmoved; `pending_wrap` untouched).
            'J' => {
                let (r, c) = (self.cursor.row, self.cursor.col);
                match first_param(params).unwrap_or(0) {
                    // Cursor to end of screen: rest of this row, then every row
                    // below.
                    0 => {
                        self.active_grid_mut().clear_line(r, c, cols);
                        for row in r.saturating_add(1)..rows {
                            self.active_grid_mut().clear_line(row, 0, cols);
                        }
                    }
                    // Start of screen to cursor: every row above, then this row
                    // through the cursor column inclusive.
                    1 => {
                        for row in 0..r {
                            self.active_grid_mut().clear_line(row, 0, cols);
                        }
                        self.active_grid_mut().clear_line(r, 0, c.saturating_add(1));
                    }
                    // Whole screen.
                    2 => {
                        for row in 0..rows {
                            self.active_grid_mut().clear_line(row, 0, cols);
                        }
                    }
                    // Erase scrollback only (an xterm extension). Scrollback
                    // storage is still a stub, so this is a no-op; the visible
                    // screen is deliberately left untouched.
                    3 => {}
                    // Unknown ED mode: ignored.
                    _ => {}
                }
            }
            // EL — erase in line (cursor unmoved; `pending_wrap` untouched).
            'K' => {
                let (r, c) = (self.cursor.row, self.cursor.col);
                match first_param(params).unwrap_or(0) {
                    // Cursor to end of line.
                    0 => self.active_grid_mut().clear_line(r, c, cols),
                    // Start of line through the cursor column inclusive.
                    1 => self.active_grid_mut().clear_line(r, 0, c.saturating_add(1)),
                    // Whole line.
                    2 => self.active_grid_mut().clear_line(r, 0, cols),
                    // Unknown EL mode: ignored.
                    _ => {}
                }
            }
            // Any other CSI final byte (SGR, DEC private modes, …) is not
            // handled yet; ignored rather than mis-applied.
            _ => {}
        }
    }
}

/// The first CSI parameter's primary value, or `None` if empty.
fn first_param(params: &vte::Params) -> Option<u16> {
    params.iter().next().and_then(|p| p.first().copied())
}

/// The `n`-th CSI parameter's primary value (0-based), or `None` when absent.
fn nth_param(params: &vte::Params, n: usize) -> Option<u16> {
    params.iter().nth(n).and_then(|p| p.first().copied())
}

/// A cursor-move distance: a missing argument or an explicit `0` both mean `1`.
fn move_count(params: &vte::Params) -> u16 {
    first_param(params).filter(|&v| v != 0).unwrap_or(1)
}

/// A 1-based CUP/HVP coordinate converted to 0-based: missing or `0` → `1`,
/// then decremented, so the default lands on the top-left cell `(0, 0)`.
fn coord_param(params: &vte::Params, n: usize) -> u16 {
    nth_param(params, n)
        .filter(|&v| v != 0)
        .unwrap_or(1)
        .saturating_sub(1)
}

#[cfg(test)]
mod tests;
