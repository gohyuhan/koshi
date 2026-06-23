//! Unit tests for the VTE performer: printing, display width (wide glyphs,
//! combining marks, ambiguous width), deferred wrap, scrolling, and the C0
//! control bytes.

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
    let cursor = state.active_cursor();
    assert_eq!((cursor.row, cursor.col), (0, 1));
    assert!(!cursor.pending_wrap);
}

#[test]
fn print_lays_a_string_left_to_right() {
    let mut state = state(5, 3);
    print_str(&mut state, "hi");
    assert_eq!(glyph(&state, 0, 0), Some('h'));
    assert_eq!(glyph(&state, 0, 1), Some('i'));
    assert_eq!(state.active_cursor().col, 2);
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 2)); // cursor stays
    assert!(state.active_cursor().pending_wrap);
}

#[test]
fn exact_width_line_does_not_scroll_until_the_next_glyph() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // row 0 full, parked
    state.print('d'); // forces the deferred wrap
    assert_eq!(glyph(&state, 0, 0), Some('a')); // row 0 untouched, no early scroll
    assert_eq!(glyph(&state, 1, 0), Some('d')); // wrapped onto row 1
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 1));
    assert!(!cur.pending_wrap);
}

#[test]
fn deferred_wrap_on_the_bottom_row_scrolls() {
    let mut state = state(2, 2);
    print_str(&mut state, "abcde"); // a,b | c,d then 'e' wraps off the bottom
    assert_eq!(glyph(&state, 0, 0), Some('c')); // old bottom row rose
    assert_eq!(glyph(&state, 0, 1), Some('d'));
    assert_eq!(glyph(&state, 1, 0), Some('e')); // 'e' on the fresh bottom row
    assert_eq!(glyph(&state, 1, 1), Some(' '));
    assert_eq!(state.active_cursor().row, 1);
}

#[test]
fn newline_moves_down_and_leaves_the_column() {
    let mut state = state(5, 3);
    state.print('a'); // cursor at col 1
    state.execute(b'\n');
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 1));
    assert!(!cur.pending_wrap);
}

#[test]
fn vertical_tab_and_form_feed_behave_like_newline() {
    for byte in [0x0Bu8, 0x0C] {
        let mut state = state(5, 3);
        state.execute(byte);
        let cur = state.active_cursor();
        assert_eq!(cur.row, 1, "byte {byte:#x} should line-feed");
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
    assert_eq!(state.active_cursor().row, 1); // cursor pinned to the last row
}

#[test]
fn carriage_return_returns_to_column_zero() {
    let mut state = state(5, 3);
    print_str(&mut state, "ab");
    state.execute(b'\r');
    assert_eq!(state.active_cursor().col, 0);
}

#[test]
fn backspace_steps_back_one_column_and_floors_at_zero() {
    let mut state = state(5, 3);
    print_str(&mut state, "ab"); // col 2
    state.execute(0x08);
    assert_eq!(state.active_cursor().col, 1);
    state.execute(0x08);
    state.execute(0x08); // already at 0, saturates
    assert_eq!(state.active_cursor().col, 0);
}

#[test]
fn tab_advances_to_each_eight_column_stop() {
    let mut state = state(20, 1);
    state.execute(b'\t');
    assert_eq!(state.active_cursor().col, 8);
    state.execute(b'\t');
    assert_eq!(state.active_cursor().col, 16);
}

#[test]
fn tab_from_mid_stop_lands_on_the_next_stop() {
    let mut state = state(20, 1);
    print_str(&mut state, "abc"); // col 3
    state.execute(b'\t');
    assert_eq!(state.active_cursor().col, 8);
}

#[test]
fn tab_clamps_to_the_last_column() {
    let mut state = state(6, 1); // last column is 5
    state.execute(b'\t');
    assert_eq!(state.active_cursor().col, 5);
}

#[test]
fn bell_is_ignored() {
    let mut state = state(5, 3);
    print_str(&mut state, "a"); // col 1
    state.execute(0x07);
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 1));
    assert_eq!(glyph(&state, 0, 0), Some('a'));
}

#[test]
fn unknown_control_byte_is_ignored() {
    let mut state = state(5, 3);
    print_str(&mut state, "a");
    state.execute(0x01); // SOH — unhandled
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 1));
    assert_eq!(glyph(&state, 0, 0), Some('a'));
}

