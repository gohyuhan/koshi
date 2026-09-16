//! Image movement and clipping when terminal rows scroll.

use super::*;

impl TerminalState {
    pub(in crate::state) fn scroll_image_rows_in_columns(
        &mut self,
        first_row_index: u16,
        bottom_row_index: u16,
        row_shift_count: u16,
        is_scrolling_up: bool,
        horizontal_margin_bounds: (u16, u16),
        old_live_top_row_index: u64,
    ) {
        let (left_column_index, right_column_index) = horizontal_margin_bounds;
        let is_primary_screen = self.active_screen == Screen::Primary;
        let new_live_top_row_index = if is_primary_screen {
            self.scrollback.get_total_pushed_line_count()
        } else {
            0
        };
        let old_live_top_row_index = if is_primary_screen {
            old_live_top_row_index
        } else {
            0
        };
        let (grid_row_count, grid_column_count) = self.get_active_grid().get_grid_dimensions();
        let is_full_width =
            left_column_index == 0 && right_column_index == grid_column_count.saturating_sub(1);
        let is_full_history_scroll = is_primary_screen
            && is_full_width
            && is_scrolling_up
            && first_row_index == 0
            && bottom_row_index + 1 == self.primary.get_grid_dimensions().0;
        let absolute_image_placements = if is_primary_screen {
            self.list_primary_absolute_image_placements_at(old_live_top_row_index)
        } else {
            std::mem::take(&mut self.alternate_image_placements)
                .into_iter()
                .filter_map(|image_placement| {
                    AbsoluteImagePlacement::from_live_image_placement(image_placement, 0)
                })
                .collect()
        };
        let mut mapped_absolute_image_placements = absolute_image_placements
            .into_iter()
            .filter_map(|mut image_placement| {
                if image_placement
                    .image_record
                    .display
                    .relative_image_id
                    .is_some()
                {
                    image_placement.anchor.0 = new_live_top_row_index;
                    return Some(image_placement);
                }
                if is_full_history_scroll || image_placement.anchor.0 < old_live_top_row_index {
                    return Some(image_placement);
                }
                let relative_row_offset = image_placement.anchor.0 - old_live_top_row_index;
                let relative_end_row_offset =
                    relative_row_offset + u64::from(image_placement.row_count);
                let is_image_fully_contained_vertically = relative_row_offset
                    >= u64::from(first_row_index)
                    && relative_end_row_offset <= u64::from(bottom_row_index) + 1;
                let image_column_end =
                    u32::from(image_placement.anchor.1) + u32::from(image_placement.column_count);
                let is_image_fully_contained_horizontally = image_placement.anchor.1
                    >= left_column_index
                    && image_column_end <= u32::from(right_column_index) + 1;
                if !is_image_fully_contained_vertically || !is_image_fully_contained_horizontally {
                    image_placement.anchor.0 =
                        new_live_top_row_index.checked_add(relative_row_offset)?;
                    return Some(image_placement);
                }
                if is_scrolling_up {
                    let removed_row_count = u64::from(row_shift_count)
                        .saturating_sub(relative_row_offset - u64::from(first_row_index))
                        .min(u64::from(image_placement.row_count));
                    if removed_row_count == u64::from(image_placement.row_count) {
                        return None;
                    }
                    image_placement.plan.geometry.cell_offset.row = image_placement
                        .plan
                        .geometry
                        .cell_offset
                        .row
                        .checked_add(u16::try_from(removed_row_count).ok()?)?;
                    image_placement.row_count -= u16::try_from(removed_row_count).ok()?;
                    image_placement.anchor.0 = new_live_top_row_index.checked_add(
                        (relative_row_offset + removed_row_count)
                            .checked_sub(u64::from(row_shift_count))?,
                    )?;
                } else {
                    image_placement.anchor.0 = new_live_top_row_index
                        .checked_add(relative_row_offset + u64::from(row_shift_count))?;
                    image_placement = image_placement.clip_to_visible_area(
                        new_live_top_row_index + u64::from(first_row_index),
                        new_live_top_row_index + u64::from(bottom_row_index) + 1,
                        grid_column_count,
                    )?;
                }
                Some(image_placement)
            })
            .collect::<Vec<_>>();
        if is_primary_screen {
            self.set_primary_absolute_image_placements(&mut mapped_absolute_image_placements);
        } else {
            self.alternate_image_placements = mapped_absolute_image_placements
                .into_iter()
                .filter_map(|image_placement| {
                    if image_placement
                        .image_record
                        .display
                        .relative_image_id
                        .is_some()
                    {
                        image_placement.into_live_image_placement(0)
                    } else {
                        image_placement
                            .clip_to_visible_area(0, u64::from(grid_row_count), grid_column_count)?
                            .into_live_image_placement(0)
                    }
                })
                .collect();
        }
    }
}

#[cfg(test)]
mod tests;
