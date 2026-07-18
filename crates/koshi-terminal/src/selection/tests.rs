//! Tests for reading a pane's text by absolute row and growing a selection to
//! whole words or lines.

use super::*;

use koshi_core::{command::GridPos, process::PtySize};

use crate::engine::TerminalEngine;
use crate::scrollback::ScrollbackLimit;
use crate::style::Style;

/// A grid holding `rows`, each padded to `cols` with blanks.
fn grid_of(rows: &[&str], cols: u16) -> Grid {
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|line| {
            line.chars()
                .map(|ch| Cell::new(ch, 1, Style::default()))
                .collect()
        })
        .collect();
    Grid::from_rows(cells, cols, Style::default())
}

/// One text line as grid cells.
fn cells_of(line: &str) -> Vec<Cell> {
    line.chars()
        .map(|ch| Cell::new(ch, 1, Style::default()))
        .collect()
}

/// A scrollback holding `lines`, each ended hard, under a `max_lines` cap.
fn scrollback_of(lines: &[&str], max_lines: usize) -> Scrollback {
    let mut scrollback = Scrollback::new(ScrollbackLimit::new(max_lines, usize::MAX));
    for line in lines {
        scrollback.push_line(cells_of(line), RowEnd::Hard);
    }
    scrollback
}

/// Read a row back as a trimmed string, for asserting which line a number names.
fn row_text(view: &TextView<'_>, row: u64) -> String {
    let (cells, _) = view.row(row).expect("row is readable");
    cells
        .iter()
        .map(Cell::ch)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn the_live_screens_top_row_is_the_lines_pushed_so_far() {
    let scrollback = scrollback_of(&["old0", "old1", "old2"], 100);
    let grid = grid_of(&["live0", "live1"], 10);
    let view = TextView::new(&scrollback, &grid);

    // Three lines pushed, so the screen's top row is line 3 and history is 0..=2.
    assert_eq!(view.first_row(), 0);
    assert_eq!(view.last_row(), 4);
    assert_eq!(row_text(&view, 0), "old0");
    assert_eq!(row_text(&view, 2), "old2");
    assert_eq!(row_text(&view, 3), "live0");
    assert_eq!(row_text(&view, 4), "live1");
}

#[test]
fn a_row_number_still_names_the_same_line_after_more_output() {
    let mut scrollback = scrollback_of(&["old0", "old1", "old2"], 100);
    {
        let grid = grid_of(&["live0", "live1"], 10);
        let view = TextView::new(&scrollback, &grid);
        assert_eq!(row_text(&view, 2), "old2");
        assert_eq!(row_text(&view, 3), "live0");
    }

    // `live0` and `live1` scroll off into history; two fresh lines take their place.
    scrollback.push_line(cells_of("live0"), RowEnd::Hard);
    scrollback.push_line(cells_of("live1"), RowEnd::Hard);
    let grid = grid_of(&["new0", "new1"], 10);
    let view = TextView::new(&scrollback, &grid);

    // Every line kept its number: row 3 is still `live0`, now in history.
    assert_eq!(row_text(&view, 2), "old2");
    assert_eq!(row_text(&view, 3), "live0");
    assert_eq!(row_text(&view, 4), "live1");
    assert_eq!(row_text(&view, 5), "new0");
}

#[test]
fn a_row_number_still_names_the_same_line_after_the_cap_drops_history() {
    // A cap of 2 keeps only the two newest history lines: pushing four drops the
    // two oldest, which is the moment a from-the-top numbering would renumber.
    let scrollback = scrollback_of(&["old0", "old1", "old2", "old3"], 2);
    let grid = grid_of(&["live0"], 10);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(scrollback.dropped_lines(), 2, "the cap dropped two lines");
    assert_eq!(
        view.first_row(),
        2,
        "rows 0 and 1 are gone; 2 is the oldest"
    );
    assert_eq!(row_text(&view, 2), "old2", "row 2 still names old2");
    assert_eq!(row_text(&view, 3), "old3");
    assert_eq!(row_text(&view, 4), "live0");
    assert!(view.row(1).is_none(), "a dropped row reads as gone");
    assert!(view.row(0).is_none());
}

#[test]
fn erasing_saved_lines_leaves_surviving_rows_their_numbers() {
    // `clear` (ED 3, erase saved lines) empties history without counting as a
    // cap-driven drop, so `dropped_lines` does not move. The numbering must not
    // rely on that counter.
    let mut scrollback = scrollback_of(&["old0", "old1", "old2"], 100);
    scrollback.clear();
    scrollback.push_line(cells_of("after"), RowEnd::Hard);
    let grid = grid_of(&["live0"], 10);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(
        scrollback.dropped_lines(),
        0,
        "an erase is not a truncation"
    );
    assert_eq!(scrollback.total_pushed(), 4, "four lines were ever pushed");
    assert_eq!(
        view.first_row(),
        3,
        "only the line pushed after the erase is left"
    );
    assert_eq!(row_text(&view, 3), "after");
    assert_eq!(row_text(&view, 4), "live0");
    assert!(view.row(0).is_none(), "the erased lines are gone");
}

#[test]
fn a_row_past_the_bottom_of_the_screen_is_gone() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["a", "b"], 10);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(view.last_row(), 1);
    assert!(view.row(1).is_some());
    assert!(view.row(2).is_none());
}

