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

// --- save/restore, insert/delete, scroll regions ---

#[test]
fn decsc_decrc_restores_the_cursor_and_pen() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // cursor -> (2, 3)
    advance(&mut state, b"\x1b[1;31m"); // bold + fg red
    advance(&mut state, b"\x1b7"); // DECSC
    advance(&mut state, b"\x1b[1;1H"); // move home
    advance(&mut state, b"\x1b[0m"); // reset pen
    advance(&mut state, b"\x1b8"); // DECRC
    assert_eq!((state.cursor.row, state.cursor.col), (2, 3));
    assert_eq!(
        state.style,
        styled(|s| {
            s.set_bold(true);
            s.set_fg(Color::Indexed(1));
        })
    );
}

#[test]
fn decsc_decrc_preserves_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // fills row 0, parks at the last column
    assert!(state.cursor.pending_wrap);
    advance(&mut state, b"\x1b7"); // DECSC saves the latch
    advance(&mut state, b"\x1b[1;1H"); // a cursor move clears it
    assert!(!state.cursor.pending_wrap);
    advance(&mut state, b"\x1b8"); // DECRC restores the latch
    assert!(state.cursor.pending_wrap);
    state.print('c'); // the latch makes the next glyph wrap, not overwrite
    assert_eq!(glyph(&state, 0, 0), Some('a')); // row 0 untouched
    assert_eq!(glyph(&state, 1, 0), Some('c')); // wrapped onto row 1
}

#[test]
fn scosc_scorc_save_and_restore_the_cursor_and_pen() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[2;5H"); // (1, 4)
    advance(&mut state, b"\x1b[1;31m"); // bold + fg red
    let saved_style = state.style;
    advance(&mut state, b"\x1b[s"); // SCOSC
    advance(&mut state, b"\x1b[5;5H"); // move away
    advance(&mut state, b"\x1b[0m"); // reset the pen to a different style
    advance(&mut state, b"\x1b[u"); // SCORC
    assert_eq!((state.cursor.row, state.cursor.col), (1, 4));
    assert_eq!(state.style, saved_style); // pen restored too
}

#[test]
fn decrc_without_a_save_homes_and_resets_the_pen() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // move away
    advance(&mut state, b"\x1b[1;31m"); // dirty the pen
    advance(&mut state, b"\x1b8"); // DECRC with no prior DECSC
    assert_eq!((state.cursor.row, state.cursor.col), (0, 0));
    assert_eq!(state.style, Style::default());
}

#[test]
fn decrc_clamps_the_restored_cursor_into_a_shrunk_grid() {
    let mut state = state(10, 10);
    advance(&mut state, b"\x1b[6;9H"); // (5, 8)
    advance(&mut state, b"\x1b7"); // save
    state.resize(PtySize { cols: 3, rows: 3 });
    advance(&mut state, b"\x1b8"); // restore -> clamped to the new bounds
    assert_eq!((state.cursor.row, state.cursor.col), (2, 2));
}

#[test]
fn reverse_index_moves_the_cursor_up_one_line() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[3;1H"); // row 2
    advance(&mut state, b"\x1bM"); // RI
    assert_eq!(state.cursor.row, 1);
}

#[test]
fn reverse_index_at_the_top_scrolls_the_region_down() {
    let mut state = state(3, 3);
    fill_3x3(&mut state); // abc / def / ghi
    advance(&mut state, b"\x1b[1;1H"); // home — at the top margin
    advance(&mut state, b"\x1bM"); // RI scrolls down
    assert_eq!(row_text(&state, 0), "   "); // fresh blank top
    assert_eq!(row_text(&state, 1), "abc"); // pushed down
    assert_eq!(row_text(&state, 2), "def"); // ghi fell off the bottom
}

#[test]
fn decstbm_sets_the_region_and_homes_the_cursor() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[3;1H"); // move away from home first
    advance(&mut state, b"\x1b[2;4r"); // margins rows 2..4 (1-based) -> (1, 3)
    assert_eq!(state.scroll_region, Some((1, 3)));
    assert_eq!((state.cursor.row, state.cursor.col), (0, 0));
}

