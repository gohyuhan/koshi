//! Tests for resize reflow: soft-wrap re-joining, re-wrapping to narrower
//! and wider screens, hard-break preservation, wide-glyph spacers, cursor
//! tracking, scrollback round-trips, caps, and the alternate-screen crop.

use super::*;

use crate::engine::TerminalEngine;
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::style::Color;

fn engine(cols: u16, rows: u16) -> TerminalEngine {
    TerminalEngine::new(PtySize { cols, rows })
}

fn feed(engine: &mut TerminalEngine, bytes: &str) {
    let _ = engine.advance(bytes.as_bytes());
}

fn resize(engine: &mut TerminalEngine, cols: u16, rows: u16) {
    engine.resize(PtySize { cols, rows });
}

/// The visible text of `row`: base characters of non-continuation cells,
/// trailing spaces trimmed.
fn row_text(engine: &TerminalEngine, row: u16) -> String {
    let grid = engine.state().active_grid();
    let (_, cols) = grid.dimensions();
    let text: String = (0..cols)
        .filter_map(|col| grid.cell(row, col))
        .filter(|cell| cell.width() != 0)
        .map(Cell::ch)
        .collect();
    text.trim_end().to_string()
}

/// The visible text of every retained history row, oldest first.
fn history_text(engine: &TerminalEngine) -> Vec<String> {
    engine
        .state()
        .scrollback()
        .lines()
        .iter()
        .map(|(cells, _)| {
            cells
                .iter()
                .filter(|cell| cell.width() != 0)
                .map(Cell::ch)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn cursor(engine: &TerminalEngine) -> (u16, u16) {
    engine.state().active_cursor_position()
}

fn row_end(engine: &TerminalEngine, row: u16) -> RowEnd {
    engine.state().active_grid().row_end(row)
}

#[test]
fn print_wrap_records_a_soft_row_end() {
    let mut e = engine(8, 4);
    feed(&mut e, "abcdefghij");
    assert_eq!(row_text(&e, 0), "abcdefgh");
    assert_eq!(row_text(&e, 1), "ij");
    assert_eq!(row_end(&e, 0), RowEnd::Soft);
    assert_eq!(row_end(&e, 1), RowEnd::Hard);
}

#[test]
fn linefeed_after_a_full_row_keeps_the_hard_end() {
    // The row exactly fills the width but the app sent a real line break:
    // the rows are two logical lines and a reflow must never join them.
    let mut e = engine(4, 4);
    feed(&mut e, "abcd\r\nef");
    assert_eq!(row_end(&e, 0), RowEnd::Hard);

    resize(&mut e, 8, 4);
    assert_eq!(row_text(&e, 0), "abcd");
    assert_eq!(row_text(&e, 1), "ef");
}

#[test]
fn shrink_wraps_a_long_line_instead_of_cropping() {
    let mut e = engine(8, 4);
    feed(&mut e, "abcdefghij");

    resize(&mut e, 4, 4);
    assert_eq!(row_text(&e, 0), "abcd");
    assert_eq!(row_text(&e, 1), "efgh");
    assert_eq!(row_text(&e, 2), "ij");
    assert_eq!(row_text(&e, 3), "");
    assert_eq!(row_end(&e, 0), RowEnd::Soft);
    assert_eq!(row_end(&e, 1), RowEnd::Soft);
    assert_eq!(row_end(&e, 2), RowEnd::Hard);
    assert_eq!(e.state().scrollback().len(), 0);
}

#[test]
fn widen_rejoins_soft_wrapped_rows() {
    let mut e = engine(8, 4);
    feed(&mut e, "abcdefghij");

    resize(&mut e, 4, 4);
    resize(&mut e, 8, 4);
    assert_eq!(row_text(&e, 0), "abcdefgh");
    assert_eq!(row_text(&e, 1), "ij");

    resize(&mut e, 12, 4);
    assert_eq!(row_text(&e, 0), "abcdefghij");
    assert_eq!(row_end(&e, 0), RowEnd::Hard);
}

#[test]
fn cursor_follows_its_content_offset_across_reflows() {
    let mut e = engine(8, 4);
    feed(&mut e, "abcdefghij");
    assert_eq!(cursor(&e), (1, 2));

    resize(&mut e, 4, 4);
    assert_eq!(cursor(&e), (2, 2));

    resize(&mut e, 8, 4);
    assert_eq!(cursor(&e), (1, 2));

    resize(&mut e, 12, 4);
    assert_eq!(cursor(&e), (0, 10));
}

#[test]
fn shrink_overflow_enters_history_and_widen_pulls_it_back() {
    let mut e = engine(6, 3);
    feed(&mut e, "aaaaaa\r\nbbb\r\ncc");

    // At width 3 the first line needs two rows: four rows of content on a
    // three-row screen, so the oldest row scrolls into history.
    resize(&mut e, 3, 3);
    assert_eq!(history_text(&e), vec!["aaa"]);
    assert_eq!(
        e.state().scrollback().lines()[0].1.end,
        RowEnd::Soft,
        "the history row must remember it soft-wraps into the screen"
    );
    assert_eq!(row_text(&e, 0), "aaa");
    assert_eq!(row_text(&e, 1), "bbb");
    assert_eq!(row_text(&e, 2), "cc");

    // Widening re-joins the split line across the history boundary and
    // empties history again.
    resize(&mut e, 6, 3);
    assert_eq!(e.state().scrollback().len(), 0);
    assert_eq!(row_text(&e, 0), "aaaaaa");
    assert_eq!(row_text(&e, 1), "bbb");
    assert_eq!(row_text(&e, 2), "cc");
}

#[test]
fn scrollback_rows_rewrap_with_the_screen() {
    // Two full-width lines scroll into history, then the width halves: every
    // history row re-wraps and no text is lost.
    let mut e = engine(6, 2);
    feed(&mut e, "abcdef\r\nghijkl\r\nm\r\nn");
    assert_eq!(history_text(&e), vec!["abcdef", "ghijkl"]);
    assert_eq!(row_text(&e, 0), "m");
    assert_eq!(row_text(&e, 1), "n");

    resize(&mut e, 3, 2);
    assert_eq!(history_text(&e), vec!["abc", "def", "ghi", "jkl"]);
    assert_eq!(row_text(&e, 0), "m");
    assert_eq!(row_text(&e, 1), "n");

    resize(&mut e, 6, 2);
    assert_eq!(history_text(&e), vec!["abcdef", "ghijkl"]);
    assert_eq!(row_text(&e, 0), "m");
    assert_eq!(row_text(&e, 1), "n");
}

#[test]
fn wide_glyph_wrap_leaves_a_spacer_and_rejoins_without_a_phantom_space() {
    let mut e = engine(4, 3);
    feed(&mut e, "abc\u{6f22}"); // 漢 needs two columns; only one is free.
    assert_eq!(row_text(&e, 0), "abc");
    assert_eq!(row_end(&e, 0), RowEnd::SoftWide);
    assert_eq!(row_text(&e, 1), "\u{6f22}");

    // Widening drops the spacer: the glyph reattaches directly after `c`.
    resize(&mut e, 8, 3);
    assert_eq!(row_text(&e, 0), "abc\u{6f22}");
    let grid = e.state().active_grid();
    assert_eq!(grid.cell(0, 3).unwrap().ch(), '\u{6f22}');
    assert_eq!(grid.cell(0, 3).unwrap().width(), 2);
    assert_eq!(grid.cell(0, 4).unwrap().width(), 0);

    // Narrowing again re-creates the spacer and the wrap.
    resize(&mut e, 4, 3);
    assert_eq!(row_text(&e, 0), "abc");
    assert_eq!(row_end(&e, 0), RowEnd::SoftWide);
    assert_eq!(row_text(&e, 1), "\u{6f22}");
    assert_eq!(e.state().active_grid().cell(1, 0).unwrap().width(), 2);
}

#[test]
fn one_column_screen_stores_wide_glyphs_narrow() {
    let mut e = engine(4, 4);
    feed(&mut e, "\u{6f22}\u{5b57}"); // 漢字 fills the 4-column row.

    resize(&mut e, 1, 4);
    let grid = e.state().active_grid();
    assert_eq!(grid.cell(0, 0).unwrap().ch(), '\u{6f22}');
    assert_eq!(grid.cell(0, 0).unwrap().width(), 1);
    assert_eq!(grid.cell(1, 0).unwrap().ch(), '\u{5b57}');
    assert_eq!(grid.cell(1, 0).unwrap().width(), 1);
    assert_eq!(row_end(&e, 0), RowEnd::Soft);
    assert_eq!(row_end(&e, 1), RowEnd::Hard);
}

#[test]
fn combining_marks_travel_with_their_base_through_a_reflow() {
    let mut e = engine(4, 3);
    feed(&mut e, "abce\u{0301}"); // é as e + combining acute at the last column
    resize(&mut e, 2, 3);
    let grid = e.state().active_grid();
    assert_eq!(grid.cell(1, 1).unwrap().ch(), 'e');
    assert_eq!(grid.cell(1, 1).unwrap().combining(), &['\u{0301}']);
}

#[test]
fn erase_to_end_of_line_breaks_the_continuation() {
    let mut e = engine(8, 4);
    feed(&mut e, "abcdefghij");
    assert_eq!(row_end(&e, 0), RowEnd::Soft);

    // CUP to row 1 column 5 (0-based (0, 4)), then EL(0): the erase runs to
    // the row's last column, breaking its continuation into "ij".
    feed(&mut e, "\x1b[1;5H\x1b[K");
    assert_eq!(row_end(&e, 0), RowEnd::Hard);

    // The lines no longer join: widening keeps them separate.
    resize(&mut e, 12, 4);
    assert_eq!(row_text(&e, 0), "abcd");
    assert_eq!(row_text(&e, 1), "ij");
}

#[test]
fn overwriting_the_last_column_resets_a_stale_wrap() {
    let mut e = engine(4, 4);
    feed(&mut e, "abcdef"); // row 0 soft-wraps into "ef"
    assert_eq!(row_end(&e, 0), RowEnd::Soft);

    // Rewrite the last column of row 0 without wrapping afterwards.
    feed(&mut e, "\x1b[1;4HX");
    assert_eq!(row_end(&e, 0), RowEnd::Hard);

    resize(&mut e, 8, 4);
    assert_eq!(row_text(&e, 0), "abcX");
    assert_eq!(row_text(&e, 1), "ef");
}

#[test]
fn styled_blank_tail_counts_as_content() {
    // A red-background erase paints the row tail; those cells are visual
    // content and must survive a reflow, not be trimmed as padding.
    let mut e = engine(6, 2);
    feed(&mut e, "ab\x1b[41m\x1b[K");
    resize(&mut e, 8, 2);
    let grid = e.state().active_grid();
    assert_eq!(grid.cell(0, 0).unwrap().ch(), 'a');
    let painted = grid.cell(0, 5).unwrap();
    assert_eq!(painted.ch(), ' ');
    let mut red = Style::default();
    red.set_bg(Color::Indexed(1));
    assert_eq!(painted.style(), red);
}

#[test]
fn styled_blank_row_below_the_cursor_survives_a_shrink() {
    // Row 1 is spaces with a blue background — visual content by the same
    // rule the unwind uses — while rows 2 and 3 are true default padding.
    // A height shrink drops the padding rows and keeps the colored row.
    let mut e = engine(6, 4);
    feed(&mut e, "top\r\n\x1b[44m\x1b[2K\x1b[0m\x1b[H");

    resize(&mut e, 6, 2);
    assert_eq!(e.state().scrollback().len(), 0);
    assert_eq!(row_text(&e, 0), "top");
    let painted = e.state().active_grid().cell(1, 0).unwrap();
    assert_eq!(painted.ch(), ' ');
    let mut blue = Style::default();
    blue.set_bg(Color::Indexed(4));
    assert_eq!(painted.style(), blue);
}

#[test]
fn trailing_blank_rows_drop_instead_of_entering_history() {
    let mut e = engine(20, 10);
    feed(&mut e, "hi");
    resize(&mut e, 20, 4);
    assert_eq!(e.state().scrollback().len(), 0);
    assert_eq!(row_text(&e, 0), "hi");
    assert_eq!(cursor(&e), (0, 2));
}

#[test]
fn alternate_screen_crops_and_never_reflows() {
    let mut e = engine(8, 4);
    feed(&mut e, "primary!"); // exactly fills row 0, hard end
    feed(&mut e, "\x1b[?1049h\x1b[H"); // enter the alternate screen, home
    feed(&mut e, "abcdefghij"); // wraps at 8 on the alt screen

    resize(&mut e, 4, 4);
    // Alt: rows crop to 4 columns — TUI apps repaint after a resize.
    assert_eq!(row_text(&e, 0), "abcd");
    assert_eq!(row_text(&e, 1), "ij");

    // Primary reflowed underneath and comes back re-wrapped.
    feed(&mut e, "\x1b[?1049l");
    assert_eq!(row_text(&e, 0), "prim");
    assert_eq!(row_text(&e, 1), "ary!");
}

#[test]
fn reflow_respects_the_scrollback_caps_and_stays_monotonic() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 2 });
    state.scrollback = Scrollback::new(ScrollbackLimit::new(2, 100_000));
    let mut engine = vte::Parser::new();
    engine.advance(&mut state, b"abcdefgh12345678\r\nx");
    // The first row scrolled into history as the line feed arrived.
    assert_eq!(state.scrollback.total_pushed(), 1);
    assert_eq!(state.scrollback.dropped_lines(), 0);

    // At width 4 the 16-cell line needs 4 rows; 3 overflow the 2-row screen
    // but only 2 fit the cap — the oldest drops and is tallied. The retained
    // count grew by one, so the monotonic counter grows by one.
    state.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(state.scrollback.len(), 2);
    assert_eq!(state.scrollback.dropped_lines(), 1);
    assert_eq!(state.scrollback.total_pushed(), 2);

    // Widening pulls one row back onto the screen: history shrinks, the
    // monotonic counter stays put.
    state.resize(PtySize { cols: 8, rows: 2 });
    assert_eq!(state.scrollback.len(), 1);
    assert_eq!(state.scrollback.total_pushed(), 2);
}

