//! Unit tests for the cell grid.

use super::*;
use crate::style::{Color, Style};

/// A blank grid in the default style — the common fixture for these tests.
fn build_default_grid(row_count: u16, column_count: u16) -> Grid {
    Grid::blank(row_count, column_count, Style::default())
}

#[test]
#[cfg(target_pointer_width = "64")]
fn a_cell_is_thirty_two_bytes() {
    // One cell per grid slot and per scrollback column: at a 10 000-line cap
    // and 200 columns, one extra byte per cell is 2 MB more per pane.
    assert_eq!(std::mem::size_of::<Cell>(), 32);
}

#[test]
fn a_cell_without_continuations_equals_one_that_had_none_added() {
    // `None` is the only representation of "no continuations"; the derived
    // equality compares it exactly.
    let mut cell = Cell::from_character('e', 1, Style::default());
    cell.push_combining('\u{301}');
    assert_eq!(cell.list_combining_characters(), &['\u{301}']);
    assert_ne!(cell, Cell::from_character('e', 1, Style::default()));
}

#[test]
fn continuations_accumulate_in_arrival_order() {
    let mut cell = Cell::from_character('a', 1, Style::default());
    assert_eq!(cell.list_combining_characters(), &[] as &[char]);
    cell.push_combining('\u{301}');
    cell.push_combining('\u{308}');
    assert_eq!(cell.list_combining_characters(), &['\u{301}', '\u{308}']);
    // The cloned cell owns its own copy of the combining characters.
    let mut cloned_cell = cell.clone();
    cloned_cell.push_combining('\u{327}');
    assert_eq!(cell.list_combining_characters(), &['\u{301}', '\u{308}']);
    assert_eq!(
        cloned_cell.list_combining_characters(),
        &['\u{301}', '\u{308}', '\u{327}']
    );
}

/// A default style with `color` as its background: the background style that
/// background-color erase (BCE) writes into erased cells.
fn build_background_style(color: Color) -> Style {
    let mut style = Style::default();
    style.set_background_color(color);
    style
}

/// Write `row_text` left-to-right into `row_index`, one default-styled character per cell.
fn write_row(grid: &mut Grid, row_index: u16, row_text: &str) {
    for (column_index, character) in row_text.chars().enumerate() {
        *grid
            .get_cell_mut(row_index, column_index as u16)
            .expect("in bounds") = Cell::from_character(character, 1, Style::default());
    }
}

/// Read `row_index` of the grid as a string; blank cells read as spaces.
fn get_row_text(grid: &Grid, row_index: u16) -> String {
    let (_, column_count) = grid.get_grid_dimensions();
    (0..column_count)
        .map(|column_index| {
            grid.get_cell(row_index, column_index)
                .map(Cell::get_character)
                .unwrap_or(' ')
        })
        .collect()
}

#[test]
fn blank_cell_is_a_space_of_width_one_in_the_default_style() {
    let cell = Cell::blank();
    assert_eq!(cell.character, ' ');
    assert_eq!(cell.display_width, 1);
    assert_eq!(cell.style, Style::default());
}

#[test]
fn blank_with_is_a_space_of_width_one_in_the_given_style() {
    let fill_style = build_background_style(Color::Indexed(4));
    let cell = Cell::blank_with(fill_style);
    assert_eq!(cell.character, ' ');
    assert_eq!(cell.display_width, 1);
    assert_eq!(cell.style, fill_style);
}

#[test]
fn blank_grid_has_exactly_row_and_column_counts() {
    let grid = build_default_grid(3, 5);
    assert_eq!(grid.rows.len(), 3);
    assert!(grid.rows.iter().all(|row_cells| row_cells.len() == 5));
}

#[test]
fn blank_grid_fills_every_cell_with_a_blank() {
    let grid = build_default_grid(2, 2);
    assert!(grid
        .rows
        .iter()
        .all(|row_cells| row_cells.iter().all(|cell| *cell == Cell::blank())));
}

#[test]
fn blank_grid_fills_every_cell_with_the_given_fill_style() {
    let fill_style = build_background_style(Color::Indexed(2));
    let grid = Grid::blank(2, 3, fill_style);
    assert!(grid
        .rows
        .iter()
        .all(|row_cells| row_cells.iter().all(|cell| cell.get_style() == fill_style)));
}

#[test]
fn zero_rows_yields_an_empty_grid() {
    assert_eq!(build_default_grid(0, 5).rows.len(), 0);
}

