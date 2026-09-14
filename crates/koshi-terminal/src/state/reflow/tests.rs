//! Tests for resize reflow: soft-wrap re-joining, re-wrapping to narrower
//! and wider screens, hard-break preservation, wide-glyph spacers, cursor
//! tracking, scrollback round-trips, caps, and the alternate-screen crop.

use super::*;

use crate::engine::TerminalEngine;
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::style::Color;

fn build_terminal_engine(column_count: u16, row_count: u16) -> TerminalEngine {
    TerminalEngine::from_pty_size(PtySize {
        column_count,
        row_count,
    })
}

fn feed_terminal_text(terminal_engine: &mut TerminalEngine, terminal_text: &str) {
    let _ = terminal_engine.process_pty_output(terminal_text.as_bytes());
}

fn resize_terminal_engine(terminal_engine: &mut TerminalEngine, column_count: u16, row_count: u16) {
    terminal_engine.resize_terminal_state(PtySize {
        column_count,
        row_count,
    });
}

/// The visible text of `row_index`: base characters of non-continuation cells,
/// trailing spaces trimmed.
fn get_row_text(terminal_engine: &TerminalEngine, row_index: u16) -> String {
    let grid = terminal_engine.get_terminal_state().get_active_grid();
    let (_, column_count) = grid.get_grid_dimensions();
    let row_text: String = (0..column_count)
        .filter_map(|column_index| grid.get_cell(row_index, column_index))
        .filter(|cell| cell.get_display_width() != 0)
        .map(Cell::get_character)
        .collect();
    row_text.trim_end().to_string()
}

/// The visible text of every retained history row, oldest first.
fn get_scrollback_text_rows(terminal_engine: &TerminalEngine) -> Vec<String> {
    terminal_engine
        .get_terminal_state()
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .map(|(cells, _)| {
            cells
                .iter()
                .filter(|cell| cell.get_display_width() != 0)
                .map(Cell::get_character)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn get_cursor_position(terminal_engine: &TerminalEngine) -> (u16, u16) {
    terminal_engine
        .get_terminal_state()
        .get_active_cursor_position()
}

fn get_row_end(terminal_engine: &TerminalEngine, row_index: u16) -> RowEnd {
    terminal_engine
        .get_terminal_state()
        .get_active_grid()
        .get_row_end(row_index)
}

#[test]
fn print_wrap_records_a_soft_row_end() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "abcdefghij");
    assert_eq!(get_row_text(&terminal_engine, 0), "abcdefgh");
    assert_eq!(get_row_text(&terminal_engine, 1), "ij");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Soft);
    assert_eq!(get_row_end(&terminal_engine, 1), RowEnd::Hard);
}

#[test]
fn linefeed_after_a_full_row_keeps_the_hard_end() {
    // The row exactly fills the width but the app sent a real line break:
    // the rows are two logical lines and a reflow must never join them.
    let mut terminal_engine = build_terminal_engine(4, 4);
    feed_terminal_text(&mut terminal_engine, "abcd\r\nef");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Hard);

    resize_terminal_engine(&mut terminal_engine, 8, 4);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcd");
    assert_eq!(get_row_text(&terminal_engine, 1), "ef");
}

#[test]
fn shrink_wraps_a_long_line_instead_of_cropping() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "abcdefghij");

    resize_terminal_engine(&mut terminal_engine, 4, 4);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcd");
    assert_eq!(get_row_text(&terminal_engine, 1), "efgh");
    assert_eq!(get_row_text(&terminal_engine, 2), "ij");
    assert_eq!(get_row_text(&terminal_engine, 3), "");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Soft);
    assert_eq!(get_row_end(&terminal_engine, 1), RowEnd::Soft);
    assert_eq!(get_row_end(&terminal_engine, 2), RowEnd::Hard);
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        0
    );
}

#[test]
fn widen_rejoins_soft_wrapped_rows() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "abcdefghij");

    resize_terminal_engine(&mut terminal_engine, 4, 4);
    resize_terminal_engine(&mut terminal_engine, 8, 4);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcdefgh");
    assert_eq!(get_row_text(&terminal_engine, 1), "ij");

    resize_terminal_engine(&mut terminal_engine, 12, 4);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcdefghij");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Hard);
}

