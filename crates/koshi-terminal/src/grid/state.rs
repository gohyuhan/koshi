//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.

use crate::style::Style;
use std::cmp::min;

/// A single grid cell: its character, display width, and style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The base character occupying the cell.
    ch: char,
    /// The rest of the grapheme cluster layered over the base [`ch`](Cell::ch)
    /// — a grapheme cluster is the run of code points a person perceives as
    /// one visual character — in arrival order: combining accents, variation
    /// selectors, and the joined parts of a multi-codepoint emoji (ZWJ-joined
    /// glyphs, skin-tone modifiers, the second half of a flag). Empty for a
    /// plain cell; the renderer draws `ch` followed by these as one glyph.
    /// Named for the common case (combining marks) though it also carries
    /// non-zero-width emoji continuations.
    combining: Vec<char>,
    /// Display width in cells: 0 (continuation half of a wide glyph), 1
    /// (narrow), or 2 (wide, e.g. CJK).
    width: u8,
    /// The cell's visual style (color, bold, italic, etc.).
    style: Style,
}

impl Cell {
    /// A blank cell: a single space in the default style.
    pub fn blank() -> Self {
        Cell::blank_with(Style::default())
    }

    /// A blank cell — a single space — in the given `style`. Used to carry the
    /// current background into erased and scrolled cells (background-color
    /// erase); `style` is typically just the pen's background — the pen is
    /// the color/attribute state applied to newly written text.
    pub fn blank_with(style: Style) -> Self {
        Cell {
            ch: ' ',
            combining: Vec::new(),
            width: 1,
            style,
        }
    }

    /// A cell holding `ch` of the given display `width`, in `style`.
    pub fn new(ch: char, width: u8, style: Style) -> Self {
        Cell {
            ch,
            combining: Vec::new(),
            width,
            style,
        }
    }

    /// The character occupying this cell.
    pub fn ch(&self) -> char {
        self.ch
    }

    /// The rest of the grapheme cluster layered over the base character, in
    /// arrival order (combining marks plus any emoji continuation); empty for a
    /// plain cell.
    pub fn combining(&self) -> &[char] {
        &self.combining
    }

    /// Layer one continuation code point (combining mark, ZWJ, variation
    /// selector, joined emoji part, …) onto this cell, keeping the base
    /// character and width unchanged.
    pub fn push_combining(&mut self, mark: char) {
        self.combining.push(mark);
    }

    /// The cell's display width: 0 (combining/continuation), 1 (narrow), or 2
    /// (wide).
    pub fn width(&self) -> u8 {
        self.width
    }

    /// The cell's visual style.
    pub fn style(&self) -> Style {
        self.style
    }
}

/// How a row ends relative to the row directly below it. This is row state,
/// not cell state: it records whether the two rows hold one logical line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowEnd {
    /// The row ends its logical line: the next row starts a new one.
    #[default]
    Hard,
    /// The row soft-wrapped under autowrap: the next row continues this
    /// row's logical line, and a resize reflow re-joins them.
    Soft,
    /// The row soft-wrapped because a wide glyph did not fit its last
    /// column: the final cell is a blank spacer, dropped when a reflow
    /// re-joins the line, so the wide glyph rejoins the text with no
    /// phantom space.
    SoftWide,
}

/// A fixed-size grid of cells, addressed `rows[row][col]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    /// Row-major cell storage: `rows[row][col]`.
    rows: Vec<Vec<Cell>>,
    /// Per-row line-continuation state, parallel to `rows`: `row_ends[row]`
    /// says whether `rows[row]` and `rows[row + 1]` hold one logical line.
    /// Every operation that adds, removes, or reorders rows maintains it.
    row_ends: Vec<RowEnd>,
}

impl Grid {
    /// Build a `rows × cols` grid, every cell a blank space in `fill`.
    pub fn blank(rows: u16, cols: u16, fill: Style) -> Self {
        Grid {
            rows: vec![vec![Cell::blank_with(fill); cols as usize]; rows as usize],
            row_ends: vec![RowEnd::Hard; rows as usize],
        }
    }

    /// Build a grid from ready-made `rows`, normalizing each to exactly `cols`
    /// cells: a longer row is truncated, a shorter one padded with blank spaces
    /// in `fill` (both via [`Vec::resize`]). Every row end starts [`RowEnd::Hard`];
    /// a caller carrying real continuation state sets it afterwards with
    /// [`set_row_end`](Grid::set_row_end). Used to assemble a scrolled-back
    /// view window from a mix of scrollback and live-screen rows captured at
    /// possibly differing widths.
    pub fn from_rows(mut rows: Vec<Vec<Cell>>, cols: u16, fill: Style) -> Self {
        for row in &mut rows {
            row.resize(cols as usize, Cell::blank_with(fill));
        }
        let row_ends = vec![RowEnd::Hard; rows.len()];
        Grid { rows, row_ends }
    }

    /// How `row` ends relative to the row below it; out of bounds reads as
    /// [`RowEnd::Hard`].
    pub fn row_end(&self, row: u16) -> RowEnd {
        self.row_ends
            .get(row as usize)
            .copied()
            .unwrap_or(RowEnd::Hard)
    }

    /// Record how `row` ends relative to the row below it. Out of bounds is a
    /// no-op.
    pub fn set_row_end(&mut self, row: u16, end: RowEnd) {
        if let Some(slot) = self.row_ends.get_mut(row as usize) {
            *slot = end;
        }
    }

