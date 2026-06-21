//! Unit tests for the cell grid.

use super::*;
use crate::style::{Color, Style};

/// A blank grid in the default style — the common fixture for these tests.
fn default_grid(rows: u16, cols: u16) -> Grid {
    Grid::blank(rows, cols, Style::default())
}

/// The default pen carrying `color` as its background — a non-default fill for
/// the background-color-erase cases.
fn bg(color: Color) -> Style {
    let mut style = Style::default();
    style.set_bg(color);
    style
}

#[test]
fn blank_cell_is_a_space_of_width_one_in_the_default_style() {
    let cell = Cell::blank();
    assert_eq!(cell.ch, ' ');
    assert_eq!(cell.width, 1);
    assert_eq!(cell.style, Style::default());
}

#[test]
fn blank_with_is_a_space_of_width_one_in_the_given_style() {
    let fill = bg(Color::Indexed(4));
    let cell = Cell::blank_with(fill);
    assert_eq!(cell.ch, ' ');
    assert_eq!(cell.width, 1);
    assert_eq!(cell.style, fill);
}

#[test]
fn blank_grid_has_exactly_rows_by_cols_cells() {
    let grid = default_grid(3, 5);
    assert_eq!(grid.rows.len(), 3);
    assert!(grid.rows.iter().all(|row| row.len() == 5));
}

#[test]
fn blank_grid_fills_every_cell_with_a_blank() {
    let grid = default_grid(2, 2);
    assert!(grid
        .rows
        .iter()
        .all(|row| row.iter().all(|cell| *cell == Cell::blank())));
}

#[test]
fn blank_grid_fills_every_cell_with_the_given_fill_style() {
    let fill = bg(Color::Indexed(2));
    let grid = Grid::blank(2, 3, fill);
    assert!(grid
        .rows
        .iter()
        .all(|row| row.iter().all(|cell| cell.style() == fill)));
}

#[test]
fn zero_rows_yields_an_empty_grid() {
    assert_eq!(default_grid(0, 5).rows.len(), 0);
}

#[test]
fn zero_cols_yields_rows_with_no_cells() {
    let grid = default_grid(2, 0);
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
    assert_eq!(default_grid(3, 5).dimensions(), (3, 5));
}

#[test]
fn dimensions_of_grids_with_a_zero_axis() {
    assert_eq!(default_grid(0, 5).dimensions(), (0, 0));
    assert_eq!(default_grid(3, 0).dimensions(), (3, 0));
}

#[test]
fn cell_returns_the_cell_for_in_range_coordinates() {
    let grid = default_grid(3, 5);
    assert_eq!(grid.cell(0, 0), Some(&Cell::blank()));
    assert_eq!(grid.cell(2, 4), Some(&Cell::blank()));
}

#[test]
fn cell_at_a_coordinate_equal_to_the_length_is_none() {
    let grid = default_grid(3, 5);
    assert_eq!(grid.cell(3, 0), None); // row == row count
    assert_eq!(grid.cell(0, 5), None); // col == col count
}

#[test]
fn cell_far_out_of_bounds_is_none() {
    assert_eq!(default_grid(3, 5).cell(100, 100), None);
}

#[test]
fn cell_mut_writes_a_cell_that_reads_back() {
    let mut grid = default_grid(2, 2);
    *grid.cell_mut(1, 1).expect("in bounds") = Cell::new('Z', 1, Style::default());
    assert_eq!(grid.cell(1, 1).map(Cell::ch), Some('Z'));
    assert_eq!(grid.cell(0, 0), Some(&Cell::blank())); // neighbour untouched
}

#[test]
fn cell_mut_out_of_bounds_is_none() {
    let mut grid = default_grid(3, 5);
    assert!(grid.cell_mut(3, 0).is_none());
    assert!(grid.cell_mut(0, 5).is_none());
}

#[test]
fn rows_exposes_every_row() {
    let grid = default_grid(3, 5);
    assert_eq!(grid.rows().len(), 3);
    assert!(grid.rows().iter().all(|row| row.len() == 5));
}