#[test]
fn a_cursor_move_clears_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // parked on the last column
    assert!(state.active_cursor().pending_wrap);
    state.execute(b'\r'); // any cursor move clears the latch
    assert!(!state.active_cursor().pending_wrap);
    state.print('c'); // must overwrite in place, not wrap to a new line
    assert_eq!(glyph(&state, 0, 0), Some('c'));
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 1));
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
    assert_eq!(state.active_cursor().col, 5);
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 2));
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 2)); // 2;3 -> 0-based
}

#[test]
fn cup_with_no_arguments_homes_the_cursor() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[4;4H"); // move away first
    advance(&mut state, b"\x1b[H");
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
}

#[test]
fn cup_zero_arguments_are_treated_as_one() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[0;0H");
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
}

#[test]
fn cup_clamps_out_of_range_arguments_to_the_grid_edges() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99;99H");
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (4, 9)); // last row, last col
}

#[test]
fn hvp_positions_like_cup() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[2;4f");
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 3));
}

#[test]
fn cuu_moves_up_by_the_count() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[4;4H"); // (3, 3)
    advance(&mut state, b"\x1b[2A");
    assert_eq!(state.active_cursor().row, 1);
}

#[test]
fn cud_moves_down_and_clamps_to_the_last_row() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99B");
    let cur = state.active_cursor();
    assert_eq!(cur.row, 4);
}

#[test]
fn cuf_moves_forward_and_clamps_to_the_last_column() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99C");
    assert_eq!(state.active_cursor().col, 9);
}

#[test]
fn cub_moves_back_and_floors_at_column_zero() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[1;4H"); // col 3
    advance(&mut state, b"\x1b[5D");
    assert_eq!(state.active_cursor().col, 0);
}

#[test]
fn a_missing_or_zero_move_count_defaults_to_one() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;3H"); // (2, 2)
    advance(&mut state, b"\x1b[A"); // no argument -> up one
    assert_eq!(state.active_cursor().row, 1);
    advance(&mut state, b"\x1b[0A"); // explicit zero -> up one
    let cur = state.active_cursor();
    assert_eq!(cur.row, 0);
}

#[test]
fn a_csi_cursor_move_clears_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // parked on the last column
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[C"); // CUF clears the latch
    assert!(!state.active_cursor().pending_wrap);
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 2));
}

#[test]
fn sgr_preserves_the_pending_wrap_latch() {
    let mut state = state(2, 2);
    print_str(&mut state, "ab"); // parked on the last column
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[1m"); // SGR is not a cursor move
    assert!(state.active_cursor().pending_wrap); // latch survives, unlike a cursor move
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3));
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
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b7"); // DECSC saves the latch
    advance(&mut state, b"\x1b[1;1H"); // a cursor move clears it
    assert!(!state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b8"); // DECRC restores the latch
    assert!(state.active_cursor().pending_wrap);
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 4));
    assert_eq!(state.style, saved_style); // pen restored too
}

#[test]
fn decrc_without_a_save_homes_and_resets_the_pen() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // move away
    advance(&mut state, b"\x1b[1;31m"); // dirty the pen
    advance(&mut state, b"\x1b8"); // DECRC with no prior DECSC
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
    assert_eq!(state.style, Style::default());
}

#[test]
fn decrc_clamps_the_restored_cursor_into_a_shrunk_grid() {
    let mut state = state(10, 10);
    advance(&mut state, b"\x1b[6;9H"); // (5, 8)
    advance(&mut state, b"\x1b7"); // save
    state.resize(PtySize { cols: 3, rows: 3 });
    advance(&mut state, b"\x1b8"); // restore -> clamped to the new bounds
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 2));
}

#[test]
fn reverse_index_moves_the_cursor_up_one_line() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[3;1H"); // row 2
    advance(&mut state, b"\x1bM"); // RI
    assert_eq!(state.active_cursor().row, 1);
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
    assert_eq!(state.primary_scroll_region, Some((1, 3)));
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
}

#[test]
fn decstbm_full_span_clears_the_region() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[2;4r"); // set a region
    advance(&mut state, b"\x1b[1;5r"); // whole screen -> None
    assert_eq!(state.primary_scroll_region, None);
}

#[test]
fn decstbm_with_no_parameters_clears_the_region() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[2;4r"); // set a region
    advance(&mut state, b"\x1b[r"); // CSI r, defaults = whole screen -> None
    assert_eq!(state.primary_scroll_region, None);
}

