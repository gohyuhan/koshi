//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.

use crate::style::Style;
use std::cmp::min;

/// A single grid cell: its character, display width, and style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The base character occupying the cell.
    ch: char,
    /// Zero-width marks (combining accents, ZWJ, variation selectors, …)
    /// layered over [`ch`](Cell::ch), in arrival order. Empty for a plain cell;
    /// the renderer draws the base glyph with these stacked on top.
    combining: Vec<char>,
    /// Display width in cells: 0 (continuation half of a wide glyph), 1
    /// (narrow), or 2 (wide, e.g. CJK).
    width: u8,
    /// The cell's visual style.
    style: Style,
}

impl Cell {
    /// A blank cell: a single space in the default style.
    pub fn blank() -> Self {
        Cell::blank_with(Style::default())
    }

    /// A blank cell — a single space — in the given `style`. Used to carry the
    /// current background into erased and scrolled cells (background-color
    /// erase); `style` is typically the pen's background only.
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

    /// The zero-width marks layered over the base character, in arrival order;
    /// empty for a plain cell.
    pub fn combining(&self) -> &[char] {
        &self.combining
    }

    /// Layer one zero-width `mark` (combining accent, ZWJ, …) onto this cell,
    /// keeping the base character and width unchanged.
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

/// A fixed-size grid of cells, addressed `rows[row][col]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    /// Row-major cell storage: `rows[row][col]`.
    rows: Vec<Vec<Cell>>,
}

impl Grid {
    /// Build a `rows × cols` grid, every cell a blank space in `fill`.
    pub fn blank(rows: u16, cols: u16, fill: Style) -> Self {
        let mut blank_grid_rows = Vec::new();

        for _ in 0..rows {
            let mut row = Vec::new();
            for _ in 0..cols {
                row.push(Cell::blank_with(fill));
            }
            blank_grid_rows.push(row);
        }

        Grid {
            rows: blank_grid_rows,
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
        if row as usize >= self.rows.len() {
            return None;
        }

        if col as usize >= self.rows[row as usize].len() {
            return None;
        }

        Some(&self.rows[row as usize][col as usize])
    }

    /// A mutable reference to the cell at (`row`, `col`), or `None` if out of
    /// bounds — the write path used by the VTE performer.
    pub fn cell_mut(&mut self, row: u16, col: u16) -> Option<&mut Cell> {
        if row as usize >= self.rows.len() {
            return None;
        }

        if col as usize >= self.rows[row as usize].len() {
            return None;
        }

        Some(&mut self.rows[row as usize][col as usize])
    }

    /// All rows, row-major, for read-only iteration by the renderer.
    pub fn rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }

    /// Blank columns `from..to` (half-open, `to` exclusive) in `row`, resetting
    /// each to a blank space in `fill`. Coordinates outside the grid are skipped via
    /// [`cell_mut`](Grid::cell_mut), so an oversized span, an inverted range
    /// (`from >= to`), or an empty grid is a safe no-op rather than a panic.
    pub fn clear_line(&mut self, row: u16, from: u16, to: u16, fill: Style) {
        for i in from..to {
            if let Some(cell) = self.cell_mut(row, i) {
                *cell = Cell::blank_with(fill);
            }
        }
    }

    pub fn insert_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];
        let blanks = vec![Cell::blank_with(fill); n as usize];

        r.splice(col as usize..col as usize, blanks);
        r.truncate(cols as usize);
    }

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
    }

    pub fn delete_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        let blank_row = vec![Cell::blank_with(fill); cols as usize];

        let remove_count = min(n, last - first + 1);

        for _ in 0..remove_count as usize {
            self.rows.remove(first as usize);
            self.rows.insert(last as usize, blank_row.clone());
        }
    }

    pub fn insert_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        let blank_row = vec![Cell::blank_with(fill); cols as usize];

        let insert_count = min(n, last - first + 1);

        for _ in 0..insert_count as usize {
            self.rows.insert(first as usize, blank_row.clone());
            self.rows.remove(last as usize + 1);
        }
    }
}

#[cfg(test)]
mod tests;
