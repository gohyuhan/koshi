//! Grapheme clustering and wide-glyph placement. A grapheme cluster is what a
//! reader sees as "one character" but that may be built from several Unicode
//! code points — e.g. an emoji plus a skin-tone modifier. This module folds
//! those continuations (combining marks, ZWJ — zero-width joiner — emoji
//! parts, variation selectors) onto a base cell, places narrow and wide
//! glyphs, and keeps wide-glyph pairs intact across edits.
//!
//! A cluster takes at most 2 columns. A halfwidth katakana sound mark
//! (`U+FF9E`, `U+FF9F`), and a continuation that makes its cluster wider than
//! 2 columns, start a new cell.

use crate::grid::state::{Cell, RowEnd, MAX_GRAPHEME_CONTINUATION_COUNT};
use crate::state::{rebuild_cell_with_width, Charset, TerminalState};
use koshi_core::display_width::{compute_text_display_width, is_halfwidth_katakana_sound_mark};
use unicode_segmentation::GraphemeCursor;

impl TerminalState {
    /// Print every character of `printed_text` in order, as one `print` call
    /// per character leaves the screen, the cursor and the grapheme cluster.
    ///
    /// Each run of printable ASCII characters that fits before the right wrap
    /// bound is written into its row at once; every other character goes
    /// through `print`. Example — `ab\u{301}c` at column 0 writes `ab` at once,
    /// folds `\u{301}` onto `b`, then writes `c` at column 2.
    pub(crate) fn print_text(&mut self, printed_text: &str) {
        let mut unprinted_text = printed_text;
        while let Some(first_character) = unprinted_text.chars().next() {
            let printed_byte_count = match self.print_ascii_run(unprinted_text.as_bytes()) {
                0 => {
                    vte::Perform::print(self, first_character);
                    first_character.len_utf8()
                }
                ascii_byte_count => ascii_byte_count,
            };
            unprinted_text = &unprinted_text[printed_byte_count..];
        }
    }

    /// Print the leading printable ASCII characters (`0x20`–`0x7E`) of
    /// `text_bytes` that fit on the cursor row up to the right wrap bound, and
    /// return how many it printed. A run that starts left of the left margin
    /// stops at the column before the left margin. Each cell, the cursor, the
    /// wrap latch and the grapheme cluster end as `print` leaves them for the
    /// same characters.
    ///
    /// Prints nothing and returns `0` when the first byte is not printable
    /// ASCII, the wrap latch is armed, insert mode (IRM) is on, the active
    /// charset is not ASCII, or the grapheme cluster built so far holds more
    /// than one byte.
    ///
    /// The run blanks, in the pen background, the base of a wide glyph whose
    /// right half is its first cell, and the right half of a wide glyph whose
    /// base is its last cell. It clears the image fragments of every cell it
    /// writes or blanks.
    fn print_ascii_run(&mut self, text_bytes: &[u8]) -> usize {
        let Some(first_text_byte) = text_bytes.first() else {
            return 0;
        };
        if !is_printable_ascii_byte(*first_text_byte)
            || self.get_active_cursor().is_wrap_pending
            || self.modes.is_insert_mode_enabled
            || self.cluster.len() > 1
            || self.get_active_charset() != Charset::Ascii
        {
            return 0;
        }
        let (_, last_column_index) = self.get_horizontal_wrap_bounds();
        let cursor_row_index = self.get_active_cursor().row;
        let first_column_index = self.get_active_cursor().column;
        // A run that starts left of the left margin ends at the column before
        // it. From the left margin on, the margins are the wrap bounds.
        let last_run_column_index = match self.get_horizontal_margins() {
            Some((left_margin_column_index, _))
                if first_column_index < left_margin_column_index =>
            {
                left_margin_column_index - 1
            }
            _ => last_column_index,
        };
        let row_room_count =
            usize::from(last_run_column_index.saturating_sub(first_column_index)) + 1;
        let ascii_byte_count = text_bytes[..text_bytes.len().min(row_room_count)]
            .iter()
            .take_while(|text_byte| is_printable_ascii_byte(**text_byte))
            .count();
        let last_written_column_index = first_column_index + (ascii_byte_count as u16 - 1);

        let active_grid = self.get_active_grid();
        let is_first_cell_wide_glyph_right_half = first_column_index > 0
            && active_grid
                .get_cell(cursor_row_index, first_column_index)
                .is_some_and(|cell| cell.get_display_width() == 0);
        let is_last_cell_wide_glyph_base = active_grid
            .get_cell(cursor_row_index, last_written_column_index)
            .is_some_and(|cell| cell.get_display_width() == 2);
        let (_, column_count) = active_grid.get_grid_dimensions();
        let cleared_first_column_index = if is_first_cell_wide_glyph_right_half {
            first_column_index - 1
        } else {
            first_column_index
        };
        let cleared_end_column_index = if is_last_cell_wide_glyph_base {
            last_written_column_index + 2
        } else {
            last_written_column_index + 1
        };
        self.clear_images_at_cells(
            cursor_row_index,
            cleared_first_column_index,
            cleared_end_column_index - cleared_first_column_index,
        );

        let style = self.get_active_render().style;
        let background_style = style.get_background_fill_style();
        let active_grid = self.get_active_grid_mut();
        if is_first_cell_wide_glyph_right_half {
            if let Some(wide_glyph_base_cell) =
                active_grid.get_cell_mut(cursor_row_index, first_column_index - 1)
            {
                *wide_glyph_base_cell = Cell::build_blank_with_style(background_style);
            }
        }
        if is_last_cell_wide_glyph_base {
            if let Some(wide_glyph_right_half_cell) =
                active_grid.get_cell_mut(cursor_row_index, last_written_column_index + 1)
            {
                *wide_glyph_right_half_cell = Cell::build_blank_with_style(background_style);
            }
        }
        active_grid.write_ascii_cells(
            cursor_row_index,
            first_column_index,
            &text_bytes[..ascii_byte_count],
            style,
        );
        if last_written_column_index + 1 >= column_count {
            active_grid.set_row_end(cursor_row_index, RowEnd::Hard);
        }

        self.cluster.clear();
        self.cluster
            .push(char::from(text_bytes[ascii_byte_count - 1]));
        self.cluster_base = Some((cursor_row_index, last_written_column_index));
        if last_written_column_index >= last_column_index {
            self.arm_wrap_latch();
        } else {
            self.get_active_cursor_mut().column = last_written_column_index + 1;
        }
        ascii_byte_count
    }

