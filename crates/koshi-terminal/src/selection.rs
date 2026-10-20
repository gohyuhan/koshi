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

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, LazyLock};

use koshi_core::command::{GridPosition, Selection, SelectionKind};

use crate::grid::row_snapshot::RowSnapshot;
use crate::grid::state::{Cell, Grid, RowEnd};
use crate::scrollback::Scrollback;

/// The cell a column past a row's last cell reads as: a default blank.
static DEFAULT_PADDING_CELL: LazyLock<Cell> = LazyLock::new(Cell::build_blank);

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
/// This is a borrowed view, built per read. It reads live rows in place. A
/// history row is decoded when a read reaches it, into one cell buffer that
/// the view keeps. A walk along one row decodes it once.
#[derive(Debug)]
pub struct TextView<'a> {
    /// Retained history rows, oldest first, or [`None`] for a screen that keeps
    /// no history of its own.
    scrollback_lines: Option<&'a VecDeque<Arc<RowSnapshot>>>,
    /// The live screen.
    screen_grid: &'a Grid,
    /// The absolute row number of the live screen's top row.
    live_screen_top_row_number: u64,
    /// The last history row a read decoded.
    decoded_history_row: RefCell<DecodedHistoryRow>,
}

/// The history row a [`TextView`] decoded last.
#[derive(Debug, Default)]
struct DecodedHistoryRow {
    /// Its absolute row number, or `None` before the first decode.
    row_index: Option<u64>,
    /// The cells of its runs: every cell but its trailing default blanks.
    row_cells: Vec<Cell>,
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
    pub fn from_scrollback_and_grid(scrollback: &'a Scrollback, screen_grid: &'a Grid) -> Self {
        TextView {
            scrollback_lines: Some(scrollback.list_retained_lines()),
            screen_grid,
            live_screen_top_row_number: scrollback.get_total_pushed_line_count(),
            decoded_history_row: RefCell::default(),
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
        screen_grid: &'a Grid,
        live_screen_top_row_number: u64,
    ) -> Self {
        TextView {
            scrollback_lines: None,
            screen_grid,
            live_screen_top_row_number,
            decoded_history_row: RefCell::default(),
        }
    }

    /// How many history rows sit above the live screen; `0` when it has none.
    fn get_scrollback_row_count(&self) -> usize {
        self.scrollback_lines.map_or(0, VecDeque::len)
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
            .saturating_sub(self.get_scrollback_row_count() as u64)
    }

    /// The newest row: the bottom of the live screen.
    #[must_use]
    pub fn get_last_row_index(&self) -> u64 {
        let (row_count, _) = self.screen_grid.get_grid_dimensions();
        self.live_screen_top_row_number + u64::from(row_count.saturating_sub(1))
    }

    /// The number of columns every row has.
    #[must_use]
    pub fn get_column_count(&self) -> u16 {
        let (_, column_count) = self.screen_grid.get_grid_dimensions();
        column_count
    }

    /// The history row at absolute row `row_index`, or `None` when the view
    /// keeps no history or history no longer holds that row.
    fn find_history_row(&self, row_index: u64) -> Option<&'a RowSnapshot> {
        // Index back from the newest history row, which sits just above the
        // live screen's top row.
        let scrollback_lines = self.scrollback_lines?;
        let scrollback_line_offset_from_top = self.live_screen_top_row_number - row_index;
        let scrollback_line_index = scrollback_lines
            .len()
            .checked_sub(usize::try_from(scrollback_line_offset_from_top).ok()?)?;
        scrollback_lines
            .get(scrollback_line_index)
            .map(|row_snapshot| &**row_snapshot)
    }

    /// The live screen row of absolute row `row_index`, or `None` past the
    /// bottom of the live screen. `row_index` is at or below the live
    /// screen's top row.
    fn find_live_row_index(&self, row_index: u64) -> Option<u16> {
        let grid_row_index = u16::try_from(row_index - self.live_screen_top_row_number).ok()?;
        let (row_count, _) = self.screen_grid.get_grid_dimensions();
        (grid_row_index < row_count).then_some(grid_row_index)
    }

