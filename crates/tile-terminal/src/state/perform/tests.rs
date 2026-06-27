//! Unit tests for the VTE performer: printing, display width (wide glyphs,
//! combining marks, ambiguous width), deferred wrap, scrolling, and the C0
//! control bytes.

use super::*;
use std::path::{Path, PathBuf};
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
    assert_eq!(cell.style(), state.active_render().style);
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
fn el_0_is_a_noop_when_a_wrap_is_pending() {
    // After printing into the last column the cursor parks there with a pending
    // wrap, so it is logically past the line end: EL 0 (cursor-to-end) erases
    // nothing and the parked last-column glyph survives (alacritty parity).
    let mut state = state(5, 2);
    print_str(&mut state, "abcde"); // 'e' lands at col 4 with the wrap pending
    advance(&mut state, b"\x1b[K"); // EL 0
    assert_eq!(row_text(&state, 0), "abcde"); // 'e' preserved, not erased
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
    advance(&mut state, b"\x1b[3J"); // erase scrollback only — visible screen intact
    assert_eq!(row_text(&state, 0), "abc");
    assert_eq!(row_text(&state, 1), "def");
    assert_eq!(row_text(&state, 2), "ghi");
}

#[test]
fn ed_3_clears_the_retained_scrollback() {
    let mut state = state(3, 2); // two rows
    state.active_cursor_mut().row = 1; // sit at the bottom so line feeds scroll
    state.linefeed();
    state.linefeed();
    assert_eq!(state.scrollback().len(), 2); // history populated

    advance(&mut state, b"\x1b[3J"); // xterm "erase saved lines"
    assert!(state.scrollback().is_empty());
}

#[test]
fn ed_3_on_the_alternate_screen_leaves_primary_scrollback_intact() {
    // Scrollback is the primary screen's history; a full-screen app on the
    // alternate screen must not erase it with CSI 3 J.
    let mut state = state(3, 2);
    state.active_cursor_mut().row = 1; // bottom row, on the primary screen
    state.linefeed();
    state.linefeed();
    assert_eq!(state.scrollback().len(), 2); // populated from the primary

    state.active = Screen::Alternate;
    advance(&mut state, b"\x1b[3J"); // ED 3 while on the alternate screen
    assert_eq!(state.scrollback().len(), 2); // primary history untouched
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
    assert_eq!(state.active_render().style, styled(|s| s.set_bold(true)));
}

#[test]
fn sgr_zero_resets_the_pen() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;31m"); // bold + red
    advance(&mut state, b"\x1b[0m");
    assert_eq!(state.active_render().style, Style::default());
}

#[test]
fn sgr_empty_params_reset_like_zero() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1m");
    advance(&mut state, b"\x1b[m"); // bare CSI m is an implicit reset
    assert_eq!(state.active_render().style, Style::default());
}

#[test]
fn sgr_attribute_off_codes_clear_each_attribute() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;3;4;7m"); // bold, italic, underline, reverse on
    advance(&mut state, b"\x1b[22;23;24;27m"); // each turned back off
    assert_eq!(state.active_render().style, Style::default());
}

#[test]
fn sgr_sixteen_color_foreground_and_background() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[31;42m"); // fg red (1), bg green (2)
    assert_eq!(
        state.active_render().style,
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
        state.active_render().style,
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
    assert_eq!(state.active_render().style, Style::default());
}

#[test]
fn sgr_256_color_foreground() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;5;196m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Indexed(196)))
    );
}

#[test]
fn sgr_256_color_background() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[48;5;21m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_bg(Color::Indexed(21)))
    );
}

#[test]
fn sgr_truecolor_foreground_semicolon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;2;255;128;0m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Rgb(255, 128, 0)))
    );
}

#[test]
fn sgr_truecolor_background_semicolon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[48;2;10;20;30m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_bg(Color::Rgb(10, 20, 30)))
    );
}

#[test]
fn sgr_256_color_colon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:5:196m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Indexed(196)))
    );
}

#[test]
fn sgr_256_color_colon_form_with_empty_colorspace_id() {
    // `38:5::196` — a stray empty colorspace slot before the index (stored by vte
    // as a leading `0`). The index is read from the final subparameter, so the
    // slot is skipped and the palette index is honored (symmetric with the RGB
    // colon form above).
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:5::196m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Indexed(196)))
    );
}

#[test]
fn sgr_truecolor_colon_form() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:2:255:128:0m");
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Rgb(255, 128, 0)))
    );
}

#[test]
fn sgr_truecolor_colon_form_with_empty_colorspace_id() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:2::255:128:0m"); // ITU form: empty colorspace slot
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Rgb(255, 128, 0)))
    );
}

#[test]
fn sgr_combines_multiple_codes_in_one_sequence() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;4;38;5;200;48;2;1;2;3m");
    assert_eq!(
        state.active_render().style,
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
    assert_eq!(state.active_render().style, styled(|s| s.set_bold(true)));
}

#[test]
fn sgr_incomplete_extended_color_leaves_the_pen_unchanged() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;5m"); // 256-color selector with no index
    assert_eq!(state.active_render().style, Style::default());
}

#[test]
fn sgr_incomplete_colon_extended_color_leaves_the_pen_unchanged() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38:5m"); // colon 256-color selector with no index
    assert_eq!(state.active_render().style, Style::default());
}

#[test]
fn sgr_256_color_index_out_of_range_leaves_the_pen_unchanged() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;5;256m"); // index 256 > 255 — out of range
    assert_eq!(state.active_render().style, Style::default()); // rejected, NOT wrapped to Indexed(0)
    advance(&mut state, b"\x1b[38:5:300m"); // colon form, index 300 > 255
    assert_eq!(state.active_render().style, Style::default()); // rejected, NOT wrapped to Indexed(44)
}

#[test]
fn sgr_truecolor_channel_out_of_range_leaves_the_pen_unchanged() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[38;2;999;0;0m"); // semicolon form, r = 999 > 255
    assert_eq!(state.active_render().style, Style::default()); // rejected, NOT wrapped to Rgb(231, 0, 0)
    advance(&mut state, b"\x1b[48:2:0:256:0m"); // colon form bg, g = 256 > 255
    assert_eq!(state.active_render().style, Style::default()); // rejected, NOT wrapped to Rgb(0, 0, 0)
}

#[test]
fn sgr_out_of_range_truecolor_drains_its_channels_not_leaking_to_later_codes() {
    let mut state = state(5, 2);
    // r = 999 is out of range → the color is rejected, but 31 and 32 are its g/b
    // channels and must be CONSUMED, not reinterpreted as standalone SGR codes
    // (fg red / fg green). The pen must end fully unchanged.
    advance(&mut state, b"\x1b[38;2;999;31;32m");
    assert_eq!(state.active_render().style, Style::default()); // no leak

    // Exactly three channels (999, 1, 2) are drained, then a genuine trailing
    // `1` is applied as SGR bold — proving we consume three and no more.
    advance(&mut state, b"\x1b[38;2;999;1;2;1m");
    assert_eq!(state.active_render().style, styled(|s| s.set_bold(true)));
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
        state.active_render().style,
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
        state.active_render().style,
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
    let saved_style = state.active_render().style;
    advance(&mut state, b"\x1b[s"); // SCOSC
    advance(&mut state, b"\x1b[5;5H"); // move away
    advance(&mut state, b"\x1b[0m"); // reset the pen to a different style
    advance(&mut state, b"\x1b[u"); // SCORC
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 4));
    assert_eq!(state.active_render().style, saved_style); // pen restored too
}

#[test]
fn decrc_without_a_save_homes_and_resets_the_pen() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // move away
    advance(&mut state, b"\x1b[1;31m"); // dirty the pen
    advance(&mut state, b"\x1b8"); // DECRC with no prior DECSC
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
    assert_eq!(state.active_render().style, Style::default());
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
        state.active_render().style,
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
fn osc_7_reports_the_working_directory() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///Users/me/proj\x07");
    let cwd = state.current_cwd().expect("cwd set");
    assert_eq!(cwd.path(), Path::new("/Users/me/proj"));
    assert_eq!(cwd.host(), None); // empty authority
}

#[test]
fn osc_7_preserves_the_host_component() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file://myhost/home/u\x07");
    let cwd = state.current_cwd().expect("cwd set");
    assert_eq!(cwd.path(), Path::new("/home/u"));
    assert_eq!(cwd.host(), Some("myhost")); // host kept for the spawn-layer check
}

#[test]
fn osc_7_keeps_localhost_as_the_host() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file://localhost/home/u\x07");
    let cwd = state.current_cwd().expect("cwd set");
    assert_eq!(cwd.path(), Path::new("/home/u"));
    assert_eq!(cwd.host(), Some("localhost"));
}

#[test]
fn osc_7_percent_decodes_the_path() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///home/a%20b\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/home/a b")
    );
}

#[test]
fn osc_7_percent_decodes_a_multibyte_sequence() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///p/%C3%A9\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/p/\u{e9}")
    );
}