#[test]
fn decstbm_full_span_clears_the_region() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[2;4r"); // set a region
    advance(&mut state, b"\x1b[1;5r"); // whole screen -> None
    assert_eq!(state.scroll_region, None);
}

#[test]
fn decstbm_with_no_parameters_clears_the_region() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[2;4r"); // set a region
    advance(&mut state, b"\x1b[r"); // CSI r, defaults = whole screen -> None
    assert_eq!(state.scroll_region, None);
}

#[test]
fn decstbm_with_an_invalid_range_is_ignored() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[2;4r"); // valid region (1, 3)
    advance(&mut state, b"\x1b[4;2r"); // top not above bottom -> ignored
    assert_eq!(state.scroll_region, Some((1, 3)));
}

#[test]
fn line_feed_scrolls_only_within_the_region() {
    let mut state = state(3, 4); // 4 rows
    advance(&mut state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    advance(&mut state, b"\x1b[2;3r"); // region rows 2..3 -> (1, 2); homes cursor
    advance(&mut state, b"\x1b[3;1H"); // to the bottom margin (row 2)
    state.execute(b'\n'); // line feed at the bottom margin -> scroll region up
    assert_eq!(row_text(&state, 0), "AAA"); // above region, untouched
    assert_eq!(row_text(&state, 1), "CCC"); // old row 2 rose
    assert_eq!(row_text(&state, 2), "   "); // blank exposed at the region bottom
    assert_eq!(row_text(&state, 3), "DDD"); // below region, untouched
}

#[test]
fn ich_inserts_blank_cells_shifting_the_line_right() {
    let mut state = state(5, 1);
    advance(&mut state, b"abcde"); // fills row 0
    advance(&mut state, b"\x1b[1;3H"); // cursor -> (0, 2) on 'c'
    advance(&mut state, b"\x1b[2@"); // ICH 2
    assert_eq!(row_text(&state, 0), "ab  c"); // c shifts right; d, e fall off
    assert_eq!((state.cursor.row, state.cursor.col), (0, 2)); // cursor unchanged
    assert!(!state.cursor.pending_wrap);
}

#[test]
fn ich_fills_inserted_cells_with_the_current_background() {
    let mut state = state(5, 1);
    advance(&mut state, b"\x1b[42m"); // bg = green
    advance(&mut state, b"abcde");
    advance(&mut state, b"\x1b[1;3H"); // cursor -> (0, 2)
    advance(&mut state, b"\x1b[2@"); // ICH 2 — inserted blanks carry the bg
    let fill = styled(|s| s.set_bg(Color::Indexed(2)));
    assert_eq!(state.active_grid().cell(0, 2).map(Cell::style), Some(fill));
    assert_eq!(state.active_grid().cell(0, 3).map(Cell::style), Some(fill));
}

#[test]
fn dch_deletes_cells_pulling_the_line_left() {
    let mut state = state(5, 1);
    advance(&mut state, b"abcde");
    advance(&mut state, b"\x1b[1;2H"); // cursor -> (0, 1) on 'b'
    advance(&mut state, b"\x1b[2P"); // DCH 2
    assert_eq!(row_text(&state, 0), "ade  "); // b, c removed; padded right
    assert_eq!((state.cursor.row, state.cursor.col), (0, 1)); // cursor unchanged
    assert!(!state.cursor.pending_wrap);
}

#[test]
fn dch_fills_padded_cells_with_the_current_background() {
    let mut state = state(5, 1);
    advance(&mut state, b"\x1b[42m"); // bg = green
    advance(&mut state, b"abcde");
    advance(&mut state, b"\x1b[1;2H"); // cursor -> (0, 1)
    advance(&mut state, b"\x1b[2P"); // DCH 2 — the right-end pad carries the bg
    let fill = styled(|s| s.set_bg(Color::Indexed(2)));
    assert_eq!(state.active_grid().cell(0, 4).map(Cell::style), Some(fill));
}

#[test]
fn il_inserts_a_blank_line_and_keeps_the_cursor() {
    let mut state = state(3, 3);
    fill_3x3(&mut state); // abc / def / ghi
    advance(&mut state, b"\x1b[2;3H"); // cursor -> (1, 2)
    advance(&mut state, b"\x1b[L"); // IL 1
    assert_eq!(row_text(&state, 0), "abc"); // above, untouched
    assert_eq!(row_text(&state, 1), "   "); // blank inserted
    assert_eq!(row_text(&state, 2), "def"); // def pushed down; ghi fell off
    assert_eq!((state.cursor.row, state.cursor.col), (1, 2)); // cursor unchanged (column kept)
}

#[test]
fn dl_deletes_a_line_within_the_region() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[1;1H"); // cursor -> (0, 0)
    advance(&mut state, b"\x1b[M"); // DL 1
    assert_eq!(row_text(&state, 0), "def"); // def rose
    assert_eq!(row_text(&state, 1), "ghi");
    assert_eq!(row_text(&state, 2), "   "); // blank at the bottom
}

#[test]
fn il_outside_the_region_is_ignored() {
    let mut state = state(3, 4); // 4 rows
    advance(&mut state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    advance(&mut state, b"\x1b[2;3r"); // region rows 2..3 -> (1, 2); homes cursor
    advance(&mut state, b"\x1b[1;1H"); // cursor row 0 — above the region
    advance(&mut state, b"\x1b[L"); // IL ignored outside the region
    assert_eq!(row_text(&state, 0), "AAA");
    assert_eq!(row_text(&state, 1), "BBB");
    assert_eq!(row_text(&state, 2), "CCC");
    assert_eq!(row_text(&state, 3), "DDD");
}

#[test]
fn su_scrolls_the_region_up_leaving_the_cursor() {
    let mut state = state(3, 3);
    fill_3x3(&mut state); // abc / def / ghi
    advance(&mut state, b"\x1b[2;2H"); // cursor -> (1, 1)
    advance(&mut state, b"\x1b[S"); // SU 1
    assert_eq!(row_text(&state, 0), "def");
    assert_eq!(row_text(&state, 1), "ghi");
    assert_eq!(row_text(&state, 2), "   ");
    assert_eq!((state.cursor.row, state.cursor.col), (1, 1)); // cursor unmoved
}

#[test]
fn sd_scrolls_the_region_down_leaving_the_cursor() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[2;2H"); // cursor -> (1, 1)
    advance(&mut state, b"\x1b[T"); // SD 1
    assert_eq!(row_text(&state, 0), "   ");
    assert_eq!(row_text(&state, 1), "abc");
    assert_eq!(row_text(&state, 2), "def"); // ghi fell off the bottom
    assert_eq!((state.cursor.row, state.cursor.col), (1, 1)); // cursor unmoved
}

#[test]
fn sd_via_the_ecma48_caret_form_scrolls_the_region_down() {
    let mut state = state(3, 3);
    fill_3x3(&mut state); // abc / def / ghi
    advance(&mut state, b"\x1b[2;2H"); // cursor -> (1, 1)
    advance(&mut state, b"\x1b[^"); // CSI ^ = SD (ECMA-48 form)
    assert_eq!(row_text(&state, 0), "   ");
    assert_eq!(row_text(&state, 1), "abc");
    assert_eq!(row_text(&state, 2), "def"); // ghi fell off the bottom
    assert_eq!((state.cursor.row, state.cursor.col), (1, 1)); // cursor unmoved
}

#[test]
fn the_highlight_tracking_form_of_csi_t_does_not_scroll() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[1;2;3;4;5T"); // 5-param CSI T = highlight tracking, not SD
    assert_eq!(row_text(&state, 0), "abc"); // grid unchanged
    assert_eq!(row_text(&state, 1), "def");
    assert_eq!(row_text(&state, 2), "ghi");
}

// --- Alternate screen (`?47`/`?1047`/`?1048`/`?1049`), DECTCEM (`?25`), and
// OSC 0/1/2 title ---

#[test]
fn dec_47_swaps_to_the_alternate_buffer_and_back() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?47h");
    assert_eq!(state.active, Screen::Alternate);
    advance(&mut state, b"\x1b[?47l");
    assert_eq!(state.active, Screen::Primary);
}

#[test]
fn alternate_screen_output_leaves_the_primary_grid_untouched() {
    let mut state = state(5, 3);
    advance(&mut state, b"abc"); // primary row 0
    advance(&mut state, b"\x1b[?47h");
    advance(&mut state, b"ZZ"); // written to the alternate grid
    advance(&mut state, b"\x1b[?47l");
    assert_eq!(row_text(&state, 0), "abc  "); // primary unchanged
}

#[test]
fn dec_1049_saves_the_cursor_switches_and_restores() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
    advance(&mut state, b"\x1b[?1049h");
    assert_eq!(state.active, Screen::Alternate);
    let saved = state.saved[Screen::Primary as usize].expect("primary cursor saved");
    assert_eq!((saved.row, saved.col), (2, 3));
    advance(&mut state, b"\x1b[1;1H"); // move on the alternate screen
    advance(&mut state, b"\x1b[?1049l");
    assert_eq!(state.active, Screen::Primary);
    assert_eq!((state.cursor.row, state.cursor.col), (2, 3)); // restored
}

