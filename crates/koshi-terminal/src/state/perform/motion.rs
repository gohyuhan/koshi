//! Cursor motion, scrolling, and the scroll region: line feed and reverse
//! index, save / restore cursor, absolute placement, the deferred-wrap latch,
//! and tab-stop math.

use crate::grid::state::RowEnd;
use crate::state::{RenderState, SavedCursor, Screen, TerminalState};
use crate::style::Style;

impl TerminalState {
    /// The scroll-region margins as 0-based inclusive `(top_row_index,
    /// bottom_row_index)` rows,
    /// resolving `None` to the whole active grid.
    pub(super) fn get_scroll_region_bounds(&self) -> (u16, u16) {
        let last_grid_row_index = self
            .get_active_grid()
            .get_grid_dimensions()
            .0
            .saturating_sub(1);
        self.get_scroll_region().unwrap_or((0, last_grid_row_index))
    }

    /// The horizontal-margin bounds as 0-based inclusive `(left, right)`
    /// columns, resolving `None` to the full active grid width.
    pub(super) fn get_horizontal_margin_bounds(&self) -> (u16, u16) {
        let last_grid_column_index = self
            .get_active_grid()
            .get_grid_dimensions()
            .1
            .saturating_sub(1);
        self.get_horizontal_margins()
            .unwrap_or((0, last_grid_column_index))
    }

    /// The row bounds used by cursor movement. DECOM confines movement to the
    /// active vertical region; normal mode uses the full active grid.
    pub(super) fn get_cursor_row_bounds(&self) -> (u16, u16) {
        let last_grid_row_index = self
            .get_active_grid()
            .get_grid_dimensions()
            .0
            .saturating_sub(1);
        if self.active_cursor().origin {
            self.get_scroll_region_bounds()
        } else {
            (0, last_grid_row_index)
        }
    }

    /// The column bounds used by cursor movement. DECOM confines movement to
    /// the active horizontal region; normal mode uses the full active grid.
    pub(super) fn get_cursor_column_bounds(&self) -> (u16, u16) {
        let last_grid_column_index = self
            .get_active_grid()
            .get_grid_dimensions()
            .1
            .saturating_sub(1);
        if self.active_cursor().origin {
            self.get_horizontal_margin_bounds()
        } else {
            (0, last_grid_column_index)
        }
    }

    /// The row and column offsets applied to CUP, HVP, and VPA under DECOM.
    pub(super) fn get_cursor_origin_offsets(&self) -> (u16, u16) {
        if self.active_cursor().origin {
            let (top_row_index, _) = self.get_scroll_region_bounds();
            let (left_column_index, _) = self.get_horizontal_margin_bounds();
            (top_row_index, left_column_index)
        } else {
            (0, 0)
        }
    }