#[test]
fn osc_7_accepts_a_string_terminator() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///srv\x1b\\");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/srv")
    );
}

#[test]
fn osc_7_keeps_an_embedded_semicolon_in_the_path() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///a;b\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/a;b")
    );
}

#[test]
fn the_cwd_is_none_until_osc_7_reports_one() {
    let state = state(5, 3);
    assert!(state.current_cwd().is_none());
}

#[test]
fn osc_7_ignores_a_non_file_uri() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;http://example/x\x07");
    assert!(state.current_cwd().is_none());
}

#[test]
fn osc_7_ignores_a_uri_with_no_path() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file://host\x07");
    assert!(state.current_cwd().is_none());
}

#[test]
fn osc_7_ignores_an_empty_payload() {
    // `ESC ] 7 ST` → params = ["7"], so the `params.len() > 1` guard skips it.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7\x07");
    assert!(state.current_cwd().is_none());
}

#[test]
fn osc_7_accepts_a_case_insensitive_scheme() {
    // RFC 3986: the scheme compares case-insensitively, the path does not.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;FILE:///srv\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/srv")
    );
    advance(&mut state, b"\x1b]7;File:///opt/App\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/opt/App")
    );
}

#[test]
fn osc_7_keeps_the_last_good_cwd_when_a_later_emit_is_invalid() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///good\x07");
    advance(&mut state, b"\x1b]7;garbage\x07"); // unparseable
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/good")
    );
}

#[test]
fn osc_7_a_later_valid_report_updates_the_cwd() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///first\x07");
    advance(&mut state, b"\x1b]7;file:///second\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/second")
    );
}

#[test]
fn osc_7_rejects_a_path_with_a_nul_byte() {
    // `%00` decodes to a NUL, which cannot occur in a real path; the report is
    // rejected and the previous good cwd is left intact.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///good\x07");
    advance(&mut state, b"\x1b]7;file:///a%00b\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/good")
    );
}

#[test]
fn osc_7_cwd_survives_a_screen_switch() {
    // The reported cwd belongs to the shell, not a screen buffer, so entering
    // the alternate screen must not clear it.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///work\x07");
    advance(&mut state, b"\x1b[?1049h"); // enter the alternate screen
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("/work")
    );
}

#[test]
fn osc_7_reports_the_root_directory() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///\x07");
    assert_eq!(state.current_cwd().expect("cwd set").path(), Path::new("/"));
}

#[test]
fn osc_7_decodes_an_encoded_slash_after_splitting_the_host() {
    // The host/path split is on the first *raw* slash, so a `%2F` survives the
    // split and only then decodes to `/` — yielding two path components.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file://host/a%2Fb\x07");
    let cwd = state.current_cwd().expect("cwd set");
    assert_eq!(cwd.path(), Path::new("/a/b"));
    assert_eq!(cwd.host(), Some("host"));
}

#[cfg(windows)]
#[test]
fn osc_7_strips_the_leading_slash_before_a_windows_drive() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///C:/Users/me\x07");
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        Path::new("C:/Users/me")
    );
}

#[cfg(unix)]
#[test]
fn osc_7_preserves_a_non_utf8_path_on_unix() {
    use std::os::unix::ffi::OsStringExt;
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b]7;file:///p/%FF\x07");
    let expected = PathBuf::from(std::ffi::OsString::from_vec(b"/p/\xff".to_vec()));
    assert_eq!(
        state.current_cwd().expect("cwd set").path(),
        expected.as_path()
    );
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

#[test]
fn overwriting_a_wide_base_with_a_narrow_clears_the_orphan_continuation() {
    let mut state = state(5, 2);
    state.print('中'); // col 0 base (width 2), col 1 continuation (width 0)
    advance(&mut state, b"\x1b[1;1H"); // cursor home (0, 0)
    state.print('a'); // overwrite the base with a narrow glyph
    assert_eq!(glyph(&state, 0, 0), Some('a'));
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    // The stale continuation must be blanked, not left as a width-0 orphan.
    let cont = state.active_grid().cell(0, 1).expect("in bounds");
    assert_eq!(cont.ch(), ' ');
    assert_eq!(cont.width(), 1);
}

#[test]
fn overwriting_a_wide_continuation_with_a_narrow_clears_the_orphan_base() {
    let mut state = state(5, 2);
    state.print('中'); // col 0 base, col 1 continuation
    advance(&mut state, b"\x1b[1;2H"); // cursor to (0, 1), the continuation
    state.print('a'); // overwrite the continuation with a narrow glyph
                      // The orphaned wide base must be blanked, not left claiming two columns.
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), ' ');
    assert_eq!(base.width(), 1);
    assert_eq!(glyph(&state, 0, 1), Some('a'));
}

#[test]
fn a_wide_write_splitting_an_adjacent_wide_clears_its_far_half() {
    let mut state = state(6, 2);
    advance(&mut state, b"\x1b[1;2H"); // cursor to (0, 1)
    state.print('文'); // col 1 base (width 2), col 2 continuation (width 0)
    advance(&mut state, b"\x1b[1;1H"); // home (0, 0)
    state.print('中'); // wide write over cols 0,1 — splits the old glyph
    assert_eq!(glyph(&state, 0, 0), Some('中'));
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0)); // new continuation
                                                                          // The old glyph's far continuation at col 2 is now orphaned → blanked.
    let far = state.active_grid().cell(0, 2).expect("in bounds");
    assert_eq!(far.ch(), ' ');
    assert_eq!(far.width(), 1);
}

// --- Wide-pair integrity across erase / insert / delete cell ops ---

#[test]
fn el_to_eol_from_a_continuation_column_clears_the_orphan_base() {
    let mut state = state(5, 2);
    state.print('中'); // col 0 base (w2), col 1 continuation (w0)
    advance(&mut state, b"\x1b[1;2H"); // cursor onto the continuation (0, 1)
    advance(&mut state, b"\x1b[0K"); // erase cursor→EOL: clears col 1, splits the pair
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), ' '); // orphaned base blanked
    assert_eq!(base.width(), 1);
}

#[test]
fn el_to_cursor_ending_on_a_wide_base_clears_the_orphan_continuation() {
    let mut state = state(5, 2);
    state.print('中'); // col 0 base, col 1 continuation
    advance(&mut state, b"\x1b[1;1H"); // cursor home (0, 0) = the base
    advance(&mut state, b"\x1b[1K"); // erase SOL→cursor: clears col 0, orphans col 1
    let cont = state.active_grid().cell(0, 1).expect("in bounds");
    assert_eq!(cont.ch(), ' ');
    assert_eq!(cont.width(), 1);
}

#[test]
fn ed_to_end_from_a_continuation_column_clears_the_orphan_base() {
    let mut state = state(5, 2);
    state.print('中'); // (0,0) base, (0,1) continuation
    advance(&mut state, b"\x1b[1;2H"); // cursor onto the continuation
    advance(&mut state, b"\x1b[0J"); // erase cursor→end of screen: clears (0,1)
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), ' ');
    assert_eq!(base.width(), 1);
}

#[test]
fn ich_between_a_wide_pair_clears_both_orphaned_halves() {
    let mut state = state(6, 2);
    state.print('中'); // col 0 base, col 1 continuation
    advance(&mut state, b"\x1b[1;2H"); // cursor onto the continuation (0, 1)
    advance(&mut state, b"\x1b[@"); // insert 1 blank at col 1, splitting the pair
                                    // base@0 lost its continuation; the displaced continuation@2 lost its base.
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    assert_eq!(state.active_grid().cell(0, 2).map(Cell::width), Some(1));
    assert_eq!(glyph(&state, 0, 0), Some(' '));
    assert_eq!(glyph(&state, 0, 2), Some(' '));
}

#[test]
fn ich_truncating_a_wide_continuation_off_the_edge_clears_the_orphan_base() {
    let mut state = state(4, 2);
    print_str(&mut state, "xx"); // cols 0,1
    state.print('中'); // col 2 base, col 3 continuation
    advance(&mut state, b"\x1b[1;1H"); // home
    advance(&mut state, b"\x1b[@"); // insert pushes the pair right; continuation falls off
                                    // base now at the last column with no continuation → blanked.
    assert_eq!(state.active_grid().cell(0, 3).map(Cell::width), Some(1));
    assert_eq!(glyph(&state, 0, 3), Some(' '));
}

#[test]
fn dch_deleting_a_continuation_clears_the_orphan_base() {
    let mut state = state(5, 2);
    state.print('中'); // col 0 base, col 1 continuation
    advance(&mut state, b"\x1b[1;2H"); // cursor onto the continuation
    advance(&mut state, b"\x1b[P"); // delete it, pulling the line left
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    assert_eq!(glyph(&state, 0, 0), Some(' '));
}