    /// Discard any in-progress grapheme cluster. Every non-printing event
    /// (control bytes, CSI / ESC / OSC, DCS hook/unhook) calls it; a
    /// continuation printed after one starts a new cluster.
    ///
    /// A malformed CSI that vte routes to its internal `CsiIgnore` state (e.g.
    /// `CSI 1 < m`, a private marker after a parameter) ends with no `Perform`
    /// callback and never reaches here: a combining mark printed after it
    /// folds onto the preceding glyph.
    pub(in crate::state) fn reset_cluster(&mut self) {
        self.cluster.clear();
        self.cluster_base = None;
    }

    /// Whether `character` continues the current grapheme cluster: `true` when there is
    /// no grapheme-cluster boundary between the cluster built so far and `character`,
    /// and the cluster with `character` is at most 2 columns wide
    /// ([`compute_text_display_width`]). `false` when `character` starts a new
    /// cluster. This folds combining marks, ZWJ (zero-width joiner) emoji
    /// sequences, variation selectors, skin-tone modifiers, and
    /// regional-indicator flags onto one base. An incomplete or invalid boundary
    /// result counts as a boundary.
    ///
    /// A halfwidth katakana sound mark (`U+FF9E`, `U+FF9F`) always starts a new
    /// cluster: `ｶﾞ` is two clusters of 1 column each. A character that makes
    /// the cluster wider than 2 columns starts a new cluster: `क्ष` is 2 columns
    /// wide, `क्षि` is 3, so the vowel sign `ि` (`U+093F`) starts a new cluster.
    ///
    /// Runs on `cluster` itself: `character` is appended, the boundary is read,
    /// and with no boundary the width too, then `cluster` is truncated back to
    /// its original bytes.
    pub(super) fn is_character_continuing_cluster(&mut self, character: char) -> bool {
        if is_halfwidth_katakana_sound_mark(character) {
            return false;
        }
        let cluster_byte_length = self.cluster.len();
        self.cluster.push(character);
        let mut grapheme_cursor =
            GraphemeCursor::new(cluster_byte_length, self.cluster.len(), true);
        let is_grapheme_continuation = !grapheme_cursor
            .is_boundary(&self.cluster, 0)
            .unwrap_or(true);
        let is_continuation_within_two_columns =
            is_grapheme_continuation && compute_text_display_width(&self.cluster) <= 2;
        self.cluster.truncate(cluster_byte_length);
        is_continuation_within_two_columns
    }