    /// Delete `line_count` lines starting at `first_row_index`, scrolling the
    /// band `first_row_index..=bottom_row_index` up and filling the vacated
    /// bottom rows with `fill_style`.
    ///
    /// When `first_row_index == 0` on the primary screen — a line feed at the
    /// bottom margin of a region starting at row 0, an SU whose region starts
    /// at row 0, a DL with the cursor on row 0 — the departing rows
    /// `0..min(line_count, bottom_row_index + 1)`
    /// go into scrollback first, oldest first, each with its row end and prompt
    /// mark. The alternate screen, and a delete with `first > 0`, feed nothing:
    /// the removed lines are discarded.
    pub(in crate::state) fn delete_lines_into_scrollback(
        &mut self,
        first_row_index: u16,
        bottom_row_index: u16,
        line_count: u16,
        fill_style: Style,
    ) {
        let (grid_row_count, grid_column_count) = self.get_active_grid().get_grid_dimensions();
        if grid_row_count == 0
            || first_row_index > bottom_row_index
            || first_row_index >= grid_row_count
        {
            return;
        }
        let bottom_row_index = bottom_row_index.min(grid_row_count.saturating_sub(1));
        let scroll_region_row_count = bottom_row_index
            .saturating_sub(first_row_index)
            .saturating_add(1);
        let shifted_row_count = line_count.min(scroll_region_row_count);
        let (left_column_index, right_column_index) = self.get_horizontal_margin_bounds();
        let is_full_width =
            left_column_index == 0 && right_column_index == grid_column_count.saturating_sub(1);
        let previous_scrollback_line_count = self.scrollback.get_total_pushed_line_count();
        let should_feed_scrollback =
            self.active_screen == Screen::Primary && first_row_index == 0 && is_full_width;
        let mut has_removed_native_image_source_fragments = false;
        if should_feed_scrollback {
            if self.native_fragment_count_by_image_source_id.is_empty() {
                for row_index in 0..shifted_row_count {
                    if let Some(scrolled_off_row) = self.primary.list_rows().get(row_index as usize)
                    {
                        let row_metadata = self.primary.get_row_metadata(row_index);
                        self.scrollback.push_row(scrolled_off_row, row_metadata);
                    }
                }
            } else {
                let primary_grid = &self.primary;
                let scrollback = &mut self.scrollback;
                let native_fragment_count_by_image_source_id =
                    &mut self.native_fragment_count_by_image_source_id;
                for row_index in 0..shifted_row_count {
                    if let Some(scrolled_off_row) = primary_grid.list_rows().get(row_index as usize)
                    {
                        let row_metadata = primary_grid.get_row_metadata(row_index);
                        scrollback.push_row_with_evicted(
                            scrolled_off_row,
                            row_metadata,
                            |evicted_rows| {
                                has_removed_native_image_source_fragments |=
                                    super::super::images::discard_native_fragment_references(
                                        native_fragment_count_by_image_source_id,
                                        evicted_rows.iter(),
                                    );
                            },
                        );
                    }
                }
            }
        } else {
            has_removed_native_image_source_fragments |= self.discard_active_image_fragments(
                first_row_index,
                first_row_index.saturating_add(shifted_row_count),
                if is_full_width { 0 } else { left_column_index },
                if is_full_width {
                    grid_column_count
                } else {
                    right_column_index.saturating_add(1)
                },
            );
        }
        if is_full_width {
            self.active_grid_mut().delete_lines(
                first_row_index,
                bottom_row_index,
                line_count,
                fill_style,
            );
        } else {
            self.active_grid_mut().delete_lines_in_columns(
                first_row_index,
                bottom_row_index,
                line_count,
                left_column_index,
                right_column_index,
                fill_style,
            );
        }
        self.finish_native_fragment_removal(has_removed_native_image_source_fragments);

        if shifted_row_count == 0 {
            return;
        }
        self.scroll_image_rows_in_columns(
            first_row_index,
            bottom_row_index,
            shifted_row_count,
            true,
            (left_column_index, right_column_index),
            previous_scrollback_line_count,
        );
    }

    /// Insert `line_count` blank lines at `first_row_index`, shifting the rest
    /// of the region down.
    /// Fully contained image rectangles move with rows that remain in the region.
    pub(super) fn insert_lines_preserving_images(
        &mut self,
        first_row_index: u16,
        bottom_row_index: u16,
        line_count: u16,
        fill_style: Style,
    ) {
        let (grid_row_count, grid_column_count) = self.get_active_grid().get_grid_dimensions();
        if grid_row_count == 0
            || first_row_index > bottom_row_index
            || first_row_index >= grid_row_count
        {
            return;
        }
        let bottom_row_index = bottom_row_index.min(grid_row_count.saturating_sub(1));
        let scroll_region_row_count = bottom_row_index
            .saturating_sub(first_row_index)
            .saturating_add(1);
        let shifted_row_count = line_count.min(scroll_region_row_count);
        let (left_column_index, right_column_index) = self.get_horizontal_margin_bounds();
        let is_full_width =
            left_column_index == 0 && right_column_index == grid_column_count.saturating_sub(1);
        let previous_scrollback_line_count = self.scrollback.get_total_pushed_line_count();
        let discarded_first_row_index = bottom_row_index
            .saturating_add(1)
            .saturating_sub(shifted_row_count);
        let has_removed_native_image_source_fragments = self.discard_active_image_fragments(
            discarded_first_row_index,
            bottom_row_index.saturating_add(1),
            if is_full_width { 0 } else { left_column_index },
            if is_full_width {
                grid_column_count
            } else {
                right_column_index.saturating_add(1)
            },
        );
        if is_full_width {
            self.active_grid_mut().insert_lines(
                first_row_index,
                bottom_row_index,
                line_count,
                fill_style,
            );
        } else {
            self.active_grid_mut().insert_lines_in_columns(
                first_row_index,
                bottom_row_index,
                line_count,
                left_column_index,
                right_column_index,
                fill_style,
            );
        }
        self.finish_native_fragment_removal(has_removed_native_image_source_fragments);

        if shifted_row_count == 0 {
            return;
        }
        self.scroll_image_rows_in_columns(
            first_row_index,
            bottom_row_index,
            shifted_row_count,
            false,
            (left_column_index, right_column_index),
            previous_scrollback_line_count,
        );
    }