#[test]
fn zero_columns_yields_rows_with_no_cells() {
    let grid = build_default_grid(2, 0);
    assert_eq!(grid.rows.len(), 2);
    assert!(grid.rows.iter().all(|row_cells| row_cells.is_empty()));
}

#[test]
fn new_cell_round_trips_through_its_accessors() {
    let cell = Cell::from_character('A', 2, Style::default());
    assert_eq!(cell.get_character(), 'A');
    assert_eq!(cell.get_display_width(), 2);
    assert_eq!(cell.get_style(), Style::default());
}

#[test]
fn push_combining_appends_marks_without_changing_character_or_display_width() {
    let mut cell = Cell::from_character('e', 1, Style::default());
    assert_eq!(cell.list_combining_characters(), &[]);
    cell.push_combining('\u{0301}'); // combining acute
    cell.push_combining('\u{0302}'); // combining circumflex
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.get_display_width(), 1);
    assert_eq!(cell.list_combining_characters(), &['\u{0301}', '\u{0302}']);
}

#[test]
fn dimensions_reports_rows_then_cols() {
    assert_eq!(build_default_grid(3, 5).get_grid_dimensions(), (3, 5));
}

#[test]
fn dimensions_of_grids_with_a_zero_axis() {
    assert_eq!(build_default_grid(0, 5).get_grid_dimensions(), (0, 0));
    assert_eq!(build_default_grid(3, 0).get_grid_dimensions(), (3, 0));
}

#[test]
fn cell_returns_the_cell_for_in_range_coordinates() {
    let grid = build_default_grid(3, 5);
    assert_eq!(grid.get_cell(0, 0), Some(&Cell::blank()));
    assert_eq!(grid.get_cell(2, 4), Some(&Cell::blank()));
}

#[test]
fn cell_at_a_coordinate_equal_to_the_length_is_none() {
    let grid = build_default_grid(3, 5);
    assert_eq!(grid.get_cell(3, 0), None); // row index equals row count
    assert_eq!(grid.get_cell(0, 5), None); // column index equals column count
}

#[test]
fn cell_far_out_of_bounds_is_none() {
    assert_eq!(build_default_grid(3, 5).get_cell(100, 100), None);
}

#[test]
fn cell_mut_writes_a_cell_that_reads_back() {
    let mut grid = build_default_grid(2, 2);
    *grid.get_cell_mut(1, 1).expect("in bounds") = Cell::from_character('Z', 1, Style::default());
    assert_eq!(grid.get_cell(1, 1).map(Cell::get_character), Some('Z'));
    assert_eq!(grid.get_cell(0, 0), Some(&Cell::blank())); // neighbour untouched
}

#[test]
fn cell_mut_out_of_bounds_is_none() {
    let mut grid = build_default_grid(3, 5);
    assert_eq!(grid.get_cell_mut(3, 0), None);
    assert_eq!(grid.get_cell_mut(0, 5), None);
}

#[test]
fn cell_at_the_largest_coordinates_is_none() {
    let mut grid = build_default_grid(3, 5);
    assert_eq!(grid.get_cell(u16::MAX, u16::MAX), None);
    assert_eq!(grid.get_cell_mut(u16::MAX, 0), None);
    assert_eq!(grid.get_cell_mut(0, u16::MAX), None);
}

#[test]
fn list_rows_returns_every_row() {
    let grid = build_default_grid(3, 5);
    assert_eq!(grid.list_rows().len(), 3);
    assert!(grid
        .list_rows()
        .iter()
        .all(|row_cells| row_cells.len() == 5));
}

#[test]
fn delete_lines_full_grid_scrolls_up_dropping_the_top_row() {
    let mut grid = build_default_grid(3, 2);
    *grid.get_cell_mut(0, 0).expect("in bounds") = Cell::from_character('a', 1, Style::default());
    *grid.get_cell_mut(1, 0).expect("in bounds") = Cell::from_character('b', 1, Style::default());
    *grid.get_cell_mut(2, 0).expect("in bounds") = Cell::from_character('c', 1, Style::default());

    // A whole-grid band scrolled up by one.
    grid.delete_lines(0, 2, 1, Style::default());

    assert_eq!(grid.get_cell(0, 0).map(Cell::get_character), Some('b')); // old row 1 rises
    assert_eq!(grid.get_cell(1, 0).map(Cell::get_character), Some('c')); // old row 2 rises
    assert_eq!(grid.get_cell(2, 0), Some(&Cell::blank())); // fresh blank bottom
}

