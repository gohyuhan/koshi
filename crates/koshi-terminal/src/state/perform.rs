//! [`vte::Perform`] implementation that drives [`TerminalState`] from parsed
//! PTY output: printable glyphs land in the active grid at the cursor, and
//! control sequences move the cursor, style text, and switch modes. `vte`
//! decodes UTF-8 upstream; `print` receives a `char`.
//!
//! What each callback does:
//!
//! - `print` — printable glyphs. Each is translated through the active GL
//!   charset (whichever of the `G0`–`G3` slots is selected for printing: DEC
//!   line-drawing, UK), then placed display-width aware: wide CJK and emoji
//!   span two cells, and grapheme continuations — combining marks, ZWJ
//!   (zero-width joiner) emoji sequences, variation selectors, skin tones,
//!   flags — fold onto the base cell.
//! - `execute` — C0 and C1 control bytes: newline, tab, backspace, line
//!   movement, tab-stop setup, and the `SI`/`SO` charset shifts.
//! - `csi_dispatch` — CSI sequences (Control Sequence Introducer, `ESC [ …`):
//!   cursor moves (relative, absolute, line-relative), tab stops, erase in
//!   display/line and erase-char, including active image placement clearing
//!   for whole-screen erase, SGR text attributes (Select Graphic
//!   Rendition: color, bold, underline, …), insert/delete char and line,
//!   scroll up/down, the DECSTBM scroll region, the DEC private modes
//!   (alternate screen, cursor visibility, …), and the device queries
//!   (DA1/DA2/DA3, the DSR family, DECRQM), whose replies land on the
//!   state's reply queue.
//! - `esc_dispatch` — plain ESC sequences: cursor save/restore, line movement,
//!   tab-stop setup, terminal reset, and `G0`–`G3` charset designation.
//! - `osc_dispatch` — OSC sequences (Operating System Command, `ESC ] …`,
//!   carrying a text payload): the OSC 0/1/2 window title, OSC 7
//!   working-directory report, and OSC 133 shell markers.
//! - `hook`/`unhook` — start/end of a DCS (device control string,
//!   `ESC P … ST`). They only clear the in-progress grapheme cluster (a DCS
//!   ends a text run like any non-printing event); the payload and `put`
//!   change nothing.
//!
//! The performer's helpers are split across submodules by concern — charset
//! translation ([`charset`]), device-query replies ([`device`]), grapheme
//! clustering and wide-glyph placement ([`glyph`]), cursor motion / scrolling
//! / the scroll region ([`motion`]), alternate-screen entry/exit
//! ([`alt_screen`]), hard/soft reset ([`reset`]), SGR ([`sgr`]), OSC parsing
//! ([`osc`]), and CSI parameter accessors ([`params`]) — while the
//! [`vte::Perform`] trait impl itself stays here as the dispatch surface.

use koshi_core::text::sanitize_reported_text;

use crate::grid::state::{Cell, RowEnd};
use crate::state::{
    CursorShape, MouseEncoding, MouseTracking, Screen, ShellIntegrationFact, ShellIntegrationState,
    TerminalState,
};
use unicode_width::UnicodeWidthChar;

use self::motion::{next_tab_stop, prev_tab_stop};
use self::osc::{parse_osc133, parse_osc7_cwd, Osc133};
use self::params::{coord_param, first_param, move_count, nth_param};
use self::sgr::apply_sgr;

mod alt_screen;
mod charset;
mod device;
mod glyph;
mod motion;
mod osc;
mod params;
mod reset;
mod sgr;

impl vte::Perform for TerminalState {
    /// Print a displayable character to the active grid. Translates the
    /// character through the active GL charset (DEC line-drawing, UK, or
    /// passthrough), folds continuations (combining marks, ZWJ emoji parts,
    /// variation selectors) onto the preceding base, handles display width
    /// (narrow single-column or wide CJK/emoji two-column), and respects
    /// autowrap at the line end.
    fn print(&mut self, c: char) {
        // Translate through the active GL charset (DEC line drawing, UK). The
        // cell stores the remapped glyph and width is computed on it; the
        // result is always a narrow, non-combining char.
        let c = self.map_charset(c);

        // A continuation (combining mark, ZWJ-joined emoji part, variation
        // selector, skin-tone modifier, flag half) folds onto the current
        // cluster's base without occupying a cell of its own.
        if !self.cluster.is_empty() && self.continues_cluster(c) {
            self.extend_cluster(c);
            return;
        }

        // `c` starts a new grapheme. A control char that slipped past `execute`
        // has no display width (`None`) → ignore it. A zero-width char with no
        // cluster to join (e.g. a combining mark at the very start of a line)
        // has no base to attach to → drop it. Every other glyph is narrow (1)
        // or wide (2, e.g. CJK / emoji); `unicode-width` treats ambiguous-width
        // characters as narrow.
        let Some(raw_width) = c.width() else {
            // The reset ends the text run: a continuation that follows starts
            // fresh.
            self.reset_cluster();
            return;
        };
        if raw_width == 0 {
            // A zero-width char that did not continue the cluster is a grapheme
            // boundary (e.g. ZWSP `U+200B`). The reset ends the text run: a
            // combining mark or VS16 that follows starts fresh.
            self.reset_cluster();
            return;
        }
        let glyph_width: u16 = if raw_width >= 2 { 2 } else { 1 };

        // Deferred wrap: a prior print parked on the last column. Under autowrap
        // (DECAWM `?7`, the default) the cursor wraps to the next line before
        // this glyph is placed (a row that exactly fills the width scrolls only
        // when the next glyph arrives). With autowrap off the cursor stays on
        // the last column and this glyph overwrites in place. Either way the
        // latch clears.
        if self.active_cursor().pending_wrap {
            if self.modes.autowrap {
                // The row the cursor leaves soft-wraps into the next, including
                // when a bottom-margin scroll moves it above a fresh blank row.
                self.wrap_linefeed(RowEnd::Soft);
                self.active_cursor_mut().col = 0;
            }
            self.clear_wrap_latch();
        }

        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);
        let style = self.active_render().style;

