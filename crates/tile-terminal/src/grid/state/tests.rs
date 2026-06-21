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

#[test]
fn new_cell_round_trips_through_its_accessors() {
    let cell = Cell::new('A', 2, Style::default());
    assert_eq!(cell.ch(), 'A');
    assert_eq!(cell.width(), 2);
    assert_eq!(cell.style(), Style::default());
}

#[test]
fn dimensions_reports_rows_then_cols() {
    assert_eq!(Grid::blank(3, 5).dimensions(), (3, 5));
}

#[test]
fn dimensions_of_grids_with_a_zero_axis() {
    assert_eq!(Grid::blank(0, 5).dimensions(), (0, 0));
    assert_eq!(Grid::blank(3, 0).dimensions(), (3, 0));
}

#[test]
fn cell_returns_the_cell_for_in_range_coordinates() {
    let grid = Grid::blank(3, 5);
    assert_eq!(grid.cell(0, 0), Some(&Cell::blank()));
    assert_eq!(grid.cell(2, 4), Some(&Cell::blank()));
}

#[test]
fn cell_at_a_coordinate_equal_to_the_length_is_none() {
    let grid = Grid::blank(3, 5);
    assert_eq!(grid.cell(3, 0), None); // row == row count
    assert_eq!(grid.cell(0, 5), None); // col == col count
}

#[test]
fn cell_far_out_of_bounds_is_none() {
    assert_eq!(Grid::blank(3, 5).cell(100, 100), None);
}

#[test]
fn cell_mut_writes_a_cell_that_reads_back() {
    let mut grid = Grid::blank(2, 2);
    *grid.cell_mut(1, 1).expect("in bounds") = Cell::new('Z', 1, Style::default());
    assert_eq!(grid.cell(1, 1).map(Cell::ch), Some('Z'));
    assert_eq!(grid.cell(0, 0), Some(&Cell::blank())); // neighbour untouched
}

#[test]
fn cell_mut_out_of_bounds_is_none() {
    let mut grid = Grid::blank(3, 5);
    assert!(grid.cell_mut(3, 0).is_none());
    assert!(grid.cell_mut(0, 5).is_none());
}

#[test]
fn rows_exposes_every_row() {
    let grid = Grid::blank(3, 5);
    assert_eq!(grid.rows().len(), 3);
    assert!(grid.rows().iter().all(|row| row.len() == 5));
}

#[test]
fn scroll_up_drops_the_top_row_and_blanks_a_new_bottom_row() {
    let mut grid = Grid::blank(3, 2);
    *grid.cell_mut(0, 0).expect("in bounds") = Cell::new('a', 1, Style::default());
    *grid.cell_mut(1, 0).expect("in bounds") = Cell::new('b', 1, Style::default());
    *grid.cell_mut(2, 0).expect("in bounds") = Cell::new('c', 1, Style::default());

    grid.scroll_up();

    assert_eq!(grid.cell(0, 0).map(Cell::ch), Some('b')); // old row 1 rises
    assert_eq!(grid.cell(1, 0).map(Cell::ch), Some('c')); // old row 2 rises
    assert_eq!(grid.cell(2, 0), Some(&Cell::blank())); // fresh blank bottom
}

#[test]
fn scroll_up_preserves_dimensions() {
    let mut grid = Grid::blank(2, 4);
    grid.scroll_up();
    assert_eq!(grid.dimensions(), (2, 4));
}
