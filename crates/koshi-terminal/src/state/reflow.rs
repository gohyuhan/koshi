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

use crate::grid::state::{content_len, Cell, Grid, RowEnd, RowMeta};
use crate::style::Style;

use super::{rebuilt_with_width, TerminalState};

impl TerminalState {
    /// Rebuild the primary screen and its scrollback for `size` by re-wrapping
    /// every logical line to the new width. The cursor stays on its logical
    /// line at its content offset, clamped to the new screen. Rows past the
    /// new height scroll into history; trailing rows below the cursor that
    /// end hard and hold only default blanks are dropped first. A zero-row
    /// size parks every row in history, and the next non-zero reflow puts the
    /// cursor on the first logical line.
    pub(super) fn reflow_primary(&mut self, size: PtySize) {
        let fill = self.primary_render.style.bg_fill();

        // Every physical row: history first (oldest at the front, taken out
        // of the scrollback buffer), then the live screen, each with its row
        // metadata.
        let mut physical: Vec<(Vec<Cell>, RowMeta)> = Vec::from(self.scrollback.take_lines());
        let history_len = physical.len();
        for (index, row) in self.primary.rows().iter().enumerate() {
            physical.push((row.clone(), self.primary.row_meta(index as u16)));
        }
        let cursor_physical = history_len + self.primary_cursor.row as usize;
        let cursor_col = self.primary_cursor.col as usize;

        // Unwind into logical lines, tracking which line holds the cursor and
        // where each prompt-marked row begins in that line.
        let mut lines: Vec<(Vec<Cell>, Vec<usize>)> = Vec::new();
        let mut current: Vec<Cell> = Vec::new();
        let mut prompt_offsets: Vec<usize> = Vec::new();
        let mut cursor_line = 0_usize;
        let mut cursor_offset = 0_usize;
        for (index, (row, meta)) in physical.into_iter().enumerate() {
            let contributed = match meta.end {
                // A soft-wrapped row is full: every cell is content.
                RowEnd::Soft => row.len(),
                // Its trailing blank is a spacer standing in for the wide
                // glyph that starts the next row; the glyph is the content.
                RowEnd::SoftWide => row.len().saturating_sub(1),
                // Trailing fully-default blanks are padding, not text.
                RowEnd::Hard => content_len(&row),
            };
            if meta.prompt {
                prompt_offsets.push(current.len());
            }
            if index == cursor_physical {
                cursor_line = lines.len();
                // On a hard-ended row the cursor's column counts in full,
                // padding included; on a soft row it is capped at the row's
                // content.
                cursor_offset = current.len()
                    + if meta.end == RowEnd::Hard {
                        cursor_col
                    } else {
                        min(cursor_col, contributed)
                    };
            }
            current.extend(row.into_iter().take(contributed));
            if meta.end == RowEnd::Hard {
                lines.push((
                    std::mem::take(&mut current),
                    std::mem::take(&mut prompt_offsets),
                ));
            }
        }
        // A trailing soft-wrapped row with no hard end below it still forms a
        // line, as does a prompt mark left on an empty tail.
        if !current.is_empty() || !prompt_offsets.is_empty() {
            lines.push((current, prompt_offsets));
        }

        // Re-wrap every logical line to the new width and move each prompt
        // mark to the new row containing the marked row's first content cell.
        let mut rewrapped: Vec<(Vec<Cell>, RowMeta)> = Vec::new();
        let mut new_cursor_physical = 0_usize;
        let mut new_cursor_col = 0_usize;
        for (index, (content, prompt_offsets)) in lines.into_iter().enumerate() {
            let start = rewrapped.len();
            let mut rows = rewrap_line(content, size.cols, fill);
            if index == cursor_line {
                let (row_in_line, col) = locate_offset(&rows, cursor_offset);
                new_cursor_physical = start + row_in_line;
                new_cursor_col = col;
            }
            for offset in prompt_offsets {
                let (row_in_line, _) = locate_offset(&rows, offset);
                rows[row_in_line].1.prompt = true;
            }
            rewrapped.extend(rows);
        }
        if rewrapped.is_empty() {
            rewrapped.push((Vec::new(), RowMeta::default()));
        }

        // Drop trailing rows below the cursor that end hard and hold only
        // default blanks, down to the screen height. A styled blank row and a
        // blank row carrying a prompt mark each count as content and stay.
        while rewrapped.len() > size.rows as usize
            && rewrapped.len() > new_cursor_physical + 1
            && rewrapped.last().is_some_and(|(row, meta)| {
                meta.end == RowEnd::Hard && !meta.prompt && content_len(row) == 0
            })
        {
            rewrapped.pop();
        }

        // Rows past the screen's height scroll into history, oldest first;
        // the rest — padded with blanks at the bottom — is the new screen.
        let overflow = rewrapped.len().saturating_sub(size.rows as usize);
        let history: Vec<(Vec<Cell>, RowMeta)> = rewrapped.drain(..overflow).collect();
        self.scrollback.replace_lines(history, history_len as u64);

        while rewrapped.len() < size.rows as usize {
            rewrapped.push((
                vec![Cell::blank_with(fill); size.cols as usize],
                RowMeta::default(),
            ));
        }
        self.primary = Arc::new(Grid::from_rows_with_meta(rewrapped, size.cols, fill));

        self.primary_cursor.row = min(
            new_cursor_physical.saturating_sub(overflow),
            size.rows.saturating_sub(1) as usize,
        ) as u16;
        self.primary_cursor.col = min(new_cursor_col, size.cols.saturating_sub(1) as usize) as u16;
    }
}