#[test]
fn cursor_follows_its_content_offset_across_reflows() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "abcdefghij");
    assert_eq!(get_cursor_position(&terminal_engine), (1, 2));

    resize_terminal_engine(&mut terminal_engine, 4, 4);
    assert_eq!(get_cursor_position(&terminal_engine), (2, 2));

    resize_terminal_engine(&mut terminal_engine, 8, 4);
    assert_eq!(get_cursor_position(&terminal_engine), (1, 2));

    resize_terminal_engine(&mut terminal_engine, 12, 4);
    assert_eq!(get_cursor_position(&terminal_engine), (0, 10));
}

#[test]
fn shrink_overflow_enters_history_and_widen_pulls_it_back() {
    let mut terminal_engine = build_terminal_engine(6, 3);
    feed_terminal_text(&mut terminal_engine, "aaaaaa\r\nbbb\r\ncc");

    // At width 3 the first line needs two rows: four rows of content on a
    // three-row screen, so the oldest row scrolls into history.
    resize_terminal_engine(&mut terminal_engine, 3, 3);
    assert_eq!(get_scrollback_text_rows(&terminal_engine), vec!["aaa"]);
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .list_retained_lines()[0]
            .1
            .row_end,
        RowEnd::Soft,
        "the history row must remember it soft-wraps into the screen"
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "aaa");
    assert_eq!(get_row_text(&terminal_engine, 1), "bbb");
    assert_eq!(get_row_text(&terminal_engine, 2), "cc");

    // Widening re-joins the split line across the history boundary and
    // empties history again.
    resize_terminal_engine(&mut terminal_engine, 6, 3);
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        0
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "aaaaaa");
    assert_eq!(get_row_text(&terminal_engine, 1), "bbb");
    assert_eq!(get_row_text(&terminal_engine, 2), "cc");
}

#[test]
fn scrollback_rows_rewrap_with_the_screen() {
    // Two full-width lines scroll into history, then the width halves: every
    // history row re-wraps and no text is lost.
    let mut terminal_engine = build_terminal_engine(6, 2);
    feed_terminal_text(&mut terminal_engine, "abcdef\r\nghijkl\r\nm\r\nn");
    assert_eq!(
        get_scrollback_text_rows(&terminal_engine),
        vec!["abcdef", "ghijkl"]
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "m");
    assert_eq!(get_row_text(&terminal_engine, 1), "n");

    resize_terminal_engine(&mut terminal_engine, 3, 2);
    assert_eq!(
        get_scrollback_text_rows(&terminal_engine),
        vec!["abc", "def", "ghi", "jkl"]
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "m");
    assert_eq!(get_row_text(&terminal_engine, 1), "n");

    resize_terminal_engine(&mut terminal_engine, 6, 2);
    assert_eq!(
        get_scrollback_text_rows(&terminal_engine),
        vec!["abcdef", "ghijkl"]
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "m");
    assert_eq!(get_row_text(&terminal_engine, 1), "n");
}

#[test]
fn wide_glyph_wrap_leaves_a_spacer_and_rejoins_without_a_phantom_space() {
    let mut terminal_engine = build_terminal_engine(4, 3);
    feed_terminal_text(&mut terminal_engine, "abc\u{6f22}"); // 漢 needs two columns; only one is free.
    assert_eq!(get_row_text(&terminal_engine, 0), "abc");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::SoftWide);
    assert_eq!(get_row_text(&terminal_engine, 1), "\u{6f22}");

    // Widening drops the spacer: the glyph reattaches directly after `c`.
    resize_terminal_engine(&mut terminal_engine, 8, 3);
    assert_eq!(get_row_text(&terminal_engine, 0), "abc\u{6f22}");
    let grid = terminal_engine.get_terminal_state().get_active_grid();
    assert_eq!(grid.get_cell(0, 3).unwrap().get_character(), '\u{6f22}');
    assert_eq!(grid.get_cell(0, 3).unwrap().get_display_width(), 2);
    assert_eq!(grid.get_cell(0, 4).unwrap().get_display_width(), 0);

    // Narrowing again re-creates the spacer and the wrap.
    resize_terminal_engine(&mut terminal_engine, 4, 3);
    assert_eq!(get_row_text(&terminal_engine, 0), "abc");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::SoftWide);
    assert_eq!(get_row_text(&terminal_engine, 1), "\u{6f22}");
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_active_grid()
            .get_cell(1, 0)
            .unwrap()
            .get_display_width(),
        2
    );
}