#[test]
fn decstbm_with_an_invalid_range_is_ignored() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[2;4r"); // valid region (1, 3)
    advance(&mut state, b"\x1b[4;2r"); // top not above bottom -> ignored
    assert_eq!(state.primary_scroll_region, Some((1, 3)));
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 2)); // cursor unchanged
    assert!(!state.active_cursor().pending_wrap);
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 1)); // cursor unchanged
    assert!(!state.active_cursor().pending_wrap);
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 2)); // cursor unchanged (column kept)
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 1)); // cursor unmoved
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 1)); // cursor unmoved
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 1)); // cursor unmoved
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
    let saved = state.primary_cursor.saved.expect("primary cursor saved");
    assert_eq!((saved.row, saved.col), (2, 3));
    advance(&mut state, b"\x1b[1;1H"); // move on the alternate screen
    advance(&mut state, b"\x1b[?1049l");
    assert_eq!(state.active, Screen::Primary);
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3)); // restored
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
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3)); // position restored
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
    let primary = state.primary_cursor.saved.expect("primary cursor saved");
    assert_eq!((primary.row, primary.col), (2, 3));
    advance(&mut state, b"\x1b[2;2H\x1b7"); // on the alternate: move to (1, 1), DECSC into the alt's OWN slot
    let alt = state.alternate_cursor.saved.expect("alternate saved");
    assert_eq!((alt.row, alt.col), (1, 1));
    let primary_after = state.primary_cursor.saved.expect("primary still saved");
    assert_eq!((primary_after.row, primary_after.col), (2, 3)); // the alt DECSC did NOT touch the primary slot
    advance(&mut state, b"\x1b[?1049l"); // back to primary, restore from the primary slot
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3)); // primary stash intact + restored
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

#[test]
fn a_decset_sequence_applies_every_mode_in_the_list() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?25l"); // hide the cursor first
    advance(&mut state, b"\x1b[?25;47h"); // show the cursor AND enter the alternate in one sequence
    assert!(state.cursor_visible()); // ?25 applied
    assert_eq!(state.active, Screen::Alternate); // ?47 applied — not just the first param
}

#[test]
fn a_decrst_sequence_applies_every_mode_in_the_list() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?47h"); // enter the alternate
    advance(&mut state, b"\x1b[?47;25l"); // exit the alternate AND hide the cursor in one sequence
    assert_eq!(state.active, Screen::Primary); // ?47 applied
    assert!(!state.cursor_visible()); // ?25 applied — the second param is honored
}

#[test]
fn dec_1049_clears_the_alternate_buffer_on_exit() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate (cleared on entry)
    advance(&mut state, b"\x1b[1;1Hxyz"); // write on the alternate
    advance(&mut state, b"\x1b[?1049l"); // exit: must clear the alternate too
    assert_eq!(state.active, Screen::Primary);
    advance(&mut state, b"\x1b[?47h"); // re-enter via ?47 (no clear on entry)
    assert_eq!(row_text(&state, 0), "     "); // the alternate was cleared on the prior ?1049 l exit
}

#[test]
fn dec_47_before_1049_in_one_decset_saves_the_primary_cursor_not_the_alternate() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
                                       // `?47` switches to the alternate first; `?1049` must still stash the cursor
                                       // of the screen the list began on (the primary), not the alternate.
    advance(&mut state, b"\x1b[?47;1049h");
    assert_eq!(state.active, Screen::Alternate);
    let saved = state.primary_cursor.saved.expect("primary cursor saved");
    assert_eq!((saved.row, saved.col), (2, 3));
    assert!(
        state.alternate_cursor.saved.is_none(),
        "the save must not land in the alternate slot"
    );
    advance(&mut state, b"\x1b[1;1H"); // move on the alternate
    advance(&mut state, b"\x1b[?1049l"); // exit + restore
    assert_eq!(state.active, Screen::Primary);
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3)); // primary cursor restored
}

#[test]
fn dec_47_before_1049_in_one_decset_clears_the_stale_alternate() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?47h"); // enter the alternate without clearing
    advance(&mut state, b"xyz"); // alternate row 0 = "xyz"
    advance(&mut state, b"\x1b[?47l"); // back to primary; the alternate keeps "xyz"
                                       // `?47` re-enters onto the stale "xyz"; `?1049` must still clear it.
    advance(&mut state, b"\x1b[?47;1049h");
    assert_eq!(state.active, Screen::Alternate);
    assert_eq!(row_text(&state, 0), "     "); // 1049 cleared the stale alternate
}