        // A wide glyph at the last column of a multi-column pane: blank that
        // column and wrap, and the glyph begins the next line as one whole
        // cell. In a 1-column pane (`last_col == 0`) this is skipped and
        // `place_glyph` stores the glyph narrow in place.
        if glyph_width == 2 && self.active_cursor().col == last_col && last_col > 0 {
            // With autowrap off the glyph is dropped: the cursor rests on the
            // last column with no wrap armed, and the next glyph overwrites
            // there. The cluster resets: a combining mark that follows does not
            // fold onto the previous cell.
            if !self.modes.autowrap {
                self.reset_cluster();
                return;
            }
            let row = self.active_cursor().row;
            // When the last column is the continuation of a wide glyph, its base
            // one column to the left is cleared too.
            self.clear_wide_at(row, last_col);
            if let Some(cell) = self.active_grid_mut().cell_mut(row, last_col) {
                *cell = Cell::blank_with(style.bg_fill());
            }
            // The freed last column is a wide-glyph spacer; `SoftWide` marks the
            // row so a reflow re-joins the rows and drops the spacer.
            self.wrap_linefeed(RowEnd::SoftWide);
            self.active_cursor_mut().col = 0;
            self.clear_wrap_latch();
        }

        let row = self.active_cursor().row;
        let col = self.active_cursor().col;

        // Install the base glyph (and, when wide, its continuation), clearing any
        // wide pair the write would split — see `place_glyph`.
        self.place_glyph(row, col, Cell::new(c, glyph_width as u8, style));

        // Anchor a new cluster at this base; continuations that follow
        // (combining marks, ZWJ emoji parts, …) fold onto it.
        self.cluster.clear();
        self.cluster.push(c);
        self.cluster_base = Some((row, col));