    /// `read_row` applied to the cells of row `row_index` and how it ended, or
    /// `None` if that row has been dropped from history or is past the bottom
    /// of the live screen.
    ///
    /// A live row is read in place. A history row is decoded without its
    /// trailing default blanks, unless it is the history row the last read
    /// decoded. [`read_cell`](Self::read_cell) reads those blanks back.
    fn read_row<RowAnswer>(
        &self,
        row_index: u64,
        read_row: impl FnOnce(&[Cell], RowEnd) -> RowAnswer,
    ) -> Option<RowAnswer> {
        if row_index >= self.live_screen_top_row_number {
            let grid_row_index = self.find_live_row_index(row_index)?;
            let row_cells = self.screen_grid.get_row_cells(grid_row_index)?;
            return Some(read_row(
                row_cells,
                self.screen_grid.get_row_end(grid_row_index),
            ));
        }
        let row_snapshot = self.find_history_row(row_index)?;
        let mut decoded_history_row = self.decoded_history_row.borrow_mut();
        if decoded_history_row.row_index != Some(row_index) {
            decoded_history_row.row_cells.clear();
            row_snapshot.decode_row_run_cells_into(&mut decoded_history_row.row_cells);
            decoded_history_row.row_index = Some(row_index);
        }
        Some(read_row(
            &decoded_history_row.row_cells,
            row_snapshot.get_row_metadata().row_end,
        ))
    }

    /// `read_cell` applied to the cell at `row_index`/`column_index`, or
    /// `None` if the row is gone or the column is past the screen width. A
    /// column past the row's last cell reads as a default blank.
    ///
    /// A history row holding `hi` on an 80-column screen answers `h`, `i`,
    /// then a blank for columns 2 through 79, then `None`.
    fn read_cell<CellAnswer>(
        &self,
        row_index: u64,
        column_index: u16,
        read_cell: impl FnOnce(&Cell) -> CellAnswer,
    ) -> Option<CellAnswer> {
        let column_count = self.get_column_count();
        self.read_row(row_index, |row_cells, _| {
            match row_cells.get(usize::from(column_index)) {
                Some(cell) => Some(read_cell(cell)),
                None if column_index < column_count => Some(read_cell(&DEFAULT_PADDING_CELL)),
                None => None,
            }
        })
        .flatten()
    }

    /// How row `row_index` ended, or `None` if that row has been dropped from
    /// history or is past the bottom of the live screen. Decodes nothing.
    #[must_use]
    pub fn get_row_end(&self, row_index: u64) -> Option<RowEnd> {
        if row_index >= self.live_screen_top_row_number {
            let grid_row_index = self.find_live_row_index(row_index)?;
            return Some(self.screen_grid.get_row_end(grid_row_index));
        }
        let row_snapshot = self.find_history_row(row_index)?;
        Some(row_snapshot.get_row_metadata().row_end)
    }

    /// A copy of the cell at `row_index`/`column_index`, or `None` if the row
    /// is gone or the column is past the screen width. A column past the row's
    /// last cell reads as a default blank.
    #[must_use]
    pub fn get_cell(&self, row_index: u64, column_index: u16) -> Option<Cell> {
        self.read_cell(row_index, column_index, Cell::clone)
    }

    /// Whether `row_index` soft-wrapped into the row below it: the two rows hold one
    /// logical line.
    ///
    /// A `hello world` that wrapped mid-word across two rows is one logical
    /// line; two separate `echo` outputs are two. Word and line selections both
    /// follow the text across a soft wrap and stop at a hard one.
    #[must_use]
    pub fn is_soft_wrapped(&self, row_index: u64) -> bool {
        matches!(
            self.get_row_end(row_index),
            Some(RowEnd::Soft | RowEnd::SoftWide)
        )
    }

    /// Whether `row_index`/`column_index` is the blank last-column spacer left when a wide
    /// glyph wrapped whole onto the next row. The spacer carries layout only;
    /// it is not selectable text.
    #[must_use]
    pub fn is_wide_wrap_spacer(&self, row_index: u64, column_index: u16) -> bool {
        self.get_row_end(row_index) == Some(RowEnd::SoftWide)
            && usize::from(column_index) + 1 == usize::from(self.get_column_count())
    }