#[test]
fn cell_ops_leave_an_untouched_wide_pair_intact() {
    let mut state = state(6, 2);
    advance(&mut state, b"\x1b[1;3H"); // cursor to col 2
    state.print('中'); // col 2 base, col 3 continuation
    advance(&mut state, b"\x1b[1;1H"); // home
    advance(&mut state, b"\x1b[1K"); // clear SOL→cursor (col 0 only) — pair untouched
    assert_eq!(glyph(&state, 0, 2), Some('中'));
    assert_eq!(state.active_grid().cell(0, 2).map(Cell::width), Some(2));
    assert_eq!(state.active_grid().cell(0, 3).map(Cell::width), Some(0));
}

// --- Multi-codepoint grapheme clusters: emoji ZWJ / VS16 / modifiers / flags ---

#[test]
fn vs16_promotes_a_text_glyph_to_a_wide_emoji_cell() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, text presentation, width 1
    state.print('\u{FE0F}'); // VS16 → emoji presentation, width 2
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{2764}');
    assert_eq!(base.combining(), ['\u{FE0F}']);
    assert_eq!(base.width(), 2); // promoted from 1 to 2
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0)); // claimed continuation
    assert_eq!(state.active_cursor().col, 2); // advanced over both columns
}

#[test]
fn zwj_emoji_sequence_folds_into_one_wide_cell() {
    let mut state = state(10, 2);
    print_str(&mut state, "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"); // 👨‍👩‍👧
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{1F468}');
    assert_eq!(
        base.combining(),
        ['\u{200D}', '\u{1F469}', '\u{200D}', '\u{1F467}']
    );
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0));
    assert_eq!(state.active_cursor().col, 2); // one glyph, not three (would be 6)
}

#[test]
fn skin_tone_modifier_folds_onto_the_base() {
    let mut state = state(6, 2);
    print_str(&mut state, "\u{1F44D}\u{1F3FD}"); // 👍 + medium skin tone
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{1F44D}');
    assert_eq!(base.combining(), ['\u{1F3FD}']);
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_cursor().col, 2);
}

#[test]
fn regional_indicator_pair_is_one_flag_cell() {
    let mut state = state(6, 2);
    print_str(&mut state, "\u{1F1EF}\u{1F1F5}"); // 🇯 + 🇵 = JP flag
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{1F1EF}');
    assert_eq!(base.combining(), ['\u{1F1F5}']);
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0));
    assert_eq!(state.active_cursor().col, 2);
}

#[test]
fn separate_emoji_without_a_joiner_stay_two_cells_each() {
    let mut state = state(10, 2);
    print_str(&mut state, "\u{1F468}\u{1F469}"); // 👨👩 — no ZWJ, two graphemes
    assert_eq!(glyph(&state, 0, 0), Some('\u{1F468}'));
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(2));
    assert!(state
        .active_grid()
        .cell(0, 0)
        .expect("in bounds")
        .combining()
        .is_empty());
    assert_eq!(glyph(&state, 0, 2), Some('\u{1F469}'));
    assert_eq!(state.active_grid().cell(0, 2).map(Cell::width), Some(2));
    assert_eq!(state.active_cursor().col, 4);
}

#[test]
fn a_control_byte_breaks_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, width 1
    advance(&mut state, b"\n"); // LF ends the run
    state.print('\u{FE0F}'); // VS16 now has no cluster to join → dropped
    let heart = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(heart.ch(), '\u{2764}');
    assert_eq!(heart.width(), 1); // NOT promoted across the control byte
    assert!(heart.combining().is_empty());
}

#[test]
fn vs16_promotion_at_the_last_column_wraps_to_the_next_line() {
    let mut state = state(3, 3); // last col = 2
    print_str(&mut state, "ab"); // a@0, b@1, cursor at col 2
    state.print('\u{2764}'); // heart width 1 at col 2, parks
    assert!(state.active_cursor().pending_wrap);
    state.print('\u{FE0F}'); // VS16 promotes → no room at the edge → move whole cluster down
    let freed = state.active_grid().cell(0, 2).expect("in bounds");
    assert_eq!(freed.ch(), ' '); // old narrow cell blanked
    let base = state.active_grid().cell(1, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{2764}');
    assert_eq!(base.combining(), ['\u{FE0F}']);
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_grid().cell(1, 1).map(Cell::width), Some(0));
    assert_eq!(
        (state.active_cursor().row, state.active_cursor().col),
        (1, 2)
    );
}

#[test]
fn vs16_promotes_the_immediately_preceding_glyph_only() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, width 1
    state.print('X'); // boundary → the heart's cluster run ends here
    state.print('\u{FE0F}'); // VS16 belongs to X's cluster, must not reach back to the heart
    let heart = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(heart.ch(), '\u{2764}');
    assert_eq!(heart.width(), 1); // untouched — not promoted
    assert!(heart.combining().is_empty());
    assert_eq!(glyph(&state, 0, 1), Some('X'));
}

#[test]
fn a_cursor_move_breaks_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, width 1
    advance(&mut state, b"\x1b[1;1H"); // CUP — any CSI ends the run
    state.print('\u{FE0F}'); // VS16 now has no cluster to join → dropped
    let heart = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(heart.width(), 1); // not promoted across the cursor move
    assert!(heart.combining().is_empty());
}

#[test]
fn a_dcs_passthrough_breaks_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, width 1
    advance(&mut state, b"\x1bPq\x1b\\"); // DCS ... ST — a non-printing control string
    state.print('\u{FE0F}'); // VS16 must NOT promote the heart across the DCS
    let heart = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(heart.ch(), '\u{2764}');
    assert_eq!(heart.width(), 1); // not promoted across the DCS
    assert!(heart.combining().is_empty());
}

#[test]
fn a_dcs_terminated_by_c1_st_breaks_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('e'); // base
    advance(&mut state, b"\x1bPq\x9c"); // DCS closed by the 8-bit C1 ST (0x9C),
                                        // whose only Perform callback is `unhook`
    state.print('\u{301}'); // combining acute must NOT fold onto 'e'
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), 'e');
    assert!(cell.combining().is_empty()); // the DCS ended the run
}

#[test]
fn an_apc_string_breaks_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('e'); // base
    advance(&mut state, b"\x1b_payload\x1b\\"); // APC ... ST — silently consumed by vte
    state.print('\u{301}'); // combining acute must NOT fold onto 'e'
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), 'e');
    assert!(cell.combining().is_empty());
}

#[test]
fn a_style_only_sgr_does_not_break_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('e'); // base at (0, 0)
    advance(&mut state, b"\x1b[31m"); // SGR set fg red — pen only, no cursor move
    state.print('\u{301}'); // combining acute must still fold onto the 'e'
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), 'e');
    assert_eq!(cell.combining(), ['\u{301}']); // attached across the SGR
    assert_eq!(cell.width(), 1);
    assert_eq!(state.active_cursor().col, 1); // no advance
}

#[test]
fn a_style_only_sgr_does_not_break_a_vs16_promotion() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, text presentation, width 1
    advance(&mut state, b"\x1b[1m"); // bold — pen only, no cursor move
    state.print('\u{FE0F}'); // VS16 must still promote the heart across the SGR
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{2764}');
    assert_eq!(base.combining(), ['\u{FE0F}']);
    assert_eq!(base.width(), 2); // promoted across the SGR
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0));
    assert_eq!(state.active_cursor().col, 2);
}

#[test]
fn an_sgr_preserved_cluster_does_not_fold_a_later_mark_onto_the_old_base() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart at (0, 0), width 1
    advance(&mut state, b"\x1b[1m"); // SGR preserves the heart's cluster run
    state.print('X'); // boundary → starts a fresh cluster at (0, 1)
    state.print('\u{FE0F}'); // VS16 belongs to X, must NOT reach back to the heart
    let heart = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(heart.width(), 1); // untouched — not promoted
    assert!(heart.combining().is_empty());
    assert_eq!(glyph(&state, 0, 1), Some('X'));
}

#[test]
fn an_overlong_ignored_sgr_shaped_csi_breaks_a_cluster_run() {
    let mut state = state(6, 2);
    state.print('e'); // base
                      // A CSI with more parameters than vte keeps (MAX_PARAMS = 32) is flagged
                      // `ignore` and dropped. It ends in `m` but is NOT a real applied SGR, so it
                      // must break the cluster like any other non-printing CSI — the SGR-preserve
                      // exception only applies to a well-formed (`!ignore`) style-only SGR.
    let mut seq = Vec::from(&b"\x1b["[..]);
    for _ in 0..40 {
        seq.extend_from_slice(b"0;"); // 40 params overflow vte's 32-param buffer
    }
    seq.push(b'm');
    advance(&mut state, &seq);
    state.print('\u{301}'); // combining acute must NOT fold onto 'e'
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), 'e');
    assert!(cell.combining().is_empty()); // the malformed CSI ended the run
}

