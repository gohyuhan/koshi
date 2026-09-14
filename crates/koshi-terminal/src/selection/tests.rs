//! Tests for reading a pane's text by absolute row and growing a selection to
//! whole words or lines.

use super::*;

use koshi_core::{command::GridPosition, process::PtySize};

use crate::engine::TerminalEngine;
use crate::scrollback::ScrollbackLimit;
use crate::style::Style;

/// A grid holding `row_texts`, each padded to `column_count` with blanks.
fn grid_of(row_texts: &[&str], column_count: u16) -> Grid {
    let cells: Vec<Vec<Cell>> = row_texts
        .iter()
        .map(|line| {
            line.chars()
                .map(|ch| Cell::from_character(ch, 1, Style::default()))
                .collect()
        })
        .collect();
    Grid::from_rows(cells, column_count, Style::default())
}

/// One text line as grid cells.
fn cells_of(line: &str) -> Vec<Cell> {
    line.chars()
        .map(|ch| Cell::from_character(ch, 1, Style::default()))
        .collect()
}

/// A scrollback holding `lines`, each ended hard, under a `maximum_line_count` cap.
fn scrollback_of(lines: &[&str], maximum_line_count: usize) -> Scrollback {
    let mut scrollback = Scrollback::from_scrollback_limit(
        ScrollbackLimit::from_line_and_byte_limits(maximum_line_count, usize::MAX),
    );
    for line in lines {
        scrollback.push_row(&cells_of(line), RowMetadata::default());
    }
    scrollback
}

/// Read a row back as a trimmed string, for asserting which line a number names.
fn get_row_text(view: &TextView<'_>, row_index: u64) -> String {
    let (cells, _) = view.get_row(row_index).expect("row is readable");
    cells
        .iter()
        .map(Cell::get_character)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn the_live_screens_top_row_is_the_lines_pushed_so_far() {
    let scrollback = scrollback_of(&["old0", "old1", "old2"], 100);
    let grid = grid_of(&["live0", "live1"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // Three lines pushed, so the screen's top row is line 3 and history is 0..=2.
    assert_eq!(view.get_first_row_index(), 0);
    assert_eq!(view.get_last_row_index(), 4);
    assert_eq!(get_row_text(&view, 0), "old0");
    assert_eq!(get_row_text(&view, 2), "old2");
    assert_eq!(get_row_text(&view, 3), "live0");
    assert_eq!(get_row_text(&view, 4), "live1");
}

#[test]
fn a_row_number_still_names_the_same_line_after_more_output() {
    let mut scrollback = scrollback_of(&["old0", "old1", "old2"], 100);
    {
        let grid = grid_of(&["live0", "live1"], 10);
        let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
        assert_eq!(get_row_text(&view, 2), "old2");
        assert_eq!(get_row_text(&view, 3), "live0");
    }

    // `live0` and `live1` scroll off into history; two fresh lines take their place.
    scrollback.push_row(&cells_of("live0"), RowMetadata::default());
    scrollback.push_row(&cells_of("live1"), RowMetadata::default());
    let grid = grid_of(&["new0", "new1"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // Every line kept its number: row 3 is still `live0`, now in history.
    assert_eq!(get_row_text(&view, 2), "old2");
    assert_eq!(get_row_text(&view, 3), "live0");
    assert_eq!(get_row_text(&view, 4), "live1");
    assert_eq!(get_row_text(&view, 5), "new0");
}

#[test]
fn a_row_number_still_names_the_same_line_after_the_cap_drops_history() {
    // A cap of 2 keeps only the two newest history lines: pushing four drops the
    // two oldest, which is the moment a from-the-top numbering would renumber.
    let scrollback = scrollback_of(&["old0", "old1", "old2", "old3"], 2);
    let grid = grid_of(&["live0"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        scrollback.get_dropped_line_count(),
        2,
        "the cap dropped two lines"
    );
    assert_eq!(
        view.get_first_row_index(),
        2,
        "rows 0 and 1 are gone; 2 is the oldest"
    );
    assert_eq!(get_row_text(&view, 2), "old2", "row 2 still names old2");
    assert_eq!(get_row_text(&view, 3), "old3");
    assert_eq!(get_row_text(&view, 4), "live0");
    assert_eq!(view.get_row(1), None, "a dropped row reads as gone");
    assert_eq!(view.get_row(0), None);
}

#[test]
fn erasing_saved_lines_leaves_surviving_rows_their_numbers() {
    // `clear_scrollback` (ED 3, erase saved lines) empties history without counting as a
    // cap-driven drop, so `dropped_lines` does not move. The numbering must not
    // rely on that counter.
    let mut scrollback = scrollback_of(&["old0", "old1", "old2"], 100);
    scrollback.clear_scrollback();
    scrollback.push_row(&cells_of("after"), RowMetadata::default());
    let grid = grid_of(&["live0"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        scrollback.get_dropped_line_count(),
        0,
        "an erase is not a truncation"
    );
    assert_eq!(
        scrollback.get_total_pushed_line_count(),
        4,
        "four lines were ever pushed"
    );
    assert_eq!(
        view.get_first_row_index(),
        3,
        "only the line pushed after the erase is left"
    );
    assert_eq!(get_row_text(&view, 3), "after");
    assert_eq!(get_row_text(&view, 4), "live0");
    assert_eq!(view.get_row(0), None, "the erased lines are gone");
}

#[test]
fn a_row_past_the_bottom_of_the_screen_is_gone() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["a", "b"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_last_row_index(), 1);
    let mut bottom = cells_of("b");
    bottom.resize(10, Cell::blank());
    assert_eq!(view.get_row(1), Some((bottom.as_slice(), RowEnd::Hard)));
    assert_eq!(view.get_row(2), None);
}

#[test]
fn a_screen_with_no_history_starts_at_row_zero() {
    // What the alternate screen looks like: it keeps no scrollback, so the view
    // is the screen alone.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["alt0", "alt1"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_first_row_index(), 0);
    assert_eq!(view.get_last_row_index(), 1);
}

#[test]
fn a_word_grows_to_its_separators() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["cargo build"], 11);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // The pointer on the `i` of `build` (column 8).
    assert_eq!(
        view.get_word_start_position(0, 8),
        (0, 6),
        "the word starts at the `b`"
    );
    assert_eq!(
        view.get_word_end_position(0, 8),
        (0, 10),
        "and ends at the `d`"
    );
    // And on the `r` of `cargo` (column 2).
    assert_eq!(view.get_word_start_position(0, 2), (0, 0));
    assert_eq!(
        view.get_word_end_position(0, 2),
        (0, 4),
        "the space after `cargo` stops it"
    );
}

#[test]
fn a_path_is_one_word_because_slashes_are_not_separators() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["cd /usr/local/bin"], 17);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // The pointer inside `local`: the whole path comes out, not one segment.
    assert_eq!(
        view.get_word_start_position(0, 12),
        (0, 3),
        "back to the leading slash"
    );
    assert_eq!(
        view.get_word_end_position(0, 12),
        (0, 16),
        "on to the end of `bin`"
    );
}

#[test]
fn a_dotted_filename_is_one_word() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["tar xf foo.tar.gz"], 17);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        view.get_word_start_position(0, 12),
        (0, 7),
        "dots do not split the name"
    );
    assert_eq!(view.get_word_end_position(0, 12), (0, 16));
}

