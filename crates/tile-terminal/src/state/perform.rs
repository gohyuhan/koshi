//! [`vte::Perform`] implementation that drives [`TerminalState`] from parsed
//! PTY output: printable glyphs land in the active grid at the cursor, and the
//! basic C0 control bytes move the cursor and scroll.
//!
//! Implemented so far: `print` (printable glyphs, translated through the active
//! GL charset — DEC line-drawing, UK — then display-width aware: wide
//! CJK/emoji span two cells; grapheme continuations — combining marks, ZWJ
//! emoji sequences, variation selectors, skin-tone modifiers, flags — fold onto
//! the base cell, with variation-selector width promotion), `execute` (C0 control
//! bytes, including the `SI`/`SO` charset shifts), `csi_dispatch` (relative + absolute cursor moves, line-relative
//! moves, forward/back tab stops, erase in display/line and erase-char, SGR,
//! insert/delete char & line, scroll up/down, the DECSTBM scroll region,
//! the DEC private modes for the alternate screen and cursor visibility, and
//! the device queries — DA1/DA2/DA3, the DSR family, DECRQM — whose replies
//! land on the state's reply queue),
//! `esc_dispatch` (cursor save/restore,
//! reverse index, and `G0`–`G3` charset designation), and `osc_dispatch` (the OSC 0/1/2 window title and the
//! OSC 7 working-directory report). The
//! device-control-string callbacks `hook`/`unhook` clear the in-progress
//! grapheme cluster (a DCS ends a text run, like any non-printing event); their
//! payload handling, and `put`, are otherwise left to a later task. `vte` decodes
//! UTF-8 upstream, so `print` receives a ready `char`.
//!
//! The performer's helpers are split across submodules by concern — charset
//! translation ([`charset`]), device-query replies ([`device`]), grapheme
//! clustering and wide-glyph placement ([`glyph`]), cursor motion / scrolling
//! / the scroll region ([`motion`]), alternate-screen entry/exit
//! ([`alt_screen`]), SGR ([`sgr`]), OSC parsing ([`osc`]), and CSI parameter
//! accessors ([`params`]) — while the [`vte::Perform`] trait impl itself stays
//! here as the dispatch surface.

use crate::grid::state::Cell;
use crate::state::{MouseEncoding, MouseTracking, Screen, TerminalState};
use unicode_width::UnicodeWidthChar;

use self::motion::{next_tab_stop, prev_tab_stop};
use self::osc::parse_osc7_cwd;
use self::params::{coord_param, first_param, move_count, nth_param};
use self::sgr::apply_sgr;

mod alt_screen;
mod charset;
mod device;
mod glyph;
mod motion;
mod osc;
mod params;
mod sgr;

impl vte::Perform for TerminalState {
    /// Print a displayable character to the active grid. Translates the
    /// character through the active GL charset (DEC line-drawing, UK, or
    /// passthrough), folds continuations (combining marks, ZWJ emoji parts,
    /// variation selectors) onto the preceding base, handles display width
    /// (narrow single-column or wide CJK/emoji two-column), and respects
    /// autowrap at the line end.
    fn print(&mut self, c: char) {
        // Translate through the active GL charset first (DEC line-drawing, UK),
        // so the cell stores the resolved glyph and width is computed on it. The
        // result is always a narrow, non-combining char, so every path below is
        // unaffected by the remap.
        let c = self.map_charset(c);

        // A continuation (combining mark, ZWJ-joined emoji part, variation
        // selector, skin-tone modifier, flag half) folds onto the current
        // cluster's base instead of taking its own cell.
        if !self.cluster.is_empty() && self.continues_cluster(c) {
            self.extend_cluster(c);
            return;
        }

        // `c` starts a new grapheme. A control char that slipped past `execute`
        // has no display width (`None`) → ignore it. A zero-width char with no
        // cluster to join (e.g. a combining mark at the very start of a line)
        // has no base to attach to → drop it. Otherwise the glyph is narrow (1)
        // or wide (2, e.g. CJK / emoji); `unicode-width` treats ambiguous-width
        // characters as narrow.
        let Some(raw_width) = c.width() else {
            // A control char with no display width slipped past `execute`; it is
            // not text, so it ends the run. Drop it but reset, so a following
            // continuation cannot attach across it.
            self.reset_cluster();
            return;
        };
        if raw_width == 0 {
            // A zero-width char that did NOT continue the cluster is a grapheme
            // boundary (e.g. ZWSP U+200B): it ends the run. Drop it but reset, so
            // a following combining mark / VS16 cannot attach across the break.
            self.reset_cluster();
            return;
        }
        let glyph_width: u16 = if raw_width >= 2 { 2 } else { 1 };

        // Deferred wrap: a prior print parked on the last column. Under autowrap
        // (DECAWM `?7`, the default) wrap to the next line before placing this
        // glyph, so a row that exactly fills the width is not scrolled early. With
        // autowrap off the cursor stays pinned at the last column and this glyph
        // overwrites in place; either way the parked latch is cleared.
        if self.active_cursor().pending_wrap {
            if self.modes.autowrap {
                self.linefeed();
                self.active_cursor_mut().col = 0;
            }
            self.active_cursor_mut().pending_wrap = false;
        }

        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);
        let style = self.active_render().style;