#[test]
fn dec_47_l_before_1049_l_leaves_the_alternate_uncleared() {
    // `?47 l` switches to the primary first (without clearing); a following
    // `?1049 l` then sees the primary as the live buffer, so its clear is a no-op
    // (only the unconditional DECRC runs), matching alacritty's whichBuf guard.
    // The alternate keeps its contents.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1047h"); // enter the alternate
    advance(&mut state, b"xyz"); // alternate row 0 = "xyz"
    advance(&mut state, b"\x1b[?47;1049l"); // ?47 l leaves first -> ?1049 l clear is skipped
    assert_eq!(state.active, Screen::Primary);
    advance(&mut state, b"\x1b[?47h"); // re-enter via ?47 (no clear on entry)
    assert_eq!(row_text(&state, 0), "xyz  "); // NOT cleared — the clear was skipped on the primary
}

#[test]
fn dec_1049_l_then_1047_l_clears_the_alternate_only_once() {
    // `?1049 l` clears the alternate (with the alternate's pen), switches to the
    // primary, and restores the primary SGR. A trailing `?1047 l` is then on the
    // primary, so its clear must be a no-op — re-clearing would blank with the
    // primary's pen, leaving the wrong background for a later re-entry.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate
    advance(&mut state, b"\x1b[44m"); // alternate pen bg = blue (Indexed 4)
    advance(&mut state, b"xyz"); // draw on the alternate with the blue pen
    advance(&mut state, b"\x1b[?1049;1047l"); // one clearing exit; the trailing ?1047 l must be a no-op
    assert_eq!(state.active, Screen::Primary);
    advance(&mut state, b"\x1b[?47h"); // re-enter to inspect the alternate cells
    let blue = styled(|s| s.set_bg(Color::Indexed(4)));
    for row in 0..3 {
        assert!(
            (0..5).all(|c| state.active_grid().cell(row, c).map(Cell::style) == Some(blue)),
            "row {row} should be blanked with the alternate's blue pen, not re-cleared with the primary's"
        );
    }
}

#[test]
fn dec_1049_then_47_in_one_decset_saves_the_primary_and_seeds_the_alternate_once() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
                                       // `?1049` enters + saves + clears; the trailing `?47` must be a no-op (no
                                       // re-seed of the alternate cursor, no second clear).
    advance(&mut state, b"\x1b[?1049;47h");
    assert_eq!(state.active, Screen::Alternate);
    let saved = state.primary_cursor.saved.expect("primary cursor saved");
    assert_eq!((saved.row, saved.col), (2, 3));
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3)); // alternate cursor seeded from the primary
}

#[test]
fn verify_params_iter_yields_correct_param_groups() {
    // Empirical verification of vte 0.15's Params::iter() behavior.
    // Each item yielded is &[u16] — a slice containing the top-level param
    // and any colon-separated subparams. For simple DEC modes (no subparams),
    // it's a slice with one element.
    use vte::{Parser, Perform};

    struct Inspector {
        results: Vec<Vec<u16>>,
    }

    impl Perform for Inspector {
        fn csi_dispatch(
            &mut self,
            params: &vte::Params,
            intermediates: &[u8],
            _ignore: bool,
            _action: char,
        ) {
            if intermediates == b"?" {
                for param in params.iter() {
                    self.results.push(param.to_vec());
                }
            }
        }
        fn execute(&mut self, _: u8) {}
        fn print(&mut self, _: char) {}
        fn put(&mut self, _: u8) {}
        fn unhook(&mut self) {}
        fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {}
        fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
    }

    let mut parser = Parser::new();
    let mut insp = Inspector { results: vec![] };

    // Test 1: multi-param with simple params — each param is a single u16
    parser.advance(&mut insp, b"\x1b[?1049;25h");
    assert_eq!(insp.results.len(), 2);
    assert_eq!(insp.results[0], vec![1049]);
    assert_eq!(insp.results[1], vec![25]);
    insp.results.clear();

    // Test 2: different order
    parser.advance(&mut insp, b"\x1b[?47;1049h");
    assert_eq!(insp.results.len(), 2);
    assert_eq!(insp.results[0], vec![47]);
    assert_eq!(insp.results[1], vec![1049]);
    insp.results.clear();

    // Test 3: missing param defaults to 0 per ANSI spec
    parser.advance(&mut insp, b"\x1b[?h");
    assert_eq!(insp.results.len(), 1);
    assert_eq!(insp.results[0], vec![0]);
    insp.results.clear();

    // Test 4: single param
    parser.advance(&mut insp, b"\x1b[?25h");
    assert_eq!(insp.results.len(), 1);
    assert_eq!(insp.results[0], vec![25]);
    insp.results.clear();

    // Test 5: duplicate modes
    parser.advance(&mut insp, b"\x1b[?1049;1049h");
    assert_eq!(insp.results.len(), 2);
    assert_eq!(insp.results[0], vec![1049]);
    assert_eq!(insp.results[1], vec![1049]);
}