#[test]
fn brackets_and_quotes_stop_a_word() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["(foo bar)"], 9);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // The pointer on `foo`: the paren and the space bound it.
    assert_eq!(view.get_word_start_position(0, 2), (0, 1));
    assert_eq!(view.get_word_end_position(0, 2), (0, 3));
}

#[test]
fn a_word_follows_the_text_across_a_soft_wrap() {
    let scrollback = scrollback_of(&[], 100);
    // `abcde` wrapped after `abc`: one logical word split over two rows.
    let mut grid = grid_of(&["abc", "de "], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // From the `d` on row 1, the word runs back onto row 0.
    assert_eq!(
        view.get_word_start_position(1, 0),
        (0, 0),
        "back across the wrap to the `a`"
    );
    assert_eq!(
        view.get_word_end_position(0, 0),
        (1, 1),
        "and forward across it to the `e`"
    );
}

#[test]
fn a_word_stops_at_a_hard_line_end() {
    let scrollback = scrollback_of(&[], 100);
    // Two separate lines: row 0 ends hard, so `abc` and `def` are not one word.
    let grid = grid_of(&["abc", "def"], 3);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        view.get_word_start_position(1, 0),
        (1, 0),
        "a new line starts a new word"
    );
    assert_eq!(
        view.get_word_end_position(0, 0),
        (0, 2),
        "the line end stops it"
    );
}

