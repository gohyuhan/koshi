//! Grapheme clustering and wide-glyph placement. A grapheme cluster is what a
//! reader sees as "one character" but that may be built from several Unicode
//! code points — e.g. an emoji plus a skin-tone modifier. This module folds
//! those continuations (combining marks, ZWJ — zero-width joiner — emoji
//! parts, variation selectors) onto a base cell, places narrow and wide
//! glyphs, and keeps wide-glyph pairs intact across edits.

use crate::grid::state::{Cell, RowEnd};
use crate::state::{rebuilt_with_width, TerminalState};
use unicode_segmentation::GraphemeCursor;
use unicode_width::UnicodeWidthStr;

/// Upper bound on the continuation code points one cell keeps in its
/// `combining` tail. A continuation that arrives when the base already holds
/// this many is dropped. A real grapheme cluster — a skin-toned ZWJ
/// (zero-width joiner) emoji family included — stays well under it; a flood of
/// combining marks ("zalgo" text) reaches it.
pub(super) const MAX_GRAPHEME_CONTINUATIONS: usize = 32;

impl TerminalState {
    /// Discard any in-progress grapheme cluster. Every non-printing event
    /// (control bytes, CSI / ESC / OSC, DCS hook/unhook) calls it; a
    /// continuation printed after one starts a new cluster.
    ///
    /// A malformed CSI that vte routes to its internal `CsiIgnore` state (e.g.
    /// `CSI 1 < m`, a private marker after a parameter) ends with no `Perform`
    /// callback and never reaches here: a combining mark printed after it
    /// folds onto the preceding glyph.
    pub(super) fn reset_cluster(&mut self) {
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Whether `c` continues the current grapheme cluster: `true` when there is
    /// no grapheme-cluster boundary between the cluster built so far and `c`,
    /// `false` when `c` starts a new cluster. This folds combining marks, ZWJ
    /// (zero-width joiner) emoji sequences, variation selectors, skin-tone
    /// modifiers, and regional-indicator flags onto one base. An incomplete or
    /// invalid boundary result counts as a boundary.
    ///
    /// Runs on `cluster` itself: `c` is appended, the boundary is read at the
    /// join, then `cluster` is truncated back to its original bytes.
    pub(super) fn continues_cluster(&mut self, c: char) -> bool {
        let base_len = self.cluster.len();
        self.cluster.push(c);
        let mut cursor = GraphemeCursor::new(base_len, self.cluster.len(), true);
        let joined = !cursor.is_boundary(&self.cluster, 0).unwrap_or(true);
        self.cluster.truncate(base_len);
        joined
    }

    /// Fold `c` into the current cluster: push it onto the base cell's
    /// combining tail without consuming a column, and re-fit the base when the
    /// cluster's display width changes — one column to two (e.g. VS16,
    /// `U+FE0F`, giving a text glyph its emoji form) widens it, two to one
    /// (e.g. VS15, `U+FE0E`) narrows it. With no cluster base, or with the base
    /// already holding [`MAX_GRAPHEME_CONTINUATIONS`], `c` is dropped.
    pub(super) fn extend_cluster(&mut self, c: char) {
        let Some((row, col)) = self.cluster_base else {
            return;
        };
        // A base at the cap takes no more continuations; `cluster` stops
        // growing too.
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

    /// Narrow the cluster's base at (`row`, `col`) from two cells to one after
    /// a continuation shrank its display width — e.g. a text-presentation
    /// selector (VS15, `U+FE0E`) on an emoji-presentation base. The base keeps
    /// its character, combining marks, and style with `width == 1`; the cell to
    /// its right is blanked in the pen background. The cursor moves to
    /// `col + 1` with the wrap latch cleared. When the base sits in the last
    /// column (kept narrow there by a refused promotion), the cursor stays
    /// parked on it, with the wrap latch armed under autowrap.
    fn demote_cluster_to_narrow(&mut self, row: u16, col: u16) {
        if let Some(slot) = self.active_grid_mut().cell_mut(row, col) {
            *slot = rebuilt_with_width(slot, 1);
        }
        let fill = self.active_render().style.bg_fill();
        if let Some(slot) = self.active_grid_mut().cell_mut(row, col + 1) {
            *slot = Cell::blank_with(fill);
        }
        // The glyph occupies one column; the cursor sits just past the base.
        let (_, cols) = self.active_grid().dimensions();
        let last_col = cols.saturating_sub(1);
        if col >= last_col {
            self.arm_wrap_latch(last_col);
        } else {
            self.active_cursor_mut().col = col + 1;
            self.clear_wrap_latch();
        }
    }

    /// Widen the cluster's base at (`row`, `col`) from one cell to two after a
    /// continuation grew its display width. With room to its right
    /// (`col < last_col`): the base keeps its character, combining marks, and
    /// style with `width == 2`, the column to its right becomes a width-0
    /// continuation, and the cursor steps past the claimed column or parks on
    /// the last column. In the last column of a multi-column grid: under
    /// autowrap the whole cluster moves to column 0 of the next line as a wide
    /// glyph, the vacated cell is blanked, and the row ends `SoftWide`; with
    /// autowrap off the base stays narrow where it sits. In a 1-column grid
    /// the base stays narrow where it sits.
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
            // The glyph ends at col + 1: park there when that is the last
            // column, else step past it.
            if col + 1 >= last_col {
                self.arm_wrap_latch(last_col);
            } else {
                self.active_cursor_mut().col = col + 2;
            }
        } else if last_col > 0 {
            // Base in the last column of a multi-column grid. With autowrap off
            // the base stays narrow where it sits (the continuation is already
            // on it) and the cursor stays put.
            if !self.modes.autowrap {
                return;
            }
            // Under autowrap the whole cluster moves to the next line as a wide
            // glyph.
            let Some((base_ch, style, marks)) = self
                .active_grid()
                .cell(row, col)
                .map(|cell| (cell.ch(), cell.style(), cell.combining().to_vec()))
            else {
                return;
            };
            let fill = self.active_render().style.bg_fill();
            if let Some(slot) = self.active_grid_mut().cell_mut(row, col) {
                *slot = Cell::blank_with(fill);
            }
            // The vacated last column is a wide-glyph spacer; `SoftWide` marks
            // the row so a reflow re-joins the rows and drops the spacer.
            self.wrap_linefeed(RowEnd::SoftWide);
            self.active_cursor_mut().col = 0;
            self.clear_wrap_latch();

            let new_row = self.active_cursor().row;
            let mut widened = Cell::new(base_ch, 2, style);
            for mark in &marks {
                widened.push_combining(*mark);
            }
            // `place_glyph` clears any wide pair at columns 0–1 that this write
            // would split.
            self.place_glyph(new_row, 0, widened);
            self.cluster_base = Some((new_row, 0));
            if 1 >= last_col {
                self.arm_wrap_latch(last_col);
            } else {
                self.active_cursor_mut().col = 2;
            }
        }
        // 1-column pane (`last_col == 0`): the base stays narrow where it sits,
        // with the promoting mark already on it.
    }

