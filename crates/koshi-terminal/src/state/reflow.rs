//! Resize reflow for the primary screen.
//!
//! A resize re-wraps the primary screen: scrollback and screen rows are
//! unwound into logical lines using each row's [`RowEnd`], prompt marks are
//! attached to content cell offsets in those lines, every logical line is
//! re-wrapped to the new width, and the result is split back into history and
//! screen. Text that soft-wrapped at the old width re-joins at a wider one,
//! and text wider than the new width wraps onto continuation rows: printing
//! `abcdef` at width 6, resizing to width 4, then back to 6 shows `abcd` /
//! `ef` and then `abcdef` again. Hard-ended rows never merge: a line that
//! exactly fills the width and ends with a line feed stays its own line.

use std::cmp::min;
use std::sync::Arc;

use koshi_core::process::PtySize;

use crate::grid::row_snapshot::{RepaddedRow, RowSnapshot};
use crate::grid::state::{count_row_content_cells, Cell, Grid, RowEnd, RowMetadata};
use crate::style::Style;

use super::{rebuild_cell_with_width, TerminalState};

#[derive(Debug, Default)]
struct LogicalLine {
    cells: Vec<Cell>,
    prompt_cell_offsets: Vec<usize>,
}

/// One row a logical line re-wraps into: the line's cells
/// `first_cell_index..end_cell_index`, then, for a [`RowEnd::SoftWide`] row, a
/// blank spacer in the background style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RewrappedRowSpan {
    first_cell_index: usize,
    end_cell_index: usize,
    row_metadata: RowMetadata,
}

impl RewrappedRowSpan {
    /// The line's cells the row holds. A soft-wide row's spacer is not one of
    /// them.
    fn count_content_cells(&self) -> usize {
        self.end_cell_index - self.first_cell_index
    }

    /// The row's cells: its content cells, and the spacer of a soft-wide row.
    fn count_row_cells(&self) -> usize {
        match self.row_metadata.row_end {
            RowEnd::SoftWide => self.count_content_cells() + 1,
            RowEnd::Soft | RowEnd::Hard => self.count_content_cells(),
        }
    }
}

/// One re-wrapped row: its cells encoded at the new width, padded with
/// default blanks, and the count of cells it held before the padding.
#[derive(Debug)]
struct RewrappedRow {
    row_snapshot: Arc<RowSnapshot>,
    row_cell_count: usize,
}

impl RewrappedRow {
    /// The row of `row_cell_count` cells that starts with `leading_cells` and
    /// holds default blanks after them, encoded with `row_metadata` and padded
    /// with default blanks to `column_count` cells. A row of `column_count`
    /// cells or more is encoded as it is.
    fn from_leading_cells(
        leading_cells: &[Cell],
        row_cell_count: usize,
        row_metadata: RowMetadata,
        column_count: u16,
    ) -> Self {
        let encoded_column_count = usize::from(column_count).max(row_cell_count);
        RewrappedRow {
            row_snapshot: Arc::new(RowSnapshot::from_padded_row_cells(
                leading_cells,
                u16::try_from(encoded_column_count).expect("a row holds at most 65 535 cells"),
                None,
                row_metadata,
                Vec::new(),
            )),
            row_cell_count,
        }
    }

    /// The row's cells without the padding.
    fn decode_row_cells(&self) -> Vec<Cell> {
        let mut row_cells = self.row_snapshot.decode_row_cells();
        row_cells.truncate(self.row_cell_count);
        row_cells
    }
}

/// The rows a reflow has re-wrapped so far, oldest first, with the shape of
/// each logical line, the place of each row it read, and where the cursor
/// lands.
#[derive(Debug)]
struct ReflowOutput {
    column_count: u16,
    background_style: Style,
    rewrapped_rows: Vec<RewrappedRow>,
    rewrapped_line_shapes: Vec<RewrappedLineShape>,
    /// The count of its logical line's content cells each rewrapped row
    /// holds, in the order of `rewrapped_rows`.
    rewrapped_row_content_cell_counts: Vec<usize>,
    /// One entry per history line and screen row read, oldest first: where
    /// its content cells sit in its logical line.
    source_rows: Vec<SourceRow>,
    new_cursor_physical_row_index: usize,
    new_cursor_column_index: usize,
    /// The spans of the logical line being pushed.
    row_spans: Vec<RewrappedRowSpan>,
    /// The cells of a soft-wide row whose spacer is not a default blank.
    spacer_row_cells: Vec<Cell>,
}

