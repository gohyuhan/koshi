//! Resize reflow for the primary screen.
//!
//! A resize re-wraps the primary screen: scrollback and screen rows are
//! unwound into logical lines using each row's [`RowEnd`], prompt marks are
//! attached to content offsets in those lines, every logical line is
//! re-wrapped to the new width, and the result is split back into history and
//! screen. Text that soft-wrapped at the old width re-joins at a wider one,
//! and text wider than the new width wraps onto continuation rows: printing
//! `abcdef` at width 6, resizing to width 4, then back to 6 shows `abcd` /
//! `ef` and then `abcdef` again. Hard-ended rows never merge: a line that
//! exactly fills the width and ends with a line feed stays its own line.

use std::cmp::min;
use std::sync::Arc;

use koshi_core::process::PtySize;

use crate::grid::state::{count_row_content_cells, Cell, Grid, RowEnd, RowMetadata};
use crate::style::Style;

use super::{rebuild_cell_with_width, TerminalState};

#[derive(Debug)]
struct LogicalLine {
    content: Vec<Cell>,
    prompt_offsets: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
struct SourceRow {
    line_index: usize,
    start_offset: usize,
    contributed_cell_count: usize,
    row_end: RowEnd,
}

#[derive(Debug)]
struct RewrappedLineShape {
    first_row_index: usize,
    row_shapes: Vec<RewrappedRowShape>,
}

#[derive(Debug, Clone, Copy)]
struct RewrappedRowShape {
    cell_count: usize,
    contributed_cell_count: usize,
}

impl TerminalState {
    /// Rebuild the primary screen and its scrollback for `pty_size` by re-wrapping
    /// every logical line to the new width. The cursor stays on its logical
    /// line at its content offset, clamped to the new screen. Rows past the
    /// new height scroll into history; trailing rows below the cursor that
    /// end hard and hold only default blanks are dropped first. A zero-row
    /// size parks every row in history, and the next non-zero reflow puts the
    /// cursor on the first logical line.
    pub(super) fn reflow_primary(&mut self, pty_size: PtySize) {
        let background_style = self.primary_render.style.get_background_fill_style();
        let old_total_pushed_row_count = self.scrollback.get_total_pushed_line_count();
        let old_retained_history_row_count = self.scrollback.get_retained_line_count();

        // Every physical row: history first (oldest at the front, taken out
        // of the scrollback buffer), then the live screen, each with its row
        // metadata.
        let mut physical_rows: Vec<(Vec<Cell>, RowMetadata)> =
            Vec::from(self.scrollback.take_retained_lines());
        let old_history_row_count = physical_rows.len();
        for (row_index, row_cells) in self.primary.list_rows().iter().enumerate() {
            physical_rows.push((
                row_cells.clone(),
                self.primary.get_row_metadata(row_index as u16),
            ));
        }
        let cursor_physical_row_index = old_history_row_count + self.primary_cursor.row as usize;
        let cursor_column_index = self.primary_cursor.column as usize;

        // Unwind into logical lines, tracking which line holds the cursor and
        // where each prompt-marked row begins in that line.
        let mut lines: Vec<LogicalLine> = Vec::new();
        let mut current_line_cells: Vec<Cell> = Vec::new();
        let mut prompt_offsets: Vec<usize> = Vec::new();
        let mut source_rows: Vec<SourceRow> = Vec::new();
        let mut cursor_line = 0_usize;
        let mut cursor_offset = 0_usize;
        for (physical_row_index, (row_cells, row_metadata)) in physical_rows.into_iter().enumerate()
        {
            let content_cell_count = match row_metadata.row_end {
                // A soft-wrapped row is full: every cell is content.
                RowEnd::Soft => row_cells.len(),
                // Its trailing blank is a spacer standing in for the wide
                // glyph that starts the next row; the glyph is the content.
                RowEnd::SoftWide => row_cells.len().saturating_sub(1),
                // Trailing fully-default blanks are padding, not text.
                RowEnd::Hard => count_row_content_cells(&row_cells),
            };
            let source_row = SourceRow {
                line_index: lines.len(),
                start_offset: current_line_cells.len(),
                contributed_cell_count: content_cell_count,
                row_end: row_metadata.row_end,
            };
            source_rows.push(source_row);
            if row_metadata.has_prompt_mark {
                prompt_offsets.push(current_line_cells.len());
            }
            if physical_row_index == cursor_physical_row_index {
                cursor_line = lines.len();
                // On a hard-ended row the cursor's column counts in full,
                // padding included; on a soft row it is capped at the row's
                // content.
                cursor_offset = current_line_cells.len()
                    + if row_metadata.row_end == RowEnd::Hard {
                        cursor_column_index
                    } else {
                        min(cursor_column_index, content_cell_count)
                    };
            }
            current_line_cells.extend(row_cells.into_iter().take(content_cell_count));
            if row_metadata.row_end == RowEnd::Hard {
                lines.push(LogicalLine {
                    content: std::mem::take(&mut current_line_cells),
                    prompt_offsets: std::mem::take(&mut prompt_offsets),
                });
            }
        }
        // A trailing soft-wrapped row with no hard end below it still forms a
        // line, as does a prompt mark left on an empty tail.
        if !current_line_cells.is_empty() || !prompt_offsets.is_empty() {
            lines.push(LogicalLine {
                content: current_line_cells,
                prompt_offsets,
            });
        }

        // Re-wrap every logical line to the new width and move each prompt
        // mark to the new row containing the marked row's first content cell.
        let mut rewrapped_rows: Vec<(Vec<Cell>, RowMetadata)> = Vec::new();
        let mut line_shapes: Vec<RewrappedLineShape> = Vec::new();
        let mut new_cursor_physical_row_index = 0_usize;
        let mut new_cursor_column_index = 0_usize;
        for (line_index, line) in lines.into_iter().enumerate() {
            let start_row_index = rewrapped_rows.len();
            let mut line_rewrapped_rows =
                rewrap_line(line.content, pty_size.column_count, background_style);
            if line_index == cursor_line {
                let (row_index_within_line, column_index) =
                    locate_content_offset(&line_rewrapped_rows, cursor_offset);
                new_cursor_physical_row_index = start_row_index + row_index_within_line;
                new_cursor_column_index = column_index;
            }
            for offset in line.prompt_offsets {
                let (row_index_within_line, _) =
                    locate_content_offset(&line_rewrapped_rows, offset);
                line_rewrapped_rows[row_index_within_line].1.has_prompt_mark = true;
            }
            line_shapes.push(RewrappedLineShape {
                first_row_index: start_row_index,
                row_shapes: line_rewrapped_rows
                    .iter()
                    .map(|(row_cells, row_metadata)| RewrappedRowShape {
                        cell_count: row_cells.len(),
                        contributed_cell_count: rewrapped_row_content_len(row_cells, *row_metadata),
                    })
                    .collect(),
            });
            rewrapped_rows.extend(line_rewrapped_rows);
        }
        if rewrapped_rows.is_empty() && pty_size.row_count > 0 {
            rewrapped_rows.push((Vec::new(), RowMetadata::default()));
        }

        // Drop trailing rows below the cursor that end hard and hold only
        // default blanks, down to the screen height. A styled blank row and a
        // blank row carrying a prompt mark each count as content and stay.
        while rewrapped_rows.len() > pty_size.row_count as usize
            && rewrapped_rows.len() > new_cursor_physical_row_index + 1
            && rewrapped_rows
                .last()
                .is_some_and(|(row_cells, row_metadata)| {
                    row_metadata.row_end == RowEnd::Hard
                        && !row_metadata.has_prompt_mark
                        && count_row_content_cells(row_cells) == 0
                })
        {
            rewrapped_rows.pop();
        }

        // Rows past the screen's height scroll into history, oldest first;
        // the rest — padded with blanks at the bottom — is the new screen.
        let overflow_row_count = rewrapped_rows
            .len()
            .saturating_sub(pty_size.row_count as usize);
        let history_rows: Vec<(Vec<Cell>, RowMetadata)> =
            rewrapped_rows.drain(..overflow_row_count).collect();
        self.scrollback
            .replace_retained_lines(history_rows, old_history_row_count as u64);

        while rewrapped_rows.len() < pty_size.row_count as usize {
            rewrapped_rows.push((
                vec![Cell::blank_with(background_style); pty_size.column_count as usize],
                RowMetadata::default(),
            ));
        }
        self.primary = Arc::new(Grid::from_rows_with_metadata(
            rewrapped_rows,
            pty_size.column_count,
            background_style,
        ));

        let retained_history_row_count = self.scrollback.get_retained_line_count();
        let retained_history_start_row_index =
            overflow_row_count.saturating_sub(retained_history_row_count);
        let new_total_pushed_row_count = self.scrollback.get_total_pushed_line_count();
        let new_combined_row_count =
            overflow_row_count.saturating_add(usize::from(pty_size.row_count));
        let old_history_first_row =
            old_total_pushed_row_count.saturating_sub(old_retained_history_row_count as u64);
        let primary_image_position_mapper = |old_absolute_row: u64, old_column_index: u16| {
            let old_physical_row_index = if old_absolute_row < old_total_pushed_row_count {
                usize::try_from(old_absolute_row.checked_sub(old_history_first_row)?).ok()?
            } else {
                let live_row_index =
                    usize::try_from(old_absolute_row - old_total_pushed_row_count).ok()?;
                old_retained_history_row_count.checked_add(live_row_index)?
            };
            let source_row = source_rows.get(old_physical_row_index)?;
            let line_shape = line_shapes.get(source_row.line_index)?;
            let old_content_offset = source_row.start_offset
                + match source_row.row_end {
                    RowEnd::Hard => usize::from(old_column_index),
                    RowEnd::Soft | RowEnd::SoftWide => {
                        usize::from(old_column_index).min(source_row.contributed_cell_count)
                    }
                };
            let (row_index_within_line, mut column_index) =
                locate_shape_offset(&line_shape.row_shapes, old_content_offset);
            if source_row.row_end == RowEnd::Hard
                && usize::from(old_column_index) >= source_row.contributed_cell_count
            {
                column_index = usize::from(old_column_index);
            }
            let combined_row_index = line_shape
                .first_row_index
                .checked_add(row_index_within_line)?;
            if combined_row_index >= new_combined_row_count {
                return None;
            }
            let new_absolute_row = if combined_row_index < overflow_row_count {
                if combined_row_index < retained_history_start_row_index {
                    return None;
                }
                new_total_pushed_row_count
                    .checked_sub(retained_history_row_count as u64)?
                    .checked_add((combined_row_index - retained_history_start_row_index) as u64)?
            } else {
                new_total_pushed_row_count
                    .checked_add((combined_row_index - overflow_row_count) as u64)?
            };
            Some((new_absolute_row, u16::try_from(column_index).ok()?))
        };
        self.remap_primary_image_placements(
            old_total_pushed_row_count,
            primary_image_position_mapper,
        );

        self.primary_cursor.row = min(
            new_cursor_physical_row_index.saturating_sub(overflow_row_count),
            pty_size.row_count.saturating_sub(1) as usize,
        ) as u16;
        self.primary_cursor.column = min(
            new_cursor_column_index,
            pty_size.column_count.saturating_sub(1) as usize,
        ) as u16;
    }
}

/// Re-wrap one logical line's content into `column_count`-wide rows. `column_count` of `0`
/// wraps at one column. Empty content gives one empty [`RowEnd::Hard`] row.
/// Every row's `prompt` is `false`.
///
/// `abcdef` at `column_count = 4` → `abcd` ([`RowEnd::Soft`]) then `ef`
/// ([`RowEnd::Hard`]). A wide glyph whose base would land in a row's last
/// column leaves a blank spacer in `background_style` there and starts the next row whole
/// ([`RowEnd::SoftWide`]). At one column a wide glyph is stored narrow and its
/// width-0 continuation cell is skipped.
fn rewrap_line(
    content_cells: Vec<Cell>,
    column_count: u16,
    background_style: Style,
) -> Vec<(Vec<Cell>, RowMetadata)> {
    let column_count = column_count.max(1) as usize;
    let mut output_rows: Vec<(Vec<Cell>, RowMetadata)> = Vec::new();
    let mut row_cells: Vec<Cell> = Vec::with_capacity(column_count.min(content_cells.len()));
    let mut content_cells_iterator = content_cells.into_iter().peekable();
    while let Some(cell) = content_cells_iterator.next() {
        if row_cells.len() == column_count {
            let full_row_cells = std::mem::replace(
                &mut row_cells,
                Vec::with_capacity(column_count.min(content_cells_iterator.len() + 1)),
            );
            output_rows.push((
                full_row_cells,
                RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: false,
                },
            ));
        }
        if cell.get_display_width() == 2 {
            if column_count == 1 {
                // Store the base narrow and skip its width-0 continuation.
                row_cells.push(rebuild_cell_with_width(&cell, 1));
                if content_cells_iterator
                    .peek()
                    .is_some_and(|next_cell| next_cell.get_display_width() == 0)
                {
                    content_cells_iterator.next();
                }
                continue;
            }
            if row_cells.len() + 1 == column_count {
                // The base would land in the last column: fill it with a
                // spacer, end the row `SoftWide`, and start the next row
                // with the glyph.
                row_cells.push(Cell::blank_with(background_style));
                let full_row_cells = std::mem::replace(
                    &mut row_cells,
                    Vec::with_capacity(column_count.min(content_cells_iterator.len() + 1)),
                );
                output_rows.push((
                    full_row_cells,
                    RowMetadata {
                        row_end: RowEnd::SoftWide,
                        has_prompt_mark: false,
                    },
                ));
            }
        }
        row_cells.push(cell);
    }
    output_rows.push((
        row_cells,
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: false,
        },
    ));
    output_rows
}

