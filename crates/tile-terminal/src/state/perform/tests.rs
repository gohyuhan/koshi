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

// --- CSI cursor + erase (driven through the parser, the only way to build
// `vte::Params`) ---

/// Feed `bytes` through a fresh parser into `state`.
fn advance(state: &mut TerminalState, bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    parser.advance(state, bytes);
}

/// Row `row` of the active grid as a string; blank cells read as spaces.
fn row_text(state: &TerminalState, row: u16) -> String {
    let (_, cols) = state.active_grid().dimensions();
    (0..cols)
        .map(|c| glyph(state, row, c).unwrap_or(' '))
        .collect()
}

/// Fill a 3×3 grid with rows `"abc"`, `"def"`, `"ghi"`.
fn fill_3x3(state: &mut TerminalState) {
    advance(state, b"abc\r\ndef\r\nghi");
}

#[test]
fn cup_sets_an_absolute_one_based_position() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[2;3H");
    assert_eq!((state.cursor.row, state.cursor.col), (1, 2)); // 2;3 -> 0-based
}

#[test]
fn cup_with_no_arguments_homes_the_cursor() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[4;4H"); // move away first
    advance(&mut state, b"\x1b[H");
    assert_eq!((state.cursor.row, state.cursor.col), (0, 0));
}

#[test]
fn cup_zero_arguments_are_treated_as_one() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[0;0H");
    assert_eq!((state.cursor.row, state.cursor.col), (0, 0));
}

#[test]
fn cup_clamps_out_of_range_arguments_to_the_grid_edges() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99;99H");
    assert_eq!((state.cursor.row, state.cursor.col), (4, 9)); // last row, last col
}

#[test]
fn hvp_positions_like_cup() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[2;4f");
    assert_eq!((state.cursor.row, state.cursor.col), (1, 3));
}

#[test]
fn cuu_moves_up_by_the_count() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[4;4H"); // (3, 3)
    advance(&mut state, b"\x1b[2A");
    assert_eq!(state.cursor.row, 1);
}

#[test]
fn cud_moves_down_and_clamps_to_the_last_row() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99B");
    assert_eq!(state.cursor.row, 4);
}

#[test]
fn cuf_moves_forward_and_clamps_to_the_last_column() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99C");
    assert_eq!(state.cursor.col, 9);
}

#[test]
fn cub_moves_back_and_floors_at_column_zero() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[1;4H"); // col 3
    advance(&mut state, b"\x1b[5D");
    assert_eq!(state.cursor.col, 0);
}

#[test]
fn a_missing_or_zero_move_count_defaults_to_one() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;3H"); // (2, 2)
    advance(&mut state, b"\x1b[A"); // no argument -> up one
    assert_eq!(state.cursor.row, 1);
    advance(&mut state, b"\x1b[0A"); // explicit zero -> up one
    assert_eq!(state.cursor.row, 0);
}

#[test]
fn a_csi_cursor_move_clears_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // parked on the last column
    assert!(state.cursor.pending_wrap);
    advance(&mut state, b"\x1b[C"); // CUF clears the latch
    assert!(!state.cursor.pending_wrap);
}

#[test]
fn a_private_mode_sequence_is_ignored_not_treated_as_erase() {
    let mut state = state(5, 2);
    print_str(&mut state, "abcde"); // fills row 0
    advance(&mut state, b"\x1b[?2J"); // `?` -> private mode, not ED 2
    assert_eq!(row_text(&state, 0), "abcde"); // untouched
}

#[test]
fn el_0_erases_from_the_cursor_to_the_end_of_the_line() {
    let mut state = state(5, 2);
    print_str(&mut state, "abcde");
    advance(&mut state, b"\x1b[1;3H"); // row 0, col 2
    advance(&mut state, b"\x1b[K"); // EL 0
    assert_eq!(row_text(&state, 0), "ab   ");
}

#[test]
fn el_1_erases_from_the_start_through_the_cursor() {
    let mut state = state(5, 2);
    print_str(&mut state, "abcde");
    advance(&mut state, b"\x1b[1;3H"); // col 2
    advance(&mut state, b"\x1b[1K"); // EL 1 — cursor column inclusive
    assert_eq!(row_text(&state, 0), "   de");
}

#[test]
fn el_2_erases_the_whole_line() {
    let mut state = state(5, 2);
    print_str(&mut state, "abcde");
    advance(&mut state, b"\x1b[2K");
    assert_eq!(row_text(&state, 0), "     ");
}