#[test]
fn a_word_crosses_a_soft_wrap_out_of_history_onto_the_screen() {
    // The wrap runs across the history/screen boundary: the last history line
    // wrapped into the screen's top row, so the word spans both.
    let mut scrollback = Scrollback::from_scrollback_limit(
        ScrollbackLimit::from_line_and_byte_limits(100, usize::MAX),
    );
    scrollback.push_row(
        &cells_of("abc"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );
    let grid = grid_of(&["def"], 3);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // Row 0 is history, row 1 the live screen: one word `abcdef` over both.
    assert_eq!(view.get_word_start_position(1, 0), (0, 0));
    assert_eq!(view.get_word_end_position(0, 0), (1, 2));
}

#[test]
fn a_live_autowrap_keeps_one_word_across_history_and_screen() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let _ = engine.process_pty_output(b"abcdefg");
    let view = engine.get_terminal_state().get_text_view();

    assert_eq!(get_row_text(&view, 0), "abc");
    assert_eq!(get_row_text(&view, 1), "def");
    assert_eq!(get_row_text(&view, 2), "g");
    assert_eq!(view.get_word_start_position(2, 0), (0, 0));
    assert_eq!(view.get_word_end_position(0, 0), (2, 0));
}

#[test]
fn a_wide_wrap_spacer_is_neither_a_word_break_nor_copied_text() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let _ = engine.process_pty_output("abcde世".as_bytes());
    let view = engine.get_terminal_state().get_text_view();

    assert!(view.is_wide_wrap_spacer(1, 2));
    assert_eq!(view.get_word_start_position(2, 0), (0, 0));
    assert_eq!(view.get_word_end_position(0, 0), (2, 0));
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 2,
            column_index: 1,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, false),
        "abcde世"
    );
}

#[test]
fn a_logical_line_spans_every_row_it_wrapped_over() {
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["one", "two", "thr", "end"], 3);
    // Rows 0..=2 are one logical line; row 3 is its own.
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        view.get_line_start_row_index(1),
        0,
        "up to the first row of the wrap"
    );
    assert_eq!(view.get_line_end_row_index(1), 2, "down to the last");
    assert_eq!(
        view.get_line_start_row_index(3),
        3,
        "the next line stands alone"
    );
    assert_eq!(view.get_line_end_row_index(3), 3);
}

#[test]
fn a_logical_line_reaching_the_oldest_row_stops_there() {
    // The row wraps onward, but the walk up has nothing older to read: it stops
    // at the oldest readable row rather than running off.
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["abc", "def"], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        view.get_line_start_row_index(0),
        0,
        "already at the oldest readable row"
    );
    assert_eq!(
        view.get_line_end_row_index(1),
        1,
        "and the newest row ends the walk down"
    );
}

