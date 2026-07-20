//! Unit tests for the grid module's public surface: the [`RowEnd`]
//! continuation kinds and the [`Cell`] accessors reached through
//! `crate::grid::state`.

use super::state::{Cell, Grid, RowEnd};
use crate::style::Style;

#[test]
fn row_end_defaults_to_hard() {
    assert_eq!(RowEnd::default(), RowEnd::Hard);
}

#[test]
fn each_row_end_kind_round_trips_through_set_and_read() {
    let mut grid = Grid::blank(2, 3, Style::default());

    grid.set_row_end(0, RowEnd::Soft);
    assert_eq!(grid.row_end(0), RowEnd::Soft);

    grid.set_row_end(0, RowEnd::SoftWide);
    assert_eq!(grid.row_end(0), RowEnd::SoftWide);

    grid.set_row_end(0, RowEnd::Hard);
    assert_eq!(grid.row_end(0), RowEnd::Hard);
}

#[test]
fn a_soft_wide_row_end_travels_with_a_scrolled_row() {
    let mut grid = Grid::blank(3, 4, Style::default());
    grid.set_row_end(1, RowEnd::SoftWide);
    // Scroll the whole grid up one line: old row 1 lands on row 0 keeping its
    // wide-glyph continuation; the fresh bottom row ends hard.
    grid.delete_lines(0, 2, 1, Style::default());
    assert_eq!(grid.row_end(0), RowEnd::SoftWide);
    assert_eq!(grid.row_end(2), RowEnd::Hard);
}

#[test]
fn cells_differing_only_by_a_combining_mark_are_not_equal() {
    let plain = Cell::new('e', 1, Style::default());
    let mut accented = Cell::new('e', 1, Style::default());
    accented.push_combining('\u{0301}'); // combining acute accent
    assert_ne!(plain, accented);

    let mut same_accent = Cell::new('e', 1, Style::default());
    same_accent.push_combining('\u{0301}');
    assert_eq!(accented, same_accent);
}

#[test]
fn a_blank_cell_carries_no_combining_marks() {
    assert_eq!(Cell::blank().combining(), &[] as &[char]);
}

#[test]
fn a_wide_glyph_and_its_continuation_report_their_display_widths() {
    let wide = Cell::new('世', 2, Style::default());
    assert_eq!(wide.ch(), '世');
    assert_eq!(wide.width(), 2);

    // The trailing half of a wide glyph is a zero-width continuation cell.
    let continuation = Cell::new(' ', 0, Style::default());
    assert_eq!(continuation.width(), 0);
}
