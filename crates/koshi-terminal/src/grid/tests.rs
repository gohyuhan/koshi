//! Unit tests for the grid module's public surface: the [`RowEnd`]
//! continuation kinds and the [`Cell`] accessors reached through
//! `crate::grid::state`.

use super::state::{Cell, Grid, RowEnd, RowMetadata};
use crate::style::Style;

#[test]
fn row_end_defaults_to_hard() {
    assert_eq!(RowEnd::default(), RowEnd::Hard);
}

#[test]
fn row_metadata_defaults_to_a_hard_end_with_no_prompt_mark() {
    assert_eq!(
        RowMetadata::default(),
        RowMetadata {
            row_end: RowEnd::Hard,
            has_prompt_mark: false,
        }
    );
}

#[test]
fn each_row_end_kind_round_trips_through_set_and_read() {
    let mut grid = Grid::blank(2, 3, Style::default());

    grid.set_row_end(0, RowEnd::Soft);
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);

    grid.set_row_end(0, RowEnd::SoftWide);
    assert_eq!(grid.get_row_end(0), RowEnd::SoftWide);

    grid.set_row_end(0, RowEnd::Hard);
    assert_eq!(grid.get_row_end(0), RowEnd::Hard);
}

#[test]
fn a_soft_wide_row_end_travels_with_a_scrolled_row() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(1, RowEnd::SoftWide);
    // Scroll the whole grid up one line: old row 1 lands on row 0 keeping its
    // wide-glyph continuation; the fresh bottom row ends hard.
    grid.delete_lines(0, 2, 1, Style::default());
    assert_eq!(grid.get_row_end(0), RowEnd::SoftWide);
    assert_eq!(grid.get_row_end(2), RowEnd::Hard);
}

#[test]
fn partial_line_scroll_moves_cells_without_moving_row_metadata() {
    let mut grid = Grid::blank(3, 3, Style::default());
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::SoftWide);
    grid.set_prompt_mark(2, true);
    *grid.get_cell_mut(1, 1).expect("the cell exists") =
        Cell::from_character('x', 1, Style::default());

    grid.delete_lines_in_columns(0, 2, 1, 1, 1, Style::default());

    assert_eq!(grid.get_cell(0, 1).map(Cell::get_character), Some('x'));
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
    assert_eq!(grid.get_row_end(1), RowEnd::SoftWide);
    assert!(grid.get_row_metadata(2).has_prompt_mark);
}

#[test]
fn cells_differing_only_by_a_combining_mark_are_not_equal() {
    let plain = Cell::from_character('e', 1, Style::default());
    let mut accented = Cell::from_character('e', 1, Style::default());
    accented.push_combining('\u{0301}'); // combining acute accent
    assert_ne!(plain, accented);

    let mut same_accent = Cell::from_character('e', 1, Style::default());
    same_accent.push_combining('\u{0301}');
    assert_eq!(accented, same_accent);
}

#[test]
fn a_blank_cell_carries_no_combining_marks() {
    assert_eq!(Cell::blank().list_combining_characters(), &[] as &[char]);
}

#[test]
fn a_wide_glyph_and_its_continuation_report_their_display_widths() {
    let wide = Cell::from_character('世', 2, Style::default());
    assert_eq!(wide.get_character(), '世');
    assert_eq!(wide.get_display_width(), 2);

    // The trailing half of a wide glyph is a zero-width continuation cell.
    let continuation = Cell::from_character(' ', 0, Style::default());
    assert_eq!(continuation.get_display_width(), 0);
}