#[test]
fn a_wide_glyphs_blank_half_is_skipped_when_growing_a_word() {
    let scrollback = scrollback_of(&[], 100);
    // `世界` — each glyph is two cells wide, its right half a width-0 blank.
    let cells = vec![vec![
        Cell::from_character('世', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
        Cell::from_character('界', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // Growing from the first glyph lands on the second glyph's own cell (2),
    // never on a blank right half (1 or 3), which would split a glyph.
    assert_eq!(view.get_word_end_position(0, 0), (0, 2));
    assert_eq!(view.get_word_start_position(0, 2), (0, 0));
}

#[test]
fn ordering_puts_the_earlier_end_first() {
    let earlier_position = GridPosition {
        row_index: 3,
        column_index: 10,
    };
    let following_position = GridPosition {
        row_index: 5,
        column_index: 2,
    };

    // Dragging down: already in order.
    let ordered = order_selection_positions(earlier_position, following_position);
    assert_eq!(ordered.start_position, earlier_position);
    assert_eq!(ordered.end_position, following_position);

    // Dragging up: the anchor is the following end, so the pair is swapped.
    let ordered = order_selection_positions(following_position, earlier_position);
    assert_eq!(ordered.start_position, earlier_position);
    assert_eq!(ordered.end_position, following_position);
}

#[test]
fn ordering_within_one_row_compares_columns() {
    let left = GridPosition {
        row_index: 4,
        column_index: 1,
    };
    let right = GridPosition {
        row_index: 4,
        column_index: 9,
    };

    let ordered = order_selection_positions(right, left);
    assert_eq!(ordered.start_position, left);
    assert_eq!(ordered.end_position, right);
}

#[test]
fn a_screen_with_no_history_cannot_read_the_rows_below_it() {
    // The alternate screen keeps no history of its own, yet the pane's
    // scrollback — the PRIMARY's — is still there. A view built for it must not
    // reach those rows: they are another screen's text.
    let scrollback = scrollback_of(&["primary0", "primary1"], 100);
    let grid = grid_of(&["alt0", "alt1"], 10);
    let view =
        TextView::from_grid_without_scrollback(&grid, scrollback.get_total_pushed_line_count());

    // Rows number from the same base, so a position means the same thing here
    // as on the primary...
    assert_eq!(
        view.get_first_row_index(),
        2,
        "the screen's first row is still line 2"
    );
    assert_eq!(view.get_last_row_index(), 3);
    // ...but nothing below the screen is readable.
    assert_eq!(view.get_row(1), None, "the primary's newest history row");
    assert_eq!(view.get_row(0), None, "and the one before it");
    assert_eq!(get_row_text(&view, 2), "alt0");
}

#[test]
fn a_word_on_a_screen_with_no_history_stops_at_its_top_row() {
    // The case the pairing bug produced: the row below the screen's top ends
    // SOFT, so a walk that could see history would step into it. With no
    // history there is nothing to step into.
    let mut scrollback = Scrollback::from_scrollback_limit(
        ScrollbackLimit::from_line_and_byte_limits(100, usize::MAX),
    );
    scrollback.push_row(
        &cells_of("abc"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );
    let grid = grid_of(&["def"], 3);

    // Built for the primary, the word crosses the boundary — correct there.
    let primary_text_view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    assert_eq!(
        primary_text_view.get_word_start_position(1, 0),
        (0, 0),
        "the primary joins the wrap"
    );

    // Built for a screen with no history, the same walk stops dead.
    let alternate =
        TextView::from_grid_without_scrollback(&grid, scrollback.get_total_pushed_line_count());
    assert_eq!(
        alternate.get_word_start_position(1, 0),
        (1, 0),
        "nothing above the screen's top row to grow into"
    );
    assert_eq!(alternate.get_line_start_row_index(1), 1);
}

#[test]
fn a_word_grows_the_same_from_either_half_of_a_wide_glyph() {
    // A wide glyph's right half is a width-0 blank, and a blank is a word
    // separator. Landing on one still grows the whole word: the separator test
    // is applied to the NEIGHBOUR the walk steps to, and the walk skips width-0
    // cells. A double click anywhere on `世界` selects both.
    let cells = vec![vec![
        Cell::from_character('世', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
        Cell::from_character('界', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // From the first glyph's own cell.
    assert_eq!(view.get_word_start_position(0, 0), (0, 0));
    assert_eq!(
        view.get_word_end_position(0, 0),
        (0, 2),
        "reaches the second glyph"
    );

    // From its blank right half — the same answer, not a one-cell selection on
    // the blank.
    assert_eq!(
        view.get_word_start_position(0, 1),
        (0, 0),
        "back to the glyph it belongs to"
    );
    assert_eq!(view.get_word_end_position(0, 1), (0, 2));

    // And from the second glyph.
    assert_eq!(view.get_word_start_position(0, 2), (0, 0));
    assert_eq!(view.get_word_end_position(0, 2), (0, 2));
}

#[test]
fn a_word_lookup_on_a_separator_covers_the_run_of_that_same_separator() {
    // Double-clicking the gap in `foo  bar` must select the two spaces, never
    // `foo  bar` entire: a lookup that starts ON a separator grows over the
    // run of that character, not into the words around it.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["foo  bar"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    // The spaces sit at columns 3 and 4; from either one the answer is the run.
    assert_eq!(view.get_word_start_position(0, 3), (0, 3));
    assert_eq!(view.get_word_end_position(0, 3), (0, 4));
    assert_eq!(view.get_word_start_position(0, 4), (0, 3));
    assert_eq!(view.get_word_end_position(0, 4), (0, 4));
}

#[test]
fn selection_text_reads_the_range_inclusive() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["hello world"], 11);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 6,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 10,
        },
    };
    assert_eq!(serialize_selection_text(&view, &selection, true), "world");
}

#[test]
fn selection_text_joins_hard_rows_with_newlines_and_soft_wraps_with_nothing() {
    // `abc` wraps into `def` (one logical line), then `ghi` starts fresh.
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["abc", "def", "ghi"], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 2,
            column_index: 2,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "abcdef\nghi"
    );
}

#[test]
fn selection_text_takes_the_same_columns_from_every_block_row() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abcde", "fghij", "klmno"], 5);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Block,
        anchor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
        cursor: GridPosition {
            row_index: 2,
            column_index: 3,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "bcd\nghi\nlmn"
    );
}

#[test]
fn selection_text_applies_the_trim_setting_to_every_selection_kind() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["ab  ", "c   "], 4);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    for selection_kind in [
        SelectionKind::Character,
        SelectionKind::Word,
        SelectionKind::Line,
    ] {
        let selection = Selection {
            selection_kind,
            anchor: GridPosition {
                row_index: 0,
                column_index: 0,
            },
            cursor: GridPosition {
                row_index: 0,
                column_index: 3,
            },
        };
        assert_eq!(serialize_selection_text(&view, &selection, true), "ab");
        assert_eq!(serialize_selection_text(&view, &selection, false), "ab  ");
    }

    let block = Selection {
        selection_kind: SelectionKind::Block,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 3,
        },
    };
    assert_eq!(serialize_selection_text(&view, &block, true), "ab\nc");
    assert_eq!(serialize_selection_text(&view, &block, false), "ab  \nc   ");
}

