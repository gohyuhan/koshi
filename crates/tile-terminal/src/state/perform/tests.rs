//! Unit tests for the VTE performer: printing, deferred wrap, scrolling, and
//! the C0 control bytes.

use super::*;
use tile_core::process::PtySize;
use vte::Perform;

/// Build per-pane state of `cols × rows`.
fn state(cols: u16, rows: u16) -> TerminalState {
    TerminalState::new(PtySize { cols, rows })
}

/// Print every char of `s` through the performer.
fn print_str(state: &mut TerminalState, s: &str) {
    for c in s.chars() {
        state.print(c);
    }
}

/// The character at `(row, col)` of the active grid.
fn glyph(state: &TerminalState, row: u16, col: u16) -> Option<char> {
    state.active_grid().cell(row, col).map(Cell::ch)
}

#[test]
fn print_writes_the_glyph_at_the_cursor_and_advances() {
    let mut state = state(5, 3);
    state.print('a');
    assert_eq!(glyph(&state, 0, 0), Some('a'));
    assert_eq!((state.cursor.row, state.cursor.col), (0, 1));
    assert!(!state.cursor.pending_wrap);
}

#[test]
fn print_lays_a_string_left_to_right() {
    let mut state = state(5, 3);
    print_str(&mut state, "hi");
    assert_eq!(glyph(&state, 0, 0), Some('h'));
    assert_eq!(glyph(&state, 0, 1), Some('i'));
    assert_eq!(state.cursor.col, 2);
}

#[test]
fn print_stamps_the_pen_style_with_width_one() {
    let mut state = state(5, 3);
    state.print('a');
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.width(), 1);
    assert_eq!(cell.style(), state.style);
}

#[test]
fn print_at_the_last_column_parks_without_moving() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // fills row 0 exactly
    assert_eq!(glyph(&state, 0, 2), Some('c'));
    assert_eq!((state.cursor.row, state.cursor.col), (0, 2)); // cursor stays
    assert!(state.cursor.pending_wrap);
}

#[test]
fn exact_width_line_does_not_scroll_until_the_next_glyph() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // row 0 full, parked
    state.print('d'); // forces the deferred wrap
    assert_eq!(glyph(&state, 0, 0), Some('a')); // row 0 untouched, no early scroll
    assert_eq!(glyph(&state, 1, 0), Some('d')); // wrapped onto row 1
    assert_eq!((state.cursor.row, state.cursor.col), (1, 1));
    assert!(!state.cursor.pending_wrap);
}

#[test]
fn deferred_wrap_on_the_bottom_row_scrolls() {
    let mut state = state(2, 2);
    print_str(&mut state, "abcde"); // a,b | c,d then 'e' wraps off the bottom
    assert_eq!(glyph(&state, 0, 0), Some('c')); // old bottom row rose
    assert_eq!(glyph(&state, 0, 1), Some('d'));
    assert_eq!(glyph(&state, 1, 0), Some('e')); // 'e' on the fresh bottom row
    assert_eq!(glyph(&state, 1, 1), Some(' '));
    assert_eq!(state.cursor.row, 1);
}

#[test]
fn newline_moves_down_and_leaves_the_column() {
    let mut state = state(5, 3);
    state.print('a'); // cursor at col 1
    state.execute(b'\n');
    assert_eq!((state.cursor.row, state.cursor.col), (1, 1));
    assert!(!state.cursor.pending_wrap);
}

#[test]
fn vertical_tab_and_form_feed_behave_like_newline() {
    for byte in [0x0Bu8, 0x0C] {
        let mut state = state(5, 3);
        state.execute(byte);
        assert_eq!(state.cursor.row, 1, "byte {byte:#x} should line-feed");
    }
}

