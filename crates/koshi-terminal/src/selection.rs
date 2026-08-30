//! Reading a pane's text as one continuous space, and growing a selection to
//! whole words or whole lines within it.
//!
//! # The row numbering
//!
//! A pane's text lives in two places: the [`Scrollback`] holds the lines that
//! have scrolled off the top, and the [`Grid`] holds the live screen. A
//! selection spans both and uses one row number that means the same thing in
//! either.
//!
//! That number is **absolute**: it counts every line the pane has ever pushed
//! into its scrollback. The live screen's top row is line number
//! [`Scrollback::total_pushed`] — the number the line takes once it scrolls
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
//! **The number never changes meaning.** Ten lines of output arrive:
//! `total_pushed` becomes 1010, the live screen's top row is 1010, and the line
//! that was row 1000 is *still* row 1000, in history now. The cap drops the ten
//! oldest: the first reachable row becomes 510, and every surviving line keeps
//! its number. A selection is stored once and never re-anchored. A dropped row
//! falls outside [`TextView::first_row`]`..=`[`TextView::last_row`] and reads
//! as [`None`].
//!
//! # Word boundaries
//!
//! A double-click grows the selection to a whole "word". [`WORD_SEPARATORS`]
//! leaves out `/`, `.`, `-`, and `_`: double-clicking `/usr/local/bin` selects
//! the whole path, and `foo.tar.gz` comes out whole.

use std::collections::VecDeque;
use std::sync::LazyLock;

use koshi_core::command::{GridPos, Selection, SelectionKind};

use crate::grid::state::{Cell, Grid, RowEnd, RowMeta};
use crate::scrollback::Scrollback;

/// The cell a column past a stored row's end reads as: a default blank.
///
/// History keeps a row's text without the default blanks that padded it out to
/// the screen width. This cell stands in for each of those dropped blanks.
static PADDING: LazyLock<Cell> = LazyLock::new(Cell::blank);

/// The cell at `col` of `cells`, treating a row shorter than `cols` as if the
/// blanks trimmed off its end were still there. `None` past `cols`, where the
/// screen itself ends.
///
/// A history row holding `hi` on an 80-column screen answers `h`, `i`, then a
/// blank for columns 2 through 79, then `None`.
fn cell_or_padding(cells: &[Cell], col: u16, cols: u16) -> Option<&Cell> {
    match cells.get(col as usize) {
        Some(cell) => Some(cell),
        None if col < cols => Some(&PADDING),
        None => None,
    }
}

/// The characters that end a word for a double-click selection.
///
/// Whitespace, quotes, brackets, and the shell's own punctuation stop a word;
/// `/`, `.`, `-`, and `_` do not: a path, a URL, or a dotted filename is one
/// word. Double-clicking `local` in `/usr/local/bin` selects `/usr/local/bin`;
/// double-clicking inside `(foo bar)` selects `foo` alone. Double-clicking a
/// separator itself selects the run of that same character — the two spaces in
/// `foo  bar`, not the words around them.
pub const WORD_SEPARATORS: &str = ",│`|:\"' ()[]{}<>\t";