#[test]
fn a_screen_with_no_history_starts_at_row_zero() {
    // What the alternate screen looks like: it keeps no scrollback, so the view
    // is the screen alone.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["alt0", "alt1"], 10);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(view.first_row(), 0);
    assert_eq!(view.last_row(), 1);
}

#[test]
fn a_word_grows_to_its_separators() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["cargo build"], 11);
    let view = TextView::new(&scrollback, &grid);

    // The pointer on the `i` of `build` (column 8).
    assert_eq!(view.word_start(0, 8), (0, 6), "the word starts at the `b`");
    assert_eq!(view.word_end(0, 8), (0, 10), "and ends at the `d`");
    // And on the `r` of `cargo` (column 2).
    assert_eq!(view.word_start(0, 2), (0, 0));
    assert_eq!(
        view.word_end(0, 2),
        (0, 4),
        "the space after `cargo` stops it"
    );
}

#[test]
fn a_path_is_one_word_because_slashes_are_not_separators() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["cd /usr/local/bin"], 17);
    let view = TextView::new(&scrollback, &grid);

    // The pointer inside `local`: the whole path comes out, not one segment.
    assert_eq!(view.word_start(0, 12), (0, 3), "back to the leading slash");
    assert_eq!(view.word_end(0, 12), (0, 16), "on to the end of `bin`");
}

#[test]
fn a_dotted_filename_is_one_word() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["tar xf foo.tar.gz"], 17);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(view.word_start(0, 12), (0, 7), "dots do not split the name");
    assert_eq!(view.word_end(0, 12), (0, 16));
}

#[test]
fn brackets_and_quotes_stop_a_word() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["(foo bar)"], 9);
    let view = TextView::new(&scrollback, &grid);

    // The pointer on `foo`: the paren and the space bound it.
    assert_eq!(view.word_start(0, 2), (0, 1));
    assert_eq!(view.word_end(0, 2), (0, 3));
}

#[test]
fn a_word_follows_the_text_across_a_soft_wrap() {
    let scrollback = scrollback_of(&[], 100);
    // `abcde` wrapped after `abc`: one logical word split over two rows.
    let mut grid = grid_of(&["abc", "de "], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::new(&scrollback, &grid);

    // From the `d` on row 1, the word runs back onto row 0.
    assert_eq!(
        view.word_start(1, 0),
        (0, 0),
        "back across the wrap to the `a`"
    );
    assert_eq!(
        view.word_end(0, 0),
        (1, 1),
        "and forward across it to the `e`"
    );
}

#[test]
fn a_word_stops_at_a_hard_line_end() {
    let scrollback = scrollback_of(&[], 100);
    // Two separate lines: row 0 ends hard, so `abc` and `def` are not one word.
    let grid = grid_of(&["abc", "def"], 3);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(
        view.word_start(1, 0),
        (1, 0),
        "a new line starts a new word"
    );
    assert_eq!(view.word_end(0, 0), (0, 2), "the line end stops it");
}

#[test]
fn a_word_crosses_a_soft_wrap_out_of_history_onto_the_screen() {
    // The wrap runs across the history/screen boundary: the last history line
    // wrapped into the screen's top row, so the word spans both.
    let mut scrollback = Scrollback::new(ScrollbackLimit::new(100, usize::MAX));
    scrollback.push_line(cells_of("abc"), RowEnd::Soft);
    let grid = grid_of(&["def"], 3);
    let view = TextView::new(&scrollback, &grid);

    // Row 0 is history, row 1 the live screen: one word `abcdef` over both.
    assert_eq!(view.word_start(1, 0), (0, 0));
    assert_eq!(view.word_end(0, 0), (1, 2));
}

#[test]
fn a_live_autowrap_keeps_one_word_across_history_and_screen() {
    let mut engine = TerminalEngine::new(PtySize { cols: 3, rows: 2 });
    let _ = engine.advance(b"abcdefg");
    let view = engine.state().text_view();

    assert_eq!(row_text(&view, 0), "abc");
    assert_eq!(row_text(&view, 1), "def");
    assert_eq!(row_text(&view, 2), "g");
    assert_eq!(view.word_start(2, 0), (0, 0));
    assert_eq!(view.word_end(0, 0), (2, 0));
}

#[test]
fn a_wide_wrap_spacer_is_neither_a_word_break_nor_copied_text() {
    let mut engine = TerminalEngine::new(PtySize { cols: 3, rows: 2 });
    let _ = engine.advance("abcde世".as_bytes());
    let view = engine.state().text_view();

    assert!(view.is_wide_wrap_spacer(1, 2));
    assert_eq!(view.word_start(2, 0), (0, 0));
    assert_eq!(view.word_end(0, 0), (2, 0));
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 2, col: 1 },
    };
    assert_eq!(selection_text(&view, &selection, false), "abcde世");
}