#[test]
fn rewrap_line_splits_exactly_and_marks_ends() {
    let cells: Vec<Cell> = "abcdef"
        .chars()
        .map(|c| Cell::new(c, 1, Style::default()))
        .collect();
    let rows = rewrap_line(&cells, 4, Style::default());
    assert_eq!(
        rows,
        vec![
            (
                cells[..4].to_vec(),
                RowMeta {
                    end: RowEnd::Soft,
                    prompt: false,
                }
            ),
            (
                cells[4..].to_vec(),
                RowMeta {
                    end: RowEnd::Hard,
                    prompt: false,
                }
            ),
        ]
    );
}

#[test]
fn rewrap_line_of_empty_content_is_one_hard_row() {
    let rows = rewrap_line(&[], 4, Style::default());
    assert_eq!(rows, vec![(Vec::new(), RowMeta::default())]);
}

#[test]
fn content_len_trims_only_fully_default_blanks() {
    let mut red = Style::default();
    red.set_bg(Color::Indexed(1));
    let row = vec![
        Cell::new('a', 1, Style::default()),
        Cell::blank(),
        Cell::blank_with(red),
        Cell::blank(),
        Cell::blank(),
    ];
    assert_eq!(content_len(&row), 3);
    assert_eq!(content_len(&[Cell::blank(), Cell::blank()]), 0);
}