    /// Move the cursor down one line. At the scroll region's bottom margin the
    /// cursor stays and the region scrolls up one line; on any other row the
    /// cursor moves down, stopping at the last grid row. The column does not
    /// change.
    pub(super) fn linefeed(&mut self) {
        let (top_row_index, bottom_row_index) = self.get_scroll_region_bounds();
        if self.active_cursor().row == bottom_row_index {
            let fill_style = self.active_render().style.get_background_fill_style();
            self.delete_lines_into_scrollback(top_row_index, bottom_row_index, 1, fill_style);
        } else {
            let last_grid_row_index = self
                .get_active_grid()
                .get_grid_dimensions()
                .0
                .saturating_sub(1);
            if self.active_cursor().row < last_grid_row_index {
                self.active_cursor_mut().row += 1;
            }
        }
    }

    /// Record `continued_row_end` on the row the line feed leaves behind, then
    /// [`linefeed`](Self::linefeed).
    ///
    /// The cursor moved down: its old row gets `continued_row_end`. The region scrolled: the
    /// cursor row is marked before the scroll, so a row leaving the top carries
    /// `continued_row_end` and its prompt mark into scrollback, and the row directly
    /// above the cursor gets `continued_row_end` afterwards (`delete_lines` reset it to
    /// [`RowEnd::Hard`]). The cursor neither moved nor scrolled (last grid row
    /// outside the region): no row is marked, since no row continues it.
    pub(super) fn wrap_linefeed(&mut self, continued_row_end: RowEnd) {
        let previous_cursor_row_index = self.active_cursor().row;
        let is_at_scroll_bottom = previous_cursor_row_index == self.get_scroll_region_bounds().1;
        if is_at_scroll_bottom {
            self.active_grid_mut()
                .set_row_end(previous_cursor_row_index, continued_row_end);
        }
        self.linefeed();

        let current_cursor_row_index = self.active_cursor().row;
        let continued_row_index = if current_cursor_row_index > previous_cursor_row_index {
            Some(previous_cursor_row_index)
        } else if is_at_scroll_bottom {
            current_cursor_row_index.checked_sub(1)
        } else {
            None
        };
        if let Some(continued_row_index) = continued_row_index {
            self.active_grid_mut()
                .set_row_end(continued_row_index, continued_row_end);
        }
    }

    /// Reverse index (RI): move the cursor up one line. At the scroll region's
    /// top margin the cursor stays and the region scrolls down one line; on row
    /// 0 outside a region the cursor stays. Clears the deferred-wrap latch.
    pub(super) fn reverse_index(&mut self) {
        let (top_row_index, bottom_row_index) = self.get_scroll_region_bounds();
        if self.active_cursor().row == top_row_index {
            let fill_style = self.active_render().style.get_background_fill_style();
            self.insert_lines_preserving_images(top_row_index, bottom_row_index, 1, fill_style);
        } else if self.active_cursor().row > 0 {
            self.active_cursor_mut().row -= 1;
        }
        self.clear_wrap_latch();
    }

    /// Save the cursor position, origin mode, wrap latch, and the active
    /// screen's render state (DECSC / SCOSC) into the active screen's cursor.
    /// Each screen keeps its own snapshot.
    pub(super) fn save_cursor(&mut self) {
        let cursor = *self.active_cursor();
        let render = *self.active_render();
        self.active_cursor_mut().saved = Some(SavedCursor {
            row: cursor.row,
            column: cursor.column,
            pending_wrap: cursor.pending_wrap,
            origin: cursor.origin,
            render,
        });
    }