/// One pane's text — its scrollback history and its live screen — addressed by
/// absolute row number. See the module docs for what the numbering means.
///
/// This is a borrowed view, built per read; it copies nothing.
#[derive(Debug, Clone, Copy)]
pub struct TextView<'a> {
    /// Retained history rows, oldest first, or [`None`] for a screen that keeps
    /// no history of its own.
    history: Option<&'a VecDeque<(Vec<Cell>, RowMeta)>>,
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
    /// its own while `scrollback` still holds the *primary's*. Use
    /// [`screen_only`](Self::screen_only) there; [`TerminalState::text_view`]
    /// picks the right one.
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
    /// `top` is the absolute row number its first row takes. Positions resolved
    /// here and on the primary agree on what a row number means.
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
    /// Saturates at row `0` when history holds more rows than the count of lines
    /// ever pushed. A resize reflow rebuilds history wholesale and grows that
    /// count only by the rows it added.
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
            let (cells, meta) = history.get(index)?;
            Some((cells.as_slice(), meta.end))
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
    /// past the screen width.
    ///
    /// A history row is stored without the default blanks that padded it out to
    /// the screen width. A column right of its text reads as one of those
    /// blanks. Every column of every live row addresses a stored cell.
    #[must_use]
    pub fn cell(&self, row: u64, col: u16) -> Option<&'a Cell> {
        let (cells, _) = self.row(row)?;
        cell_or_padding(cells, col, self.cols())
    }

    /// Whether `row` soft-wrapped into the row below it: the two rows hold one
    /// logical line.
    ///
    /// A `hello world` that wrapped mid-word across two rows is one logical
    /// line; two separate `echo` outputs are two. Word and line selections both
    /// follow the text across a soft wrap and stop at a hard one.
    #[must_use]
    pub fn wraps(&self, row: u64) -> bool {
        self.row(row)
            .is_some_and(|(_, end)| matches!(end, RowEnd::Soft | RowEnd::SoftWide))
    }

    /// Whether `row`/`col` is the blank last-column spacer left when a wide
    /// glyph wrapped whole onto the next row. The spacer carries layout only;
    /// it is not selectable text.
    #[must_use]
    pub fn is_wide_wrap_spacer(&self, row: u64, col: u16) -> bool {
        self.row(row).is_some_and(|(cells, end)| {
            end == RowEnd::SoftWide && usize::from(col) + 1 == cells.len()
        })
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
    /// A cell holding one of [`WORD_SEPARATORS`] ends a word, and so does a
    /// column past the screen width. A column right of a history line's text
    /// reads as a blank, and a blank is itself a separator.
    fn is_separator(&self, row: u64, col: u16) -> bool {
        self.cell(row, col)
            .is_none_or(|cell| WORD_SEPARATORS.contains(cell.ch()))
    }

    /// Whether `row`/`col` holds layout rather than text: the blank width-0
    /// right half of a wide (CJK/emoji) glyph, whose text lives entirely in its
    /// left half, or the spacer of [`is_wide_wrap_spacer`](Self::is_wide_wrap_spacer).
    /// A gone row or a column past the screen width is not layout.
    fn is_layout_cell(&self, row: u64, col: u16) -> bool {
        self.is_wide_wrap_spacer(row, col)
            || self.cell(row, col).is_some_and(|cell| cell.width() == 0)
    }

    /// The cell before `row`/`col` in reading order, crossing a soft wrap to the
    /// end of the row above, or `None` at the very start of the text.
    ///
    /// Layout cells are skipped: one step crosses a whole wide glyph.
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
            if !self.is_layout_cell(row, col) {
                return Some((row, col));
            }
        }
    }

    /// The cell after `row`/`col` in reading order, crossing a soft wrap to the
    /// start of the row below, or `None` at the very end of the text. Skips
    /// layout cells, as [`prev_cell`](Self::prev_cell) does.
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
            if !self.is_layout_cell(row, col) {
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

    /// Whether stepping onto `row`/`col` leaves the word being grown.
    ///
    /// Growing a separator run of `run`, the walk leaves it at any cell that
    /// does not hold that same character. Growing a word (`run` is `None`), the
    /// walk leaves it at a separator.
    fn ends_word(&self, run: Option<char>, row: u64, col: u16) -> bool {
        match run {
            Some(ch) => self.cell(row, col).is_none_or(|cell| cell.ch() != ch),
            None => self.is_separator(row, col),
        }
    }

    /// The start of the word at `row`/`col`: step left while the cell there is
    /// part of a word, and stop on the last one that was.
    ///
    /// `cargo build` with the pointer on the `i` of `build`: walking left hits
    /// the space after `cargo`, which is a separator, and the word starts at the
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
            if self.ends_word(run, prev_row, prev_col) {
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
            if self.ends_word(run, next_row, next_col) {
                break;
            }
            row = next_row;
            col = next_col;
        }
        (row, col)
    }
}

