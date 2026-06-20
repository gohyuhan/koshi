//! Unit tests for the cell grid.

use super::*;
use crate::style::Style;

#[test]
fn blank_cell_is_a_space_of_width_one_in_the_default_style() {
    let cell = Cell::blank();
    assert_eq!(cell.ch, ' ');
    assert_eq!(cell.width, 1);
    assert_eq!(cell.style, Style::default());
}

#[test]
fn blank_grid_has_exactly_rows_by_cols_cells() {
    let grid = Grid::blank(3, 5);
    assert_eq!(grid.rows.len(), 3);
    assert!(grid.rows.iter().all(|row| row.len() == 5));
}

#[test]
fn blank_grid_fills_every_cell_with_a_blank() {
    let grid = Grid::blank(2, 2);
    assert!(grid
        .rows
        .iter()
        .all(|row| row.iter().all(|cell| *cell == Cell::blank())));
}

#[test]
fn zero_rows_yields_an_empty_grid() {
    assert_eq!(Grid::blank(0, 5).rows.len(), 0);
}

#[test]
fn zero_cols_yields_rows_with_no_cells() {
    let grid = Grid::blank(2, 0);
    assert_eq!(grid.rows.len(), 2);
    assert!(grid.rows.iter().all(|row| row.is_empty()));
}