#[test]
fn dec_1049_reset_restores_the_cursor_even_when_already_on_primary() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // (2, 3)
    advance(&mut state, b"\x1b[?1048h"); // save the primary cursor (no switch)
    advance(&mut state, b"\x1b[1;1H"); // move to (0, 0)
    advance(&mut state, b"\x1b[?1049l"); // already on primary: the ?1048 l restore must still run
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3)); // restored
    assert_eq!(state.active, Screen::Primary);
}

#[test]
fn scroll_region_does_not_leak_from_the_alternate_to_the_primary() {
    let mut state = state(10, 6);
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate
    advance(&mut state, b"\x1b[2;4r"); // alt sets DECSTBM rows 2..4 (1-based) -> (1, 3)
    assert_eq!(state.alternate_scroll_region, Some((1, 3)));
    advance(&mut state, b"\x1b[?1049l"); // exit to the primary
    assert_eq!(state.active, Screen::Primary);
    assert_eq!(state.primary_scroll_region, None); // primary margins never touched
}

#[test]
fn each_screen_keeps_its_own_scroll_region_across_a_round_trip() {
    let mut state = state(10, 6);
    advance(&mut state, b"\x1b[2;4r"); // primary margins -> (1, 3)
    assert_eq!(state.primary_scroll_region, Some((1, 3)));
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate
    assert_eq!(state.alternate_scroll_region, None); // alt starts unconstrained
    advance(&mut state, b"\x1b[1;3r"); // alt margins -> (0, 2)
    assert_eq!(state.alternate_scroll_region, Some((0, 2)));
    advance(&mut state, b"\x1b[?1049l"); // back to the primary
    assert_eq!(state.primary_scroll_region, Some((1, 3))); // primary margins survived
}

#[test]
fn resize_clears_both_screens_scroll_regions() {
    let mut state = state(10, 6);
    advance(&mut state, b"\x1b[2;4r"); // primary region -> (1, 3)
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate
    advance(&mut state, b"\x1b[1;3r"); // alt region -> (0, 2)
    state.resize(PtySize { cols: 8, rows: 4 });
    assert_eq!(state.primary_scroll_region, None);
    assert_eq!(state.alternate_scroll_region, None);
}

#[test]
fn line_feed_respects_the_alternate_screens_own_region() {
    let mut state = state(4, 4);
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate
    advance(&mut state, b"\x1b[1;2r"); // alt region rows 1..2 (1-based) -> (0, 1)
    advance(&mut state, b"\x1b[2;1Hx"); // cursor to the region bottom (row 1), print 'x'
    state.execute(b'\n'); // line feed at the region bottom -> scroll within (0, 1)
    assert_eq!(glyph(&state, 0, 0), Some('x')); // 'x' rose from row 1 to row 0
}

#[test]
fn a_fresh_1049_entry_resets_a_stale_alternate_scroll_region() {
    let mut state = state(10, 6);
    advance(&mut state, b"\x1b[?1049h"); // app A enters the alternate
    advance(&mut state, b"\x1b[2;4r"); // app A sets DECSTBM rows 2..4 -> (1, 3)
    advance(&mut state, b"\x1b[?47l"); // exit via ?47 l (non-clearing: leaves the region set)
    assert_eq!(state.alternate_scroll_region, Some((1, 3))); // still stale after a non-clearing exit
    advance(&mut state, b"\x1b[?1049h"); // app B enters fresh via ?1049 h
    assert_eq!(state.alternate_scroll_region, None); // entry reset the inherited margins to full screen
}

#[test]
fn a_clearing_exit_resets_the_alternate_scroll_region() {
    let mut state = state(10, 6);
    advance(&mut state, b"\x1b[?1049h"); // enter
    advance(&mut state, b"\x1b[2;4r"); // set DECSTBM -> (1, 3)
    advance(&mut state, b"\x1b[?1049l"); // a clearing exit must reset the region too
    assert_eq!(state.alternate_scroll_region, None);
}

