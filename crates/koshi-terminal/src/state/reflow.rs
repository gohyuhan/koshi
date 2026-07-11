//! Resize reflow for the primary screen.
//!
//! A resize re-wraps the primary screen instead of cropping it: scrollback
//! and screen rows are unwound into logical lines using each row's
//! [`RowEnd`], every logical line is re-wrapped to the new width, and the
//! result is split back into history and screen. Text that soft-wrapped at
//! the old width re-joins at a wider one, and text wider than the new width
//! wraps onto continuation rows instead of being cut off — printing
//! `abcdef` at width 6, resizing to width 4, then back to 6 shows `abcd` /
//! `ef` and then `abcdef` again. Hard-ended rows never merge: a line that
//! exactly fills the width and ends with a line feed stays its own line.

use std::cmp::min;
use std::sync::Arc;

use koshi_core::process::PtySize;

use crate::grid::state::{Cell, Grid, RowEnd};
use crate::style::Style;

use super::{row_is_blank, TerminalState};

impl TerminalState {
    /// Rebuild the primary screen and its scrollback for `size` by re-wrapping
    /// every logical line to the new width. The cursor stays on its logical
    /// line at its content offset; rows past the new height scroll into
    /// history and trailing blank padding rows are dropped instead.
    pub(super) fn reflow_primary(&mut self, size: PtySize) {
        let fill = self.primary_render.style.bg_fill();

        // Every physical row: history first (oldest at the front), then the
        // live screen, each with its continuation state.
        let mut physical: Vec<(Vec<Cell>, RowEnd)> =
            self.scrollback.lines().iter().cloned().collect();
        let history_len = physical.len();
        for (index, row) in self.primary.rows().iter().enumerate() {
            physical.push((row.clone(), self.primary.row_end(index as u16)));
        }
        let cursor_physical = history_len + self.primary_cursor.row as usize;
        let cursor_col = self.primary_cursor.col as usize;

        // Unwind into logical lines, tracking which line holds the cursor and
        // how many content cells precede it there.
        let mut lines: Vec<Vec<Cell>> = Vec::new();
        let mut current: Vec<Cell> = Vec::new();
        let mut cursor_line = 0_usize;
        let mut cursor_offset = 0_usize;
        for (index, (row, end)) in physical.into_iter().enumerate() {
            let contributed = match end {
                // A soft-wrapped row is full: every cell is content.
                RowEnd::Soft => row.len(),
                // Its trailing blank is a spacer standing in for the wide
                // glyph that starts the next row; the glyph is the content.
                RowEnd::SoftWide => row.len().saturating_sub(1),
                // Trailing fully-default blanks are padding, not text.
                RowEnd::Hard => content_len(&row),
            };
            if index == cursor_physical {
                cursor_line = lines.len();
                // On a line's final row the cursor may rest in the padding
                // past the text; keep that offset so the position survives
                // when the new width still holds it.
                cursor_offset = current.len()
                    + if end == RowEnd::Hard {
                        cursor_col
                    } else {
                        min(cursor_col, contributed)
                    };
            }
            current.extend(row.into_iter().take(contributed));
            if end == RowEnd::Hard {
                lines.push(std::mem::take(&mut current));
            }
        }
        // A trailing soft-wrapped row with no hard end below it (possible in
        // history after heavy eviction) still forms a line.
        if !current.is_empty() {
            lines.push(current);
        }

        // Re-wrap every logical line to the new width.
        let mut rewrapped: Vec<(Vec<Cell>, RowEnd)> = Vec::new();
        let mut new_cursor_physical = 0_usize;
        let mut new_cursor_col = 0_usize;
        for (index, content) in lines.iter().enumerate() {
            let start = rewrapped.len();
            let rows = rewrap_line(content, size.cols, fill);
            if index == cursor_line {
                let (row_in_line, col) = locate_offset(&rows, cursor_offset);
                new_cursor_physical = start + row_in_line;
                new_cursor_col = col;
            }
            rewrapped.extend(rows);
        }
        if rewrapped.is_empty() {
            rewrapped.push((Vec::new(), RowEnd::Hard));
        }

        // Trailing blank rows below the cursor are padding the screen can
        // re-create; drop them rather than pushing real history further out.
        while rewrapped.len() > size.rows as usize
            && rewrapped.len() > new_cursor_physical + 1
            && rewrapped
                .last()
                .is_some_and(|(row, end)| *end == RowEnd::Hard && row_is_blank(row))
        {
            rewrapped.pop();
        }

        // Rows past the screen's height scroll into history, oldest first;
        // the rest — padded with blanks at the bottom — is the new screen.
        let overflow = rewrapped.len().saturating_sub(size.rows as usize);
        let history: Vec<(Vec<Cell>, RowEnd)> = rewrapped.drain(..overflow).collect();
        self.scrollback.replace_lines(history);

        let mut cells: Vec<Vec<Cell>> = Vec::with_capacity(size.rows as usize);
        let mut ends: Vec<RowEnd> = Vec::with_capacity(size.rows as usize);
        for (row, end) in rewrapped {
            cells.push(row);
            ends.push(end);
        }
        while cells.len() < size.rows as usize {
            cells.push(vec![Cell::blank_with(fill); size.cols as usize]);
            ends.push(RowEnd::Hard);
        }
        let mut grid = Grid::from_rows(cells, size.cols, fill);
        for (row, end) in ends.into_iter().enumerate() {
            grid.set_row_end(row as u16, end);
        }
        self.primary = Arc::new(grid);

        self.primary_cursor.row = min(
            new_cursor_physical.saturating_sub(overflow),
            size.rows.saturating_sub(1) as usize,
        ) as u16;
        self.primary_cursor.col = min(new_cursor_col, size.cols.saturating_sub(1) as usize) as u16;
    }
}