#[test]
fn a_wrapped_vs16_promotion_clears_a_wide_glyph_it_lands_on() {
    let mut state = state(4, 3); // last col = 3
    advance(&mut state, b"\x1b[2;2H"); // cursor -> (1, 1)
    state.print('中'); // destination row: base@(1,1), continuation@(1,2)
    advance(&mut state, b"\x1b[1;1H"); // home (0, 0)
    print_str(&mut state, "xyz"); // fill row 0 cols 0..2, cursor at the last col 3
    state.print('\u{2764}'); // heart width 1 parks at (0, 3)
    assert!(state.active_cursor().pending_wrap);
    state.print('\u{FE0F}'); // VS16 promotes -> no room at the edge -> wrap the cluster to row 1
                             // The promoted pair overwrites (1,0)+(1,1); 中's base at col 1 is gone,
                             // so its old continuation at col 2 must be cleared, not left orphaned.
    let base = state.active_grid().cell(1, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{2764}');
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_grid().cell(1, 1).map(Cell::width), Some(0)); // the pair's continuation
    let orphan = state.active_grid().cell(1, 2).expect("in bounds");
    assert_eq!(orphan.ch(), ' '); // 中's stale continuation cleared
    assert_eq!(orphan.width(), 1); // not a width-0 orphan
}

#[test]
fn an_in_place_vs16_promotion_clears_a_wide_glyph_it_claims() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;2H"); // cursor -> (0, 1)
    state.print('中'); // wide glyph at cols 1-2 (base@1, continuation@2)
    advance(&mut state, b"\x1b[1;1H"); // home (0, 0)
    state.print('\u{2764}'); // heart width 1 at col 0; 中 left intact at 1-2
    state.print('\u{FE0F}'); // VS16 promotes the heart in place, claiming col 1
                             // The promotion overwrites 中's base at col 1, so 中's
                             // old continuation at col 2 must be cleared, not orphaned.
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{2764}');
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(0)); // heart's continuation
    let orphan = state.active_grid().cell(0, 2).expect("in bounds");
    assert_eq!(orphan.ch(), ' '); // 中's stale continuation cleared
    assert_eq!(orphan.width(), 1); // not a width-0 orphan
}

#[test]
fn a_wide_glyph_in_a_one_column_pane_degrades_to_a_narrow_cell() {
    let mut state = state(1, 2); // 1 column — no room for a wide pair
    state.print('中'); // cannot occupy two cells in a single-column pane
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), '中');
    assert_eq!(cell.width(), 1); // narrow, NOT a width-2 base with no continuation
}

#[test]
fn wide_glyphs_in_a_one_column_pane_do_not_scroll_thrash() {
    let mut state = state(1, 3); // 1 column, 3 rows
    state.print('中'); // stored narrow at (0, 0), no wasteful wrap
    state.print('文'); // advances one line; must not scroll the first glyph away
    assert_eq!(glyph(&state, 0, 0), Some('中')); // first glyph still on row 0
    assert_eq!(glyph(&state, 1, 0), Some('文'));
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    assert_eq!(state.active_grid().cell(1, 0).map(Cell::width), Some(1));
}

#[test]
fn a_vs16_promotion_in_a_one_column_pane_never_orphans_a_wide_base() {
    let mut state = state(1, 3); // 1 column
    state.print('\u{2764}'); // heart width 1 at (0, 0)
    state.print('\u{FE0F}'); // VS16 would promote to width 2 — but there is no room
                             // No cell anywhere may be left a width-2 base: in a
                             // 1-column pane it could never carry a continuation.
    for row in 0..3 {
        assert_ne!(
            state.active_grid().cell(row, 0).map(Cell::width),
            Some(2),
            "row {row}: a width-2 base cannot exist in a 1-column pane"
        );
    }
}

#[test]
fn combining_marks_are_capped_to_bound_per_cell_memory() {
    let mut state = state(6, 2);
    state.print('a'); // base at (0, 0)
    for _ in 0..10_000 {
        state.print('\u{0301}'); // flood of combining acutes ("zalgo")
    }
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), 'a');
    assert_eq!(cell.combining().len(), MAX_GRAPHEME_CONTINUATIONS); // bounded, not 10_000
    assert_eq!(state.active_cursor().col, 1); // never advanced
}

#[test]
fn wide_at_edge_wrap_clears_an_existing_wide_pair_it_splits() {
    let mut state = state(3, 2); // last col = 2
    state.print('x'); // col 0
    state.print('中'); // base col 1, continuation col 2 (the last column)
    advance(&mut state, b"\x1b[1;3H"); // CUP onto the continuation (0,2); clears wrap + cluster
    state.print('文'); // wide at the last col → wide-at-edge wrap; must not orphan 中's base
                       // 中's base at col 1 was orphaned by blanking col 2 → cleared.
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(1));
    assert_eq!(glyph(&state, 0, 1), Some(' '));
    assert_eq!(glyph(&state, 1, 0), Some('文')); // new glyph wrapped to the next line
}

#[test]
fn a_boundary_zero_width_char_breaks_the_cluster_run() {
    let mut state = state(6, 2);
    state.print('\u{2764}'); // heart, width 1
    state.print('\u{200B}'); // ZWSP — width 0 but a grapheme boundary; ends the run
    state.print('\u{FE0F}'); // VS16 must NOT reach back across the ZWSP to the heart
    let heart = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(heart.ch(), '\u{2764}');
    assert_eq!(heart.width(), 1); // not promoted across the boundary
    assert!(heart.combining().is_empty());
}

#[test]
fn vs15_demotes_a_wide_emoji_base_to_a_narrow_text_glyph() {
    let mut state = state(6, 2);
    state.print('\u{26A1}'); // ⚡ high voltage, default emoji presentation → width 2
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(2));
    assert_eq!(state.active_cursor().col, 2);
    state.print('\u{FE0E}'); // VS15 → text presentation → width 1
    let base = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{26A1}');
    assert_eq!(base.combining(), ['\u{FE0E}']);
    assert_eq!(base.width(), 1); // demoted from 2 to 1
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(1)); // continuation cleared
    assert_eq!(state.active_cursor().col, 1); // cursor stepped back over the freed column
    assert!(!state.active_cursor().pending_wrap);
    state.print('Z'); // next glyph lands at the freed column, not two ahead
    assert_eq!(glyph(&state, 0, 1), Some('Z'));
}

#[test]
fn vs16_promotion_wraps_correctly_in_a_two_column_grid() {
    let mut state = state(2, 2); // last col = 1 — the narrow-grid promotion edge
    state.print('a'); // col 0
    state.print('\u{2764}'); // heart width 1 at col 1 (last), parks
    assert!(state.active_cursor().pending_wrap);
    state.print('\u{FE0F}'); // VS16 promotes; no room at col 1 → move the whole cluster down
    assert_eq!(glyph(&state, 0, 1), Some(' ')); // old narrow cell blanked
    let base = state.active_grid().cell(1, 0).expect("in bounds");
    assert_eq!(base.ch(), '\u{2764}');
    assert_eq!(base.width(), 2);
    assert_eq!(state.active_grid().cell(1, 1).map(Cell::width), Some(0)); // continuation fills the row
    let cur = state.active_cursor();
    assert_eq!(cur.col, 1); // parked at the last column
    assert!(cur.pending_wrap);
}

#[test]
fn linefeed_pushes_the_top_primary_line_into_scrollback() {
    let mut state = state(4, 2); // two rows; bottom margin is row 1
    print_str(&mut state, "ab"); // row 0 = "ab.."
    state.linefeed(); // row 0 -> 1 (descends; not yet at the bottom)
    state.linefeed(); // at the bottom: the region scrolls, row 0 scrolls off
    assert_eq!(state.scrollback().len(), 1);
    let captured = state
        .scrollback()
        .lines()
        .front()
        .expect("one retained row");
    assert_eq!(captured[0].ch(), 'a');
    assert_eq!(captured[1].ch(), 'b');
}

#[test]
fn linefeed_on_the_alternate_screen_does_not_feed_scrollback() {
    let mut state = state(4, 2);
    state.active = Screen::Alternate; // full-screen apps never pollute history
    state.linefeed(); // alt cursor 0 -> 1
    state.linefeed(); // at the bottom: the alternate scrolls, but feeds nothing
    assert!(state.scrollback().is_empty());
}

#[test]
fn linefeed_below_a_top_margin_discards_rather_than_feeds() {
    let mut state = state(4, 3); // three rows
    *state.scroll_region_mut() = Some((1, 2)); // region top margin = row 1
    state.active_cursor_mut().row = 2; // park at the region's bottom margin
    state.linefeed(); // scrolls within rows 1..=2; top margin != 0 -> no feed
    assert!(state.scrollback().is_empty());
}

#[test]
fn linefeed_in_a_region_anchored_at_the_top_feeds_scrollback() {
    let mut state = state(4, 3);
    *state.scroll_region_mut() = Some((0, 1)); // region top margin = row 0
    state.active_cursor_mut().row = 1; // the region's bottom margin
    state.linefeed();
    assert_eq!(state.scrollback().len(), 1);
}