        // Advance past the glyph: park on the last column when the glyph reached
        // it (under autowrap with the wrap latch armed), else step to the first
        // free column after it.
        let end_col = col + glyph_width - 1;
        if end_col >= last_col {
            self.arm_wrap_latch(last_col);
        } else {
            self.active_cursor_mut().col = end_col + 1;
        }
    }

    /// Handle C0 and C1 controls that move the cursor, manage tab stops,
    /// select a charset slot, or ring the bell.
    fn execute(&mut self, byte: u8) {
        // A control byte ends any text run, so no following glyph folds into it.
        self.reset_cluster();
        match byte {
            // LF, VT, FF, IND: move down one line, scrolling at the bottom
            // margin (VT and FF act as LF).
            0x0A..=0x0C | 0x84 => {
                self.linefeed();
                self.clear_wrap_latch();
            }
            // CR: carriage return to column 0.
            0x0D => {
                self.active_cursor_mut().col = 0;
                self.clear_wrap_latch();
            }
            // BS: backspace one column (no erase).
            0x08 => {
                self.active_cursor_mut().col = self.active_cursor().col.saturating_sub(1);
                self.clear_wrap_latch();
            }
            // HT: advance to the next stored tab stop, clamped to the grid.
            0x09 => {
                let (_, cols) = self.active_grid().dimensions();
                let last_col = cols.saturating_sub(1);
                let col = self.active_cursor().col;
                let next = next_tab_stop(&self.tab_stops, col, last_col);
                self.active_cursor_mut().col = next;
                self.clear_wrap_latch();
            }
            // NEL: move down one line, then return to column zero.
            0x85 => {
                self.linefeed();
                self.active_cursor_mut().col = 0;
                self.clear_wrap_latch();
            }
            // HTS: set a horizontal tab stop at the cursor.
            0x88 => self.set_tab_stop(),
            // RI: move up one line, scrolling at the top margin.
            0x8D => self.reverse_index(),
            // SO (shift out): select G1 into the GL range for printing.
            0x0E => self.active_render_mut().gl = 1,
            // SI (shift in): select G0 into the GL range for printing.
            0x0F => self.active_render_mut().gl = 0,
            // BEL (0x07) and any other control byte: discarded, never rendered.
            _ => {}
        }
    }

    /// Handle a CSI sequence: cursor movement (CUU/CUD/CUF/CUB/CUP/HVP/HPA/VPA/
    /// CNL/CPL/CHT/CBT), erase in display/line/character (ED/EL/ECH), with
    /// whole-screen erase also clearing active image placements, graphics
    /// rendition (SGR), cell/line operations (ICH/DCH/IL/DL), scroll (SU/SD),
    /// scroll region setup (DECSTBM), DEC private modes including alternate
    /// screen (`?47`/`?1047`/`?1049`), cursor visibility (`?25`/DECTCEM), mouse
    /// tracking/encoding, bracketed paste, autowrap (`?7`/DECAWM), and the
    /// device queries (DA1/DA2/DA3, the DSR family, DECRQM/RQM) that queue
    /// reply bytes for the app.
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        // Every CSI but a style-only SGR ends the text run. A style-only SGR
        // (`CSI Pm m`: no intermediates, not flagged `ignore` — the same
        // condition the SGR arm below applies) changes only the pen: a combining
        // mark or variation selector after it still folds onto the preceding
        // base (`e \x1b[31m \u{0301}` → an accented `e`). An overlong CSI
        // flagged `ignore` breaks the cluster even when it ends in `m`.
        let style_only_sgr = action == 'm' && intermediates.is_empty() && !ignore;
        if !style_only_sgr {
            self.reset_cluster();
        }
        // `ignore` flags a sequence with more params or intermediates than vte
        // keeps; it is dropped.
        if ignore {
            return;
        }

        // Device queries (DA1/DA2/DA3, DSR, DECRQM/RQM) and DECSCUSR, matched on
        // the exact (intermediates, action) pair. Each query queues its reply
        // bytes for the runtime to write back into the PTY. This match runs
        // first: the `?` private-mode block below also matches the `?`
        // intermediate of `CSI ? Ps n` and `CSI ? Ps $ p`. `CSI ! p` falls
        // through to the soft-reset check.
        match (intermediates, action) {
            // DA1 — primary device attributes.
            (b"", 'c') => return self.device_attributes_primary(params),
            // DA2 — secondary device attributes.
            (b">", 'c') => return self.device_attributes_secondary(params),
            // DA3 — tertiary device attributes (unit id).
            (b"=", 'c') => return self.device_attributes_tertiary(params),
            // DSR — operating status (5) / cursor position report (6).
            (b"", 'n') => return self.device_status_report(params),
            // DEC-form DSR — cursor position, printer, UDK, keyboard,
            // locator, macro space, checksum, data integrity, multi-session.
            (b"?", 'n') => return self.dec_device_status_report(params),
            // DECRQM — request DEC private mode state.
            (b"?$", 'p') => return self.report_dec_mode(params),
            // RQM, ANSI form — request ANSI mode state.
            (b"$", 'p') => return self.report_ansi_mode(params),
            // DECSCUSR — set the cursor style. The SPACE intermediate is part
            // of the sequence (`CSI Ps SP q`), not padding.
            (b" ", 'q') => return self.set_cursor_style(params),
            _ => {}
        }

        // DECSTR — soft terminal reset. `vte` represents its absent parameter
        // as one zero value; any nonzero or additional parameter is not DECSTR.
        if intermediates == b"!"
            && action == 'p'
            && params.len() <= 1
            && first_param(params).unwrap_or(0) == 0
        {
            self.soft_reset();
            return;
        }

        // DEC private modes: vte collects the `?` marker into `intermediates`.
        // DECSET/DECRST take a parameter list (`CSI ? Pm h/l`); every mode in
        // the list is applied, and a mode with no arm changes nothing.
        if intermediates == b"?" {
            // Modes in one list apply left to right, each taking effect at once:
            // per-screen state like `?25` visibility lands on whichever screen
            // is active at that point in the list. Screen switches read the
            // live `self.active`: a second swap in the same list is a no-op once
            // the first has flipped — a trailing `?1047 l` after `?1049 l` does
            // not re-clear. `?1049 h` entry reads `screen_at_start` instead: it
            // saves the primary cursor and freshens the alternate whenever the
            // list began on the primary, even after an earlier `?47` in the
            // same list already swapped. A repeated `?1049 h` in one list
            // re-runs the entry with the same result.
            let screen_at_start = self.active;
            for param in params.iter() {
                let mode = param.first().copied().unwrap_or(0);
                self.apply_dec_private_mode(action, mode, screen_at_start);
            }
            return;
        }

        // Any other intermediate (DECSCA `"q`, a non-DECSTR `!p`, …) is
        // ignored.
        if !intermediates.is_empty() {
            return;
        }

        let (rows, cols) = self.active_grid().dimensions();
        let last_row = rows.saturating_sub(1);
        let last_col = cols.saturating_sub(1);

        match action {
            // CUU — cursor up; absent/zero count means one.
            'A' => {
                self.active_cursor_mut().row =
                    self.active_cursor().row.saturating_sub(move_count(params));
                self.clear_wrap_latch();
            }
            // CUD / VPR — cursor down, clamped to the last row (VPR `e` is the
            // same vertical move as CUD).
            'B' | 'e' => {
                let n = move_count(params);
                self.active_cursor_mut().row =
                    self.active_cursor().row.saturating_add(n).min(last_row);
                self.clear_wrap_latch();
            }
            // CUF / HPR — cursor forward, clamped to the last column (HPR `a` is
            // the same horizontal move as CUF).
            'C' | 'a' => {
                let n = move_count(params);
                self.active_cursor_mut().col =
                    self.active_cursor().col.saturating_add(n).min(last_col);
                self.clear_wrap_latch();
            }
            // CUB — cursor back.
            'D' => {
                self.active_cursor_mut().col =
                    self.active_cursor().col.saturating_sub(move_count(params));
                self.clear_wrap_latch();
            }
            // CUP / HVP — absolute position; 1-based row;col arguments mapped to
            // 0-based coordinates and clamped into the grid (via `goto`).
            'H' | 'f' => self.goto(coord_param(params, 0), coord_param(params, 1)),
            // CHA / HPA — absolute column on the current row; 1-based → 0-based.
            'G' | '`' => {
                let row = self.active_cursor().row;
                self.goto(row, coord_param(params, 0));
            }
            // VPA — absolute row in the current column; 1-based → 0-based.
            'd' => {
                let col = self.active_cursor().col;
                self.goto(coord_param(params, 0), col);
            }
            // CNL — cursor next line: n rows down (clamped, no scroll) to col 0.
            'E' => {
                let row = self.active_cursor().row.saturating_add(move_count(params));
                self.goto(row, 0);
            }
            // CPL — cursor previous line: n rows up (clamped, no scroll) to col 0.
            'F' => {
                let row = self.active_cursor().row.saturating_sub(move_count(params));
                self.goto(row, 0);
            }
            // CHT — advance n stored tab stops, clamped to the last column.
            'I' => {
                let mut col = self.active_cursor().col;
                for _ in 0..move_count(params) {
                    if col >= last_col {
                        break;
                    }
                    col = next_tab_stop(&self.tab_stops, col, last_col);
                }
                self.active_cursor_mut().col = col;
                self.clear_wrap_latch();
            }
            // CBT — retreat n stored tab stops, floored at column zero.
            'Z' => {
                let mut col = self.active_cursor().col;
                for _ in 0..move_count(params) {
                    if col == 0 {
                        break;
                    }
                    col = prev_tab_stop(&self.tab_stops, col);
                }
                self.active_cursor_mut().col = col;
                self.clear_wrap_latch();
            }
            // TBC — clear the current tab stop (default/0) or every stop (3).
            // Only the first parameter applies.
            'g' => match first_param(params).unwrap_or(0) {
                0 => self.clear_tab_stop(),
                3 => self.clear_all_tab_stops(),
                _ => {}
            },
            // ED — erase in display (cursor unmoved; an erasing mode clears the
            // wrap latch, see below).
            'J' => {
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                let mode = first_param(params).unwrap_or(0);
                match mode {
                    // Cursor to end of screen: rest of this row, then every row
                    // below. A row erased end to end also loses its prompt
                    // mark; the partly erased cursor row keeps its own.
                    0 => {
                        let grid = self.active_grid_mut();
                        grid.clear_line(r, c, cols, fill);
                        for row in r.saturating_add(1)..rows {
                            grid.clear_line(row, 0, cols, fill);
                            grid.set_prompt_mark(row, false);
                        }
                    }
                    // Start of screen to cursor: every row above, then this row
                    // through the cursor column inclusive.
                    1 => {
                        let grid = self.active_grid_mut();
                        for row in 0..r {
                            grid.clear_line(row, 0, cols, fill);
                            grid.set_prompt_mark(row, false);
                        }
                        grid.clear_line(r, 0, c.saturating_add(1), fill);
                    }
                    // Whole screen.
                    2 => {
                        let grid = self.active_grid_mut();
                        for row in 0..rows {
                            grid.clear_line(row, 0, cols, fill);
                            grid.set_prompt_mark(row, false);
                        }
                        self.clear_active_image_placements();
                    }
                    // Erase scrollback only (xterm "erase saved lines"): drop
                    // the retained history and its primary image placements,
                    // leaving the visible screen as it is. Primary screen
                    // only: on the alternate screen ED 3 falls through to the
                    // `_` arm and changes nothing.
                    3 if self.active == Screen::Primary => {
                        self.scrollback.clear();
                        self.clear_primary_image_history();
                    }
                    // Unknown ED mode: ignored.
                    _ => {}
                }
                // ED 0/1/2 erase the cursor's cell and clear the wrap latch. ED 3
                // and unknown modes leave the grid and the latch as they are.
                if matches!(mode, 0..=2) {
                    self.clear_wrap_latch();
                }
                // Only the cursor row can be partially cleared; repair its wide
                // pairs.
                self.normalize_wide_pairs(r);
            }
            // EL — erase in line (cursor unmoved; an erasing mode clears the wrap
            // latch, see below).
            'K' => {
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                let mode = first_param(params).unwrap_or(0);
                match mode {
                    // Cursor to end of line.
                    0 => self.active_grid_mut().clear_line(r, c, cols, fill),
                    // Start of line through the cursor column inclusive.
                    1 => self
                        .active_grid_mut()
                        .clear_line(r, 0, c.saturating_add(1), fill),
                    // Whole line: the row is erased end to end and loses its
                    // prompt mark.
                    2 => {
                        let grid = self.active_grid_mut();
                        grid.clear_line(r, 0, cols, fill);
                        grid.set_prompt_mark(r, false);
                    }
                    // Unknown EL mode: ignored.
                    _ => {}
                }
                // EL 0/1/2 erase the cursor's cell and clear the wrap latch; the
                // next print writes that cell with no wrap pending. An unknown
                // mode erases nothing and leaves the latch as it is.
                if matches!(mode, 0..=2) {
                    self.clear_wrap_latch();
                }
                self.normalize_wide_pairs(r);
            }
            // ECH — erase n cells in place from the cursor (BCE, background color
            // erase: the vacated cells take the pen's current background), no
            // shift of the rest of the line, then repair any wide-glyph pair the
            // erase split. Clears the wrap latch.
            'X' => {
                let n = move_count(params);
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                let end = c.saturating_add(n).min(cols);
                self.active_grid_mut().clear_line(r, c, end, fill);
                self.clear_wrap_latch();
                self.normalize_wide_pairs(r);
            }
            // SGR — set graphic rendition: update the pen colors and text
            // attributes applied to subsequently printed cells.
            'm' => apply_sgr(&mut self.active_render_mut().style, params),
            // ICH — insert n blank cells at the cursor, shifting the rest of the
            // line right; cells pushed past the right edge fall off.
            '@' => {
                let n = move_count(params);
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                self.active_grid_mut().insert_cells(r, c, n, fill);
                self.normalize_wide_pairs(r);
                self.clear_wrap_latch();
            }
            // DCH — delete n cells at the cursor, pulling the rest of the line
            // left; the right end is refilled with blanks.
            'P' => {
                let n = move_count(params);
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                self.active_grid_mut().delete_cells(r, c, n, fill);
                self.normalize_wide_pairs(r);
                self.clear_wrap_latch();
            }
            // SCOSC — save cursor (ANSI.SYS), companion to DECSC.
            's' => self.save_cursor(),
            // SCORC — restore cursor (ANSI.SYS), companion to DECRC.
            'u' => self.restore_cursor(),
            // IL — insert n blank lines at the cursor row, scrolling the rest of
            // the region down. Ignored when the cursor is outside the region.
            // The cursor (row, column, wrap latch) is left unchanged.
            'L' => {
                let (top, bottom) = self.region_bounds();
                if (top..=bottom).contains(&self.active_cursor().row) {
                    let n = move_count(params);
                    let fill = self.active_render().style.bg_fill();
                    let r = self.active_cursor().row;
                    self.insert_lines_preserving_images(r, bottom, n, fill);
                }
            }
            // DL — delete n lines at the cursor row, scrolling the rest of the
            // region up. Same region guard and cursor handling as IL.
            'M' => {
                let (top, bottom) = self.region_bounds();
                if (top..=bottom).contains(&self.active_cursor().row) {
                    let n = move_count(params);
                    let fill = self.active_render().style.bg_fill();
                    let r = self.active_cursor().row;
                    self.delete_lines_into_scrollback(r, bottom, n, fill);
                }
            }
            // SU — scroll the region up by n (`CSI Ps S`); the cursor stays put.
            'S' => {
                let n = move_count(params);
                let fill = self.active_render().style.bg_fill();
                let (top, bottom) = self.region_bounds();
                self.delete_lines_into_scrollback(top, bottom, n, fill);
            }
            // SD — scroll the region down by n; the cursor stays put. `CSI Ps T`
            // scrolls only with 0 or 1 parameter (`CSI <5 params> T` is xterm
            // highlight mouse tracking); `CSI Ps ^` (ECMA-48) always scrolls.
            'T' | '^' => {
                if action == '^' || params.len() <= 1 {
                    let n = move_count(params);
                    let fill = self.active_render().style.bg_fill();
                    let (top, bottom) = self.region_bounds();
                    self.insert_lines_preserving_images(top, bottom, n, fill);
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
                    .map_or(last_row, |v| v - 1)
                    .min(last_row);
                if top < bottom {
                    let region = if top == 0 && bottom == last_row {
                        None
                    } else {
                        Some((top, bottom))
                    };
                    match self.active {
                        Screen::Primary => self.primary_scroll_region = region,
                        Screen::Alternate => self.alternate_scroll_region = region,
                    }
                    self.active_cursor_mut().row = 0;
                    self.active_cursor_mut().col = 0;
                    self.clear_wrap_latch();
                }
            }
            // Any other CSI final byte is ignored.
            _ => {}
        }
    }

    /// Handle charset designation, cursor save/restore, line movement,
    /// tab-stop setup, and terminal reset ESC sequences.
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // Any ESC sequence ends a text run, so no following glyph folds into it.
        self.reset_cluster();
        if ignore {
            return;
        }
        // Charset designation: `ESC (`/`)`/`*`/`+` Fc designates G0/G1/G2/G3.
        // vte collects the `(`/`)`/`*`/`+` into `intermediates`; the final `byte`
        // names the set. The plain-ESC match below runs only with no
        // intermediate.
        match intermediates {
            b"(" => return self.designate_charset(0, byte),
            b")" => return self.designate_charset(1, byte),
            b"*" => return self.designate_charset(2, byte),
            b"+" => return self.designate_charset(3, byte),
            // Any other intermediate is ignored.
            [_, ..] => return,
            // No intermediate: fall through to the plain-ESC finals below.
            [] => {}
        }
        match byte {
            // DECSC — save cursor and pen.
            b'7' => self.save_cursor(),
            // DECRC — restore cursor and pen.
            b'8' => self.restore_cursor(),
            // IND — move down one line, scrolling at the bottom margin.
            b'D' => {
                self.linefeed();
                self.clear_wrap_latch();
            }
            // NEL — move down one line, then return to column zero.
            b'E' => {
                self.linefeed();
                self.active_cursor_mut().col = 0;
                self.clear_wrap_latch();
            }
            // HTS — set a horizontal tab stop at the cursor.
            b'H' => self.set_tab_stop(),
            // RI — reverse index (reverse line feed).
            b'M' => self.reverse_index(),
            // RIS — restore terminal display state to its initial values.
            b'c' => self.hard_reset(),
            // Any other ESC final is ignored.
            _ => {}
        }
    }

    /// Handle an Operating System Command (OSC) sequence: window/icon title
    /// (OSC 0/1/2), working-directory report (OSC 7, `file://` URI), or shell
    /// marker (OSC 133).
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // Any OSC ends a text run, so no following glyph folds into it.
        self.reset_cluster();
        if let Some(marker) = parse_osc133(params) {
            match marker {
                Osc133::Prompt => {
                    let row = self.active_cursor().row;
                    self.active_grid_mut().set_prompt_mark(row, true);
                    self.shell_integration_state = ShellIntegrationState::Prompt;
                }
                Osc133::Input => {
                    self.shell_integration_state = ShellIntegrationState::Input;
                }
                Osc133::CommandStart => {
                    if self.shell_integration_state != ShellIntegrationState::Running {
                        self.shell_integration_state = ShellIntegrationState::Running;
                        self.shell_integration_facts
                            .push(ShellIntegrationFact::CommandStarted);
                    }
                }
                Osc133::CommandFinished(exit_code) => {
                    if self.shell_integration_state == ShellIntegrationState::Running {
                        self.shell_integration_state = ShellIntegrationState::Prompt;
                        self.shell_integration_facts
                            .push(ShellIntegrationFact::CommandFinished { exit_code });
                    }
                }
            }
            return;
        }
        // `params[0]` is the command number. vte splits the payload on every
        // `;`; each arm rejoins `params[1..]` with `;`, and a payload that
        // itself holds a `;` stays whole.
        let Some(&command) = params.first() else {
            return;
        };
        match std::str::from_utf8(command) {
            // OSC 0/1/2 — set the window/icon title: lossy UTF-8 decode (a
            // non-UTF-8 title keeps replacement characters), then bounded and
            // filtered by `sanitize_reported_text`.
            Ok("0" | "1" | "2") if params.len() > 1 => {
                let title = params[1..].join(&b';');
                let title = String::from_utf8_lossy(&title);
                self.title = Some(sanitize_reported_text(&title));
            }
            // OSC 7 — the shell's working directory as a `file://host/path`
            // URI. An unparseable URI leaves the last cwd unchanged.
            Ok("7") if params.len() > 1 => {
                let uri = params[1..].join(&b';');
                if let Some(cwd) = parse_osc7_cwd(&uri) {
                    self.reported_cwd = Some(cwd);
                }
            }
            // Any other OSC command is ignored.
            _ => {}
        }
    }

    /// Begin a device control string (DCS, `ESC P … ST`): clear any
    /// in-progress grapheme cluster. A combining mark or variation selector
    /// after the DCS does not fold onto the glyph before it. The body bytes
    /// arrive through `put` and print nothing; the DCS payload changes
    /// nothing.
    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.reset_cluster();
    }

    /// End a device control string (DCS): clear any in-progress grapheme
    /// cluster. A DCS closed by the 8-bit C1 ST (`0x9C`) reaches no other
    /// callback — neither `esc_dispatch` nor `execute`.
    fn unhook(&mut self) {
        self.reset_cluster();
    }
}