#[test]
fn dec_1049_clears_the_alternate_buffer_on_entry_using_the_background() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?47h"); // enter without clearing
    advance(&mut state, b"xyz"); // alternate row 0 = "xyz"
    advance(&mut state, b"\x1b[?47l"); // leave; the alternate keeps "xyz"
    advance(&mut state, b"\x1b[44m"); // pen bg = blue (Indexed 4)
    advance(&mut state, b"\x1b[?1049h"); // re-enter; clears with the current bg
    assert_eq!(state.active, Screen::Alternate);
    let fill = styled(|s| s.set_bg(Color::Indexed(4)));
    for row in 0..3 {
        assert!((0..5).all(|c| state.active_grid().cell(row, c).map(Cell::style) == Some(fill)));
    }
    assert_eq!(row_text(&state, 0), "     "); // blanked
}

#[test]
fn dec_1047_clears_the_alternate_buffer_on_exit_using_the_background() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1047h");
    advance(&mut state, b"xyz"); // alternate row 0 = "xyz"
    advance(&mut state, b"\x1b[44m"); // pen bg = blue before the clearing exit
    advance(&mut state, b"\x1b[?1047l"); // clears the alternate with the current bg, back to primary
    assert_eq!(state.active, Screen::Primary);
    advance(&mut state, b"\x1b[?1047h"); // re-enter (1047 does not clear on entry)
    let fill = styled(|s| s.set_bg(Color::Indexed(4)));
    for row in 0..3 {
        assert!((0..5).all(|c| state.active_grid().cell(row, c).map(Cell::style) == Some(fill)));
    }
    assert_eq!(row_text(&state, 0), "     "); // was cleared on the prior exit
}