impl ReflowOutput {
    /// An output with no row yet, for `column_count` columns, with soft-wide
    /// spacers in `background_style`, and the cursor on the first row.
    fn from_column_count(column_count: u16, background_style: Style) -> Self {
        ReflowOutput {
            column_count,
            background_style,
            rewrapped_rows: Vec::new(),
            rewrapped_line_shapes: Vec::new(),
            rewrapped_row_content_cell_counts: Vec::new(),
            source_rows: Vec::new(),
            new_cursor_physical_row_index: 0,
            new_cursor_column_index: 0,
            row_spans: Vec::new(),
            spacer_row_cells: Vec::new(),
        }
    }

    /// Re-wrap `logical_line` to `column_count`, append its rows, and empty
    /// `logical_line`. Each prompt mark moves to the new row containing the
    /// marked row's first content cell. When `cursor_content_cell_offset` is
    /// `Some`, the cursor moves to the row and column where that content cell
    /// lands.
    fn push_logical_line(
        &mut self,
        logical_line: &mut LogicalLine,
        cursor_content_cell_offset: Option<usize>,
    ) {
        if self.column_count <= 1 {
            narrow_wide_glyphs(&mut logical_line.cells);
        }
        let rewrapped_line_start_row_index = self.rewrapped_rows.len();
        compute_rewrapped_row_spans(&logical_line.cells, self.column_count, &mut self.row_spans);
        self.rewrapped_line_shapes.push(RewrappedLineShape {
            first_rewrapped_row_index: rewrapped_line_start_row_index,
            row_count: self.row_spans.len(),
        });
        self.rewrapped_row_content_cell_counts.extend(
            self.row_spans
                .iter()
                .map(RewrappedRowSpan::count_content_cells),
        );
        let line_row_content_cell_counts =
            &self.rewrapped_row_content_cell_counts[rewrapped_line_start_row_index..];
        if let Some(cursor_content_cell_offset) = cursor_content_cell_offset {
            let (row_index_within_line, column_index) = locate_content_cell_offset(
                line_row_content_cell_counts,
                cursor_content_cell_offset,
            );
            self.new_cursor_physical_row_index =
                rewrapped_line_start_row_index + row_index_within_line;
            self.new_cursor_column_index = column_index;
        }
        for prompt_cell_offset in logical_line.prompt_cell_offsets.drain(..) {
            let (row_index_within_line, _) =
                locate_content_cell_offset(line_row_content_cell_counts, prompt_cell_offset);
            self.row_spans[row_index_within_line]
                .row_metadata
                .has_prompt_mark = true;
        }
        for row_span_index in 0..self.row_spans.len() {
            let row_span = self.row_spans[row_span_index];
            let span_cells =
                &logical_line.cells[row_span.first_cell_index..row_span.end_cell_index];
            let has_styled_spacer = row_span.row_metadata.row_end == RowEnd::SoftWide
                && self.background_style != Style::default();
            let leading_cells = if has_styled_spacer {
                self.spacer_row_cells.clear();
                self.spacer_row_cells.extend_from_slice(span_cells);
                self.spacer_row_cells
                    .push(Cell::build_blank_with_style(self.background_style));
                self.spacer_row_cells.as_slice()
            } else {
                span_cells
            };
            self.rewrapped_rows.push(RewrappedRow::from_leading_cells(
                leading_cells,
                row_span.count_row_cells(),
                row_span.row_metadata,
                self.column_count,
            ));
        }
        logical_line.cells.clear();
    }