/// The number of content cells in a hard-ended row: its length with the
/// trailing run of fully-default blanks (the padding every row is filled
/// with) excluded. A styled blank — e.g. a background-colored prompt
/// segment — counts as content, so its color survives a reflow.
fn content_len(row: &[Cell]) -> usize {
    let blank = Cell::blank();
    row.iter()
        .rposition(|cell| *cell != blank)
        .map_or(0, |index| index + 1)
}

/// Re-wrap one logical line's content into `cols`-wide rows.
///
/// `abcdef` at `cols = 4` → `abcd` ([`RowEnd::Soft`]) then `ef`
/// ([`RowEnd::Hard`]). A wide glyph whose base would land in a row's last
/// column gets a blank spacer there and starts the next row whole
/// ([`RowEnd::SoftWide`]) — the same rule as the live print path — and in a
/// one-column screen a wide glyph stores narrow, matching `place_glyph`.
fn rewrap_line(content: &[Cell], cols: u16, fill: Style) -> Vec<(Vec<Cell>, RowEnd)> {
    let cols = cols.max(1) as usize;
    let mut rows: Vec<(Vec<Cell>, RowEnd)> = Vec::new();
    let mut row: Vec<Cell> = Vec::new();
    let mut index = 0;
    while index < content.len() {
        let cell = &content[index];
        if row.len() == cols {
            rows.push((std::mem::take(&mut row), RowEnd::Soft));
        }
        if cell.width() == 2 {
            if cols == 1 {
                // A wide pair can never fit one column; keep the base narrow
                // (the same rule as place_glyph) and skip its continuation.
                row.push(narrowed(cell));
                index += 1;
                if content.get(index).is_some_and(|next| next.width() == 0) {
                    index += 1;
                }
                continue;
            }
            if row.len() + 1 == cols {
                // The base would land in the last column, splitting the
                // pair: leave a spacer and retry the glyph on the next row.
                row.push(Cell::blank_with(fill));
                rows.push((std::mem::take(&mut row), RowEnd::SoftWide));
                continue;
            }
        }
        row.push(cell.clone());
        index += 1;
    }
    rows.push((row, RowEnd::Hard));
    rows
}

/// `cell` rebuilt one column wide, keeping its character, combining marks,
/// and style — for a wide base re-wrapped into a one-column screen.
fn narrowed(cell: &Cell) -> Cell {
    let mut out = Cell::new(cell.ch(), 1, cell.style());
    for mark in cell.combining() {
        out.push_combining(*mark);
    }
    out
}

/// The (row-within-line, column) where content offset `offset` lands among a
/// re-wrapped line's rows. An offset past the content parks in the final
/// row's padding; the caller clamps the column to the screen width.
fn locate_offset(rows: &[(Vec<Cell>, RowEnd)], offset: usize) -> (usize, usize) {
    let mut remaining = offset;
    for (index, (row, end)) in rows.iter().enumerate() {
        let contributed = match end {
            RowEnd::Soft => row.len(),
            RowEnd::SoftWide => row.len().saturating_sub(1),
            RowEnd::Hard => row.len(),
        };
        if remaining < contributed || index + 1 == rows.len() {
            return (index, remaining);
        }
        remaining -= contributed;
    }
    (0, 0)
}

#[cfg(test)]
mod tests;