#[test]
fn trimming_keeps_spaces_inside_a_soft_wrapped_line() {
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["ab ", "cd "], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 2,
        },
    };

    assert_eq!(serialize_selection_text(&view, &selection, true), "ab cd");
    assert_eq!(serialize_selection_text(&view, &selection, false), "ab cd ");
}

#[test]
fn selection_text_reads_a_wide_glyph_once_and_drops_trailing_blanks() {
    let cells = vec![vec![
        Cell::from_character('世', 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
        Cell::from_character('x', 1, Style::default()),
        Cell::from_character(' ', 1, Style::default()),
        Cell::from_character(' ', 1, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 5, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 4,
        },
    };
    // The width-0 half is skipped, and the blanks right of `x` are the
    // screen's padding, not the text's.
    assert_eq!(serialize_selection_text(&view, &selection, true), "世x");
}

#[test]
fn selection_text_keeps_a_combining_mark_with_its_base() {
    // `café` with the accent as its own code point: the `e` cell carries a
    // combining acute (U+0301) layered over it. Copying must keep the mark
    // riding its base, so the clipboard reads `cafe` + U+0301, not a bare `cafe`.
    let mut accented_e_cell = Cell::from_character('e', 1, Style::default());
    accented_e_cell.push_combining('\u{0301}');
    let cells = vec![vec![
        Cell::from_character('c', 1, Style::default()),
        Cell::from_character('a', 1, Style::default()),
        Cell::from_character('f', 1, Style::default()),
        accented_e_cell,
    ]];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 3,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "cafe\u{0301}"
    );
}

#[test]
fn selection_text_keeps_a_multi_codepoint_emoji_whole() {
    // A ZWJ family emoji 👨‍👩‍👧 is one wide glyph: the base cell holds the first
    // person, its `combining` vec holds the ZWJ-joined rest, and a width-0
    // spacer sits in its right half. Copying must emit the whole cluster once —
    // base + every joined code point — and skip the spacer.
    let mut family = Cell::from_character('\u{1F468}', 2, Style::default());
    for cp in ['\u{200D}', '\u{1F469}', '\u{200D}', '\u{1F467}'] {
        family.push_combining(cp);
    }
    let cells = vec![vec![family, Cell::from_character(' ', 0, Style::default())]];
    let grid = Grid::from_rows(cells, 2, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"
    );
}

#[test]
fn a_block_takes_a_wide_glyph_whole_on_every_row() {
    // A column block spanning a wide glyph and its blank half takes the glyph
    // once per row, never the width-0 spacer on its own. `a世b` over `c界d`,
    // block columns 0..=3: the wide glyph is at column 1, its spacer at 2.
    let cells = vec![
        vec![
            Cell::from_character('a', 1, Style::default()),
            Cell::from_character('世', 2, Style::default()),
            Cell::from_character(' ', 0, Style::default()),
            Cell::from_character('b', 1, Style::default()),
        ],
        vec![
            Cell::from_character('c', 1, Style::default()),
            Cell::from_character('界', 2, Style::default()),
            Cell::from_character(' ', 0, Style::default()),
            Cell::from_character('d', 1, Style::default()),
        ],
    ];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Block,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 3,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "a世b\nc界d"
    );
}

#[test]
fn selection_text_keeps_a_combining_mark_on_a_wide_base() {
    // A wide CJK base carries a combining mark just as a narrow one does: the
    // base cell holds the mark and its width-0 right half is skipped. Copying
    // must keep the mark riding the wide base — `漢` + U+0301, then `x`.
    let mut base = Cell::from_character('漢', 2, Style::default());
    base.push_combining('\u{0301}');
    let cells = vec![vec![
        base,
        Cell::from_character(' ', 0, Style::default()),
        Cell::from_character('x', 1, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 3, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 2,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "漢\u{0301}x"
    );
}

#[test]
fn selection_text_spans_history_and_screen() {
    let scrollback = scrollback_of(&["old line"], 100);
    let grid = grid_of(&["new line"], 8);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 4,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 2,
        },
    };
    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "line\nnew"
    );
}