        // A wide glyph needs two columns; when only the last column is free it
        // cannot fit. Blank that lone column and wrap, so the glyph begins the
        // next line whole rather than straddling the edge. Skipped in a 1-column
        // pane (`last_col == 0`), where wrapping cannot help — `place_glyph` then
        // stores the glyph narrow in place instead of thrashing the screen.
        if glyph_width == 2 && self.active_cursor().col == last_col && last_col > 0 {
            // A 2-cell glyph cannot fit in the lone last column. With autowrap off
            // there is no next line to move it onto, so drop it; the cursor rests
            // on the last column and the next glyph overwrites there (no wrap is
            // armed under autowrap-off). The dropped glyph is its own new grapheme,
            // so reset the cluster: a following combining mark must not fold onto
            // the previous cell.
            if !self.modes.autowrap {
                self.reset_cluster();
                return;
            }
            let row = self.active_cursor().row;
            // If the last column is the continuation of an existing wide glyph,
            // blanking it alone would orphan that glyph's base one column to the
            // left; clear the pair before blanking the freed column.
            self.clear_wide_at(row, last_col);
            if let Some(cell) = self.active_grid_mut().cell_mut(row, last_col) {
                *cell = Cell::blank_with(style.bg_fill());
            }
            self.linefeed();
            self.active_cursor_mut().col = 0;
            self.active_cursor_mut().pending_wrap = false;
        }

        let row = self.active_cursor().row;
        let col = self.active_cursor().col;

        // Install the base glyph (and, when wide, its continuation), clearing any
        // wide pair the write would split — see `place_glyph`.
        self.place_glyph(row, col, Cell::new(c, glyph_width as u8, style));

        // Anchor a new cluster at this base so any continuations that follow
        // (combining marks, ZWJ emoji parts, …) fold onto it.
        self.cluster.clear();
        self.cluster.push(c);
        self.cluster_base = Some((row, col));

