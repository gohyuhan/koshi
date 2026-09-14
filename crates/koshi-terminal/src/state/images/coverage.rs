//! Native image sources and the image portions carried by grid cells.

use super::*;
use crate::grid::state::{Cell, ImageCellFragment};

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static REBUILD_CELL_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One screen's canonical native image and its complete pixel transform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeImageSource {
    pub(crate) screen: Screen,
    pub(crate) placement: ImagePlacement,
}

impl TerminalState {
    pub(super) fn install_native_image_placement(
        &mut self,
        mut placement: ImagePlacement,
        is_sixel_scrolling: bool,
    ) -> Result<(), ImagePlacementError> {
        let is_iterm_protocol = placement.image_record.protocol == GraphicsProtocol::Iterm2;
        let is_overlay_protocol = placement.image_record.protocol == GraphicsProtocol::Sixel;
        let sixel_scroll_region = if is_sixel_scrolling {
            let (grid_row_count, _) = self.get_active_grid().get_grid_dimensions();
            let (scroll_region_top_row_index, scroll_region_bottom_row_index) = self
                .get_scroll_region()
                .unwrap_or((0, grid_row_count.saturating_sub(1)));
            (scroll_region_top_row_index <= scroll_region_bottom_row_index
                && placement.anchor.0 >= scroll_region_top_row_index
                && placement.anchor.0 <= scroll_region_bottom_row_index)
                .then_some((scroll_region_top_row_index, scroll_region_bottom_row_index))
        } else {
            None
        };
        let mut current_sixel_row_index = placement.anchor.0;
        if is_iterm_protocol {
            let cursor = self.active_cursor_mut();
            cursor.row = placement.image_record.anchor.0;
            cursor.column = placement.image_record.anchor.1;
            cursor.pending_wrap = false;
        }
        for row_index in 0..placement.row_count {
            if is_iterm_protocol && row_index > 0 {
                self.handle_image_linefeed();
            }
            if row_index > 0 {
                if let Some((scroll_region_top_row_index, scroll_region_bottom_row_index)) =
                    sixel_scroll_region
                {
                    if current_sixel_row_index == scroll_region_bottom_row_index {
                        let background_fill_style =
                            self.active_render().style.get_background_fill_style();
                        self.delete_lines_into_scrollback(
                            scroll_region_top_row_index,
                            scroll_region_bottom_row_index,
                            1,
                            background_fill_style,
                        );
                    } else {
                        current_sixel_row_index = current_sixel_row_index
                            .saturating_add(1)
                            .min(scroll_region_bottom_row_index);
                    }
                }
            }
            let target_row_index = if is_iterm_protocol {
                self.get_active_cursor_position().0
            } else if sixel_scroll_region.is_some() {
                current_sixel_row_index
            } else {
                let Some(target_row_index) = placement.anchor.0.checked_add(row_index) else {
                    break;
                };
                target_row_index
            };
            let background_fill_style = self.active_render().style.get_background_fill_style();
            let (active_grid, fragment_count_by_image_source_id) =
                self.get_active_grid_and_fragment_count_by_image_source_id();
            if is_iterm_protocol {
                clear_wide_fragments(
                    active_grid,
                    target_row_index,
                    placement.anchor.1,
                    fragment_count_by_image_source_id,
                );
                clear_wide_fragments(
                    active_grid,
                    target_row_index,
                    placement.anchor.1 + placement.column_count - 1,
                    fragment_count_by_image_source_id,
                );
                active_grid.clear_wide_glyph_at(
                    target_row_index,
                    placement.anchor.1,
                    background_fill_style,
                );
                active_grid.clear_wide_glyph_at(
                    target_row_index,
                    placement.anchor.1 + placement.column_count - 1,
                    background_fill_style,
                );
            }
            for column_index in 0..placement.column_count {
                let image_fragment = ImageCellFragment {
                    image_source_id: placement.image_placement_id,
                    source_row_index: placement.plan.geometry.cell_offset.row + row_index,
                    source_column_index: placement.plan.geometry.cell_offset.column + column_index,
                };
                if let Some(terminal_cell) =
                    active_grid.get_cell_mut(target_row_index, placement.anchor.1 + column_index)
                {
                    set_native_fragment(
                        terminal_cell,
                        image_fragment,
                        is_overlay_protocol,
                        fragment_count_by_image_source_id,
                    );
                }
            }
        }
        if is_iterm_protocol {
            let column_count = self.get_active_grid().get_grid_dimensions().1;
            let end_column = u32::from(placement.anchor.1) + u32::from(placement.column_count);
            let cursor = self.active_cursor_mut();
            cursor.column = end_column.min(u32::from(column_count.saturating_sub(1))) as u16;
            cursor.pending_wrap = end_column >= u32::from(column_count);
        }
        placement.anchor = (0, 0);
        self.native_images.push(NativeImageSource {
            screen: self.active_screen,
            placement,
        });
        self.retain_referenced_native_sources();
        Ok(())
    }