#[test]
fn locate_offset_walks_soft_rows_and_parks_in_final_padding() {
    let soft = |text: &str| {
        (
            text.chars()
                .map(|c| Cell::new(c, 1, Style::default()))
                .collect::<Vec<_>>(),
            RowMeta {
                end: RowEnd::Soft,
                prompt: false,
            },
        )
    };
    let hard = |text: &str| {
        (
            text.chars()
                .map(|c| Cell::new(c, 1, Style::default()))
                .collect::<Vec<_>>(),
            RowMeta {
                end: RowEnd::Hard,
                prompt: false,
            },
        )
    };
    let rows = vec![soft("abcd"), soft("efgh"), hard("ij")];
    assert_eq!(locate_offset(&rows, 0), (0, 0));
    assert_eq!(locate_offset(&rows, 3), (0, 3));
    assert_eq!(locate_offset(&rows, 4), (1, 0));
    assert_eq!(locate_offset(&rows, 9), (2, 1));
    // Past the content: parks in the final row's padding.
    assert_eq!(locate_offset(&rows, 11), (2, 3));
}

/// Every logical line visible anywhere (history then screen), soft wraps
/// collapsed — the reflow invariant is that this list never changes across
/// resizes, only how it is cut into rows.
fn logical_lines(engine: &TerminalEngine) -> Vec<String> {
    let state = engine.state();
    let mut physical: Vec<(Vec<Cell>, RowMeta)> =
        state.scrollback().lines().iter().cloned().collect();
    let grid = state.active_grid();
    let (rows, _) = grid.dimensions();
    for row in 0..rows {
        physical.push((
            grid.rows()[row as usize].clone(),
            RowMeta {
                end: grid.row_end(row),
                prompt: grid.prompt_mark(row),
            },
        ));
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for (cells, meta) in physical {
        let text: String = cells
            .iter()
            .filter(|cell| cell.width() != 0)
            .map(Cell::ch)
            .collect();
        match meta.end {
            RowEnd::Soft => current.push_str(&text),
            RowEnd::SoftWide => {
                let trimmed = text.strip_suffix(' ').unwrap_or(&text);
                current.push_str(trimmed);
            }
            RowEnd::Hard => {
                current.push_str(text.trim_end());
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[test]
fn mixed_content_survives_a_resize_chain_losslessly() {
    let mut e = engine(10, 6);
    feed(&mut e, "hello world\r\nab \u{6f22}\u{5b57} cd\r\nx\r\ntail");
    let original = logical_lines(&e);
    assert_eq!(
        original,
        vec!["hello world", "ab \u{6f22}\u{5b57} cd", "x", "tail"]
    );

    for (cols, rows) in [(5, 6), (3, 4), (7, 3), (12, 6), (10, 6)] {
        resize(&mut e, cols, rows);
        assert_eq!(
            logical_lines(&e),
            original,
            "content changed at {cols}x{rows}"
        );
    }
}

#[test]
fn colored_text_keeps_its_style_across_reflow() {
    let mut e = engine(4, 2);
    feed(&mut e, "\x1b[31mabcdef"); // red text soft-wraps at 4
    resize(&mut e, 8, 2);

    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    let grid = e.state().active_grid();
    for col in 0..6 {
        assert_eq!(grid.cell(0, col).unwrap().style(), red, "column {col}");
    }
    assert_eq!(row_text(&e, 0), "abcdef");
}

#[test]
fn autowrap_off_never_records_soft_ends() {
    let mut e = engine(8, 2);
    feed(&mut e, "\x1b[?7l"); // DECAWM off: glyphs overwrite the last column
    feed(&mut e, "abcdefghij");
    assert_eq!(row_text(&e, 0), "abcdefgj");
    assert_eq!(row_end(&e, 0), RowEnd::Hard);

    // A shrink still wraps the too-long hard line (nothing is cropped), and
    // widening re-joins it — the round trip is lossless.
    resize(&mut e, 4, 2);
    assert_eq!(row_text(&e, 0), "abcd");
    assert_eq!(row_text(&e, 1), "efgj");
    resize(&mut e, 8, 2);
    assert_eq!(row_text(&e, 0), "abcdefgj");
    assert_eq!(row_end(&e, 0), RowEnd::Hard);
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
    let mut engine = engine(10, 3);
    feed(&mut engine, "abcdefghijklmnopqrstuvwxyz0123456789");

    let state = engine.state();
    let history: Vec<RowEnd> = state
        .scrollback()
        .lines()
        .iter()
        .map(|(_, meta)| meta.end)
        .collect();
    assert_eq!(history, vec![RowEnd::Soft]);

    let grid = state.active_grid();
    assert_eq!(grid.row_end(0), RowEnd::Soft);
    assert_eq!(grid.row_end(1), RowEnd::Soft);
    assert_eq!(grid.row_end(2), RowEnd::Hard);
}

#[test]
fn a_linefeed_scroll_still_ends_its_row_hard() {
    // The counterpart: content scrolled off by an explicit newline genuinely
    // ends, so its history row must be Hard. Guards against "fix" the wrap case
    // by making every boundary Soft.
    let mut engine = engine(10, 3);
    feed(&mut engine, "one\r\ntwo\r\nthree\r\nfour");

    let state = engine.state();
    let history: Vec<RowEnd> = state
        .scrollback()
        .lines()
        .iter()
        .map(|(_, meta)| meta.end)
        .collect();
    assert_eq!(history, vec![RowEnd::Hard]);
}

/// One default-styled narrow cell per char of `text`.
fn cells(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|c| Cell::new(c, 1, Style::default()))
        .collect()
}

/// A wide glyph as stored in the grid: the width-2 base and its width-0
/// continuation cell.
fn wide(ch: char) -> [Cell; 2] {
    [
        Cell::new(ch, 2, Style::default()),
        Cell::new(' ', 0, Style::default()),
    ]
}

/// The prompt mark of every screen row, top to bottom.
fn prompt_marks(state: &TerminalState) -> Vec<bool> {
    let (rows, _) = state.primary.dimensions();
    (0..rows)
        .map(|row| state.primary.prompt_mark(row))
        .collect()
}

#[test]
fn rewrap_line_at_zero_columns_wraps_at_one_column() {
    let content = cells("abc");
    let rows = rewrap_line(&content, 0, Style::default());
    let soft = RowMeta {
        end: RowEnd::Soft,
        prompt: false,
    };
    assert_eq!(
        rows,
        vec![
            (cells("a"), soft),
            (cells("b"), soft),
            (cells("c"), RowMeta::default()),
        ]
    );
}

#[test]
fn rewrap_line_leaves_a_spacer_in_the_fill_before_a_wide_glyph_at_the_last_column() {
    let mut red = Style::default();
    red.set_bg(Color::Indexed(1));
    let mut content = cells("abc");
    content.extend(wide('\u{6f22}'));

    let rows = rewrap_line(&content, 4, red);

    let mut first = cells("abc");
    first.push(Cell::blank_with(red));
    assert_eq!(
        rows,
        vec![
            (
                first,
                RowMeta {
                    end: RowEnd::SoftWide,
                    prompt: false,
                }
            ),
            (wide('\u{6f22}').to_vec(), RowMeta::default()),
        ]
    );
}

#[test]
fn rewrap_line_at_one_column_stores_a_wide_glyph_narrow_and_skips_its_continuation() {
    let mut content = wide('\u{6f22}').to_vec();
    content.extend(wide('\u{5b57}'));

    let rows = rewrap_line(&content, 1, Style::default());

    assert_eq!(
        rows,
        vec![
            (
                vec![Cell::new('\u{6f22}', 1, Style::default())],
                RowMeta {
                    end: RowEnd::Soft,
                    prompt: false,
                }
            ),
            (
                vec![Cell::new('\u{5b57}', 1, Style::default())],
                RowMeta::default()
            ),
        ]
    );
}

#[test]
fn locate_offset_skips_a_soft_wide_spacer() {
    let mut first = cells("abc");
    first.push(Cell::blank());
    let mut second = wide('\u{6f22}').to_vec();
    second.extend(cells("c"));
    let rows = vec![
        (
            first,
            RowMeta {
                end: RowEnd::SoftWide,
                prompt: false,
            },
        ),
        (second, RowMeta::default()),
    ];
    // Offsets 0-2 are `abc`; the spacer holds none, so offset 3 is the wide
    // glyph at the start of the next row and offset 5 is the `c` after it.
    assert_eq!(locate_offset(&rows, 2), (0, 2));
    assert_eq!(locate_offset(&rows, 3), (1, 0));
    assert_eq!(locate_offset(&rows, 5), (1, 2));
}

#[test]
fn prompt_mark_follows_its_row_when_the_line_above_wraps() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 4 });
    let mut parser = vte::Parser::new();
    parser.advance(&mut state, b"abcdefg\r\n\x1b]133;A\x07$ ");
    assert_eq!(prompt_marks(&state), vec![false, true, false, false]);

    // `abcdefg` needs two rows at width 4, pushing the prompt row down one.
    state.resize(PtySize { cols: 4, rows: 4 });
    assert_eq!(prompt_marks(&state), vec![false, false, true, false]);
    assert_eq!(state.primary.rows()[2][0].ch(), '$');

    state.resize(PtySize { cols: 8, rows: 4 });
    assert_eq!(prompt_marks(&state), vec![false, true, false, false]);
}

#[test]
fn a_prompt_mark_on_a_wrapped_line_stays_on_its_first_row() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 4 });
    let mut parser = vte::Parser::new();
    parser.advance(&mut state, b"\x1b]133;A\x07abcdefgh");
    assert_eq!(prompt_marks(&state), vec![true, false, false, false]);

    state.resize(PtySize { cols: 2, rows: 4 });
    assert_eq!(prompt_marks(&state), vec![true, false, false, false]);

    state.resize(PtySize { cols: 8, rows: 4 });
    assert_eq!(prompt_marks(&state), vec![true, false, false, false]);
}