impl TerminalState {
    /// Apply one DEC private mode from a DECSET (`h`) / DECRST (`l`) list — the
    /// `25` in `CSI ? 25 h`, for instance. `screen_at_start` is the screen that
    /// was active when the list began: `?1049` entry runs when it is not the
    /// alternate, even after an earlier `?47` in the same list swapped; every
    /// other switch reads the live `self.active`. An unrecognized mode changes
    /// nothing.
    ///
    /// `apply_dec_private_mode('h', 25, Primary)` shows the cursor on the active
    /// screen; `apply_dec_private_mode('l', 1000, _)` turns Normal mouse
    /// tracking off only when Normal is the level currently set, and changes
    /// nothing when another level is set.
    fn apply_dec_private_mode(&mut self, action: char, mode: u16, screen_at_start: Screen) {
        match (action, mode) {
            // DECSET `?47`/`?1047` — switch to the alternate buffer, leaving
            // its cells and cursor untouched. Crossing from the primary clones
            // the primary's render state (pen, charsets, GL slot) into the
            // alternate.
            ('h', 47 | 1047) => {
                if self.active == Screen::Primary {
                    self.alternate_render = self.primary_render;
                }
                self.active = Screen::Alternate;
            }
            // DECSET `?1049` — DECSC the primary cursor (the primary fields
            // directly, whichever screen is live), clone the primary's render
            // state into the alternate, reset the alternate to a fresh buffer,
            // seed its cursor position from the primary, then switch. Runs
            // when the list began off the alternate, even after an earlier
            // `?47` in the same list swapped. The alternate inherits no cells,
            // cursor, wrap latch, saved cursor, or scroll region from a
            // previous session.
            ('h', 1049) => {
                if screen_at_start != Screen::Alternate {
                    self.save_primary_cursor();
                    // Clone the primary's render state into the alternate.
                    self.alternate_render = self.primary_render;
                    self.reset_alternate_buffer();
                    self.seed_alternate_cursor();
                    self.active = Screen::Alternate;
                }
            }
            // DECSET `?1048` — save the active screen's cursor only.
            ('h', 1048) => self.save_cursor(),
            // DECSET `?25` (DECTCEM) — show the cursor. Visibility is per
            // screen: this sets only the active screen's.
            ('h', 25) => self.active_cursor_mut().is_visible = true,
            // DECRST `?47` — switch back to the primary buffer, leaving the
            // alternate's cells and cursor as they are.
            ('l', 47) => self.active = Screen::Primary,
            // DECRST `?1047` — reset the alternate buffer (clear cells +
            // scroll region + cursor), then switch back to the primary. Only
            // while the alternate is live: after an earlier exit in the same
            // list, a no-op.
            ('l', 1047) => {
                if self.active == Screen::Alternate {
                    self.reset_alternate_buffer();
                    self.active = Screen::Primary;
                }
            }
            // DECRST `?1049` — `?1047 l` then `?1048 l`: the clear and the
            // switch to the primary run only while the alternate is live (a
            // second clearing exit is a no-op); the DECRC cursor restore
            // always runs, on the screen that is live after the switch.
            ('l', 1049) => {
                if self.active == Screen::Alternate {
                    self.reset_alternate_buffer();
                    self.active = Screen::Primary;
                }
                self.restore_cursor();
            }
            // DECRST `?1048` — restore the active screen's cursor only.
            ('l', 1048) => self.restore_cursor(),
            // DECRST `?25` (DECTCEM) — hide the cursor.
            ('l', 25) => self.active_cursor_mut().is_visible = false,
            // `?2004` — bracketed paste: the input layer wraps pasted text in
            // `ESC[200~`…`ESC[201~`.
            ('h', 2004) => self.modes.bracketed_paste = true,
            ('l', 2004) => self.modes.bracketed_paste = false,
            // Mouse tracking level (`?9`/`?1000`/`?1002`/`?1003`): the four
            // levels are mutually exclusive, and each enable replaces the
            // prior one. A reset turns reporting off only when it names the
            // active level; a reset naming another level falls through to `_`
            // and changes nothing.
            ('h', 9) => self.modes.mouse_tracking = MouseTracking::X10,
            ('h', 1000) => self.modes.mouse_tracking = MouseTracking::Normal,
            ('h', 1002) => self.modes.mouse_tracking = MouseTracking::ButtonMotion,
            ('h', 1003) => self.modes.mouse_tracking = MouseTracking::AnyMotion,
            ('l', 9) if self.modes.mouse_tracking == MouseTracking::X10 => {
                self.modes.mouse_tracking = MouseTracking::Off;
            }
            ('l', 1000) if self.modes.mouse_tracking == MouseTracking::Normal => {
                self.modes.mouse_tracking = MouseTracking::Off;
            }
            ('l', 1002) if self.modes.mouse_tracking == MouseTracking::ButtonMotion => {
                self.modes.mouse_tracking = MouseTracking::Off;
            }
            ('l', 1003) if self.modes.mouse_tracking == MouseTracking::AnyMotion => {
                self.modes.mouse_tracking = MouseTracking::Off;
            }
            // Mouse report encoding (`?1005`/`?1006`/`?1015`), independent of
            // the tracking level and mutually exclusive among themselves: each
            // enable replaces the prior one. A reset returns to the default
            // encoding only when it names the active encoding; a reset naming
            // another encoding falls through to `_` and changes nothing.
            ('h', 1005) => self.modes.mouse_encoding = MouseEncoding::Utf8,
            ('h', 1006) => self.modes.mouse_encoding = MouseEncoding::Sgr,
            ('h', 1015) => self.modes.mouse_encoding = MouseEncoding::Urxvt,
            ('l', 1005) if self.modes.mouse_encoding == MouseEncoding::Utf8 => {
                self.modes.mouse_encoding = MouseEncoding::Default;
            }
            ('l', 1006) if self.modes.mouse_encoding == MouseEncoding::Sgr => {
                self.modes.mouse_encoding = MouseEncoding::Default;
            }
            ('l', 1015) if self.modes.mouse_encoding == MouseEncoding::Urxvt => {
                self.modes.mouse_encoding = MouseEncoding::Default;
            }
            // `?1007` — alternate-screen scroll: wheel motion becomes
            // cursor arrow keys on the alternate screen.
            ('h', 1007) => self.modes.alt_scroll = true,
            ('l', 1007) => self.modes.alt_scroll = false,
            // `?7` (DECAWM) — autowrap. On (the default): a glyph at the
            // last column parks there and the next glyph wraps to a new
            // line. Off: the cursor stays pinned and further glyphs
            // overwrite the last column in place.
            ('h', 7) => self.modes.autowrap = true,
            ('l', 7) => self.modes.autowrap = false,
            // `?1` (DECCKM) — application cursor keys. The input layer reads
            // this to pick the arrow-key byte form (`ESC O A` vs `ESC [ A`).
            ('h', 1) => self.modes.app_cursor_keys = true,
            ('l', 1) => self.modes.app_cursor_keys = false,
            // `?5` (DECSCNM) — reverse video. The renderer reads this to
            // swap foreground and background across the whole screen.
            ('h', 5) => self.modes.reverse_video = true,
            ('l', 5) => self.modes.reverse_video = false,
            // `?12` (att610) — cursor blink. The renderer reads this to
            // blink the cursor cell.
            ('h', 12) => self.modes.cursor_blink = true,
            ('l', 12) => self.modes.cursor_blink = false,
            // `?2` (DECANM, VT52), `?3` (DECCOLM, 132-column), `?8` (DECARM,
            // keyboard auto-repeat), and every other DEC private mode: not
            // implemented, ignored.
            _ => {}
        }
    }