#[test]
fn one_column_screen_stores_wide_glyphs_narrow() {
    let mut terminal_engine = build_terminal_engine(4, 4);
    feed_terminal_text(&mut terminal_engine, "\u{6f22}\u{5b57}"); // 漢字 fills the 4-column row.

    resize_terminal_engine(&mut terminal_engine, 1, 4);
    let grid = terminal_engine.get_terminal_state().get_active_grid();
    assert_eq!(grid.get_cell(0, 0).unwrap().get_character(), '\u{6f22}');
    assert_eq!(grid.get_cell(0, 0).unwrap().get_display_width(), 1);
    assert_eq!(grid.get_cell(1, 0).unwrap().get_character(), '\u{5b57}');
    assert_eq!(grid.get_cell(1, 0).unwrap().get_display_width(), 1);
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Soft);
    assert_eq!(get_row_end(&terminal_engine, 1), RowEnd::Hard);
}

#[test]
fn combining_marks_travel_with_their_base_through_a_reflow() {
    let mut terminal_engine = build_terminal_engine(4, 3);
    feed_terminal_text(&mut terminal_engine, "abce\u{0301}"); // é as e + combining acute at the last column
    resize_terminal_engine(&mut terminal_engine, 2, 3);
    let grid = terminal_engine.get_terminal_state().get_active_grid();
    assert_eq!(grid.get_cell(1, 1).unwrap().get_character(), 'e');
    assert_eq!(
        grid.get_cell(1, 1).unwrap().list_combining_characters(),
        &['\u{0301}']
    );
}

#[test]
fn erase_to_end_of_line_breaks_the_continuation() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "abcdefghij");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Soft);

    // CUP to row 1 column 5 (0-based (0, 4)), then EL(0): the erase runs to
    // the row's last column, breaking its continuation into "ij".
    feed_terminal_text(&mut terminal_engine, "\x1b[1;5H\x1b[K");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Hard);

    // The lines no longer join: widening keeps them separate.
    resize_terminal_engine(&mut terminal_engine, 12, 4);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcd");
    assert_eq!(get_row_text(&terminal_engine, 1), "ij");
}

#[test]
fn overwriting_the_last_column_resets_a_stale_wrap() {
    let mut terminal_engine = build_terminal_engine(4, 4);
    feed_terminal_text(&mut terminal_engine, "abcdef"); // row 0 soft-wraps into "ef"
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Soft);

    // Rewrite the last column of row 0 without wrapping afterwards.
    feed_terminal_text(&mut terminal_engine, "\x1b[1;4HX");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Hard);

    resize_terminal_engine(&mut terminal_engine, 8, 4);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcX");
    assert_eq!(get_row_text(&terminal_engine, 1), "ef");
}

#[test]
fn styled_blank_tail_counts_as_content() {
    // A red-background erase paints the row tail; those cells are visual
    // content and must survive a reflow, not be trimmed as padding.
    let mut terminal_engine = build_terminal_engine(6, 2);
    feed_terminal_text(&mut terminal_engine, "ab\x1b[41m\x1b[K");
    resize_terminal_engine(&mut terminal_engine, 8, 2);
    let grid = terminal_engine.get_terminal_state().get_active_grid();
    assert_eq!(grid.get_cell(0, 0).unwrap().get_character(), 'a');
    let painted = grid.get_cell(0, 5).unwrap();
    assert_eq!(painted.get_character(), ' ');
    let mut red = Style::default();
    red.set_background_color(Color::Indexed(1));
    assert_eq!(painted.get_style(), red);
}

#[test]
fn styled_blank_row_below_the_cursor_survives_a_shrink() {
    // Row 1 is spaces with a blue background — visual content by the same
    // rule the unwind uses — while rows 2 and 3 are true default padding.
    // A height shrink drops the padding rows and keeps the colored row.
    let mut terminal_engine = build_terminal_engine(6, 4);
    feed_terminal_text(&mut terminal_engine, "top\r\n\x1b[44m\x1b[2K\x1b[0m\x1b[H");

    resize_terminal_engine(&mut terminal_engine, 6, 2);
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        0
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "top");
    let painted = terminal_engine
        .get_terminal_state()
        .get_active_grid()
        .get_cell(1, 0)
        .unwrap();
    assert_eq!(painted.get_character(), ' ');
    let mut blue = Style::default();
    blue.set_background_color(Color::Indexed(4));
    assert_eq!(painted.get_style(), blue);
}

#[test]
fn trailing_blank_rows_drop_instead_of_entering_history() {
    let mut terminal_engine = build_terminal_engine(20, 10);
    feed_terminal_text(&mut terminal_engine, "hi");
    resize_terminal_engine(&mut terminal_engine, 20, 4);
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        0
    );
    assert_eq!(get_row_text(&terminal_engine, 0), "hi");
    assert_eq!(get_cursor_position(&terminal_engine), (0, 2));
}