#[test]
fn selection_text_reads_only_the_rows_the_view_still_holds() {
    // Ten lines pushed under a cap of three: rows 0..=6 have been dropped, so
    // the view holds history rows 7, 8, 9 and screen row 10. A selection naming
    // every row number there could ever be copies exactly those four rows — and
    // returns at once, where a walk over the named range would run `u64::MAX`
    // times and never finish.
    let scrollback = scrollback_of(
        &["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"],
        3,
    );
    let grid = grid_of(&["live"], 4);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    assert_eq!(view.get_first_row_index(), 7);
    assert_eq!(view.get_last_row_index(), 10);

    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: u64::MAX,
            column_index: u16::MAX,
        },
    };

    assert_eq!(
        serialize_selection_text(&view, &selection, true),
        "l7\nl8\nl9\nlive"
    );
}

#[test]
fn selection_text_of_rows_entirely_outside_the_view_is_empty() {
    // Both ends are past the newest row, so the selection covers no row the
    // view holds and the copy is empty rather than a walk to `u64::MAX`.
    let scrollback = scrollback_of(&["old"], 100);
    let grid = grid_of(&["live"], 4);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    assert_eq!(view.get_last_row_index(), 1);

    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 900,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: u64::MAX,
            column_index: 0,
        },
    };

    assert_eq!(serialize_selection_text(&view, &selection, true), "");
}

#[test]
fn different_separators_do_not_join_into_one_run() {
    // `(` and `)` are both separators, but a run is one repeated character:
    // double-clicking `(` in `a() b` selects `(` alone.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["a() b"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_word_start_position(0, 1), (0, 1));
    assert_eq!(
        view.get_word_end_position(0, 1),
        (0, 1),
        "`)` next door is a different run"
    );
    assert_eq!(view.get_word_start_position(0, 2), (0, 2));
    assert_eq!(
        view.get_word_end_position(0, 2),
        (0, 2),
        "the space after `)` is too"
    );
}

/// A scrollback holding `lines`, each padded out to `column_count` the way a row
/// arrives from the screen, so the stored rows are the trimmed ones.
fn scrollback_padded(lines: &[&str], column_count: u16, maximum_line_count: usize) -> Scrollback {
    let mut scrollback = Scrollback::from_scrollback_limit(
        ScrollbackLimit::from_line_and_byte_limits(maximum_line_count, usize::MAX),
    );
    for line in lines {
        let mut row_cells = cells_of(line);
        row_cells.resize(column_count as usize, Cell::blank());
        scrollback.push_row(&row_cells, RowMetadata::default());
    }
    scrollback
}