#[test]
fn scroll_up_drops_the_top_row_and_blanks_a_new_bottom_row() {
    let mut grid = default_grid(3, 2);
    *grid.cell_mut(0, 0).expect("in bounds") = Cell::new('a', 1, Style::default());
    *grid.cell_mut(1, 0).expect("in bounds") = Cell::new('b', 1, Style::default());
    *grid.cell_mut(2, 0).expect("in bounds") = Cell::new('c', 1, Style::default());

    grid.scroll_up(Style::default());

    assert_eq!(grid.cell(0, 0).map(Cell::ch), Some('b')); // old row 1 rises
    assert_eq!(grid.cell(1, 0).map(Cell::ch), Some('c')); // old row 2 rises
    assert_eq!(grid.cell(2, 0), Some(&Cell::blank())); // fresh blank bottom
}

#[test]
fn scroll_up_fills_the_new_bottom_row_with_the_given_fill_style() {
    let fill = bg(Color::Indexed(4));
    let mut grid = default_grid(2, 3);
    grid.scroll_up(fill);
    // The freshly exposed bottom row carries the fill background.
    assert!((0..3).all(|c| grid.cell(1, c).map(Cell::style) == Some(fill)));
}

#[test]
fn scroll_up_preserves_dimensions() {
    let mut grid = default_grid(2, 4);
    grid.scroll_up(Style::default());
    assert_eq!(grid.dimensions(), (2, 4));
}

#[test]
fn scroll_up_on_an_empty_grid_is_a_no_op() {
    let mut grid = default_grid(0, 5);
    grid.scroll_up(Style::default());
    assert_eq!(grid.dimensions(), (0, 0));
    assert!(grid.rows().is_empty());
}

#[test]
fn clear_line_blanks_the_half_open_span() {
    let mut grid = default_grid(1, 5);
    for col in 0..5 {
        *grid.cell_mut(0, col).expect("in bounds") = Cell::new('x', 1, Style::default());
    }
    grid.clear_line(0, 1, 4, Style::default()); // columns 1, 2, 3 — `to` (4) is excluded
    assert_eq!(grid.cell(0, 0).map(Cell::ch), Some('x')); // before the span
    assert_eq!(grid.cell(0, 1), Some(&Cell::blank()));
    assert_eq!(grid.cell(0, 2), Some(&Cell::blank()));
    assert_eq!(grid.cell(0, 3), Some(&Cell::blank()));
    assert_eq!(grid.cell(0, 4).map(Cell::ch), Some('x')); // excluded end kept
}

#[test]
fn clear_line_fills_the_span_with_the_given_fill_style() {
    let fill = bg(Color::Indexed(1));
    let mut grid = default_grid(1, 5);
    for col in 0..5 {
        *grid.cell_mut(0, col).expect("in bounds") = Cell::new('x', 1, Style::default());
    }
    grid.clear_line(0, 1, 4, fill);
    assert_eq!(grid.cell(0, 0).map(Cell::ch), Some('x')); // outside the span: untouched
    assert!((1..4).all(|c| grid.cell(0, c).map(Cell::style) == Some(fill)));
    assert_eq!(grid.cell(0, 4).map(Cell::ch), Some('x'));
}

#[test]
fn clear_line_with_an_inverted_range_is_a_no_op() {
    let mut grid = default_grid(1, 3);
    *grid.cell_mut(0, 1).expect("in bounds") = Cell::new('y', 1, Style::default());
    grid.clear_line(0, 3, 1, Style::default()); // from >= to
    assert_eq!(grid.cell(0, 1).map(Cell::ch), Some('y'));
}

#[test]
fn clear_line_clamps_an_oversized_span() {
    let mut grid = default_grid(1, 3);
    for col in 0..3 {
        *grid.cell_mut(0, col).expect("in bounds") = Cell::new('z', 1, Style::default());
    }
    grid.clear_line(0, 0, 99, Style::default()); // runs past the row width — no panic
    assert!((0..3).all(|c| grid.cell(0, c) == Some(&Cell::blank())));
}

#[test]
fn clear_line_on_an_out_of_range_row_is_a_no_op() {
    let mut grid = default_grid(2, 2);
    *grid.cell_mut(0, 0).expect("in bounds") = Cell::new('q', 1, Style::default());
    grid.clear_line(9, 0, 2, Style::default()); // row out of range
    assert_eq!(grid.cell(0, 0).map(Cell::ch), Some('q'));
}