#[test]
fn a_logical_line_spans_every_row_it_wrapped_over() {
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["one", "two", "thr", "end"], 3);
    // Rows 0..=2 are one logical line; row 3 is its own.
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::Soft);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(view.line_start(1), 0, "up to the first row of the wrap");
    assert_eq!(view.line_end(1), 2, "down to the last");
    assert_eq!(view.line_start(3), 3, "the next line stands alone");
    assert_eq!(view.line_end(3), 3);
}

#[test]
fn a_logical_line_reaching_the_oldest_row_stops_there() {
    // The row wraps onward, but the walk up has nothing older to read: it stops
    // at the oldest readable row rather than running off.
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["abc", "def"], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(view.line_start(0), 0, "already at the oldest readable row");
    assert_eq!(view.line_end(1), 1, "and the newest row ends the walk down");
}

#[test]
fn a_wide_glyphs_blank_half_is_skipped_when_growing_a_word() {
    let scrollback = scrollback_of(&[], 100);
    // `世界` — each glyph is two cells wide, its right half a width-0 blank.
    let cells = vec![vec![
        Cell::new('世', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
        Cell::new('界', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let view = TextView::new(&scrollback, &grid);

    // Growing from the first glyph lands on the second glyph's own cell (2),
    // never on a blank right half (1 or 3), which would split a glyph.
    assert_eq!(view.word_end(0, 0), (0, 2));
    assert_eq!(view.word_start(0, 2), (0, 0));
}

#[test]
fn ordering_puts_the_earlier_end_first() {
    let earlier = GridPos { row: 3, col: 10 };
    let later = GridPos { row: 5, col: 2 };

    // Dragging down: already in order.
    let ordered = order(earlier, later);
    assert_eq!(ordered.start, earlier);
    assert_eq!(ordered.end, later);

    // Dragging up: the anchor is the later end, so the pair is swapped.
    let ordered = order(later, earlier);
    assert_eq!(ordered.start, earlier);
    assert_eq!(ordered.end, later);
}

#[test]
fn ordering_within_one_row_compares_columns() {
    let left = GridPos { row: 4, col: 1 };
    let right = GridPos { row: 4, col: 9 };

    let ordered = order(right, left);
    assert_eq!(ordered.start, left);
    assert_eq!(ordered.end, right);
}

#[test]
fn a_screen_with_no_history_cannot_read_the_rows_below_it() {
    // The alternate screen keeps no history of its own, yet the pane's
    // scrollback — the PRIMARY's — is still there. A view built for it must not
    // reach those rows: they are another screen's text.
    let scrollback = scrollback_of(&["primary0", "primary1"], 100);
    let grid = grid_of(&["alt0", "alt1"], 10);
    let view = TextView::screen_only(&grid, scrollback.total_pushed());

    // Rows number from the same base, so a position means the same thing here
    // as on the primary...
    assert_eq!(
        view.first_row(),
        2,
        "the screen's first row is still line 2"
    );
    assert_eq!(view.last_row(), 3);
    // ...but nothing below the screen is readable.
    assert!(view.row(1).is_none(), "the primary's newest history row");
    assert!(view.row(0).is_none(), "and the one before it");
    assert_eq!(row_text(&view, 2), "alt0");
}

#[test]
fn a_word_on_a_screen_with_no_history_stops_at_its_top_row() {
    // The case the pairing bug produced: the row below the screen's top ends
    // SOFT, so a walk that could see history would step into it. With no
    // history there is nothing to step into.
    let mut scrollback = Scrollback::new(ScrollbackLimit::new(100, usize::MAX));
    scrollback.push_line(cells_of("abc"), RowEnd::Soft);
    let grid = grid_of(&["def"], 3);

    // Built for the primary, the word crosses the boundary — correct there.
    let primary = TextView::new(&scrollback, &grid);
    assert_eq!(
        primary.word_start(1, 0),
        (0, 0),
        "the primary joins the wrap"
    );

    // Built for a screen with no history, the same walk stops dead.
    let alternate = TextView::screen_only(&grid, scrollback.total_pushed());
    assert_eq!(
        alternate.word_start(1, 0),
        (1, 0),
        "nothing above the screen's top row to grow into"
    );
    assert_eq!(alternate.line_start(1), 1);
}

#[test]
fn a_word_grows_the_same_from_either_half_of_a_wide_glyph() {
    // A wide glyph's right half is a width-0 blank, and a blank is a word
    // separator — but landing on one still grows the whole word, because the
    // separator test is applied to the NEIGHBOUR the walk steps to, and the walk
    // skips width-0 cells. So a double click anywhere on `世界` selects both.
    let cells = vec![vec![
        Cell::new('世', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
        Cell::new('界', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::new(&scrollback, &grid);

    // From the first glyph's own cell.
    assert_eq!(view.word_start(0, 0), (0, 0));
    assert_eq!(view.word_end(0, 0), (0, 2), "reaches the second glyph");

    // From its blank right half — the same answer, not a one-cell selection on
    // the blank.
    assert_eq!(
        view.word_start(0, 1),
        (0, 0),
        "back to the glyph it belongs to"
    );
    assert_eq!(view.word_end(0, 1), (0, 2));

    // And from the second glyph.
    assert_eq!(view.word_start(0, 2), (0, 0));
    assert_eq!(view.word_end(0, 2), (0, 2));
}

#[test]
fn a_word_lookup_on_a_separator_covers_the_run_of_that_same_separator() {
    // Double-clicking the gap in `foo  bar` must select the two spaces, never
    // `foo  bar` entire: a lookup that starts ON a separator grows over the
    // run of that character, not into the words around it.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["foo  bar"], 10);
    let view = TextView::new(&scrollback, &grid);

    // The spaces sit at columns 3 and 4; from either one the answer is the run.
    assert_eq!(view.word_start(0, 3), (0, 3));
    assert_eq!(view.word_end(0, 3), (0, 4));
    assert_eq!(view.word_start(0, 4), (0, 3));
    assert_eq!(view.word_end(0, 4), (0, 4));
}

#[test]
fn selection_text_reads_the_range_inclusive() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["hello world"], 11);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 6 },
        cursor: GridPos { row: 0, col: 10 },
    };
    assert_eq!(selection_text(&view, &selection, true), "world");
}

#[test]
fn selection_text_joins_hard_rows_with_newlines_and_soft_wraps_with_nothing() {
    // `abc` wraps into `def` (one logical line), then `ghi` starts fresh.
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["abc", "def", "ghi"], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 2, col: 2 },
    };
    assert_eq!(selection_text(&view, &selection, true), "abcdef\nghi");
}

#[test]
fn selection_text_takes_the_same_columns_from_every_block_row() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["abcde", "fghij", "klmno"], 5);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Block,
        anchor: GridPos { row: 0, col: 1 },
        cursor: GridPos { row: 2, col: 3 },
    };
    assert_eq!(selection_text(&view, &selection, true), "bcd\nghi\nlmn");
}