#[test]
fn a_column_right_of_a_history_lines_text_reads_as_the_blank_it_was() {
    // History stores `hi`, not `hi` plus eight blanks. Every column of the
    // screen width still answers: a double-click in the empty area right of a
    // line finds a space there.
    let scrollback = scrollback_padded(&["hi"], 10, 100);
    let grid = grid_of(&["live"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    let padded = view
        .get_cell(0, 5)
        .expect("a column inside the screen answers");
    assert_eq!(padded.get_character(), ' ');
    assert_eq!(padded.get_display_width(), 1);
    assert_eq!(padded.get_style(), Style::default());
}

#[test]
fn a_column_past_the_screen_width_still_reads_as_nothing() {
    let scrollback = scrollback_padded(&["hi"], 10, 100);
    let grid = grid_of(&["live"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_cell(0, 10), None);
    assert_eq!(view.get_cell(0, 99), None);
}

#[test]
fn a_dropped_row_still_reads_as_nothing_at_every_column() {
    // The fallback answers for a column past a row's text, never for a row
    // that is gone.
    let scrollback = scrollback_padded(&["a", "b"], 10, 1);
    let grid = grid_of(&["live"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_cell(0, 0), None, "evicted by the one-line cap");
    assert_eq!(view.get_cell(0, 5), None);
}

#[test]
fn copying_a_history_line_keeps_its_trailing_blanks_when_asked_to() {
    // With trimming off, a copy preserves every selected blank. Those blanks
    // are no longer stored, so this is the guarantee the read fallback exists
    // for.
    let scrollback = scrollback_padded(&["hi"], 10, 100);
    let grid = grid_of(&["live"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 9,
        },
        selection_kind: SelectionKind::Character,
    };

    assert_eq!(
        serialize_selection_text(&view, &selection, false),
        "hi        "
    );
    assert_eq!(serialize_selection_text(&view, &selection, true), "hi");
}

#[test]
fn a_word_selection_in_the_space_right_of_a_history_line_behaves_as_before() {
    // Double-clicking the empty area right of `hi` lands on a blank, which is
    // a word separator, so it selects that run of blanks and not the text.
    let scrollback = scrollback_padded(&["hi"], 10, 100);
    let grid = grid_of(&["live"], 10);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    let (start_row_index, start_column_index) = view.get_word_start_position(0, 5);
    let (end_row_index, end_column_index) = view.get_word_end_position(0, 5);
    let word = Selection {
        anchor: GridPosition {
            row_index: start_row_index,
            column_index: start_column_index,
        },
        cursor: GridPosition {
            row_index: end_row_index,
            column_index: end_column_index,
        },
        selection_kind: SelectionKind::Word,
    };
    assert_eq!(serialize_selection_text(&view, &word, false), "        ");
}

#[test]
fn a_word_selection_on_a_history_line_still_finds_the_text() {
    let scrollback = scrollback_padded(&["/usr/local/bin here"], 40, 100);
    let grid = grid_of(&["live"], 40);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    let (start_row_index, start_column_index) = view.get_word_start_position(0, 6);
    let (end_row_index, end_column_index) = view.get_word_end_position(0, 6);
    let word = Selection {
        anchor: GridPosition {
            row_index: start_row_index,
            column_index: start_column_index,
        },
        cursor: GridPosition {
            row_index: end_row_index,
            column_index: end_column_index,
        },
        selection_kind: SelectionKind::Word,
    };
    assert_eq!(
        serialize_selection_text(&view, &word, false),
        "/usr/local/bin"
    );
}

#[test]
fn ordering_two_equal_ends_keeps_the_anchor_first() {
    let position = GridPosition {
        row_index: 2,
        column_index: 7,
    };

    let ordered = order_selection_positions(position, position);
    assert_eq!(
        ordered,
        OrderedSelection {
            start_position: position,
            end_position: position
        }
    );
}

#[test]
fn a_history_row_reports_how_it_ended() {
    let mut scrollback = Scrollback::from_scrollback_limit(
        ScrollbackLimit::from_line_and_byte_limits(100, usize::MAX),
    );
    scrollback.push_row(
        &cells_of("abc"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );
    scrollback.push_row(&cells_of("de"), RowMetadata::default());
    let grid = grid_of(&["live"], 4);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        view.get_row(0),
        Some((cells_of("abc").as_slice(), RowEnd::Soft))
    );
    assert_eq!(
        view.get_row(1),
        Some((cells_of("de").as_slice(), RowEnd::Hard))
    );
    assert!(view.is_soft_wrapped(0));
    assert!(!view.is_soft_wrapped(1));
}

#[test]
fn a_gone_row_neither_wraps_nor_holds_a_wrap_spacer() {
    let scrollback = scrollback_of(&["a", "b"], 1);
    let grid = grid_of(&["live"], 4);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_first_row_index(), 1, "row 0 was evicted");
    assert!(!view.is_soft_wrapped(0), "a dropped history row");
    assert!(!view.is_wide_wrap_spacer(0, 3));
    assert!(
        !view.is_soft_wrapped(9),
        "a row past the bottom of the screen"
    );
    assert!(!view.is_wide_wrap_spacer(9, 3));
}

#[test]
fn a_word_or_line_lookup_on_a_gone_row_stays_where_it_is() {
    let scrollback = scrollback_of(&["abc def", "ghi"], 1);
    let grid = grid_of(&["live"], 7);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    assert_eq!(view.get_first_row_index(), 1, "row 0 was evicted");

    assert_eq!(view.get_word_start_position(0, 5), (0, 5));
    assert_eq!(view.get_word_end_position(0, 5), (0, 5));
    assert_eq!(view.get_line_start_row_index(0), 0);
    assert_eq!(view.get_line_end_row_index(0), 0);
}

#[test]
fn a_line_lookup_past_the_newest_row_returns_the_row_unchanged() {
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["abc", "def"], 3);
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    assert_eq!(view.get_last_row_index(), 1);

    assert_eq!(view.get_line_start_row_index(5), 5);
    assert_eq!(view.get_line_end_row_index(5), 5);
}

#[test]
fn a_word_at_the_edges_of_a_hard_row_stops_at_the_edges() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abc"], 3);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(
        view.get_word_start_position(0, 0),
        (0, 0),
        "nothing left of column 0"
    );
    assert_eq!(
        view.get_word_end_position(0, 2),
        (0, 2),
        "nothing right of the last column"
    );
    assert_eq!(view.get_word_start_position(0, 2), (0, 0));
    assert_eq!(view.get_word_end_position(0, 0), (0, 2));
}