#[test]
fn dec_47_reentry_preserves_the_alternate_scroll_region() {
    let mut state = state(10, 6);
    advance(&mut state, b"\x1b[?47h"); // enter via ?47 (preserve mode)
    advance(&mut state, b"\x1b[2;4r"); // set the alternate's DECSTBM -> (1, 3)
    advance(&mut state, b"\x1b[?47l"); // non-clearing exit
    advance(&mut state, b"\x1b[?47h"); // re-enter via ?47 — must preserve the region, like the cursor
    assert_eq!(state.alternate_scroll_region, Some((1, 3)));
}

#[test]
fn a_fresh_1049_entry_drops_a_stale_alternate_saved_cursor() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[?1049h"); // app A enters
    advance(&mut state, b"\x1b[3;3H\x1b7"); // app A: move to (2, 2), DECSC (stashes the alt cursor)
    assert!(state.alternate_cursor.saved.is_some());
    advance(&mut state, b"\x1b[?47l"); // non-clearing exit (leaves the alt cursor + its stash)
    advance(&mut state, b"\x1b[?1049h"); // app B enters fresh
    assert!(state.alternate_cursor.saved.is_none()); // app A's DECSC stash dropped
    advance(&mut state, b"\x1b[5;5H\x1b8"); // app B DECRC with no prior DECSC -> home, not (2, 2)
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
}

#[test]
fn a_fresh_1049_entry_shows_the_cursor_even_if_a_prior_session_hid_it() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[?1049h"); // app A enters
    advance(&mut state, b"\x1b[?25l"); // app A hides the cursor on the alternate
    assert!(!state.cursor_visible());
    advance(&mut state, b"\x1b[?47l"); // non-clearing exit (the alternate keeps is_visible = false)
    advance(&mut state, b"\x1b[?1049h"); // app B enters fresh
    assert!(state.cursor_visible()); // shown by default; app A's ?25 l is not inherited
}

#[test]
fn a_clearing_exit_drops_the_alternate_wrap_latch() {
    let mut state = state(3, 2); // 3 cols, 2 rows
    advance(&mut state, b"\x1b[?1047h"); // enter the alternate
    advance(&mut state, b"abc"); // fill row 0 -> parks the wrap latch at the last column
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[?1047l"); // clearing exit erases the parked glyph -> the latch must drop
    advance(&mut state, b"\x1b[?47h"); // non-clearing re-entry sees the fresh cursor
    assert!(!state.active_cursor().pending_wrap);
    state.print('z'); // first print lands in place, no spurious wrap against an erased glyph
    assert_eq!(glyph(&state, 0, 0), Some('z'));
    assert_eq!(state.active_cursor().row, 0);
}

#[test]
fn a_clearing_exit_resets_the_alternate_cursor_to_home() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[?1047h"); // enter
    advance(&mut state, b"\x1b[3;4H"); // move the alternate cursor to (2, 3)
    advance(&mut state, b"\x1b[?1047l"); // a clearing exit ends the session -> reset to a fresh buffer
    advance(&mut state, b"\x1b[?47h"); // non-clearing re-entry sees the fresh cursor
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0)); // home, not the dead session's (2, 3)
}

// --- Per-screen cursor independence ---

#[test]
fn dec_1049h_does_not_carry_pending_wrap_to_the_alternate() {
    let mut state = state(3, 2);
    advance(&mut state, b"abc"); // fills row 0 on primary, parks at (0, 2)
    assert!(state.active_cursor().pending_wrap); // parked on primary
    advance(&mut state, b"\x1b[?1049h"); // enter alternate: seed col from primary, clear latch + grid
    assert!(!state.active_cursor().pending_wrap); // latch NOT carried
    state.print('x'); // must not wrap early to row 1
    assert_eq!(glyph(&state, 0, 2), Some('x')); // lands at the seeded column on row 0
    assert_eq!(state.active_cursor().row, 0); // no early wrap
}

#[test]
fn pending_wrap_is_independent_per_screen() {
    let mut state = state(3, 2);
    advance(&mut state, b"abc"); // primary parks at (0, 2)
    assert!(state.active_cursor().pending_wrap); // primary has the latch
    advance(&mut state, b"\x1b[?47h"); // enter alternate (no clear, no reseed): its own cursor
    assert!(!state.active_cursor().pending_wrap); // alternate latch independent (starts clear)
    state.print('x'); // alternate cursor starts at home (0, 0); no early wrap
    assert_eq!(glyph(&state, 0, 0), Some('x'));
    assert_eq!(state.active_cursor().row, 0);
    advance(&mut state, b"\x1b[?47l"); // back to primary
    assert!(state.active_cursor().pending_wrap); // primary latch untouched
}

