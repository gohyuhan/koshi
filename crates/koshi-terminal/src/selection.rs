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
//! [`Scrollback::get_total_pushed_line_count`] — the number the line takes once it scrolls
//! off — and every row below it counts up from there. History rows count back
//! down from it.
//!
//! ```text
//!   total_pushed_line_count = 1000, scrollback retains 500, screen is 24 rows
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
//! `total_pushed_line_count` becomes 1010, the live screen's top row is 1010, and the line
//! that was row 1000 is *still* row 1000, in history now. The cap drops the ten
//! oldest: the first reachable row becomes 510, and every surviving line keeps
//! its number. A selection is stored once and never re-anchored. A dropped row
//! falls outside [`TextView::get_first_row_index`]..=[`TextView::get_last_row_index`] and reads
//! as [`None`].
//!
//! # Word boundaries
//!
//! A double-click grows the selection to a whole "word". The separator set
//! leaves out `/`, `.`, `-`, and `_`: double-clicking `/usr/local/bin` selects
//! the whole path, and `foo.tar.gz` comes out whole.

use std::collections::VecDeque;
use std::sync::LazyLock;

use koshi_core::command::{GridPosition, Selection, SelectionKind};

use crate::grid::state::{Cell, Grid, RowEnd, RowMetadata};
use crate::scrollback::Scrollback;

/// The cell a column past a stored row's end reads as: a default blank.
///
/// History keeps a row's text without the default blanks that padded it out to
/// the screen width. This cell stands in for each of those dropped blanks.
static DEFAULT_PADDING_CELL: LazyLock<Cell> = LazyLock::new(Cell::blank);