    /// Fold `character` into the current cluster: push it onto the base cell's
    /// combining tail without consuming a column, and re-fit the base when the
    /// cluster's display width ([`compute_text_display_width`]) changes — one
    /// column to two (e.g. VS16, `U+FE0F`, giving a text glyph its emoji form)
    /// widens it, two to one (e.g. VS15, `U+FE0E`) narrows it. With no cluster
    /// base, or with the base already holding [`MAX_GRAPHEME_CONTINUATION_COUNT`],
    /// `character` is dropped.
    pub(super) fn extend_cluster(&mut self, character: char) {
        let Some((row_index, column_index)) = self.cluster_base else {
            return;
        };
        // A base at the cap takes no more continuations; `cluster` stops
        // growing too.
        if self
            .get_active_grid()
            .get_cell(row_index, column_index)
            .map_or(0, |cell| cell.list_combining_characters().len())
            >= MAX_GRAPHEME_CONTINUATION_COUNT
        {
            return;
        }
        let old_cluster_display_width = compute_text_display_width(&self.cluster);
        self.cluster.push(character);
        let new_cluster_display_width = compute_text_display_width(&self.cluster);

        if let Some(cell) = self
            .get_active_grid_mut()
            .get_cell_mut(row_index, column_index)
        {
            if cell.get_image_placeholder().is_some() {
                cell.set_image_placeholder_diacritic(character);
            } else {
                cell.push_combining(character);
            }
        }
        if old_cluster_display_width == 1 && new_cluster_display_width == 2 {
            self.promote_cluster_to_wide(row_index, column_index);
        } else if old_cluster_display_width == 2 && new_cluster_display_width == 1 {
            self.demote_cluster_to_narrow(row_index, column_index);
        }
    }

    /// Narrow the cluster's base at (`row_index`, `column_index`) from two cells
    /// to one after a continuation shrank its display width — e.g. a
    /// text-presentation selector (VS15, `U+FE0E`) on an emoji-presentation
    /// base. The base keeps its character, combining marks, and style with
    /// `width == 1`; the cell to its right is blanked in the pen background. The
    /// cursor moves to `column_index + 1` with the wrap latch cleared. When the base sits at the
    /// effective right bound (kept narrow there by a refused promotion), the
    /// cursor stays parked on it, with the wrap latch armed under autowrap.
    fn demote_cluster_to_narrow(&mut self, row_index: u16, column_index: u16) {
        self.clear_images_at_cells(row_index, column_index, 2);
        if let Some(cluster_base_cell) = self
            .get_active_grid_mut()
            .get_cell_mut(row_index, column_index)
        {
            *cluster_base_cell = rebuild_cell_with_width(cluster_base_cell, 1);
        }
        let background_style = self.get_active_render().style.get_background_fill_style();
        if let Some(vacated_cell) = self
            .get_active_grid_mut()
            .get_cell_mut(row_index, column_index + 1)
        {
            *vacated_cell = Cell::build_blank_with_style(background_style);
        }
        // The glyph occupies one column; the cursor sits just past the base.
        let (_, last_column_index) = self.get_horizontal_wrap_bounds_for_column(column_index);
        if column_index >= last_column_index {
            self.arm_wrap_latch();
        } else {
            self.get_active_cursor_mut().column = column_index + 1;
            self.clear_wrap_latch();
        }
    }