#[test]
fn dec_47_reentry_resumes_where_the_alternate_left_off() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[?47h"); // enter the alternate (its own cursor at home)
    advance(&mut state, b"\x1b[3;4Hxy"); // draw on the alternate; cursor ends at (2, 5)
    let alt = state.active_cursor();
    assert_eq!((alt.row, alt.col), (2, 5));
    advance(&mut state, b"\x1b[?47l"); // pop back to the primary (alternate kept intact)
    advance(&mut state, b"zz"); // primary output — must not disturb the alternate cursor
    advance(&mut state, b"\x1b[?47h"); // re-enter: must resume at (2, 5), not reseed from the primary
    let alt = state.active_cursor();
    assert_eq!((alt.row, alt.col), (2, 5));
    advance(&mut state, b"w"); // resumes exactly where the alternate left off
    assert_eq!(glyph(&state, 2, 5), Some('w'));
}

#[test]
fn dec_47_reentry_preserves_the_alternate_wrap_latch() {
    let mut state = state(3, 2);
    advance(&mut state, b"\x1b[?47h"); // enter the alternate
    advance(&mut state, b"abc"); // fill the alternate's row 0 — parks the wrap latch at (0, 2)
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[?47l"); // back to primary
    advance(&mut state, b"\x1b[?47h"); // re-enter: the latch must survive, not be cleared by a reseed
    assert!(state.active_cursor().pending_wrap);
    state.print('z'); // the parked latch wraps to row 1 instead of overprinting (0, 2)
    assert_eq!(glyph(&state, 1, 0), Some('z'));
}

#[test]
fn cursor_position_is_independent_per_screen() {
    let mut state = state(5, 5);
    advance(&mut state, b"\x1b[3;3H"); // primary cursor at (2, 2)
    assert_eq!(
        (state.active_cursor().row, state.active_cursor().col),
        (2, 2)
    );
    advance(&mut state, b"\x1b[?1049h"); // enter alternate: cursor seeded from the primary
    assert_eq!(
        (state.active_cursor().row, state.active_cursor().col),
        (2, 2)
    ); // seeded, not (0, 0)
    advance(&mut state, b"\x1b[4;4H"); // move the alternate cursor to (3, 3)
    assert_eq!(
        (state.active_cursor().row, state.active_cursor().col),
        (3, 3)
    );
    advance(&mut state, b"\x1b[?1049l"); // back to primary
    assert_eq!(
        (state.active_cursor().row, state.active_cursor().col),
        (2, 2)
    ); // primary intact, unaffected by alt's move
}

#[test]
fn cursor_visibility_is_independent_per_screen() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?25l"); // hide the cursor on primary
    assert!(!state.cursor_visible());
    advance(&mut state, b"\x1b[?1049h"); // enter alternate
    assert!(state.cursor_visible()); // alternate keeps its own (visible) state — per-screen, deliberate xterm deviation
    advance(&mut state, b"\x1b[?1049l"); // back to primary
    assert!(!state.cursor_visible()); // primary still hidden
}

// --- Unicode display-width: wide glyphs, combining marks, ambiguous width ---

#[test]
fn wide_char_occupies_two_cells_and_advances_by_two() {
    let mut state = state(5, 3);
    state.print('中'); // CJK ideograph, display width 2
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '中');
    assert_eq!(base.width(), 2);
    // The second column is a width-0 continuation placeholder.
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0));
    // Cursor steps past both cells.
    assert_eq!(state.active_cursor().col, 2);
    assert!(!state.active_cursor().pending_wrap);
}

#[test]
fn emoji_is_wide() {
    let mut state = state(5, 3);
    state.print('😀'); // emoji, display width 2
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::ch), Some('😀'));
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(2));
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0));
    assert_eq!(state.active_cursor().col, 2);
}

#[test]
fn two_wide_chars_lay_side_by_side() {
    let mut state = state(6, 2);
    print_str(&mut state, "中文");
    assert_eq!(glyph(&state, 0, 0), Some('中'));
    assert_eq!(glyph(&state, 0, 2), Some('文'));
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0));
    assert_eq!(state.active_grid().cell(0, 3).map(Cell::width), Some(0));
    assert_eq!(state.active_cursor().col, 4);
}