    /// The first row of the logical line containing `row_index`: walk up while
    /// the row above wrapped into this one.
    ///
    /// `ls` printing one long filename that wrapped over rows 10, 11, and 12:
    /// `get_line_start_row_index(11)` is `10`.
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

    /// The last row of the logical line containing `row_index`: walk down while
    /// this row wraps into the next.
    ///
    /// For the wrapped filename above, `get_line_end_row_index(11)` is `12`.
    #[must_use]
    pub fn get_line_end_row_index(&self, row_index: u64) -> u64 {
        let mut end_row_index = row_index;
        while end_row_index < self.get_last_row_index() && self.is_soft_wrapped(end_row_index) {
            end_row_index += 1;
        }
        end_row_index
    }

    /// Whether the cell at `row_index`/`column_index` ends a word.
    ///
    /// A cell holding one of [`WORD_SEPARATORS`] ends a word, and so does a
    /// column past the screen width. A column right of a history line's text
    /// reads as a blank, and a blank is itself a separator.
    fn is_separator(&self, row_index: u64, column_index: u16) -> bool {
        self.read_cell(row_index, column_index, |cell| {
            WORD_SEPARATORS.contains(cell.get_character())
        })
        .unwrap_or(true)
    }

    /// Whether `row_index`/`column_index` holds layout rather than text: the blank width-0
    /// right half of a wide (CJK/emoji) glyph, whose text lives entirely in its
    /// left half, or the spacer of [`is_wide_wrap_spacer`](Self::is_wide_wrap_spacer).
    /// A gone row or a column past the screen width is not layout.
    fn is_layout_cell(&self, row_index: u64, column_index: u16) -> bool {
        self.is_wide_wrap_spacer(row_index, column_index)
            || self
                .read_cell(row_index, column_index, |cell| {
                    cell.get_display_width() == 0
                })
                .unwrap_or(false)
    }

    /// The cell before `row_index`/`column_index` in reading order, crossing a soft wrap to the
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

    /// The cell after `row_index`/`column_index` in reading order, crossing a soft wrap to the
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

    /// The separator character at `row_index`/`column_index`, or `None` when
    /// the cell holds part of a word, is the width-0 half of a wide glyph (the
    /// glyph's own cell is the text there), or holds nothing.
    ///
    /// A gone row and a column past the screen width read as `Some(' ')`, the
    /// same answer [`is_separator`](Self::is_separator) gives them.
    fn find_separator_character(&self, row_index: u64, column_index: u16) -> Option<char> {
        self.read_cell(row_index, column_index, |cell| {
            if cell.get_display_width() == 0 {
                return None;
            }
            WORD_SEPARATORS
                .contains(cell.get_character())
                .then(|| cell.get_character())
        })
        .unwrap_or(Some(' '))
    }

    /// Whether stepping onto `row_index`/`column_index` leaves the word being
    /// grown.
    ///
    /// Growing a separator run of `separator_run`, the walk leaves it at any
    /// cell that does not hold that same character. Growing a word
    /// (`separator_run` is `None`), the walk leaves it at a separator.
    fn is_word_boundary(
        &self,
        separator_run: Option<char>,
        row_index: u64,
        column_index: u16,
    ) -> bool {
        match separator_run {
            Some(separator_character) => self
                .read_cell(row_index, column_index, |cell| {
                    cell.get_character() != separator_character
                })
                .unwrap_or(true),
            None => self.is_separator(row_index, column_index),
        }
    }

    /// The start of the word at `row_index`/`column_index`: step left while the cell there is
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
        let separator_run = self.find_separator_character(row_index, column_index);
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