#[test]
fn delete_lines_fills_the_new_bottom_row_with_the_given_fill_style() {
    let fill_style = build_background_style(Color::Indexed(4));
    let mut grid = build_default_grid(2, 3);
    grid.delete_lines(0, 1, 1, fill_style);
    // The freshly exposed bottom row carries the fill background.
    assert!((0..3).all(|column_index| {
        grid.get_cell(1, column_index).map(Cell::get_style) == Some(fill_style)
    }));
}

#[test]
fn delete_lines_preserves_dimensions() {
    let mut grid = build_default_grid(2, 4);
    grid.delete_lines(0, 1, 1, Style::default());
    assert_eq!(grid.get_grid_dimensions(), (2, 4));
}

#[test]
fn delete_lines_on_an_empty_grid_is_a_no_op() {
    let mut grid = build_default_grid(0, 5);
    grid.delete_lines(0, 0, 1, Style::default());
    assert_eq!(grid.get_grid_dimensions(), (0, 0));
    assert!(grid.list_rows().is_empty());
}

#[test]
fn clear_line_blanks_the_half_open_span() {
    let mut grid = build_default_grid(1, 5);
    for column_index in 0..5 {
        *grid.get_cell_mut(0, column_index).expect("in bounds") =
            Cell::from_character('x', 1, Style::default());
    }
    grid.clear_line(0, 1, 4, Style::default()); // columns 1, 2, 3; column 4 is excluded
    assert_eq!(grid.get_cell(0, 0).map(Cell::get_character), Some('x')); // before the span
    assert_eq!(grid.get_cell(0, 1), Some(&Cell::blank()));
    assert_eq!(grid.get_cell(0, 2), Some(&Cell::blank()));
    assert_eq!(grid.get_cell(0, 3), Some(&Cell::blank()));
    assert_eq!(grid.get_cell(0, 4).map(Cell::get_character), Some('x')); // excluded end kept
}

#[test]
fn clear_line_fills_the_span_with_the_given_fill_style() {
    let fill_style = build_background_style(Color::Indexed(1));
    let mut grid = build_default_grid(1, 5);
    for column_index in 0..5 {
        *grid.get_cell_mut(0, column_index).expect("in bounds") =
            Cell::from_character('x', 1, Style::default());
    }
    grid.clear_line(0, 1, 4, fill_style);
    assert_eq!(grid.get_cell(0, 0).map(Cell::get_character), Some('x')); // outside the span: untouched
    assert!((1..4).all(|column_index| {
        grid.get_cell(0, column_index).map(Cell::get_style) == Some(fill_style)
    }));
    assert_eq!(grid.get_cell(0, 4).map(Cell::get_character), Some('x'));
}

#[test]
fn clear_line_with_an_inverted_range_is_a_no_op() {
    let mut grid = build_default_grid(1, 3);
    *grid.get_cell_mut(0, 1).expect("in bounds") = Cell::from_character('y', 1, Style::default());
    grid.clear_line(0, 3, 1, Style::default()); // first column is after the last column
    assert_eq!(grid.get_cell(0, 1).map(Cell::get_character), Some('y'));
}

#[test]
fn clear_line_clamps_an_oversized_span() {
    let mut grid = build_default_grid(1, 3);
    for column_index in 0..3 {
        *grid.get_cell_mut(0, column_index).expect("in bounds") =
            Cell::from_character('z', 1, Style::default());
    }
    grid.clear_line(0, 0, 99, Style::default()); // runs past the row width without panicking
    assert!((0..3).all(|column_index| { grid.get_cell(0, column_index) == Some(&Cell::blank()) }));
}

#[test]
fn clear_line_on_an_out_of_range_row_is_a_no_op() {
    let mut grid = build_default_grid(2, 2);
    *grid.get_cell_mut(0, 0).expect("in bounds") = Cell::from_character('q', 1, Style::default());
    grid.clear_line(9, 0, 2, Style::default()); // row is out of range
    assert_eq!(grid.get_cell(0, 0).map(Cell::get_character), Some('q'));
}

#[test]
fn insert_cells_shifts_right_and_drops_overflow() {
    let mut grid = build_default_grid(1, 5);
    write_row(&mut grid, 0, "abcde");
    grid.insert_cells(0, 2, 2, Style::default()); // two blanks at column_index 2
    assert_eq!(get_row_text(&grid, 0), "ab  c"); // c shifts right; d, e fall off
}

#[test]
fn insert_cells_with_excess_count_blanks_to_the_edge() {
    let mut grid = build_default_grid(1, 4);
    write_row(&mut grid, 0, "abcd");
    grid.insert_cells(0, 1, 99, Style::default()); // far more than fits
    assert_eq!(get_row_text(&grid, 0), "a   "); // everything from column 1 is pushed off
    assert_eq!(grid.get_grid_dimensions(), (1, 4)); // column count is preserved
}