    /// Append the logical line of one hard-ended row that keeps its cells at
    /// the new width: `repadded_row`.
    fn push_repadded_line(&mut self, repadded_row: RepaddedRow) {
        let run_cell_count = usize::from(repadded_row.run_cell_count);
        self.rewrapped_line_shapes.push(RewrappedLineShape {
            first_rewrapped_row_index: self.rewrapped_rows.len(),
            row_count: 1,
        });
        self.rewrapped_row_content_cell_counts.push(run_cell_count);
        self.rewrapped_rows.push(RewrappedRow {
            row_snapshot: Arc::new(repadded_row.row_snapshot),
            row_cell_count: run_cell_count,
        });
    }
}

/// One row the reflow reads: a history line, or a row of the live screen.
enum PhysicalRow<'a> {
    History(Arc<RowSnapshot>),
    Screen {
        row_cells: &'a [Cell],
        row_metadata: RowMetadata,
    },
}

#[derive(Debug, Clone, Copy)]
struct SourceRow {
    logical_line_index: usize,
    start_content_cell_offset: usize,
    contributed_cell_count: usize,
    row_end: RowEnd,
}

/// The rows one logical line re-wraps into:
/// `first_rewrapped_row_index..first_rewrapped_row_index + row_count`.
#[derive(Debug)]
struct RewrappedLineShape {
    first_rewrapped_row_index: usize,
    row_count: usize,
}

impl TerminalState {
    /// Rebuild the primary screen and its scrollback for `pty_size` by re-wrapping
    /// every logical line to the new width. The cursor stays on its logical
    /// line at its content cell offset, clamped to the new screen. Rows past the
    /// new height scroll into history; trailing rows below the cursor that
    /// end hard and hold only default blanks are dropped first. A zero-row
    /// size parks every row in history, and the next non-zero reflow puts the
    /// cursor on the first logical line.
    ///
    /// A resize that keeps the column count keeps every row as it is, with no
    /// re-wrap, when a re-wrap would give back the same rows
    /// ([`keep_primary_rows`](Self::keep_primary_rows)). The rows, the history,
    /// the cursor, the prompt marks and the image placements come out as a
    /// re-wrap gives them.
    pub(super) fn reflow_primary(&mut self, pty_size: PtySize) {
        let background_style = self.primary_render.style.get_background_fill_style();
        let old_total_pushed_row_count = self.scrollback.get_total_pushed_line_count();
        let old_retained_history_row_count = self.scrollback.get_retained_line_count();
        let reflow_output = match self.keep_primary_rows(pty_size.column_count, background_style) {
            Some(reflow_output) => reflow_output,
            None => self.rewrap_primary_rows(pty_size.column_count, background_style),
        };
        self.apply_reflow_output(
            pty_size,
            reflow_output,
            old_total_pushed_row_count,
            old_retained_history_row_count,
        );
    }

