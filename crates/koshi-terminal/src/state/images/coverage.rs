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
    pub(super) fn install_native_image(
        &mut self,
        mut placement: ImagePlacement,
        sixel_scrolling: bool,
    ) -> Result<(), ImagePlacementError> {
        let iterm = placement.record.protocol == GraphicsProtocol::Iterm2;
        let overlay = placement.record.protocol == GraphicsProtocol::Sixel;
        let sixel_region = if sixel_scrolling {
            let (grid_rows, _) = self.active_grid().dimensions();
            let (top, bottom) = self
                .scroll_region()
                .unwrap_or((0, grid_rows.saturating_sub(1)));
            (top <= bottom && placement.anchor.0 >= top && placement.anchor.0 <= bottom)
                .then_some((top, bottom))
        } else {
            None
        };
        let mut sixel_row = placement.anchor.0;
        if iterm {
            let cursor = self.active_cursor_mut();
            cursor.row = placement.record.anchor.0;
            cursor.col = placement.record.anchor.1;
            cursor.pending_wrap = false;
        }
        for row in 0..placement.rows {
            if iterm && row > 0 {
                self.image_linefeed();
            }
            if row > 0 {
                if let Some((top, bottom)) = sixel_region {
                    if sixel_row == bottom {
                        let fill = self.active_render().style.bg_fill();
                        self.delete_lines_into_scrollback(top, bottom, 1, fill);
                    } else {
                        sixel_row = sixel_row.saturating_add(1).min(bottom);
                    }
                }
            }
            let target_row = if iterm {
                self.active_cursor_position().0
            } else if sixel_region.is_some() {
                sixel_row
            } else {
                let Some(target_row) = placement.anchor.0.checked_add(row) else {
                    break;
                };
                target_row
            };
            let fill = self.active_render().style.bg_fill();
            let (grid, counts) = self.active_grid_and_fragment_counts();
            if iterm {
                clear_wide_fragments(grid, target_row, placement.anchor.1, counts);
                clear_wide_fragments(
                    grid,
                    target_row,
                    placement.anchor.1 + placement.columns - 1,
                    counts,
                );
                grid.clear_wide_at(target_row, placement.anchor.1, fill);
                grid.clear_wide_at(target_row, placement.anchor.1 + placement.columns - 1, fill);
            }
            for column in 0..placement.columns {
                let fragment = ImageCellFragment {
                    source: placement.id,
                    row: placement.plan.geometry.offset.y + row,
                    column: placement.plan.geometry.offset.x + column,
                };
                if let Some(cell) = grid.cell_mut(target_row, placement.anchor.1 + column) {
                    set_native_fragment(cell, fragment, overlay, counts);
                }
            }
        }
        if iterm {
            let columns = self.active_grid().dimensions().1;
            let end = u32::from(placement.anchor.1) + u32::from(placement.columns);
            let cursor = self.active_cursor_mut();
            cursor.col = end.min(u32::from(columns.saturating_sub(1))) as u16;
            cursor.pending_wrap = end >= u32::from(columns);
        }
        placement.anchor = (0, 0);
        self.native_images.push(NativeImageSource {
            screen: self.active,
            placement,
        });
        self.retain_referenced_native_sources();
        Ok(())
    }

    fn image_linefeed(&mut self) {
        let (rows, _) = self.active_grid().dimensions();
        let (top, bottom) = self.scroll_region().unwrap_or((0, rows.saturating_sub(1)));
        if self.active_cursor_position().0 == bottom {
            let fill = self.active_render().style.bg_fill();
            self.delete_lines_into_scrollback(top, bottom, 1, fill);
        } else {
            let cursor = self.active_cursor_mut();
            cursor.row = cursor.row.saturating_add(1).min(rows.saturating_sub(1));
        }
    }

    pub(in crate::state) fn rebuild_native_fragment_counts(&mut self) {
        let mut counts = HashMap::new();
        for cell in self.native_cells() {
            #[cfg(test)]
            REBUILD_CELL_VISITS.with(|visits| visits.set(visits.get() + 1));
            for fragment in cell.image_fragments() {
                *counts.entry(fragment.source).or_insert(0) += 1;
            }
        }
        self.native_fragment_counts = counts;
        self.retain_referenced_native_sources();
    }

    pub(in crate::state) fn clear_image_fragments_at_cells(
        &mut self,
        row: u16,
        from: u16,
        to: u16,
    ) -> bool {
        if self.native_fragment_counts.is_empty() || from >= to {
            return false;
        }
        let grid = match self.active {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        };
        let mut source_removed = false;
        for column in from..to {
            let Some(cell) = grid.cell_mut(row, column) else {
                break;
            };
            source_removed |= clear_native_fragments(cell, &mut self.native_fragment_counts);
        }
        source_removed
    }

    pub(in crate::state) fn discard_active_image_fragments(
        &mut self,
        first_row: u16,
        last_row: u16,
        first_column: u16,
        last_column: u16,
    ) -> bool {
        if self.native_fragment_counts.is_empty()
            || first_row >= last_row
            || first_column >= last_column
        {
            return false;
        }
        let grid = match self.active {
            Screen::Primary => &self.primary,
            Screen::Alternate => &self.alternate,
        };
        let rows_end = usize::from(last_row).min(grid.rows().len());
        let Some(rows) = grid.rows().get(usize::from(first_row)..rows_end) else {
            return false;
        };
        let cells = rows.iter().flat_map(|row| {
            let end = usize::from(last_column).min(row.len());
            row.get(usize::from(first_column)..end)
                .unwrap_or_default()
                .iter()
        });
        discard_native_fragment_references(&mut self.native_fragment_counts, cells)
    }

    pub(in crate::state) fn finish_native_fragment_removal(&mut self, source_removed: bool) {
        if source_removed {
            self.retain_referenced_native_sources();
        }
    }

    pub(in crate::state) fn clear_scrollback_with_images(&mut self) {
        if self.native_fragment_counts.is_empty() {
            self.scrollback.clear();
            return;
        }
        let lines = self.scrollback.take_lines();
        let source_removed = discard_native_fragment_references(
            &mut self.native_fragment_counts,
            lines.iter().flat_map(|(cells, _)| cells),
        );
        self.finish_native_fragment_removal(source_removed);
    }

    fn active_grid_and_fragment_counts(&mut self) -> (&mut Grid, &mut HashMap<u64, usize>) {
        let grid = match self.active {
            Screen::Primary => Arc::make_mut(&mut self.primary),
            Screen::Alternate => Arc::make_mut(&mut self.alternate),
        };
        (grid, &mut self.native_fragment_counts)
    }

    pub(super) fn retain_referenced_native_sources(&mut self) {
        self.native_fragment_counts.retain(|_, count| *count != 0);
        self.native_images.retain(|source| {
            self.native_fragment_counts
                .contains_key(&source.placement.id)
        });
    }

    fn native_cells(&self) -> impl Iterator<Item = &Cell> {
        self.primary
            .rows()
            .iter()
            .chain(self.alternate.rows())
            .flatten()
            .chain(self.scrollback.lines().iter().flat_map(|(cells, _)| cells))
    }

    pub(super) fn native_fragment_storage_bytes(&self) -> usize {
        self.native_cells()
            .map(Cell::image_fragment_storage_bytes)
            .sum()
    }

    pub(super) fn native_image_placements(
        &self,
        grid: &Grid,
        row_origin: u64,
    ) -> Vec<AbsoluteImagePlacement> {
        let sources = self
            .native_images
            .iter()
            .filter(|source| source.screen == self.active)
            .map(|source| (source.placement.id, &source.placement))
            .collect::<HashMap<_, _>>();
        let mut runs: HashMap<u64, Vec<ImagePlacement>> = HashMap::new();
        for (row, cells) in grid.rows().iter().enumerate() {
            for (column, cell) in cells.iter().enumerate() {
                for fragment in cell.image_fragments() {
                    let Some(source) = sources.get(&fragment.source) else {
                        continue;
                    };
                    let entries = runs.entry(fragment.source).or_default();
                    append_cell(
                        entries,
                        source,
                        row as u16,
                        column as u16,
                        fragment.row,
                        fragment.column,
                    );
                }
            }
        }
        self.native_images
            .iter()
            .filter(|source| source.screen == self.active)
            .flat_map(|source| merge_rows(runs.remove(&source.placement.id).unwrap_or_default()))
            .filter_map(|placement| AbsoluteImagePlacement::from_live(placement, row_origin))
            .collect()
    }

    pub(crate) fn restore_native_image_coverage(
        &mut self,
        cell_coverage: bool,
    ) -> Result<(), String> {
        if !cell_coverage
            && self
                .native_cells()
                .any(|cell| !cell.image_fragments().is_empty())
        {
            return Err("native image fragments require the cell coverage format".to_owned());
        }
        for screen in [Screen::Primary, Screen::Alternate] {
            let placements = match screen {
                Screen::Primary => &mut self.primary_image_placements,
                Screen::Alternate => &mut self.alternate_image_placements,
            };
            let mut physical = Vec::new();
            for mut placement in std::mem::take(placements) {
                if placement.record.protocol == GraphicsProtocol::Kitty {
                    physical.push(placement);
                    continue;
                }
                if !cell_coverage {
                    let grid = match screen {
                        Screen::Primary => Arc::make_mut(&mut self.primary),
                        Screen::Alternate => Arc::make_mut(&mut self.alternate),
                    };
                    attach_legacy_grid(grid, &placement);
                }
                placement.anchor = (0, 0);
                self.native_images
                    .push(NativeImageSource { screen, placement });
            }
            match screen {
                Screen::Primary => self.primary_image_placements = physical,
                Screen::Alternate => self.alternate_image_placements = physical,
            }
        }
        let live_top = self.scrollback.total_pushed();
        let history_start = live_top.saturating_sub(self.scrollback.len() as u64);
        let mut history = self.scrollback.take_lines();
        let mut physical = Vec::new();
        for old in std::mem::take(&mut self.primary_image_history) {
            if old.record.protocol == GraphicsProtocol::Kitty {
                physical.push(old);
                continue;
            }
            if cell_coverage {
                return Err(
                    "native image sources cannot be stored as history rectangles".to_owned(),
                );
            }
            let mut placement = ImagePlacement::new(
                old.id,
                old.record,
                old.content,
                old.plan,
                old.columns,
                old.rows,
                old.raster,
            );
            for row in 0..placement.rows {
                let absolute = old
                    .anchor
                    .0
                    .checked_add(u64::from(row))
                    .ok_or("native history row overflows")?;
                for column in 0..placement.columns {
                    let target_column = usize::from(old.anchor.1 + column);
                    let fragment = ImageCellFragment {
                        source: placement.id,
                        row: placement.plan.geometry.offset.y + row,
                        column: placement.plan.geometry.offset.x + column,
                    };
                    let overlay = placement.record.protocol == GraphicsProtocol::Sixel;
                    if absolute < live_top {
                        if let Some((cells, _)) = history.get_mut(
                            usize::try_from(
                                absolute
                                    .checked_sub(history_start)
                                    .ok_or("native history precedes retained rows")?,
                            )
                            .map_err(|_| "native history index exceeds address space")?,
                        ) {
                            cells.resize_with(cells.len().max(target_column + 1), Cell::blank);
                            cells[target_column].set_image_fragment(fragment, overlay);
                        }
                    } else if let Some(cell) = Arc::make_mut(&mut self.primary).cell_mut(
                        u16::try_from(absolute - live_top)
                            .map_err(|_| "native history row exceeds grid range")?,
                        u16::try_from(target_column)
                            .map_err(|_| "native history column exceeds grid range")?,
                    ) {
                        cell.set_image_fragment(fragment, overlay);
                    }
                }
            }
            placement.anchor = (0, 0);
            self.native_images.push(NativeImageSource {
                screen: Screen::Primary,
                placement,
            });
        }
        self.primary_image_history = physical;
        let retained = history.len() as u64;
        self.scrollback
            .replace_lines(history.into_iter().collect(), retained);
        self.native_images.sort_by_key(|source| source.placement.id);
        self.validate_native_fragments()?;
        self.rebuild_native_fragment_counts();
        if self.image_storage_bytes() > MAX_IMAGE_STORAGE_BYTES {
            return Err(
                "image pixels and fragment metadata exceed the image storage limit".to_owned(),
            );
        }
        self.retain_referenced_native_sources();
        Ok(())
    }

    fn validate_native_fragments(&self) -> Result<(), String> {
        let mut sources = HashMap::new();
        for source in &self.native_images {
            if sources.insert(source.placement.id, source).is_some() {
                return Err("native image source identities must be unique".to_owned());
            }
        }
        let mut storage_bytes = 0usize;
        for (screen, cells) in self
            .primary
            .rows()
            .iter()
            .map(|row| (Screen::Primary, row.as_slice()))
            .chain(
                self.alternate
                    .rows()
                    .iter()
                    .map(|row| (Screen::Alternate, row.as_slice())),
            )
            .chain(
                self.scrollback
                    .lines()
                    .iter()
                    .map(|(row, _)| (Screen::Primary, row.as_slice())),
            )
        {
            for cell in cells {
                storage_bytes = storage_bytes.saturating_add(cell.image_fragment_storage_bytes());
                if storage_bytes > MAX_IMAGE_STORAGE_BYTES {
                    return Err("native image fragment storage exceeds its limit".to_owned());
                }
                let mut seen = HashSet::new();
                for fragment in cell.image_fragments() {
                    let source = sources
                        .get(&fragment.source)
                        .ok_or("native image fragment has no source")?;
                    let size = source.placement.plan.geometry.full_size;
                    if source.screen != screen
                        || fragment.row >= size.rows
                        || fragment.column >= size.cols
                        || !seen.insert(fragment.source)
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
    cell: &mut Cell,
    fragment: ImageCellFragment,
    overlay: bool,
    counts: &mut HashMap<u64, usize>,
) {
    let replaces = overlay
        && cell
            .image_fragments()
            .iter()
            .any(|existing| existing.source == fragment.source);
    if !overlay {
        for existing in cell.image_fragments() {
            let _ = decrement_fragment_count(counts, existing.source);
        }
    }
    cell.set_image_fragment(fragment, overlay);
    if !replaces {
        *counts.entry(fragment.source).or_insert(0) += 1;
    }
}

fn decrement_fragment_count(counts: &mut HashMap<u64, usize>, source: u64) -> bool {
    if let Some(count) = counts.get_mut(&source) {
        *count -= 1;
        if *count == 0 {
            counts.remove(&source);
            return true;
        }
    }
    false
}

pub(super) fn clear_native_fragments(cell: &mut Cell, counts: &mut HashMap<u64, usize>) -> bool {
    let mut source_removed = false;
    for fragment in cell.image_fragments() {
        source_removed |= decrement_fragment_count(counts, fragment.source);
    }
    cell.clear_image_fragments();
    source_removed
}

pub(in crate::state) fn discard_native_fragment_references<'a>(
    counts: &mut HashMap<u64, usize>,
    cells: impl Iterator<Item = &'a Cell>,
) -> bool {
    let mut source_removed = false;
    for cell in cells {
        for fragment in cell.image_fragments() {
            source_removed |= decrement_fragment_count(counts, fragment.source);
        }
    }
    source_removed
}

fn clear_wide_fragments(grid: &mut Grid, row: u16, column: u16, counts: &mut HashMap<u64, usize>) {
    let other = match grid.cell(row, column).map_or(1, Cell::width) {
        2 => column.checked_add(1),
        0 => column.checked_sub(1),
        _ => None,
    };
    if let Some(other) = other {
        if let Some(cell) = grid.cell_mut(row, other) {
            let _ = clear_native_fragments(cell, counts);
        }
    }
}

fn attach_legacy_grid(grid: &mut Grid, placement: &ImagePlacement) {
    for row in 0..placement.rows {
        for column in 0..placement.columns {
            if let Some(cell) = grid.cell_mut(placement.anchor.0 + row, placement.anchor.1 + column)
            {
                cell.set_image_fragment(
                    ImageCellFragment {
                        source: placement.id,
                        row: placement.plan.geometry.offset.y + row,
                        column: placement.plan.geometry.offset.x + column,
                    },
                    placement.record.protocol == GraphicsProtocol::Sixel,
                );
            }
        }
    }
}

/// Extend a horizontal source run or start one cell's visible image portion.
pub(super) fn append_cell(
    entries: &mut Vec<ImagePlacement>,
    source: &ImagePlacement,
    row: u16,
    column: u16,
    source_row: u16,
    source_column: u16,
) {
    if let Some(last) = entries.last_mut() {
        if last.anchor.0 == row
            && last.anchor.1.checked_add(last.columns) == Some(column)
            && last.plan.geometry.offset.y == source_row
            && last.plan.geometry.offset.x.checked_add(last.columns) == Some(source_column)
        {
            last.columns += 1;
            return;
        }
    }
    let mut placement = source.with_anchor((row, column));
    placement.rows = 1;
    placement.columns = 1;
    placement.plan.geometry.offset = Point {
        x: source_column,
        y: source_row,
    };
    entries.push(placement);
}

/// Merge consecutive rows with identical target and source column ranges.
pub(super) fn merge_rows(runs: Vec<ImagePlacement>) -> Vec<ImagePlacement> {
    let mut result: Vec<ImagePlacement> = Vec::new();
    let mut preceding = HashMap::new();
    for run in runs {
        let key = (
            run.anchor.1,
            run.columns,
            run.plan.geometry.offset.x,
            i32::from(run.plan.geometry.offset.y) - i32::from(run.anchor.0),
        );
        if let Some(&index) = preceding.get(&key) {
            let previous: &mut ImagePlacement = &mut result[index];
            if previous.anchor.0.checked_add(previous.rows) == Some(run.anchor.0) {
                previous.rows += 1;
                continue;
            }
        }
        preceding.insert(key, result.len());
        result.push(run);
    }
    result
}

/// Assign distinct frame-local identities while preserving source paint order.
pub(super) fn append_derived(
    placements: &mut Vec<ImagePlacement>,
    derived: impl IntoIterator<Item = ImagePlacement>,
) {
    let mut used = placements
        .iter()
        .map(|placement| placement.id)
        .collect::<HashSet<_>>();
    let mut candidate = 1u64;
    for mut placement in derived {
        while used.contains(&candidate) {
            candidate = candidate
                .checked_add(1)
                .expect("a bounded grid cannot occupy the u64 identity space");
        }
        placement.id = candidate;
        used.insert(candidate);
        placements.push(placement);
    }
}