#[test]
fn a_prompt_mark_on_a_continuation_row_moves_to_the_lines_first_row() {
    let mut state = TerminalState::new(PtySize { cols: 8, rows: 4 });
    let mut parser = vte::Parser::new();
    parser.advance(&mut state, b"abcdefghij");
    // Row 1 holds `ij`, the continuation of row 0's line.
    state.active_grid_mut().set_prompt_mark(1, true);

    // The whole line fits one row: the mark lands on that row.
    state.resize(PtySize { cols: 12, rows: 4 });
    assert_eq!(prompt_marks(&state), vec![true, false, false, false]);

    // Wrapping again keeps the mark on the line's first row.
    state.resize(PtySize { cols: 8, rows: 4 });
    assert_eq!(prompt_marks(&state), vec![true, false, false, false]);
}

#[test]
fn cursor_parked_past_the_text_keeps_its_column_when_the_width_holds_it() {
    let mut e = engine(8, 2);
    feed(&mut e, "ab\x1b[1;7H"); // cursor to column 6, four cells past `ab`
    assert_eq!(cursor(&e), (0, 6));

    resize(&mut e, 12, 2);
    assert_eq!(cursor(&e), (0, 6));

    // Width 4 cannot hold column 6: the cursor clamps to the last column.
    resize(&mut e, 4, 2);
    assert_eq!(cursor(&e), (0, 3));

    // The clamp is what the next reflow starts from.
    resize(&mut e, 12, 2);
    assert_eq!(cursor(&e), (0, 3));
}