        // Advance past the glyph. If it reached the last column, park there (and,
        // under autowrap, arm the wrap latch so the next glyph wraps); otherwise
        // step to the first free column after it.
        let end_col = col + glyph_width - 1;
        if end_col >= last_col {
            self.arm_wrap_latch(last_col);
        } else {
            self.active_cursor_mut().col = end_col + 1;
        }
    }

    /// Handle a C0 control byte: line feed (`LF`/`VT`/`FF`), carriage
    /// return (`CR`), backspace (`BS`), tab (`HT`), charset shift
    /// (`SO`/`SI`), or bell (`BEL`). Most move the cursor, and all end
    /// the current grapheme cluster.
    fn execute(&mut self, byte: u8) {
        // A control byte ends any text run, so no following glyph folds into it.
        self.reset_cluster();
        match byte {
            // LF, VT, FF: line feed (VT/FF treated as LF).
            0x0A..=0x0C => {
                self.linefeed();
                self.active_cursor_mut().pending_wrap = false;
            }
            // CR: carriage return to column 0.
            0x0D => {
                self.active_cursor_mut().col = 0;
                self.active_cursor_mut().pending_wrap = false;
            }
            // BS: backspace one column (no erase).
            0x08 => {
                self.active_cursor_mut().col = self.active_cursor().col.saturating_sub(1);
                self.active_cursor_mut().pending_wrap = false;
            }
            // HT: advance to the next 8-column tab stop, clamped to the grid.
            0x09 => {
                let (_, cols) = self.active_grid().dimensions();
                let last_col = cols.saturating_sub(1);
                let col = self.active_cursor().col;
                self.active_cursor_mut().col = next_tab_stop(col, last_col);
                self.active_cursor_mut().pending_wrap = false;
            }
            // SO (shift out): select G1 into the GL range for printing.
            0x0E => self.active_render_mut().gl = 1,
            // SI (shift in): select G0 into the GL range for printing.
            0x0F => self.active_render_mut().gl = 0,
            // BEL: discarded.
            0x07 => {}
            // Any other control byte: trace and ignore, never raw-rendered.
            _ => {
                tracing::trace!(byte, "unhandled control byte; ignored");
            }
        }
    }

    /// Handle a CSI sequence: cursor movement (CUU/CUD/CUF/CUB/CUP/HVP/HPA/VPA/
    /// CNL/CPL/CHT/CBT), erase in display/line/character (ED/EL/ECH), graphics
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
        // Most CSI sequences end a text run, so no following glyph folds into
        // it. A style-only SGR (`CSI Pm m`) is the exception: it changes the pen
        // but neither moves the cursor nor edits the grid, so a combining mark or
        // variation selector that follows must still fold onto the preceding base
        // (e.g. `e \x1b[31m \u{0301}` → an accented `e`), matching xterm/alacritty.
        // The exception must mirror EXACTLY what the dispatch below treats as a
        // real, applied SGR: empty intermediates (a private/intermediate `m` is
        // not SGR) AND `!ignore` — an overlong CSI that vte flags `ignore` is
        // malformed and dropped (see the early return), so even one ending in `m`
        // must break the cluster like every other non-printing CSI. Every other
        // CSI moves the cursor or mutates cells, so it breaks the cluster too.
        if !(action == 'm' && intermediates.is_empty() && !ignore) {
            self.reset_cluster();
        }
        // `ignore` flags a sequence with too many params/intermediates to have
        // been kept intact — drop it.
        if ignore {
            return;
        }

        // Device queries (DA1/DA2/DA3, DSR, DECRQM/RQM): each queues its
        // response bytes on the reply queue for the runtime to write back into
        // the PTY. Dispatched on the exact (intermediates, action) pair —
        // before the DEC private-mode block, which would otherwise consume the
        // `?`-intermediate forms (`CSI ? Ps n`, `CSI ? Ps $ p`). The `$`-form
        // pairs leave `CSI ! p` (DECSTR) untouched.
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
            _ => {}
        }

        // DEC private modes carry a `?` private marker, which vte collects into
        // `intermediates`. DECSET/DECRST take a parameter list (`CSI ? Pm h/l`),
        // so apply every mode in the sequence; any mode not handled here is
        // owned by a later task.
        if intermediates == b"?" {
            // Modes in one DECSET/DECRST list are applied left-to-right, each
            // taking effect immediately (matching xterm/alacritty), so per-screen
            // state like `?25` visibility lands on whichever screen is active at
            // that point in the list. Switches are guarded on the **live**
            // `self.active` (alacritty's whichBuf guard), so a second swap-mode in
            // the same list is a no-op once the first has flipped buffers — e.g. a
            // trailing `?1047 l` after `?1049 l` does not re-clear (that would blank
            // with the wrong pen, since `?1049 l`'s DECRC already restored the
            // primary's). The one exception is `?1049 h` entry, guarded on the
            // screen active at the *start* of the list (`screen_at_start`): it must
            // still save the primary cursor + freshen the alternate when the list
            // began on the primary, even if an earlier `?47` already swapped (a
            // deliberate, safer deviation from alacritty, which no-ops it). Entry
            // re-firing is idempotent (no SGR can change the pen mid-`?`-list), so
            // it needs no whichBuf guard; exit re-firing is not, so it does.
            let screen_at_start = self.active;
            for param in params.iter() {
                let mode = param.first().copied().unwrap_or(0);
                match (action, mode) {
                    // DECSET `?47`/`?1047` — switch to the alternate buffer, leaving
                    // its cells and cursor untouched. Clone the primary's render
                    // state (pen, charsets, GL slot) into the alternate when
                    // crossing from the primary, so the alternate inherits it.
                    ('h', 47 | 1047) => {
                        if self.active == Screen::Primary {
                            self.alternate_render = self.primary_render;
                        }
                        self.active = Screen::Alternate;
                    }
                    // DECSET `?1049` — DECSC the primary cursor, reset the alternate
                    // to a brand-new buffer, re-seed its cursor position from the
                    // primary, then switch. Guarded on the *start* screen so an
                    // earlier `?47` in the same list cannot suppress any of this;
                    // the save targets the primary buffer explicitly so that
                    // earlier switch cannot redirect it onto the alternate cursor.
                    // `?1049` always starts a fresh session, so it inherits no
                    // cells, cursor, wrap latch, saved cursor, or scroll region
                    // from the previous one (unlike the preserving `?47`/`?1047`).
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
                    // DECSET `?25` (DECTCEM) — show the cursor. Visibility is
                    // tracked per screen (a deliberate deviation from xterm's
                    // global DECTCEM), so this toggles only the active screen.
                    ('h', 25) => self.active_cursor_mut().is_visible = true,
                    // DECRST `?47` — switch back to the primary buffer.
                    ('l', 47) => {
                        if self.active == Screen::Alternate {
                            self.active = Screen::Primary;
                        }
                    }
                    // DECRST `?1047` — reset the alternate buffer (clear cells +
                    // scroll region + cursor), then switch back to the primary.
                    // Guarded on the **live** screen (whichBuf): once an earlier
                    // exit in the same list already left the alternate, this is a
                    // no-op — re-clearing on the primary would blank with the wrong
                    // pen.
                    ('l', 1047) => {
                        if self.active == Screen::Alternate {
                            self.reset_alternate_buffer();
                            self.active = Screen::Primary;
                        }
                    }
                    // DECRST `?1049` — xterm/alacritty define `?1049 l` as `?1047 l`
                    // + `?1048 l`: the clear + switch-to-primary apply only while
                    // still on the alternate (live whichBuf guard, so a second
                    // clearing exit is a no-op), but the DECRC cursor restore (the
                    // `?1048 l` part) runs unconditionally.
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
                    // `?2004` — bracketed paste: wrap pasted text in
                    // `ESC[200~`…`ESC[201~` so the app distinguishes typing.
                    ('h', 2004) => self.modes.bracketed_paste = true,
                    ('l', 2004) => self.modes.bracketed_paste = false,
                    // Mouse tracking level (`?9`/`?1000`/`?1002`/`?1003`). The
                    // four levels are mutually exclusive, so each enable replaces
                    // the prior one (matching alacritty, whose set arm clears the
                    // other mouse bits before setting its own). A reset disables
                    // reporting only when it names the *active* level; resetting a
                    // mode that is not active is a no-op (falls through to `_`),
                    // since alacritty's unset clears only that mode's own bit.
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
                    // Mouse report encoding (`?1005`/`?1006`/`?1015`), orthogonal
                    // to the tracking level and mutually exclusive among
                    // themselves (each enable replaces the prior — matching
                    // alacritty, whose set arm removes the other encoding bit
                    // before setting its own). A reset returns to the default
                    // encoding only when it names the *active* encoding; resetting
                    // an encoding that is not active is a no-op (falls through to
                    // `_`), since alacritty's unset clears only that bit.
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
                    // keyboard auto-repeat): modes tile does not implement. Trace
                    // and ignore.
                    ('h' | 'l', 2 | 3 | 8) => {
                        tracing::trace!(mode, "unsupported DEC private mode; ignored");
                    }
                    // Any other DEC private mode is not handled yet.
                    _ => {}
                }
            }
            return;
        }

        // Any other intermediate marks a sequence (DECSTR `!p`, DECSCA `"q`, …)
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
                self.active_cursor_mut().row =
                    self.active_cursor().row.saturating_sub(move_count(params));
                self.active_cursor_mut().pending_wrap = false;
            }
            // CUD / VPR — cursor down, clamped to the last row (VPR `e` is the
            // same vertical move as CUD).
            'B' | 'e' => {
                let n = move_count(params);
                self.active_cursor_mut().row =
                    self.active_cursor().row.saturating_add(n).min(last_row);
                self.active_cursor_mut().pending_wrap = false;
            }
            // CUF / HPR — cursor forward, clamped to the last column (HPR `a` is
            // the same horizontal move as CUF).
            'C' | 'a' => {
                let n = move_count(params);
                self.active_cursor_mut().col =
                    self.active_cursor().col.saturating_add(n).min(last_col);
                self.active_cursor_mut().pending_wrap = false;
            }
            // CUB — cursor back.
            'D' => {
                self.active_cursor_mut().col =
                    self.active_cursor().col.saturating_sub(move_count(params));
                self.active_cursor_mut().pending_wrap = false;
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
            // CHT — cursor forward tabulation: advance n tab stops (every 8
            // columns until configurable stops land), clamped to the last
            // column; a cursor already at the last column does not move.
            'I' => {
                let mut col = self.active_cursor().col;
                for _ in 0..move_count(params) {
                    if col >= last_col {
                        break;
                    }
                    col = next_tab_stop(col, last_col);
                }
                self.active_cursor_mut().col = col;
                self.active_cursor_mut().pending_wrap = false;
            }
            // CBT — cursor backward tabulation: retreat n tab stops, floored at
            // column 0; a cursor already at column 0 does not move.
            'Z' => {
                let mut col = self.active_cursor().col;
                for _ in 0..move_count(params) {
                    if col == 0 {
                        break;
                    }
                    col = prev_tab_stop(col);
                }
                self.active_cursor_mut().col = col;
                self.active_cursor_mut().pending_wrap = false;
            }
            // ED — erase in display (cursor unmoved; an erasing mode clears the
            // wrap latch, see below).
            'J' => {
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                let mode = first_param(params).unwrap_or(0);
                match mode {
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
                    // Erase scrollback only (xterm "erase saved lines"): drop
                    // the retained history, leaving the visible screen untouched.
                    // Scrollback belongs to the primary screen (the alternate
                    // never feeds it), so an ED 3 from a full-screen app on the
                    // alternate screen must not wipe the user's shell history;
                    // guard to the primary, matching alacritty (whose alternate
                    // grid has zero history, making the clear a no-op there). An
                    // ED 3 on the alternate screen falls through to the `_` arm.
                    3 if self.active == Screen::Primary => self.scrollback.clear(),
                    // Unknown ED mode: ignored.
                    _ => {}
                }
                // ED 0/1/2 wipe the cursor's cell (un-filling its line), so clear
                // the parked wrap latch — the cursor is a concrete column and its
                // armed glyph is gone. ED 3 (scrollback only) and unknown modes
                // leave the visible grid and the latch untouched.
                if matches!(mode, 0..=2) {
                    self.active_cursor_mut().pending_wrap = false;
                }
                // Only the cursor row is partially cleared (the others are whole
                // rows, which cannot split a pair); repair it.
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
                    // Whole line.
                    2 => self.active_grid_mut().clear_line(r, 0, cols, fill),
                    // Unknown EL mode: ignored.
                    _ => {}
                }
                // Every EL mode (0/1/2) wipes the cursor's cell, un-filling the
                // line, so clear the parked wrap latch — the cursor is a concrete
                // column and its armed glyph is gone, so the next print overwrites
                // it rather than wrapping. An unknown mode erases nothing and
                // leaves the latch.
                if matches!(mode, 0..=2) {
                    self.active_cursor_mut().pending_wrap = false;
                }
                self.normalize_wide_pairs(r);
            }
            // ECH — erase n cells in place from the cursor (BCE background fill,
            // no shift of the rest of the line), then repair any wide-glyph pair
            // the erase split. The cursor is a concrete column, so erasing from it
            // clears the parked last-column glyph and clears the wrap latch — the
            // cell that armed the wrap is gone, so no wrap remains pending.
            'X' => {
                let n = move_count(params);
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                let end = c.saturating_add(n).min(cols);
                self.active_grid_mut().clear_line(r, c, end, fill);
                self.active_cursor_mut().pending_wrap = false;
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
                self.active_cursor_mut().pending_wrap = false;
            }
            // DCH — delete n cells at the cursor, pulling the rest of the line
            // left; the right end is refilled with blanks.
            'P' => {
                let n = move_count(params);
                let fill = self.active_render().style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                self.active_grid_mut().delete_cells(r, c, n, fill);
                self.normalize_wide_pairs(r);
                self.active_cursor_mut().pending_wrap = false;
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
                if (top..=bottom).contains(&self.active_cursor().row) {
                    let n = move_count(params);
                    let fill = self.active_render().style.bg_fill();
                    let r = self.active_cursor().row;
                    self.active_grid_mut().insert_lines(r, bottom, n, fill);
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
            // is the common form, but `CSI <5 params> T` is xterm highlight mouse
            // tracking (a later task), so only T's 0/1-param form scrolls; `CSI Ps ^`
            // is the unambiguous ECMA-48 form and always scrolls.
            'T' | '^' => {
                if action == '^' || params.len() <= 1 {
                    let n = move_count(params);
                    let fill = self.active_render().style.bg_fill();
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
                    self.active_cursor_mut().pending_wrap = false;
                }
            }
            // Any other CSI final byte is not handled yet; ignored rather than
            // mis-applied.
            _ => {}
        }
    }

    /// Handle an ESC sequence: charset designation (ESC `(` / `)` / `*` / `+` Fc
    /// → G0/G1/G2/G3 with DEC line-drawing/ASCII/UK), cursor save/restore
    /// (DECSC/DECRC: ESC `7` / `8`), or reverse index (RI: ESC `M`).
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // Any ESC sequence ends a text run, so no following glyph folds into it.
        self.reset_cluster();
        if ignore {
            return;
        }
        // Charset designation: `ESC (`/`)`/`*`/`+` Fc designates G0/G1/G2/G3.
        // vte collects the `(`/`)`/`*`/`+` into `intermediates`; the final `byte`
        // names the set. Handled before the plain-ESC match below, which the
        // intermediate would otherwise skip.
        match intermediates {
            b"(" => return self.designate_charset(0, byte),
            b")" => return self.designate_charset(1, byte),
            b"*" => return self.designate_charset(2, byte),
            b"+" => return self.designate_charset(3, byte),
            // Any other intermediate marks an ESC form owned by a later task.
            [_, ..] => return,
            // No intermediate: fall through to the plain-ESC finals below.
            [] => {}
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

    /// Handle an Operating System Command (OSC) sequence: window/icon title
    /// (OSC 0/1/2) or working-directory report (OSC 7, `file://` URI).
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // Any OSC ends a text run, so no following glyph folds into it.
        self.reset_cluster();
        // OSC 0/1/2 set the window/icon title. `params[0]` is the command
        // number. vte splits the payload on every `;`, but for a title only the
        // first `;` is the command/text separator, so rejoin `params[1..]` with
        // `;` to keep a title that itself contains one. Decode lossily so a
        // non-UTF-8 title still shows. OSC 7 (the working directory) is handled
        // by its own arm below.
        let Some(&command) = params.first() else {
            return;
        };
        if matches!(std::str::from_utf8(command), Ok("0" | "1" | "2")) && params.len() > 1 {
            let title = params[1..].join(&b';');
            self.title = Some(String::from_utf8_lossy(&title).into_owned());
        }
        // OSC 7 reports the shell's working directory as a `file://host/path`
        // URI. Rejoin `params[1..]` on `;` like the title (a path may carry a
        // literal `;`), then parse; an unparseable URI leaves the last cwd
        // unchanged so a bad emit does not erase a good value.
        if matches!(std::str::from_utf8(command), Ok("7")) && params.len() > 1 {
            let uri = params[1..].join(&b';');
            if let Some(cwd) = parse_osc7_cwd(&uri) {
                self.reported_cwd = Some(cwd);
            }
        }
    }

    /// Begin a device control string (DCS, `ESC P … ST`): clear any
    /// in-progress grapheme cluster since a non-printing control sequence ends
    /// the text run. DCS payload handling is deferred.
    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // A device control string (DCS, `ESC P … ST`) is a non-printing control
        // sequence, so it ends a text run: a combining mark or variation selector
        // that follows must not fold onto the glyph before the DCS. Clearing here,
        // at DCS entry, covers the whole string — the body bytes arrive via `put`,
        // which never prints, so they cannot extend a cluster. The DCS payload
        // itself is owned by a later task.
        self.reset_cluster();
    }

    /// End a device control string (DCS) and clear any in-progress grapheme
    /// cluster. Redundant with `hook` for well-formed strings, but necessary
    /// for DCS closed by the 8-bit C1 ST (`0x9C`), which calls only this
    /// method.
    fn unhook(&mut self) {
        // DCS termination. Redundant with `hook` for a well-formed string, but it
        // also covers a DCS closed by the 8-bit C1 ST (`0x9C`), whose only `Perform`
        // callback is this one — it does not route through `esc_dispatch`/`execute`,
        // so without this the cluster would survive such a DCS.
        self.reset_cluster();
    }
}

#[cfg(test)]
mod tests;