    /// Apply DECSCUSR (`CSI Ps SP q`) — the sequence an editor sends to change
    /// the cursor's look as it changes mode: vim sends `CSI 2 SP q` (steady
    /// block) for normal mode and `CSI 5 SP q` (blinking bar) for insert.
    ///
    /// One value carries both the shape and whether it blinks. Values `1`–`6`
    /// name a style: the odd ones blink, the even ones are steady. The blink
    /// half is written into
    /// [`cursor_blink`](crate::state::TerminalState::cursor_blink), the same
    /// field `?12` writes: `CSI 2 SP q` ("steady block") stops a blink an
    /// earlier `CSI ? 12 h` started.
    ///
    /// `0` clears the shape to `None` and blink to off: the pane asks for no
    /// style, and the renderer keeps the user's own configured cursor.
    ///
    /// An unknown value (`CSI 9 SP q`) changes nothing; the style already set
    /// stands.
    fn set_cursor_style(&mut self, params: &vte::Params) {
        let (shape, blink) = match first_param(params).unwrap_or(0) {
            0 => (None, false),
            1 => (Some(CursorShape::Block), true),
            2 => (Some(CursorShape::Block), false),
            3 => (Some(CursorShape::Underline), true),
            4 => (Some(CursorShape::Underline), false),
            5 => (Some(CursorShape::Bar), true),
            6 => (Some(CursorShape::Bar), false),
            _ => return,
        };
        self.modes.cursor_shape = shape;
        self.modes.cursor_blink = blink;
    }
}

#[cfg(test)]
mod tests;