#[test]
fn cursor_on_a_soft_wide_spacer_lands_on_the_wide_glyph_after_widening() {
    let mut e = engine(4, 3);
    feed(&mut e, "abc\u{6f22}\x1b[1;4H"); // cursor onto row 0's spacer
    assert_eq!(cursor(&e), (0, 3));
    assert_eq!(row_end(&e, 0), RowEnd::SoftWide);

    resize(&mut e, 8, 3);
    assert_eq!(cursor(&e), (0, 3));
    assert_eq!(e.state().active_grid().cell(0, 3).unwrap().ch(), '\u{6f22}');
}

#[test]
fn resizing_to_the_same_size_changes_nothing() {
    let mut e = engine(8, 4);
    feed(&mut e, "abcdefghij\r\nxy");
    let before = (
        logical_lines(&e),
        (0..4).map(|row| row_end(&e, row)).collect::<Vec<_>>(),
        cursor(&e),
    );
    assert_eq!(
        before.1,
        vec![RowEnd::Soft, RowEnd::Hard, RowEnd::Hard, RowEnd::Hard]
    );
    assert_eq!(before.2, (2, 2));

    resize(&mut e, 8, 4);
    assert_eq!(
        (
            logical_lines(&e),
            (0..4).map(|row| row_end(&e, row)).collect::<Vec<_>>(),
            cursor(&e),
        ),
        before
    );
    assert_eq!(e.state().scrollback().len(), 0);
}