#[test]
fn alternate_screen_crops_and_never_reflows() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "primary!"); // exactly fills row 0, hard end
    feed_terminal_text(&mut terminal_engine, "\x1b[?1049h\x1b[H"); // enter the alternate screen, home
    feed_terminal_text(&mut terminal_engine, "abcdefghij"); // wraps at 8 on the alt screen

    resize_terminal_engine(&mut terminal_engine, 4, 4);
    // Alt: rows crop to 4 columns — TUI apps repaint after a resize.
    assert_eq!(get_row_text(&terminal_engine, 0), "abcd");
    assert_eq!(get_row_text(&terminal_engine, 1), "ij");

    // Primary reflowed underneath and comes back re-wrapped.
    feed_terminal_text(&mut terminal_engine, "\x1b[?1049l");
    assert_eq!(get_row_text(&terminal_engine, 0), "prim");
    assert_eq!(get_row_text(&terminal_engine, 1), "ary!");
}

#[test]
fn reflow_respects_the_scrollback_caps_and_stays_monotonic() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 2,
    });
    terminal_state.scrollback =
        Scrollback::from_scrollback_limit(ScrollbackLimit::from_line_and_byte_limits(2, 100_000));
    let mut engine = vte::Parser::new();
    engine.advance(&mut terminal_state, b"abcdefgh12345678\r\nx");
    // The first row scrolled into history as the line feed arrived.
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 1);
    assert_eq!(terminal_state.scrollback.get_dropped_line_count(), 0);

    // At width 4 the 16-cell line needs 4 rows; 3 overflow the 2-row screen
    // but only 2 fit the cap — the oldest drops and is tallied. The retained
    // count grew by one, so the monotonic counter grows by one.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 2);
    assert_eq!(terminal_state.scrollback.get_dropped_line_count(), 1);
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 2);

    // Widening pulls one row back onto the screen: history shrinks, the
    // monotonic counter stays put.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 8,
        row_count: 2,
    });
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 1);
    assert_eq!(terminal_state.scrollback.get_total_pushed_line_count(), 2);
}

#[test]
fn rewrap_line_splits_exactly_and_marks_ends() {
    let cells: Vec<Cell> = "abcdef"
        .chars()
        .map(|character| Cell::from_character(character, 1, Style::default()))
        .collect();
    let wrapped_rows = rewrap_line(cells.clone(), 4, Style::default());
    assert_eq!(
        wrapped_rows,
        vec![
            (
                cells[..4].to_vec(),
                RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: false,
                }
            ),
            (
                cells[4..].to_vec(),
                RowMetadata {
                    row_end: RowEnd::Hard,
                    has_prompt_mark: false,
                }
            ),
        ]
    );
}

#[test]
fn rewrap_line_of_empty_content_is_one_hard_row() {
    let wrapped_rows = rewrap_line(Vec::new(), 4, Style::default());
    assert_eq!(wrapped_rows, vec![(Vec::new(), RowMetadata::default())]);
}

#[test]
fn content_len_trims_only_fully_default_blanks() {
    let mut red = Style::default();
    red.set_background_color(Color::Indexed(1));
    let row_cells = vec![
        Cell::from_character('a', 1, Style::default()),
        Cell::blank(),
        Cell::blank_with(red),
        Cell::blank(),
        Cell::blank(),
    ];
    assert_eq!(count_row_content_cells(&row_cells), 3);
    assert_eq!(count_row_content_cells(&[Cell::blank(), Cell::blank()]), 0);
}

#[test]
fn locate_content_offset_walks_soft_rows_and_parks_in_final_padding() {
    let soft = |text: &str| {
        (
            text.chars()
                .map(|character| Cell::from_character(character, 1, Style::default()))
                .collect::<Vec<_>>(),
            RowMetadata {
                row_end: RowEnd::Soft,
                has_prompt_mark: false,
            },
        )
    };
    let hard = |text: &str| {
        (
            text.chars()
                .map(|character| Cell::from_character(character, 1, Style::default()))
                .collect::<Vec<_>>(),
            RowMetadata {
                row_end: RowEnd::Hard,
                has_prompt_mark: false,
            },
        )
    };
    let wrapped_rows = vec![soft("abcd"), soft("efgh"), hard("ij")];
    assert_eq!(locate_content_offset(&wrapped_rows, 0), (0, 0));
    assert_eq!(locate_content_offset(&wrapped_rows, 3), (0, 3));
    assert_eq!(locate_content_offset(&wrapped_rows, 4), (1, 0));
    assert_eq!(locate_content_offset(&wrapped_rows, 9), (2, 1));
    // Past the content: parks in the final row's padding.
    assert_eq!(locate_content_offset(&wrapped_rows, 11), (2, 3));
}