#[test]
fn successive_bottom_linefeeds_accumulate_scrollback() {
    let mut state = state(4, 2);
    state.active_cursor_mut().row = 1; // sit at the bottom row
    state.linefeed();
    state.linefeed();
    state.linefeed();
    assert_eq!(state.scrollback().len(), 3);
}

#[test]
fn su_on_a_top_anchored_region_feeds_scrollback() {
    let mut state = state(3, 2);
    print_str(&mut state, "ab"); // row 0 = "ab "
    advance(&mut state, b"\x1b[S"); // SU by 1; full region starts at row 0
    assert_eq!(state.scrollback().len(), 1);
    let captured = state
        .scrollback()
        .lines()
        .front()
        .expect("one retained row");
    assert_eq!(captured[0].ch(), 'a');
    assert_eq!(captured[1].ch(), 'b');
}

#[test]
fn su_by_n_captures_each_departing_top_row_oldest_first() {
    let mut state = state(3, 3);
    fill_3x3(&mut state); // rows "abc" / "def" / "ghi"
    advance(&mut state, b"\x1b[2S"); // SU by 2: rows 0 and 1 scroll off the top
    assert_eq!(state.scrollback().len(), 2);
    let history: Vec<String> = state
        .scrollback()
        .lines()
        .iter()
        .map(|row| row.iter().map(Cell::ch).collect())
        .collect();
    assert_eq!(history, vec!["abc", "def"]); // oldest (top) first
}

#[test]
fn su_on_a_region_below_the_top_does_not_feed() {
    let mut state = state(3, 3);
    *state.scroll_region_mut() = Some((1, 2)); // region top margin = row 1
    advance(&mut state, b"\x1b[S");
    assert!(state.scrollback().is_empty());
}

#[test]
fn su_on_the_alternate_screen_does_not_feed() {
    let mut state = state(3, 2);
    state.active = Screen::Alternate;
    advance(&mut state, b"\x1b[S");
    assert!(state.scrollback().is_empty());
}

#[test]
fn dl_with_the_cursor_on_row_0_feeds_scrollback() {
    // DL routes through the same scroll-off-top path: deleting at row 0 scrolls
    // the top line off, so it joins history (matching alacritty's origin == 0).
    let mut state = state(3, 2);
    print_str(&mut state, "ab"); // row 0 = "ab ", cursor stays on row 0
    advance(&mut state, b"\x1b[M"); // DL by 1 at the cursor row (0)
    assert_eq!(state.scrollback().len(), 1);
    let captured = state
        .scrollback()
        .lines()
        .front()
        .expect("one retained row");
    assert_eq!(captured[0].ch(), 'a');
    assert_eq!(captured[1].ch(), 'b');
}

#[test]
fn dl_below_row_0_is_an_interior_delete_and_does_not_feed() {
    let mut state = state(3, 3);
    state.active_cursor_mut().row = 1; // interior delete, nothing leaves the top
    advance(&mut state, b"\x1b[M");
    assert!(state.scrollback().is_empty());
}

// --- Bracketed paste + mouse mode state (DEC private modes) ---

#[test]
fn modes_start_at_their_defaults() {
    let state = state(5, 3);
    assert!(!state.bracketed_paste());
    assert_eq!(state.mouse_tracking(), MouseTracking::Off);
    assert_eq!(state.mouse_encoding(), MouseEncoding::Default);
    assert!(!state.alt_scroll());
}

#[test]
fn bracketed_paste_enables_and_disables() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?2004h");
    assert!(state.bracketed_paste());
    advance(&mut state, b"\x1b[?2004l");
    assert!(!state.bracketed_paste());
}

#[test]
fn each_mouse_tracking_mode_sets_its_level() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?9h");
    assert_eq!(state.mouse_tracking(), MouseTracking::X10);
    advance(&mut state, b"\x1b[?1000h");
    assert_eq!(state.mouse_tracking(), MouseTracking::Normal);
    advance(&mut state, b"\x1b[?1002h");
    assert_eq!(state.mouse_tracking(), MouseTracking::ButtonMotion);
    advance(&mut state, b"\x1b[?1003h");
    assert_eq!(state.mouse_tracking(), MouseTracking::AnyMotion);
}

#[test]
fn disabling_the_active_mouse_tracking_mode_turns_it_off() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1000h");
    advance(&mut state, b"\x1b[?1000l");
    assert_eq!(state.mouse_tracking(), MouseTracking::Off);
}

#[test]
fn a_later_mouse_tracking_mode_replaces_the_earlier_one() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1000h"); // Normal
    advance(&mut state, b"\x1b[?1003h"); // AnyMotion supersedes
    assert_eq!(state.mouse_tracking(), MouseTracking::AnyMotion);
}

#[test]
fn disabling_a_non_active_tracking_mode_leaves_the_active_one() {
    // A reset turns reporting off only when it names the active level. Resetting
    // a mode that is not the active one is a no-op, matching alacritty (whose
    // unset clears only that mode's own bit, leaving the active mode set).
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1003h"); // AnyMotion
    advance(&mut state, b"\x1b[?1000l"); // resets a different mode number
    assert_eq!(state.mouse_tracking(), MouseTracking::AnyMotion);
}

#[test]
fn disabling_the_active_tracking_mode_after_a_replace_turns_it_off() {
    // After a replace (`?1000h` then `?1003h` -> AnyMotion), resetting the now-
    // active mode (`?1003l`) turns reporting off — the superseded `?1000` is gone.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1000h"); // Normal
    advance(&mut state, b"\x1b[?1003h"); // AnyMotion supersedes
    advance(&mut state, b"\x1b[?1003l"); // reset the active mode
    assert_eq!(state.mouse_tracking(), MouseTracking::Off);
}

#[test]
fn each_mouse_encoding_mode_sets_its_form_and_resets_to_default() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1005h");
    assert_eq!(state.mouse_encoding(), MouseEncoding::Utf8);
    advance(&mut state, b"\x1b[?1006h");
    assert_eq!(state.mouse_encoding(), MouseEncoding::Sgr);
    advance(&mut state, b"\x1b[?1015h");
    assert_eq!(state.mouse_encoding(), MouseEncoding::Urxvt);
    advance(&mut state, b"\x1b[?1015l"); // reset the active encoding
    assert_eq!(state.mouse_encoding(), MouseEncoding::Default);
}

#[test]
fn disabling_a_non_active_encoding_leaves_the_active_one() {
    // A reset returns to the default only when it names the active encoding;
    // resetting a different encoding is a no-op, matching alacritty (its unset
    // clears only that encoding's own bit, leaving the active one set).
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1005h"); // Utf8 active
    advance(&mut state, b"\x1b[?1006l"); // reset a non-active encoding
    assert_eq!(state.mouse_encoding(), MouseEncoding::Utf8);
}

#[test]
fn disabling_the_active_encoding_after_a_replace_returns_to_default() {
    // After a replace (`?1005h` then `?1006h` -> Sgr), resetting the now-active
    // encoding (`?1006l`) returns to Default — the superseded Utf8 is gone.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1005h"); // Utf8
    advance(&mut state, b"\x1b[?1006h"); // Sgr supersedes
    advance(&mut state, b"\x1b[?1006l"); // reset the active encoding
    assert_eq!(state.mouse_encoding(), MouseEncoding::Default);
}

#[test]
fn mouse_tracking_and_encoding_are_independent() {
    // The orthogonal axes the two-enum model exists for: enabling SGR encoding
    // must not clear the tracking level, and vice versa.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1000h"); // tracking: Normal
    advance(&mut state, b"\x1b[?1006h"); // encoding: SGR
    assert_eq!(state.mouse_tracking(), MouseTracking::Normal);
    assert_eq!(state.mouse_encoding(), MouseEncoding::Sgr);
}

#[test]
fn one_decset_list_sets_tracking_and_encoding_together() {
    // The common real handshake — tracking and encoding in a single sequence.
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1000;1006h");
    assert_eq!(state.mouse_tracking(), MouseTracking::Normal);
    assert_eq!(state.mouse_encoding(), MouseEncoding::Sgr);
}

#[test]
fn alt_scroll_enables_and_disables() {
    let mut state = state(5, 3);
    advance(&mut state, b"\x1b[?1007h");
    assert!(state.alt_scroll());
    advance(&mut state, b"\x1b[?1007l");
    assert!(!state.alt_scroll());
}

// --- Absolute / relative cursor positioning, tab moves, erase-char ---

#[test]
fn cha_sets_an_absolute_one_based_column() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;3H"); // (2, 2)
    advance(&mut state, b"\x1b[5G"); // column 5 -> 0-based 4, row unchanged
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 4));
}

#[test]
fn cha_clamps_past_the_last_column() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99G");
    assert_eq!(state.active_cursor().col, 9); // clamped to the last column
}

#[test]
fn cha_with_no_argument_homes_the_column() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[5;5H"); // (4, 4)
    advance(&mut state, b"\x1b[G"); // default 1 -> column 0
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (4, 0));
}