#[test]
fn insert_cells_fills_with_the_given_style() {
    let fill_style = build_background_style(Color::Indexed(3));
    let mut grid = build_default_grid(1, 3);
    write_row(&mut grid, 0, "abc");
    grid.insert_cells(0, 0, 1, fill_style);
    assert_eq!(grid.get_cell(0, 0).map(Cell::get_style), Some(fill_style)); // inserted blank carries fill_style
}

#[test]
fn insert_cells_out_of_bounds_is_a_no_op() {
    let mut grid = build_default_grid(2, 3);
    write_row(&mut grid, 0, "xyz");
    grid.insert_cells(9, 0, 1, Style::default()); // row is out of range
    grid.insert_cells(0, 9, 1, Style::default()); // column is out of range
    assert_eq!(get_row_text(&grid, 0), "xyz");
}

#[test]
fn delete_cells_pulls_left_and_pads_the_right() {
    let mut grid = build_default_grid(1, 5);
    write_row(&mut grid, 0, "abcde");
    grid.delete_cells(0, 1, 2, Style::default()); // remove b, c
    assert_eq!(get_row_text(&grid, 0), "ade  ");
}

#[test]
fn delete_cells_clamps_delete_count_and_preserves_column_count() {
    let mut grid = build_default_grid(1, 4);
    write_row(&mut grid, 0, "abcd");
    grid.delete_cells(0, 2, 99, Style::default()); // delete count exceeds the cells to the right
    assert_eq!(get_row_text(&grid, 0), "ab  ");
    assert_eq!(grid.get_grid_dimensions(), (1, 4)); // column count must not grow
}

#[test]
fn delete_cells_fills_with_the_given_style() {
    let fill_style = build_background_style(Color::Indexed(3));
    let mut grid = build_default_grid(1, 3);
    write_row(&mut grid, 0, "abc");
    grid.delete_cells(0, 0, 1, fill_style);
    assert_eq!(grid.get_cell(0, 2).map(Cell::get_style), Some(fill_style)); // pad cell carries fill_style
}