/// Every logical line visible anywhere (history then screen), soft wraps
/// collapsed — the reflow invariant is that this list never changes across
/// resizes, only how it is cut into rows.
fn logical_lines(engine: &TerminalEngine) -> Vec<String> {
    let terminal_state = engine.get_terminal_state();
    let mut physical: Vec<(Vec<Cell>, RowMetadata)> = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .cloned()
        .collect();
    let grid = terminal_state.get_active_grid();
    let (row_count, _) = grid.get_grid_dimensions();
    for row_index in 0..row_count {
        physical.push((
            grid.list_rows()[row_index as usize].clone(),
            RowMetadata {
                row_end: grid.get_row_end(row_index),
                has_prompt_mark: grid.has_prompt_mark(row_index),
            },
        ));
    }
    let mut lines = Vec::new();
    let mut current_logical_line = String::new();
    for (cells, row_metadata) in physical {
        let row_text: String = cells
            .iter()
            .filter(|cell| cell.get_display_width() != 0)
            .map(Cell::get_character)
            .collect();
        match row_metadata.row_end {
            RowEnd::Soft => current_logical_line.push_str(&row_text),
            RowEnd::SoftWide => {
                let trimmed_row_text = row_text.strip_suffix(' ').unwrap_or(&row_text);
                current_logical_line.push_str(trimmed_row_text);
            }
            RowEnd::Hard => {
                current_logical_line.push_str(row_text.trim_end());
                lines.push(std::mem::take(&mut current_logical_line));
            }
        }
    }
    if !current_logical_line.is_empty() {
        lines.push(current_logical_line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[test]
fn mixed_content_survives_a_resize_chain_losslessly() {
    let mut terminal_engine = build_terminal_engine(10, 6);
    feed_terminal_text(
        &mut terminal_engine,
        "hello world\r\nab \u{6f22}\u{5b57} cd\r\nx\r\ntail",
    );
    let original_logical_lines = logical_lines(&terminal_engine);
    assert_eq!(
        original_logical_lines,
        vec!["hello world", "ab \u{6f22}\u{5b57} cd", "x", "tail"]
    );

    for (column_count, row_count) in [(5, 6), (3, 4), (7, 3), (12, 6), (10, 6)] {
        resize_terminal_engine(&mut terminal_engine, column_count, row_count);
        assert_eq!(
            logical_lines(&terminal_engine),
            original_logical_lines,
            "content changed at {column_count}x{row_count}"
        );
    }
}

#[test]
fn colored_text_keeps_its_style_across_reflow() {
    let mut terminal_engine = build_terminal_engine(4, 2);
    feed_terminal_text(&mut terminal_engine, "\x1b[31mabcdef"); // red text soft-wraps at 4
    resize_terminal_engine(&mut terminal_engine, 8, 2);

    let mut red = Style::default();
    red.set_foreground_color(Color::Indexed(1));
    let grid = terminal_engine.get_terminal_state().get_active_grid();
    for column_index in 0..6 {
        assert_eq!(
            grid.get_cell(0, column_index).unwrap().get_style(),
            red,
            "column {column_index}"
        );
    }
    assert_eq!(get_row_text(&terminal_engine, 0), "abcdef");
}

#[test]
fn autowrap_off_never_records_soft_ends() {
    let mut terminal_engine = build_terminal_engine(8, 2);
    feed_terminal_text(&mut terminal_engine, "\x1b[?7l"); // DECAWM off: glyphs overwrite the last column
    feed_terminal_text(&mut terminal_engine, "abcdefghij");
    assert_eq!(get_row_text(&terminal_engine, 0), "abcdefgj");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Hard);

    // A shrink still wraps the too-long hard line (nothing is cropped), and
    // widening re-joins it — the round trip is lossless.
    resize_terminal_engine(&mut terminal_engine, 4, 2);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcd");
    assert_eq!(get_row_text(&terminal_engine, 1), "efgj");
    resize_terminal_engine(&mut terminal_engine, 8, 2);
    assert_eq!(get_row_text(&terminal_engine, 0), "abcdefgj");
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::Hard);
}

#[test]
fn a_wrapped_row_keeps_its_link_when_it_scrolls_into_history() {
    // One continuous line long enough to wrap across the whole screen and push
    // its first row into history. Every row of it is a continuation, so every
    // row end -- in history and on screen -- must stay Soft except the last.
    //
    // `delete_lines` force-marks the row sliding to the band's bottom `Hard`,
    // which is right for a linefeed-driven scroll and wrong for a wrap-driven
    // one; `wrap_linefeed` re-applies the real end afterwards. Without that
    // repair, double-clicking a word straddling the boundary selects only its
    // on-screen half.
    let mut engine = build_terminal_engine(10, 3);
    feed_terminal_text(&mut engine, "abcdefghijklmnopqrstuvwxyz0123456789");

    let terminal_state = engine.get_terminal_state();
    let history: Vec<RowEnd> = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .map(|(_, row_metadata)| row_metadata.row_end)
        .collect();
    assert_eq!(history, vec![RowEnd::Soft]);

    let grid = terminal_state.get_active_grid();
    assert_eq!(grid.get_row_end(0), RowEnd::Soft);
    assert_eq!(grid.get_row_end(1), RowEnd::Soft);
    assert_eq!(grid.get_row_end(2), RowEnd::Hard);
}

#[test]
fn a_linefeed_scroll_still_ends_its_row_hard() {
    // The counterpart: content scrolled off by an explicit newline genuinely
    // ends, so its history row must be Hard. Guards against "fix" the wrap case
    // by making every boundary Soft.
    let mut engine = build_terminal_engine(10, 3);
    feed_terminal_text(&mut engine, "one\r\ntwo\r\nthree\r\nfour");

    let terminal_state = engine.get_terminal_state();
    let history: Vec<RowEnd> = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .map(|(_, row_metadata)| row_metadata.row_end)
        .collect();
    assert_eq!(history, vec![RowEnd::Hard]);
}

/// One default-styled narrow cell per char of `text`.
fn cells(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|character| Cell::from_character(character, 1, Style::default()))
        .collect()
}