#[test]
fn ed_0_erases_from_the_cursor_to_the_end_of_the_screen() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[2;2H"); // (1, 1)
    advance(&mut state, b"\x1b[J"); // ED 0
    assert_eq!(row_text(&state, 0), "abc"); // above kept
    assert_eq!(row_text(&state, 1), "d  "); // cursor column onward cleared
    assert_eq!(row_text(&state, 2), "   "); // row below cleared
}

#[test]
fn ed_1_erases_from_the_start_of_the_screen_through_the_cursor() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[2;2H"); // (1, 1)
    advance(&mut state, b"\x1b[1J"); // ED 1
    assert_eq!(row_text(&state, 0), "   "); // row above cleared
    assert_eq!(row_text(&state, 1), "  f"); // start through cursor cleared
    assert_eq!(row_text(&state, 2), "ghi"); // below kept
}

#[test]
fn ed_2_erases_the_whole_screen() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[2J");
    assert_eq!(row_text(&state, 0), "   ");
    assert_eq!(row_text(&state, 1), "   ");
    assert_eq!(row_text(&state, 2), "   ");
}

#[test]
fn ed_3_leaves_the_visible_screen_untouched() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[3J"); // erase scrollback only (stub) — screen intact
    assert_eq!(row_text(&state, 0), "abc");
    assert_eq!(row_text(&state, 1), "def");
    assert_eq!(row_text(&state, 2), "ghi");
}

// --- SGR: set graphic rendition (pen colors + text attributes) ---

/// The default pen with the setters in `f` applied — the expected pen for an
/// SGR assertion, built the same way the performer mutates `self.style`.
fn styled(f: impl FnOnce(&mut Style)) -> Style {
    let mut style = Style::default();
    f(&mut style);
    style
}

#[test]
fn sgr_bold_sets_the_bold_attribute() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1m");
    assert_eq!(state.style, styled(|s| s.set_bold(true)));
}

#[test]
fn sgr_zero_resets_the_pen() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;31m"); // bold + red
    advance(&mut state, b"\x1b[0m");
    assert_eq!(state.style, Style::default());
}

#[test]
fn sgr_empty_params_reset_like_zero() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1m");
    advance(&mut state, b"\x1b[m"); // bare CSI m is an implicit reset
    assert_eq!(state.style, Style::default());
}

#[test]
fn sgr_attribute_off_codes_clear_each_attribute() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;3;4;7m"); // bold, italic, underline, reverse on
    advance(&mut state, b"\x1b[22;23;24;27m"); // each turned back off
    assert_eq!(state.style, Style::default());
}

#[test]
fn sgr_sixteen_color_foreground_and_background() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[31;42m"); // fg red (1), bg green (2)
    assert_eq!(
        state.style,
        styled(|s| {
            s.set_fg(Color::Indexed(1));
            s.set_bg(Color::Indexed(2));
        })
    );
}

#[test]
fn sgr_bright_colors_map_to_indices_eight_through_fifteen() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[91;102m"); // bright red fg (8+1), bright green bg (8+2)
    assert_eq!(
        state.style,
        styled(|s| {
            s.set_fg(Color::Indexed(9));
            s.set_bg(Color::Indexed(10));
        })
    );
}

#[test]
fn sgr_default_color_codes_restore_the_default() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[31;42m");
    advance(&mut state, b"\x1b[39;49m"); // default fg + bg
    assert_eq!(state.style, Style::default());
}

#[test]
fn sgr_256_color_foreground() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;5;196m");
    assert_eq!(state.style, styled(|s| s.set_fg(Color::Indexed(196))));
}

#[test]
fn sgr_256_color_background() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[48;5;21m");
    assert_eq!(state.style, styled(|s| s.set_bg(Color::Indexed(21))));
}

#[test]
fn sgr_truecolor_foreground_semicolon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;2;255;128;0m");
    assert_eq!(state.style, styled(|s| s.set_fg(Color::Rgb(255, 128, 0))));
}

#[test]
fn sgr_truecolor_background_semicolon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[48;2;10;20;30m");
    assert_eq!(state.style, styled(|s| s.set_bg(Color::Rgb(10, 20, 30))));
}

#[test]
fn sgr_256_color_colon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:5:196m");
    assert_eq!(state.style, styled(|s| s.set_fg(Color::Indexed(196))));
}

#[test]
fn sgr_truecolor_colon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:2:255:128:0m");
    assert_eq!(state.style, styled(|s| s.set_fg(Color::Rgb(255, 128, 0))));
}