    /// Widen the cluster's base at (`row_index`, `column_index`) from one cell to
    /// two after a continuation grew its display width. With room to its right
    /// (`column_index < last_column_index`): the base keeps its character, combining marks, and
    /// style with `width == 2`, the column to its right becomes a width-0
    /// continuation, and the cursor steps past the claimed column or parks on
    /// the effective right bound. At the effective right bound of a multi-column
    /// region: under autowrap the whole cluster moves to the region's left bound
    /// on the next line as a wide glyph, the vacated cell is blanked, and the row
    /// ends `SoftWide`; with autowrap off the base stays narrow where it sits. In
    /// a 1-column region the base stays narrow where it sits.
    fn promote_cluster_to_wide(&mut self, row_index: u16, column_index: u16) {
        let (first_column_index, last_column_index) =
            self.get_horizontal_wrap_bounds_for_column(column_index);

        if column_index < last_column_index {
            // Room to the right: widen the base in place and claim column + 1.
            let Some(widened_base_cell) = self
                .get_active_grid()
                .get_cell(row_index, column_index)
                .map(|cell| rebuild_cell_with_width(cell, 2))
            else {
                return;
            };
            self.place_glyph(row_index, column_index, widened_base_cell);
            // The glyph ends at column + 1: park there when that is the
            // effective right bound, else step past it.
            if column_index + 1 >= last_column_index {
                self.arm_wrap_latch();
            } else {
                self.get_active_cursor_mut().column = column_index + 2;
            }
        } else if first_column_index < last_column_index {
            // Base at the effective right bound of a multi-column region. With
            // autowrap off the base stays narrow where it sits (the continuation
            // is already on it) and the cursor stays put.
            if !self.modes.is_autowrap_enabled {
                return;
            }
            // Under autowrap the whole cluster moves to the next line as a wide
            // glyph.
            let Some((base_character, cell_style, combining_characters)) = self
                .get_active_grid()
                .get_cell(row_index, column_index)
                .map(|cell| {
                    (
                        cell.get_character(),
                        cell.get_style(),
                        cell.list_combining_characters().to_vec(),
                    )
                })
            else {
                return;
            };
            let background_style = self.get_active_render().style.get_background_fill_style();
            self.clear_images_at_cells(row_index, column_index, 1);
            if let Some(vacated_cell) = self
                .get_active_grid_mut()
                .get_cell_mut(row_index, column_index)
            {
                *vacated_cell = Cell::build_blank_with_style(background_style);
            }
            // The vacated right bound is a wide-glyph spacer; `SoftWide` marks
            // the row so a reflow re-joins the rows and drops the spacer.
            self.wrap_linefeed(RowEnd::SoftWide);
            self.get_active_cursor_mut().column = first_column_index;
            self.clear_wrap_latch();

            let new_base_row_index = self.get_active_cursor().row;
            let mut widened_base_cell = Cell::from_character(base_character, 2, cell_style);
            for combining_character in &combining_characters {
                widened_base_cell.push_combining(*combining_character);
            }
            // `place_glyph` clears any wide pair at the new left bound that
            // this write would split.
            self.place_glyph(new_base_row_index, first_column_index, widened_base_cell);
            self.cluster_base = Some((new_base_row_index, first_column_index));
            if first_column_index.saturating_add(1) >= last_column_index {
                self.arm_wrap_latch();
            } else {
                self.get_active_cursor_mut().column = first_column_index.saturating_add(2);
            }
        }
        // 1-column region (`last_column_index == first_column_index`): the base stays narrow
        // where it sits, with the promoting mark already on it.
    }

    /// Blank the orphaned half of any wide glyph a write at (`row_index`,
    /// `column_index`) would split. A wide base there (`width == 2`) loses its continuation to
    /// the right; a continuation there (`width == 0`) loses the base to its
    /// left. The freed half becomes a blank in the current pen background. A
    /// narrow cell, an out-of-bounds cell, or a continuation at column 0 is
    /// left as it is.
    pub(super) fn clear_wide_glyph_at(&mut self, row_index: u16, column_index: u16) {
        let cell_display_width = self
            .get_active_grid()
            .get_cell(row_index, column_index)
            .map_or(1, Cell::get_display_width);
        match cell_display_width {
            0 if column_index > 0 => self.clear_images_at_cells(row_index, column_index - 1, 2),
            2 => self.clear_images_at_cells(row_index, column_index, 2),
            _ => self.clear_images_at_cells(row_index, column_index, 1),
        }
        let background_style = self.get_active_render().style.get_background_fill_style();
        self.get_active_grid_mut()
            .clear_wide_glyph_at(row_index, column_index, background_style);
    }