/// A wide glyph as stored in the grid: the width-2 base and its width-0
/// continuation cell.
fn build_wide_cell_pair(ch: char) -> [Cell; 2] {
    [
        Cell::from_character(ch, 2, Style::default()),
        Cell::from_character(' ', 0, Style::default()),
    ]
}

/// The prompt mark of every screen row, top to bottom.
fn prompt_marks(terminal_state: &TerminalState) -> Vec<bool> {
    let (row_count, _) = terminal_state.primary.get_grid_dimensions();
    (0..row_count)
        .map(|row_index| terminal_state.primary.has_prompt_mark(row_index))
        .collect()
}

#[test]
fn rewrap_line_at_zero_columns_wraps_at_one_column() {
    let content = cells("abc");
    let wrapped_rows = rewrap_line(content, 0, Style::default());
    let soft = RowMetadata {
        row_end: RowEnd::Soft,
        has_prompt_mark: false,
    };
    assert_eq!(
        wrapped_rows,
        vec![
            (cells("a"), soft),
            (cells("b"), soft),
            (cells("c"), RowMetadata::default()),
        ]
    );
}

#[test]
fn rewrap_line_leaves_a_spacer_in_the_fill_before_a_wide_glyph_at_the_last_column() {
    let mut red = Style::default();
    red.set_background_color(Color::Indexed(1));
    let mut content = cells("abc");
    content.extend(build_wide_cell_pair('\u{6f22}'));

    let wrapped_rows = rewrap_line(content, 4, red);

    let mut leading_row_cells = cells("abc");
    leading_row_cells.push(Cell::blank_with(red));
    assert_eq!(
        wrapped_rows,
        vec![
            (
                leading_row_cells,
                RowMetadata {
                    row_end: RowEnd::SoftWide,
                    has_prompt_mark: false,
                }
            ),
            (
                build_wide_cell_pair('\u{6f22}').to_vec(),
                RowMetadata::default()
            ),
        ]
    );
}

#[test]
fn rewrap_line_at_one_column_stores_a_wide_glyph_narrow_and_skips_its_continuation() {
    let mut content = build_wide_cell_pair('\u{6f22}').to_vec();
    content.extend(build_wide_cell_pair('\u{5b57}'));

    let wrapped_rows = rewrap_line(content, 1, Style::default());

    assert_eq!(
        wrapped_rows,
        vec![
            (
                vec![Cell::from_character('\u{6f22}', 1, Style::default())],
                RowMetadata {
                    row_end: RowEnd::Soft,
                    has_prompt_mark: false,
                }
            ),
            (
                vec![Cell::from_character('\u{5b57}', 1, Style::default())],
                RowMetadata::default()
            ),
        ]
    );
}