    fn handle_image_linefeed(&mut self) {
        let (row_count, _) = self.get_active_grid().get_grid_dimensions();
        let (scroll_region_top_row_index, scroll_region_bottom_row_index) = self
            .get_scroll_region()
            .unwrap_or((0, row_count.saturating_sub(1)));
        if self.get_active_cursor_position().0 == scroll_region_bottom_row_index {
            let background_fill_style = self.active_render().style.get_background_fill_style();
            self.delete_lines_into_scrollback(
                scroll_region_top_row_index,
                scroll_region_bottom_row_index,
                1,
                background_fill_style,
            );
        } else {
            let cursor = self.active_cursor_mut();
            cursor.row = cursor
                .row
                .saturating_add(1)
                .min(row_count.saturating_sub(1));
        }
    }

    pub(in crate::state) fn rebuild_native_fragment_count_by_image_source_id(&mut self) {
        let mut fragment_count_by_image_source_id = HashMap::new();
        for terminal_cell in self.list_native_cells() {
            #[cfg(test)]
            REBUILD_CELL_VISITS.with(|visits| visits.set(visits.get() + 1));
            for image_fragment in terminal_cell.image_fragments() {
                *fragment_count_by_image_source_id
                    .entry(image_fragment.image_source_id)
                    .or_insert(0) += 1;
            }
        }
        self.native_fragment_count_by_image_source_id = fragment_count_by_image_source_id;
        self.retain_referenced_native_sources();
    }

    pub(in crate::state) fn clear_image_fragments_at_cells(
        &mut self,
        row_index: u16,
        first_column_index: u16,
        last_column_index: u16,
    ) -> bool {
        if self.native_fragment_count_by_image_source_id.is_empty()
            || first_column_index >= last_column_index
        {
            return false;
        }
        let active_grid = match self.active_screen {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        };
        let mut has_removed_native_image_source = false;
        for column_index in first_column_index..last_column_index {
            let Some(terminal_cell) = active_grid.get_cell_mut(row_index, column_index) else {
                break;
            };
            has_removed_native_image_source |= clear_native_fragments(
                terminal_cell,
                &mut self.native_fragment_count_by_image_source_id,
            );
        }
        has_removed_native_image_source
    }

    pub(in crate::state) fn discard_active_image_fragments(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        first_column_index: u16,
        last_column_index: u16,
    ) -> bool {
        if self.native_fragment_count_by_image_source_id.is_empty()
            || first_row_index >= last_row_index
            || first_column_index >= last_column_index
        {
            return false;
        }
        let active_grid = match self.active_screen {
            Screen::Primary => &self.primary,
            Screen::Alternate => &self.alternate,
        };
        let row_end_index = usize::from(last_row_index).min(active_grid.list_rows().len());
        let Some(grid_rows) = active_grid
            .list_rows()
            .get(usize::from(first_row_index)..row_end_index)
        else {
            return false;
        };
        let terminal_cells = grid_rows.iter().flat_map(|grid_row_cells| {
            let row_end_column = usize::from(last_column_index).min(grid_row_cells.len());
            grid_row_cells
                .get(usize::from(first_column_index)..row_end_column)
                .unwrap_or_default()
                .iter()
        });
        discard_native_fragment_references(
            &mut self.native_fragment_count_by_image_source_id,
            terminal_cells,
        )
    }

    pub(in crate::state) fn finish_native_fragment_removal(
        &mut self,
        has_removed_native_image_source: bool,
    ) {
        if has_removed_native_image_source {
            self.retain_referenced_native_sources();
        }
    }

    pub(in crate::state) fn clear_scrollback_with_images(&mut self) {
        if self.native_fragment_count_by_image_source_id.is_empty() {
            self.scrollback.clear_scrollback();
            return;
        }
        let scrollback_lines = self.scrollback.take_retained_lines();
        let has_removed_native_image_source = discard_native_fragment_references(
            &mut self.native_fragment_count_by_image_source_id,
            scrollback_lines.iter().flat_map(|(row_cells, _)| row_cells),
        );
        self.finish_native_fragment_removal(has_removed_native_image_source);
    }