    /// The end of the word at `row_index`/`column_index`: the mirror of
    /// [`get_word_start_position`](Self::get_word_start_position), stepping
    /// right — including the separator-run rule for a start cell that is itself
    /// a separator.
    #[must_use]
    pub fn get_word_end_position(&self, row_index: u64, column_index: u16) -> (u64, u16) {
        let separator_run = self.find_separator_character(row_index, column_index);
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

/// The text `selection` covers in `text_view`, as the string a copy places on the
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
/// When `should_trim_trailing_whitespace` is true, trailing blanks are dropped from
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
    text_view: &TextView<'_>,
    selection: &Selection,
    should_trim_trailing_whitespace: bool,
) -> String {
    let ordered_selection = order_selection_positions(selection.anchor, selection.cursor);
    let (selection_start, selection_end) = (
        ordered_selection.start_position,
        ordered_selection.end_position,
    );
    let column_count = text_view.get_column_count();
    let last_column_index = column_count.saturating_sub(1);
    let is_block_selection = matches!(selection.selection_kind, SelectionKind::Block);
    // Clamped to the rows the view holds: a selection can name any row number,
    // and only `first_row..=last_row` has text to read.
    let first_selected_row_index = selection_start
        .row_index
        .max(text_view.get_first_row_index());
    let last_selected_row_index = selection_end.row_index.min(text_view.get_last_row_index());
    // Reserves one byte per column and one per row end, for at most one
    // screen of rows.
    let (screen_row_count, _) = text_view.screen_grid.get_grid_dimensions();
    let reserved_row_count = (last_selected_row_index + 1)
        .saturating_sub(first_selected_row_index)
        .min(u64::from(screen_row_count));
    let mut selected_text = String::with_capacity(
        usize::try_from(reserved_row_count).expect("a screen holds at most 65 535 rows")
            * (usize::from(column_count) + 1),
    );
    let mut has_written_selected_row = false;
    for row_index in first_selected_row_index..=last_selected_row_index {
        let Some(row_end) = text_view.get_row_end(row_index) else {
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
        if has_written_selected_row
            && (is_block_selection || !text_view.is_soft_wrapped(row_index - 1))
        {
            selected_text.push('\n');
        }
        has_written_selected_row = true;
        let row_text_start = selected_text.len();
        text_view.read_row(row_index, |row_cells, _| {
            // Skipped: the spacer left in the last column when a wide glyph
            // wrapped whole onto the next row, and the blank right half of a
            // wide glyph, whose text lives in its left half. Both are layout.
            let text_column_count = if row_end == RowEnd::SoftWide {
                column_count.saturating_sub(1)
            } else {
                column_count
            };
            let first_column = usize::from(first_column_index);
            let selected_column_end =
                (usize::from(last_selected_column_index) + 1).min(usize::from(text_column_count));
            let selected_cell_end = selected_column_end.min(row_cells.len());
            for cell in row_cells
                .get(first_column..selected_cell_end)
                .unwrap_or_default()
            {
                if cell.get_display_width() == 0 {
                    continue;
                }
                selected_text.push(cell.get_character());
                for &combining_character in cell.list_combining_characters() {
                    selected_text.push(combining_character);
                }
            }
            // Columns past the row's last cell read as default blanks.
            let padding_start = first_column.max(row_cells.len());
            selected_text.extend(std::iter::repeat_n(
                ' ',
                selected_column_end.saturating_sub(padding_start),
            ));
        });
        let is_selected_row_soft_wrapped = matches!(row_end, RowEnd::Soft | RowEnd::SoftWide);
        if should_trim_trailing_whitespace && (is_block_selection || !is_selected_row_soft_wrapped)
        {
            let trimmed_row_text_length = selected_text[row_text_start..].trim_end().len();
            selected_text.truncate(row_text_start + trimmed_row_text_length);
        }
    }
    selected_text
}

/// A selection's two ends put into text order — `start_position` never comes after `end_position`.
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
pub fn order_selection_positions(
    selection_anchor: GridPosition,
    selection_cursor: GridPosition,
) -> OrderedSelection {
    if (selection_anchor.row_index, selection_anchor.column_index)
        <= (selection_cursor.row_index, selection_cursor.column_index)
    {
        OrderedSelection {
            start_position: selection_anchor,
            end_position: selection_cursor,
        }
    } else {
        OrderedSelection {
            start_position: selection_cursor,
            end_position: selection_anchor,
        }
    }
}

#[cfg(test)]
mod tests;