    /// Blank the orphaned half of any wide glyph a write at (`row`, `col`)
    /// would split. A wide base there (`width == 2`) loses its continuation to
    /// the right; a continuation there (`width == 0`) loses the base to its
    /// left. The freed half becomes a blank in the current pen background. A
    /// narrow cell, an out-of-bounds cell, or a continuation at column 0 is
    /// left as it is.
    pub(super) fn clear_wide_at(&mut self, row: u16, col: u16) {
        let fill = self.active_render().style.bg_fill();
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

    /// Install `base` at (`row`, `col`), first clearing any wide glyph the
    /// write would split. Every base write goes through here: a fresh base, an
    /// in-place widen, a wrapped widen. `base` carries its width (1 or 2),
    /// character, combining marks, and style. A width-2 base also writes a
    /// width-0 continuation placeholder at `col + 1`, after clearing whatever
    /// pair sat there; a width-1 base writes `col` alone. When `col + 1` is
    /// past the grid (a 1-column pane), a width-2 base is stored narrow. A
    /// write that reaches the row's last column sets the row end to `Hard`.
    /// Cursor and cluster bookkeeping stay with the caller.
    pub(super) fn place_glyph(&mut self, row: u16, col: u16, base: Cell) {
        let (_, cols) = self.active_grid().dimensions();
        // A width-2 base is stored only with its continuation column in bounds;
        // in a 1-column pane it is stored narrow.
        let wide = base.width() == 2 && col + 1 < cols;
        let base = if base.width() == 2 && !wide {
            rebuilt_with_width(&base, 1)
        } else {
            base
        };
        let style = base.style();
        // Clear any wide pair this write would split, on every column it lands
        // on.
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
        // A write reaching the row's last column resets the row end to `Hard`;
        // a wrap on the next glyph records `Soft` again.
        let end_col = if wide { col + 1 } else { col };
        if end_col + 1 >= cols {
            self.active_grid_mut().set_row_end(row, RowEnd::Hard);
        }
    }

    /// Repair `row`'s wide-glyph pairs after a cell op (erase / insert / delete)
    /// may have split one. The pair invariant: a wide base (`width == 2`) is
    /// always immediately followed by a width-0 continuation, and a
    /// continuation always immediately follows a wide base. Any half that
    /// breaks it — a base with no continuation to its right, or a continuation
    /// with no base to its left — is blanked in the current pen background.
    /// The scan runs left to right: a base blanked at `col` leaves its
    /// continuation at `col + 1` orphaned, and the next step blanks that too.
    pub(super) fn normalize_wide_pairs(&mut self, row: u16) {
        let (_, cols) = self.active_grid().dimensions();
        let fill = self.active_render().style.bg_fill();
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