    /// Restore the cursor position, origin mode, wrap latch, and render state
    /// saved by [`save_cursor`](Self::save_cursor) (DECRC / SCORC), clamping the
    /// position into the current grid or active origin region. With no saved
    /// cursor, home the cursor, clear origin mode and the wrap latch, and reset
    /// the render state to [`RenderState::fresh`]. The saved snapshot stays.
    pub(super) fn restore_cursor(&mut self) {
        let saved_cursor = self.active_cursor().saved;
        let (grid_row_count, grid_column_count) = self.get_active_grid().get_grid_dimensions();
        if let Some(saved) = saved_cursor {
            let (minimum_row_index, maximum_row_index) = if saved.origin {
                self.get_scroll_region_bounds()
            } else {
                (0, grid_row_count.saturating_sub(1))
            };
            let (minimum_column_index, maximum_column_index) = if saved.origin {
                self.get_horizontal_margin_bounds()
            } else {
                (0, grid_column_count.saturating_sub(1))
            };
            let cursor = self.active_cursor_mut();
            cursor.origin = saved.origin;
            cursor.row = saved.row.max(minimum_row_index).min(maximum_row_index);
            cursor.column = saved
                .column
                .max(minimum_column_index)
                .min(maximum_column_index);
            cursor.pending_wrap = saved.pending_wrap;
            *self.active_render_mut() = saved.render;
        } else {
            let cursor = self.active_cursor_mut();
            cursor.row = 0;
            cursor.column = 0;
            cursor.origin = false;
            cursor.pending_wrap = false;
            *self.active_render_mut() = RenderState::fresh();
        }
    }

    /// Move the cursor to an absolute (`target_row_index`, `target_column_index`),
    /// clamped into the active grid or active origin region, and clear the
    /// deferred-wrap latch. Every absolute cursor placement — CUP/HVP,
    /// CHA/HPA, VPA, CNL, CPL — routes through here.
    pub(super) fn move_cursor_to(&mut self, target_row_index: u16, target_column_index: u16) {
        let (minimum_row_index, maximum_row_index) = self.get_cursor_row_bounds();
        let (minimum_column_index, maximum_column_index) = self.get_cursor_column_bounds();
        let cursor = self.active_cursor_mut();
        cursor.row = target_row_index
            .max(minimum_row_index)
            .min(maximum_row_index);
        cursor.column = target_column_index
            .max(minimum_column_index)
            .min(maximum_column_index);
        cursor.pending_wrap = false;
    }

    /// Park the cursor on the active horizontal right margin. With autowrap
    /// (DECAWM `?7`) on, arm the deferred-wrap latch: the next glyph wraps
    /// before printing. With autowrap off, clear the latch so the next glyph
    /// overwrites the margin in place.
    pub(super) fn arm_wrap_latch(&mut self) {
        let (_, right_column_index) = self.get_horizontal_margin_bounds();
        let should_arm_wrap_latch = self.modes.autowrap;
        let cursor = self.active_cursor_mut();
        cursor.column = right_column_index;
        cursor.pending_wrap = should_arm_wrap_latch;
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
        let cursor_column_index = self.active_cursor().column;
        if let Some(tab_stop) = self.tab_stops.get_mut(cursor_column_index as usize) {
            *tab_stop = true;
        }
    }

    /// Clear the horizontal tab stop at the active cursor column. A column past
    /// the tab-stop table is a no-op.
    pub(super) fn clear_tab_stop(&mut self) {
        let cursor_column_index = self.active_cursor().column;
        if let Some(tab_stop) = self.tab_stops.get_mut(cursor_column_index as usize) {
            *tab_stop = false;
        }
    }

    /// Clear every horizontal tab stop.
    pub(super) fn clear_all_tab_stops(&mut self) {
        self.tab_stops.fill(false);
    }
}

/// The first tab stop strictly after `column_index`, or `last_column_index`
/// when there is none or `column_index >= last_column_index`. A column past the
/// end of `tab_stops` holds no stop.
pub(super) fn find_next_tab_stop(
    tab_stops: &[bool],
    column_index: u16,
    last_column_index: u16,
) -> u16 {
    if column_index >= last_column_index {
        return last_column_index;
    }
    (column_index + 1..=last_column_index)
        .find(|&tab_stop_column_index| {
            tab_stops
                .get(tab_stop_column_index as usize)
                .copied()
                .unwrap_or(false)
        })
        .unwrap_or(last_column_index)
}

/// The first tab stop strictly before `column_index`, or column `0` when there
/// is none.
/// A column past the end of `tab_stops` holds no stop.
pub(super) fn find_previous_tab_stop(tab_stops: &[bool], column_index: u16) -> u16 {
    (0..column_index)
        .rev()
        .find(|&previous_column_index| {
            tab_stops
                .get(previous_column_index as usize)
                .copied()
                .unwrap_or(false)
        })
        .unwrap_or(0)
}