#[test]
fn a_word_lookup_past_the_screen_width_of_a_hard_row_stays_put() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abc"], 3);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_word_start_position(0, 50), (0, 50));
    assert_eq!(view.get_word_end_position(0, 50), (0, 50));
}

#[test]
fn selection_text_reads_the_same_text_however_the_drag_ran() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abcde", "fghij"], 5);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    let down = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 3,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 1,
        },
    };
    let up = Selection {
        anchor: down.cursor,
        cursor: down.anchor,
        ..down
    };
    assert_eq!(serialize_selection_text(&view, &down, true), "de\nfg");
    assert_eq!(serialize_selection_text(&view, &up, true), "de\nfg");

    // A block drag up and to the left: the column range is still 1..=3.
    let block_up = Selection {
        selection_kind: SelectionKind::Block,
        anchor: GridPosition {
            row_index: 1,
            column_index: 3,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
    };
    assert_eq!(serialize_selection_text(&view, &block_up, true), "bcd\nghi");
}

#[test]
fn selection_text_of_one_cell_is_that_cells_character() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abc"], 3);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
    };

    assert_eq!(serialize_selection_text(&view, &selection, true), "b");
}

#[test]
fn selection_text_on_a_screen_with_no_rows_is_empty() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&[], 0);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    assert_eq!(view.get_column_count(), 0);
    assert_eq!(view.get_first_row_index(), 0);
    assert_eq!(view.get_last_row_index(), 0);
    assert_eq!(view.get_row(0), None);

    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 0,
            column_index: 0,
        },
    };
    assert_eq!(serialize_selection_text(&view, &selection, true), "");
    assert_eq!(serialize_selection_text(&view, &selection, false), "");
}

#[test]
fn a_start_column_past_the_screen_width_reads_nothing_from_its_row() {
    // Row 0 contributes no text, but it is still a selected row: the hard row
    // end between it and row 1 still becomes a newline.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abc", "def"], 3);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 0,
            column_index: 50,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 2,
        },
    };

    assert_eq!(serialize_selection_text(&view, &selection, true), "\ndef");
    assert_eq!(serialize_selection_text(&view, &selection, false), "\ndef");
}

#[test]
fn a_block_reaching_past_the_screen_width_stops_at_the_edge() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abcde", "fghij"], 5);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);
    let selection = Selection {
        selection_kind: SelectionKind::Block,
        anchor: GridPosition {
            row_index: 0,
            column_index: 1,
        },
        cursor: GridPosition {
            row_index: 1,
            column_index: 50,
        },
    };

    assert_eq!(
        serialize_selection_text(&view, &selection, false),
        "bcde\nghij"
    );
}

#[test]
fn a_wide_wrap_spacer_is_skipped_with_trimming_on_and_marks_only_its_own_cell() {
    let mut engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 3,
        row_count: 2,
    });
    let _ = engine.process_pty_output("abcde世".as_bytes());
    let view = engine.get_terminal_state().get_text_view();

    assert!(
        view.is_wide_wrap_spacer(1, 2),
        "the last column of the row `世` did not fit"
    );
    assert!(!view.is_wide_wrap_spacer(1, 1), "a column inside that row");
    assert!(
        !view.is_wide_wrap_spacer(0, 2),
        "a plain soft wrap has no spacer"
    );
    let selection = Selection {
        selection_kind: SelectionKind::Character,
        anchor: GridPosition {
            row_index: 1,
            column_index: 0,
        },
        cursor: GridPosition {
            row_index: 2,
            column_index: 2,
        },
    };
    assert_eq!(serialize_selection_text(&view, &selection, true), "de世");
}

#[test]
fn a_word_lookup_at_the_last_representable_column_stays_put() {
    // Stepping right of u16::MAX has no cell to step to, so the walk stops
    // where it started instead of wrapping the column back to 0.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["cargo build"], 11);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_word_end_position(0, u16::MAX), (0, u16::MAX));
    assert_eq!(view.get_word_start_position(0, u16::MAX), (0, u16::MAX));
}

#[test]
fn a_word_end_lookup_past_the_width_of_a_wrapped_row_stays_put() {
    // Column 50 of a six-column row holds nothing, which reads as a blank —
    // a separator — so the run is blanks and it ends at the `d` below.
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["abc", "def gh"], 6);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::from_scrollback_and_grid(&scrollback, &grid);

    assert_eq!(view.get_word_end_position(0, 50), (0, 50));
}
