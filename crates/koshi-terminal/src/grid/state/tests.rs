//! Unit tests for the cell grid.

use super::*;
use crate::style::{Color, Style};

/// A blank grid in the default style — the common fixture for these tests.
fn default_grid(rows: u16, cols: u16) -> Grid {
    Grid::blank(rows, cols, Style::default())
}

/// A text rendering style (pen) with the given background color — used to test
/// background-color-erase (BCE) operations that fill erased cells with the pen's color.
fn bg(color: Color) -> Style {
    let mut style = Style::default();
    style.set_bg(color);
    style
}

/// Write `s` left-to-right into `row`, one default-styled char per cell.
fn write_row(grid: &mut Grid, row: u16, s: &str) {
    for (col, ch) in s.chars().enumerate() {
        *grid.cell_mut(row, col as u16).expect("in bounds") = Cell::new(ch, 1, Style::default());
    }
}

/// Read `row` of the grid as a string; blank cells read as spaces.
fn row_text(grid: &Grid, row: u16) -> String {
    let (_, cols) = grid.dimensions();
    (0..cols)
        .map(|c| grid.cell(row, c).map(Cell::ch).unwrap_or(' '))
        .collect()
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
fn push_combining_appends_marks_in_arrival_order_without_changing_ch_or_width() {
    let mut cell = Cell::new('e', 1, Style::default());
    assert_eq!(cell.combining(), &[]);
    cell.push_combining('\u{0301}'); // combining acute
    cell.push_combining('\u{0302}'); // combining circumflex
    assert_eq!(cell.ch(), 'e');
    assert_eq!(cell.width(), 1);
    assert_eq!(cell.combining(), &['\u{0301}', '\u{0302}']);
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
fn delete_lines_full_grid_scrolls_up_dropping_the_top_row() {
    let mut grid = default_grid(3, 2);
    *grid.cell_mut(0, 0).expect("in bounds") = Cell::new('a', 1, Style::default());
    *grid.cell_mut(1, 0).expect("in bounds") = Cell::new('b', 1, Style::default());
    *grid.cell_mut(2, 0).expect("in bounds") = Cell::new('c', 1, Style::default());

    // A whole-grid band scrolled up by one is the old scroll-up behavior.
    grid.delete_lines(0, 2, 1, Style::default());

    assert_eq!(grid.cell(0, 0).map(Cell::ch), Some('b')); // old row 1 rises
    assert_eq!(grid.cell(1, 0).map(Cell::ch), Some('c')); // old row 2 rises
    assert_eq!(grid.cell(2, 0), Some(&Cell::blank())); // fresh blank bottom
}

#[test]
fn delete_lines_fills_the_new_bottom_row_with_the_given_fill_style() {
    let fill = bg(Color::Indexed(4));
    let mut grid = default_grid(2, 3);
    grid.delete_lines(0, 1, 1, fill);
    // The freshly exposed bottom row carries the fill background.
    assert!((0..3).all(|c| grid.cell(1, c).map(Cell::style) == Some(fill)));
}

#[test]
fn delete_lines_preserves_dimensions() {
    let mut grid = default_grid(2, 4);
    grid.delete_lines(0, 1, 1, Style::default());
    assert_eq!(grid.dimensions(), (2, 4));
}

#[test]
fn delete_lines_on_an_empty_grid_is_a_no_op() {
    let mut grid = default_grid(0, 5);
    grid.delete_lines(0, 0, 1, Style::default());
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

#[test]
fn insert_cells_shifts_right_and_drops_overflow() {
    let mut grid = default_grid(1, 5);
    write_row(&mut grid, 0, "abcde");
    grid.insert_cells(0, 2, 2, Style::default()); // two blanks at col 2
    assert_eq!(row_text(&grid, 0), "ab  c"); // c shifts right; d, e fall off
}

#[test]
fn insert_cells_with_n_past_the_row_blanks_to_the_edge() {
    let mut grid = default_grid(1, 4);
    write_row(&mut grid, 0, "abcd");
    grid.insert_cells(0, 1, 99, Style::default()); // far more than fits
    assert_eq!(row_text(&grid, 0), "a   "); // everything from col 1 pushed off
    assert_eq!(grid.dimensions(), (1, 4)); // width preserved
}

#[test]
fn insert_cells_fills_with_the_given_style() {
    let fill = bg(Color::Indexed(3));
    let mut grid = default_grid(1, 3);
    write_row(&mut grid, 0, "abc");
    grid.insert_cells(0, 0, 1, fill);
    assert_eq!(grid.cell(0, 0).map(Cell::style), Some(fill)); // inserted blank carries fill
}

#[test]
fn insert_cells_out_of_bounds_is_a_no_op() {
    let mut grid = default_grid(2, 3);
    write_row(&mut grid, 0, "xyz");
    grid.insert_cells(9, 0, 1, Style::default()); // bad row
    grid.insert_cells(0, 9, 1, Style::default()); // bad col
    assert_eq!(row_text(&grid, 0), "xyz");
}

#[test]
fn delete_cells_pulls_left_and_pads_the_right() {
    let mut grid = default_grid(1, 5);
    write_row(&mut grid, 0, "abcde");
    grid.delete_cells(0, 1, 2, Style::default()); // remove b, c
    assert_eq!(row_text(&grid, 0), "ade  ");
}

#[test]
fn delete_cells_clamps_n_and_preserves_width() {
    let mut grid = default_grid(1, 4);
    write_row(&mut grid, 0, "abcd");
    grid.delete_cells(0, 2, 99, Style::default()); // n exceeds the cells to the right
    assert_eq!(row_text(&grid, 0), "ab  ");
    assert_eq!(grid.dimensions(), (1, 4)); // width must not grow when n > remaining
}

#[test]
fn delete_cells_fills_with_the_given_style() {
    let fill = bg(Color::Indexed(3));
    let mut grid = default_grid(1, 3);
    write_row(&mut grid, 0, "abc");
    grid.delete_cells(0, 0, 1, fill);
    assert_eq!(grid.cell(0, 2).map(Cell::style), Some(fill)); // pad cell carries fill
}

#[test]
fn delete_lines_scrolls_a_band_up_leaving_outside_rows() {
    let mut grid = default_grid(4, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    write_row(&mut grid, 3, "DDD");
    grid.delete_lines(1, 2, 1, Style::default()); // band rows 1..=2
    assert_eq!(row_text(&grid, 0), "AAA"); // above band, kept
    assert_eq!(row_text(&grid, 1), "CCC"); // rose
    assert_eq!(row_text(&grid, 2), "   "); // blank at band bottom
    assert_eq!(row_text(&grid, 3), "DDD"); // below band, kept
}

#[test]
fn insert_lines_scrolls_a_band_down_dropping_the_bottom() {
    let mut grid = default_grid(4, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    write_row(&mut grid, 3, "DDD");
    grid.insert_lines(1, 2, 1, Style::default());
    assert_eq!(row_text(&grid, 0), "AAA"); // above band, kept
    assert_eq!(row_text(&grid, 1), "   "); // blank opened
    assert_eq!(row_text(&grid, 2), "BBB"); // pushed down (CCC fell off band bottom)
    assert_eq!(row_text(&grid, 3), "DDD"); // below band, kept
}

#[test]
fn line_ops_clamp_n_to_the_band_height() {
    let mut grid = default_grid(3, 2);
    write_row(&mut grid, 0, "AA");
    write_row(&mut grid, 1, "BB");
    write_row(&mut grid, 2, "CC");
    grid.delete_lines(0, 1, 99, Style::default()); // n far exceeds the 2-row band
    assert_eq!(row_text(&grid, 0), "  "); // whole band blanked
    assert_eq!(row_text(&grid, 1), "  ");
    assert_eq!(row_text(&grid, 2), "CC"); // outside band, kept
    assert_eq!(grid.dimensions(), (3, 2));
}

#[test]
fn line_ops_with_an_inverted_or_oob_band_are_no_ops() {
    let mut grid = default_grid(3, 2);
    write_row(&mut grid, 0, "AA");
    write_row(&mut grid, 1, "BB");
    write_row(&mut grid, 2, "CC");
    grid.delete_lines(2, 1, 1, Style::default()); // first > last
    grid.insert_lines(0, 9, 1, Style::default()); // last out of range
    assert_eq!(row_text(&grid, 0), "AA");
    assert_eq!(row_text(&grid, 1), "BB");
    assert_eq!(row_text(&grid, 2), "CC");
}

#[test]
fn delete_lines_with_a_single_row_band_blanks_only_that_row() {
    // A degenerate band (`first == last`) is still a legal band, e.g. a
    // scroll region collapsed to one row: it must blank exactly that row,
    // never touch a neighbor or fall through as a no-op.
    let mut grid = default_grid(3, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    grid.delete_lines(1, 1, 1, Style::default());
    assert_eq!(row_text(&grid, 0), "AAA"); // above the band, untouched
    assert_eq!(row_text(&grid, 1), "   "); // the band's one row blanked
    assert_eq!(row_text(&grid, 2), "CCC"); // below the band, untouched
}

#[test]
fn insert_lines_with_a_single_row_band_blanks_only_that_row() {
    let mut grid = default_grid(3, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    grid.insert_lines(1, 1, 1, Style::default());
    assert_eq!(row_text(&grid, 0), "AAA"); // above the band, untouched
    assert_eq!(row_text(&grid, 1), "   "); // the band's one row blanked
    assert_eq!(row_text(&grid, 2), "CCC"); // below the band, untouched
}

/// A row of `s`, one default-styled cell per char.
fn text_row(s: &str) -> Vec<Cell> {
    s.chars()
        .map(|ch| Cell::new(ch, 1, Style::default()))
        .collect()
}

#[test]
fn from_rows_normalizes_each_row_to_the_given_width() {
    // Row 0 is short (padded to 3), row 1 is long (truncated to 3).
    let grid = Grid::from_rows(vec![text_row("ab"), text_row("abcd")], 3, Style::default());
    assert_eq!(grid.dimensions(), (2, 3));
    assert_eq!(row_text(&grid, 0), "ab ");
    assert_eq!(row_text(&grid, 1), "abc");
}

#[test]
fn from_rows_pads_short_rows_with_the_fill_style() {
    let fill = bg(Color::Indexed(4));
    let grid = Grid::from_rows(vec![text_row("x")], 3, fill);
    // The base char keeps its own (default) style; the two padded cells carry the fill.
    assert_eq!(grid.cell(0, 0).unwrap().style(), Style::default());
    assert_eq!(grid.cell(0, 1).unwrap().style(), fill);
    assert_eq!(grid.cell(0, 2).unwrap().style(), fill);
}

#[test]
fn row_ends_travel_with_scrolled_rows() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(1, RowEnd::Soft);
    // Scroll the whole grid up one line: old row 1 lands on row 0 with its
    // continuation state; the fresh bottom row is a hard end.
    grid.delete_lines(0, 2, 1, Style::default());
    assert_eq!(grid.row_end(0), RowEnd::Soft);
    assert_eq!(grid.row_end(2), RowEnd::Hard);
}

#[test]
fn delete_lines_breaks_the_continuation_above_the_band() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(0, RowEnd::Soft); // row 0 wrapped into row 1
    grid.delete_lines(1, 2, 1, Style::default());
    // Row 0's continuation row is gone: the wrap no longer holds.
    assert_eq!(grid.row_end(0), RowEnd::Hard);
}

#[test]
fn insert_lines_breaks_continuations_at_the_band_edges() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::Soft);
    grid.insert_lines(1, 2, 1, Style::default());
    // Row 0 now precedes an inserted blank; the row shifted to the band's
    // bottom precedes a row it never wrapped into.
    assert_eq!(grid.row_end(0), RowEnd::Hard);
    assert_eq!(grid.row_end(2), RowEnd::Hard);
}

#[test]
fn tail_edits_reset_the_row_end() {
    let mut grid = Grid::blank(1, 4, Style::default());

    grid.set_row_end(0, RowEnd::Soft);
    grid.clear_line(0, 2, 4, Style::default()); // reaches the last column
    assert_eq!(grid.row_end(0), RowEnd::Hard);

    grid.set_row_end(0, RowEnd::Soft);
    grid.clear_line(0, 0, 2, Style::default()); // stops short of it
    assert_eq!(grid.row_end(0), RowEnd::Soft);

    grid.insert_cells(0, 1, 1, Style::default()); // shifts the tail
    assert_eq!(grid.row_end(0), RowEnd::Hard);

    grid.set_row_end(0, RowEnd::Soft);
    grid.delete_cells(0, 1, 1, Style::default()); // shifts the tail
    assert_eq!(grid.row_end(0), RowEnd::Hard);
}

#[test]
fn row_end_out_of_bounds_reads_hard_and_ignores_writes() {
    let mut grid = Grid::blank(2, 2, Style::default());
    assert_eq!(grid.row_end(9), RowEnd::Hard);
    grid.set_row_end(9, RowEnd::Soft); // no-op, no panic
    assert_eq!(grid.row_end(9), RowEnd::Hard);
}