#[test]
fn hpa_backtick_is_the_same_as_cha() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;3H"); // (2, 2)
    advance(&mut state, b"\x1b[5\x60"); // HPA `CSI 5 \`` -> column 4
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 4));
}

#[test]
fn cha_clears_the_pending_wrap_latch() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // parks at the last column with the latch set
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[2G"); // column 2 -> 0-based 1
    let cur = state.active_cursor();
    assert_eq!(cur.col, 1);
    assert!(!cur.pending_wrap);
}

#[test]
fn vpa_sets_an_absolute_one_based_row() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[1;4H"); // (0, 3)
    advance(&mut state, b"\x1b[3d"); // row 3 -> 0-based 2, column unchanged
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 3));
}

#[test]
fn vpa_clamps_past_the_last_row() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[99d");
    assert_eq!(state.active_cursor().row, 4); // clamped to the last row
}

#[test]
fn hpr_moves_forward_like_cuf_and_clamps() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[1;3H"); // column 2
    advance(&mut state, b"\x1b[2a"); // HPR forward 2 -> column 4
    assert_eq!(state.active_cursor().col, 4);
    advance(&mut state, b"\x1b[99a"); // clamps to the last column
    assert_eq!(state.active_cursor().col, 9);
}

#[test]
fn vpr_moves_down_like_cud_and_clamps() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[2;1H"); // row 1
    advance(&mut state, b"\x1b[2e"); // VPR down 2 -> row 3
    assert_eq!(state.active_cursor().row, 3);
    advance(&mut state, b"\x1b[99e"); // clamps to the last row
    assert_eq!(state.active_cursor().row, 4);
}

#[test]
fn cnl_moves_down_to_column_zero() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[2;4H"); // (1, 3)
    advance(&mut state, b"\x1b[1E"); // next line: down 1, column 0
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 0));
}

#[test]
fn cnl_clamps_to_the_last_row_without_scrolling() {
    let mut state = state(3, 3);
    fill_3x3(&mut state); // rows "abc" / "def" / "ghi"
    advance(&mut state, b"\x1b[3;2H"); // (2, 1) — the last row
    advance(&mut state, b"\x1b[5E"); // down 5 clamps; no scroll
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (2, 0));
    assert_eq!(glyph(&state, 0, 0), Some('a')); // content did not scroll up
}

#[test]
fn cpl_moves_up_to_column_zero() {
    let mut state = state(10, 5);
    advance(&mut state, b"\x1b[3;4H"); // (2, 3)
    advance(&mut state, b"\x1b[1F"); // previous line: up 1, column 0
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 0));
}

#[test]
fn cpl_clamps_at_row_zero_without_scrolling() {
    let mut state = state(3, 3);
    fill_3x3(&mut state);
    advance(&mut state, b"\x1b[1;2H"); // (0, 1) — the top row
    advance(&mut state, b"\x1b[5F"); // up 5 clamps; no scroll
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
    assert_eq!(glyph(&state, 2, 0), Some('g')); // content did not scroll down
}

#[test]
fn cnl_clears_the_pending_wrap_latch() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // parks with the latch set
    advance(&mut state, b"\x1b[1E");
    assert!(!state.active_cursor().pending_wrap);
}

#[test]
fn cht_advances_to_the_next_tab_stop() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[I"); // default 1 stop, from column 0 -> 8
    assert_eq!(state.active_cursor().col, 8);
}

#[test]
fn cht_from_a_tab_stop_advances_a_full_eight() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[9G"); // column 8 (a stop)
    advance(&mut state, b"\x1b[I");
    assert_eq!(state.active_cursor().col, 16);
}

#[test]
fn cht_count_advances_multiple_stops() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[2I"); // two stops from column 0 -> 8 -> 16
    assert_eq!(state.active_cursor().col, 16);
}

#[test]
fn cht_clamps_to_the_last_column() {
    let mut state = state(20, 3); // last column 19
    advance(&mut state, b"\x1b[9I"); // far more stops than fit
    assert_eq!(state.active_cursor().col, 19);
}

#[test]
fn cht_at_the_last_column_stays_put() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[20G"); // last column (19)
    advance(&mut state, b"\x1b[I");
    assert_eq!(state.active_cursor().col, 19);
}

#[test]
fn cbt_retreats_to_the_previous_tab_stop() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[11G"); // column 10
    advance(&mut state, b"\x1b[Z");
    assert_eq!(state.active_cursor().col, 8);
}

#[test]
fn cbt_from_a_tab_stop_retreats_a_full_eight() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[17G"); // column 16 (a stop)
    advance(&mut state, b"\x1b[Z");
    assert_eq!(state.active_cursor().col, 8);
}

#[test]
fn cbt_at_column_zero_stays_put() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[Z");
    assert_eq!(state.active_cursor().col, 0);
}

#[test]
fn cbt_count_retreats_multiple_stops() {
    let mut state = state(20, 3);
    advance(&mut state, b"\x1b[18G"); // column 17
    advance(&mut state, b"\x1b[2Z"); // 17 -> 16 -> 8
    assert_eq!(state.active_cursor().col, 8);
}

#[test]
fn cbt_clears_the_pending_wrap_latch() {
    let mut state = state(20, 2);
    print_str(&mut state, "aaaaaaaaaaaaaaaaaaaa"); // 20 chars -> parks at column 19
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[Z"); // 19 -> 16
    let cur = state.active_cursor();
    assert_eq!(cur.col, 16);
    assert!(!cur.pending_wrap);
}

#[test]
fn ech_erases_n_cells_in_place_without_shifting() {
    let mut state = state(6, 2);
    print_str(&mut state, "abcde");
    advance(&mut state, b"\x1b[2G"); // column 1
    advance(&mut state, b"\x1b[2X"); // erase 2 cells in place
    assert_eq!(row_text(&state, 0), "a  de "); // d, e stay put; no shift
}

#[test]
fn ech_with_no_argument_erases_one_cell() {
    let mut state = state(6, 2);
    print_str(&mut state, "abcde");
    advance(&mut state, b"\x1b[2G"); // column 1
    advance(&mut state, b"\x1b[X"); // default 1 cell
    assert_eq!(row_text(&state, 0), "a cde ");
}

#[test]
fn ech_clamps_the_count_to_the_line_end() {
    let mut state = state(5, 2);
    print_str(&mut state, "abc");
    advance(&mut state, b"\x1b[2G"); // column 1
    advance(&mut state, b"\x1b[99X"); // far past the line end -> clamps
    assert_eq!(row_text(&state, 0), "a    ");
}

#[test]
fn ech_fills_with_the_current_background_only() {
    let mut state = state(5, 2);
    advance(&mut state, b"\x1b[1;31;44m"); // bold + fg red + bg blue
    advance(&mut state, b"\x1b[3X"); // erase 3 cells from column 0
    let fill = styled(|s| s.set_bg(Color::Indexed(4))); // background only — bold + fg dropped
    assert!((0..3).all(|c| state.active_grid().cell(0, c).map(Cell::style) == Some(fill)));
}

#[test]
fn ech_leaves_the_pending_wrap_latch_set() {
    // Unlike EL 0 (which no-ops on a pending wrap), ECH erases the parked
    // last-column glyph and leaves the latch set, so the next print still wraps.
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // parks at column 2 with the latch
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[X"); // erases the parked glyph
    assert_eq!(glyph(&state, 0, 2), Some(' '));
    assert!(state.active_cursor().pending_wrap); // latch untouched
    state.print('d'); // honors the surviving latch -> wraps to the next row
    assert_eq!(glyph(&state, 1, 0), Some('d'));
}

#[test]
fn ech_repairs_a_wide_glyph_whose_base_it_erases() {
    let mut state = state(6, 2);
    state.print('中'); // wide base at column 0, continuation at column 1
    advance(&mut state, b"\x1b[1G"); // column 0 (the base)
    advance(&mut state, b"\x1b[X"); // erase the base
    assert_eq!(glyph(&state, 0, 0), Some(' '));
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    // The orphaned continuation is repaired to a blank narrow cell.
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(1));
    assert_eq!(glyph(&state, 0, 1), Some(' '));
}

#[test]
fn ech_starting_on_a_wide_continuation_repairs_the_base() {
    let mut state = state(6, 2);
    state.print('中'); // wide base at column 0, continuation at column 1
    advance(&mut state, b"\x1b[2G"); // column 1 (the continuation)
    advance(&mut state, b"\x1b[X"); // erase the continuation
                                    // The now-orphaned wide base is repaired to a blank narrow cell.
    assert_eq!(glyph(&state, 0, 0), Some(' '));
    assert_eq!(state.active_grid().cell(0, 0).map(Cell::width), Some(1));
    assert_eq!(glyph(&state, 0, 1), Some(' '));
    assert_eq!(state.active_grid().cell(0, 1).map(Cell::width), Some(1)); // pair fully unwound
}

#[test]
fn cup_clears_the_pending_wrap_latch_through_goto() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // parks with the latch set
    advance(&mut state, b"\x1b[1;1H"); // home, via the shared goto path
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
    assert!(!cur.pending_wrap);
}

