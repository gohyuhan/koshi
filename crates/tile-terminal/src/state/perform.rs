//! [`vte::Perform`] implementation that drives [`TerminalState`] from parsed
//! PTY output: printable glyphs land in the active grid at the cursor, and the
//! basic C0 control bytes move the cursor and scroll.
//!
//! Implemented so far: `print` (printable glyphs, display-width aware: wide
//! CJK/emoji span two cells; grapheme continuations — combining marks, ZWJ
//! emoji sequences, variation selectors, skin-tone modifiers, flags — fold onto
//! the base cell, with variation-selector width promotion), `execute` (C0 control
//! bytes), `csi_dispatch` (cursor moves, erase, SGR, insert/delete char & line,
//! scroll up/down, the DECSTBM scroll region, and the DEC private modes for the
//! alternate screen and cursor visibility), `esc_dispatch` (cursor save/restore
//! and reverse index), and `osc_dispatch` (the OSC 0/1/2 window title). The
//! device-control-string callbacks `hook`/`unhook` clear the in-progress
//! grapheme cluster (a DCS ends a text run, like any non-printing event); their
//! payload handling, and `put`, are otherwise left to a later task. `vte` decodes
//! UTF-8 upstream, so `print` receives a ready `char`.

use crate::grid::state::Cell;
use crate::state::{MouseEncoding, MouseTracking, SavedCursor, Screen, TerminalState};
use crate::style::{Color, Style};
use unicode_segmentation::GraphemeCursor;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Upper bound on the number of continuation code points folded onto one cell's
/// base (the `combining` tail). Real grapheme clusters — even a skin-toned ZWJ
/// emoji family — stay well under this; the cap exists only to bound per-cell
/// memory against pathological input (e.g. a flood of combining marks, "zalgo"
/// text) that would otherwise grow a single cell without limit. Continuations
/// past the cap are dropped.
const MAX_GRAPHEME_CONTINUATIONS: usize = 32;

impl TerminalState {
    /// The scroll-region margins for the active screen.
    fn active_scroll_region(&self) -> Option<(u16, u16)> {
        match self.active {
            Screen::Primary => self.primary_scroll_region,
            Screen::Alternate => self.alternate_scroll_region,
        }
    }

    /// Override the alternate cursor's *position* with the primary cursor's, run
    /// after [`Self::reset_alternate_buffer`] on a `?1049` entry so the entering
    /// app continues from where the primary cursor was. Only the position is
    /// touched here — visibility, the wrap latch, and the saved stash were
    /// already reset to fresh defaults by `reset_alternate_buffer`. The plain
    /// `?47`/`?1047` switches never call this (or the reset): they leave the
    /// alternate buffer intact so a re-entry resumes where it left off.
    fn seed_alternate_cursor(&mut self) {
        self.alternate_cursor.row = self.primary_cursor.row;
        self.alternate_cursor.col = self.primary_cursor.col;
    }

