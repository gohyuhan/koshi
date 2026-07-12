//! Grapheme clustering and wide-glyph placement. A grapheme cluster is what a
//! reader sees as "one character" but that may be built from several Unicode
//! code points — e.g. an emoji plus a skin-tone modifier. This module folds
//! those continuations (combining marks, ZWJ — zero-width joiner — emoji
//! parts, variation selectors) onto a base cell, places narrow and wide
//! glyphs, and keeps wide-glyph pairs intact across edits.

use crate::grid::state::{Cell, RowEnd};
use crate::state::TerminalState;
use unicode_segmentation::GraphemeCursor;
use unicode_width::UnicodeWidthStr;

/// Upper bound on the number of continuation code points folded onto one cell's
/// base (the `combining` tail). Real grapheme clusters — even a skin-toned ZWJ
/// (zero-width joiner) emoji family — stay well under this; the cap bounds
/// per-cell memory against pathological input (e.g. a flood of combining
/// marks, "zalgo" text) from growing a single cell without limit. Continuations
/// past the cap are dropped.
pub(super) const MAX_GRAPHEME_CONTINUATIONS: usize = 32;

impl TerminalState {
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
    pub(super) fn reset_cluster(&mut self) {
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Whether `c` continues the current grapheme cluster, i.e. whether there is
    /// no grapheme-cluster boundary between the cluster built so far and `c` — a
    /// `false` result means `c` starts a new cluster. This is what folds
    /// combining marks, ZWJ (zero-width joiner) emoji sequences, variation
    /// selectors, skin-tone modifiers, and regional-
    /// indicator flags onto a single base. An ambiguous / incomplete result is
    /// treated as a boundary (start fresh) — the safe default.
    pub(super) fn continues_cluster(&self, c: char) -> bool {
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
    pub(super) fn extend_cluster(&mut self, c: char) {
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
        let fill = self.active_render().style.bg_fill();
        if let Some(slot) = self.active_grid_mut().cell_mut(row, col + 1) {
            *slot = Cell::blank_with(fill);
        }
        // The glyph now occupies one column; the cursor sits just past the base.
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
    /// continuation grew its display width. The base keeps its character,
    /// combining marks, and style but becomes `width == 2`; the column to its
    /// right becomes a width-0 continuation, and the cursor advances over the
    /// newly claimed column. If the base sits in the last column — no room to
    /// its right — the whole cluster moves to the next line, the same way a wide
    /// glyph wraps to keep itself on one line.
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
                self.arm_wrap_latch(last_col);
            } else {
                self.active_cursor_mut().col = col + 2;
            }
        } else if last_col > 0 {
            // Base in the last column of a multi-column grid: it cannot widen in
            // place. With autowrap off there is no wrap to make room, so the
            // cluster keeps its narrow form where it sits (the continuation is
            // already recorded on the base) and the cursor stays parked.
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
            // The vacated last column is a wide-glyph spacer: record the wrap
            // so a resize reflow re-joins the rows and drops the spacer.
            self.active_grid_mut().set_row_end(row, RowEnd::SoftWide);
            self.linefeed();
            self.active_cursor_mut().col = 0;
            self.clear_wrap_latch();

            let new_row = self.active_cursor().row;
            let mut widened = Cell::new(base_ch, 2, style);
            for mark in &marks {
                widened.push_combining(*mark);
            }
            // The destination row may already hold a wide glyph; `place_glyph`
            // clears any pair these writes would split before installing the
            // promoted cluster, so a wide base at col 1 never leaves its width-0
            // continuation at col 2 orphaned.
            self.place_glyph(new_row, 0, widened);
            self.cluster_base = Some((new_row, 0));
            if 1 >= last_col {
                self.arm_wrap_latch(last_col);
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

    /// Install `base` at (`row`, `col`), first clearing any wide glyph the write
    /// would split so the wide-pair invariant always holds. This is the single
    /// path EVERY base/continuation write goes through (a fresh base, an in-place
    /// widen, a wrapped widen), so no call site can forget to clear and orphan a
    /// half. `base` already carries its display width (1 or 2), character,
    /// combining marks, and style. A width-2 base also lays its width-0
    /// continuation placeholder at `col + 1` (after clearing whatever pair sat
    /// there); a width-1 base writes `col` alone. Cursor and cluster bookkeeping
    /// stay with the caller, since they differ per write site.
    pub(super) fn place_glyph(&mut self, row: u16, col: u16, base: Cell) {
        let (_, cols) = self.active_grid().dimensions();
        // A wide glyph needs its continuation column in bounds. In a pane too
        // narrow to hold the pair (e.g. a 1-column split, where even col 0 is the
        // last column) there is no room, so the base is stored as a single
        // narrow cell, keeping the wide-pair invariant intact: a width-2 base
        // with no continuation would let a later erase / cell op treat the lone
        // cell as an orphan and blank it, and would let a renderer trusting
        // widths draw an impossible row.
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
        // A write that reaches the row's last column replaces whatever the
        // previous wrap left there, so the row's continuation state resets;
        // an actual wrap on the NEXT glyph re-records it.
        let end_col = if wide { col + 1 } else { col };
        if end_col + 1 >= cols {
            self.active_grid_mut().set_row_end(row, RowEnd::Hard);
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