/// The cell at `col` of `cells`, treating a row shorter than `cols` as if the
/// blanks trimmed off its end were still there. `None` past `cols`, where the
/// screen itself ends.
///
/// A history row holding `hi` on an 80-column screen answers `h`, `i`, then a
/// blank for columns 2 through 79, then `None`.
fn get_cell_or_padding(cells: &[Cell], column_index: u16, column_count: u16) -> Option<&Cell> {
    match cells.get(column_index as usize) {
        Some(cell) => Some(cell),
        None if column_index < column_count => Some(&DEFAULT_PADDING_CELL),
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
pub(crate) const WORD_SEPARATORS: &str = ",│`|:\"' ()[]{}<>\t";

/// One pane's text — its scrollback history and its live screen — addressed by
/// absolute row number. See the module docs for what the numbering means.
///
/// This is a borrowed view, built per read; it copies nothing.
#[derive(Debug, Clone, Copy)]
pub struct TextView<'a> {
    /// Retained history rows, oldest first, or [`None`] for a screen that keeps
    /// no history of its own.
    history: Option<&'a VecDeque<(Vec<Cell>, RowMetadata)>>,
    /// The live screen.
    grid: &'a Grid,
    /// The absolute row number of the live screen's top row.
    live_screen_top_row_number: u64,
}

impl<'a> TextView<'a> {
    /// A view over `grid` as the live screen with `scrollback` as its history —
    /// the primary screen.
    ///
    /// **Only for the primary screen.** The alternate screen keeps no history of
    /// its own while `scrollback` still holds the *primary's*;
    /// [`TerminalState::get_text_view`] builds the right view for each screen.
    ///
    /// [`TerminalState::get_text_view`]: crate::state::TerminalState::get_text_view
    #[must_use]
    pub fn from_scrollback_and_grid(scrollback: &'a Scrollback, grid: &'a Grid) -> Self {
        TextView {
            history: Some(scrollback.list_retained_lines()),
            grid,
            live_screen_top_row_number: scrollback.get_total_pushed_line_count(),
        }
    }

    /// A view over `grid` alone, with no history above it — the alternate
    /// screen, whose rows are only its own.
    ///
    /// `live_screen_top_row_number` is the absolute row number its first row
    /// takes. Positions resolved here and on the primary agree on what a row
    /// number means.
    #[must_use]
    pub(crate) fn from_grid_without_scrollback(
        grid: &'a Grid,
        live_screen_top_row_number: u64,
    ) -> Self {
        TextView {
            history: None,
            grid,
            live_screen_top_row_number,
        }
    }

    /// How many history rows sit above the live screen; `0` when it has none.
    fn get_history_row_count(&self) -> usize {
        self.history.map_or(0, VecDeque::len)
    }

    /// The oldest row still readable: the top of retained history, or the top of
    /// the live screen when there is none.
    ///
    /// Saturates at row `0` when history holds more rows than the count of lines
    /// ever pushed. A resize reflow rebuilds history wholesale and grows that
    /// count only by the rows it added.
    #[must_use]
    pub fn get_first_row_index(&self) -> u64 {
        self.live_screen_top_row_number
            .saturating_sub(self.get_history_row_count() as u64)
    }

    /// The newest row: the bottom of the live screen.
    #[must_use]
    pub fn get_last_row_index(&self) -> u64 {
        let (row_count, _) = self.grid.get_grid_dimensions();
        self.live_screen_top_row_number + u64::from(row_count.saturating_sub(1))
    }

    /// The number of columns every row has.
    #[must_use]
    pub fn get_column_count(&self) -> u16 {
        let (_, column_count) = self.grid.get_grid_dimensions();
        column_count
    }

    /// The cells of row `row` and how it ended, or `None` if that row has been
    /// dropped from history or is past the bottom of the live screen.
    #[must_use]
    pub fn get_row(&self, row_index: u64) -> Option<(&'a [Cell], RowEnd)> {
        if row_index < self.live_screen_top_row_number {
            // A history row: index back from the newest, which sits just above
            // the live screen's top row.
            let history = self.history?;
            let from_top = self.live_screen_top_row_number - row_index;
            let history_row_index = history.len().checked_sub(usize::try_from(from_top).ok()?)?;
            let (row_cells, row_metadata) = history.get(history_row_index)?;
            Some((row_cells.as_slice(), row_metadata.row_end))
        } else {
            let grid_row_index = u16::try_from(row_index - self.live_screen_top_row_number).ok()?;
            let (row_count, _) = self.grid.get_grid_dimensions();
            if grid_row_index >= row_count {
                return None;
            }
            let cells = self.grid.list_rows().get(grid_row_index as usize)?;
            Some((cells.as_slice(), self.grid.get_row_end(grid_row_index)))
        }
    }

    /// The cell at `row`/`col`, or `None` if the row is gone or the column is
    /// past the screen width.
    ///
    /// A history row is stored without the default blanks that padded it out to
    /// the screen width. A column right of its text reads as one of those
    /// blanks. Every column of every live row addresses a stored cell.
    #[must_use]
    pub fn get_cell(&self, row_index: u64, column_index: u16) -> Option<&'a Cell> {
        let (cells, _) = self.get_row(row_index)?;
        get_cell_or_padding(cells, column_index, self.get_column_count())
    }

    /// Whether `row` soft-wrapped into the row below it: the two rows hold one
    /// logical line.
    ///
    /// A `hello world` that wrapped mid-word across two rows is one logical
    /// line; two separate `echo` outputs are two. Word and line selections both
    /// follow the text across a soft wrap and stop at a hard one.
    #[must_use]
    pub fn is_soft_wrapped(&self, row_index: u64) -> bool {
        self.get_row(row_index)
            .is_some_and(|(_, end)| matches!(end, RowEnd::Soft | RowEnd::SoftWide))
    }

    /// Whether `row`/`col` is the blank last-column spacer left when a wide
    /// glyph wrapped whole onto the next row. The spacer carries layout only;
    /// it is not selectable text.
    #[must_use]
    pub fn is_wide_wrap_spacer(&self, row_index: u64, column_index: u16) -> bool {
        self.get_row(row_index).is_some_and(|(cells, end)| {
            end == RowEnd::SoftWide && usize::from(column_index) + 1 == cells.len()
        })
    }

    /// The first row of the logical line containing `row`: walk up while the row
    /// above wrapped into this one.
    ///
    /// `ls` printing one long filename that wrapped over rows 10, 11, and 12:
    /// `line_start(11)` is `10`.
    #[must_use]
    pub fn get_line_start_row_index(&self, row_index: u64) -> u64 {
        let mut start_row_index = row_index;
        while start_row_index > self.get_first_row_index()
            && self.is_soft_wrapped(start_row_index - 1)
        {
            start_row_index -= 1;
        }
        start_row_index
    }

    /// The last row of the logical line containing `row`: walk down while this
    /// row wraps into the next.
    ///
    /// For the wrapped filename above, `line_end(11)` is `12`.
    #[must_use]
    pub fn get_line_end_row_index(&self, row_index: u64) -> u64 {
        let mut end_row_index = row_index;
        while end_row_index < self.get_last_row_index() && self.is_soft_wrapped(end_row_index) {
            end_row_index += 1;
        }
        end_row_index
    }

    /// Whether the cell at `row`/`col` ends a word.
    ///
    /// A cell holding one of [`WORD_SEPARATORS`] ends a word, and so does a
    /// column past the screen width. A column right of a history line's text
    /// reads as a blank, and a blank is itself a separator.
    fn is_separator(&self, row_index: u64, column_index: u16) -> bool {
        self.get_cell(row_index, column_index)
            .is_none_or(|cell| WORD_SEPARATORS.contains(cell.get_character()))
    }

    /// Whether `row`/`col` holds layout rather than text: the blank width-0
    /// right half of a wide (CJK/emoji) glyph, whose text lives entirely in its
    /// left half, or the spacer of [`is_wide_wrap_spacer`](Self::is_wide_wrap_spacer).
    /// A gone row or a column past the screen width is not layout.
    fn is_layout_cell(&self, row_index: u64, column_index: u16) -> bool {
        self.is_wide_wrap_spacer(row_index, column_index)
            || self
                .get_cell(row_index, column_index)
                .is_some_and(|cell| cell.get_display_width() == 0)
    }

    /// The cell before `row`/`col` in reading order, crossing a soft wrap to the
    /// end of the row above, or `None` at the very start of the text.
    ///
    /// Layout cells are skipped: one step crosses a whole wide glyph.
    fn find_previous_text_cell(&self, row_index: u64, column_index: u16) -> Option<(u64, u16)> {
        let (mut row_index, mut column_index) = (row_index, column_index);
        loop {
            if column_index > 0 {
                column_index -= 1;
            } else if row_index > self.get_first_row_index() && self.is_soft_wrapped(row_index - 1)
            {
                row_index -= 1;
                column_index = self.get_column_count().saturating_sub(1);
            } else {
                return None;
            }
            if !self.is_layout_cell(row_index, column_index) {
                return Some((row_index, column_index));
            }
        }
    }

    /// The cell after `row`/`col` in reading order, crossing a soft wrap to the
    /// start of the row below, or `None` at the very end of the text. Skips
    /// layout cells, as [`find_previous_text_cell`](Self::find_previous_text_cell) does.
    fn find_next_text_cell(&self, row_index: u64, column_index: u16) -> Option<(u64, u16)> {
        let (mut row_index, mut column_index) = (row_index, column_index);
        loop {
            if column_index < self.get_column_count().saturating_sub(1) {
                column_index += 1;
            } else if row_index < self.get_last_row_index() && self.is_soft_wrapped(row_index) {
                row_index += 1;
                column_index = 0;
            } else {
                return None;
            }
            if !self.is_layout_cell(row_index, column_index) {
                return Some((row_index, column_index));
            }
        }
    }

    /// The separator character at `row`/`col`, or `None` when the cell holds
    /// part of a word, is the width-0 half of a wide glyph (the glyph's own
    /// cell is the text there), or holds nothing.
    ///
    /// A gone row and a column past the screen width read as `Some(' ')`, the
    /// same answer [`is_separator`](Self::is_separator) gives them.
    fn separator_char(&self, row_index: u64, column_index: u16) -> Option<char> {
        let Some(cell) = self.get_cell(row_index, column_index) else {
            return Some(' ');
        };
        if cell.get_display_width() == 0 {
            return None;
        }
        WORD_SEPARATORS
            .contains(cell.get_character())
            .then(|| cell.get_character())
    }

    /// Whether stepping onto `row`/`col` leaves the word being grown.
    ///
    /// Growing a separator run of `run`, the walk leaves it at any cell that
    /// does not hold that same character. Growing a word (`run` is `None`), the
    /// walk leaves it at a separator.
    fn is_word_boundary(
        &self,
        separator_run: Option<char>,
        row_index: u64,
        column_index: u16,
    ) -> bool {
        match separator_run {
            Some(separator_character) => self
                .get_cell(row_index, column_index)
                .is_none_or(|cell| cell.get_character() != separator_character),
            None => self.is_separator(row_index, column_index),
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
    pub fn get_word_start_position(&self, row_index: u64, column_index: u16) -> (u64, u16) {
        let separator_run = self.separator_char(row_index, column_index);
        let (mut row_index, mut column_index) = (row_index, column_index);
        while let Some((previous_row_index, previous_column_index)) =
            self.find_previous_text_cell(row_index, column_index)
        {
            if self.is_word_boundary(separator_run, previous_row_index, previous_column_index) {
                break;
            }
            row_index = previous_row_index;
            column_index = previous_column_index;
        }
        (row_index, column_index)
    }

    /// The end of the word at `row`/`col`: the mirror of
    /// [`word_start`](Self::get_word_start_position), stepping right — including the
    /// separator-run rule for a start cell that is itself a separator.
    #[must_use]
    pub fn get_word_end_position(&self, row_index: u64, column_index: u16) -> (u64, u16) {
        let separator_run = self.separator_char(row_index, column_index);
        let (mut row_index, mut column_index) = (row_index, column_index);
        while let Some((next_row_index, next_column_index)) =
            self.find_next_text_cell(row_index, column_index)
        {
            if self.is_word_boundary(separator_run, next_row_index, next_column_index) {
                break;
            }
            row_index = next_row_index;
            column_index = next_column_index;
        }
        (row_index, column_index)
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
/// [`TextView::get_first_row_index`]..=[`TextView::get_last_row_index`]. A selection reaching past
/// either end yields the text of the rows that are there, and one whose ends are
/// both outside yields the empty string. On a view holding rows 500..=1023, a
/// selection from row 0 to row `u64::MAX` reads rows 500 through 1023 and
/// nothing else.
#[must_use]
pub fn serialize_selection_text(
    view: &TextView<'_>,
    selection: &Selection,
    should_trim_trailing_whitespace: bool,
) -> String {
    let ordered_selection = order_selection_positions(selection.anchor, selection.cursor);
    let (selection_start, selection_end) = (
        ordered_selection.start_position,
        ordered_selection.end_position,
    );
    let column_count = view.get_column_count();
    let last_column_index = column_count.saturating_sub(1);
    let is_block_selection = matches!(selection.selection_kind, SelectionKind::Block);
    let mut selected_text = String::new();
    let mut has_written_row = false;
    let mut row_text = String::new();
    // Clamped to the rows the view holds: a selection can name any row number,
    // and only `first_row..=last_row` has text to read.
    for row_index in selection_start.row_index.max(view.get_first_row_index())
        ..=selection_end.row_index.min(view.get_last_row_index())
    {
        let Some((cells, row_end)) = view.get_row(row_index) else {
            continue;
        };
        let (first_column_index, last_selected_column_index) = if is_block_selection {
            (
                selection_start.column_index.min(selection_end.column_index),
                selection_start.column_index.max(selection_end.column_index),
            )
        } else {
            (
                if row_index == selection_start.row_index {
                    selection_start.column_index
                } else {
                    0
                },
                if row_index == selection_end.row_index {
                    selection_end.column_index
                } else {
                    last_column_index
                },
            )
        };
        if has_written_row && (is_block_selection || !view.is_soft_wrapped(row_index - 1)) {
            selected_text.push('\n');
        }
        has_written_row = true;
        row_text.clear();
        for column_index in first_column_index..=last_selected_column_index {
            let Some(cell) = get_cell_or_padding(cells, column_index, column_count) else {
                break;
            };
            // Skipped: the blank right half of a wide glyph, whose text lives
            // in its left half, and the spacer left in the last column when a
            // wide glyph wrapped whole onto the next row. Both are layout.
            let is_wrap_spacer =
                row_end == RowEnd::SoftWide && usize::from(column_index) + 1 == cells.len();
            if cell.get_display_width() == 0 || is_wrap_spacer {
                continue;
            }
            row_text.push(cell.get_character());
            row_text.extend(cell.list_combining_characters());
        }
        let is_soft_wrapped = matches!(row_end, RowEnd::Soft | RowEnd::SoftWide);
        if should_trim_trailing_whitespace && (is_block_selection || !is_soft_wrapped) {
            selected_text.push_str(row_text.trim_end());
        } else {
            selected_text.push_str(&row_text);
        }
    }
    selected_text
}

/// A selection's two ends put into text order — `start` never comes after `end`.
///
/// A drag stores where it began and where the pointer is, in that order, and a
/// drag up or leftward leaves the two ends reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedSelection {
    /// The earlier end.
    pub start_position: GridPosition,
    /// The end after the start.
    pub end_position: GridPosition,
}

/// `anchor` and `cursor` in text order: earlier row first, and within one row,
/// earlier column first. Both ends are inclusive. Two equal positions come back
/// as `anchor` then `cursor`.
#[must_use]
pub fn order_selection_positions(anchor: GridPosition, cursor: GridPosition) -> OrderedSelection {
    if (anchor.row_index, anchor.column_index) <= (cursor.row_index, cursor.column_index) {
        OrderedSelection {
            start_position: anchor,
            end_position: cursor,
        }
    } else {
        OrderedSelection {
            start_position: cursor,
            end_position: anchor,
        }
    }
}

#[cfg(test)]
mod tests;
