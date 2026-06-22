//! [`vte::Perform`] implementation that drives [`TerminalState`] from parsed
//! PTY output: printable glyphs land in the active grid at the cursor, and the
//! basic C0 control bytes move the cursor and scroll.
//!
//! Implemented so far: `print` (printable glyphs), `execute` (C0 control
//! bytes), `csi_dispatch` (cursor moves, erase, SGR, insert/delete char & line,
//! scroll up/down, the DECSTBM scroll region, and the DEC private modes for the
//! alternate screen and cursor visibility), `esc_dispatch` (cursor save/restore
//! and reverse index), and `osc_dispatch` (the OSC 0/1/2 window title). The
//! remaining `Perform` methods (`hook`/`put`/`unhook`) keep their default no-op
//! until later tasks fill them in. `vte` decodes UTF-8 upstream, so `print`
//! receives a ready `char`.

use crate::grid::state::Cell;
use crate::state::{SavedCursor, Screen, TerminalState};
use crate::style::{Color, Style};

impl TerminalState {
    /// The scroll-region margins as 0-based inclusive `(top, bottom)` rows,
    /// resolving `None` to the whole active grid.
    fn region_bounds(&self) -> (u16, u16) {
        let last_row = self.active_grid().dimensions().0.saturating_sub(1);
        self.scroll_region.unwrap_or((0, last_row))
    }

    /// Move the cursor down one line. At the scroll region's bottom margin the
    /// region scrolls up instead of the cursor advancing; below the margin the
    /// cursor just descends to the last grid row. The column is left unchanged
    /// (LNM is off, so a line feed is a pure vertical move).
    fn linefeed(&mut self) {
        let fill = self.style.bg_fill();
        let (top, bottom) = self.region_bounds();
        if self.cursor.row == bottom {
            self.active_grid_mut().delete_lines(top, bottom, 1, fill);
        } else {
            let last_row = self.active_grid().dimensions().0.saturating_sub(1);
            if self.cursor.row < last_row {
                self.cursor.row += 1;
            }
        }
    }