#[test]
fn newline_on_the_bottom_row_scrolls() {
    let mut state = state(3, 2);
    state.print('a'); // (0,0)
    state.execute(b'\n'); // to row 1
    state.execute(b'\r'); // column 0
    state.print('z'); // (1,0)
    state.execute(b'\n'); // bottom row -> scroll
    assert_eq!(glyph(&state, 0, 0), Some('z')); // row 1 rose to row 0
    assert_eq!(glyph(&state, 1, 0), Some(' ')); // fresh blank bottom
    assert_eq!(state.cursor.row, 1); // cursor pinned to the last row
}

#[test]
fn carriage_return_returns_to_column_zero() {
    let mut state = state(5, 3);
    print_str(&mut state, "ab");
    state.execute(b'\r');
    assert_eq!(state.cursor.col, 0);
}

#[test]
fn backspace_steps_back_one_column_and_floors_at_zero() {
    let mut state = state(5, 3);
    print_str(&mut state, "ab"); // col 2
    state.execute(0x08);
    assert_eq!(state.cursor.col, 1);
    state.execute(0x08);
    state.execute(0x08); // already at 0, saturates
    assert_eq!(state.cursor.col, 0);
}

#[test]
fn tab_advances_to_each_eight_column_stop() {
    let mut state = state(20, 1);
    state.execute(b'\t');
    assert_eq!(state.cursor.col, 8);
    state.execute(b'\t');
    assert_eq!(state.cursor.col, 16);
}

#[test]
fn tab_from_mid_stop_lands_on_the_next_stop() {
    let mut state = state(20, 1);
    print_str(&mut state, "abc"); // col 3
    state.execute(b'\t');
    assert_eq!(state.cursor.col, 8);
}

#[test]
fn tab_clamps_to_the_last_column() {
    let mut state = state(6, 1); // last column is 5
    state.execute(b'\t');
    assert_eq!(state.cursor.col, 5);
}

#[test]
fn bell_is_ignored() {
    let mut state = state(5, 3);
    print_str(&mut state, "a"); // col 1
    state.execute(0x07);
    assert_eq!((state.cursor.row, state.cursor.col), (0, 1));
    assert_eq!(glyph(&state, 0, 0), Some('a'));
}

#[test]
fn unknown_control_byte_is_ignored() {
    let mut state = state(5, 3);
    print_str(&mut state, "a");
    state.execute(0x01); // SOH — unhandled
    assert_eq!((state.cursor.row, state.cursor.col), (0, 1));
    assert_eq!(glyph(&state, 0, 0), Some('a'));
}

#[test]
fn a_cursor_move_clears_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // parked on the last column
    assert!(state.cursor.pending_wrap);
    state.execute(b'\r'); // any cursor move clears the latch
    assert!(!state.cursor.pending_wrap);
    state.print('c'); // must overwrite in place, not wrap to a new line
    assert_eq!(glyph(&state, 0, 0), Some('c'));
    assert_eq!((state.cursor.row, state.cursor.col), (0, 1));
}

#[test]
fn driven_through_the_parser_plain_text_lands_in_the_grid() {
    let mut state = state(10, 2);
    let mut parser = vte::Parser::new();
    parser.advance(&mut state, b"h\xc3\xa9llo"); // "héllo" — é is multi-byte UTF-8
    assert_eq!(glyph(&state, 0, 0), Some('h'));
    assert_eq!(glyph(&state, 0, 1), Some('é'));
    assert_eq!(glyph(&state, 0, 2), Some('l'));
    assert_eq!(glyph(&state, 0, 4), Some('o'));
    assert_eq!(state.cursor.col, 5);
}

#[test]
fn driven_through_the_parser_newline_and_carriage_return() {
    let mut state = state(10, 3);
    let mut parser = vte::Parser::new();
    parser.advance(&mut state, b"ab\r\ncd");
    assert_eq!(glyph(&state, 0, 0), Some('a'));
    assert_eq!(glyph(&state, 0, 1), Some('b'));
    assert_eq!(glyph(&state, 1, 0), Some('c'));
    assert_eq!(glyph(&state, 1, 1), Some('d'));
    assert_eq!((state.cursor.row, state.cursor.col), (1, 2));
}