    fn get_active_grid_and_fragment_count_by_image_source_id(
        &mut self,
    ) -> (&mut Grid, &mut HashMap<u64, usize>) {
        let active_grid = match self.active_screen {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        };
        (
            active_grid,
            &mut self.native_fragment_count_by_image_source_id,
        )
    }

    pub(super) fn retain_referenced_native_sources(&mut self) {
        self.native_fragment_count_by_image_source_id
            .retain(|_, fragment_count| *fragment_count != 0);
        self.native_images.retain(|native_image_source| {
            self.native_fragment_count_by_image_source_id
                .contains_key(&native_image_source.placement.image_placement_id)
        });
    }

    fn list_native_cells(&self) -> impl Iterator<Item = &Cell> {
        self.primary
            .list_rows()
            .iter()
            .chain(self.alternate.list_rows())
            .flatten()
            .chain(
                self.scrollback
                    .list_retained_lines()
                    .iter()
                    .flat_map(|(cells, _)| cells),
            )
    }

    pub(super) fn get_native_fragment_storage_byte_count(&self) -> usize {
        self.list_native_cells()
            .map(Cell::image_fragment_storage_bytes)
            .sum()
    }

    pub(super) fn list_native_image_placements(
        &self,
        terminal_grid: &Grid,
        view_top_row_index: u64,
    ) -> Vec<AbsoluteImagePlacement> {
        let native_image_source_by_image_placement_id = self
            .native_images
            .iter()
            .filter(|native_image_source| native_image_source.screen == self.active_screen)
            .map(|native_image_source| {
                (
                    native_image_source.placement.image_placement_id,
                    &native_image_source.placement,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut image_placement_runs_by_image_source_id: HashMap<u64, Vec<ImagePlacement>> =
            HashMap::new();
        for (row_index, grid_row_cells) in terminal_grid.list_rows().iter().enumerate() {
            for (column_index, terminal_cell) in grid_row_cells.iter().enumerate() {
                for image_fragment in terminal_cell.image_fragments() {
                    let Some(native_image_source) = native_image_source_by_image_placement_id
                        .get(&image_fragment.image_source_id)
                    else {
                        continue;
                    };
                    let image_placements = image_placement_runs_by_image_source_id
                        .entry(image_fragment.image_source_id)
                        .or_default();
                    append_image_placement_cell(
                        image_placements,
                        native_image_source,
                        row_index as u16,
                        column_index as u16,
                        image_fragment.source_row_index,
                        image_fragment.source_column_index,
                    );
                }
            }
        }
        self.native_images
            .iter()
            .filter(|native_image_source| native_image_source.screen == self.active_screen)
            .flat_map(|native_image_source| {
                merge_image_placement_rows(
                    image_placement_runs_by_image_source_id
                        .remove(&native_image_source.placement.image_placement_id)
                        .unwrap_or_default(),
                )
            })
            .filter_map(|placement| {
                AbsoluteImagePlacement::from_live_image_placement(placement, view_top_row_index)
            })
            .collect()
    }

    pub(crate) fn restore_native_image_coverage(
        &mut self,
        has_cell_coverage: bool,
    ) -> Result<(), String> {
        if !has_cell_coverage
            && self
                .list_native_cells()
                .any(|terminal_cell| !terminal_cell.image_fragments().is_empty())
        {
            return Err("native image fragments require the cell coverage format".to_owned());
        }
        for screen in [Screen::Primary, Screen::Alternate] {
            let screen_image_placements = match screen {
                Screen::Primary => &mut self.primary_image_placements,
                Screen::Alternate => &mut self.alternate_image_placements,
            };
            let mut physical_image_placements = Vec::new();
            for mut image_placement in std::mem::take(screen_image_placements) {
                if image_placement.image_record.protocol == GraphicsProtocol::Kitty {
                    physical_image_placements.push(image_placement);
                    continue;
                }
                if !has_cell_coverage {
                    let active_grid = match screen {
                        Screen::Primary => Arc::make_mut(&mut self.primary),
                        Screen::Alternate => Arc::make_mut(&mut self.alternate),
                    };
                    attach_legacy_grid(active_grid, &image_placement);
                }
                image_placement.anchor = (0, 0);
                self.native_images.push(NativeImageSource {
                    screen,
                    placement: image_placement,
                });
            }
            match screen {
                Screen::Primary => self.primary_image_placements = physical_image_placements,
                Screen::Alternate => self.alternate_image_placements = physical_image_placements,
            }
        }
        let live_top_row = self.scrollback.get_total_pushed_line_count();
        let retained_history_start_row =
            live_top_row.saturating_sub(self.scrollback.get_retained_line_count() as u64);
        let mut scrollback_lines = self.scrollback.take_retained_lines();
        let mut physical_history_placements = Vec::new();
        for history_image_placement in std::mem::take(&mut self.primary_image_history) {
            if history_image_placement.image_record.protocol == GraphicsProtocol::Kitty {
                physical_history_placements.push(history_image_placement);
                continue;
            }
            if has_cell_coverage {
                return Err(
                    "native image sources cannot be stored as history rectangles".to_owned(),
                );
            }
            let mut image_placement = ImagePlacement::from_image_record(
                history_image_placement.image_placement_id,
                history_image_placement.image_record,
                history_image_placement.image_content,
                history_image_placement.plan,
                history_image_placement.column_count,
                history_image_placement.row_count,
                history_image_placement.raster,
            );
            for row_index in 0..image_placement.row_count {
                let absolute_row = history_image_placement
                    .anchor
                    .0
                    .checked_add(u64::from(row_index))
                    .ok_or("native history row overflows")?;
                for column_index in 0..image_placement.column_count {
                    let target_column_index =
                        usize::from(history_image_placement.anchor.1 + column_index);
                    let image_fragment = ImageCellFragment {
                        image_source_id: image_placement.image_placement_id,
                        source_row_index: image_placement.plan.geometry.cell_offset.row + row_index,
                        source_column_index: image_placement.plan.geometry.cell_offset.column
                            + column_index,
                    };
                    let is_overlay_protocol =
                        image_placement.image_record.protocol == GraphicsProtocol::Sixel;
                    if absolute_row < live_top_row {
                        if let Some((row_cells, _)) = scrollback_lines.get_mut(
                            usize::try_from(
                                absolute_row
                                    .checked_sub(retained_history_start_row)
                                    .ok_or("native history precedes retained rows")?,
                            )
                            .map_err(|_| "native history index exceeds address space")?,
                        ) {
                            row_cells.resize_with(
                                row_cells.len().max(target_column_index + 1),
                                Cell::blank,
                            );
                            row_cells[target_column_index]
                                .set_image_fragment(image_fragment, is_overlay_protocol);
                        }
                    } else if let Some(terminal_cell) = Arc::make_mut(&mut self.primary)
                        .get_cell_mut(
                            u16::try_from(absolute_row - live_top_row)
                                .map_err(|_| "native history row exceeds grid range")?,
                            u16::try_from(target_column_index)
                                .map_err(|_| "native history column exceeds grid range")?,
                        )
                    {
                        terminal_cell.set_image_fragment(image_fragment, is_overlay_protocol);
                    }
                }
            }
            image_placement.anchor = (0, 0);
            self.native_images.push(NativeImageSource {
                screen: Screen::Primary,
                placement: image_placement,
            });
        }
        self.primary_image_history = physical_history_placements;
        let retained_row_count = scrollback_lines.len() as u64;
        self.scrollback
            .replace_retained_lines(scrollback_lines.into_iter().collect(), retained_row_count);
        self.native_images
            .sort_by_key(|native_image_source| native_image_source.placement.image_placement_id);
        self.validate_native_fragments()?;
        self.rebuild_native_fragment_count_by_image_source_id();
        if self.get_image_storage_byte_count() > MAX_IMAGE_STORAGE_BYTE_COUNT {
            return Err(
                "image pixels and fragment metadata exceed the image storage limit".to_owned(),
            );
        }
        self.retain_referenced_native_sources();
        Ok(())
    }

    fn validate_native_fragments(&self) -> Result<(), String> {
        let mut native_image_source_by_image_placement_id = HashMap::new();
        for native_image_source in &self.native_images {
            if native_image_source_by_image_placement_id
                .insert(
                    native_image_source.placement.image_placement_id,
                    native_image_source,
                )
                .is_some()
            {
                return Err("native image source identities must be unique".to_owned());
            }
        }
        let mut fragment_storage_byte_count = 0usize;
        for (screen, row_cells) in self
            .primary
            .list_rows()
            .iter()
            .map(|row| (Screen::Primary, row.as_slice()))
            .chain(
                self.alternate
                    .list_rows()
                    .iter()
                    .map(|row| (Screen::Alternate, row.as_slice())),
            )
            .chain(
                self.scrollback
                    .list_retained_lines()
                    .iter()
                    .map(|(row, _)| (Screen::Primary, row.as_slice())),
            )
        {
            for terminal_cell in row_cells {
                fragment_storage_byte_count = fragment_storage_byte_count
                    .saturating_add(terminal_cell.image_fragment_storage_bytes());
                if fragment_storage_byte_count > MAX_IMAGE_STORAGE_BYTE_COUNT {
                    return Err("native image fragment storage exceeds its limit".to_owned());
                }
                let mut seen_image_placement_ids = HashSet::new();
                for image_fragment in terminal_cell.image_fragments() {
                    let native_image_source = native_image_source_by_image_placement_id
                        .get(&image_fragment.image_source_id)
                        .ok_or("native image fragment has no source")?;
                    let image_size = native_image_source.placement.plan.geometry.full_size;
                    if native_image_source.screen != screen
                        || image_fragment.source_row_index >= image_size.row_count
                        || image_fragment.source_column_index >= image_size.column_count
                        || !seen_image_placement_ids.insert(image_fragment.image_source_id)
                    {
                        return Err(
                            "native image fragment has invalid screen, coordinates, or identity"
                                .to_owned(),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

fn set_native_fragment(
    terminal_cell: &mut Cell,
    image_fragment: ImageCellFragment,
    is_overlay_protocol: bool,
    fragment_count_by_image_source_id: &mut HashMap<u64, usize>,
) {
    let is_existing_fragment_replaced = is_overlay_protocol
        && terminal_cell
            .image_fragments()
            .iter()
            .any(|existing_fragment| {
                existing_fragment.image_source_id == image_fragment.image_source_id
            });
    if !is_overlay_protocol {
        for existing_fragment in terminal_cell.image_fragments() {
            let _ = decrement_native_fragment_count(
                fragment_count_by_image_source_id,
                existing_fragment.image_source_id,
            );
        }
    }
    terminal_cell.set_image_fragment(image_fragment, is_overlay_protocol);
    if !is_existing_fragment_replaced {
        *fragment_count_by_image_source_id
            .entry(image_fragment.image_source_id)
            .or_insert(0) += 1;
    }
}

fn decrement_native_fragment_count(
    fragment_count_by_image_source_id: &mut HashMap<u64, usize>,
    image_placement_id: u64,
) -> bool {
    if let Some(fragment_count) = fragment_count_by_image_source_id.get_mut(&image_placement_id) {
        *fragment_count -= 1;
        if *fragment_count == 0 {
            fragment_count_by_image_source_id.remove(&image_placement_id);
            return true;
        }
    }
    false
}

pub(super) fn clear_native_fragments(
    terminal_cell: &mut Cell,
    fragment_count_by_image_source_id: &mut HashMap<u64, usize>,
) -> bool {
    let mut has_removed_native_image_source = false;
    for image_fragment in terminal_cell.image_fragments() {
        has_removed_native_image_source |= decrement_native_fragment_count(
            fragment_count_by_image_source_id,
            image_fragment.image_source_id,
        );
    }
    terminal_cell.clear_image_fragments();
    has_removed_native_image_source
}

pub(in crate::state) fn discard_native_fragment_references<'a>(
    fragment_count_by_image_source_id: &mut HashMap<u64, usize>,
    terminal_cells: impl Iterator<Item = &'a Cell>,
) -> bool {
    let mut has_removed_native_image_source = false;
    for terminal_cell in terminal_cells {
        for image_fragment in terminal_cell.image_fragments() {
            has_removed_native_image_source |= decrement_native_fragment_count(
                fragment_count_by_image_source_id,
                image_fragment.image_source_id,
            );
        }
    }
    has_removed_native_image_source
}

fn clear_wide_fragments(
    terminal_grid: &mut Grid,
    row_index: u16,
    column_index: u16,
    fragment_count_by_image_source_id: &mut HashMap<u64, usize>,
) {
    let wide_cell_column_index = match terminal_grid
        .get_cell(row_index, column_index)
        .map_or(1, Cell::get_display_width)
    {
        2 => column_index.checked_add(1),
        0 => column_index.checked_sub(1),
        _ => None,
    };
    if let Some(wide_cell_column_index) = wide_cell_column_index {
        if let Some(terminal_cell) = terminal_grid.get_cell_mut(row_index, wide_cell_column_index) {
            let _ = clear_native_fragments(terminal_cell, fragment_count_by_image_source_id);
        }
    }
}

fn attach_legacy_grid(terminal_grid: &mut Grid, image_placement: &ImagePlacement) {
    for row_index in 0..image_placement.row_count {
        for column_index in 0..image_placement.column_count {
            if let Some(terminal_cell) = terminal_grid.get_cell_mut(
                image_placement.anchor.0 + row_index,
                image_placement.anchor.1 + column_index,
            ) {
                terminal_cell.set_image_fragment(
                    ImageCellFragment {
                        image_source_id: image_placement.image_placement_id,
                        source_row_index: image_placement.plan.geometry.cell_offset.row + row_index,
                        source_column_index: image_placement.plan.geometry.cell_offset.column
                            + column_index,
                    },
                    image_placement.image_record.protocol == GraphicsProtocol::Sixel,
                );
            }
        }
    }
}

/// Extend a horizontal source run or start one cell's visible image portion.
pub(super) fn append_image_placement_cell(
    image_placements: &mut Vec<ImagePlacement>,
    source_placement: &ImagePlacement,
    row_index: u16,
    column_index: u16,
    source_row_index: u16,
    source_column_index: u16,
) {
    if let Some(last_image_placement) = image_placements.last_mut() {
        if last_image_placement.anchor.0 == row_index
            && last_image_placement
                .anchor
                .1
                .checked_add(last_image_placement.column_count)
                == Some(column_index)
            && last_image_placement.plan.geometry.cell_offset.row == source_row_index
            && last_image_placement
                .plan
                .geometry
                .cell_offset
                .column
                .checked_add(last_image_placement.column_count)
                == Some(source_column_index)
        {
            last_image_placement.column_count += 1;
            return;
        }
    }
    let mut image_placement = source_placement.with_image_anchor((row_index, column_index));
    image_placement.row_count = 1;
    image_placement.column_count = 1;
    image_placement.plan.geometry.cell_offset = Point {
        column: source_column_index,
        row: source_row_index,
    };
    image_placements.push(image_placement);
}

/// Merge consecutive rows with identical target and source column ranges.
pub(super) fn merge_image_placement_rows(
    image_placement_runs: Vec<ImagePlacement>,
) -> Vec<ImagePlacement> {
    let mut merged_image_placements: Vec<ImagePlacement> = Vec::new();
    let mut placement_index_by_merge_key = HashMap::new();
    for image_placement in image_placement_runs {
        let merge_key = (
            image_placement.anchor.1,
            image_placement.column_count,
            image_placement.plan.geometry.cell_offset.column,
            i32::from(image_placement.plan.geometry.cell_offset.row)
                - i32::from(image_placement.anchor.0),
        );
        if let Some(&existing_image_placement_index) = placement_index_by_merge_key.get(&merge_key)
        {
            let previous_image_placement: &mut ImagePlacement =
                &mut merged_image_placements[existing_image_placement_index];
            if previous_image_placement
                .anchor
                .0
                .checked_add(previous_image_placement.row_count)
                == Some(image_placement.anchor.0)
            {
                previous_image_placement.row_count += 1;
                continue;
            }
        }
        placement_index_by_merge_key.insert(merge_key, merged_image_placements.len());
        merged_image_placements.push(image_placement);
    }
    merged_image_placements
}

/// Assign distinct frame-local identities while preserving source paint order.
pub(super) fn append_derived_image_placements(
    image_placements: &mut Vec<ImagePlacement>,
    derived_image_placements: impl IntoIterator<Item = ImagePlacement>,
) {
    let mut used_image_placement_ids = image_placements
        .iter()
        .map(|image_placement| image_placement.image_placement_id)
        .collect::<HashSet<_>>();
    let mut candidate_image_placement_id = 1u64;
    for mut image_placement in derived_image_placements {
        while used_image_placement_ids.contains(&candidate_image_placement_id) {
            candidate_image_placement_id = candidate_image_placement_id
                .checked_add(1)
                .expect("a bounded grid cannot occupy the u64 identity space");
        }
        image_placement.image_placement_id = candidate_image_placement_id;
        used_image_placement_ids.insert(candidate_image_placement_id);
        image_placements.push(image_placement);
    }
}