/// Re-wrap one logical line's content into `cols`-wide rows. `cols` of `0`
/// wraps at one column. Empty content gives one empty [`RowEnd::Hard`] row.
/// Every row's `prompt` is `false`.
///
/// `abcdef` at `cols = 4` → `abcd` ([`RowEnd::Soft`]) then `ef`
/// ([`RowEnd::Hard`]). A wide glyph whose base would land in a row's last
/// column leaves a blank spacer in `fill` there and starts the next row whole
/// ([`RowEnd::SoftWide`]). At one column a wide glyph is stored narrow and its
/// width-0 continuation cell is skipped.
fn rewrap_line(content: Vec<Cell>, cols: u16, fill: Style) -> Vec<(Vec<Cell>, RowMeta)> {
    let cols = cols.max(1) as usize;
    let mut rows: Vec<(Vec<Cell>, RowMeta)> = Vec::new();
    let mut row: Vec<Cell> = Vec::with_capacity(cols.min(content.len()));
    let mut cells = content.into_iter().peekable();
    while let Some(cell) = cells.next() {
        if row.len() == cols {
            let full = std::mem::replace(&mut row, Vec::with_capacity(cols.min(cells.len() + 1)));
            rows.push((
                full,
                RowMeta {
                    end: RowEnd::Soft,
                    prompt: false,
                },
            ));
        }
        if cell.width() == 2 {
            if cols == 1 {
                // Store the base narrow and skip its width-0 continuation.
                row.push(rebuilt_with_width(&cell, 1));
                if cells.peek().is_some_and(|next| next.width() == 0) {
                    cells.next();
                }
                continue;
            }
            if row.len() + 1 == cols {
                // The base would land in the last column: fill it with a
                // spacer, end the row `SoftWide`, and start the next row
                // with the glyph.
                row.push(Cell::blank_with(fill));
                let full =
                    std::mem::replace(&mut row, Vec::with_capacity(cols.min(cells.len() + 1)));
                rows.push((
                    full,
                    RowMeta {
                        end: RowEnd::SoftWide,
                        prompt: false,
                    },
                ));
            }
        }
        row.push(cell);
    }
    rows.push((
        row,
        RowMeta {
            end: RowEnd::Hard,
            prompt: false,
        },
    ));
    rows
}

/// The (row-within-line, column) where content offset `offset` lands among a
/// re-wrapped line's rows. A [`RowEnd::SoftWide`] row's spacer holds no
/// offset. An offset past the content lands in the final row at a column
/// past its cells, not clamped to the screen width. Empty `rows` gives
/// `(0, 0)`.
fn locate_offset(rows: &[(Vec<Cell>, RowMeta)], offset: usize) -> (usize, usize) {
    let mut remaining = offset;
    for (index, (row, meta)) in rows.iter().enumerate() {
        let contributed = match meta.end {
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