    /// Install `base_cell` at (`row_index`, `column_index`), first clearing any
    /// wide glyph the write would split. Every base write goes through here: a
    /// fresh base, an in-place widen, a wrapped widen. `base_cell` carries its
    /// width (1 or 2), character, combining marks, and style. A width-2 base
    /// also writes a width-0 continuation placeholder at `column_index + 1`,
    /// after clearing whatever pair sat there; a width-1 base writes
    /// `column_index` alone. When `column_index + 1` is past the grid (a
    /// 1-column pane), a width-2 base is stored narrow. A
    /// write that reaches the row's last column sets the row end to `Hard`.
    /// Cursor and cluster bookkeeping stay with the caller.
    pub(super) fn place_glyph(&mut self, row_index: u16, column_index: u16, base_cell: Cell) {
        let (_, column_count) = self.get_active_grid().get_grid_dimensions();
        let old_cell_display_width = self
            .get_active_grid()
            .get_cell(row_index, column_index)
            .map_or(1, Cell::get_display_width);
        // A width-2 base is stored only with its continuation column in bounds;
        // in a 1-column pane it is stored narrow.
        let is_wide_glyph = base_cell.get_display_width() == 2 && column_index + 1 < column_count;
        let base_cell = if base_cell.get_display_width() == 2 && !is_wide_glyph {
            rebuild_cell_with_width(&base_cell, 1)
        } else {
            base_cell
        };
        if old_cell_display_width == 0 && column_index > 0 {
            self.clear_images_at_cells(row_index, column_index - 1, 1);
        }
        self.clear_images_at_cells(
            row_index,
            column_index,
            if is_wide_glyph || old_cell_display_width == 2 {
                2
            } else {
                1
            },
        );
        let cell_style = base_cell.get_style();
        // Clear any wide pair this write would split, on every column it lands
        // on.
        self.clear_wide_glyph_at(row_index, column_index);
        if is_wide_glyph {
            self.clear_wide_glyph_at(row_index, column_index + 1);
        }
        if let Some(target_cell) = self
            .get_active_grid_mut()
            .get_cell_mut(row_index, column_index)
        {
            *target_cell = base_cell;
        }
        // A wide glyph's second column is a width-0 continuation placeholder,
        // covered by the glyph's left half; the renderer skips it.
        if is_wide_glyph {
            if let Some(continuation_cell) = self
                .get_active_grid_mut()
                .get_cell_mut(row_index, column_index + 1)
            {
                *continuation_cell = Cell::from_character(' ', 0, cell_style);
            }
        }
        // A write reaching the row's last column resets the row end to `Hard`;
        // a wrap on the next glyph records `Soft` again.
        let end_column_index = if is_wide_glyph {
            column_index + 1
        } else {
            column_index
        };
        if end_column_index + 1 >= column_count {
            self.get_active_grid_mut()
                .set_row_end(row_index, RowEnd::Hard);
        }
    }

    /// Repair `row_index`'s wide-glyph pairs after a cell op (erase / insert / delete)
    /// may have split one. The pair invariant: a wide base (`width == 2`) is
    /// always immediately followed by a width-0 continuation, and a
    /// continuation always immediately follows a wide base. Any half that
    /// breaks it — a base with no continuation to its right, or a continuation
    /// with no base to its left — is blanked in the current pen background.
    /// The scan runs left to right: a base blanked at `column_index` leaves its
    /// continuation at `column_index + 1` orphaned, and the next step blanks that too.
    pub(super) fn normalize_wide_pairs(&mut self, row_index: u16) {
        let (_, column_count) = self.get_active_grid().get_grid_dimensions();
        let background_style = self.get_active_render().style.get_background_fill_style();
        for column_index in 0..column_count {
            let is_wide_pair_cell_orphaned = match self
                .get_active_grid()
                .get_cell(row_index, column_index)
                .map_or(1, Cell::get_display_width)
            {
                // Wide base needs a continuation immediately to its right.
                2 => self
                    .get_active_grid()
                    .get_cell(row_index, column_index + 1)
                    .is_none_or(|cell| cell.get_display_width() != 0),
                // Continuation needs a wide base immediately to its left.
                0 => {
                    column_index == 0
                        || self
                            .get_active_grid()
                            .get_cell(row_index, column_index - 1)
                            .is_none_or(|cell| cell.get_display_width() != 2)
                }
                _ => false,
            };
            if is_wide_pair_cell_orphaned {
                self.clear_images_at_cells(row_index, column_index, 1);
                if let Some(cell) = self
                    .get_active_grid_mut()
                    .get_cell_mut(row_index, column_index)
                {
                    *cell = Cell::build_blank_with_style(background_style);
                }
            }
        }
    }
}

/// Whether `text_byte` is a printable ASCII character: `0x20` (space) through
/// `0x7E` (`~`).
fn is_printable_ascii_byte(text_byte: u8) -> bool {
    (0x20..=0x7e).contains(&text_byte)
}