#[test]
fn selection_text_applies_the_trim_setting_to_every_selection_kind() {
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["ab  ", "c   "], 4);
    let view = TextView::new(&scrollback, &grid);

    for kind in [
        SelectionKind::Character,
        SelectionKind::Word,
        SelectionKind::Line,
    ] {
        let selection = Selection {
            kind,
            anchor: GridPos { row: 0, col: 0 },
            cursor: GridPos { row: 0, col: 3 },
        };
        assert_eq!(selection_text(&view, &selection, true), "ab");
        assert_eq!(selection_text(&view, &selection, false), "ab  ");
    }

    let block = Selection {
        kind: SelectionKind::Block,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 1, col: 3 },
    };
    assert_eq!(selection_text(&view, &block, true), "ab\nc");
    assert_eq!(selection_text(&view, &block, false), "ab  \nc   ");
}

#[test]
fn trimming_keeps_spaces_inside_a_soft_wrapped_line() {
    let scrollback = scrollback_of(&[], 100);
    let mut grid = grid_of(&["ab ", "cd "], 3);
    grid.set_row_end(0, RowEnd::Soft);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 1, col: 2 },
    };

    assert_eq!(selection_text(&view, &selection, true), "ab cd");
    assert_eq!(selection_text(&view, &selection, false), "ab cd ");
}