#[test]
fn dec_1048_saves_and_restores_the_cursor_and_pen_without_swapping() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // (2, 3)
    advance(&mut state, b"\x1b[1;31m"); // bold + fg red
    advance(&mut state, b"\x1b[?1048h"); // save (no buffer swap)
    assert_eq!(state.active, Screen::Primary);
    advance(&mut state, b"\x1b[1;1H\x1b[0m"); // move home + reset pen
    advance(&mut state, b"\x1b[?1048l"); // restore
    assert_eq!((state.cursor.row, state.cursor.col), (2, 3)); // position restored
    assert_eq!(state.active, Screen::Primary);
    assert_eq!(
        state.style,
        styled(|s| {
            s.set_bold(true);
            s.set_fg(Color::Indexed(1));
        })
    ); // pen restored
}

#[test]
fn a_save_on_the_alternate_screen_does_not_clobber_the_primary_stash() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
    advance(&mut state, b"\x1b[?1049h"); // stash (2, 3) in the primary slot
    advance(&mut state, b"\x1b[2;2H\x1b7"); // on the alternate: move to (1, 1), DECSC
    advance(&mut state, b"\x1b[?1049l"); // back to primary, restore
    assert_eq!((state.cursor.row, state.cursor.col), (2, 3)); // primary stash intact
    let alt = state.saved[Screen::Alternate as usize].expect("alternate saved");
    assert_eq!((alt.row, alt.col), (1, 1)); // the alternate kept its own snapshot
}

