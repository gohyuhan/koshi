//! Reading a pane's text as one continuous space, and growing a selection to
//! whole words or whole lines within it.
//!
//! # The row numbering
//!
//! A pane's text lives in two places: the [`Scrollback`] holds the lines that
//! have scrolled off the top, and the [`Grid`] holds the live screen. A
//! selection spans both, so it needs one row number that means the same thing in
//! either.
//!
//! That number is **absolute**: it counts every line the pane has ever pushed
//! into its scrollback. The live screen's top row is line number
//! [`Scrollback::total_pushed`] — the number the line will have once it scrolls
//! off — and every row below it counts up from there. History rows count back
//! down from it.
//!
//! ```text
//!   total_pushed = 1000, scrollback retains 500, screen is 24 rows
//!
//!   row  500  ─┐
//!    ...       ├─ scrollback (rows 500..=999)
//!   row  999  ─┘
//!   row 1000  ─┐
//!    ...       ├─ live screen (rows 1000..=1023)
//!   row 1023  ─┘
//! ```
//!
//! The point of counting this way is that **the number never changes meaning**.
//! Ten lines of output arrive: `total_pushed` becomes 1010, the live screen's
//! top row is 1010, and the line that was row 1000 is *still* row 1000 — it just
//! lives in history now. The cap drops the ten oldest: the first reachable row
//! becomes 510, and every surviving line kept its number. So a selection is
//! stored once and never re-anchored; a row that has been dropped is out of
//! [`TextView::first_row`]`..=`[`TextView::last_row`] and reads as [`None`],
//! which is a question about what still exists rather than about what the number
//! means.
//!
//! # Word boundaries
//!
//! A double-click grows the selection to a whole "word", but a terminal's idea
//! of a word is not an editor's. [`WORD_SEPARATORS`] deliberately leaves out
//! `/`, `.`, `-`, and `_`, so double-clicking `/usr/local/bin` selects the whole
//! path and `foo.tar.gz` comes out whole — paths and URLs are what people
//! actually double-click in a terminal.

use std::collections::VecDeque;

use koshi_core::command::GridPos;

use crate::grid::state::{Cell, Grid, RowEnd};
use crate::scrollback::Scrollback;

/// The characters that end a word for a double-click selection.
///
/// Whitespace, quotes, brackets, and the shell's own punctuation stop a word;
/// `/`, `.`, `-`, and `_` do not, so a path, a URL, or a dotted filename is one
/// word. Double-clicking `local` in `/usr/local/bin` selects `/usr/local/bin`;
/// double-clicking inside `(foo bar)` selects `foo` alone, because the space and
/// the parentheses are separators. Double-clicking a separator itself selects
/// the run of that same character — the two spaces in `foo  bar`, not the words
/// around them.
pub const WORD_SEPARATORS: &str = ",│`|:\"' ()[]{}<>\t";

/// One pane's text — its scrollback history and its live screen — addressed by
/// absolute row number. See the module docs for what the numbering means.
///
/// This is a borrowed view, built per read; it copies nothing.
#[derive(Debug, Clone, Copy)]
pub struct TextView<'a> {
    /// Retained history rows, oldest first, or [`None`] for a screen that keeps
    /// no history of its own.
    history: Option<&'a VecDeque<(Vec<Cell>, RowEnd)>>,
    /// The live screen.
    grid: &'a Grid,
    /// The absolute row number of the live screen's top row.
    top: u64,
}

impl<'a> TextView<'a> {
    /// A view over `grid` as the live screen with `scrollback` as its history —
    /// the primary screen.
    ///
    /// **Only for the primary screen.** The alternate screen keeps no history of
    /// its own while `scrollback` still holds the *primary's*, so pairing the two
    /// would let a walk step off the alternate's top row into text from another
    /// screen entirely. Use [`screen_only`](Self::screen_only) there —
    /// [`TerminalState::text_view`] picks the right one.
    ///
    /// [`TerminalState::text_view`]: crate::state::TerminalState::text_view
    #[must_use]
    pub fn new(scrollback: &'a Scrollback, grid: &'a Grid) -> Self {
        TextView {
            history: Some(scrollback.lines()),
            grid,
            top: scrollback.total_pushed(),
        }
    }