/// The (row-within-line, column) where content offset `offset` lands among a
/// re-wrapped line's rows. A [`RowEnd::SoftWide`] row's spacer holds no
/// offset. An offset past the content lands in the final row at a column
/// past its cells, not clamped to the screen width. Empty `rows` gives
/// `(0, 0)`.
fn locate_content_offset(
    row_cells_and_metadata: &[(Vec<Cell>, RowMetadata)],
    content_offset: usize,
) -> (usize, usize) {
    let mut remaining_content_offset = content_offset;
    for (row_index, (row_cells, row_metadata)) in row_cells_and_metadata.iter().enumerate() {
        let contributed_cell_count = match row_metadata.row_end {
            RowEnd::SoftWide => row_cells.len().saturating_sub(1),
            RowEnd::Soft | RowEnd::Hard => row_cells.len(),
        };
        if remaining_content_offset < contributed_cell_count
            || row_index + 1 == row_cells_and_metadata.len()
        {
            return (row_index, remaining_content_offset);
        }
        remaining_content_offset -= contributed_cell_count;
    }
    (0, 0)
}

/// Return the content cells represented by one re-wrapped row. A soft-wide
/// row's final blank is a spacer for the wide glyph on the next row.
fn rewrapped_row_content_len(row_cells: &[Cell], row_metadata: RowMetadata) -> usize {
    match row_metadata.row_end {
        RowEnd::SoftWide => row_cells.len().saturating_sub(1),
        RowEnd::Soft | RowEnd::Hard => row_cells.len(),
    }
}

/// Locate a content offset in the compact shape of one re-wrapped line.
fn locate_shape_offset(row_shapes: &[RewrappedRowShape], content_offset: usize) -> (usize, usize) {
    let mut remaining_content_offset = content_offset;
    for (row_index, row_shape) in row_shapes.iter().enumerate() {
        if remaining_content_offset < row_shape.contributed_cell_count
            || row_index + 1 == row_shapes.len()
        {
            return (
                row_index,
                remaining_content_offset.min(row_shape.cell_count),
            );
        }
        remaining_content_offset -= row_shape.contributed_cell_count;
    }
    (0, 0)
}

#[cfg(test)]
mod tests;