    /// The scroll-region margins as 0-based inclusive `(top, bottom)` rows,
    /// resolving `None` to the whole active grid.
    fn region_bounds(&self) -> (u16, u16) {
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
    fn delete_lines_into_scrollback(&mut self, first: u16, bottom: u16, n: u16, fill: Style) {
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
    fn linefeed(&mut self) {
        let fill = self.style.bg_fill();
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
    fn reverse_index(&mut self) {
        let fill = self.style.bg_fill();
        let (top, bottom) = self.region_bounds();
        if self.active_cursor().row == top {
            self.active_grid_mut().insert_lines(top, bottom, 1, fill);
        } else if self.active_cursor().row > 0 {
            self.active_cursor_mut().row -= 1;
        }
        self.active_cursor_mut().pending_wrap = false;
    }

    /// Save the cursor position and pen style (DECSC / SCOSC) into the active
    /// screen's cursor, so the primary and alternate screens snapshot separately.
    fn save_cursor(&mut self) {
        let row = self.active_cursor().row;
        let col = self.active_cursor().col;
        let pending_wrap = self.active_cursor().pending_wrap;
        self.active_cursor_mut().saved = Some(SavedCursor {
            row,
            col,
            style: self.style,
            pending_wrap,
        });
    }

    /// Restore the cursor position and pen style saved by `save_cursor` (DECRC /
    /// SCORC). With no prior save, xterm homes the cursor and resets the pen to
    /// defaults; the restored position is clamped into the current grid in case
    /// it shrank since the save.
    fn restore_cursor(&mut self) {
        let (rows, cols) = self.active_grid().dimensions();
        let (last_row, last_col) = (rows.saturating_sub(1), cols.saturating_sub(1));
        match self.active_cursor().saved {
            Some(saved) => {
                self.active_cursor_mut().row = saved.row.min(last_row);
                self.active_cursor_mut().col = saved.col.min(last_col);
                self.style = saved.style;
                self.active_cursor_mut().pending_wrap = saved.pending_wrap;
            }
            None => {
                self.active_cursor_mut().row = 0;
                self.active_cursor_mut().col = 0;
                self.style = Style::default();
                self.active_cursor_mut().pending_wrap = false;
            }
        }
    }

    /// Reset the alternate screen to a brand-new, blank state — the single
    /// definition of "a fresh alternate buffer". Resets **every** piece of the
    /// alternate's per-screen state so a new session can inherit none of the
    /// previous one:
    /// - cells blanked to the current pen background (BCE),
    /// - scroll region (DECSTBM) back to the full screen,
    /// - cursor home, shown, no wrap latch, no DECSC stash.
    ///
    /// The wrap latch in particular is *cell-coupled* — it means "a glyph is
    /// parked at the last column", so blanking the cells must drop it or a later
    /// print would wrap against an erased glyph. Operates on `self.alternate`
    /// directly (not the active grid), so it stays correct even when an earlier
    /// mode in the same DECRST list (e.g. `?47 l`) already switched the active
    /// screen back to the primary. Called wherever a switch *clears* the
    /// alternate: `?1049 h` entry (which then re-seeds the cursor position from
    /// the primary) and the `?1047 l`/`?1049 l` clearing exits. The plain
    /// `?47`/`?1047` switches never clear, so they never call this — they
    /// preserve the buffer and a re-entry resumes exactly where it left off.
    fn reset_alternate_buffer(&mut self) {
        let fill = self.style.bg_fill();
        let (rows, cols) = self.alternate.dimensions();
        for row in 0..rows {
            self.alternate.clear_line(row, 0, cols, fill);
        }
        self.alternate_scroll_region = None;
        self.alternate_cursor.row = 0;
        self.alternate_cursor.col = 0;
        self.alternate_cursor.is_visible = true;
        self.alternate_cursor.pending_wrap = false;
        self.alternate_cursor.saved = None;
    }

    /// DECSC the primary screen's cursor into its own saved slot. Used by the
    /// `?1049` entry specifically: it must stash the *primary* cursor even when
    /// an earlier mode in the same DECSET list (e.g. `?47 h`) already switched the
    /// active screen to the alternate, which would make the active-relative
    /// `save_cursor` stash the alternate cursor instead.
    fn save_primary_cursor(&mut self) {
        self.primary_cursor.saved = Some(SavedCursor {
            row: self.primary_cursor.row,
            col: self.primary_cursor.col,
            style: self.style,
            pending_wrap: self.primary_cursor.pending_wrap,
        });
    }

    /// Discard any in-progress grapheme cluster. Called by every non-printing
    /// event (control bytes, CSI / ESC / OSC, DCS hook/unhook), since anything
    /// other than a printed glyph ends the run a continuation could attach to.
    ///
    /// One edge is deliberately not covered: a malformed CSI that vte routes to
    /// its internal `CsiIgnore` state (e.g. `CSI 1 < m`, a private marker after a
    /// parameter) terminates straight to ground with NO `Perform` callback at
    /// all, so there is nothing to hook here. A combining mark printed afterward
    /// folds onto the preceding glyph — which matches xterm/alacritty, since the
    /// ignored sequence neither moved the cursor nor printed, so the mark still
    /// belongs to the cell left of the (unmoved) cursor. Harmless (the base cell
    /// is bounds-checked); flagged here so it is a known, accepted behavior.
    fn reset_cluster(&mut self) {
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Whether `c` continues the current grapheme cluster rather than starting a
    /// new one — i.e. there is no grapheme-cluster boundary between the cluster
    /// built so far and `c`. This is what folds combining marks, ZWJ emoji
    /// sequences, variation selectors, skin-tone modifiers, and regional-
    /// indicator flags onto a single base. An ambiguous / incomplete result is
    /// treated as a boundary (start fresh) — the safe default.
    fn continues_cluster(&self, c: char) -> bool {
        let mut probe = self.cluster.clone();
        probe.push(c);
        let mut cursor = GraphemeCursor::new(self.cluster.len(), probe.len(), true);
        !cursor.is_boundary(&probe, 0).unwrap_or(true)
    }

    /// Fold `c` into the current cluster: stack it on the base cell (so the
    /// renderer draws the whole cluster) without consuming a column, and — if
    /// the cluster's display width grew from one column to two (e.g. a variation
    /// selector promoting a text-presentation glyph to its wider emoji form) —
    /// widen the base.
    fn extend_cluster(&mut self, c: char) {
        let Some((row, col)) = self.cluster_base else {
            return;
        };
        // Bound per-cell memory: once the base already carries the maximum
        // continuations, drop further ones (and stop growing the tracking
        // string) so pathological input cannot grow a single cell without limit.
        if self
            .active_grid()
            .cell(row, col)
            .map_or(0, |cell| cell.combining().len())
            >= MAX_GRAPHEME_CONTINUATIONS
        {
            return;
        }
        let old_width = UnicodeWidthStr::width(self.cluster.as_str());
        self.cluster.push(c);
        let new_width = UnicodeWidthStr::width(self.cluster.as_str());

        if let Some(cell) = self.active_grid_mut().cell_mut(row, col) {
            cell.push_combining(c);
        }
        if old_width == 1 && new_width == 2 {
            self.promote_cluster_to_wide(row, col);
        } else if old_width == 2 && new_width == 1 {
            self.demote_cluster_to_narrow(row, col);
        }
    }

    /// Narrow the cluster's base at (`row`, `col`) from two cells to one after a
    /// continuation shrank its display width — e.g. a text-presentation selector
    /// (VS15, `U+FE0E`) forcing an emoji-presentation base back to its narrow
    /// text form. The base keeps its character, combining marks, and style but
    /// becomes `width == 1`; the continuation to its right is blanked, and the
    /// cursor steps back over the column the glyph no longer occupies. The base
    /// of a wide glyph never sits in the last column (a wide write wraps first),
    /// so the narrowed glyph always has room and never parks.
    fn demote_cluster_to_narrow(&mut self, row: u16, col: u16) {
        let narrowed = self
            .active_grid()
            .cell(row, col)
            .map(|cell| rebuilt_with_width(cell, 1));
        if let Some(narrowed) = narrowed {
            if let Some(slot) = self.active_grid_mut().cell_mut(row, col) {
                *slot = narrowed;
            }
        }
        let fill = self.style.bg_fill();
        if let Some(slot) = self.active_grid_mut().cell_mut(row, col + 1) {
            *slot = Cell::blank_with(fill);
        }
        // The glyph now occupies one column; the cursor sits just past the base.
        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);
        if col >= last_col {
            self.active_cursor_mut().col = last_col;
            self.active_cursor_mut().pending_wrap = true;
        } else {
            self.active_cursor_mut().col = col + 1;
            self.active_cursor_mut().pending_wrap = false;
        }
    }

    /// Widen the cluster's base at (`row`, `col`) from one cell to two after a
    /// continuation grew its display width. The base keeps its character,
    /// combining marks, and style but becomes `width == 2`; the column to its
    /// right becomes a width-0 continuation, and the cursor advances over the
    /// newly claimed column. If the base sits in the last column — no room to
    /// its right — the whole cluster moves to the next line, the same way a wide
    /// glyph wraps rather than straddling the edge.
    fn promote_cluster_to_wide(&mut self, row: u16, col: u16) {
        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);

        if col < last_col {
            // Room to the right: widen the base in place and claim col + 1.
            let Some(widened) = self
                .active_grid()
                .cell(row, col)
                .map(|cell| rebuilt_with_width(cell, 2))
            else {
                return;
            };
            self.place_glyph(row, col, widened);
            // The base advanced the cursor by one as a narrow glyph; the second
            // column it now occupies advances it once more, or parks at the edge.
            if col + 1 >= last_col {
                self.active_cursor_mut().col = last_col;
                self.active_cursor_mut().pending_wrap = true;
            } else {
                self.active_cursor_mut().col = col + 2;
            }
        } else if last_col > 0 {
            // Base in the last column of a multi-column grid: it cannot widen in
            // place, so move the whole cluster to the next line as a wide glyph.
            let Some((base_ch, style, marks)) = self
                .active_grid()
                .cell(row, col)
                .map(|cell| (cell.ch(), cell.style(), cell.combining().to_vec()))
            else {
                return;
            };
            let fill = self.style.bg_fill();
            if let Some(slot) = self.active_grid_mut().cell_mut(row, col) {
                *slot = Cell::blank_with(fill);
            }
            self.linefeed();
            self.active_cursor_mut().col = 0;
            self.active_cursor_mut().pending_wrap = false;

            let new_row = self.active_cursor().row;
            let mut widened = Cell::new(base_ch, 2, style);
            for mark in &marks {
                widened.push_combining(*mark);
            }
            // The destination row may already hold a wide glyph; `place_glyph`
            // clears any pair these writes would split before installing the
            // promoted cluster — otherwise a wide base at col 1 would leave its
            // width-0 continuation at col 2 orphaned.
            self.place_glyph(new_row, 0, widened);
            self.cluster_base = Some((new_row, 0));
            if 1 >= last_col {
                self.active_cursor_mut().col = last_col;
                self.active_cursor_mut().pending_wrap = true;
            } else {
                self.active_cursor_mut().col = 2;
            }
        }
        // No final `else`: in a 1-column pane (`last_col == 0`) the base cannot
        // widen here or on any other line, and `extend_cluster` has already folded
        // the promoting mark onto it, so the base simply stays narrow in place.
    }

    /// Before a glyph is written at (`row`, `col`), blank the orphaned half of
    /// any wide glyph this write would split, so a renderer never sees a wide
    /// base without its continuation or a continuation without its base. If the
    /// cell currently holds a wide base (`width == 2`), its continuation to the
    /// right is cleared; if it holds a continuation (`width == 0`), the base to
    /// its left is cleared. The freed half becomes a blank in the current pen
    /// background, matching the erase/scroll fill convention.
    fn clear_wide_at(&mut self, row: u16, col: u16) {
        let fill = self.style.bg_fill();
        match self.active_grid().cell(row, col).map_or(1, Cell::width) {
            // Wide base: clear its continuation half on the right.
            2 => {
                if let Some(cell) = self.active_grid_mut().cell_mut(row, col + 1) {
                    *cell = Cell::blank_with(fill);
                }
            }
            // Continuation half: clear the wide base on its left.
            0 if col > 0 => {
                if let Some(cell) = self.active_grid_mut().cell_mut(row, col - 1) {
                    *cell = Cell::blank_with(fill);
                }
            }
            _ => {}
        }
    }

    /// Install `base` at (`row`, `col`), first clearing any wide glyph the write
    /// would split so the wide-pair invariant always holds. This is the single
    /// path EVERY base/continuation write goes through (a fresh base, an in-place
    /// widen, a wrapped widen), so no call site can forget to clear and orphan a
    /// half. `base` already carries its display width (1 or 2), character,
    /// combining marks, and style. A width-2 base also lays its width-0
    /// continuation placeholder at `col + 1` (after clearing whatever pair sat
    /// there); a width-1 base writes `col` alone. Cursor and cluster bookkeeping
    /// stay with the caller, since they differ per write site.
    fn place_glyph(&mut self, row: u16, col: u16, base: Cell) {
        let (_, cols) = self.active_grid().dimensions();
        // A wide glyph needs its continuation column in bounds. In a pane too
        // narrow to hold the pair (e.g. a 1-column split, where even col 0 is the
        // last column) there is no room, so store the base as a single narrow
        // cell instead of a width-2 base with no continuation — the latter breaks
        // the wide-pair invariant, so a later erase / cell op would treat the lone
        // cell as an orphan and blank it, and a renderer trusting widths would see
        // an impossible row.
        let wide = base.width() == 2 && col + 1 < cols;
        let base = if base.width() == 2 && !wide {
            rebuilt_with_width(&base, 1)
        } else {
            base
        };
        let style = base.style();
        // Clear any wide pair this write would split, on every column it lands on
        // — for a width-1 base over its own narrow cell this is a no-op.
        self.clear_wide_at(row, col);
        if wide {
            self.clear_wide_at(row, col + 1);
        }
        if let Some(slot) = self.active_grid_mut().cell_mut(row, col) {
            *slot = base;
        }
        // A wide glyph's second column is a width-0 continuation placeholder,
        // covered by the glyph's left half; the renderer skips it.
        if wide {
            if let Some(slot) = self.active_grid_mut().cell_mut(row, col + 1) {
                *slot = Cell::new(' ', 0, style);
            }
        }
    }

    /// Repair `row`'s wide-glyph pairs after a cell op (erase / insert / delete)
    /// may have split one. The pair invariant: a wide base (`width == 2`) is
    /// always immediately followed by a width-0 continuation, and a continuation
    /// always immediately follows a wide base. Any half that breaks it — a base
    /// with no continuation to its right, or a continuation with no base to its
    /// left — is blanked in the current pen background. Scanned left-to-right so
    /// a freshly blanked base cascades to clear its now-orphaned continuation on
    /// the next column. Keeps a renderer (or later logic) that trusts `width`
    /// from drawing an erased wide glyph or a stray continuation.
    fn normalize_wide_pairs(&mut self, row: u16) {
        let (_, cols) = self.active_grid().dimensions();
        let fill = self.style.bg_fill();
        for col in 0..cols {
            let orphan = match self.active_grid().cell(row, col).map_or(1, Cell::width) {
                // Wide base needs a continuation immediately to its right.
                2 => self
                    .active_grid()
                    .cell(row, col + 1)
                    .is_none_or(|c| c.width() != 0),
                // Continuation needs a wide base immediately to its left.
                0 => {
                    col == 0
                        || self
                            .active_grid()
                            .cell(row, col - 1)
                            .is_none_or(|c| c.width() != 2)
                }
                _ => false,
            };
            if orphan {
                if let Some(cell) = self.active_grid_mut().cell_mut(row, col) {
                    *cell = Cell::blank_with(fill);
                }
            }
        }
    }
}

impl vte::Perform for TerminalState {
    fn print(&mut self, c: char) {
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

        // Deferred wrap: a prior print parked on the last column. Wrap to the
        // next line before placing this glyph, so a row that exactly fills the
        // width is not scrolled early.
        if self.active_cursor().pending_wrap {
            self.linefeed();
            self.active_cursor_mut().col = 0;
            self.active_cursor_mut().pending_wrap = false;
        }

        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);
        let style = self.style;

        // A wide glyph needs two columns; when only the last column is free it
        // cannot fit. Blank that lone column and wrap, so the glyph begins the
        // next line whole rather than straddling the edge. Skipped in a 1-column
        // pane (`last_col == 0`), where wrapping cannot help — `place_glyph` then
        // stores the glyph narrow in place instead of thrashing the screen.
        if glyph_width == 2 && self.active_cursor().col == last_col && last_col > 0 {
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

        // Advance past the glyph. If it reached the last column, park there with
        // the wrap latch set so the next glyph wraps; otherwise step to the
        // first free column after it.
        let end_col = col + glyph_width - 1;
        if end_col >= last_col {
            self.active_cursor_mut().col = last_col;
            self.active_cursor_mut().pending_wrap = true;
        } else {
            self.active_cursor_mut().col = end_col + 1;
        }
    }

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
                let to_next_stop = 8 - (self.active_cursor().col % 8);
                let next_tab = self.active_cursor().col.saturating_add(to_next_stop);
                self.active_cursor_mut().col = next_tab.min(last_col);
                self.active_cursor_mut().pending_wrap = false;
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
                    // DECSET `?47`/`?1047` — switch to the alternate buffer (no
                    // clear on entry). The alternate cursor is left untouched:
                    // these modes leave the buffer intact across a `?47 l`/`?1047 l`
                    // round-trip, so re-entry must resume where the previous
                    // alternate session left off, not reseed from the primary.
                    ('h', 47 | 1047) => {
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
                self.active_cursor_mut().row =
                    self.active_cursor().row.saturating_sub(move_count(params));
                self.active_cursor_mut().pending_wrap = false;
            }
            // CUD — cursor down, clamped to the last row.
            'B' => {
                let n = move_count(params);
                self.active_cursor_mut().row =
                    self.active_cursor().row.saturating_add(n).min(last_row);
                self.active_cursor_mut().pending_wrap = false;
            }
            // CUF — cursor forward, clamped to the last column.
            'C' => {
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
            // 0-based coordinates and clamped into the grid.
            'H' | 'f' => {
                self.active_cursor_mut().row = coord_param(params, 0).min(last_row);
                self.active_cursor_mut().col = coord_param(params, 1).min(last_col);
                self.active_cursor_mut().pending_wrap = false;
            }
            // ED — erase in display (cursor unmoved; `pending_wrap` untouched).
            'J' => {
                let fill = self.style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
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
                // Only the cursor row is partially cleared (the others are whole
                // rows, which cannot split a pair); repair it.
                self.normalize_wide_pairs(r);
            }
            // EL — erase in line (cursor unmoved; `pending_wrap` untouched).
            'K' => {
                let fill = self.style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                match first_param(params).unwrap_or(0) {
                    // Cursor to end of line. With a wrap pending the cursor is
                    // logically past the last column, so there is nothing to its
                    // right to erase — matching alacritty's `clear_line`, which
                    // returns early here and preserves the parked last-column
                    // glyph.
                    0 if self.active_cursor().pending_wrap => {}
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
                self.normalize_wide_pairs(r);
            }
            // SGR — set graphic rendition: update the pen colors and text
            // attributes applied to subsequently printed cells.
            'm' => apply_sgr(&mut self.style, params),
            // ICH — insert n blank cells at the cursor, shifting the rest of the
            // line right; cells pushed past the right edge fall off.
            '@' => {
                let n = move_count(params);
                let fill = self.style.bg_fill();
                let (r, c) = (self.active_cursor().row, self.active_cursor().col);
                self.active_grid_mut().insert_cells(r, c, n, fill);
                self.normalize_wide_pairs(r);
                self.active_cursor_mut().pending_wrap = false;
            }
            // DCH — delete n cells at the cursor, pulling the rest of the line
            // left; the right end is refilled with blanks.
            'P' => {
                let n = move_count(params);
                let fill = self.style.bg_fill();
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
                    let fill = self.style.bg_fill();
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
                    let fill = self.style.bg_fill();
                    let r = self.active_cursor().row;
                    self.delete_lines_into_scrollback(r, bottom, n, fill);
                }
            }
            // SU — scroll the region up by n (`CSI Ps S`); the cursor stays put.
            'S' => {
                let n = move_count(params);
                let fill = self.style.bg_fill();
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
            // Any other CSI final byte (DEC private modes, device queries, …)
            // is not handled yet; ignored rather than mis-applied.
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // Any ESC sequence ends a text run, so no following glyph folds into it.
        self.reset_cluster();
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
        // Any OSC ends a text run, so no following glyph folds into it.
        self.reset_cluster();
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

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // A device control string (DCS, `ESC P … ST`) is a non-printing control
        // sequence, so it ends a text run: a combining mark or variation selector
        // that follows must not fold onto the glyph before the DCS. Clearing here,
        // at DCS entry, covers the whole string — the body bytes arrive via `put`,
        // which never prints, so they cannot extend a cluster. The DCS payload
        // itself is owned by a later task.
        self.reset_cluster();
    }

    fn unhook(&mut self) {
        // DCS termination. Redundant with `hook` for a well-formed string, but it
        // also covers a DCS closed by the 8-bit C1 ST (`0x9C`), whose only `Perform`
        // callback is this one — it does not route through `esc_dispatch`/`execute`,
        // so without this the cluster would survive such a DCS.
        self.reset_cluster();
    }
}

/// Rebuild `cell` with a new display `width`, preserving its character,
/// combining marks, and style — used to re-width a cluster's base when a
/// continuation promotes it to a two-column emoji glyph or demotes it back to a
/// one-column text glyph.
fn rebuilt_with_width(cell: &Cell, width: u8) -> Cell {
    let mut out = Cell::new(cell.ch(), width, cell.style());
    for mark in cell.combining() {
        out.push_combining(*mark);
    }
    out
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
/// missing or unrecognized payload — or an out-of-range value (a palette index
/// or channel > 255) — yields `None`, leaving the pen unchanged.
fn extended_color<'a>(first: &[u16], iter: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    if first.len() > 1 {
        // Colon form: selector at first[1], its values follow in the same slice.
        match first.get(1).copied()? {
            // `38:5:n` — 256-palette index is the final subparameter. Reading
            // the last (not `first[2]`) skips a leading empty colorspace slot in
            // the malformed `38:5::n` form (which `vte` stores as a `0`), keeping
            // the index symmetric with the RGB branch below; `len >= 3` requires
            // the index to be present so `38:5` alone rejects. An index that does
            // not fit a u8 (> 255) is out of range, so reject the color (`None`,
            // pen unchanged), matching vte's own `ansi.rs` (`u8::try_from(..).ok()?`).
            5 if first.len() >= 3 => Some(Color::Indexed(u8::try_from(*first.last()?).ok()?)),
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
                // A channel that does not fit a u8 (> 255) is out of range →
                // reject the whole color, as vte's `ansi.rs` does.
                Some(Color::Rgb(
                    u8::try_from(*rgb.first()?).ok()?,
                    u8::try_from(*rgb.get(1)?).ok()?,
                    u8::try_from(*rgb.get(2)?).ok()?,
                ))
            }
            _ => None,
        }
    } else {
        // Semicolon form: selector then values are the next separate params.
        match next_val(iter)? {
            // `38;5;n` — one following param is the 256-palette index; reject an
            // out-of-range (> 255) index, matching vte's `ansi.rs`.
            5 => Some(Color::Indexed(u8::try_from(next_val(iter)?).ok()?)),
            // `38;2;r;g;b` — three following params are the RGB channels.
            // Consume all THREE before validating: a malformed (out-of-range)
            // channel must still drain its g/b params, or the leftover values
            // bleed back into the outer SGR loop as standalone color codes (e.g.
            // `38;2;999;31;32m` would set fg-red then fg-green). Once drained, a
            // channel > 255 rejects the whole color (`None`, pen unchanged).
            2 => {
                let (r, g, b) = (next_val(iter)?, next_val(iter)?, next_val(iter)?);
                Some(Color::Rgb(
                    u8::try_from(r).ok()?,
                    u8::try_from(g).ok()?,
                    u8::try_from(b).ok()?,
                ))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