    /// Reverse index (RI): move the cursor up one line. At the scroll region's
    /// top margin the region scrolls down instead.
    fn reverse_index(&mut self) {
        let fill = self.style.bg_fill();
        let (top, bottom) = self.region_bounds();
        if self.cursor.row == top {
            self.active_grid_mut().insert_lines(top, bottom, 1, fill);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.cursor.pending_wrap = false;
    }

    /// Save the cursor position and pen style (DECSC / SCOSC) into the active
    /// screen's slot, so the primary and alternate screens snapshot separately.
    fn save_cursor(&mut self) {
        self.saved[self.active as usize] = Some(SavedCursor {
            row: self.cursor.row,
            col: self.cursor.col,
            style: self.style,
            pending_wrap: self.cursor.pending_wrap,
        });
    }

    /// Restore the cursor position and pen style saved by `save_cursor` (DECRC /
    /// SCORC). With no prior save, xterm homes the cursor and resets the pen to
    /// defaults; the restored position is clamped into the current grid in case
    /// it shrank since the save.
    fn restore_cursor(&mut self) {
        let (rows, cols) = self.active_grid().dimensions();
        let (last_row, last_col) = (rows.saturating_sub(1), cols.saturating_sub(1));
        match self.saved[self.active as usize] {
            Some(saved) => {
                self.cursor.row = saved.row.min(last_row);
                self.cursor.col = saved.col.min(last_col);
                self.style = saved.style;
                self.cursor.pending_wrap = saved.pending_wrap;
            }
            None => {
                self.cursor.row = 0;
                self.cursor.col = 0;
                self.style = Style::default();
                self.cursor.pending_wrap = false;
            }
        }
    }

    /// Blank every cell of the active grid, filling with the current pen
    /// background (BCE). Used when entering (`?1049`) or leaving (`?1047`) the
    /// alternate screen, mirroring the whole-screen ED 2 erase.
    fn clear_active_grid(&mut self) {
        let fill = self.style.bg_fill();
        let (rows, cols) = self.active_grid().dimensions();
        for row in 0..rows {
            self.active_grid_mut().clear_line(row, 0, cols, fill);
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
        // `ignore` flags a sequence with too many params/intermediates to have
        // been kept intact — drop it.
        if ignore {
            return;
        }

        // DEC private modes carry a `?` private marker, which vte collects into
        // `intermediates`. Handle the alternate-screen and cursor-visibility
        // modes here; any other `?`-mode is owned by a later task.
        if intermediates == b"?" {
            if let Some(mode) = first_param(params) {
                match (action, mode) {
                    // DECSET `?47`/`?1047` — switch to the alternate buffer.
                    ('h', 47 | 1047) => {
                        if self.active != Screen::Alternate {
                            self.active = Screen::Alternate;
                        }
                    }
                    // DECSET `?1049` — save the cursor, switch to the alternate
                    // buffer, then clear it.
                    ('h', 1049) => {
                        if self.active != Screen::Alternate {
                            self.save_cursor();
                            self.active = Screen::Alternate;
                            self.clear_active_grid();
                        }
                    }
                    // DECSET `?1048` — save the cursor only.
                    ('h', 1048) => self.save_cursor(),
                    // DECSET `?25` (DECTCEM) — show the cursor.
                    ('h', 25) => self.cursor.is_visible = true,
                    // DECRST `?47` — switch back to the primary buffer.
                    ('l', 47) => {
                        if self.active == Screen::Alternate {
                            self.active = Screen::Primary;
                        }
                    }
                    // DECRST `?1047` — clear the alternate buffer, then switch
                    // back to the primary.
                    ('l', 1047) => {
                        if self.active == Screen::Alternate {
                            self.clear_active_grid();
                            self.active = Screen::Primary;
                        }
                    }
                    // DECRST `?1049` — switch back to the primary buffer, then
                    // restore the saved cursor.
                    ('l', 1049) => {
                        if self.active == Screen::Alternate {
                            self.active = Screen::Primary;
                            self.restore_cursor();
                        }
                    }
                    // DECRST `?1048` — restore the saved cursor only.
                    ('l', 1048) => self.restore_cursor(),
                    // DECRST `?25` (DECTCEM) — hide the cursor.
                    ('l', 25) => self.cursor.is_visible = false,
                    // Any other DEC private mode is not handled yet.
                    _ => {}
                }
            }
            return;
        }

        // A non-`?` intermediate marks a sequence (charset, device query, …)
        // owned by a later task — skip it.
        if !intermediates.is_empty() {
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
                let fill = self.style.bg_fill();
                let (r, c) = (self.cursor.row, self.cursor.col);
                match first_param(params).unwrap_or(0) {
                    // Cursor to end of screen: rest of this row, then every row
                    // below.
                    0 => {
                        self.active_grid_mut().clear_line(r, c, cols, fill);
                        for row in r.saturating_add(1)..rows {
                            self.active_grid_mut().clear_line(row, 0, cols, fill);
                        }
                    }
                    // Start of screen to cursor: every row above, then this row
                    // through the cursor column inclusive.
                    1 => {
                        for row in 0..r {
                            self.active_grid_mut().clear_line(row, 0, cols, fill);
                        }
                        self.active_grid_mut()
                            .clear_line(r, 0, c.saturating_add(1), fill);
                    }
                    // Whole screen.
                    2 => {
                        for row in 0..rows {
                            self.active_grid_mut().clear_line(row, 0, cols, fill);
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
                let fill = self.style.bg_fill();
                let (r, c) = (self.cursor.row, self.cursor.col);
                match first_param(params).unwrap_or(0) {
                    // Cursor to end of line.
                    0 => self.active_grid_mut().clear_line(r, c, cols, fill),
                    // Start of line through the cursor column inclusive.
                    1 => self
                        .active_grid_mut()
                        .clear_line(r, 0, c.saturating_add(1), fill),
                    // Whole line.
                    2 => self.active_grid_mut().clear_line(r, 0, cols, fill),
                    // Unknown EL mode: ignored.
                    _ => {}
                }
            }
            // SGR — set graphic rendition: update the pen colors and text
            // attributes applied to subsequently printed cells.
            'm' => apply_sgr(&mut self.style, params),
            // ICH — insert n blank cells at the cursor, shifting the rest of the
            // line right; cells pushed past the right edge fall off.
            '@' => {
                let n = move_count(params);
                let fill = self.style.bg_fill();
                let (r, c) = (self.cursor.row, self.cursor.col);
                self.active_grid_mut().insert_cells(r, c, n, fill);
                self.cursor.pending_wrap = false;
            }
            // DCH — delete n cells at the cursor, pulling the rest of the line
            // left; the right end is refilled with blanks.
            'P' => {
                let n = move_count(params);
                let fill = self.style.bg_fill();
                let (r, c) = (self.cursor.row, self.cursor.col);
                self.active_grid_mut().delete_cells(r, c, n, fill);
                self.cursor.pending_wrap = false;
            }
            // SCOSC — save cursor (ANSI.SYS), companion to DECSC.
            's' => self.save_cursor(),
            // SCORC — restore cursor (ANSI.SYS), companion to DECRC.
            'u' => self.restore_cursor(),
            // IL — insert n blank lines at the cursor row, scrolling the rest of
            // the region down. Ignored when the cursor is outside the region; the
            // cursor position (row, column, and wrap latch) is left unchanged,
            // matching the DEC/xterm lineage that TUIs target.
            'L' => {
                let (top, bottom) = self.region_bounds();
                if (top..=bottom).contains(&self.cursor.row) {
                    let n = move_count(params);
                    let fill = self.style.bg_fill();
                    let r = self.cursor.row;
                    self.active_grid_mut().insert_lines(r, bottom, n, fill);
                }
            }
            // DL — delete n lines at the cursor row, scrolling the rest of the
            // region up. Same region guard and cursor handling as IL.
            'M' => {
                let (top, bottom) = self.region_bounds();
                if (top..=bottom).contains(&self.cursor.row) {
                    let n = move_count(params);
                    let fill = self.style.bg_fill();
                    let r = self.cursor.row;
                    self.active_grid_mut().delete_lines(r, bottom, n, fill);
                }
            }
            // SU — scroll the region up by n (`CSI Ps S`); the cursor stays put.
            'S' => {
                let n = move_count(params);
                let fill = self.style.bg_fill();
                let (top, bottom) = self.region_bounds();
                self.active_grid_mut().delete_lines(top, bottom, n, fill);
            }
            // SD — scroll the region down by n; the cursor stays put. `CSI Ps T`
            // is the common form, but `CSI <5 params> T` is xterm highlight mouse
            // tracking (a later task), so only T's 0/1-param form scrolls; `CSI Ps ^`
            // is the unambiguous ECMA-48 form and always scrolls.
            'T' | '^' => {
                if action == '^' || params.len() <= 1 {
                    let n = move_count(params);
                    let fill = self.style.bg_fill();
                    let (top, bottom) = self.region_bounds();
                    self.active_grid_mut().insert_lines(top, bottom, n, fill);
                }
            }
            // DECSTBM — set the top/bottom scroll margins (1-based; defaults are
            // the full screen). An invalid range (top not above bottom) is
            // ignored; a full-screen span clears the region to `None`. The cursor
            // is homed to the top-left.
            'r' => {
                let top = coord_param(params, 0).min(last_row);
                let bottom = nth_param(params, 1)
                    .filter(|&v| v != 0)
                    .map(|v| v - 1)
                    .unwrap_or(last_row)
                    .min(last_row);
                if top < bottom {
                    self.scroll_region = if top == 0 && bottom == last_row {
                        None
                    } else {
                        Some((top, bottom))
                    };
                    self.cursor.row = 0;
                    self.cursor.col = 0;
                    self.cursor.pending_wrap = false;
                }
            }
            // Any other CSI final byte (DEC private modes, device queries, …)
            // is not handled yet; ignored rather than mis-applied.
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // A plain ESC sequence carries no intermediate byte; an intermediate
        // marks a charset designation or other ESC form owned by a later task.
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            // DECSC — save cursor and pen.
            b'7' => self.save_cursor(),
            // DECRC — restore cursor and pen.
            b'8' => self.restore_cursor(),
            // RI — reverse index (reverse line feed).
            b'M' => self.reverse_index(),
            // Other ESC finals (charset selection, …) are not handled yet.
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0/1/2 set the window/icon title. `params[0]` is the command
        // number. vte splits the payload on every `;`, but for a title only the
        // first `;` is the command/text separator, so rejoin `params[1..]` with
        // `;` to keep a title that itself contains one. Decode lossily so a
        // non-UTF-8 title still shows. Other OSC commands (e.g. OSC 7 cwd) are
        // owned by later tasks.
        let Some(&command) = params.first() else {
            return;
        };
        if matches!(std::str::from_utf8(command), Ok("0" | "1" | "2")) && params.len() > 1 {
            let title = params[1..].join(&b';');
            self.title = Some(String::from_utf8_lossy(&title).into_owned());
        }
    }
}

/// Apply an SGR (Select Graphic Rendition, `CSI … m`) sequence to `style`:
/// update the pen colors and text attributes carried by subsequently printed
/// cells. Empty parameters are an implicit reset (equivalent to SGR `0`); the
/// extended-color selectors `38`/`48` are parsed by [`extended_color`].
fn apply_sgr(style: &mut Style, params: &vte::Params) {
    if params.is_empty() {
        style.reset();
        return;
    }

    let mut iter = params.iter();
    while let Some(p) = iter.next() {
        // Dispatch on the SGR code number `p.first()`; an empty parameter (e.g.
        // `CSI ;m`) carries no value, so `unwrap_or(0)` makes it code 0 (reset).
        // Each arm's comment names the code so the mapping reads without the spec.
        match p.first().copied().unwrap_or(0) {
            0 => style.reset(),               // 0: reset all attributes + colors
            1 => style.set_bold(true),        // 1: bold
            3 => style.set_italic(true),      // 3: italic
            4 => style.set_underline(true),   // 4: underline
            7 => style.set_reverse(true),     // 7: reverse video (swap fg/bg)
            22 => style.set_bold(false),      // 22: bold off (normal intensity; no faint attr)
            23 => style.set_italic(false),    // 23: italic off
            24 => style.set_underline(false), // 24: underline off
            27 => style.set_reverse(false),   // 27: reverse off
            c @ 30..=37 => style.set_fg(Color::Indexed((c - 30) as u8)), // 30-37: fg palette 0-7
            c @ 90..=97 => style.set_fg(Color::Indexed((c - 90 + 8) as u8)), // 90-97: bright fg 8-15
            39 => style.set_fg(Color::Default),                              // 39: default fg
            c @ 40..=47 => style.set_bg(Color::Indexed((c - 40) as u8)), // 40-47: bg palette 0-7
            c @ 100..=107 => style.set_bg(Color::Indexed((c - 100 + 8) as u8)), // 100-107: bright bg 8-15
            49 => style.set_bg(Color::Default),                                 // 49: default bg
            // 38: extended fg — 256-palette (`38;5;n`) or truecolor (`38;2;r;g;b`).
            38 => {
                if let Some(col) = extended_color(p, &mut iter) {
                    style.set_fg(col);
                }
            }
            // 48: extended bg — 256-palette (`48;5;n`) or truecolor (`48;2;r;g;b`).
            48 => {
                if let Some(col) = extended_color(p, &mut iter) {
                    style.set_bg(col);
                }
            }
            _ => {} // unknown / out-of-scope SGR code: ignore
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

/// The primary value of the iterator's next CSI parameter, or `None` when the
/// iterator is exhausted. Used to walk the separate params of a semicolon-form
/// extended color (`38;5;n` / `38;2;r;g;b`).
fn next_val<'a>(iter: &mut impl Iterator<Item = &'a [u16]>) -> Option<u16> {
    iter.next().and_then(|p| p.first().copied())
}

/// Parse a `38` (foreground) or `48` (background) extended-color payload into a
/// [`Color`], for whichever of the two wire forms `vte` produced:
///
/// - **colon** — `38:5:n` / `38:2:r:g:b`: the selector and values are
///   subparameters grouped into the single `first` slice (`first[0]` is the
///   `38`/`48`), so everything is read from `first`.
/// - **semicolon** — `38;5;n` / `38;2;r;g;b`: the selector and values are
///   separate following parameters, pulled in turn from `iter`.
///
/// Selector `5` is a 256-color palette index; selector `2` is 24-bit RGB. A
/// missing or unrecognized payload yields `None`, leaving the pen unchanged.
fn extended_color<'a>(first: &[u16], iter: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    if first.len() > 1 {
        // Colon form: selector at first[1], its values follow in the same slice.
        match first.get(1).copied()? {
            // `38:5:n` — 256-palette index sits at first[2].
            5 => Some(Color::Indexed(*first.get(2)? as u8)),
            2 => {
                // The colon RGB form may carry a leading colorspace id
                // (`38:2::r:g:b`, whose empty field `vte` stores as `0`), so the
                // real r, g, b are always the last three subparameters.
                let vals = &first[2..];
                let rgb = if vals.len() >= 4 {
                    &vals[vals.len() - 3..]
                } else {
                    vals
                };
                Some(Color::Rgb(
                    *rgb.first()? as u8,
                    *rgb.get(1)? as u8,
                    *rgb.get(2)? as u8,
                ))
            }
            _ => None,
        }
    } else {
        // Semicolon form: selector then values are the next separate params.
        match next_val(iter)? {
            // `38;5;n` — one following param is the 256-palette index.
            5 => Some(Color::Indexed(next_val(iter)? as u8)),
            // `38;2;r;g;b` — three following params are the RGB channels.
            2 => Some(Color::Rgb(
                next_val(iter)? as u8,
                next_val(iter)? as u8,
                next_val(iter)? as u8,
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