    /// Make `reflow_output` the primary screen and its history at `pty_size`.
    /// `old_total_pushed_row_count` and `old_retained_history_row_count` are
    /// the scrollback's counts before the reflow read its rows. Drops the
    /// blank rows below the cursor that do not fit, moves the rows past the
    /// screen height into history, pads the screen with blank rows in the
    /// output's background style, moves each primary image placement with its
    /// cell, and puts the cursor where the output says.
    fn apply_reflow_output(
        &mut self,
        pty_size: PtySize,
        reflow_output: ReflowOutput,
        old_total_pushed_row_count: u64,
        old_retained_history_row_count: usize,
    ) {
        let background_style = reflow_output.background_style;
        let ReflowOutput {
            mut rewrapped_rows,
            rewrapped_line_shapes,
            rewrapped_row_content_cell_counts,
            source_rows,
            new_cursor_physical_row_index,
            new_cursor_column_index,
            ..
        } = reflow_output;
        if rewrapped_rows.is_empty() && pty_size.row_count > 0 {
            rewrapped_rows.push(RewrappedRow::from_leading_cells(
                &[],
                0,
                RowMetadata::default(),
                pty_size.column_count,
            ));
        }

        // Drop trailing rows below the cursor that end hard and hold only
        // default blanks, down to the screen height. A styled blank row and a
        // blank row carrying a prompt mark each count as content and stay.
        while rewrapped_rows.len() > pty_size.row_count as usize
            && rewrapped_rows.len() > new_cursor_physical_row_index + 1
            && rewrapped_rows.last().is_some_and(|rewrapped_row| {
                let row_metadata = rewrapped_row.row_snapshot.get_row_metadata();
                row_metadata.row_end == RowEnd::Hard
                    && !row_metadata.has_prompt_mark
                    && count_row_content_cells(&rewrapped_row.decode_row_cells()) == 0
            })
        {
            rewrapped_rows.pop();
        }

        // Rows past the screen's height scroll into history, oldest first;
        // the rest — padded with blanks at the bottom — is the new screen.
        let overflow_row_count = rewrapped_rows
            .len()
            .saturating_sub(pty_size.row_count as usize);
        let history_rows: Vec<Arc<RowSnapshot>> = rewrapped_rows
            .drain(..overflow_row_count)
            .map(|rewrapped_row| rewrapped_row.row_snapshot)
            .collect();
        self.scrollback
            .replace_retained_lines(history_rows, old_retained_history_row_count as u64);

        let mut screen_rows: Vec<(Vec<Cell>, RowMetadata)> = rewrapped_rows
            .iter()
            .map(|rewrapped_row| {
                (
                    rewrapped_row.decode_row_cells(),
                    rewrapped_row.row_snapshot.get_row_metadata(),
                )
            })
            .collect();
        while screen_rows.len() < pty_size.row_count as usize {
            screen_rows.push((
                vec![
                    Cell::build_blank_with_style(background_style);
                    pty_size.column_count as usize
                ],
                RowMetadata::default(),
            ));
        }
        self.primary =
            Grid::from_rows_with_metadata(screen_rows, pty_size.column_count, background_style);

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
            let rewrapped_line_shape = rewrapped_line_shapes.get(source_row.logical_line_index)?;
            let old_content_cell_offset = source_row.start_content_cell_offset
                + match source_row.row_end {
                    RowEnd::Hard => usize::from(old_column_index),
                    RowEnd::Soft | RowEnd::SoftWide => {
                        usize::from(old_column_index).min(source_row.contributed_cell_count)
                    }
                };
            let first_rewrapped_row_index = rewrapped_line_shape.first_rewrapped_row_index;
            let (row_index_within_line, mut column_index) = locate_content_cell_offset(
                rewrapped_row_content_cell_counts.get(
                    first_rewrapped_row_index
                        ..first_rewrapped_row_index + rewrapped_line_shape.row_count,
                )?,
                old_content_cell_offset,
            );
            if source_row.row_end == RowEnd::Hard
                && usize::from(old_column_index) >= source_row.contributed_cell_count
            {
                column_index = usize::from(old_column_index);
            }
            let new_combined_row_index = rewrapped_line_shape
                .first_rewrapped_row_index
                .checked_add(row_index_within_line)?;
            if new_combined_row_index >= new_combined_row_count {
                return None;
            }
            let new_absolute_row = if new_combined_row_index < overflow_row_count {
                if new_combined_row_index < retained_history_start_row_index {
                    return None;
                }
                new_total_pushed_row_count
                    .checked_sub(retained_history_row_count as u64)?
                    .checked_add(
                        (new_combined_row_index - retained_history_start_row_index) as u64,
                    )?
            } else {
                new_total_pushed_row_count
                    .checked_add((new_combined_row_index - overflow_row_count) as u64)?
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

    /// The reflow output for `column_count` columns that keeps every history
    /// line and screen row of the primary screen as it is: each row stays one
    /// row with its cells, row end and prompt mark, and the cursor stays on its
    /// row and column. No history line is decoded but one whose soft-wide
    /// spacer or first cell the checks below read.
    ///
    /// Returns `None` when a re-wrap at `column_count` gives other rows:
    /// - `column_count` is under 2, differs from the screen's column count, or
    ///   the screen has no row;
    /// - the last row is not hard-ended, or a soft-ended row is followed by a
    ///   hard-ended row with no content cell;
    /// - a soft-wide row's last cell is not a blank in `background_style`, or
    ///   the row after it does not start with a wide glyph;
    /// - the cursor is on a soft-wide row's last column.
    ///
    /// Example — at 80 columns, 10 000 hard-ended history lines above a
    /// 30-row screen resize to 80 × 20: every line keeps its row, and 10 of the
    /// screen rows move into history.
    fn keep_primary_rows(
        &self,
        column_count: u16,
        background_style: Style,
    ) -> Option<ReflowOutput> {
        let (screen_row_count, screen_column_count) = self.primary.get_grid_dimensions();
        if column_count < 2 || column_count != screen_column_count || screen_row_count == 0 {
            return None;
        }
        let spacer_cell = Cell::build_blank_with_style(background_style);
        let history_lines = self.scrollback.list_retained_lines();
        let cursor_physical_row_index = history_lines.len() + usize::from(self.primary_cursor.row);
        let cursor_column_index = usize::from(self.primary_cursor.column);
        let mut kept_row_reflow = KeptRowReflow {
            reflow_output: ReflowOutput::from_column_count(column_count, background_style),
            current_line_first_row_index: 0,
            current_line_cell_count: 0,
            previous_row_end: RowEnd::Hard,
        };
        for history_line in history_lines {
            let row_metadata = history_line.get_row_metadata();
            let encoded_row_shape = history_line.get_encoded_row_shape();
            let row_cells = if kept_row_reflow.previous_row_end == RowEnd::SoftWide
                || row_metadata.row_end == RowEnd::SoftWide
            {
                history_line.decode_row_cells()
            } else {
                Vec::new()
            };
            let content_cell_count =
                usize::from(column_count - encoded_row_shape.trailing_blank_count);
            let row_cell_count = kept_row_reflow.push_kept_row(
                row_metadata,
                content_cell_count,
                is_wide_glyph_first(&row_cells),
                row_cells.last() == Some(&spacer_cell),
            )?;
            kept_row_reflow
                .reflow_output
                .rewrapped_rows
                .push(RewrappedRow {
                    row_snapshot: Arc::clone(history_line),
                    row_cell_count,
                });
        }
        for (screen_row_index, row_cells) in self.primary.list_row_cells().enumerate() {
            let row_metadata = self.primary.get_row_metadata(screen_row_index as u16);
            let physical_row_index = kept_row_reflow.reflow_output.rewrapped_rows.len();
            if physical_row_index == cursor_physical_row_index {
                if row_metadata.row_end == RowEnd::SoftWide
                    && cursor_column_index + 1 >= usize::from(column_count)
                {
                    return None;
                }
                kept_row_reflow.reflow_output.new_cursor_physical_row_index = physical_row_index;
                kept_row_reflow.reflow_output.new_cursor_column_index = cursor_column_index;
            }
            let row_cell_count = kept_row_reflow.push_kept_row(
                row_metadata,
                count_row_content_cells(row_cells),
                is_wide_glyph_first(row_cells),
                row_cells.last() == Some(&spacer_cell),
            )?;
            kept_row_reflow
                .reflow_output
                .rewrapped_rows
                .push(RewrappedRow::from_leading_cells(
                    row_cells,
                    row_cell_count,
                    row_metadata,
                    column_count,
                ));
        }
        if kept_row_reflow.previous_row_end != RowEnd::Hard {
            return None;
        }
        Some(kept_row_reflow.reflow_output)
    }

    /// The reflow output for `column_count` columns built by unwinding every
    /// history line and screen row of the primary screen into logical lines and
    /// re-wrapping each line. Takes every history line out of the scrollback.
    fn rewrap_primary_rows(&mut self, column_count: u16, background_style: Style) -> ReflowOutput {
        // Every physical row: history first (oldest at the front, taken out
        // of the scrollback buffer), then the live screen. A history row is
        // decoded straight into the logical line it belongs to.
        let history_lines = self.scrollback.take_retained_lines();
        let old_history_row_count = history_lines.len();
        let primary_grid = &self.primary;
        let screen_rows =
            primary_grid
                .list_row_cells()
                .enumerate()
                .map(|(row_index, row_cells)| PhysicalRow::Screen {
                    row_cells,
                    row_metadata: primary_grid.get_row_metadata(row_index as u16),
                });
        let cursor_physical_row_index =
            old_history_row_count + usize::from(self.primary_cursor.row);
        let cursor_column_index = usize::from(self.primary_cursor.column);

        // Unwind into logical lines, tracking which line holds the cursor and
        // where each prompt-marked row begins in that line. Each logical line
        // is re-wrapped and encoded as soon as its hard row end arrives.
        let mut reflow_output = ReflowOutput::from_column_count(column_count, background_style);
        let mut current_logical_line = LogicalLine::default();
        let mut cursor_content_cell_offset: Option<usize> = None;
        for (physical_row_index, physical_row) in history_lines
            .into_iter()
            .map(PhysicalRow::History)
            .chain(screen_rows)
            .enumerate()
        {
            // A hard-ended history row that starts its line and holds fewer
            // cells than the new width stays one row: its encoding takes the
            // new column count with no decode.
            if let PhysicalRow::History(row_snapshot) = &physical_row {
                let is_line_start = current_logical_line.cells.is_empty()
                    && current_logical_line.prompt_cell_offsets.is_empty();
                let repadded_row = if is_line_start && column_count > 1 {
                    row_snapshot.repad_hard_row(column_count)
                } else {
                    None
                };
                if let Some(repadded_row) = repadded_row {
                    reflow_output.source_rows.push(SourceRow {
                        logical_line_index: reflow_output.rewrapped_line_shapes.len(),
                        start_content_cell_offset: 0,
                        contributed_cell_count: usize::from(repadded_row.run_cell_count),
                        row_end: RowEnd::Hard,
                    });
                    reflow_output.push_repadded_line(repadded_row);
                    continue;
                }
            }
            let line_cells = &mut current_logical_line.cells;
            let row_start_cell_offset = line_cells.len();
            let row_metadata = match physical_row {
                PhysicalRow::History(row_snapshot) => {
                    let row_metadata = row_snapshot.get_row_metadata();
                    let encoded_row_shape = row_snapshot.decode_row_run_cells_into(line_cells);
                    if row_metadata.row_end != RowEnd::Hard {
                        line_cells.resize_with(
                            row_start_cell_offset + usize::from(encoded_row_shape.column_count),
                            Cell::build_blank,
                        );
                    }
                    row_metadata
                }
                PhysicalRow::Screen {
                    row_cells,
                    row_metadata,
                } => {
                    line_cells.extend_from_slice(row_cells);
                    row_metadata
                }
            };
            let row_cells = &line_cells[row_start_cell_offset..];
            let content_cell_count = match row_metadata.row_end {
                // A soft-wrapped row is full: every cell is content.
                RowEnd::Soft => row_cells.len(),
                // Its trailing blank is a spacer standing in for the wide
                // glyph that starts the next row; the glyph is the content.
                RowEnd::SoftWide => row_cells.len().saturating_sub(1),
                // Trailing fully-default blanks are padding, not text.
                RowEnd::Hard => count_row_content_cells(row_cells),
            };
            line_cells.truncate(row_start_cell_offset + content_cell_count);
            reflow_output.source_rows.push(SourceRow {
                logical_line_index: reflow_output.rewrapped_line_shapes.len(),
                start_content_cell_offset: row_start_cell_offset,
                contributed_cell_count: content_cell_count,
                row_end: row_metadata.row_end,
            });
            if row_metadata.has_prompt_mark {
                current_logical_line
                    .prompt_cell_offsets
                    .push(row_start_cell_offset);
            }
            if physical_row_index == cursor_physical_row_index {
                // On a hard-ended row the cursor's column counts in full,
                // padding included; on a soft row it is capped at the row's
                // content.
                cursor_content_cell_offset = Some(
                    row_start_cell_offset
                        + if row_metadata.row_end == RowEnd::Hard {
                            cursor_column_index
                        } else {
                            min(cursor_column_index, content_cell_count)
                        },
                );
            }
            if row_metadata.row_end == RowEnd::Hard {
                reflow_output.push_logical_line(
                    &mut current_logical_line,
                    cursor_content_cell_offset.take(),
                );
            }
        }
        // A trailing soft-wrapped row with no hard end below it still forms a
        // line, as does a prompt mark left on an empty tail.
        if !current_logical_line.cells.is_empty()
            || !current_logical_line.prompt_cell_offsets.is_empty()
        {
            reflow_output
                .push_logical_line(&mut current_logical_line, cursor_content_cell_offset.take());
        }
        reflow_output
    }
}

/// The rows a resize that keeps the column count keeps, built row by row into
/// the reflow output a re-wrap gives for them.
struct KeptRowReflow {
    /// The output so far. Its rewrapped rows are the caller's to push.
    reflow_output: ReflowOutput,
    /// The index of the first row of the logical line not yet ended.
    current_line_first_row_index: usize,
    /// The content cells the rows of the logical line not yet ended hold.
    current_line_cell_count: usize,
    /// How the last row added ends: [`RowEnd::Hard`] before the first row.
    previous_row_end: RowEnd,
}

impl KeptRowReflow {
    /// Add the shape of one kept row: its source row, the count of its logical
    /// line's content cells it holds, and the logical line it ends when it is
    /// hard-ended. `content_cell_count` counts the row's cells without its
    /// trailing default blanks. `has_leading_wide_glyph` and
    /// `has_trailing_spacer` are read only after a soft-wide row and on a
    /// soft-wide row.
    ///
    /// Returns the row's cell count, which a re-wrap would give it: its content
    /// cells for a hard-ended row, and the column count for a soft-ended or
    /// soft-wide row. Returns `None`, adding nothing, when a re-wrap changes
    /// the row: a hard-ended row with no content cell after a soft-ended row, a
    /// row that does not start with a wide glyph after a soft-wide row, or a
    /// soft-wide row whose last cell is not the spacer.
    fn push_kept_row(
        &mut self,
        row_metadata: RowMetadata,
        content_cell_count: usize,
        has_leading_wide_glyph: bool,
        has_trailing_spacer: bool,
    ) -> Option<usize> {
        let column_cell_count = usize::from(self.reflow_output.column_count);
        let (row_cell_count, contributed_cell_count) = match row_metadata.row_end {
            RowEnd::Hard => (content_cell_count, content_cell_count),
            RowEnd::Soft => (column_cell_count, column_cell_count),
            RowEnd::SoftWide => (column_cell_count, column_cell_count - 1),
        };
        let is_kept_after_previous_row = match self.previous_row_end {
            RowEnd::Hard => true,
            RowEnd::Soft => contributed_cell_count > 0,
            RowEnd::SoftWide => has_leading_wide_glyph,
        };
        if !is_kept_after_previous_row
            || (row_metadata.row_end == RowEnd::SoftWide && !has_trailing_spacer)
        {
            return None;
        }
        let reflow_output = &mut self.reflow_output;
        reflow_output.source_rows.push(SourceRow {
            logical_line_index: reflow_output.rewrapped_line_shapes.len(),
            start_content_cell_offset: self.current_line_cell_count,
            contributed_cell_count,
            row_end: row_metadata.row_end,
        });
        self.current_line_cell_count += contributed_cell_count;
        reflow_output
            .rewrapped_row_content_cell_counts
            .push(contributed_cell_count);
        if row_metadata.row_end == RowEnd::Hard {
            let row_index_after_line = reflow_output.source_rows.len();
            reflow_output
                .rewrapped_line_shapes
                .push(RewrappedLineShape {
                    first_rewrapped_row_index: self.current_line_first_row_index,
                    row_count: row_index_after_line - self.current_line_first_row_index,
                });
            self.current_line_first_row_index = row_index_after_line;
            self.current_line_cell_count = 0;
        }
        self.previous_row_end = row_metadata.row_end;
        Some(row_cell_count)
    }
}

/// Whether the first of `row_cells` is the base of a wide glyph. `false` for
/// no cell.
fn is_wide_glyph_first(row_cells: &[Cell]) -> bool {
    row_cells
        .first()
        .is_some_and(|row_cell| row_cell.get_display_width() == 2)
}

/// Store each wide glyph of `line_cells` narrow, and drop the width-0 cell
/// right after it, as a one-column row holds them. Every other cell stays.
fn narrow_wide_glyphs(line_cells: &mut Vec<Cell>) {
    let mut follows_wide_glyph = false;
    line_cells.retain_mut(|line_cell| {
        let is_wide_glyph_continuation = follows_wide_glyph && line_cell.get_display_width() == 0;
        follows_wide_glyph = !is_wide_glyph_continuation && line_cell.get_display_width() == 2;
        if follows_wide_glyph {
            *line_cell = rebuild_cell_with_width(line_cell, 1);
        }
        !is_wide_glyph_continuation
    });
}

/// Fill `row_spans` with the `column_count`-wide rows that one logical line's
/// content cells re-wrap into. `column_count` of `0` wraps at one column, and
/// a line for one column holds no wide glyph. Empty content gives one empty
/// [`RowEnd::Hard`] row. Every row's `has_prompt_mark` is `false`.
///
/// `abcdef` at `column_count = 4` → `abcd` ([`RowEnd::Soft`]) then `ef`
/// ([`RowEnd::Hard`]). A wide glyph whose base would land in a row's last
/// column ends the row [`RowEnd::SoftWide`], with a spacer in that column, and
/// starts the next row whole. A line of fewer than `column_count` cells is one
/// row, read without a pass over its cells.
fn compute_rewrapped_row_spans(
    line_cells: &[Cell],
    column_count: u16,
    row_spans: &mut Vec<RewrappedRowSpan>,
) {
    let column_count = usize::from(column_count.max(1));
    row_spans.clear();
    let mut first_cell_index = 0usize;
    let cells_to_scan = if line_cells.len() < column_count {
        &[]
    } else {
        line_cells
    };
    for (cell_index, line_cell) in cells_to_scan.iter().enumerate() {
        if cell_index - first_cell_index == column_count {
            row_spans.push(RewrappedRowSpan {
                first_cell_index,
                end_cell_index: cell_index,
                row_metadata: RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: false,
                },
            });
            first_cell_index = cell_index;
        }
        if line_cell.get_display_width() == 2 && cell_index - first_cell_index + 1 == column_count {
            row_spans.push(RewrappedRowSpan {
                first_cell_index,
                end_cell_index: cell_index,
                row_metadata: RowMetadata {
                    row_end: RowEnd::SoftWide,
                    has_prompt_mark: false,
                },
            });
            first_cell_index = cell_index;
        }
    }
    row_spans.push(RewrappedRowSpan {
        first_cell_index,
        end_cell_index: line_cells.len(),
        row_metadata: RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: false,
        },
    });
}

/// The (row-within-line, column) where `content_cell_offset` lands among the
/// rows of one re-wrapped line, given the count of the line's content cells
/// each row holds. A [`RowEnd::SoftWide`] row's spacer holds no offset. An
/// offset past the content lands in the final row at a column past its
/// cells, not clamped to the screen width. No row gives `(0, 0)`.
///
/// Example — rows holding 4, 4 and 2 content cells: offset 9 lands at
/// `(2, 1)`, and offset 11 at `(2, 3)`.
fn locate_content_cell_offset(
    row_content_cell_counts: &[usize],
    content_cell_offset: usize,
) -> (usize, usize) {
    let mut remaining_content_cell_offset = content_cell_offset;
    for (row_index, row_content_cell_count) in row_content_cell_counts.iter().enumerate() {
        if remaining_content_cell_offset < *row_content_cell_count
            || row_index + 1 == row_content_cell_counts.len()
        {
            return (row_index, remaining_content_cell_offset);
        }
        remaining_content_cell_offset -= row_content_cell_count;
    }
    (0, 0)
}

#[cfg(test)]
mod tests;