#[test]
fn vpa_clears_the_pending_wrap_latch() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // parks at (0, 2) with the latch set
    advance(&mut state, b"\x1b[2d"); // row 2 -> 0-based 1, column unchanged
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (1, 2));
    assert!(!cur.pending_wrap);
}

#[test]
fn cpl_clears_the_pending_wrap_latch() {
    let mut state = state(3, 2);
    print_str(&mut state, "abc"); // parks with the latch set
    advance(&mut state, b"\x1b[1F"); // previous line clamps to row 0, column 0
    let cur = state.active_cursor();
    assert_eq!((cur.row, cur.col), (0, 0));
    assert!(!cur.pending_wrap);
}

#[test]
fn cht_clears_the_pending_wrap_latch() {
    let mut state = state(20, 2);
    print_str(&mut state, "aaaaaaaaaaaaaaaaaaaa"); // 20 chars -> parks at column 19
    assert!(state.active_cursor().pending_wrap);
    advance(&mut state, b"\x1b[I"); // already at the last column: stays, but clears the latch
    let cur = state.active_cursor();
    assert_eq!(cur.col, 19);
    assert!(!cur.pending_wrap);
}

// --- Charset designation + DEC line-drawing (G0-G3, SI/SO) ---

#[test]
fn dec_line_drawing_renders_box_glyphs() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0lqqqk"); // designate G0 = DEC line drawing, then print
    assert_eq!(glyph(&state, 0, 0), Some('┌'));
    assert_eq!(glyph(&state, 0, 1), Some('─'));
    assert_eq!(glyph(&state, 0, 2), Some('─'));
    assert_eq!(glyph(&state, 0, 3), Some('─'));
    assert_eq!(glyph(&state, 0, 4), Some('┐'));
}

#[test]
fn ascii_designation_returns_to_passthrough() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0l"); // G0 = DEC: 'l' -> box corner
    assert_eq!(glyph(&state, 0, 0), Some('┌'));
    advance(&mut state, b"\x1b(Bl"); // G0 = ASCII: 'l' -> literal
    assert_eq!(glyph(&state, 0, 1), Some('l'));
}

#[test]
fn dec_line_drawing_maps_the_full_table() {
    // The verified VT100 special-graphics table (`StandardCharset::map`): every
    // byte 0x5F-0x7E and its glyph.
    let table: &[(char, char)] = &[
        ('_', ' '),
        ('`', '◆'),
        ('a', '▒'),
        ('b', '\u{2409}'),
        ('c', '\u{240c}'),
        ('d', '\u{240d}'),
        ('e', '\u{240a}'),
        ('f', '°'),
        ('g', '±'),
        ('h', '\u{2424}'),
        ('i', '\u{240b}'),
        ('j', '┘'),
        ('k', '┐'),
        ('l', '┌'),
        ('m', '└'),
        ('n', '┼'),
        ('o', '⎺'),
        ('p', '⎻'),
        ('q', '─'),
        ('r', '⎼'),
        ('s', '⎽'),
        ('t', '├'),
        ('u', '┤'),
        ('v', '┴'),
        ('w', '┬'),
        ('x', '│'),
        ('y', '≤'),
        ('z', '≥'),
        ('{', 'π'),
        ('|', '≠'),
        ('}', '£'),
        ('~', '·'),
    ];
    for &(input, expected) in table {
        let mut state = state(4, 2);
        advance(&mut state, b"\x1b(0");
        state.print(input);
        assert_eq!(glyph(&state, 0, 0), Some(expected), "input {input:?}");
    }
}

#[test]
fn dec_line_drawing_passes_through_outside_the_mapped_range() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing
                                    // 'A' (0x41) and '0' (0x30) are below the 0x5F-0x7E table; unchanged.
    state.print('A');
    state.print('0');
    assert_eq!(glyph(&state, 0, 0), Some('A'));
    assert_eq!(glyph(&state, 0, 1), Some('0'));
}

#[test]
fn line_drawing_glyphs_are_narrow() {
    let mut state = state(4, 2);
    advance(&mut state, b"\x1b(0q"); // '─'
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.width(), 1);
}

#[test]
fn so_selects_g1_and_si_selects_g0() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b)0"); // designate G1 = DEC line drawing
    advance(&mut state, b"\x0e"); // SO -> G1 into GL
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
    advance(&mut state, b"\x0f"); // SI -> G0 (still ASCII) into GL
    state.print('q');
    assert_eq!(glyph(&state, 0, 1), Some('q'));
}

#[test]
fn charset_designation_persists_across_line_feeds() {
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing
    state.print('q'); // row 0
    advance(&mut state, b"\r\n"); // CR + LF to the next row
    state.print('q'); // row 1, charset still in effect
    assert_eq!(glyph(&state, 0, 0), Some('─'));
    assert_eq!(glyph(&state, 1, 0), Some('─'));
}

#[test]
fn uk_charset_maps_only_the_hash() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(A"); // G0 = UK
    state.print('#');
    state.print('a');
    assert_eq!(glyph(&state, 0, 0), Some('£'));
    assert_eq!(glyph(&state, 0, 1), Some('a'));
}

#[test]
fn unknown_charset_final_falls_back_to_ascii() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing
    advance(&mut state, b"\x1b(>"); // unsupported final -> ASCII passthrough
    assert_eq!(state.active_render().charsets[0], Charset::Ascii);
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q'));
}

#[test]
fn g2_and_g3_are_designated_but_not_selectable() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b*0"); // designate G2 = DEC line drawing
    advance(&mut state, b"\x1b+0"); // designate G3 = DEC line drawing
    assert_eq!(state.active_render().charsets[2], Charset::DecLineDrawing);
    assert_eq!(state.active_render().charsets[3], Charset::DecLineDrawing);
    // No LS2/LS3, so GL stays on G0 (ASCII): printing is unaffected.
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q'));
}

#[test]
fn charset_is_carried_into_the_alternate_screen() {
    // Designations are shared global rendering state (like the pen and GL slot),
    // so entering the alternate by ANY route keeps them: a child that did
    // `ESC ( 0` keeps drawing line-drawing glyphs after `?47h`.
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing
    advance(&mut state, b"\x1b[?47h"); // switch to the alternate
    state.print('q'); // shared G0 still DEC -> box glyph, not literal 'q'
    assert_eq!(glyph(&state, 0, 0), Some('─'));
    advance(&mut state, b"\x1b[?47l"); // back to the primary
    state.print('q'); // still DEC
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn decsc_and_decrc_save_and_restore_the_charset() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing
    advance(&mut state, b"\x1b7"); // DECSC: save cursor + charset
    advance(&mut state, b"\x1b(B"); // G0 = ASCII
    state.print('q'); // literal 'q' at (0, 0)
    assert_eq!(glyph(&state, 0, 0), Some('q'));
    advance(&mut state, b"\x1b8"); // DECRC: restore charset (and home the cursor)
    state.print('q'); // DEC again -> box glyph at (0, 0)
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn decrc_without_a_save_resets_the_charset_to_ascii() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing, never saved
    advance(&mut state, b"\x1b8"); // DECRC with no prior DECSC -> defaults
    assert_eq!(state.active_render().charsets[0], Charset::Ascii);
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q'));
}