#[test]
fn selection_text_reads_a_wide_glyph_once_and_drops_trailing_blanks() {
    let cells = vec![vec![
        Cell::new('世', 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
        Cell::new('x', 1, Style::default()),
        Cell::new(' ', 1, Style::default()),
        Cell::new(' ', 1, Style::default()),
    ]];
    let grid = Grid::from_rows(cells, 5, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 0, col: 4 },
    };
    // The width-0 half is skipped, and the blanks right of `x` are the
    // screen's padding, not the text's.
    assert_eq!(selection_text(&view, &selection, true), "世x");
}

#[test]
fn selection_text_keeps_a_combining_mark_with_its_base() {
    // `café` with the accent as its own code point: the `e` cell carries a
    // combining acute (U+0301) layered over it. Copying must keep the mark
    // riding its base, so the clipboard reads `cafe` + U+0301, not a bare `cafe`.
    let mut e = Cell::new('e', 1, Style::default());
    e.push_combining('\u{0301}');
    let cells = vec![vec![
        Cell::new('c', 1, Style::default()),
        Cell::new('a', 1, Style::default()),
        Cell::new('f', 1, Style::default()),
        e,
    ]];
    let grid = Grid::from_rows(cells, 4, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 0, col: 3 },
    };
    assert_eq!(selection_text(&view, &selection, true), "cafe\u{0301}");
}

#[test]
fn selection_text_keeps_a_multi_codepoint_emoji_whole() {
    // A ZWJ family emoji 👨‍👩‍👧 is one wide glyph: the base cell holds the first
    // person, its `combining` vec holds the ZWJ-joined rest, and a width-0
    // spacer sits in its right half. Copying must emit the whole cluster once —
    // base + every joined code point — and skip the spacer.
    let mut family = Cell::new('\u{1F468}', 2, Style::default());
    for cp in ['\u{200D}', '\u{1F469}', '\u{200D}', '\u{1F467}'] {
        family.push_combining(cp);
    }
    let cells = vec![vec![family, Cell::new(' ', 0, Style::default())]];
    let grid = Grid::from_rows(cells, 2, Style::default());
    let scrollback = scrollback_of(&[], 100);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 0 },
        cursor: GridPos { row: 0, col: 1 },
    };
    assert_eq!(
        selection_text(&view, &selection, true),
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"
    );
}

#[test]
fn selection_text_spans_history_and_screen() {
    let scrollback = scrollback_of(&["old line"], 100);
    let grid = grid_of(&["new line"], 8);
    let view = TextView::new(&scrollback, &grid);
    let selection = Selection {
        kind: SelectionKind::Character,
        anchor: GridPos { row: 0, col: 4 },
        cursor: GridPos { row: 1, col: 2 },
    };
    assert_eq!(selection_text(&view, &selection, true), "line\nnew");
}

#[test]
fn different_separators_do_not_join_into_one_run() {
    // `(` and `)` are both separators, but a run is one repeated character:
    // double-clicking `(` in `a() b` selects `(` alone.
    let scrollback = scrollback_of(&[], 100);
    let grid = grid_of(&["a() b"], 10);
    let view = TextView::new(&scrollback, &grid);

    assert_eq!(view.word_start(0, 1), (0, 1));
    assert_eq!(
        view.word_end(0, 1),
        (0, 1),
        "`)` next door is a different run"
    );
    assert_eq!(view.word_start(0, 2), (0, 2));
    assert_eq!(view.word_end(0, 2), (0, 2), "the space after `)` is too");
}
