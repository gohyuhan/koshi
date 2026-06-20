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

/// A fixed-size grid of cells, addressed `rows[row][col]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    /// Row-major cell storage: `rows[row][col]`.
    rows: Vec<Vec<Cell>>,
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
}

#[cfg(test)]
mod tests;
