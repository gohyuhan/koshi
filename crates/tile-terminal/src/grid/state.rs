//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.
use crate::style::Style;

/// A single grid cell: its character, display width, and style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The character occupying the cell.
    ch: char,
    /// Display width in cells: 0 (combining/continuation), 1 (narrow), or 2
    /// (wide, e.g. CJK).
    width: u8,
    /// The cell's visual style.
    style: Style,
}

impl Cell {
    /// A blank cell: a single space in the default style.
    pub fn blank() -> Self {
        Cell {
            ch: ' ',
            width: 1,
            style: Style::default(),
        }
    }

    /// A cell holding `ch` of the given display `width`, in `style`.
    pub fn new(ch: char, width: u8, style: Style) -> Self {
        Cell { ch, width, style }
    }

    /// The character occupying this cell.
    pub fn ch(&self) -> char {
        self.ch
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
    /// Build a `rows × cols` grid filled with [`blank`](Cell::blank) cells.
    pub fn blank(rows: u16, cols: u16) -> Self {
        let mut blank_grid_rows = Vec::new();

        for _ in 0..rows {
            let mut row = Vec::new();
            for _ in 0..cols {
                row.push(Cell::blank());
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

    pub fn scroll_up(&mut self) {
        let removed_top_row = self.rows.remove(0);
        let mut new_cell_row = Vec::new();
        for _ in 0..removed_top_row.len() {
            new_cell_row.push(Cell::blank());
        }

        self.rows.push(new_cell_row);
    }
}

#[cfg(test)]
mod tests;