#[test]
fn a_trailing_soft_history_row_becomes_a_hard_line_on_regrow() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 0 });
    state.scrollback.push_row_with_meta(
        &cells("ab"),
        RowMeta {
            end: RowEnd::Soft,
            prompt: false,
        },
    );

    state.resize(PtySize { cols: 4, rows: 2 });

    let mut expected = cells("ab");
    expected.extend([Cell::blank(), Cell::blank()]);
    assert_eq!(state.primary.rows()[0], expected);
    assert_eq!(state.primary.row_end(0), RowEnd::Hard);
    assert_eq!(state.scrollback.len(), 0);
    assert_eq!((state.primary_cursor.row, state.primary_cursor.col), (0, 0));
}

#[test]
fn a_prompt_mark_on_an_empty_trailing_soft_row_survives_regrow() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 0 });
    state.scrollback.push_row_with_meta(
        &[],
        RowMeta {
            end: RowEnd::Soft,
            prompt: true,
        },
    );

    state.resize(PtySize { cols: 4, rows: 2 });

    assert_eq!(state.primary.rows()[0], vec![Cell::blank(); 4]);
    assert_eq!(state.primary.row_end(0), RowEnd::Hard);
    assert_eq!(prompt_marks(&state), vec![true, false]);
    assert_eq!(state.scrollback.len(), 0);
}