    /// A view over `grid` alone, with no history above it — the alternate
    /// screen, whose rows are only its own.
    ///
    /// `top` is the absolute row number its first row takes, so positions
    /// resolved here and on the primary agree on what a row number means.
    #[must_use]
    pub fn screen_only(grid: &'a Grid, top: u64) -> Self {
        TextView {
            history: None,
            grid,
            top,
        }
    }

    /// How many history rows sit above the live screen; `0` when it has none.
    fn history_len(&self) -> usize {
        self.history.map_or(0, VecDeque::len)
    }

    /// The oldest row still readable: the top of retained history, or the top of
    /// the live screen when there is none.
    ///
    /// Saturating, so a history longer than the count of lines ever pushed — a
    /// resize reflow rebuilds history wholesale and grows that count only by the
    /// rows it added — reads as row `0` rather than wrapping.
    #[must_use]
    pub fn first_row(&self) -> u64 {
        self.top.saturating_sub(self.history_len() as u64)
    }

    /// The newest row: the bottom of the live screen.
    #[must_use]
    pub fn last_row(&self) -> u64 {
        let (rows, _) = self.grid.dimensions();
        self.top + u64::from(rows.saturating_sub(1))
    }

    /// The number of columns every row has.
    #[must_use]
    pub fn cols(&self) -> u16 {
        let (_, cols) = self.grid.dimensions();
        cols
    }

    /// The cells of row `row` and how it ended, or `None` if that row has been
    /// dropped from history or is past the bottom of the live screen.
    #[must_use]
    pub fn row(&self, row: u64) -> Option<(&'a [Cell], RowEnd)> {
        if row < self.top {
            // A history row: index back from the newest, which sits just above
            // the live screen's top row.
            let history = self.history?;
            let from_top = self.top - row;
            let index = history.len().checked_sub(from_top as usize)?;
            let (cells, end) = history.get(index)?;
            Some((cells.as_slice(), *end))
        } else {
            let grid_row = u16::try_from(row - self.top).ok()?;
            let (rows, _) = self.grid.dimensions();
            if grid_row >= rows {
                return None;
            }
            let cells = self.grid.rows().get(grid_row as usize)?;
            Some((cells.as_slice(), self.grid.row_end(grid_row)))
        }
    }