#[test]
fn locate_content_offset_skips_a_soft_wide_spacer() {
    let mut leading_row_cells = cells("abc");
    leading_row_cells.push(Cell::blank());
    let mut following_row_cells = build_wide_cell_pair('\u{6f22}').to_vec();
    following_row_cells.extend(cells("c"));
    let wrapped_rows = vec![
        (
            leading_row_cells,
            RowMetadata {
                row_end: RowEnd::SoftWide,
                has_prompt_mark: false,
            },
        ),
        (following_row_cells, RowMetadata::default()),
    ];
    // Offsets 0-2 are `abc`; the spacer holds none, so offset 3 is the wide
    // glyph at the start of the next row and offset 5 is the `c` after it.
    assert_eq!(locate_content_offset(&wrapped_rows, 2), (0, 2));
    assert_eq!(locate_content_offset(&wrapped_rows, 3), (1, 0));
    assert_eq!(locate_content_offset(&wrapped_rows, 5), (1, 2));
}

#[test]
fn prompt_mark_follows_its_row_when_the_line_above_wraps() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 4,
    });
    let mut parser = vte::Parser::new();
    parser.advance(&mut terminal_state, b"abcdefg\r\n\x1b]133;A\x07$ ");
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![false, true, false, false]
    );

    // `abcdefg` needs two rows at width 4, pushing the prompt row down one.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 4,
    });
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![false, false, true, false]
    );
    assert_eq!(
        terminal_state.primary.list_rows()[2][0].get_character(),
        '$'
    );

    terminal_state.resize_terminal_state(PtySize {
        column_count: 8,
        row_count: 4,
    });
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![false, true, false, false]
    );
}

#[test]
fn a_prompt_mark_on_a_wrapped_line_stays_on_its_first_row() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 4,
    });
    let mut parser = vte::Parser::new();
    parser.advance(&mut terminal_state, b"\x1b]133;A\x07abcdefgh");
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![true, false, false, false]
    );

    terminal_state.resize_terminal_state(PtySize {
        column_count: 2,
        row_count: 4,
    });
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![true, false, false, false]
    );

    terminal_state.resize_terminal_state(PtySize {
        column_count: 8,
        row_count: 4,
    });
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![true, false, false, false]
    );
}

#[test]
fn a_prompt_mark_on_a_continuation_row_moves_to_the_lines_first_row() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 8,
        row_count: 4,
    });
    let mut parser = vte::Parser::new();
    parser.advance(&mut terminal_state, b"abcdefghij");
    // Row 1 holds `ij`, the continuation of row 0's line.
    terminal_state.active_grid_mut().set_prompt_mark(1, true);

    // The whole line fits one row: the mark lands on that row.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 12,
        row_count: 4,
    });
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![true, false, false, false]
    );

    // Wrapping again keeps the mark on the line's first row.
    terminal_state.resize_terminal_state(PtySize {
        column_count: 8,
        row_count: 4,
    });
    assert_eq!(
        prompt_marks(&terminal_state),
        vec![true, false, false, false]
    );
}

#[test]
fn cursor_parked_past_the_text_keeps_its_column_when_the_width_holds_it() {
    let mut terminal_engine = build_terminal_engine(8, 2);
    feed_terminal_text(&mut terminal_engine, "ab\x1b[1;7H"); // cursor to column 6, four cells past `ab`
    assert_eq!(get_cursor_position(&terminal_engine), (0, 6));

    resize_terminal_engine(&mut terminal_engine, 12, 2);
    assert_eq!(get_cursor_position(&terminal_engine), (0, 6));

    // Width 4 cannot hold column 6: the cursor clamps to the last column.
    resize_terminal_engine(&mut terminal_engine, 4, 2);
    assert_eq!(get_cursor_position(&terminal_engine), (0, 3));

    // The clamp is what the next reflow starts from.
    resize_terminal_engine(&mut terminal_engine, 12, 2);
    assert_eq!(get_cursor_position(&terminal_engine), (0, 3));
}

#[test]
fn cursor_on_a_soft_wide_spacer_lands_on_the_wide_glyph_after_widening() {
    let mut terminal_engine = build_terminal_engine(4, 3);
    feed_terminal_text(&mut terminal_engine, "abc\u{6f22}\x1b[1;4H"); // cursor onto row 0's spacer
    assert_eq!(get_cursor_position(&terminal_engine), (0, 3));
    assert_eq!(get_row_end(&terminal_engine, 0), RowEnd::SoftWide);

    resize_terminal_engine(&mut terminal_engine, 8, 3);
    assert_eq!(get_cursor_position(&terminal_engine), (0, 3));
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_active_grid()
            .get_cell(0, 3)
            .unwrap()
            .get_character(),
        '\u{6f22}'
    );
}