    /// The grid's dimensions as `(rows, cols)`.
    pub fn dimensions(&self) -> (u16, u16) {
        (
            self.rows.len() as u16,
            self.rows.first().map_or(0, Vec::len) as u16,
        )
    }

    /// A reference to the cell at (`row`, `col`), or `None` if out of bounds.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.rows.get(row as usize)?.get(col as usize)
    }

    /// A mutable reference to the cell at (`row`, `col`), or `None` if out of
    /// bounds — the write path used by the VTE performer.
    pub fn cell_mut(&mut self, row: u16, col: u16) -> Option<&mut Cell> {
        self.rows.get_mut(row as usize)?.get_mut(col as usize)
    }

    /// All rows, row-major, for read-only iteration by the renderer.
    pub fn rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }

    /// Blank columns `from..to` (half-open, `to` exclusive) in `row`, resetting
    /// each to a blank space in `fill`. An erase that reaches the row's last
    /// column breaks the row's continuation into the next row, so its end
    /// resets to [`RowEnd::Hard`]. Coordinates outside the grid are skipped via
    /// [`cell_mut`](Grid::cell_mut), so an oversized span, an inverted range
    /// (`from >= to`), or an empty grid never panics — it is simply a no-op.
    pub fn clear_line(&mut self, row: u16, from: u16, to: u16, fill: Style) {
        for i in from..to {
            if let Some(cell) = self.cell_mut(row, i) {
                *cell = Cell::blank_with(fill);
            }
        }
        let (_, cols) = self.dimensions();
        if to >= cols && from < cols {
            self.set_row_end(row, RowEnd::Hard);
        }
    }

    /// Insert `n` blank cells at column `col` of `row`, shifting existing cells
    /// to the right; cells pushed past the right edge are dropped. If `row` or
    /// `col` are out of bounds, this is a no-op. The inserted cells are blanks
    /// in `fill` style (background-color erase).
    pub fn insert_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];
        let blanks = vec![Cell::blank_with(fill); n as usize];

        r.splice(col as usize..col as usize, blanks);
        r.truncate(cols as usize);
        // The shift replaced the row's tail, so any continuation into the
        // next row is broken.
        self.set_row_end(row, RowEnd::Hard);
    }

    /// Delete `n` cells starting at column `col` of `row`, shifting existing
    /// cells to the left; the freed space on the right is filled with blank cells
    /// in `fill` style (background-color erase). If `row` or `col` are out of
    /// bounds, this is a no-op.
    pub fn delete_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];
        let del = min(cols - col, n);
        let blanks = vec![Cell::blank_with(fill); del as usize];

        r.drain(col as usize..(col + del) as usize);
        r.extend(blanks);
        // The shift replaced the row's tail, so any continuation into the
        // next row is broken.
        self.set_row_end(row, RowEnd::Hard);
    }

    /// Delete `n` lines from the band `[first, last]` (both inclusive), shifting
    /// lines below the band upward; blank lines are inserted at the bottom of the
    /// band to preserve the band's height. Cells are filled in `fill` style
    /// (background-color erase). Coordinates outside the grid are no-ops.
    pub fn delete_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        let blank_row = vec![Cell::blank_with(fill); cols as usize];

        // Never remove more lines than the band actually holds.
        let remove_count = min(n, last - first + 1);

        // Each iteration removes the band's top line — the lines below it slide
        // up to fill the gap — then re-inserts a blank line at the band's
        // bottom, so the band keeps its original height after every step. Row
        // ends travel with their rows, so a soft-wrapped row scrolled off the
        // top keeps its continuation state.
        for _ in 0..remove_count as usize {
            self.rows.remove(first as usize);
            self.rows.insert(last as usize, blank_row.clone());
            self.row_ends.remove(first as usize);
            self.row_ends.insert(last as usize, RowEnd::Hard);
        }
        if remove_count > 0 {
            // The removed rows broke two continuations: the row above the
            // band lost the neighbor it wrapped into, and the row that slid
            // into the band's bottom now precedes a row it never wrapped into.
            if first > 0 {
                self.set_row_end(first - 1, RowEnd::Hard);
            }
            if let Some(slid_last) = last.checked_sub(remove_count) {
                self.set_row_end(slid_last, RowEnd::Hard);
            }
        }
    }

    /// Insert `n` blank lines within the band `[first, last]` (both inclusive),
    /// shifting lines downward; lines pushed below the band are dropped. Blank
    /// lines are filled in `fill` style (background-color erase). Coordinates
    /// outside the grid are no-ops.
    pub fn insert_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        let blank_row = vec![Cell::blank_with(fill); cols as usize];

        // Never insert more lines than the band can hold.
        let insert_count = min(n, last - first + 1);

        // Each iteration inserts a blank line at the band's top — the lines
        // below it slide down — then removes the line pushed just past the
        // band's bottom, so the band keeps its original height after every
        // step. Row ends travel with their rows.
        for _ in 0..insert_count as usize {
            self.rows.insert(first as usize, blank_row.clone());
            self.rows.remove(last as usize + 1);
            self.row_ends.insert(first as usize, RowEnd::Hard);
            self.row_ends.remove(last as usize + 1);
        }
        // The inserted blanks broke two continuations: the row above the band
        // now precedes a blank row, and the row that slid into the band's
        // bottom now precedes a row it never wrapped into.
        if first > 0 {
            self.set_row_end(first - 1, RowEnd::Hard);
        }
        self.set_row_end(last, RowEnd::Hard);
    }
}

#[cfg(test)]
mod tests;