#[test]
fn sgr_truecolor_colon_form_with_empty_colorspace_id() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:2::255:128:0m"); // ITU form: empty colorspace slot
    assert_eq!(state.style, styled(|s| s.set_fg(Color::Rgb(255, 128, 0))));
}

#[test]
fn sgr_combines_multiple_codes_in_one_sequence() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;4;38;5;200;48;2;1;2;3m");
    assert_eq!(
        state.style,
        styled(|s| {
            s.set_bold(true);
            s.set_underline(true);
            s.set_fg(Color::Indexed(200));
            s.set_bg(Color::Rgb(1, 2, 3));
        })
    );
}

#[test]
fn sgr_pen_is_stamped_onto_subsequently_printed_glyphs() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;31m");
    state.print('x');
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(
        cell.style(),
        styled(|s| {
            s.set_bold(true);
            s.set_fg(Color::Indexed(1));
        })
    );
}

#[test]
fn sgr_unknown_code_is_ignored() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1m"); // bold on
    advance(&mut state, b"\x1b[99m"); // unknown SGR code -> pen unchanged
    assert_eq!(state.style, styled(|s| s.set_bold(true)));
}

#[test]
fn sgr_incomplete_extended_color_leaves_the_pen_unchanged() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;5m"); // 256-color selector with no index
    assert_eq!(state.style, Style::default());
}

#[test]
fn sgr_incomplete_colon_extended_color_leaves_the_pen_unchanged() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:5m"); // colon 256-color selector with no index
    assert_eq!(state.style, Style::default());
}

#[test]
fn sgr_does_not_move_the_cursor() {
    let mut state = state(5, 2);
    print_str(&mut state, "ab"); // col 2
    advance(&mut state, b"\x1b[1;31m");
    assert_eq!((state.cursor.row, state.cursor.col), (0, 2));
}

#[test]
fn sgr_preserves_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // parked on the last column
    assert!(state.cursor.pending_wrap);
    advance(&mut state, b"\x1b[1m"); // SGR is not a cursor move
    assert!(state.cursor.pending_wrap); // latch survives, unlike a cursor move
}

// --- BCE: erase / scroll fill with the current background (not default) ---

#[test]
fn el_erases_the_line_to_the_current_background() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[44m"); // bg = blue (Indexed 4)
    advance(&mut state, b"\x1b[K"); // EL 0 from col 0 -> whole row
    let fill = styled(|s| s.set_bg(Color::Indexed(4)));
    assert!((0..5).all(|c| state.active_grid().cell(0, c).map(Cell::style) == Some(fill)));
}

#[test]
fn ed_2_erases_the_screen_to_the_current_background() {
    let mut state = state(3, 2);
    advance(&mut state, b"\x1b[42m"); // bg = green (Indexed 2)
    advance(&mut state, b"\x1b[2J"); // ED 2 — whole screen
    let fill = styled(|s| s.set_bg(Color::Indexed(2)));
    for row in 0..2 {
        assert!((0..3).all(|c| state.active_grid().cell(row, c).map(Cell::style) == Some(fill)));
    }
}

#[test]
fn erase_uses_the_background_only_not_the_full_pen() {
    let mut state = state(3, 1);
    advance(&mut state, b"\x1b[1;31;44m"); // bold + fg red + bg blue
    advance(&mut state, b"\x1b[K"); // erase row 0
                                    // Erased cells carry ONLY the background; bold + foreground are dropped.
    let fill = styled(|s| s.set_bg(Color::Indexed(4)));
    assert!((0..3).all(|c| state.active_grid().cell(0, c).map(Cell::style) == Some(fill)));
    // The pen itself is unchanged by the erase.
    assert_eq!(
        state.style,
        styled(|s| {
            s.set_bold(true);
            s.set_fg(Color::Indexed(1));
            s.set_bg(Color::Indexed(4));
        })
    );
}

#[test]
fn scroll_fills_the_exposed_row_with_the_current_background() {
    let mut state = state(2, 2);
    advance(&mut state, b"\x1b[42m"); // bg = green
    advance(&mut state, b"\x1b[2;1H"); // move to the bottom row (row 1)
    state.execute(b'\n'); // line feed on the last row -> scroll
    let fill = styled(|s| s.set_bg(Color::Indexed(2)));
    // The freshly exposed bottom row carries the current background.
    assert!((0..2).all(|c| state.active_grid().cell(1, c).map(Cell::style) == Some(fill)));
}