#[test]
fn resizing_to_the_same_size_changes_nothing() {
    let mut terminal_engine = build_terminal_engine(8, 4);
    feed_terminal_text(&mut terminal_engine, "abcdefghij\r\nxy");
    let original_reflow_state = (
        logical_lines(&terminal_engine),
        (0..4)
            .map(|row_index| get_row_end(&terminal_engine, row_index))
            .collect::<Vec<_>>(),
        get_cursor_position(&terminal_engine),
    );
    assert_eq!(
        original_reflow_state.1,
        vec![RowEnd::Soft, RowEnd::Hard, RowEnd::Hard, RowEnd::Hard]
    );
    assert_eq!(original_reflow_state.2, (2, 2));

    resize_terminal_engine(&mut terminal_engine, 8, 4);
    assert_eq!(
        (
            logical_lines(&terminal_engine),
            (0..4)
                .map(|row_index| get_row_end(&terminal_engine, row_index))
                .collect::<Vec<_>>(),
            get_cursor_position(&terminal_engine),
        ),
        original_reflow_state
    );
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        0
    );
}

#[test]
fn a_trailing_soft_history_row_becomes_a_hard_line_on_regrow() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 0,
    });
    terminal_state.scrollback.push_row(
        &cells("ab"),
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: false,
        },
    );

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });

    let mut expected_regrown_cells = cells("ab");
    expected_regrown_cells.extend([Cell::blank(), Cell::blank()]);
    assert_eq!(
        terminal_state.primary.list_rows()[0],
        expected_regrown_cells
    );
    assert_eq!(terminal_state.primary.get_row_end(0), RowEnd::Hard);
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
    assert_eq!(
        (
            terminal_state.primary_cursor.row,
            terminal_state.primary_cursor.column
        ),
        (0, 0)
    );
}

#[test]
fn a_prompt_mark_on_an_empty_trailing_soft_row_survives_regrow() {
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 4,
        row_count: 0,
    });
    terminal_state.scrollback.push_row(
        &[],
        RowMetadata {
            row_end: RowEnd::Soft,
            has_prompt_mark: true,
        },
    );

    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });

    assert_eq!(
        terminal_state.primary.list_rows()[0],
        vec![Cell::blank(); 4]
    );
    assert_eq!(terminal_state.primary.get_row_end(0), RowEnd::Hard);
    assert_eq!(prompt_marks(&terminal_state), vec![true, false]);
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 0);
}

#[test]
fn height_shrink_with_the_cursor_above_scrolled_content_clamps_it_to_the_top_row() {
    let mut terminal_engine = build_terminal_engine(4, 3);
    feed_terminal_text(&mut terminal_engine, "a\r\nb\r\nc\x1b[H");
    assert_eq!(get_cursor_position(&terminal_engine), (0, 0));

    // Rows `a` and `b` are content, so they scroll into history rather than
    // dropping; the cursor's own row goes with them and the cursor clamps to
    // the top row, which now holds `c`.
    resize_terminal_engine(&mut terminal_engine, 4, 1);
    assert_eq!(get_scrollback_text_rows(&terminal_engine), vec!["a", "b"]);
    assert_eq!(get_row_text(&terminal_engine, 0), "c");
    assert_eq!(get_cursor_position(&terminal_engine), (0, 0));
}

#[test]
fn a_blank_prompt_marked_row_below_the_cursor_enters_history_on_a_height_shrink() {
    // A prompt mark is content: the row it sits on is not trailing padding,
    // however few cells it holds.
    let mut engine = build_terminal_engine(6, 4);
    feed_terminal_text(&mut engine, "a\r\n\x1b]133;A\x07\x1b[H");
    assert!(engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(1));
    assert_eq!(get_cursor_position(&engine), (0, 0));

    resize_terminal_engine(&mut engine, 6, 1);

    // The marked row is kept, so the row above it is the one that overflows
    // into history and the mark stays on the one screen row.
    let history: Vec<bool> = engine
        .get_terminal_state()
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .map(|(_, row_metadata)| row_metadata.has_prompt_mark)
        .collect();
    assert_eq!(history, vec![false]);
    assert!(engine
        .get_terminal_state()
        .get_active_grid()
        .has_prompt_mark(0));
}