#[test]
fn height_shrink_with_the_cursor_above_scrolled_content_clamps_it_to_the_top_row() {
    let mut e = engine(4, 3);
    feed(&mut e, "a\r\nb\r\nc\x1b[H");
    assert_eq!(cursor(&e), (0, 0));

    // Rows `a` and `b` are content, so they scroll into history rather than
    // dropping; the cursor's own row goes with them and the cursor clamps to
    // the top row, which now holds `c`.
    resize(&mut e, 4, 1);
    assert_eq!(history_text(&e), vec!["a", "b"]);
    assert_eq!(row_text(&e, 0), "c");
    assert_eq!(cursor(&e), (0, 0));
}

#[test]
fn a_blank_prompt_marked_row_below_the_cursor_enters_history_on_a_height_shrink() {
    // A prompt mark is content: the row it sits on is not trailing padding,
    // however few cells it holds.
    let mut engine = engine(6, 4);
    feed(&mut engine, "a\r\n\x1b]133;A\x07\x1b[H");
    assert!(engine.state().active_grid().prompt_mark(1));
    assert_eq!(cursor(&engine), (0, 0));

    resize(&mut engine, 6, 1);

    // The marked row is kept, so the row above it is the one that overflows
    // into history and the mark stays on the one screen row.
    let history: Vec<bool> = engine
        .state()
        .scrollback()
        .lines()
        .iter()
        .map(|(_, meta)| meta.prompt)
        .collect();
    assert_eq!(history, vec![false]);
    assert!(engine.state().active_grid().prompt_mark(0));
}