#[test]
fn delete_lines_scrolls_a_band_up_leaving_outside_rows() {
    let mut grid = build_default_grid(4, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    write_row(&mut grid, 3, "DDD");
    grid.delete_lines(1, 2, 1, Style::default()); // band rows 1..=2
    assert_eq!(get_row_text(&grid, 0), "AAA"); // above band, kept
    assert_eq!(get_row_text(&grid, 1), "CCC"); // rose
    assert_eq!(get_row_text(&grid, 2), "   "); // blank at band bottom
    assert_eq!(get_row_text(&grid, 3), "DDD"); // below band, kept
}

#[test]
fn insert_lines_scrolls_a_band_down_dropping_the_bottom() {
    let mut grid = build_default_grid(4, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    write_row(&mut grid, 3, "DDD");
    grid.insert_lines(1, 2, 1, Style::default());
    assert_eq!(get_row_text(&grid, 0), "AAA"); // above band, kept
    assert_eq!(get_row_text(&grid, 1), "   "); // blank opened
    assert_eq!(get_row_text(&grid, 2), "BBB"); // pushed down (CCC fell off band bottom)
    assert_eq!(get_row_text(&grid, 3), "DDD"); // below band, kept
}

#[test]
fn line_operations_clamp_delete_count_to_band_height() {
    let mut grid = build_default_grid(3, 2);
    write_row(&mut grid, 0, "AA");
    write_row(&mut grid, 1, "BB");
    write_row(&mut grid, 2, "CC");
    grid.delete_lines(0, 1, 99, Style::default()); // delete count exceeds the two-row band
    assert_eq!(get_row_text(&grid, 0), "  "); // whole band blanked
    assert_eq!(get_row_text(&grid, 1), "  ");
    assert_eq!(get_row_text(&grid, 2), "CC"); // outside band, kept
    assert_eq!(grid.get_grid_dimensions(), (3, 2));
}

#[test]
fn line_operations_with_an_inverted_or_out_of_bounds_band_are_no_ops() {
    let mut grid = build_default_grid(3, 2);
    write_row(&mut grid, 0, "AA");
    write_row(&mut grid, 1, "BB");
    write_row(&mut grid, 2, "CC");
    grid.delete_lines(2, 1, 1, Style::default()); // first row is after the last row
    grid.insert_lines(0, 9, 1, Style::default()); // last row is out of range
    assert_eq!(get_row_text(&grid, 0), "AA");
    assert_eq!(get_row_text(&grid, 1), "BB");
    assert_eq!(get_row_text(&grid, 2), "CC");
}

#[test]
fn delete_lines_with_a_single_row_band_blanks_only_that_row() {
    // A band with equal first and last rows, such as a collapsed scroll region,
    // one row: exactly that row is blanked.
    let mut grid = build_default_grid(3, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    grid.delete_lines(1, 1, 1, Style::default());
    assert_eq!(get_row_text(&grid, 0), "AAA"); // above the band, untouched
    assert_eq!(get_row_text(&grid, 1), "   "); // the band's one row is blanked
    assert_eq!(get_row_text(&grid, 2), "CCC"); // below the band, untouched
}

#[test]
fn insert_lines_with_a_single_row_band_blanks_only_that_row() {
    let mut grid = build_default_grid(3, 3);
    write_row(&mut grid, 0, "AAA");
    write_row(&mut grid, 1, "BBB");
    write_row(&mut grid, 2, "CCC");
    grid.insert_lines(1, 1, 1, Style::default());
    assert_eq!(get_row_text(&grid, 0), "AAA"); // above the band, untouched
    assert_eq!(get_row_text(&grid, 1), "   "); // the band's one row is blanked
    assert_eq!(get_row_text(&grid, 2), "CCC"); // below the band, untouched
}

/// A row of `row_text`, one default-styled cell per character.
fn build_text_row(row_text: &str) -> Vec<Cell> {
    row_text
        .chars()
        .map(|character| Cell::from_character(character, 1, Style::default()))
        .collect()
}

#[test]
fn from_rows_normalizes_each_row_to_the_given_column_count() {
    // Row 0 is short (padded to 3), row 1 is long (truncated to 3).
    let grid = Grid::from_rows(
        vec![build_text_row("ab"), build_text_row("abcd")],
        3,
        Style::default(),
    );
    assert_eq!(grid.get_grid_dimensions(), (2, 3));
    assert_eq!(get_row_text(&grid, 0), "ab ");
    assert_eq!(get_row_text(&grid, 1), "abc");
}

#[test]
fn from_rows_pads_short_rows_with_the_fill_style() {
    let fill_style = build_background_style(Color::Indexed(4));
    let grid = Grid::from_rows(vec![build_text_row("x")], 3, fill_style);
    // The base char keeps its own (default) style; the two padded cells carry the fill_style.
    assert_eq!(grid.get_cell(0, 0).unwrap().get_style(), Style::default());
    assert_eq!(grid.get_cell(0, 1).unwrap().get_style(), fill_style);
    assert_eq!(grid.get_cell(0, 2).unwrap().get_style(), fill_style);
}

#[test]
fn row_ends_travel_with_scrolled_rows() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(1, RowEnd::Soft);
    // Scroll the whole grid up one line: old row 1 lands on row 0 with its
    // continuation state; the fresh bottom row is a hard end.
    grid.delete_lines(0, 2, 1, Style::default());
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
    assert_eq!(grid.get_row_end(2), RowEnd::Hard);
}

#[test]
fn delete_lines_breaks_the_continuation_above_the_band() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(0, RowEnd::Soft); // row 0 wrapped into row 1
    grid.delete_lines(1, 2, 1, Style::default());
    // Row 0's continuation row is gone: the wrap no longer holds.
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
}

#[test]
fn insert_lines_breaks_continuations_at_the_band_edges() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::Soft);
    grid.insert_lines(1, 2, 1, Style::default());
    // Row 0 precedes an inserted blank; the row shifted to the band's bottom
    // precedes a row it never wrapped into.
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
    assert_eq!(grid.get_row_end(2), RowEnd::Hard);
}

#[test]
fn tail_edits_reset_the_row_end() {
    let mut grid = Grid::blank(1, 4, Style::default());

    grid.set_row_end(0, RowEnd::Soft);
    grid.clear_line(0, 2, 4, Style::default()); // reaches the last column
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);

    grid.set_row_end(0, RowEnd::Soft);
    grid.clear_line(0, 0, 2, Style::default()); // stops short of it
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);

    grid.insert_cells(0, 1, 1, Style::default()); // shifts the tail
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);

    grid.set_row_end(0, RowEnd::Soft);
    grid.delete_cells(0, 1, 1, Style::default()); // shifts the tail
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
}

#[test]
fn row_end_out_of_bounds_reads_hard_and_ignores_writes() {
    let mut grid = Grid::blank(2, 2, Style::default());
    assert_eq!(grid.get_row_end(9), RowEnd::Hard);
    grid.set_row_end(9, RowEnd::Soft); // no-op, no panic
    assert_eq!(grid.get_row_end(9), RowEnd::Hard);
}