#[test]
fn ambiguous_width_char_is_narrow() {
    let mut state = state(5, 3);
    state.print('§'); // East-Asian Ambiguous → narrow under the default policy
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    assert_eq!(state.active_cursor().col, 1);
}

#[test]
fn combining_mark_attaches_to_the_previous_cell_without_advancing() {
    let mut state = state(5, 3);
    state.print('e');
    state.print('\u{301}'); // combining acute accent → é
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), 'e');
    assert_eq!(cell.combining(), ['\u{301}']);
    assert_eq!(cell.width(), 1); // base width unchanged
    assert_eq!(state.active_cursor().col, 1); // cursor did not advance
}

#[test]
fn multiple_combining_marks_stack_in_arrival_order() {
    let mut state = state(5, 3);
    state.print('a');
    state.print('\u{301}'); // acute
    state.print('\u{308}'); // diaeresis
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.combining(), ['\u{301}', '\u{308}']);
    assert_eq!(state.active_cursor().col, 1);
}

#[test]
fn combining_mark_attaches_to_a_wide_base_not_its_continuation() {
    let mut state = state(6, 2);
    state.print('中'); // base at col 0, continuation at col 1, cursor → 2
    state.print('\u{301}'); // must land on the base at col 0, stepping over col 1
    assert_eq!(
        state.active_grid().cell(0, 0).expect("base").combining(),
        ['\u{301}']
    );
    assert!(state
        .active_grid()
        .cell(0, 1)
        .expect("continuation")
        .combining()
        .is_empty());
    assert_eq!(state.active_cursor().col, 2);
}

#[test]
fn combining_mark_at_line_start_is_dropped() {
    let mut state = state(5, 3);
    state.print('\u{301}'); // nothing precedes it on the line
    assert!(state
        .active_grid()
        .cell(0, 0)
        .expect("in bounds")
        .combining()
        .is_empty());
    assert_eq!(state.active_cursor().col, 0); // no advance, no panic
}

#[test]
fn combining_mark_attaches_to_a_parked_glyph() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // row 0 full, cursor parked at col 2 with the wrap latch
    assert!(state.active_cursor().pending_wrap);
    state.print('\u{301}'); // attaches to the parked 'c' without wrapping
    let cell = state.active_grid().cell(0, 2).expect("in bounds");
    assert_eq!(cell.ch(), 'c');
    assert_eq!(cell.combining(), ['\u{301}']);
    assert_eq!(state.active_cursor().col, 2);
    assert!(state.active_cursor().pending_wrap); // latch preserved
}

#[test]
fn wide_char_at_the_last_column_wraps_and_blanks_the_freed_cell() {
    let mut state = state(3, 2); // columns 0..=2; last col = 2
    print_str(&mut state, "ab"); // a@0, b@1, cursor at the last free col 2
    assert_eq!(state.active_cursor().col, 2);
    assert!(!state.active_cursor().pending_wrap);
    state.print('中'); // width 2, only col 2 free → blank it and wrap whole
    let freed = state.active_grid().cell(0, 2).expect("in bounds");
    assert_eq!(freed.ch(), ' '); // freed column blanked
    assert_eq!(freed.width(), 1);
    assert_eq!(glyph(&state, 1, 0), Some('中')); // glyph starts the next line whole
    assert_eq!(state.active_grid().cell(1, 1).map(Cell::width), Some(0));
    assert_eq!(
        (state.active_cursor().row, state.active_cursor().col),
        (1, 2)
    );
}

#[test]
fn wide_char_reaching_the_last_column_parks() {
    let mut state = state(4, 2); // last col = 3
    print_str(&mut state, "xx"); // cursor at col 2
    state.print('中'); // occupies cols 2 and 3 (the last) → park, no wrap yet
    assert_eq!(glyph(&state, 0, 2), Some('中'));
    assert_eq!(state.active_grid().cell(0, 3).map(Cell::width), Some(0));
    let cur = state.active_cursor();
    assert_eq!(cur.col, 3);
    assert!(cur.pending_wrap);
    state.print('y'); // the deferred wrap fires here
    assert_eq!(glyph(&state, 1, 0), Some('y'));
}

#[test]
fn control_char_reaching_print_is_ignored() {
    let mut state = state(5, 3);
    state.print('a');
    state.print('\u{0}'); // NUL: a control char, no display width
    assert_eq!(glyph(&state, 0, 0), Some('a'));
    assert_eq!(state.active_cursor().col, 1); // nothing written, no advance
}