    /// The cell at `row`/`col`, or `None` if the row is gone or the column is
    /// past the end of it.
    #[must_use]
    pub fn cell(&self, row: u64, col: u16) -> Option<&'a Cell> {
        let (cells, _) = self.row(row)?;
        cells.get(col as usize)
    }

    /// Whether `row` continues onto the row below it because the text wrapped
    /// rather than because a new line started.
    ///
    /// A `hello world` that wrapped mid-word across two rows is one logical
    /// line; two separate `echo` outputs are two. Word and line selections both
    /// follow the text across a soft wrap and stop at a hard one.
    #[must_use]
    pub fn wraps(&self, row: u64) -> bool {
        self.row(row)
            .is_some_and(|(_, end)| matches!(end, RowEnd::Soft | RowEnd::SoftWide))
    }

    /// The first row of the logical line containing `row`: walk up while the row
    /// above wrapped into this one.
    ///
    /// `ls` printing one long filename that wrapped over rows 10, 11, and 12:
    /// `line_start(11)` is `10`.
    #[must_use]
    pub fn line_start(&self, row: u64) -> u64 {
        let mut start = row;
        while start > self.first_row() && self.wraps(start - 1) {
            start -= 1;
        }
        start
    }

    /// The last row of the logical line containing `row`: walk down while this
    /// row wraps into the next.
    ///
    /// For the wrapped filename above, `line_end(11)` is `12`.
    #[must_use]
    pub fn line_end(&self, row: u64) -> u64 {
        let mut end = row;
        while end < self.last_row() && self.wraps(end) {
            end += 1;
        }
        end
    }

    /// Whether the cell at `row`/`col` ends a word.
    ///
    /// A cell holding one of [`WORD_SEPARATORS`] ends a word, and so does a row
    /// short enough that the column is past its end — there is no text there to
    /// be part of one.
    fn is_separator(&self, row: u64, col: u16) -> bool {
        self.cell(row, col)
            .is_none_or(|cell| WORD_SEPARATORS.contains(cell.ch()))
    }

    /// The cell before `pos` in reading order, crossing a soft wrap to the end of
    /// the row above, or `None` at the very start of the text.
    ///
    /// Width-0 cells are skipped: they are the blank right halves of wide
    /// (CJK/emoji) glyphs, and the glyph's text lives entirely in its left half,
    /// so stopping on one would split the glyph.
    fn prev_cell(&self, row: u64, col: u16) -> Option<(u64, u16)> {
        let (mut row, mut col) = (row, col);
        loop {
            if col > 0 {
                col -= 1;
            } else if row > self.first_row() && self.wraps(row - 1) {
                row -= 1;
                col = self.cols().saturating_sub(1);
            } else {
                return None;
            }
            if self.cell(row, col).is_none_or(|cell| cell.width() != 0) {
                return Some((row, col));
            }
        }
    }

    /// The cell after `pos` in reading order, crossing a soft wrap to the start
    /// of the row below, or `None` at the very end of the text. Skips the blank
    /// right halves of wide glyphs, as [`prev_cell`](Self::prev_cell) does.
    fn next_cell(&self, row: u64, col: u16) -> Option<(u64, u16)> {
        let (mut row, mut col) = (row, col);
        loop {
            if col + 1 < self.cols() {
                col += 1;
            } else if row < self.last_row() && self.wraps(row) {
                row += 1;
                col = 0;
            } else {
                return None;
            }
            if self.cell(row, col).is_none_or(|cell| cell.width() != 0) {
                return Some((row, col));
            }
        }
    }

    /// The separator character at `row`/`col`, or `None` when the cell holds
    /// part of a word, is the width-0 half of a wide glyph (the glyph's own
    /// cell is the text there), or holds nothing.
    fn separator_char(&self, row: u64, col: u16) -> Option<char> {
        self.cell(row, col)
            .filter(|cell| cell.width() != 0)
            .map(|cell| cell.ch())
            .filter(|ch| WORD_SEPARATORS.contains(*ch))
    }

    /// The start of the word at `row`/`col`: step left while the cell there is
    /// part of a word, and stop on the last one that was.
    ///
    /// `cargo build` with the pointer on the `i` of `build`: walking left hits
    /// the space after `cargo`, which is a separator, so the word starts at the
    /// `b`.
    ///
    /// Starting ON a separator, the "word" is the run of that same character:
    /// the space in `foo  bar` grows over the two spaces, never into `foo`, and
    /// `(` next to `)` stays alone — each separator is its own run.
    #[must_use]
    pub fn word_start(&self, row: u64, col: u16) -> (u64, u16) {
        let run = self.separator_char(row, col);
        let (mut row, mut col) = (row, col);
        while let Some((prev_row, prev_col)) = self.prev_cell(row, col) {
            let stop = match run {
                Some(ch) => self
                    .cell(prev_row, prev_col)
                    .is_none_or(|cell| cell.ch() != ch),
                None => self.is_separator(prev_row, prev_col),
            };
            if stop {
                break;
            }
            row = prev_row;
            col = prev_col;
        }
        (row, col)
    }

    /// The end of the word at `row`/`col`: the mirror of
    /// [`word_start`](Self::word_start), stepping right — including the
    /// separator-run rule for a start cell that is itself a separator.
    #[must_use]
    pub fn word_end(&self, row: u64, col: u16) -> (u64, u16) {
        let run = self.separator_char(row, col);
        let (mut row, mut col) = (row, col);
        while let Some((next_row, next_col)) = self.next_cell(row, col) {
            let stop = match run {
                Some(ch) => self
                    .cell(next_row, next_col)
                    .is_none_or(|cell| cell.ch() != ch),
                None => self.is_separator(next_row, next_col),
            };
            if stop {
                break;
            }
            row = next_row;
            col = next_col;
        }
        (row, col)
    }
}

/// A selection's two ends put into text order — `start` never comes after `end`.
///
/// A drag stores where it began and where the pointer is, in that order, so
/// dragging up or leftward leaves the two ends reversed. Anything that reads the
/// text under a selection wants them the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ordered {
    /// The earlier end.
    pub start: GridPos,
    /// The later end.
    pub end: GridPos,
}

/// `anchor` and `cursor` in text order: earlier row first, and within one row,
/// earlier column first. Both ends are inclusive.
#[must_use]
pub fn order(anchor: GridPos, cursor: GridPos) -> Ordered {
    let key = |pos: &GridPos| (pos.row, pos.col);
    if key(&anchor) <= key(&cursor) {
        Ordered {
            start: anchor,
            end: cursor,
        }
    } else {
        Ordered {
            start: cursor,
            end: anchor,
        }
    }
}

#[cfg(test)]
mod tests;