#[test]
fn prompt_marks_travel_with_scrolled_rows() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_prompt_mark(1, true);

    grid.delete_lines(0, 2, 1, Style::default());

    assert!(grid.has_prompt_mark(0));
    assert!(!grid.has_prompt_mark(2));
}

#[test]
fn prompt_marks_travel_with_inserted_rows_and_cell_edits() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_prompt_mark(1, true);

    grid.insert_lines(1, 2, 1, Style::default());
    grid.clear_line(2, 0, 4, Style::default());

    assert!(!grid.has_prompt_mark(1));
    assert!(grid.has_prompt_mark(2));
}

#[test]
fn serialized_grid_state_round_trips_row_metadata() {
    let mut grid = Grid::blank(2, 2, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_prompt_mark(0, true);

    let serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    let restored_grid: Grid = serde_json::from_value(serialized_grid).expect("grid deserializes");

    assert_eq!(restored_grid.get_row_end(0), RowEnd::Soft);
    assert!(restored_grid.has_prompt_mark(0));
}

#[test]
fn legacy_row_end_grid_state_deserializes_with_unmarked_rows() {
    let mut grid = Grid::blank(2, 2, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_prompt_mark(0, true);
    let mut serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    let serialized_grid_object = serialized_grid.as_object_mut().expect("grid is an object");
    let serialized_row_metadata = serialized_grid_object
        .remove("row_meta")
        .expect("current metadata exists");
    let serialized_row_end_values = serialized_row_metadata
        .as_array()
        .expect("row metadata is an array")
        .iter()
        .map(|row_metadata| row_metadata["end"].clone())
        .collect();
    serialized_grid_object.insert(
        "row_ends".to_string(),
        serde_json::Value::Array(serialized_row_end_values),
    );

    let restored_grid: Grid =
        serde_json::from_value(serialized_grid).expect("legacy grid deserializes");

    assert_eq!(restored_grid.get_row_end(0), RowEnd::Soft);
    assert!(!restored_grid.has_prompt_mark(0));
}

#[test]
fn content_cell_count_of_an_empty_row_is_zero() {
    assert_eq!(count_row_content_cells(&[]), 0);
}

#[test]
fn content_cell_count_of_an_all_blank_row_is_zero() {
    assert_eq!(count_row_content_cells(&vec![Cell::blank(); 4]), 0);
}

#[test]
fn content_cell_count_stops_after_the_last_non_default_cell() {
    let mut row_cells = build_text_row("ab");
    row_cells.resize(6, Cell::blank());
    assert_eq!(count_row_content_cells(&row_cells), 2);
}

#[test]
fn content_cell_count_includes_a_styled_blank() {
    let mut row_cells = build_text_row("a");
    row_cells.push(Cell::blank_with(build_background_style(Color::Indexed(1))));
    row_cells.resize(6, Cell::blank());
    assert_eq!(count_row_content_cells(&row_cells), 2);
}

#[test]
fn content_cell_count_includes_a_wide_glyph_continuation_cell() {
    let mut row_cells = vec![
        Cell::from_character('漢', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ];
    row_cells.resize(6, Cell::blank());
    assert_eq!(count_row_content_cells(&row_cells), 2);
}

#[test]
fn content_cell_count_includes_a_blank_with_a_combining_mark() {
    let mut marked_cell = Cell::blank();
    marked_cell.push_combining('\u{0301}');
    let row_cells = vec![marked_cell, Cell::blank(), Cell::blank()];
    assert_eq!(count_row_content_cells(&row_cells), 1);
}

#[test]
fn from_rows_with_metadata_keeps_each_row_metadata_and_normalizes_column_count() {
    let grid = Grid::from_rows_with_metadata(
        vec![
            (
                build_text_row("ab"),
                RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: true,
                },
            ),
            (
                build_text_row("abcd"),
                RowMetadata {
                    row_end: RowEnd::SoftWide,
                    has_prompt_mark: false,
                },
            ),
        ],
        3,
        Style::default(),
    );
    assert_eq!(grid.get_grid_dimensions(), (2, 3));
    assert_eq!(get_row_text(&grid, 0), "ab ");
    assert_eq!(get_row_text(&grid, 1), "abc");
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
    assert!(grid.has_prompt_mark(0));
    assert_eq!(grid.get_row_end(1), RowEnd::SoftWide);
    assert!(!grid.has_prompt_mark(1));
}

#[test]
fn from_rows_with_no_rows_is_an_empty_grid() {
    let grid = Grid::from_rows(Vec::new(), 3, Style::default());
    assert_eq!(grid.get_grid_dimensions(), (0, 0));
    assert!(grid.list_rows().is_empty());
}

#[test]
fn from_rows_with_zero_columns_empties_every_row() {
    let grid = Grid::from_rows(vec![build_text_row("ab")], 0, Style::default());
    assert_eq!(grid.get_grid_dimensions(), (1, 0));
    assert_eq!(grid.list_rows(), &[Vec::<Cell>::new()]);
}

#[test]
fn row_end_and_prompt_mark_on_an_empty_grid_read_as_defaults() {
    let mut grid = build_default_grid(0, 0);
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
    assert!(!grid.has_prompt_mark(0));
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_prompt_mark(0, true);
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
    assert!(!grid.has_prompt_mark(0));
}

#[test]
fn prompt_mark_out_of_bounds_reads_false_and_ignores_writes() {
    let mut grid = Grid::blank(2, 2, Style::default());
    assert!(!grid.has_prompt_mark(2));
    grid.set_prompt_mark(2, true);
    assert!(!grid.has_prompt_mark(2));
    grid.set_prompt_mark(1, true);
    grid.set_prompt_mark(1, false);
    assert!(!grid.has_prompt_mark(1));
}

#[test]
fn clear_line_starting_past_the_row_leaves_the_row_end_alone() {
    let mut grid = Grid::blank(1, 4, Style::default());
    write_row(&mut grid, 0, "abcd");
    grid.set_row_end(0, RowEnd::Soft);
    grid.clear_line(0, 4, 9, Style::default()); // first column equals the column count
    assert_eq!(get_row_text(&grid, 0), "abcd");
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
}

#[test]
fn clear_line_of_only_the_last_column_breaks_the_row_end() {
    let mut grid = Grid::blank(1, 4, Style::default());
    write_row(&mut grid, 0, "abcd");
    grid.set_row_end(0, RowEnd::Soft);
    grid.clear_line(0, 3, 4, Style::default());
    assert_eq!(get_row_text(&grid, 0), "abc ");
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
}

#[test]
fn clear_line_on_a_zero_column_grid_is_a_no_op() {
    let mut grid = build_default_grid(1, 0);
    grid.clear_line(0, 0, 0, Style::default());
    assert_eq!(grid.get_grid_dimensions(), (1, 0));
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
}

#[test]
fn delete_lines_with_a_zero_count_moves_nothing_and_keeps_row_ends() {
    let mut grid = build_default_grid(3, 2);
    write_row(&mut grid, 0, "AA");
    write_row(&mut grid, 1, "BB");
    write_row(&mut grid, 2, "CC");
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(2, RowEnd::Soft);
    grid.delete_lines(1, 2, 0, Style::default());
    assert_eq!(get_row_text(&grid, 0), "AA");
    assert_eq!(get_row_text(&grid, 1), "BB");
    assert_eq!(get_row_text(&grid, 2), "CC");
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
    assert_eq!(grid.get_row_end(2), RowEnd::Soft);
}

#[test]
fn delete_cells_at_the_last_column_blanks_only_that_cell() {
    let mut grid = build_default_grid(1, 4);
    write_row(&mut grid, 0, "abcd");
    grid.delete_cells(0, 3, 1, Style::default());
    assert_eq!(get_row_text(&grid, 0), "abc ");
    assert_eq!(grid.get_grid_dimensions(), (1, 4));
}

#[test]
fn delete_cells_out_of_bounds_is_a_no_op() {
    let mut grid = build_default_grid(2, 3);
    write_row(&mut grid, 0, "xyz");
    grid.set_row_end(0, RowEnd::Soft);
    grid.delete_cells(9, 0, 1, Style::default()); // row is out of range
    grid.delete_cells(0, 3, 1, Style::default()); // column index equals the column count
    assert_eq!(get_row_text(&grid, 0), "xyz");
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
}

#[test]
fn a_cell_with_combining_marks_round_trips_through_serde() {
    let mut cell = Cell::from_character('e', 1, build_background_style(Color::Indexed(5)));
    cell.push_combining('\u{0301}');
    cell.push_combining('\u{0308}');
    let serialized_cell = serde_json::to_value(&cell).expect("cell serializes");
    let restored_cell: Cell = serde_json::from_value(serialized_cell).expect("cell deserializes");
    assert_eq!(restored_cell, cell);
    assert_eq!(
        restored_cell.list_combining_characters(),
        &['\u{0301}', '\u{0308}']
    );
}

#[test]
fn serialized_grid_without_row_metadata_deserializes_with_default_rows() {
    let mut grid = Grid::blank(2, 2, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_prompt_mark(1, true);
    let mut serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    let serialized_grid_object = serialized_grid.as_object_mut().expect("grid is an object");
    serialized_grid_object
        .remove("row_meta")
        .expect("current metadata exists");

    let restored_grid: Grid =
        serde_json::from_value(serialized_grid).expect("bare grid deserializes");

    assert_eq!(restored_grid.get_grid_dimensions(), (2, 2));
    assert_eq!(restored_grid.get_row_end(0), RowEnd::Hard);
    assert!(!restored_grid.has_prompt_mark(1));
}

#[test]
fn serialized_grid_with_both_metadata_forms_takes_the_current_one() {
    let mut grid = Grid::blank(2, 2, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_prompt_mark(0, true);
    let mut serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    let serialized_grid_object = serialized_grid.as_object_mut().expect("grid is an object");
    serialized_grid_object.insert(
        "row_ends".to_string(),
        serde_json::json!(["Hard", "SoftWide"]),
    );

    let restored_grid: Grid = serde_json::from_value(serialized_grid).expect("grid deserializes");

    assert_eq!(restored_grid.get_row_end(0), RowEnd::Soft);
    assert!(restored_grid.has_prompt_mark(0));
    assert_eq!(restored_grid.get_row_end(1), RowEnd::Hard);
}

#[test]
fn serialized_grid_with_metadata_count_different_from_rows_is_rejected() {
    let grid = Grid::blank(2, 2, Style::default());
    let mut serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    let serialized_grid_object = serialized_grid.as_object_mut().expect("grid is an object");
    serialized_grid_object
        .get_mut("row_meta")
        .and_then(serde_json::Value::as_array_mut)
        .expect("row metadata is an array")
        .pop();

    let grid_deserialization_error =
        serde_json::from_value::<Grid>(serialized_grid).expect_err("one row lacks metadata");

    assert_eq!(
        grid_deserialization_error.to_string(),
        "grid row metadata does not match rows"
    );
}

#[test]
fn legacy_serialized_grid_with_row_end_count_different_from_rows_is_rejected() {
    let grid = Grid::blank(2, 2, Style::default());
    let mut serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    let serialized_grid_object = serialized_grid.as_object_mut().expect("grid is an object");
    serialized_grid_object
        .remove("row_meta")
        .expect("current metadata exists");
    serialized_grid_object.insert("row_ends".to_string(), serde_json::json!(["Hard"]));

    let grid_deserialization_error =
        serde_json::from_value::<Grid>(serialized_grid).expect_err("one row lacks a row end");

    assert_eq!(
        grid_deserialization_error.to_string(),
        "grid row metadata does not match rows"
    );
}

#[test]
fn serialized_grid_with_rows_different_in_length_is_rejected() {
    // Every row carries the same number of cells.
    let grid = Grid::blank(2, 3, Style::default());
    let mut serialized_grid = serde_json::to_value(&grid).expect("grid serializes");
    serialized_grid["rows"][1]
        .as_array_mut()
        .expect("the second row is an array")
        .truncate(1);

    let grid_deserialization_error =
        serde_json::from_value::<Grid>(serialized_grid).expect_err("ragged rows are refused");

    assert_eq!(
        grid_deserialization_error.to_string(),
        "grid rows differ in length"
    );
}

#[test]
fn insert_lines_with_a_zero_count_moves_nothing_and_keeps_row_ends() {
    // Nothing slides, so no row starts preceding a row it never wrapped into.
    let mut grid = build_default_grid(3, 2);
    write_row(&mut grid, 0, "AA");
    write_row(&mut grid, 1, "BB");
    write_row(&mut grid, 2, "CC");
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(2, RowEnd::Soft);

    grid.insert_lines(1, 2, 0, Style::default());

    assert_eq!(get_row_text(&grid, 1), "BB");
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
    assert_eq!(grid.get_row_end(2), RowEnd::Soft);
}

#[test]
fn row_metadata_reads_both_facts_at_once_and_defaults_out_of_bounds() {
    let mut grid = Grid::blank(2, 2, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_prompt_mark(0, true);

    assert_eq!(
        grid.get_row_metadata(0),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: true
        }
    );
    assert_eq!(grid.get_row_metadata(1), RowMetadata::default());
    assert_eq!(grid.get_row_metadata(9), RowMetadata::default());
}