#[test]
fn dec_1049_entry_inherits_the_primary_charset() {
    // Charset designations are global rendering state (like the pen): entering
    // the alternate via `?1049h` inherits the primary's, so an app that did
    // `ESC ( 0` then entered keeps drawing line-drawing glyphs, not ASCII.
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b(0"); // primary G0 = DEC line drawing
    advance(&mut state, b"\x1b[?1049h"); // enter alt: inherit the primary's designations
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn dec_1049_entry_does_not_leak_a_prior_alternate_charset() {
    // Seeding from the *primary* overwrites any designations a previous
    // alternate session left, so re-entry never resurrects stale charset state.
    let mut state = state(8, 3);
    // Primary stays ASCII throughout.
    advance(&mut state, b"\x1b[?1049h"); // enter alt (inherits ASCII)
    advance(&mut state, b"\x1b(0"); // this alt session designates G0 = DEC
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
    advance(&mut state, b"\x1b[?1049l"); // exit
    advance(&mut state, b"\x1b[?1049h"); // re-enter: seed from primary (ASCII) wipes the stale DEC
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q'));
}

#[test]
fn an_alternate_designation_does_not_leak_to_the_primary() {
    // Render state is per-screen: a designation a full-screen app makes on the
    // alternate must NOT corrupt the user's shell on the primary after it exits.
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b[?1047h"); // enter the alternate (clones primary's ASCII)
    advance(&mut state, b"\x1b(0"); // alt designates G0 = DEC line drawing
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─')); // alt draws box glyphs
    advance(&mut state, b"\x1b[?1047l"); // exit to the primary
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q')); // primary UNAFFECTED — no leak
}

#[test]
fn dec_1049_exit_restores_the_charset_via_decrc() {
    // `?1049 l` restores the cursor as in DECRC, which carries the saved charset
    // back — so a designation made on the alternate is undone on exit, leaving
    // the primary's set in effect (here ASCII).
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b[?1049h"); // save primary (ASCII), enter alt
    advance(&mut state, b"\x1b(0"); // alt designates G0 = DEC
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
    advance(&mut state, b"\x1b[?1049l"); // DECRC restore -> charset back to the primary's ASCII
    advance(&mut state, b"\x1b[?1047h"); // a non-restoring entry observes the restored ASCII
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q'));
}

#[test]
fn the_alternate_render_is_recloned_from_primary_on_each_entry() {
    // Every alternate entry clones the primary's render state, so a designation
    // the alternate made in a prior session is discarded on re-entry (the
    // alternate resumes its BUFFER, but its render is re-inherited from primary).
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b[?47h"); // enter (clone primary's ASCII)
    advance(&mut state, b"\x1b(0"); // alt G0 = DEC line drawing
    advance(&mut state, b"\x1b[?47l"); // exit (primary unaffected)
    advance(&mut state, b"\x1b[?47h"); // re-enter -> re-clone primary's ASCII
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q')); // alt's prior DEC was discarded
}

#[test]
fn decsc_and_decrc_save_and_restore_the_active_gl_slot() {
    // xterm stores `curgl` in its SavedCursor, so a save/restore must carry
    // *which* set is invoked into GL, not only the G0-G3 table contents.
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b)0"); // designate G1 = DEC line drawing
    advance(&mut state, b"\x0e"); // SO -> GL = G1
    advance(&mut state, b"\x1b7"); // DECSC: save cursor, charsets, AND the GL slot
    advance(&mut state, b"\x0f"); // SI -> GL = G0 (ASCII)
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('q')); // G0 ASCII -> literal
    advance(&mut state, b"\x1b8"); // DECRC: GL restored to G1 (and cursor home to the saved (0,0))
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─')); // G1 line drawing again
}

#[test]
fn scosc_and_scorc_save_and_restore_the_active_gl_slot() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b)0"); // G1 = DEC line drawing
    advance(&mut state, b"\x0e"); // SO -> GL = G1
    advance(&mut state, b"\x1b[s"); // SCOSC: save (ANSI.SYS form of DECSC)
    advance(&mut state, b"\x0f"); // SI -> GL = G0
    advance(&mut state, b"\x1b[u"); // SCORC: GL restored to G1
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn decrc_without_a_save_resets_the_gl_slot_to_g0() {
    let mut state = state(8, 2);
    advance(&mut state, b"\x0e"); // SO -> GL = G1
    advance(&mut state, b"\x1b8"); // DECRC with no prior save -> GL back to G0
    assert_eq!(state.active_render().gl, 0);
}

#[test]
fn the_gl_slot_is_carried_into_the_alternate() {
    // The GL selection is part of the render state, so a `?47` entry clones the
    // primary's into the alternate (the alternate inherits GL = G1).
    let mut state = state(8, 3);
    advance(&mut state, b"\x0e"); // SO -> GL = G1 on the primary
    advance(&mut state, b"\x1b[?47h"); // enter the alternate (clones the primary's render)
    assert_eq!(state.active_render().gl, 1); // alternate inherited GL = G1
    advance(&mut state, b"\x1b[?47l"); // back to the primary (its GL is its own)
    assert_eq!(state.active_render().gl, 1);
}

#[test]
fn an_alternate_pen_change_does_not_leak_to_the_primary() {
    // The pen is per-screen render state too: colors set by a full-screen app on
    // the alternate must not bleed onto the primary shell after it exits — but
    // the alternate does inherit the primary's pen on entry.
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b[31m"); // primary pen: red fg
    advance(&mut state, b"\x1b[?1047h"); // enter the alternate (inherits red)
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Indexed(1)))
    );
    advance(&mut state, b"\x1b[32m"); // alt changes pen to green fg
    advance(&mut state, b"\x1b[?1047l"); // exit to the primary
    assert_eq!(
        state.active_render().style,
        styled(|s| s.set_fg(Color::Indexed(1))) // primary still red — green did not leak
    );
}

#[test]
fn a_resize_on_the_alternate_keeps_the_primary_background() {
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b[?1047h"); // enter the alternate
    advance(&mut state, b"\x1b[44m"); // alternate sets a blue background
    state.resize(PtySize { cols: 4, rows: 2 }); // resize while on the alternate
    advance(&mut state, b"\x1b[?1047l"); // exit to the primary
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.style(), Style::default()); // primary blanks stayed default, not blue
}

#[test]
fn dec_1049_round_trip_restores_the_gl_slot() {
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b)0"); // primary G1 = DEC line drawing
    advance(&mut state, b"\x0e"); // SO -> GL = G1
    advance(&mut state, b"\x1b[?1049h"); // enter alt: saves the primary cursor incl GL = G1
    advance(&mut state, b"\x0f"); // change GL = G0 while on the alternate
    advance(&mut state, b"\x1b[?1049l"); // exit: restores the primary cursor, GL back to G1
    state.print('q'); // primary G1 line drawing still selected
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn mixed_mode_47_then_1049_keeps_the_charset() {
    // `CSI ? 47 ; 1049 h`: neither mode touches the shared charset, so the
    // designation survives the mixed-mode entry regardless of the buffer flips.
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b(0"); // G0 = DEC line drawing
    advance(&mut state, b"\x1b[?47;1049h"); // ?47h flips active, ?1049h saves/switches/clears cells
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─')); // shared charset intact
}

#[test]
fn decsc_decrc_round_trips_the_charset_on_the_alternate_screen() {
    // The per-screen saved slot carries charsets on the alternate too, not only
    // the primary.
    let mut state = state(8, 3);
    advance(&mut state, b"\x1b[?1049h"); // enter alt
    advance(&mut state, b"\x1b(0"); // alt G0 = DEC
    advance(&mut state, b"\x1b7"); // DECSC on the alt: saves G0 = DEC
    advance(&mut state, b"\x1b(B"); // alt G0 = ASCII
    advance(&mut state, b"\x1b8"); // DECRC on the alt: restores G0 = DEC
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn dec_1048_saves_and_restores_the_charset() {
    // `?1048` is "save/restore cursor as in DECSC/DECRC" — it must carry charsets.
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // G0 = DEC
    advance(&mut state, b"\x1b[?1048h"); // save cursor (incl charsets)
    advance(&mut state, b"\x1b(B"); // G0 = ASCII
    advance(&mut state, b"\x1b[?1048l"); // restore -> G0 = DEC
    state.print('q');
    assert_eq!(glyph(&state, 0, 0), Some('─'));
}

#[test]
fn dec_line_drawing_survives_a_deferred_wrap() {
    // The remap runs at the top of `print`, before the wrap/width logic, so a
    // line-drawing row wraps exactly like an ASCII one (every glyph is narrow).
    let mut state = state(3, 2);
    advance(&mut state, b"\x1b(0"); // GL = DEC line drawing
    advance(&mut state, b"qqq"); // fills row 0 with ───, parks at the last column
    assert_eq!(glyph(&state, 0, 0), Some('─'));
    assert_eq!(glyph(&state, 0, 2), Some('─'));
    assert!(state.active_cursor().pending_wrap);
    state.print('q'); // forces the deferred wrap onto row 1
    assert_eq!(glyph(&state, 1, 0), Some('─'));
    assert_eq!(state.active_cursor().row, 1);
}

#[test]
fn dec_line_drawing_passes_multibyte_utf8_through() {
    // The table only remaps the ASCII range 0x5F-0x7E; a real Unicode glyph
    // (vte has already decoded the UTF-8) is printed unchanged.
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // GL = DEC line drawing
    advance(&mut state, "é".as_bytes()); // multibyte, outside the table
    assert_eq!(glyph(&state, 0, 0), Some('é'));
}

#[test]
fn a_combining_mark_folds_onto_a_line_drawing_glyph() {
    // The remapped glyph anchors the grapheme cluster, so a following combining
    // mark folds onto it (one cell) rather than taking its own — the remap does
    // not disturb the cluster machinery.
    let mut state = state(8, 2);
    advance(&mut state, b"\x1b(0"); // GL = DEC line drawing
    state.print('q'); // '─' base at (0, 0)
    state.print('\u{0301}'); // combining acute: folds onto the base, no new cell
    let cell = state.active_grid().cell(0, 0).expect("in bounds");
    assert_eq!(cell.ch(), '─');
    assert_eq!(cell.combining(), &['\u{0301}']);
    assert_eq!(state.active_cursor().col, 1); // cursor did not advance a 2nd column
}