#[test]
fn re_entering_the_alternate_screen_does_not_re_clear_it() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[?1049h"); // enter + clear
    advance(&mut state, b"\x1b[1;1Hhi"); // write "hi" at the top-left of the alternate
    advance(&mut state, b"\x1b[?1049h"); // already on the alternate: must not re-clear
    assert_eq!(glyph(&state, 0, 0), Some('h'));
    assert_eq!(glyph(&state, 0, 1), Some('i'));
}

#[test]
fn dectcem_toggles_cursor_visibility() {
    let mut state = state(5, 3);
    assert!(state.cursor_visible()); // visible by default
    advance(&mut state, b"\x1b[?25l");
    assert!(!state.cursor_visible());
    advance(&mut state, b"\x1b[?25h");
    assert!(state.cursor_visible());
}

#[test]
fn osc_2_sets_the_window_title() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]2;hello\x07");
    assert_eq!(state.title(), Some("hello"));
}

#[test]
fn osc_0_and_1_also_set_the_title() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]0;zero\x07");
    assert_eq!(state.title(), Some("zero"));
    advance(&mut state, b"\x1b]1;icon\x07");
    assert_eq!(state.title(), Some("icon"));
}

#[test]
fn osc_title_keeps_embedded_semicolons() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]2;a;b;c\x07");
    assert_eq!(state.title(), Some("a;b;c"));
}

#[test]
fn osc_title_accepts_a_string_terminator() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]2;via-st\x1b\\");
    assert_eq!(state.title(), Some("via-st"));
}

#[test]
fn the_title_is_none_until_an_osc_sets_it() {
    let state = state(5, 3);
    assert_eq!(state.title(), None);
}

#[test]
fn dec_47_reset_is_a_noop_when_already_on_primary() {
    let mut state = state(5, 3);
    advance(&mut state, b"abc"); // primary row 0
    advance(&mut state, b"\x1b[?47l"); // already on primary: must do nothing
    assert_eq!(state.active, Screen::Primary);
    assert_eq!(row_text(&state, 0), "abc  "); // primary untouched
}

#[test]
fn dec_1047_reset_when_already_on_primary_does_not_clear_it() {
    let mut state = state(5, 3);
    advance(&mut state, b"abc"); // primary row 0
    advance(&mut state, b"\x1b[?1047l"); // already on primary: the guard must stop the clear
    assert_eq!(state.active, Screen::Primary);
    assert_eq!(row_text(&state, 0), "abc  "); // primary not wiped
}

#[test]
fn dec_1049_preserves_primary_content_across_the_cycle() {
    let mut state = state(5, 3);
    advance(&mut state, b"abc"); // primary row 0
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate (clears the alternate, not the primary)
    advance(&mut state, b"\x1b[1;1HZZ"); // write on the alternate
    advance(&mut state, b"\x1b[?1049l"); // back to the primary
    assert_eq!(state.active, Screen::Primary);
    assert_eq!(row_text(&state, 0), "abc  "); // primary intact
}

#[test]
fn an_unknown_dec_private_mode_is_ignored() {
    let mut state = state(5, 3);
    advance(&mut state, b"abc");
    advance(&mut state, b"\x1b[?9999h"); // unknown DEC private mode
    advance(&mut state, b"\x1b[?9999l");
    assert_eq!(state.active, Screen::Primary); // no screen change
    assert_eq!(row_text(&state, 0), "abc  "); // grid untouched
}

#[test]
fn an_unknown_osc_command_does_not_change_the_title() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]2;keep\x07");
    advance(&mut state, b"\x1b]3;ignored\x07"); // OSC 3 is not handled
    assert_eq!(state.title(), Some("keep"));
}