/// The text `selection` covers in `view`, as the string a copy places on the
/// clipboard.
///
/// Reading order, both ends inclusive. A soft wrap continues the line — no
/// newline — and a hard row end inserts `\n`: a wrapped `hello world` comes
/// out as one line and two `echo` outputs come out as two. A block takes the
/// same column range from every row and always joins with `\n`. The blank
/// right half of a wide glyph is skipped (the glyph's text lives in its left
/// half); combining marks ride along with their base. Every kind other than
/// `Block` reads the same cells; `Character`, `Word`, and `Line` differ only
/// in the ends the caller chose.
///
/// When `trim_trailing_whitespace` is true, trailing blanks are dropped from
/// each finished line, but not from a soft-wrapped row, whose spaces continue
/// onto the next row. When false, every selected blank is preserved.
///
/// Only the rows the view still holds are read:
/// [`TextView::first_row`]`..=`[`TextView::last_row`]. A selection reaching past
/// either end yields the text of the rows that are there, and one whose ends are
/// both outside yields the empty string. On a view holding rows 500..=1023, a
/// selection from row 0 to row `u64::MAX` reads rows 500 through 1023 and
/// nothing else.
#[must_use]
pub fn selection_text(
    view: &TextView<'_>,
    selection: &Selection,
    trim_trailing_whitespace: bool,
) -> String {
    let ordered = order(selection.anchor, selection.cursor);
    let (start, end) = (ordered.start, ordered.end);
    let cols = view.cols();
    let last_col = cols.saturating_sub(1);
    let block = matches!(selection.kind, SelectionKind::Block);
    let mut out = String::new();
    let mut any_row_written = false;
    let mut line = String::new();
    // Clamped to the rows the view holds: a selection can name any row number,
    // and only `first_row..=last_row` has text to read.
    for row in start.row.max(view.first_row())..=end.row.min(view.last_row()) {
        let Some((cells, row_end)) = view.row(row) else {
            continue;
        };
        let (from, to) = if block {
            (start.col.min(end.col), start.col.max(end.col))
        } else {
            (
                if row == start.row { start.col } else { 0 },
                if row == end.row { end.col } else { last_col },
            )
        };
        if any_row_written && (block || !view.wraps(row - 1)) {
            out.push('\n');
        }
        any_row_written = true;
        line.clear();
        for col in from..=to {
            let Some(cell) = cell_or_padding(cells, col, cols) else {
                break;
            };
            // Skipped: the blank right half of a wide glyph, whose text lives
            // in its left half, and the spacer left in the last column when a
            // wide glyph wrapped whole onto the next row. Both are layout.
            let wrap_spacer = row_end == RowEnd::SoftWide && usize::from(col) + 1 == cells.len();
            if cell.width() == 0 || wrap_spacer {
                continue;
            }
            line.push(cell.ch());
            line.extend(cell.combining());
        }
        let wraps = matches!(row_end, RowEnd::Soft | RowEnd::SoftWide);
        if trim_trailing_whitespace && (block || !wraps) {
            out.push_str(line.trim_end());
        } else {
            out.push_str(&line);
        }
    }
    out
}

/// A selection's two ends put into text order — `start` never comes after `end`.
///
/// A drag stores where it began and where the pointer is, in that order, and a
/// drag up or leftward leaves the two ends reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ordered {
    /// The earlier end.
    pub start: GridPos,
    /// The later end.
    pub end: GridPos,
}

/// `anchor` and `cursor` in text order: earlier row first, and within one row,
/// earlier column first. Both ends are inclusive. Two equal positions come back
/// as `anchor` then `cursor`.
#[must_use]
pub fn order(anchor: GridPos, cursor: GridPos) -> Ordered {
    if (anchor.row, anchor.col) <= (cursor.row, cursor.col) {
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
