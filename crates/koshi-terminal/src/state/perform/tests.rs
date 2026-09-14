//! Unit tests for the VTE performer: printing and display width (wide glyphs,
//! grapheme clusters, ambiguous width), the deferred wrap, C0/C1 control bytes,
//! CSI cursor / erase / line / cell sequences, SGR, the alternate screen and
//! DEC private modes, OSC title / reported_working_directory / shell markers, charset designation,
//! DECSCUSR, the soft and hard resets, and malformed or hostile input.

use super::glyph::MAX_GRAPHEME_CONTINUATION_COUNT;
use super::*;
use crate::graphics::{DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord};
use crate::grid::state::RowMetadata;
use crate::scrollback::{Scrollback, ScrollbackLimit};
use crate::state::{Charset, RenderState, TerminalModes};
use crate::style::{Color, Style, UnderlineStyle};
use koshi_core::process::PtySize;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use vte::Perform;

/// Build a per-pane terminal state of `column_count × row_count`.
fn build_terminal_state(column_count: u16, row_count: u16) -> TerminalState {
    TerminalState::from_pty_size(PtySize {
        column_count,
        row_count,
    })
}

/// Print every character of `text` through the performer.
fn print_text(terminal_state: &mut TerminalState, text: &str) {
    for character in text.chars() {
        terminal_state.print(character);
    }
}

/// The character at `(row_index, column_index)` of the active grid.
fn get_terminal_glyph(
    terminal_state: &TerminalState,
    row_index: u16,
    column_index: u16,
) -> Option<char> {
    terminal_state
        .get_active_grid()
        .get_cell(row_index, column_index)
        .map(Cell::get_character)
}

fn build_image_record(
    image_anchor: (u16, u16),
    image_column_count: u32,
    image_row_count: u32,
) -> ImageRecord {
    ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: image_column_count,
            pixel_height: image_row_count,
            rgba_bytes: vec![255; (image_column_count * image_row_count * 4) as usize],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            requested_column_count: Some(image_column_count),
            requested_row_count: Some(image_row_count),
            should_move_cursor: false,
            ..ImageDisplay::default()
        },
        anchor: image_anchor,
    }
}

#[test]
fn print_writes_the_glyph_at_the_cursor_and_advances() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('a');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    let cursor = terminal_state.active_cursor();
    assert_eq!((cursor.row, cursor.column), (0, 1));
    assert!(!cursor.pending_wrap);
}

#[test]
fn print_lays_a_string_left_to_right() {
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "hi");
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('h'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('i'));
    assert_eq!(terminal_state.active_cursor().column, 2);
}

#[test]
fn print_stamps_the_pen_style_with_width_one() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('a');
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_display_width(), 1);
    assert_eq!(cell.get_style(), terminal_state.active_render().style);
}

#[test]
fn print_at_the_last_column_parks_without_moving() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // fills row 0 exactly
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('c'));
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 2)); // cursor stays
    assert!(terminal_state.active_cursor().pending_wrap);
}

#[test]
fn exact_width_line_does_not_scroll_until_the_next_get_terminal_glyph() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // row 0 full, parked
    terminal_state.print('d'); // forces the deferred wrap
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a')); // row 0 untouched, no early scroll
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('d')); // wrapped onto row 1
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 1));
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn deferred_wrap_on_the_bottom_row_scrolls() {
    let mut terminal_state = build_terminal_state(2, 2);
    print_text(&mut terminal_state, "abcde"); // a,b | c,d then 'e' wraps off the bottom
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('c')); // old bottom row rose
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('d'));
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('e')); // 'e' on the fresh bottom row
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 1), Some(' '));
    assert_eq!(terminal_state.active_cursor().row, 1);
}

#[test]
fn newline_moves_down_and_leaves_the_column() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('a'); // cursor at column 1
    terminal_state.execute(b'\n');
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 1));
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn vertical_tab_and_form_feed_behave_like_newline() {
    for byte in [0x0Bu8, 0x0C] {
        let mut terminal_state = build_terminal_state(2, 3);
        print_text(&mut terminal_state, "ab"); // parks at (0, 1) with the wrap latch armed
        terminal_state.execute(byte);
        let cursor_position = terminal_state.active_cursor();
        assert_eq!(
            (cursor_position.row, cursor_position.column),
            (1, 1),
            "byte {byte:#x} should line-feed"
        );
        assert!(
            !cursor_position.pending_wrap,
            "byte {byte:#x} should clear the latch"
        );
    }
}

#[test]
fn newline_on_the_bottom_row_scrolls() {
    let mut terminal_state = build_terminal_state(3, 2);
    terminal_state.print('a'); // (0,0)
    terminal_state.execute(b'\n'); // to row 1
    terminal_state.execute(b'\r'); // column 0
    terminal_state.print('z'); // (1,0)
    terminal_state.execute(b'\n'); // bottom row -> scroll
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('z')); // row 1 rose to row 0
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // fresh blank bottom
    assert_eq!(terminal_state.active_cursor().row, 1); // cursor pinned to the last row
}

#[test]
fn carriage_return_returns_to_column_zero() {
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "ab");
    terminal_state.execute(b'\r');
    assert_eq!(terminal_state.active_cursor().column, 0);
}

#[test]
fn backspace_steps_back_one_column_and_floors_at_zero() {
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "ab"); // column 2
    terminal_state.execute(0x08);
    assert_eq!(terminal_state.active_cursor().column, 1);
    terminal_state.execute(0x08);
    terminal_state.execute(0x08); // already at 0, saturates
    assert_eq!(terminal_state.active_cursor().column, 0);
}

#[test]
fn tab_advances_to_each_eight_column_stop() {
    let mut terminal_state = build_terminal_state(20, 1);
    terminal_state.execute(b'\t');
    assert_eq!(terminal_state.active_cursor().column, 8);
    terminal_state.execute(b'\t');
    assert_eq!(terminal_state.active_cursor().column, 16);
}

#[test]
fn tab_from_mid_stop_lands_on_the_next_stop() {
    let mut terminal_state = build_terminal_state(20, 1);
    print_text(&mut terminal_state, "abc"); // column 3
    terminal_state.execute(b'\t');
    assert_eq!(terminal_state.active_cursor().column, 8);
}

#[test]
fn tab_clamps_to_the_last_column() {
    let mut terminal_state = build_terminal_state(6, 1); // last column is 5
    terminal_state.execute(b'\t');
    assert_eq!(terminal_state.active_cursor().column, 5);
}

#[test]
fn bell_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "a"); // column 1
    terminal_state.execute(0x07);
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 1));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
}

#[test]
fn unknown_control_byte_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "a");
    terminal_state.execute(0x01); // SOH — unhandled
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 1));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
}

#[test]
fn a_cursor_move_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(2, 2);
    print_text(&mut terminal_state, "ab"); // parked on the last column
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.execute(b'\r'); // any cursor move clears the latch
    assert!(!terminal_state.active_cursor().pending_wrap);
    terminal_state.print('c'); // must overwrite in place, not wrap to a new line
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('c'));
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 1));
}

#[test]
fn driven_through_the_parser_plain_text_lands_in_the_grid() {
    let mut terminal_state = build_terminal_state(10, 2);
    process_terminal_bytes(&mut terminal_state, b"h\xc3\xa9llo"); // "héllo" — é is multi-byte UTF-8
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('h'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('é'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('l'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('o'));
    assert_eq!(terminal_state.active_cursor().column, 5);
}

#[test]
fn driven_through_the_parser_newline_and_carriage_return() {
    let mut terminal_state = build_terminal_state(10, 3);
    process_terminal_bytes(&mut terminal_state, b"ab\r\ncd");
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('b'));
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('c'));
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 1), Some('d'));
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 2));
}

// --- CSI cursor + erase (driven through the parser) ---

/// Feed `input_bytes` through a fresh parser into `terminal_state`.
fn process_terminal_bytes(terminal_state: &mut TerminalState, input_bytes: &[u8]) {
    let mut parser = vte::Parser::<{ crate::engine::OSC_BUFFER_BYTE_CAPACITY }>::new_with_size();
    parser.advance(terminal_state, input_bytes);
}

/// Row `row_index` of the active grid as a string; blank cells read as spaces.
fn get_row_text(terminal_state: &TerminalState, row_index: u16) -> String {
    let (_, column_count) = terminal_state.get_active_grid().get_grid_dimensions();
    (0..column_count)
        .map(|column_index| {
            get_terminal_glyph(terminal_state, row_index, column_index).unwrap_or(' ')
        })
        .collect()
}

/// Fill a 3×3 grid with rows `"abc"`, `"def"`, `"ghi"`.
fn fill_three_by_three_grid(terminal_state: &mut TerminalState) {
    process_terminal_bytes(terminal_state, b"abc\r\ndef\r\nghi");
}

#[test]
fn cup_sets_an_absolute_one_based_position() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3H");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 2)); // 2;3 -> 0-based
}

#[test]
fn cup_with_no_arguments_homes_the_cursor() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;4H"); // move away first
    process_terminal_bytes(&mut terminal_state, b"\x1b[H");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
}

#[test]
fn cup_zero_arguments_are_treated_as_one() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[0;0H");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
}

#[test]
fn cup_clamps_out_of_range_arguments_to_the_grid_edges() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99;99H");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (4, 9)); // last row, last column
}

#[test]
fn hvp_positions_like_cup() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4f");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 3));
}

#[test]
fn cuu_moves_up_by_the_count() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;4H"); // (3, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2A");
    assert_eq!(terminal_state.active_cursor().row, 1);
}

#[test]
fn cud_moves_down_and_clamps_to_the_last_row() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99B");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.row, 4);
}

#[test]
fn cuf_moves_forward_and_clamps_to_the_last_column() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99C");
    assert_eq!(terminal_state.active_cursor().column, 9);
}

#[test]
fn cub_moves_back_and_floors_at_column_zero() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;4H"); // column 3
    process_terminal_bytes(&mut terminal_state, b"\x1b[5D");
    assert_eq!(terminal_state.active_cursor().column, 0);
}

#[test]
fn a_missing_or_zero_move_count_defaults_to_one() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[A"); // no argument -> up one
    assert_eq!(terminal_state.active_cursor().row, 1);
    process_terminal_bytes(&mut terminal_state, b"\x1b[0A"); // explicit zero -> up one
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.row, 0);
}

#[test]
fn a_csi_cursor_move_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(2, 2);
    print_text(&mut terminal_state, "ab"); // parked on the last column
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[C"); // CUF clears the latch
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn a_private_mode_sequence_is_ignored_not_treated_as_erase() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde"); // fills row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[?2J"); // `?` -> private mode, not ED 2
    assert_eq!(get_row_text(&terminal_state, 0), "abcde"); // untouched
}

#[test]
fn el_0_erases_from_the_cursor_to_the_end_of_the_line() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // row 0, column 2
    process_terminal_bytes(&mut terminal_state, b"\x1b[K"); // EL 0
    assert_eq!(get_row_text(&terminal_state, 0), "ab   ");
}

#[test]
fn el_0_erases_the_parked_last_column_glyph_and_clears_the_wrap_latch() {
    // Filling the last column parks the cursor there with a wrap pending. EL 0
    // (cursor-to-end) erases from that column — the parked glyph included — and
    // clears the wrap latch. The next print overwrites at the last column.
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde"); // 'e' lands at column 4 with the wrap pending
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[K"); // EL 0
    assert_eq!(get_row_text(&terminal_state, 0), "abcd "); // 'e' erased
    assert!(!terminal_state.active_cursor().pending_wrap); // latch cleared
    terminal_state.print('f');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('f')); // overwrote at the last column
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // did NOT wrap
}

#[test]
fn el_0_erases_the_last_column_when_autowrap_is_off() {
    // With autowrap off, the cursor sits on the last column. EL 0 erases from
    // that column, the cursor's own cell included.
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l"); // autowrap off
    print_text(&mut terminal_state, "abcde"); // fills the row; cursor parked on column 4
    process_terminal_bytes(&mut terminal_state, b"\x1b[K"); // EL 0
    assert_eq!(get_row_text(&terminal_state, 0), "abcd "); // last column erased, rest intact
}

#[test]
fn el_1_and_el_2_clear_the_wrap_latch_when_parked() {
    // EL 1 and EL 2 both wipe the parked cursor's cell, clearing the line.
    // This clears the wrap latch; the next print overwrites in place.
    for erase_sequence_bytes in [&b"\x1b[1K"[..], &b"\x1b[2K"[..]] {
        let mut terminal_state = build_terminal_state(5, 2);
        print_text(&mut terminal_state, "abcde"); // parks at column 4 with the latch
        assert!(terminal_state.active_cursor().pending_wrap);
        process_terminal_bytes(&mut terminal_state, erase_sequence_bytes);
        assert!(!terminal_state.active_cursor().pending_wrap);
    }
}

#[test]
fn ed_clears_the_wrap_latch_for_erasing_modes_but_not_ed_3() {
    // ED 0/1/2 wipe the cursor's cell → clear the latch; ED 3 (scrollback only)
    // leaves the visible grid and the latch untouched.
    for erase_sequence_bytes in [&b"\x1b[J"[..], &b"\x1b[1J"[..], &b"\x1b[2J"[..]] {
        let mut terminal_state = build_terminal_state(5, 2);
        print_text(&mut terminal_state, "abcde"); // parks at column 4
        assert!(terminal_state.active_cursor().pending_wrap);
        process_terminal_bytes(&mut terminal_state, erase_sequence_bytes);
        assert!(!terminal_state.active_cursor().pending_wrap);
    }
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[3J"); // scrollback only — visible grid untouched
    assert!(terminal_state.active_cursor().pending_wrap); // latch survives
}

#[test]
fn el_1_erases_from_the_start_through_the_cursor() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // column 2
    process_terminal_bytes(&mut terminal_state, b"\x1b[1K"); // EL 1 — cursor column inclusive
    assert_eq!(get_row_text(&terminal_state, 0), "   de");
}

#[test]
fn el_2_erases_the_whole_line() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2K");
    assert_eq!(get_row_text(&terminal_state, 0), "     ");
}

#[test]
fn ed_0_erases_from_the_cursor_to_the_end_of_the_screen() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // (1, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[J"); // ED 0
    assert_eq!(get_row_text(&terminal_state, 0), "abc"); // above kept
    assert_eq!(get_row_text(&terminal_state, 1), "d  "); // cursor column onward cleared
    assert_eq!(get_row_text(&terminal_state, 2), "   "); // row below cleared
}

#[test]
fn ed_1_erases_from_the_start_of_the_screen_through_the_cursor() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // (1, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1J"); // ED 1
    assert_eq!(get_row_text(&terminal_state, 0), "   "); // row above cleared
    assert_eq!(get_row_text(&terminal_state, 1), "  f"); // start through cursor cleared
    assert_eq!(get_row_text(&terminal_state, 2), "ghi"); // below kept
}

#[test]
fn ed_2_erases_the_whole_screen() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2J");
    assert_eq!(get_row_text(&terminal_state, 0), "   ");
    assert_eq!(get_row_text(&terminal_state, 1), "   ");
    assert_eq!(get_row_text(&terminal_state, 2), "   ");
}

#[test]
fn glyph_writes_preserve_overlapped_kitty_placements() {
    let mut terminal_state = build_terminal_state(8, 6);
    let image_record = build_image_record((2, 2), 2, 2);
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");

    let expected_placements = terminal_state.list_image_placements().to_vec();

    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1HA\x1b[3;3HB\x1b[5;5HC");
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('A'));
    assert_eq!(get_terminal_glyph(&terminal_state, 2, 2), Some('B'));
    assert_eq!(get_terminal_glyph(&terminal_state, 4, 4), Some('C'));
    assert_eq!(terminal_state.list_image_placements(), expected_placements);
}

#[test]
fn cell_operations_and_glyph_writes_preserve_kitty_placements() {
    let mut terminal_state = build_terminal_state(8, 6);
    let image_record = build_image_record((2, 2), 2, 2);
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");
    let expected_placements = terminal_state.list_image_placements().to_vec();

    for terminal_sequence_bytes in [
        &b"\x1b[6;8H"[..],
        &b"\x1b[3;3H\x1b[K"[..],
        &b"\x1b[3;3H\x1b[2X"[..],
        &b"\x1b[3;3H\x1b[1J"[..],
        &b"\x1b[3;3H\x1b[1@"[..],
        &b"\x1b[3;3H\x1b[1P"[..],
    ] {
        process_terminal_bytes(&mut terminal_state, terminal_sequence_bytes);
        assert_eq!(
            terminal_state.list_image_placements(),
            expected_placements.as_slice(),
            "ordinary operation changed image metadata: {terminal_sequence_bytes:?}"
        );
    }

    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3HB");
    assert_eq!(terminal_state.list_image_placements(), expected_placements);
}

#[test]
fn line_operations_move_or_drop_image_placements_with_their_rows() {
    let mut terminal_state = build_terminal_state(8, 6);
    let image_record = build_image_record((2, 2), 1, 1);
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");

    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[L");
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (3, 2)
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[M");
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (2, 2)
    );

    let bottom_image_record = build_image_record((5, 2), 1, 1);
    terminal_state
        .apply_image_record(&bottom_image_record)
        .expect("the second image fits the grid");
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[L");
    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (3, 2)
    );
}

#[test]
fn reverse_index_moves_image_placements_with_inserted_rows() {
    let mut terminal_state = build_terminal_state(8, 6);
    let image_record = build_image_record((0, 2), 1, 1);
    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");

    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H\x1bM");

    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (1, 2)
    );
}

#[test]
fn alternate_line_operations_move_or_drop_image_placements() {
    let mut terminal_state = build_terminal_state(8, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    let image_record = build_image_record((2, 2), 1, 1);
    terminal_state
        .apply_image_record(&image_record)
        .expect("the alternate image fits the grid");

    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[L");
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (3, 2)
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[M");
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (2, 2)
    );

    let bottom_image_record = build_image_record((5, 2), 1, 1);
    terminal_state
        .apply_image_record(&bottom_image_record)
        .expect("the second alternate image fits the grid");
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[L");
    assert_eq!(terminal_state.list_image_placements().len(), 1);
    assert_eq!(
        terminal_state.list_image_placements()[0].get_image_anchor(),
        (3, 2)
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert!(terminal_state.list_image_placements().is_empty());
}

#[test]
fn line_operations_drop_images_when_a_one_row_screen_has_no_survivor() {
    let mut insertion_terminal_state = build_terminal_state(8, 1);
    let image_record = build_image_record((0, 2), 1, 1);
    insertion_terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the one-row grid");
    process_terminal_bytes(&mut insertion_terminal_state, b"\x1b[1;1H\x1b[L");
    assert!(insertion_terminal_state.list_image_placements().is_empty());

    let mut deletion_terminal_state = build_terminal_state(8, 1);
    deletion_terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the one-row grid");
    process_terminal_bytes(&mut deletion_terminal_state, b"\x1b[1;1H\x1b[M");
    assert!(deletion_terminal_state.list_image_placements().is_empty());
}

#[test]
fn image_placement_cursor_motion_uses_accepted_cell_dimensions() {
    let mut terminal_state = build_terminal_state(10, 8);
    terminal_state.active_cursor_mut().row = 2;
    terminal_state.active_cursor_mut().column = 3;
    terminal_state.active_cursor_mut().pending_wrap = true;
    let mut image_record = build_image_record((2, 3), 3, 2);
    image_record.display.should_move_cursor = true;

    terminal_state
        .apply_image_record(&image_record)
        .expect("the image fits the grid");

    assert_eq!(terminal_state.get_active_cursor_position(), (4, 6));
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn image_placement_screen_state_is_isolated_and_reset_with_its_screen() {
    let mut terminal_state = build_terminal_state(8, 6);
    let primary_image_record = build_image_record((0, 0), 1, 1);
    let alternate_image_record = build_image_record((1, 1), 1, 1);
    terminal_state
        .apply_image_record(&primary_image_record)
        .expect("the primary image fits");
    let primary_placements = terminal_state.list_image_placements().to_vec();

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    assert_eq!(terminal_state.list_image_placements(), &[]);
    terminal_state
        .apply_image_record(&alternate_image_record)
        .expect("the alternate image fits");
    let alternate_placements = terminal_state.list_image_placements().to_vec();
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert_eq!(
        terminal_state.list_image_placements(),
        primary_placements.as_slice()
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    assert_eq!(
        terminal_state.list_image_placements(),
        alternate_placements.as_slice()
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l");
    assert_eq!(
        terminal_state.list_image_placements(),
        primary_placements.as_slice()
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    assert_eq!(terminal_state.list_image_placements(), &[]);
    terminal_state
        .apply_image_record(&alternate_image_record)
        .expect("the alternate image fits after a reset");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h");
    assert_eq!(terminal_state.list_image_placements(), &[]);
    terminal_state
        .apply_image_record(&alternate_image_record)
        .expect("the alternate image fits after 1049 entry");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l");
    assert_eq!(
        terminal_state.list_image_placements(),
        primary_placements.as_slice()
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    terminal_state
        .apply_image_record(&alternate_image_record)
        .expect("the alternate image fits before a hard reset");
    process_terminal_bytes(&mut terminal_state, b"\x1bc");
    assert_eq!(terminal_state.list_image_placements(), &[]);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    assert_eq!(terminal_state.list_image_placements(), &[]);
}

#[test]
fn ed_2_clears_only_the_active_screen_images() {
    let mut terminal_state = build_terminal_state(8, 6);
    let primary_image_record = build_image_record((0, 0), 1, 1);
    let alternate_image_record = build_image_record((1, 1), 1, 1);
    terminal_state
        .apply_image_record(&primary_image_record)
        .expect("the primary image fits");
    let primary_placements = terminal_state.list_image_placements().to_vec();

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    terminal_state
        .apply_image_record(&alternate_image_record)
        .expect("the alternate image fits");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2J");
    assert_eq!(terminal_state.list_image_placements(), &[]);

    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert_eq!(
        terminal_state.list_image_placements(),
        primary_placements.as_slice()
    );
}

#[test]
fn ed_3_leaves_the_visible_screen_untouched() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3J"); // erase scrollback only — visible screen intact
    assert_eq!(get_row_text(&terminal_state, 0), "abc");
    assert_eq!(get_row_text(&terminal_state, 1), "def");
    assert_eq!(get_row_text(&terminal_state, 2), "ghi");
}

#[test]
fn ed_3_clears_the_retained_scrollback() {
    let mut terminal_state = build_terminal_state(3, 2); // two rows
    terminal_state.active_cursor_mut().row = 1; // bottom row: each line feed scrolls
    terminal_state.linefeed();
    terminal_state.linefeed();
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 2); // history populated

    process_terminal_bytes(&mut terminal_state, b"\x1b[3J"); // xterm "erase saved lines"
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn ed_3_on_the_alternate_screen_leaves_primary_scrollback_intact() {
    // Scrollback is the primary screen's history. ED 3 on the alternate screen
    // leaves it intact.
    let mut terminal_state = build_terminal_state(3, 2);
    terminal_state.active_cursor_mut().row = 1; // bottom row, on the primary screen
    terminal_state.linefeed();
    terminal_state.linefeed();
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 2); // populated from the primary

    terminal_state.active_screen = Screen::Alternate;
    process_terminal_bytes(&mut terminal_state, b"\x1b[3J"); // ED 3 while on the alternate screen
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 2); // primary history untouched
}

// --- SGR: set graphic rendition (pen colors + text attributes) ---

/// The default pen with the setters in `f` applied.
fn build_style_with_mutator(style_mutator: impl FnOnce(&mut Style)) -> Style {
    let mut style = Style::default();
    style_mutator(&mut style);
    style
}

#[test]
fn sgr_bold_sets_the_bold_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_bold(true))
    );
}

#[test]
fn sgr_zero_resets_the_pen() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // bold + red
    process_terminal_bytes(&mut terminal_state, b"\x1b[0m");
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_empty_params_reset_like_zero() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m");
    process_terminal_bytes(&mut terminal_state, b"\x1b[m"); // bare CSI m is an implicit reset
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_attribute_off_codes_clear_each_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3;4;7m"); // bold, italic, underline, reverse on
    process_terminal_bytes(&mut terminal_state, b"\x1b[22;23;24;27m"); // each turned back off
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_sixteen_color_foreground_and_background() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[31;42m"); // fg red (1), bg green (2)
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_foreground_color(Color::Indexed(1));
            style.set_background_color(Color::Indexed(2));
        })
    );
}

#[test]
fn sgr_bright_colors_map_to_indices_eight_through_fifteen() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[91;102m"); // bright red fg (8+1), bright green bg (8+2)
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_foreground_color(Color::Indexed(9));
            style.set_background_color(Color::Indexed(10));
        })
    );
}

#[test]
fn sgr_default_color_codes_restore_the_default() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[31;42m");
    process_terminal_bytes(&mut terminal_state, b"\x1b[39;49m"); // default fg + bg
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_256_color_foreground() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;5;196m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Indexed(196)))
    );
}

#[test]
fn sgr_256_color_background() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[48;5;21m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(21)))
    );
}

#[test]
fn sgr_truecolor_foreground_semicolon_form() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;2;255;128;0m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Rgb(255, 128, 0)))
    );
}

#[test]
fn sgr_truecolor_background_semicolon_form() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[48;2;10;20;30m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_background_color(Color::Rgb(10, 20, 30)))
    );
}

#[test]
fn sgr_256_color_colon_form() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:5:196m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Indexed(196)))
    );
}

#[test]
fn sgr_256_color_colon_form_with_empty_colorspace_id() {
    // `38:5::196` — an empty colorspace slot before the index (vte stores it as
    // a `0`). The index is read from the final subparameter; the slot is skipped.
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:5::196m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Indexed(196)))
    );
}

#[test]
fn sgr_truecolor_colon_form() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:2:255:128:0m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Rgb(255, 128, 0)))
    );
}

#[test]
fn sgr_truecolor_colon_form_with_empty_colorspace_id() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:2::255:128:0m"); // ITU form: empty colorspace slot
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Rgb(255, 128, 0)))
    );
}

#[test]
fn sgr_truecolor_colon_form_reads_the_channels_after_the_colorspace_slot() {
    // The full ITU direct-colour form:
    // `2:<colorspace>:<r>:<g>:<b>:<unused>:<tolerance>`. The channels are the
    // three after the colorspace slot, never the last three.
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:2:1:255:0:0:0:5m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Rgb(255, 0, 0)))
    );
}

#[test]
fn sgr_combines_multiple_codes_in_one_sequence() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;4;38;5;200;48;2;1;2;3m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_underline(UnderlineStyle::Single);
            style.set_foreground_color(Color::Indexed(200));
            style.set_background_color(Color::Rgb(1, 2, 3));
        })
    );
}

#[test]
fn sgr_pen_is_stamped_onto_subsequently_printed_glyphs() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m");
    terminal_state.print('x');
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(
        cell.get_style(),
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_foreground_color(Color::Indexed(1));
        })
    );
}

#[test]
fn sgr_unknown_code_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m"); // bold on
    process_terminal_bytes(&mut terminal_state, b"\x1b[99m"); // unknown SGR code -> pen unchanged
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_bold(true))
    );
}

#[test]
fn sgr_incomplete_extended_color_leaves_the_pen_unchanged() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;5m"); // 256-color selector with no index
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_incomplete_colon_extended_color_leaves_the_pen_unchanged() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:5m"); // colon 256-color selector with no index
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_256_color_index_out_of_range_leaves_the_pen_unchanged() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;5;256m"); // index 256 > 255 — out of range
    assert_eq!(terminal_state.active_render().style, Style::default()); // rejected, NOT wrapped to Indexed(0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[38:5:300m"); // colon form, index 300 > 255
    assert_eq!(terminal_state.active_render().style, Style::default()); // rejected, NOT wrapped to Indexed(44)
}

#[test]
fn sgr_truecolor_channel_out_of_range_leaves_the_pen_unchanged() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;2;999;0;0m"); // semicolon form, r = 999 > 255
    assert_eq!(terminal_state.active_render().style, Style::default()); // rejected, NOT wrapped to Rgb(231, 0, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[48:2:0:256:0m"); // colon form bg, g = 256 > 255
    assert_eq!(terminal_state.active_render().style, Style::default()); // rejected, NOT wrapped to Rgb(0, 0, 0)
}

#[test]
fn sgr_faint_sets_the_faint_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_faint(true))
    );
}

#[test]
fn sgr_blink_slow_and_rapid_both_set_one_flag() {
    let mut slow = build_terminal_state(5, 2);
    process_terminal_bytes(&mut slow, b"\x1b[5m"); // 5: slow blink
    assert_eq!(
        slow.active_render().style,
        build_style_with_mutator(|style| style.set_blink(true))
    );

    let mut rapid = build_terminal_state(5, 2);
    process_terminal_bytes(&mut rapid, b"\x1b[6m"); // 6: rapid blink — same flag
    assert_eq!(
        rapid.active_render().style,
        build_style_with_mutator(|style| style.set_blink(true))
    );
}

#[test]
fn sgr_conceal_sets_the_conceal_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[8m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_conceal(true))
    );
}

#[test]
fn sgr_strike_sets_the_strike_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[9m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_strike(true))
    );
}

#[test]
fn sgr_double_underline_sets_the_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[21m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_underline(UnderlineStyle::Double))
    );
}

#[test]
fn sgr_overline_sets_the_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[53m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_overline(true))
    );
}

#[test]
fn sgr_new_attribute_off_codes_clear_each_attribute() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[5;8;9;53m"); // blink, conceal, strike, overline on
    process_terminal_bytes(&mut terminal_state, b"\x1b[25;28;29;55m"); // each turned back off
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_normal_intensity_clears_both_bold_and_faint() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2m"); // bold AND faint — both held
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_faint(true);
        })
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[22m"); // 22 cancels both
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_underline_styles_are_mutually_exclusive_last_one_wins() {
    let mut single_last = build_terminal_state(5, 2);
    process_terminal_bytes(&mut single_last, b"\x1b[21;4m"); // double then single — single wins
    assert_eq!(
        single_last.active_render().style,
        build_style_with_mutator(|style| style.set_underline(UnderlineStyle::Single))
    );

    let mut double_last = build_terminal_state(5, 2);
    process_terminal_bytes(&mut double_last, b"\x1b[4;21m"); // single then double — double wins
    assert_eq!(
        double_last.active_render().style,
        build_style_with_mutator(|style| style.set_underline(UnderlineStyle::Double))
    );
}

#[test]
fn sgr_not_underlined_clears_the_underline_style() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[21m"); // double underline on
    process_terminal_bytes(&mut terminal_state, b"\x1b[24m"); // 24 → no underline
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_underline_subparameters_select_the_style() {
    // `4:n` — vte groups the subparameter into the SGR-4 param slice.
    let underline_style_cases: &[(&[u8], UnderlineStyle)] = &[
        (b"\x1b[4m", UnderlineStyle::Single),   // bare 4
        (b"\x1b[4:0m", UnderlineStyle::None),   // 4:0 cancels
        (b"\x1b[4:1m", UnderlineStyle::Single), // 4:1 single
        (b"\x1b[4:2m", UnderlineStyle::Double), // 4:2 double
        (b"\x1b[4:3m", UnderlineStyle::Curly),  // 4:3 curly
        (b"\x1b[4:4m", UnderlineStyle::Dotted), // 4:4 dotted
        (b"\x1b[4:5m", UnderlineStyle::Dashed), // 4:5 dashed
        (b"\x1b[4:9m", UnderlineStyle::Single), // unknown subparam → single
    ];
    for (sgr_sequence_bytes, underline_style) in underline_style_cases {
        let mut terminal_state = build_terminal_state(5, 2);
        process_terminal_bytes(&mut terminal_state, sgr_sequence_bytes);
        assert_eq!(
            terminal_state.active_render().style,
            build_style_with_mutator(|style| style.set_underline(*underline_style)),
            "sequence {sgr_sequence_bytes:?}"
        );
    }
}

#[test]
fn sgr_underline_colon_double_matches_legacy_21() {
    let mut colon_form_terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut colon_form_terminal_state, b"\x1b[4:2m"); // colon form
    let mut legacy_terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut legacy_terminal_state, b"\x1b[21m"); // legacy double-underline code
    assert_eq!(
        colon_form_terminal_state.active_render().style,
        legacy_terminal_state.active_render().style
    );
}

#[test]
fn sgr_underline_semicolon_two_is_single_plus_faint_not_double() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;2m"); // two separate params: underline + faint
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_underline(UnderlineStyle::Single);
            style.set_faint(true);
        })
    );
}

#[test]
fn sgr_underline_color_256_both_forms() {
    let mut semicolon_form_terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut semicolon_form_terminal_state, b"\x1b[58;5;208m");
    assert_eq!(
        semicolon_form_terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_underline_color(Some(Color::Indexed(208))))
    );

    let mut colon_form_terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut colon_form_terminal_state, b"\x1b[58:5:208m");
    assert_eq!(
        colon_form_terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_underline_color(Some(Color::Indexed(208))))
    );
}

#[test]
fn sgr_underline_color_truecolor_both_forms() {
    let mut semicolon_form_terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut semicolon_form_terminal_state, b"\x1b[58;2;10;20;30m");
    assert_eq!(
        semicolon_form_terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_underline_color(Some(Color::Rgb(10, 20, 30))))
    );

    let mut colon_form_terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut colon_form_terminal_state, b"\x1b[58:2:10:20:30m");
    assert_eq!(
        colon_form_terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_underline_color(Some(Color::Rgb(10, 20, 30))))
    );
}

#[test]
fn sgr_default_underline_color_resets_to_none() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[58;5;208m"); // explicit underline color
    process_terminal_bytes(&mut terminal_state, b"\x1b[59m"); // 59: back to default (follow fg)
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_underline_color_out_of_range_leaves_the_pen_unchanged() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[58;5;256m"); // index 256 > 255 — rejected
    assert_eq!(terminal_state.active_render().style, Style::default());
    process_terminal_bytes(&mut terminal_state, b"\x1b[58:2:300:0:0m"); // colon channel 300 > 255 — rejected
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn sgr_new_attributes_stamp_onto_printed_glyphs() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;9;5;21m"); // faint, strike, blink, double underline
    terminal_state.print('z');
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(
        cell.get_style(),
        build_style_with_mutator(|style| {
            style.set_faint(true);
            style.set_strike(true);
            style.set_blink(true);
            style.set_underline(UnderlineStyle::Double);
        })
    );
}

#[test]
fn decsc_restores_the_new_sgr_attributes() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;58;5;208m"); // faint + underline color
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC snapshots the whole render terminal_state
    process_terminal_bytes(&mut terminal_state, b"\x1b[0m"); // wipe the pen
    assert_eq!(terminal_state.active_render().style, Style::default());
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC restores it
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_faint(true);
            style.set_underline_color(Some(Color::Indexed(208)));
        })
    );
}

#[test]
fn sgr_out_of_range_truecolor_drains_its_channels_not_leaking_into_following_codes() {
    let mut terminal_state = build_terminal_state(5, 2);
    // r = 999 is out of range → the color is rejected, but 31 and 32 are its g/b
    // channels and must be CONSUMED, not reinterpreted as standalone SGR codes
    // (fg red / fg green). The pen must end fully unchanged.
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;2;999;31;32m");
    assert_eq!(terminal_state.active_render().style, Style::default()); // no leak

    // Exactly three channels (999, 1, 2) are drained, then the trailing `1` is
    // applied as SGR bold.
    process_terminal_bytes(&mut terminal_state, b"\x1b[38;2;999;1;2;1m");
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_bold(true))
    );
}

#[test]
fn sgr_does_not_move_the_cursor() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "ab"); // column 2
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 2));
}

#[test]
fn sgr_preserves_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(2, 2);
    print_text(&mut terminal_state, "ab"); // parked on the last column
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m"); // SGR is not a cursor move
    assert!(terminal_state.active_cursor().pending_wrap); // latch survives
}

// --- BCE: erase / scroll fill with the current background (not default) ---

#[test]
fn el_erases_the_line_to_the_current_background() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[44m"); // bg = blue (Indexed 4)
    process_terminal_bytes(&mut terminal_state, b"\x1b[K"); // EL 0 from column 0 -> whole row
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(4)));
    assert!((0..5).all(|column_index| {
        terminal_state
            .get_active_grid()
            .get_cell(0, column_index)
            .map(Cell::get_style)
            == Some(background_style)
    }));
}

#[test]
fn ed_2_erases_the_screen_to_the_current_background() {
    let mut terminal_state = build_terminal_state(3, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[42m"); // bg = green (Indexed 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2J"); // ED 2 — whole screen
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(2)));
    for row_index in 0..2 {
        assert!((0..3).all(|column_index| terminal_state
            .get_active_grid()
            .get_cell(row_index, column_index)
            .map(Cell::get_style)
            == Some(background_style)));
    }
}

#[test]
fn erase_uses_the_background_only_not_the_full_pen() {
    let mut terminal_state = build_terminal_state(3, 1);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31;44m"); // bold + fg red + bg blue
    process_terminal_bytes(&mut terminal_state, b"\x1b[K"); // erase row 0
                                                            // Erased cells carry ONLY the background; bold + foreground are dropped.
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(4)));
    assert!((0..3).all(|column_index| {
        terminal_state
            .get_active_grid()
            .get_cell(0, column_index)
            .map(Cell::get_style)
            == Some(background_style)
    }));
    // The pen itself is unchanged by the erase.
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_foreground_color(Color::Indexed(1));
            style.set_background_color(Color::Indexed(4));
        })
    );
}

#[test]
fn scroll_fills_the_exposed_row_with_the_current_background() {
    let mut terminal_state = build_terminal_state(2, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[42m"); // bg = green
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H"); // move to the bottom row (row 1)
    terminal_state.execute(b'\n'); // line feed on the last row -> scroll
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(2)));
    // The freshly exposed bottom row carries the current background.
    assert!((0..2).all(|column_index| {
        terminal_state
            .get_active_grid()
            .get_cell(1, column_index)
            .map(Cell::get_style)
            == Some(background_style)
    }));
}

// --- save/restore, insert/delete, scroll regions ---

#[test]
fn decsc_decrc_restores_the_cursor_and_pen() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // cursor -> (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // bold + fg red
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // move home
    process_terminal_bytes(&mut terminal_state, b"\x1b[0m"); // reset pen
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3));
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_foreground_color(Color::Indexed(1));
        })
    );
}

#[test]
fn decsc_decrc_preserves_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(2, 2);
    print_text(&mut terminal_state, "ab"); // fills row 0, parks at the last column
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC saves the latch
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // a cursor move clears it
    assert!(!terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC restores the latch
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('c'); // the latch makes the next glyph wrap, not overwrite
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a')); // row 0 untouched
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('c')); // wrapped onto row 1
}

#[test]
fn scosc_scorc_save_and_restore_the_cursor_and_pen() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;5H"); // (1, 4)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // bold + fg red
    let saved_style = terminal_state.active_render().style;
    process_terminal_bytes(&mut terminal_state, b"\x1b[s"); // SCOSC
    process_terminal_bytes(&mut terminal_state, b"\x1b[5;5H"); // move away
    process_terminal_bytes(&mut terminal_state, b"\x1b[0m"); // reset the pen to a different style
    process_terminal_bytes(&mut terminal_state, b"\x1b[u"); // SCORC
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 4));
    assert_eq!(terminal_state.active_render().style, saved_style); // pen restored too
}

#[test]
fn decrc_without_a_save_homes_and_resets_the_pen() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // move away
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // dirty the pen
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC with no prior DECSC
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn decrc_clamps_the_restored_cursor_into_a_shrunk_grid() {
    let mut terminal_state = build_terminal_state(10, 10);
    process_terminal_bytes(&mut terminal_state, b"\x1b[6;9H"); // (5, 8)
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // save
    terminal_state.resize_terminal_state(PtySize {
        column_count: 3,
        row_count: 3,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // restore -> clamped to the new bounds
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 2));
}

#[test]
fn reverse_index_moves_the_cursor_up_one_line() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;1H"); // row 2
    process_terminal_bytes(&mut terminal_state, b"\x1bM"); // RI
    assert_eq!(terminal_state.active_cursor().row, 1);
}

#[test]
fn reverse_index_at_the_top_scrolls_the_region_down() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home — at the top margin
    process_terminal_bytes(&mut terminal_state, b"\x1bM"); // RI scrolls down
    assert_eq!(get_row_text(&terminal_state, 0), "   "); // fresh blank top
    assert_eq!(get_row_text(&terminal_state, 1), "abc"); // pushed down
    assert_eq!(get_row_text(&terminal_state, 2), "def"); // ghi fell off the bottom
}

#[test]
fn decstbm_sets_the_region_and_homes_the_cursor() {
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;1H"); // move away from home first
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // margins rows 2..4 (1-based) -> (1, 3)
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 3)));
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
}

#[test]
fn decstbm_full_span_clears_the_region() {
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // set a region
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;5r"); // whole screen -> None
    assert_eq!(terminal_state.primary_scroll_region, None);
}

#[test]
fn decstbm_with_no_parameters_clears_the_region() {
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // set a region
    process_terminal_bytes(&mut terminal_state, b"\x1b[r"); // CSI r, defaults = whole screen -> None
    assert_eq!(terminal_state.primary_scroll_region, None);
}

#[test]
fn decstbm_with_an_invalid_range_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // valid region (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;2r"); // top not above bottom -> ignored
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 3)));
}

#[test]
fn decstbm_top_equal_bottom_is_ignored() {
    // A single-row request (`top == bottom`) is rejected: the region needs a
    // strict `top < bottom`. The region stays as it was and the cursor is not
    // homed.
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // valid region (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // move the cursor away from home
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3r"); // top == bottom (both 0-based 2) -> ignored
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 3))); // region unchanged
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 2)); // cursor NOT homed by the ignored request
}

#[test]
fn line_feed_scrolls_only_within_the_region() {
    let mut terminal_state = build_terminal_state(3, 4); // 4 rows
    process_terminal_bytes(&mut terminal_state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3r"); // region rows 2..3 -> (1, 2); homes cursor
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;1H"); // to the bottom margin (row 2)
    terminal_state.execute(b'\n'); // line feed at the bottom margin -> scroll region up
    assert_eq!(get_row_text(&terminal_state, 0), "AAA"); // above region, untouched
    assert_eq!(get_row_text(&terminal_state, 1), "CCC"); // old row 2 rose
    assert_eq!(get_row_text(&terminal_state, 2), "   "); // blank exposed at the region bottom
    assert_eq!(get_row_text(&terminal_state, 3), "DDD"); // below region, untouched
}

#[test]
fn ich_inserts_blank_cells_shifting_the_line_right() {
    let mut terminal_state = build_terminal_state(5, 1);
    process_terminal_bytes(&mut terminal_state, b"abcde"); // fills row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // cursor -> (0, 2) on 'c'
    process_terminal_bytes(&mut terminal_state, b"\x1b[2@"); // ICH 2
    assert_eq!(get_row_text(&terminal_state, 0), "ab  c"); // c shifts right; d, e fall off
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 2)); // cursor unchanged
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn ich_fills_inserted_cells_with_the_current_background() {
    let mut terminal_state = build_terminal_state(5, 1);
    process_terminal_bytes(&mut terminal_state, b"\x1b[42m"); // bg = green
    process_terminal_bytes(&mut terminal_state, b"abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // cursor -> (0, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2@"); // ICH 2 — inserted blanks carry the bg
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(2)));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 2)
            .map(Cell::get_style),
        Some(background_style)
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 3)
            .map(Cell::get_style),
        Some(background_style)
    );
}

#[test]
fn dch_deletes_cells_pulling_the_line_left() {
    let mut terminal_state = build_terminal_state(5, 1);
    process_terminal_bytes(&mut terminal_state, b"abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor -> (0, 1) on 'b'
    process_terminal_bytes(&mut terminal_state, b"\x1b[2P"); // DCH 2
    assert_eq!(get_row_text(&terminal_state, 0), "ade  "); // b, c removed; padded right
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 1)); // cursor unchanged
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn dch_fills_padded_cells_with_the_current_background() {
    let mut terminal_state = build_terminal_state(5, 1);
    process_terminal_bytes(&mut terminal_state, b"\x1b[42m"); // bg = green
    process_terminal_bytes(&mut terminal_state, b"abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor -> (0, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2P"); // DCH 2 — the right-end pad carries the bg
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(2)));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 4)
            .map(Cell::get_style),
        Some(background_style)
    );
}

#[test]
fn il_inserts_a_blank_line_and_keeps_the_cursor() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3H"); // cursor -> (1, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[L"); // IL 1
    assert_eq!(get_row_text(&terminal_state, 0), "abc"); // above, untouched
    assert_eq!(get_row_text(&terminal_state, 1), "   "); // blank inserted
    assert_eq!(get_row_text(&terminal_state, 2), "def"); // def pushed down; ghi fell off
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 2)); // cursor unchanged (column kept)
}

#[test]
fn dl_deletes_a_line_within_the_region() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // cursor -> (0, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[M"); // DL 1
    assert_eq!(get_row_text(&terminal_state, 0), "def"); // def rose
    assert_eq!(get_row_text(&terminal_state, 1), "ghi");
    assert_eq!(get_row_text(&terminal_state, 2), "   "); // blank at the bottom
}

#[test]
fn il_outside_the_region_is_ignored() {
    let mut terminal_state = build_terminal_state(3, 4); // 4 rows
    process_terminal_bytes(&mut terminal_state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3r"); // region rows 2..3 -> (1, 2); homes cursor
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // cursor row 0 — above the region
    process_terminal_bytes(&mut terminal_state, b"\x1b[L"); // IL ignored outside the region
    assert_eq!(get_row_text(&terminal_state, 0), "AAA");
    assert_eq!(get_row_text(&terminal_state, 1), "BBB");
    assert_eq!(get_row_text(&terminal_state, 2), "CCC");
    assert_eq!(get_row_text(&terminal_state, 3), "DDD");
}

#[test]
fn su_scrolls_the_region_up_leaving_the_cursor() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // cursor -> (1, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[S"); // SU 1
    assert_eq!(get_row_text(&terminal_state, 0), "def");
    assert_eq!(get_row_text(&terminal_state, 1), "ghi");
    assert_eq!(get_row_text(&terminal_state, 2), "   ");
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 1)); // cursor unmoved
}

#[test]
fn sd_scrolls_the_region_down_leaving_the_cursor() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // cursor -> (1, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[T"); // SD 1
    assert_eq!(get_row_text(&terminal_state, 0), "   ");
    assert_eq!(get_row_text(&terminal_state, 1), "abc");
    assert_eq!(get_row_text(&terminal_state, 2), "def"); // ghi fell off the bottom
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 1)); // cursor unmoved
}

#[test]
fn sd_via_the_ecma48_caret_form_scrolls_the_region_down() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // cursor -> (1, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[^"); // CSI ^ = SD (ECMA-48 form)
    assert_eq!(get_row_text(&terminal_state, 0), "   ");
    assert_eq!(get_row_text(&terminal_state, 1), "abc");
    assert_eq!(get_row_text(&terminal_state, 2), "def"); // ghi fell off the bottom
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 1)); // cursor unmoved
}

#[test]
fn the_highlight_tracking_form_of_csi_t_does_not_scroll() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2;3;4;5T"); // 5-param CSI T = highlight tracking, not SD
    assert_eq!(get_row_text(&terminal_state, 0), "abc"); // grid unchanged
    assert_eq!(get_row_text(&terminal_state, 1), "def");
    assert_eq!(get_row_text(&terminal_state, 2), "ghi");
}

// --- Alternate screen (`?47`/`?1047`/`?1048`/`?1049`), DECTCEM (`?25`), and
// OSC 0/1/2 title ---

#[test]
fn dec_47_swaps_to_the_alternate_buffer_and_back() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert_eq!(terminal_state.active_screen, Screen::Primary);
}

#[test]
fn alternate_screen_output_leaves_the_primary_grid_untouched() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"abc"); // primary row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    process_terminal_bytes(&mut terminal_state, b"ZZ"); // written to the alternate grid
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert_eq!(get_row_text(&terminal_state, 0), "abc  "); // primary unchanged
}

#[test]
fn dec_1049_saves_the_cursor_switches_and_restores() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h");
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    let saved = terminal_state
        .primary_cursor
        .saved
        .expect("primary cursor saved");
    assert_eq!((saved.row, saved.column), (2, 3));
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // move on the alternate screen
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l");
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3)); // restored
}

#[test]
fn dec_1049_clears_the_alternate_buffer_on_entry_using_the_background() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter without clearing
    process_terminal_bytes(&mut terminal_state, b"xyz"); // alternate row 0 = "xyz"
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // leave; the alternate keeps "xyz"
    process_terminal_bytes(&mut terminal_state, b"\x1b[44m"); // pen bg = blue (Indexed 4)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // re-enter; clears with the current bg
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(4)));
    for row_index in 0..3 {
        assert!((0..5).all(|column_index| terminal_state
            .get_active_grid()
            .get_cell(row_index, column_index)
            .map(Cell::get_style)
            == Some(background_style)));
    }
    assert_eq!(get_row_text(&terminal_state, 0), "     "); // blanked
}

#[test]
fn dec_1047_clears_the_alternate_buffer_on_exit_using_the_background() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h");
    process_terminal_bytes(&mut terminal_state, b"xyz"); // alternate row 0 = "xyz"
    process_terminal_bytes(&mut terminal_state, b"\x1b[44m"); // pen bg = blue before the clearing exit
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // clears the alternate with the current bg, back to primary
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // re-enter (1047 does not clear on entry)
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(4)));
    for row_index in 0..3 {
        assert!((0..5).all(|column_index| terminal_state
            .get_active_grid()
            .get_cell(row_index, column_index)
            .map(Cell::get_style)
            == Some(background_style)));
    }
    assert_eq!(get_row_text(&terminal_state, 0), "     "); // was cleared on the prior exit
}

#[test]
fn dec_1048_saves_and_restores_the_cursor_and_pen_without_swapping() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // bold + fg red
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1048h"); // save (no buffer swap)
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H\x1b[0m"); // move home + reset pen
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1048l"); // restore
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3)); // position restored
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_foreground_color(Color::Indexed(1));
        })
    ); // pen restored
}

#[test]
fn a_save_on_the_alternate_screen_does_not_clobber_the_primary_stash() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // stash (2, 3) in the primary slot
    let primary_saved_cursor = terminal_state
        .primary_cursor
        .saved
        .expect("primary cursor saved");
    assert_eq!(
        (primary_saved_cursor.row, primary_saved_cursor.column),
        (2, 3)
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H\x1b7"); // on the alternate: move to (1, 1), DECSC into the alt's OWN slot
    let alternate_saved_cursor = terminal_state
        .alternate_cursor
        .saved
        .expect("alternate saved");
    assert_eq!(
        (alternate_saved_cursor.row, alternate_saved_cursor.column),
        (1, 1)
    );
    let primary_after = terminal_state
        .primary_cursor
        .saved
        .expect("primary still saved");
    assert_eq!((primary_after.row, primary_after.column), (2, 3)); // the alt DECSC did NOT touch the primary slot
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // back to primary, restore from the primary slot
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3)); // primary stash intact + restored
}

#[test]
fn re_entering_the_alternate_screen_does_not_re_clear_it() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter + clear
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1Hhi"); // write "hi" at the top-left of the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // already on the alternate: must not re-clear
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('h'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('i'));
}

#[test]
fn dectcem_toggles_cursor_visibility() {
    let mut terminal_state = build_terminal_state(5, 3);
    assert!(terminal_state.is_cursor_visible()); // visible by default
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25l");
    assert!(!terminal_state.is_cursor_visible());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25h");
    assert!(terminal_state.is_cursor_visible());
}

#[test]
fn osc_2_sets_the_window_title() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;hello\x07");
    assert_eq!(terminal_state.get_title(), Some("hello"));
}

#[test]
fn osc_0_and_1_also_set_the_title() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]0;zero\x07");
    assert_eq!(terminal_state.get_title(), Some("zero"));
    process_terminal_bytes(&mut terminal_state, b"\x1b]1;icon\x07");
    assert_eq!(terminal_state.get_title(), Some("icon"));
}

#[test]
fn osc_title_keeps_embedded_semicolons() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;a;b;c\x07");
    assert_eq!(terminal_state.get_title(), Some("a;b;c"));
}

#[test]
fn osc_title_accepts_a_string_terminator() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;via-st\x1b\\");
    assert_eq!(terminal_state.get_title(), Some("via-st"));
}

#[test]
fn osc133_marks_the_prompt_row_and_tracks_the_shell_state() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;A\x07");

    assert!(terminal_state.get_active_grid().has_prompt_mark(0));
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0));
    assert_eq!(get_row_text(&terminal_state, 0), "     ");

    process_terminal_bytes(&mut terminal_state, b"\x1b]133;B\x07");
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Input
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;C\x07");
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Running
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;D;137\x07");
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
}

#[test]
fn osc133_finish_does_not_close_input_without_command_start() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;B\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;D;1\x07");

    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Input
    );
}

#[test]
fn output_without_osc133_keeps_the_unmarked_prompt_state() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"plain shell output");

    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert!(
        (0..3).all(|row_index| { !terminal_state.get_active_grid().has_prompt_mark(row_index) })
    );
}

#[test]
fn each_pane_keeps_its_own_osc133_state() {
    let mut first_terminal_state = build_terminal_state(5, 3);
    let mut second_terminal_state = build_terminal_state(5, 3);

    process_terminal_bytes(
        &mut first_terminal_state,
        b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07",
    );
    process_terminal_bytes(&mut second_terminal_state, b"\x1b]133;B\x07");
    process_terminal_bytes(&mut first_terminal_state, b"\x1b]133;D;0\x07");

    assert!(first_terminal_state.get_active_grid().has_prompt_mark(0));
    assert_eq!(
        first_terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(
        second_terminal_state.shell_integration_state,
        ShellIntegrationState::Input
    );
}

#[test]
fn osc133_does_not_mark_a_row_on_the_alternate_screen_after_reset() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h\x1b]133;A\x07\x1b[?1047l");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h");

    assert!(!terminal_state.get_active_grid().has_prompt_mark(0));
}

#[test]
fn the_title_is_none_until_an_osc_sets_it() {
    let terminal_state = build_terminal_state(5, 3);
    assert_eq!(terminal_state.get_title(), None);
}

#[test]
fn osc_7_reports_the_working_directory() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///Users/me/proj\x07");
    let reported_working_directory = terminal_state
        .get_current_working_directory()
        .expect("reported_working_directory set");
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/Users/me/proj")
    );
    assert_eq!(reported_working_directory.get_host(), None); // empty authority
}

#[test]
fn osc_7_preserves_the_host_component() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file://myhost/home/u\x07");
    let reported_working_directory = terminal_state
        .get_current_working_directory()
        .expect("reported_working_directory set");
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/home/u")
    );
    assert_eq!(reported_working_directory.get_host(), Some("myhost")); // host kept
}

#[test]
fn osc_7_keeps_localhost_as_the_host() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file://localhost/home/u\x07");
    let reported_working_directory = terminal_state
        .get_current_working_directory()
        .expect("reported_working_directory set");
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/home/u")
    );
    assert_eq!(reported_working_directory.get_host(), Some("localhost"));
}

#[test]
fn osc_7_percent_decodes_the_path() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///home/a%20b\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/home/a b")
    );
}

#[test]
fn osc_7_percent_decodes_a_multibyte_sequence() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///p/%C3%A9\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/p/\u{e9}")
    );
}

#[test]
fn osc_7_accepts_a_string_terminator() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///srv\x1b\\");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/srv")
    );
}

#[test]
fn osc_7_keeps_an_embedded_semicolon_in_the_path() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///a;b\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/a;b")
    );
}

#[test]
fn the_cwd_is_none_until_osc_7_reports_one() {
    let terminal_state = build_terminal_state(5, 3);
    assert!(terminal_state.get_current_working_directory().is_none());
}

#[test]
fn osc_7_ignores_a_non_file_uri() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;http://example/x\x07");
    assert!(terminal_state.get_current_working_directory().is_none());
}

#[test]
fn osc_7_ignores_a_uri_with_no_path() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file://host\x07");
    assert!(terminal_state.get_current_working_directory().is_none());
}

#[test]
fn osc_7_ignores_an_empty_payload() {
    // `ESC ] 7 ST` → params = ["7"]: no payload, nothing is parsed.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7\x07");
    assert!(terminal_state.get_current_working_directory().is_none());
}

#[test]
fn osc_7_accepts_a_case_insensitive_scheme() {
    // RFC 3986: the scheme compares case-insensitively, the path does not.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;FILE:///srv\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/srv")
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;File:///opt/App\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/opt/App")
    );
}

#[test]
fn osc_7_keeps_the_last_good_cwd_when_a_new_emit_is_invalid() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///good\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;garbage\x07"); // unparseable
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/good")
    );
}

#[test]
fn osc_7_a_new_valid_report_updates_the_cwd() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///first\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///second\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/second")
    );
}

#[test]
fn osc_7_rejects_a_path_with_a_nul_byte() {
    // `%00` decodes to a NUL: the report is rejected and the previous good reported_working_directory
    // is left intact.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///good\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///a%00b\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/good")
    );
}

#[test]
fn osc_7_cwd_survives_a_screen_switch() {
    // Entering the alternate screen keeps the reported reported_working_directory.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///work\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate screen
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/work")
    );
}

#[test]
fn osc_7_reports_the_root_directory() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("/")
    );
}

#[test]
fn osc_7_decodes_an_encoded_slash_after_splitting_the_host() {
    // The host/path split is on the first raw slash. A `%2F` survives the split
    // and decodes to `/` afterward, giving two path components.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file://host/a%2Fb\x07");
    let reported_working_directory = terminal_state
        .get_current_working_directory()
        .expect("reported_working_directory set");
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/a/b")
    );
    assert_eq!(reported_working_directory.get_host(), Some("host"));
}

#[cfg(windows)]
#[test]
fn osc_7_strips_the_leading_slash_before_a_windows_drive() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///C:/Users/me\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        Path::new("C:/Users/me")
    );
}

#[cfg(unix)]
#[test]
fn osc_7_preserves_a_non_utf8_path_on_unix() {
    use std::os::unix::ffi::OsStringExt;
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file:///p/%FF\x07");
    let expected_path = PathBuf::from(std::ffi::OsString::from_vec(b"/p/\xff".to_vec()));
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .expect("reported_working_directory set")
            .get_working_directory_path(),
        expected_path.as_path()
    );
}

#[test]
fn dec_47_reset_is_a_noop_when_already_on_primary() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"abc"); // primary row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // already on primary: must do nothing
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    assert_eq!(get_row_text(&terminal_state, 0), "abc  "); // primary untouched
}

#[test]
fn dec_1047_reset_when_already_on_primary_does_not_clear_it() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"abc"); // primary row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // already on primary: the guard must stop the clear
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    assert_eq!(get_row_text(&terminal_state, 0), "abc  "); // primary not wiped
}

#[test]
fn dec_1049_preserves_primary_content_across_the_cycle() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"abc"); // primary row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate (clears the alternate, not the primary)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1HZZ"); // write on the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // back to the primary
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    assert_eq!(get_row_text(&terminal_state, 0), "abc  "); // primary intact
}

#[test]
fn an_unknown_dec_private_mode_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"abc");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?9999h"); // unknown DEC private mode
    process_terminal_bytes(&mut terminal_state, b"\x1b[?9999l");
    assert_eq!(terminal_state.active_screen, Screen::Primary); // no screen change
    assert_eq!(get_row_text(&terminal_state, 0), "abc  "); // grid untouched
}

#[test]
fn an_unknown_osc_command_does_not_change_the_title() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;keep\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]3;ignored\x07"); // OSC 3 is not handled
    assert_eq!(terminal_state.get_title(), Some("keep"));
}

#[test]
fn a_decset_sequence_applies_every_mode_in_the_list() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25l"); // hide the cursor first
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25;47h"); // show the cursor AND enter the alternate in one sequence
    assert!(terminal_state.is_cursor_visible()); // ?25 applied
    assert_eq!(terminal_state.active_screen, Screen::Alternate); // ?47 applied — not just the first param
}

#[test]
fn a_decrst_sequence_applies_every_mode_in_the_list() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47;25l"); // exit the alternate AND hide the cursor in one sequence
    assert_eq!(terminal_state.active_screen, Screen::Primary); // ?47 applied
    assert!(!terminal_state.is_cursor_visible()); // ?25 applied — the second param is honored
}

#[test]
fn dec_1049_clears_the_alternate_buffer_on_exit() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate (cleared on entry)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1Hxyz"); // write on the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // exit: must clear the alternate too
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter via ?47 (no clear on entry)
    assert_eq!(get_row_text(&terminal_state, 0), "     "); // the alternate was cleared on the prior ?1049 l exit
}

#[test]
fn dec_47_before_1049_in_one_decset_saves_the_primary_cursor_not_the_alternate() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
                                                               // `?47` switches to the alternate first; `?1049` must still stash the cursor
                                                               // of the screen the list began on (the primary), not the alternate.
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47;1049h");
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    let saved = terminal_state
        .primary_cursor
        .saved
        .expect("primary cursor saved");
    assert_eq!((saved.row, saved.column), (2, 3));
    assert!(
        terminal_state.alternate_cursor.saved.is_none(),
        "the save must not land in the alternate slot"
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // move on the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // exit + restore
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3)); // primary cursor restored
}

#[test]
fn dec_47_before_1049_in_one_decset_clears_the_stale_alternate() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate without clearing
    process_terminal_bytes(&mut terminal_state, b"xyz"); // alternate row 0 = "xyz"
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to primary; the alternate keeps "xyz"
                                                               // `?47` re-enters onto the stale "xyz"; `?1049` must still clear it.
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47;1049h");
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    assert_eq!(get_row_text(&terminal_state, 0), "     "); // 1049 cleared the stale alternate
}

#[test]
fn dec_47_l_before_1049_l_leaves_the_alternate_uncleared() {
    // `?47 l` switches to the primary first, without clearing. The following
    // `?1049 l` runs on the primary: its clear is a no-op and only the DECRC
    // runs. The alternate keeps its contents.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"xyz"); // alternate row 0 = "xyz"
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47;1049l"); // ?47 l leaves first -> ?1049 l clear is skipped
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter via ?47 (no clear on entry)
    assert_eq!(get_row_text(&terminal_state, 0), "xyz  "); // NOT cleared — the clear was skipped on the primary
}

#[test]
fn dec_1049_l_then_1047_l_clears_the_alternate_only_once() {
    // `?1049 l` clears the alternate with the alternate's pen, switches to the
    // primary, and restores the primary SGR. The trailing `?1047 l` runs on the
    // primary and clears nothing: the alternate keeps the first clear's blue
    // background.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[44m"); // alternate pen bg = blue (Indexed 4)
    process_terminal_bytes(&mut terminal_state, b"xyz"); // draw on the alternate with the blue pen
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049;1047l"); // one clearing exit; the trailing ?1047 l must be a no-op
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter to inspect the alternate cells
    let blue_background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(4)));
    for row_index in 0..3 {
        assert!(
            (0..5).all(|column_index| {
                terminal_state
                    .get_active_grid()
                    .get_cell(row_index, column_index)
                    .map(Cell::get_style)
                    == Some(blue_background_style)
            }),
            "row {row_index} should be blanked with the alternate's blue pen, not re-cleared with the primary's"
        );
    }
}

#[test]
fn dec_1049_then_47_in_one_decset_saves_the_primary_and_seeds_the_alternate_once() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
                                                               // `?1049` enters + saves + clears; the trailing `?47` must be a no-op (no
                                                               // re-seed of the alternate cursor, no second clear).
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049;47h");
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    let saved = terminal_state
        .primary_cursor
        .saved
        .expect("primary cursor saved");
    assert_eq!((saved.row, saved.column), (2, 3));
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3)); // alternate cursor seeded from the primary
}

#[test]
fn verify_params_iter_yields_correct_param_groups() {
    // `vte::Params::iter()` yields one `&[u16]` per parameter: the top-level
    // value followed by its colon-separated subparameters. A DEC mode with no
    // subparameters is a one-element slice.
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
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1048h"); // save the primary cursor (no switch)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // move to (0, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // already on primary: the ?1048 l restore must still run
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3)); // restored
    assert_eq!(terminal_state.active_screen, Screen::Primary);
}

#[test]
fn scroll_region_does_not_leak_from_the_alternate_to_the_primary() {
    let mut terminal_state = build_terminal_state(10, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // alt sets DECSTBM rows 2..4 (1-based) -> (1, 3)
    assert_eq!(terminal_state.alternate_scroll_region, Some((1, 3)));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // exit to the primary
    assert_eq!(terminal_state.active_screen, Screen::Primary);
    assert_eq!(terminal_state.primary_scroll_region, None); // primary margins never touched
}

#[test]
fn each_screen_keeps_its_own_scroll_region_across_a_round_trip() {
    let mut terminal_state = build_terminal_state(10, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // primary margins -> (1, 3)
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 3)));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate
    assert_eq!(terminal_state.alternate_scroll_region, None); // alt starts unconstrained
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3r"); // alt margins -> (0, 2)
    assert_eq!(terminal_state.alternate_scroll_region, Some((0, 2)));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // back to the primary
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 3))); // primary margins survived
}

#[test]
fn resize_clears_both_screens_scroll_regions() {
    let mut terminal_state = build_terminal_state(10, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // primary region -> (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3r"); // alt region -> (0, 2)
    terminal_state.resize_terminal_state(PtySize {
        column_count: 8,
        row_count: 4,
    });
    assert_eq!(terminal_state.primary_scroll_region, None);
    assert_eq!(terminal_state.alternate_scroll_region, None);
}

#[test]
fn line_feed_respects_the_alternate_screens_own_region() {
    let mut terminal_state = build_terminal_state(4, 4);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2r"); // alt region rows 1..2 (1-based) -> (0, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1Hx"); // cursor to the region bottom (row 1), print 'x'
    terminal_state.execute(b'\n'); // line feed at the region bottom -> scroll within (0, 1)
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('x')); // 'x' rose from row 1 to row 0
}

#[test]
fn a_fresh_1049_entry_resets_a_stale_alternate_scroll_region() {
    let mut terminal_state = build_terminal_state(10, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // app A enters the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // app A sets DECSTBM rows 2..4 -> (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // exit via ?47 l (non-clearing: leaves the region set)
    assert_eq!(terminal_state.alternate_scroll_region, Some((1, 3))); // still stale after a non-clearing exit
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // app B enters fresh via ?1049 h
    assert_eq!(terminal_state.alternate_scroll_region, None); // entry reset the inherited margins to full screen
}

#[test]
fn a_clearing_exit_resets_the_alternate_scroll_region() {
    let mut terminal_state = build_terminal_state(10, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // set DECSTBM -> (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // a clearing exit must reset the region too
    assert_eq!(terminal_state.alternate_scroll_region, None);
}

#[test]
fn dec_47_reentry_preserves_the_alternate_scroll_region() {
    let mut terminal_state = build_terminal_state(10, 6);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter via ?47 (preserve mode)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // set the alternate's DECSTBM -> (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // non-clearing exit
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter via ?47 — must preserve the region, like the cursor
    assert_eq!(terminal_state.alternate_scroll_region, Some((1, 3)));
}

#[test]
fn a_fresh_1049_entry_drops_a_stale_alternate_saved_cursor() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // app A enters
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b7"); // app A: move to (2, 2), DECSC (stashes the alt cursor)
    let stashed = terminal_state
        .alternate_cursor
        .saved
        .expect("alternate DECSC stash");
    assert_eq!((stashed.row, stashed.column), (2, 2));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // non-clearing exit (leaves the alt cursor + its stash)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // app B enters fresh
    assert_eq!(terminal_state.alternate_cursor.saved, None); // app A's DECSC stash dropped
    process_terminal_bytes(&mut terminal_state, b"\x1b[5;5H\x1b8"); // app B DECRC with no prior DECSC -> home, not (2, 2)
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
}

#[test]
fn a_fresh_1049_entry_shows_the_cursor_even_if_a_prior_session_hid_it() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // app A enters
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25l"); // app A hides the cursor on the alternate
    assert!(!terminal_state.is_cursor_visible());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // non-clearing exit (the alternate keeps is_visible = false)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // app B enters fresh
    assert!(terminal_state.is_cursor_visible()); // shown by default; app A's ?25 l is not inherited
}

#[test]
fn a_clearing_exit_drops_the_alternate_wrap_latch() {
    let mut terminal_state = build_terminal_state(3, 2); // 3 columns, 2 rows
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"abc"); // fill row 0 -> parks the wrap latch at the last column
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // clearing exit erases the parked glyph -> the latch must drop
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // non-clearing re-entry sees the fresh cursor
    assert!(!terminal_state.active_cursor().pending_wrap);
    terminal_state.print('z'); // first print lands in place, no spurious wrap against an erased glyph
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('z'));
    assert_eq!(terminal_state.active_cursor().row, 0);
}

#[test]
fn a_clearing_exit_resets_the_alternate_cursor_to_home() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // enter
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // move the alternate cursor to (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // a clearing exit ends the session -> reset to a fresh buffer
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // non-clearing re-entry sees the fresh cursor
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0)); // home — the clearing exit reset the cursor before re-entry
}

// --- Per-screen cursor independence ---

#[test]
fn dec_1049h_does_not_carry_pending_wrap_to_the_alternate() {
    let mut terminal_state = build_terminal_state(3, 2);
    process_terminal_bytes(&mut terminal_state, b"abc"); // fills row 0 on primary, parks at (0, 2)
    assert!(terminal_state.active_cursor().pending_wrap); // parked on primary
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alternate: seed column from primary, clear latch + grid
    assert!(!terminal_state.active_cursor().pending_wrap); // latch NOT carried
    terminal_state.print('x'); // must not wrap early to row 1
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('x')); // lands at the seeded column on row 0
    assert_eq!(terminal_state.active_cursor().row, 0); // no early wrap
}

#[test]
fn pending_wrap_is_independent_per_screen() {
    let mut terminal_state = build_terminal_state(3, 2);
    process_terminal_bytes(&mut terminal_state, b"abc"); // primary parks at (0, 2)
    assert!(terminal_state.active_cursor().pending_wrap); // primary has the latch
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter alternate (no clear, no reseed): its own cursor
    assert!(!terminal_state.active_cursor().pending_wrap); // alternate latch independent (starts clear)
    terminal_state.print('x'); // alternate cursor starts at home (0, 0); no early wrap
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('x'));
    assert_eq!(terminal_state.active_cursor().row, 0);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to primary
    assert!(terminal_state.active_cursor().pending_wrap); // primary latch untouched
}

#[test]
fn dec_47_reentry_resumes_where_the_alternate_left_off() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate (its own cursor at home)
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4Hxy"); // draw on the alternate; cursor ends at (2, 5)
    let alt = terminal_state.active_cursor();
    assert_eq!((alt.row, alt.column), (2, 5));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // pop back to the primary (alternate kept intact)
    process_terminal_bytes(&mut terminal_state, b"zz"); // primary output — must not disturb the alternate cursor
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter: must resume at (2, 5), not reseed from the primary
    let alt = terminal_state.active_cursor();
    assert_eq!((alt.row, alt.column), (2, 5));
    process_terminal_bytes(&mut terminal_state, b"w"); // resumes exactly where the alternate left off
    assert_eq!(get_terminal_glyph(&terminal_state, 2, 5), Some('w'));
}

#[test]
fn dec_47_reentry_preserves_the_alternate_wrap_latch() {
    let mut terminal_state = build_terminal_state(3, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"abc"); // fill the alternate's row 0 — parks the wrap latch at (0, 2)
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to primary
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter: the latch must survive, not be cleared by a reseed
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('z'); // the parked latch wraps to row 1 instead of overprinting (0, 2)
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('z'));
}

#[test]
fn cursor_position_is_independent_per_screen() {
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // primary cursor at (2, 2)
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (2, 2)
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alternate: cursor seeded from the primary
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (2, 2)
    ); // seeded, not (0, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;4H"); // move the alternate cursor to (3, 3)
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (3, 3)
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // back to primary
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (2, 2)
    ); // primary intact, unaffected by alt's move
}

#[test]
fn cursor_visibility_is_independent_per_screen() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25l"); // hide the cursor on primary
    assert!(!terminal_state.is_cursor_visible());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alternate
    assert!(terminal_state.is_cursor_visible()); // the alternate's own visibility: still shown
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // back to primary
    assert!(!terminal_state.is_cursor_visible()); // primary still hidden
}

#[test]
fn dec_47_round_trip_leaves_the_primary_cursor_untouched() {
    // `?47`/`?1047` neither save nor restore the cursor. Each screen has its own
    // live cursor: the primary cursor is back at its pre-switch spot.
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // primary cursor at (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate (no save, no seed)
    process_terminal_bytes(&mut terminal_state, b"\x1b[5;5H"); // move the alternate cursor to (4, 4)
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (4, 4)
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // exit (no restore)
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (2, 2)
    ); // primary cursor never touched by the alternate's motion
}

#[test]
fn cursor_visibility_is_independent_across_a_dec_47_round_trip() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?25l"); // hide the cursor on primary
    assert!(!terminal_state.is_cursor_visible());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate — its own visibility
    assert!(terminal_state.is_cursor_visible()); // alternate is shown, independent of primary
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to primary
    assert!(!terminal_state.is_cursor_visible()); // primary still hidden
}

// --- Renderer-facing read accessors: cursor position + active screen ---

#[test]
fn active_cursor_position_reports_cursor_moves() {
    let mut terminal_state = build_terminal_state(5, 5);
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0)); // home at start
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // CUP row 3 column 4 (1-based) -> (2, 3) 0-based
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 3));
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // move to (1, 1)
    assert_eq!(terminal_state.get_active_cursor_position(), (1, 1));
}

#[test]
fn active_screen_flips_on_alternate_switch() {
    let mut terminal_state = build_terminal_state(5, 5);
    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter the alternate screen
    assert_eq!(terminal_state.get_active_screen(), Screen::Alternate);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // back to the primary
    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // also flips via ?47
    assert_eq!(terminal_state.get_active_screen(), Screen::Alternate);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l");
    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
}

// --- Unicode display-pixel_width: wide glyphs, combining marks, ambiguous width ---

#[test]
fn wide_char_occupies_two_cells_and_advances_by_two() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('中'); // CJK ideograph, display width 2
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '中');
    assert_eq!(base.get_display_width(), 2);
    // The second column is a width-0 continuation placeholder.
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    // Cursor steps past both cells.
    assert_eq!(terminal_state.active_cursor().column, 2);
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn emoji_is_wide() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('😀'); // emoji, display width 2
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_character),
        Some('😀')
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(2)
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(terminal_state.active_cursor().column, 2);
}

#[test]
fn two_wide_chars_lay_side_by_side() {
    let mut terminal_state = build_terminal_state(6, 2);
    print_text(&mut terminal_state, "中文");
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('中'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('文'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 3)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(terminal_state.active_cursor().column, 4);
}

#[test]
fn ambiguous_width_char_is_narrow() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('§'); // East-Asian Ambiguous → narrow under the default policy
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(terminal_state.active_cursor().column, 1);
}

#[test]
fn combining_mark_attaches_to_the_previous_cell_without_advancing() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('e');
    terminal_state.print('\u{301}'); // combining acute accent → é
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.list_combining_characters(), ['\u{301}']);
    assert_eq!(cell.get_display_width(), 1); // base width unchanged
    assert_eq!(terminal_state.active_cursor().column, 1); // cursor did not advance
}

#[test]
fn multiple_combining_marks_stack_in_arrival_order() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('a');
    terminal_state.print('\u{301}'); // acute
    terminal_state.print('\u{308}'); // diaeresis
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.list_combining_characters(), ['\u{301}', '\u{308}']);
    assert_eq!(terminal_state.active_cursor().column, 1);
}

#[test]
fn combining_mark_attaches_to_a_wide_base_not_its_continuation() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('中'); // base at column 0, continuation at column 1, cursor → 2
    terminal_state.print('\u{301}'); // must land on the base at column 0, stepping over column 1
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .expect("base")
            .list_combining_characters(),
        ['\u{301}']
    );
    assert!(terminal_state
        .get_active_grid()
        .get_cell(0, 1)
        .expect("continuation")
        .list_combining_characters()
        .is_empty());
    assert_eq!(terminal_state.active_cursor().column, 2);
}

#[test]
fn combining_mark_at_line_start_is_dropped() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('\u{301}'); // nothing precedes it on the line
    assert!(terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds")
        .list_combining_characters()
        .is_empty());
    assert_eq!(terminal_state.active_cursor().column, 0); // no advance, no panic
}

#[test]
fn combining_mark_attaches_to_a_parked_get_terminal_glyph() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // row 0 full, cursor parked at column 2 with the wrap latch
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('\u{301}'); // attaches to the parked 'c' without wrapping
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 2)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'c');
    assert_eq!(cell.list_combining_characters(), ['\u{301}']);
    assert_eq!(terminal_state.active_cursor().column, 2);
    assert!(terminal_state.active_cursor().pending_wrap); // latch preserved
}

#[test]
fn wide_char_at_the_last_column_wraps_and_blanks_the_freed_cell() {
    let mut terminal_state = build_terminal_state(3, 2); // columns 0..=2; last column = 2
    print_text(&mut terminal_state, "ab"); // a@0, b@1, cursor at the last free column 2
    assert_eq!(terminal_state.active_cursor().column, 2);
    assert!(!terminal_state.active_cursor().pending_wrap);
    terminal_state.print('中'); // width 2, only column 2 free → blank it and wrap whole
    let freed = terminal_state
        .get_active_grid()
        .get_cell(0, 2)
        .expect("in bounds");
    assert_eq!(freed.get_character(), ' '); // freed column blanked
    assert_eq!(freed.get_display_width(), 1);
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('中')); // glyph starts the next line whole
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(1, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (1, 2)
    );
}

#[test]
fn wide_char_reaching_the_last_column_parks() {
    let mut terminal_state = build_terminal_state(4, 2); // last column = 3
    print_text(&mut terminal_state, "xx"); // cursor at column 2
    terminal_state.print('中'); // occupies columns 2 and 3 (the last) → park, no wrap yet
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('中'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 3)
            .map(Cell::get_display_width),
        Some(0)
    );
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.column, 3);
    assert!(cursor_position.pending_wrap);
    terminal_state.print('y'); // the deferred wrap fires here
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('y'));
}

#[test]
fn control_char_reaching_print_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('a');
    terminal_state.print('\u{0}'); // NUL: a control char, no display width
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    assert_eq!(terminal_state.active_cursor().column, 1); // nothing written, no advance
}

#[test]
fn overwriting_a_wide_base_with_a_narrow_clears_the_orphan_continuation() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('中'); // column 0 base (width 2), column 1 continuation (width 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // cursor home (0, 0)
    terminal_state.print('a'); // overwrite the base with a narrow glyph
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    // The stale continuation must be blanked, not left as a width-0 orphan.
    let cont = terminal_state
        .get_active_grid()
        .get_cell(0, 1)
        .expect("in bounds");
    assert_eq!(cont.get_character(), ' ');
    assert_eq!(cont.get_display_width(), 1);
}

#[test]
fn overwriting_a_wide_continuation_with_a_narrow_clears_the_orphan_base() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('中'); // column 0 base, column 1 continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor to (0, 1), the continuation
    terminal_state.print('a'); // overwrite the continuation with a narrow glyph
                               // The orphaned wide base must be blanked, not left claiming two columns.
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), ' ');
    assert_eq!(base.get_display_width(), 1);
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('a'));
}

#[test]
fn a_wide_write_splitting_an_adjacent_wide_clears_its_far_half() {
    let mut terminal_state = build_terminal_state(6, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor to (0, 1)
    terminal_state.print('文'); // column 1 base (width 2), column 2 continuation (width 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home (0, 0)
    terminal_state.print('中'); // wide write over columns 0,1 — splits the old glyph
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('中'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    ); // new continuation
       // The old glyph's far continuation at column 2 is orphaned → blanked.
    let far = terminal_state
        .get_active_grid()
        .get_cell(0, 2)
        .expect("in bounds");
    assert_eq!(far.get_character(), ' ');
    assert_eq!(far.get_display_width(), 1);
}

// --- Cursor motion across a wide glyph (a pair spans two columns, and motion
// counts columns) ---

#[test]
fn cursor_forward_counts_columns_so_one_step_lands_inside_a_wide_glyph() {
    let mut terminal_state = build_terminal_state(6, 2);
    print_text(&mut terminal_state, "漢字"); // 漢 on columns 0-1, 字 on columns 2-3
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home, on the 漢 base
    process_terminal_bytes(&mut terminal_state, b"\x1b[C"); // one column forward → the 漢 continuation
    assert_eq!(terminal_state.active_cursor().column, 1);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[C"); // one more → the 字 base
    assert_eq!(terminal_state.active_cursor().column, 2);
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('字'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 2)
            .map(Cell::get_display_width),
        Some(2)
    );
}

#[test]
fn cursor_back_from_past_a_wide_glyph_lands_on_its_continuation_then_its_base() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('漢'); // columns 0-1, cursor rests at column 2
    assert_eq!(terminal_state.active_cursor().column, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[D"); // back one column → the continuation
    assert_eq!(terminal_state.active_cursor().column, 1);
    process_terminal_bytes(&mut terminal_state, b"\x1b[D"); // back one more → the base
    assert_eq!(terminal_state.active_cursor().column, 0);
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('漢'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(2)
    );
}

#[test]
fn backspacing_over_a_wide_glyph_and_blanking_it_leaves_no_orphan_half() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('漢'); // columns 0-1, cursor at column 2
    terminal_state.execute(0x08); // BS → column 1
    terminal_state.execute(0x08); // BS → column 0
    assert_eq!(terminal_state.active_cursor().column, 0);

    // The two spaces a shell erases a wide glyph with. The first one lands on
    // the base and blanks the continuation with it.
    terminal_state.print(' ');
    let orphan = terminal_state
        .get_active_grid()
        .get_cell(0, 1)
        .expect("in bounds");
    assert_eq!(orphan.get_character(), ' ');
    assert_eq!(orphan.get_display_width(), 1);

    terminal_state.print(' ');
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), ' ');
    assert_eq!(base.get_display_width(), 1);
    let right = terminal_state
        .get_active_grid()
        .get_cell(0, 1)
        .expect("in bounds");
    assert_eq!(right.get_character(), ' ');
    assert_eq!(right.get_display_width(), 1);
    assert_eq!(terminal_state.active_cursor().column, 2);
}

// --- Wide-pair integrity across erase / insert / delete cell ops ---

#[test]
fn el_to_eol_from_a_continuation_column_clears_the_orphan_base() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('中'); // column 0 base (w2), column 1 continuation (w0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor onto the continuation (0, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[0K"); // erase cursor→EOL: clears column 1, splits the pair
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), ' '); // orphaned base blanked
    assert_eq!(base.get_display_width(), 1);
}

#[test]
fn el_to_cursor_ending_on_a_wide_base_clears_the_orphan_continuation() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('中'); // column 0 base, column 1 continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // cursor home (0, 0) = the base
    process_terminal_bytes(&mut terminal_state, b"\x1b[1K"); // erase SOL→cursor: clears column 0, orphans column 1
    let cont = terminal_state
        .get_active_grid()
        .get_cell(0, 1)
        .expect("in bounds");
    assert_eq!(cont.get_character(), ' ');
    assert_eq!(cont.get_display_width(), 1);
}

#[test]
fn ed_to_end_from_a_continuation_column_clears_the_orphan_base() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('中'); // (0,0) base, (0,1) continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor onto the continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[0J"); // erase cursor→end of screen: clears (0,1)
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), ' ');
    assert_eq!(base.get_display_width(), 1);
}

#[test]
fn ich_between_a_wide_pair_clears_both_orphaned_halves() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('中'); // column 0 base, column 1 continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor onto the continuation (0, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[@"); // insert 1 blank at column 1, splitting the pair
                                                            // base@0 lost its continuation; the displaced continuation@2 lost its base.
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 2)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some(' '));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some(' '));
}

#[test]
fn ich_truncating_a_wide_continuation_off_the_edge_clears_the_orphan_base() {
    let mut terminal_state = build_terminal_state(4, 2);
    print_text(&mut terminal_state, "xx"); // columns 0,1
    terminal_state.print('中'); // column 2 base, column 3 continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home
    process_terminal_bytes(&mut terminal_state, b"\x1b[@"); // insert pushes the pair right; continuation falls off
                                                            // the base sits at the last column with no continuation → blanked.
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 3)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 3), Some(' '));
}

#[test]
fn dch_deleting_a_continuation_clears_the_orphan_base() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('中'); // column 0 base, column 1 continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor onto the continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[P"); // delete it, pulling the line left
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some(' '));
}

#[test]
fn cell_ops_leave_an_untouched_wide_pair_intact() {
    let mut terminal_state = build_terminal_state(6, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // cursor to column 2
    terminal_state.print('中'); // column 2 base, column 3 continuation
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home
    process_terminal_bytes(&mut terminal_state, b"\x1b[1K"); // clear SOL→cursor (column 0 only) — pair untouched
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('中'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 2)
            .map(Cell::get_display_width),
        Some(2)
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 3)
            .map(Cell::get_display_width),
        Some(0)
    );
}

// --- Multi-codepoint grapheme clusters: emoji ZWJ / VS16 / modifiers / flags ---

#[test]
fn vs16_promotes_a_text_glyph_to_a_wide_emoji_cell() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, text presentation, width 1
    terminal_state.print('\u{FE0F}'); // VS16 → emoji presentation, width 2
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{2764}');
    assert_eq!(base.list_combining_characters(), ['\u{FE0F}']);
    assert_eq!(base.get_display_width(), 2); // promoted from 1 to 2
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    ); // claimed continuation
    assert_eq!(terminal_state.active_cursor().column, 2); // advanced over both columns
}

#[test]
fn zwj_emoji_sequence_folds_into_one_wide_cell() {
    let mut terminal_state = build_terminal_state(10, 2);
    print_text(
        &mut terminal_state,
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
    ); // 👨‍👩‍👧
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{1F468}');
    assert_eq!(
        base.list_combining_characters(),
        ['\u{200D}', '\u{1F469}', '\u{200D}', '\u{1F467}']
    );
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(terminal_state.active_cursor().column, 2); // one wide glyph, not three
}

#[test]
fn skin_tone_modifier_folds_onto_the_base() {
    let mut terminal_state = build_terminal_state(6, 2);
    print_text(&mut terminal_state, "\u{1F44D}\u{1F3FD}"); // 👍 + medium skin tone
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{1F44D}');
    assert_eq!(base.list_combining_characters(), ['\u{1F3FD}']);
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(terminal_state.active_cursor().column, 2);
}

#[test]
fn regional_indicator_pair_is_one_flag_cell() {
    let mut terminal_state = build_terminal_state(6, 2);
    print_text(&mut terminal_state, "\u{1F1EF}\u{1F1F5}"); // 🇯 + 🇵 = JP flag
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{1F1EF}');
    assert_eq!(base.list_combining_characters(), ['\u{1F1F5}']);
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(terminal_state.active_cursor().column, 2);
}

#[test]
fn separate_emoji_without_a_joiner_stay_two_cells_each() {
    let mut terminal_state = build_terminal_state(10, 2);
    print_text(&mut terminal_state, "\u{1F468}\u{1F469}"); // 👨👩 — no ZWJ, two graphemes
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('\u{1F468}'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(2)
    );
    assert!(terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds")
        .list_combining_characters()
        .is_empty());
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('\u{1F469}'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 2)
            .map(Cell::get_display_width),
        Some(2)
    );
    assert_eq!(terminal_state.active_cursor().column, 4);
}

#[test]
fn a_control_byte_breaks_a_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, width 1
    process_terminal_bytes(&mut terminal_state, b"\n"); // LF ends the run
    terminal_state.print('\u{FE0F}'); // VS16 has no cluster to join → dropped
    let heart = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(heart.get_character(), '\u{2764}');
    assert_eq!(heart.get_display_width(), 1); // NOT promoted across the control byte
    assert!(heart.list_combining_characters().is_empty());
}

#[test]
fn vs16_promotion_at_the_last_column_wraps_to_the_next_line() {
    let mut terminal_state = build_terminal_state(3, 3); // last column = 2
    print_text(&mut terminal_state, "ab"); // a@0, b@1, cursor at column 2
    terminal_state.print('\u{2764}'); // heart width 1 at column 2, parks
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('\u{FE0F}'); // VS16 promotes → no room at the edge → move whole cluster down
    let freed = terminal_state
        .get_active_grid()
        .get_cell(0, 2)
        .expect("in bounds");
    assert_eq!(freed.get_character(), ' '); // old narrow cell blanked
    let base = terminal_state
        .get_active_grid()
        .get_cell(1, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{2764}');
    assert_eq!(base.list_combining_characters(), ['\u{FE0F}']);
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(1, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (1, 2)
    );
}

#[test]
fn vs16_promotes_the_immediately_preceding_glyph_only() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, width 1
    terminal_state.print('X'); // boundary → the heart's cluster run ends here
    terminal_state.print('\u{FE0F}'); // VS16 belongs to X's cluster, must not reach back to the heart
    let heart = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(heart.get_character(), '\u{2764}');
    assert_eq!(heart.get_display_width(), 1); // untouched — not promoted
    assert!(heart.list_combining_characters().is_empty());
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('X'));
}

#[test]
fn a_cursor_move_breaks_a_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, width 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // CUP — any CSI ends the run
    terminal_state.print('\u{FE0F}'); // VS16 has no cluster to join → dropped
    let heart = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(heart.get_display_width(), 1); // not promoted across the cursor move
    assert!(heart.list_combining_characters().is_empty());
}

#[test]
fn a_dcs_passthrough_breaks_a_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, width 1
    process_terminal_bytes(&mut terminal_state, b"\x1bPq\x1b\\"); // DCS ... ST — a non-printing control string
    terminal_state.print('\u{FE0F}'); // VS16 must NOT promote the heart across the DCS
    let heart = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(heart.get_character(), '\u{2764}');
    assert_eq!(heart.get_display_width(), 1); // not promoted across the DCS
    assert!(heart.list_combining_characters().is_empty());
}

#[test]
fn a_dcs_terminated_by_c1_st_breaks_a_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('e'); // base
    process_terminal_bytes(&mut terminal_state, b"\x1bPq\x9c"); // DCS closed by the 8-bit C1 ST (0x9C),
                                                                // whose only Perform callback is `unhook`
    terminal_state.print('\u{301}'); // combining acute must NOT fold onto 'e'
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert!(cell.list_combining_characters().is_empty()); // the DCS ended the run
}

#[test]
fn an_apc_string_breaks_a_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('e'); // base
    process_terminal_bytes(&mut terminal_state, b"\x1b_payload\x1b\\"); // APC ... ST — silently consumed by vte
    terminal_state.print('\u{301}'); // combining acute must NOT fold onto 'e'
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert!(cell.list_combining_characters().is_empty());
}

#[test]
fn a_style_only_sgr_does_not_break_a_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('e'); // base at (0, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[31m"); // SGR set fg red — pen only, no cursor move
    terminal_state.print('\u{301}'); // combining acute must still fold onto the 'e'
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.list_combining_characters(), ['\u{301}']); // attached across the SGR
    assert_eq!(cell.get_display_width(), 1);
    assert_eq!(terminal_state.active_cursor().column, 1); // no advance
}

#[test]
fn a_style_only_sgr_does_not_break_a_vs16_promotion() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, text presentation, width 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m"); // bold — pen only, no cursor move
    terminal_state.print('\u{FE0F}'); // VS16 must still promote the heart across the SGR
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{2764}');
    assert_eq!(base.list_combining_characters(), ['\u{FE0F}']);
    assert_eq!(base.get_display_width(), 2); // promoted across the SGR
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    );
    assert_eq!(terminal_state.active_cursor().column, 2);
}

#[test]
fn an_sgr_preserved_cluster_does_not_fold_a_new_mark_onto_the_old_base() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart at (0, 0), width 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m"); // SGR preserves the heart's cluster run
    terminal_state.print('X'); // boundary → starts a fresh cluster at (0, 1)
    terminal_state.print('\u{FE0F}'); // VS16 belongs to X, must NOT reach back to the heart
    let heart = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(heart.get_display_width(), 1); // untouched — not promoted
    assert!(heart.list_combining_characters().is_empty());
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('X'));
}

#[test]
fn an_overlong_ignored_sgr_shaped_csi_breaks_a_cluster_run() {
    // A CSI with more parameters than vte keeps (32) is flagged `ignore` and
    // dropped. It ends in `m` but is not an applied SGR: it breaks the cluster
    // like every other non-printing CSI.
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('e'); // base
    let mut seq = Vec::from(&b"\x1b["[..]);
    for _ in 0..40 {
        seq.extend_from_slice(b"0;"); // 40 params overflow vte's 32-param buffer
    }
    seq.push(b'm');
    process_terminal_bytes(&mut terminal_state, &seq);
    terminal_state.print('\u{301}'); // combining acute must NOT fold onto 'e'
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert!(cell.list_combining_characters().is_empty()); // the malformed CSI ended the run
}

#[test]
fn a_wrapped_vs16_promotion_clears_a_wide_glyph_it_lands_on() {
    let mut terminal_state = build_terminal_state(4, 3); // last column = 3
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // cursor -> (1, 1)
    terminal_state.print('中'); // destination row: base@(1,1), continuation@(1,2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home (0, 0)
    print_text(&mut terminal_state, "xyz"); // fill row 0 columns 0..2, cursor at the last column 3
    terminal_state.print('\u{2764}'); // heart width 1 parks at (0, 3)
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('\u{FE0F}'); // VS16 promotes -> no room at the edge -> wrap the cluster to row 1
                                      // The promoted pair overwrites (1,0)+(1,1), taking 中's base at column 1;
                                      // 中's old continuation at column 2 is cleared with it.
    let base = terminal_state
        .get_active_grid()
        .get_cell(1, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{2764}');
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(1, 1)
            .map(Cell::get_display_width),
        Some(0)
    ); // the pair's continuation
    let orphan = terminal_state
        .get_active_grid()
        .get_cell(1, 2)
        .expect("in bounds");
    assert_eq!(orphan.get_character(), ' '); // 中's stale continuation cleared
    assert_eq!(orphan.get_display_width(), 1); // not a width-0 orphan
}

#[test]
fn an_in_place_vs16_promotion_clears_a_wide_glyph_it_claims() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor -> (0, 1)
    terminal_state.print('中'); // wide glyph at columns 1-2 (base@1, continuation@2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home (0, 0)
    terminal_state.print('\u{2764}'); // heart width 1 at column 0; 中 left intact at 1-2
    terminal_state.print('\u{FE0F}'); // VS16 promotes the heart in place, claiming column 1
                                      // The promotion overwrites 中's base at column 1; 中's old
                                      // continuation at column 2 is cleared with it.
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{2764}');
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(0)
    ); // heart's continuation
    let orphan = terminal_state
        .get_active_grid()
        .get_cell(0, 2)
        .expect("in bounds");
    assert_eq!(orphan.get_character(), ' '); // 中's stale continuation cleared
    assert_eq!(orphan.get_display_width(), 1); // not a width-0 orphan
}

#[test]
fn a_wide_glyph_in_a_one_column_pane_degrades_to_a_narrow_cell() {
    let mut terminal_state = build_terminal_state(1, 2); // 1 column — no room for a wide pair
    terminal_state.print('中'); // cannot occupy two cells in a single-column pane
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), '中');
    assert_eq!(cell.get_display_width(), 1); // narrow, NOT a width-2 base with no continuation
}

#[test]
fn wide_glyphs_in_a_one_column_pane_do_not_scroll_thrash() {
    let mut terminal_state = build_terminal_state(1, 3); // 1 column, 3 rows
    terminal_state.print('中'); // stored narrow at (0, 0), no wrap
    terminal_state.print('文'); // advances one line; must not scroll the first glyph away
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('中')); // first glyph still on row 0
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('文'));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(1, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
}

#[test]
fn a_vs16_promotion_in_a_one_column_pane_never_orphans_a_wide_base() {
    let mut terminal_state = build_terminal_state(1, 3); // 1 column
    terminal_state.print('\u{2764}'); // heart width 1 at (0, 0)
    terminal_state.print('\u{FE0F}'); // VS16 asks for width 2; a 1-column pane has no room
                                      // No cell is left a width-2 base: a 1-column pane holds
                                      // no continuation.
    for row_index in 0..3 {
        assert_ne!(
            terminal_state
                .get_active_grid()
                .get_cell(row_index, 0)
                .map(Cell::get_display_width),
            Some(2),
            "row {row_index}: a width-2 base cannot exist in a 1-column pane"
        );
    }
}

#[test]
fn combining_marks_are_capped_to_bound_per_cell_memory() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('a'); // base at (0, 0)
    for _ in 0..10_000 {
        terminal_state.print('\u{0301}'); // flood of combining acutes ("zalgo")
    }
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'a');
    assert_eq!(
        cell.list_combining_characters().len(),
        MAX_GRAPHEME_CONTINUATION_COUNT
    ); // bounded, not 10_000
    assert_eq!(terminal_state.active_cursor().column, 1); // never advanced
}

#[test]
fn wide_at_edge_wrap_clears_an_existing_wide_pair_it_splits() {
    let mut terminal_state = build_terminal_state(3, 2); // last column = 2
    terminal_state.print('x'); // column 0
    terminal_state.print('中'); // base column 1, continuation column 2 (the last column)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // CUP onto the continuation (0,2); clears wrap + cluster
    terminal_state.print('文'); // wide at the last column → wide-at-edge wrap; must not orphan 中's base
                                // 中's base at column 1 was orphaned by blanking column 2 → cleared.
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some(' '));
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('文')); // new glyph wrapped to the next line
}

#[test]
fn a_boundary_zero_width_char_breaks_the_cluster_run() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{2764}'); // heart, width 1
    terminal_state.print('\u{200B}'); // ZWSP — width 0 but a grapheme boundary; ends the run
    terminal_state.print('\u{FE0F}'); // VS16 must NOT reach back across the ZWSP to the heart
    let heart = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(heart.get_character(), '\u{2764}');
    assert_eq!(heart.get_display_width(), 1); // not promoted across the boundary
    assert!(heart.list_combining_characters().is_empty());
}

#[test]
fn vs15_demotes_a_wide_emoji_base_to_a_narrow_text_get_terminal_glyph() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('\u{26A1}'); // ⚡ high voltage, default emoji presentation → width 2
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(2)
    );
    assert_eq!(terminal_state.active_cursor().column, 2);
    terminal_state.print('\u{FE0E}'); // VS15 → text presentation → width 1
    let base = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{26A1}');
    assert_eq!(base.list_combining_characters(), ['\u{FE0E}']);
    assert_eq!(base.get_display_width(), 1); // demoted from 2 to 1
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(1)
    ); // continuation cleared
    assert_eq!(terminal_state.active_cursor().column, 1); // cursor stepped back over the freed column
    assert!(!terminal_state.active_cursor().pending_wrap);
    terminal_state.print('Z'); // next glyph lands at the freed column, not two ahead
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('Z'));
}

#[test]
fn vs16_promotion_wraps_correctly_in_a_two_column_grid() {
    let mut terminal_state = build_terminal_state(2, 2); // last column = 1 — the narrow-grid promotion edge
    terminal_state.print('a'); // column 0
    terminal_state.print('\u{2764}'); // heart width 1 at column 1 (last), parks
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('\u{FE0F}'); // VS16 promotes; no room at column 1 → move the whole cluster down
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some(' ')); // old narrow cell blanked
    let base = terminal_state
        .get_active_grid()
        .get_cell(1, 0)
        .expect("in bounds");
    assert_eq!(base.get_character(), '\u{2764}');
    assert_eq!(base.get_display_width(), 2);
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(1, 1)
            .map(Cell::get_display_width),
        Some(0)
    ); // continuation fills the row
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.column, 1); // parked at the last column
    assert!(cursor_position.pending_wrap);
}

#[test]
fn linefeed_pushes_the_top_primary_line_into_scrollback() {
    let mut terminal_state = build_terminal_state(4, 2); // two rows; bottom margin is row 1
    print_text(&mut terminal_state, "ab"); // row 0 = "ab.."
    terminal_state.linefeed(); // row 0 -> 1 (descends; not yet at the bottom)
    terminal_state.linefeed(); // at the bottom: the region scrolls, row 0 scrolls off
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    let captured = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .front()
        .expect("one retained row")
        .0
        .as_slice();
    assert_eq!(captured[0].get_character(), 'a');
    assert_eq!(captured[1].get_character(), 'b');
}

#[test]
fn linefeed_on_the_alternate_screen_does_not_feed_scrollback() {
    let mut terminal_state = build_terminal_state(4, 2);
    terminal_state.active_screen = Screen::Alternate; // the alternate never feeds history
    terminal_state.linefeed(); // alt cursor 0 -> 1
    terminal_state.linefeed(); // at the bottom: the alternate scrolls, but feeds nothing
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn linefeed_below_a_top_margin_discards_rather_than_feeds() {
    let mut terminal_state = build_terminal_state(4, 3); // three rows
    *terminal_state.scroll_region_mut() = Some((1, 2)); // region top margin = row 1
    terminal_state.active_cursor_mut().row = 2; // park at the region's bottom margin
    terminal_state.linefeed(); // scrolls within rows 1..=2; top margin != 0 -> no feed
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn linefeed_in_a_region_anchored_at_the_top_feeds_scrollback() {
    let mut terminal_state = build_terminal_state(4, 3);
    *terminal_state.scroll_region_mut() = Some((0, 1)); // region top margin = row 0
    terminal_state.active_cursor_mut().row = 1; // the region's bottom margin
    terminal_state.linefeed();
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
}

#[test]
fn successive_bottom_linefeeds_accumulate_scrollback() {
    let mut terminal_state = build_terminal_state(4, 2);
    terminal_state.active_cursor_mut().row = 1; // sit at the bottom row
    terminal_state.linefeed();
    terminal_state.linefeed();
    terminal_state.linefeed();
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 3);
}

#[test]
fn linefeed_on_a_full_height_single_row_screen_always_scrolls() {
    // rows == 1: get_scroll_region_bounds() resolves to (0, 0) with no DECSTBM. Every
    // linefeed scrolls the one row.
    let mut terminal_state = build_terminal_state(2, 1);
    print_text(&mut terminal_state, "aa"); // fills the only row; cursor parks (pending wrap)
    terminal_state.execute(b'\n');
    terminal_state.execute(b'\r');
    print_text(&mut terminal_state, "bb");
    assert_eq!(get_row_text(&terminal_state, 0), "bb");
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    let captured = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .front()
        .expect("one retained row")
        .0
        .as_slice();
    assert_eq!(captured[0].get_character(), 'a');
    assert_eq!(captured[1].get_character(), 'a');
    assert_eq!(terminal_state.active_cursor().row, 0); // pinned to the only row
}

#[test]
fn ordinary_linefeeds_evict_the_oldest_scrollback_row_at_the_line_cap() {
    // A 1-row screen scrolls on every linefeed: three linefeeds push three
    // rows into scrollback through the print/linefeed path. A cap of 1 keeps
    // only the newest row and counts the other two as dropped.
    let mut terminal_state = build_terminal_state(2, 1);
    terminal_state.scrollback =
        Scrollback::from_scrollback_limit(ScrollbackLimit::from_line_and_byte_limits(1, 100_000));
    for row_text in ["aa", "bb", "cc"] {
        print_text(&mut terminal_state, row_text);
        terminal_state.execute(b'\n');
        terminal_state.execute(b'\r');
    }
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    let history: String = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .front()
        .expect("one retained row")
        .0
        .iter()
        .map(Cell::get_character)
        .collect();
    assert_eq!(history, "cc");
    assert_eq!(terminal_state.get_scrollback().get_dropped_line_count(), 2);
    assert_eq!(get_row_text(&terminal_state, 0), "  "); // the only row is blank after the last scroll
}

#[test]
fn su_on_a_top_anchored_region_feeds_scrollback() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "ab"); // row 0 = "ab "
    process_terminal_bytes(&mut terminal_state, b"\x1b[S"); // SU by 1; full region starts at row 0
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    let captured = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .front()
        .expect("one retained row")
        .0
        .as_slice();
    assert_eq!(captured[0].get_character(), 'a');
    assert_eq!(captured[1].get_character(), 'b');
}

#[test]
fn su_by_n_captures_each_departing_top_row_oldest_first() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // rows "abc" / "def" / "ghi"
    process_terminal_bytes(&mut terminal_state, b"\x1b[2S"); // SU by 2: rows 0 and 1 scroll off the top
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 2);
    let history: Vec<String> = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .map(|(row, _)| row.iter().map(Cell::get_character).collect())
        .collect();
    assert_eq!(history, vec!["abc", "def"]); // oldest (top) first
}

#[test]
fn su_on_a_region_below_the_top_does_not_feed() {
    let mut terminal_state = build_terminal_state(3, 3);
    *terminal_state.scroll_region_mut() = Some((1, 2)); // region top margin = row 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[S");
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn su_on_the_alternate_screen_does_not_feed() {
    let mut terminal_state = build_terminal_state(3, 2);
    terminal_state.active_screen = Screen::Alternate;
    process_terminal_bytes(&mut terminal_state, b"\x1b[S");
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn dl_with_the_cursor_on_row_0_feeds_scrollback() {
    // DL at row 0 scrolls the top line off the screen and into history.
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "ab"); // row 0 = "ab ", cursor stays on row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[M"); // DL by 1 at the cursor row (0)
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    let captured = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .front()
        .expect("one retained row")
        .0
        .as_slice();
    assert_eq!(captured[0].get_character(), 'a');
    assert_eq!(captured[1].get_character(), 'b');
}

#[test]
fn dl_below_row_0_is_an_interior_delete_and_does_not_feed() {
    let mut terminal_state = build_terminal_state(3, 3);
    terminal_state.active_cursor_mut().row = 1; // interior delete, nothing leaves the top
    process_terminal_bytes(&mut terminal_state, b"\x1b[M");
    assert!(terminal_state.get_scrollback().is_empty());
}

// --- Bracketed paste + mouse mode terminal_state (DEC private modes) ---

#[test]
fn modes_start_at_their_defaults() {
    let terminal_state = build_terminal_state(5, 3);
    assert!(!terminal_state.is_bracketed_paste_enabled());
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::Off);
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Default);
    assert!(!terminal_state.is_alternate_scroll_enabled());
}

#[test]
fn bracketed_paste_enables_and_disables() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?2004h");
    assert!(terminal_state.is_bracketed_paste_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?2004l");
    assert!(!terminal_state.is_bracketed_paste_enabled());
}

#[test]
fn each_mouse_tracking_mode_sets_its_level() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?9h");
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::X10);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000h");
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::Normal);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1002h");
    assert_eq!(
        terminal_state.get_mouse_tracking(),
        MouseTracking::ButtonMotion
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1003h");
    assert_eq!(
        terminal_state.get_mouse_tracking(),
        MouseTracking::AnyMotion
    );
}

#[test]
fn disabling_the_active_mouse_tracking_mode_turns_it_off() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000h");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000l");
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::Off);
}

#[test]
fn a_new_mouse_tracking_mode_replaces_the_previous_one() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000h"); // Normal
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1003h"); // AnyMotion supersedes
    assert_eq!(
        terminal_state.get_mouse_tracking(),
        MouseTracking::AnyMotion
    );
}

#[test]
fn disabling_a_non_active_tracking_mode_leaves_the_active_one() {
    // A reset turns reporting off only when it names the active level. Resetting
    // a mode that is not the active one is a no-op; the active mode stays set.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1003h"); // AnyMotion
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000l"); // resets a different mode number
    assert_eq!(
        terminal_state.get_mouse_tracking(),
        MouseTracking::AnyMotion
    );
}

#[test]
fn disabling_the_active_tracking_mode_after_a_replace_turns_it_off() {
    // After a replace (`?1000h` then `?1003h` -> AnyMotion), resetting the active
    // mode (`?1003l`) turns reporting off; the superseded `?1000` is gone.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000h"); // Normal
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1003h"); // AnyMotion supersedes
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1003l"); // reset the active mode
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::Off);
}

#[test]
fn each_mouse_encoding_mode_sets_its_form_and_resets_to_default() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1005h");
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Utf8);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1006h");
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Sgr);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1015h");
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Urxvt);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1015l"); // reset the active encoding
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Default);
}

#[test]
fn disabling_a_non_active_encoding_leaves_the_active_one() {
    // A reset returns to the default only when it names the active encoding.
    // Resetting a different encoding is a no-op; the active one stays set.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1005h"); // Utf8 active
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1006l"); // reset a non-active encoding
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Utf8);
}

#[test]
fn disabling_the_active_encoding_after_a_replace_returns_to_default() {
    // After a replace (`?1005h` then `?1006h` -> Sgr), resetting the active
    // encoding (`?1006l`) returns to Default; the superseded Utf8 is gone.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1005h"); // Utf8
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1006h"); // Sgr supersedes
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1006l"); // reset the active encoding
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Default);
}

#[test]
fn mouse_tracking_and_encoding_are_independent() {
    // Enabling SGR encoding leaves the tracking level set, and vice versa.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000h"); // tracking: Normal
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1006h"); // encoding: SGR
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::Normal);
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Sgr);
}

#[test]
fn one_decset_list_sets_tracking_and_encoding_together() {
    // Tracking and encoding in a single DECSET list.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000;1006h");
    assert_eq!(terminal_state.get_mouse_tracking(), MouseTracking::Normal);
    assert_eq!(terminal_state.get_mouse_encoding(), MouseEncoding::Sgr);
}

#[test]
fn alt_scroll_enables_and_disables() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1007h");
    assert!(terminal_state.is_alternate_scroll_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1007l");
    assert!(!terminal_state.is_alternate_scroll_enabled());
}

#[test]
fn sixel_modes_start_with_scrolling_and_private_registers() {
    let terminal_state = build_terminal_state(5, 3);
    assert!(terminal_state.modes.sixel_scrolling);
    assert!(terminal_state.modes.sixel_private_color_registers);
    assert!(!terminal_state.modes.sixel_cursor_right);
}

#[test]
fn sixel_private_modes_set_and_reset_their_terminal_state() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?80;1070;8452h");
    assert!(!terminal_state.modes.sixel_scrolling);
    assert!(terminal_state.modes.sixel_private_color_registers);
    assert!(terminal_state.modes.sixel_cursor_right);

    process_terminal_bytes(&mut terminal_state, b"\x1b[?80;1070;8452l");
    assert!(terminal_state.modes.sixel_scrolling);
    assert!(!terminal_state.modes.sixel_private_color_registers);
    assert!(!terminal_state.modes.sixel_cursor_right);
}

// --- Absolute / relative cursor positioning, tab moves, erase-char ---

#[test]
fn cha_sets_an_absolute_one_based_column() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[5G"); // column 5 -> 0-based 4, row unchanged
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 4));
}

#[test]
fn cha_clamps_past_the_last_column() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99G");
    assert_eq!(terminal_state.active_cursor().column, 9); // clamped to the last column
}

#[test]
fn cha_with_no_argument_homes_the_column() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[5;5H"); // (4, 4)
    process_terminal_bytes(&mut terminal_state, b"\x1b[G"); // default 1 -> column 0
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (4, 0));
}

#[test]
fn hpa_backtick_is_the_same_as_cha() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[5\x60"); // HPA `CSI 5 \`` -> column 4
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 4));
}

#[test]
fn cha_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // parks at the last column with the latch set
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2G"); // column 2 -> 0-based 1
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.column, 1);
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn vpa_sets_an_absolute_one_based_row() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;4H"); // (0, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[3d"); // row 3 -> 0-based 2, column unchanged
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 3));
}

#[test]
fn vpa_clamps_past_the_last_row() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99d");
    assert_eq!(terminal_state.active_cursor().row, 4); // clamped to the last row
}

#[test]
fn hpr_moves_forward_like_cuf_and_clamps() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // column 2
    process_terminal_bytes(&mut terminal_state, b"\x1b[2a"); // HPR forward 2 -> column 4
    assert_eq!(terminal_state.active_cursor().column, 4);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99a"); // clamps to the last column
    assert_eq!(terminal_state.active_cursor().column, 9);
}

#[test]
fn vpr_moves_down_like_cud_and_clamps() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H"); // row 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[2e"); // VPR down 2 -> row 3
    assert_eq!(terminal_state.active_cursor().row, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[99e"); // clamps to the last row
    assert_eq!(terminal_state.active_cursor().row, 4);
}

#[test]
fn cnl_moves_down_to_column_zero() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4H"); // (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1E"); // next line: down 1, column 0
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 0));
}

#[test]
fn cnl_clamps_to_the_last_row_without_scrolling() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // rows "abc" / "def" / "ghi"
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;2H"); // (2, 1) — the last row
    process_terminal_bytes(&mut terminal_state, b"\x1b[5E"); // down 5 clamps; no scroll
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (2, 0));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a')); // content did not scroll up
}

#[test]
fn cpl_moves_up_to_column_zero() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[1F"); // previous line: up 1, column 0
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 0));
}

#[test]
fn cpl_clamps_at_row_zero_without_scrolling() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // (0, 1) — the top row
    process_terminal_bytes(&mut terminal_state, b"\x1b[5F"); // up 5 clamps; no scroll
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
    assert_eq!(get_terminal_glyph(&terminal_state, 2, 0), Some('g')); // content did not scroll down
}

#[test]
fn cnl_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // parks with the latch set
    process_terminal_bytes(&mut terminal_state, b"\x1b[1E");
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn cht_advances_to_the_next_tab_stop() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[I"); // default 1 stop, from column 0 -> 8
    assert_eq!(terminal_state.active_cursor().column, 8);
}

#[test]
fn cht_from_a_tab_stop_advances_a_full_eight() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[9G"); // column 8 (a stop)
    process_terminal_bytes(&mut terminal_state, b"\x1b[I");
    assert_eq!(terminal_state.active_cursor().column, 16);
}

#[test]
fn cht_count_advances_multiple_stops() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2I"); // two stops from column 0 -> 8 -> 16
    assert_eq!(terminal_state.active_cursor().column, 16);
}

#[test]
fn cht_clamps_to_the_last_column() {
    let mut terminal_state = build_terminal_state(20, 3); // last column 19
    process_terminal_bytes(&mut terminal_state, b"\x1b[9I"); // far more stops than fit
    assert_eq!(terminal_state.active_cursor().column, 19);
}

#[test]
fn cht_at_the_last_column_stays_put() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[20G"); // last column (19)
    process_terminal_bytes(&mut terminal_state, b"\x1b[I");
    assert_eq!(terminal_state.active_cursor().column, 19);
}

#[test]
fn cbt_retreats_to_the_previous_tab_stop() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[11G"); // column 10
    process_terminal_bytes(&mut terminal_state, b"\x1b[Z");
    assert_eq!(terminal_state.active_cursor().column, 8);
}

#[test]
fn cbt_from_a_tab_stop_retreats_a_full_eight() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[17G"); // column 16 (a stop)
    process_terminal_bytes(&mut terminal_state, b"\x1b[Z");
    assert_eq!(terminal_state.active_cursor().column, 8);
}

#[test]
fn cbt_at_column_zero_stays_put() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[Z");
    assert_eq!(terminal_state.active_cursor().column, 0);
}

#[test]
fn cbt_count_retreats_multiple_stops() {
    let mut terminal_state = build_terminal_state(20, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[18G"); // column 17
    process_terminal_bytes(&mut terminal_state, b"\x1b[2Z"); // 17 -> 16 -> 8
    assert_eq!(terminal_state.active_cursor().column, 8);
}

#[test]
fn cbt_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(20, 2);
    print_text(&mut terminal_state, "aaaaaaaaaaaaaaaaaaaa"); // 20 chars -> parks at column 19
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[Z"); // 19 -> 16
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.column, 16);
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn ech_erases_n_cells_in_place_without_shifting() {
    let mut terminal_state = build_terminal_state(6, 2);
    print_text(&mut terminal_state, "abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2G"); // column 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[2X"); // erase 2 cells in place
    assert_eq!(get_row_text(&terminal_state, 0), "a  de "); // d, e stay put; no shift
}

#[test]
fn ech_with_no_argument_erases_one_cell() {
    let mut terminal_state = build_terminal_state(6, 2);
    print_text(&mut terminal_state, "abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2G"); // column 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[X"); // default 1 cell
    assert_eq!(get_row_text(&terminal_state, 0), "a cde ");
}

#[test]
fn ech_clamps_the_count_to_the_line_end() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abc");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2G"); // column 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[99X"); // far past the line end -> clamps
    assert_eq!(get_row_text(&terminal_state, 0), "a    ");
}

#[test]
fn ech_fills_with_the_current_background_only() {
    let mut terminal_state = build_terminal_state(5, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31;44m"); // bold + fg red + bg blue
    process_terminal_bytes(&mut terminal_state, b"\x1b[3X"); // erase 3 cells from column 0
    let background_style =
        build_style_with_mutator(|style| style.set_background_color(Color::Indexed(4))); // background only — bold + fg dropped
    assert!((0..3).all(|column_index| {
        terminal_state
            .get_active_grid()
            .get_cell(0, column_index)
            .map(Cell::get_style)
            == Some(background_style)
    }));
}

#[test]
fn ech_erases_the_parked_glyph_and_clears_the_wrap_latch() {
    // ECH erases from the cursor column: the parked last-column glyph goes and
    // the wrap latch clears. The next print overwrites in place.
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // parks at column 2 with the latch
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[X"); // erases the parked glyph
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some(' '));
    assert!(!terminal_state.active_cursor().pending_wrap); // latch cleared
    terminal_state.print('d');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('d')); // overwrote at the last column
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // did NOT wrap
}

#[test]
fn ech_repairs_a_wide_glyph_whose_base_it_erases() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('中'); // wide base at column 0, continuation at column 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[1G"); // column 0 (the base)
    process_terminal_bytes(&mut terminal_state, b"\x1b[X"); // erase the base
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some(' '));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    // The orphaned continuation is repaired to a blank narrow cell.
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some(' '));
}

#[test]
fn ech_starting_on_a_wide_continuation_repairs_the_base() {
    let mut terminal_state = build_terminal_state(6, 2);
    terminal_state.print('中'); // wide base at column 0, continuation at column 1
    process_terminal_bytes(&mut terminal_state, b"\x1b[2G"); // column 1 (the continuation)
    process_terminal_bytes(&mut terminal_state, b"\x1b[X"); // erase the continuation
                                                            // The orphaned wide base is repaired to a blank narrow cell.
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some(' '));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .map(Cell::get_display_width),
        Some(1)
    );
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some(' '));
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 1)
            .map(Cell::get_display_width),
        Some(1)
    ); // pair fully unwound
}

#[test]
fn cup_clears_the_pending_wrap_latch_through_goto() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // parks with the latch set
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;1H"); // home, via the shared cursor path
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn vpa_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // parks at (0, 2) with the latch set
    process_terminal_bytes(&mut terminal_state, b"\x1b[2d"); // row 2 -> 0-based 1, column unchanged
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (1, 2));
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn cpl_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "abc"); // parks with the latch set
    process_terminal_bytes(&mut terminal_state, b"\x1b[1F"); // previous line clamps to row 0, column 0
    let cursor_position = terminal_state.active_cursor();
    assert_eq!((cursor_position.row, cursor_position.column), (0, 0));
    assert!(!cursor_position.pending_wrap);
}

#[test]
fn cht_clears_the_pending_wrap_latch() {
    let mut terminal_state = build_terminal_state(20, 2);
    print_text(&mut terminal_state, "aaaaaaaaaaaaaaaaaaaa"); // 20 chars -> parks at column 19
    assert!(terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[I"); // already at the last column: stays, but clears the latch
    let cursor_position = terminal_state.active_cursor();
    assert_eq!(cursor_position.column, 19);
    assert!(!cursor_position.pending_wrap);
}

// --- Charset designation + DEC line-drawing (G0-G3, SI/SO) ---

#[test]
fn dec_line_drawing_renders_box_glyphs() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0lqqqk"); // designate G0 = DEC line drawing, then print
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('┌'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 3), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('┐'));
}

#[test]
fn ascii_designation_returns_to_passthrough() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0l"); // G0 = DEC: 'l' -> box corner
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('┌'));
    process_terminal_bytes(&mut terminal_state, b"\x1b(Bl"); // G0 = ASCII: 'l' -> literal
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('l'));
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
    for &(source_character, expected_glyph) in table {
        let mut terminal_state = build_terminal_state(4, 2);
        process_terminal_bytes(&mut terminal_state, b"\x1b(0");
        terminal_state.print(source_character);
        assert_eq!(
            get_terminal_glyph(&terminal_state, 0, 0),
            Some(expected_glyph),
            "source character {source_character:?}"
        );
    }
}

#[test]
fn dec_line_drawing_passes_through_outside_the_mapped_range() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
                                                            // 'A' (0x41) and '0' (0x30) are below the 0x5F-0x7E table; unchanged.
    terminal_state.print('A');
    terminal_state.print('0');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('A'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('0'));
}

#[test]
fn line_drawing_glyphs_are_narrow() {
    let mut terminal_state = build_terminal_state(4, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0q"); // '─'
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_display_width(), 1);
}

#[test]
fn so_selects_g1_and_si_selects_g0() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b)0"); // designate G1 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> G1 into GL
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    process_terminal_bytes(&mut terminal_state, b"\x0f"); // SI -> G0 (still ASCII) into GL
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('q'));
}

#[test]
fn charset_designation_persists_across_line_feeds() {
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    terminal_state.print('q'); // row 0
    process_terminal_bytes(&mut terminal_state, b"\r\n"); // CR + LF to the next row
    terminal_state.print('q'); // row 1, charset still in effect
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('─'));
}

#[test]
fn uk_charset_maps_only_the_hash() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(A"); // G0 = UK
    terminal_state.print('#');
    terminal_state.print('a');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('£'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('a'));
}

#[test]
fn unknown_charset_final_falls_back_to_ascii() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b(>"); // unsupported final -> ASCII passthrough
    assert_eq!(terminal_state.active_render().charsets[0], Charset::Ascii);
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q'));
}

#[test]
fn g2_and_g3_are_designated_but_not_selectable() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b*0"); // designate G2 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b+0"); // designate G3 = DEC line drawing
    assert_eq!(
        terminal_state.active_render().charsets[2],
        Charset::DecLineDrawing
    );
    assert_eq!(
        terminal_state.active_render().charsets[3],
        Charset::DecLineDrawing
    );
    // There is no LS2/LS3: GL stays on G0 (ASCII) and printing is unchanged.
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q'));
}

#[test]
fn charset_is_carried_into_the_alternate_screen() {
    // Entering the alternate clones the primary's render terminal_state, charset
    // designations included: a child that did `ESC ( 0` keeps drawing
    // line-drawing glyphs after `?47h`.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // switch to the alternate
    terminal_state.print('q'); // shared G0 still DEC -> box glyph, not literal 'q'
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to the primary
    terminal_state.print('q'); // still DEC
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn decsc_and_decrc_save_and_restore_the_charset() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC: save cursor + charset
    process_terminal_bytes(&mut terminal_state, b"\x1b(B"); // G0 = ASCII
    terminal_state.print('q'); // literal 'q' at (0, 0)
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q'));
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC: restore charset (and home the cursor)
    terminal_state.print('q'); // DEC again -> box glyph at (0, 0)
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn decrc_without_a_save_resets_the_charset_to_ascii() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing, never saved
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC with no prior DECSC -> defaults
    assert_eq!(terminal_state.active_render().charsets[0], Charset::Ascii);
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q'));
}

#[test]
fn dec_1049_entry_inherits_the_primary_charset() {
    // `?1049h` clones the primary's render terminal_state into the alternate, charset
    // designations included: an app that did `ESC ( 0` then entered keeps
    // drawing line-drawing glyphs.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // primary G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alt: inherit the primary's designations
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn dec_1049_entry_does_not_leak_a_prior_alternate_charset() {
    // The clone from the primary overwrites the designations a previous
    // alternate session left.
    let mut terminal_state = build_terminal_state(8, 3);
    // Primary stays ASCII throughout.
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alt (inherits ASCII)
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // this alt session designates G0 = DEC
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // exit
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // re-enter: seed from primary (ASCII) wipes the stale DEC
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q'));
}

#[test]
fn an_alternate_designation_does_not_leak_to_the_primary() {
    // Render terminal_state is per-screen: a designation made on the alternate leaves
    // the primary's untouched after exit.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // enter the alternate (clones primary's ASCII)
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // alt designates G0 = DEC line drawing
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─')); // alt draws box glyphs
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // exit to the primary
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q')); // primary UNAFFECTED — no leak
}

#[test]
fn dec_1049_exit_restores_the_charset_via_decrc() {
    // `?1049 l` restores the cursor as in DECRC, saved charset included: a
    // designation made on the alternate is undone on exit and the primary's
    // set (here ASCII) is in effect.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // save primary (ASCII), enter alt
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // alt designates G0 = DEC
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // DECRC restore -> charset back to the primary's ASCII
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // a non-restoring entry observes the restored ASCII
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q'));
}

#[test]
fn the_alternate_render_is_recloned_from_primary_on_each_entry() {
    // Every alternate entry clones the primary's render terminal_state. A designation
    // the alternate made in a prior session is discarded on re-entry; the
    // alternate resumes its buffer with the primary's render terminal_state.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter (clone primary's ASCII)
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // alt G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // exit (primary unaffected)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // re-enter -> re-clone primary's ASCII
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q')); // alt's prior DEC was discarded
}

#[test]
fn decsc_and_decrc_save_and_restore_the_active_gl_slot() {
    // A save/restore carries which set is invoked into GL, not only the G0-G3
    // table contents.
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b)0"); // designate G1 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC: save cursor, charsets, AND the GL slot
    process_terminal_bytes(&mut terminal_state, b"\x0f"); // SI -> GL = G0 (ASCII)
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('q')); // G0 ASCII -> literal
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC: GL restored to G1 (and cursor home to the saved (0,0))
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─')); // G1 line drawing again
}

#[test]
fn scosc_and_scorc_save_and_restore_the_active_gl_slot() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b)0"); // G1 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x1b[s"); // SCOSC: save (ANSI.SYS form of DECSC)
    process_terminal_bytes(&mut terminal_state, b"\x0f"); // SI -> GL = G0
    process_terminal_bytes(&mut terminal_state, b"\x1b[u"); // SCORC: GL restored to G1
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn decrc_without_a_save_resets_the_gl_slot_to_g0() {
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC with no prior save -> GL back to G0
    assert_eq!(terminal_state.active_render().gl, 0);
}

#[test]
fn the_gl_slot_is_carried_into_the_alternate() {
    // The GL selection is part of the render terminal_state a `?47` entry clones into
    // the alternate: the alternate inherits GL = G1.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> GL = G1 on the primary
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter the alternate (clones the primary's render)
    assert_eq!(terminal_state.active_render().gl, 1); // alternate inherited GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to the primary (its GL is its own)
    assert_eq!(terminal_state.active_render().gl, 1);
}

#[test]
fn an_alternate_pen_change_does_not_leak_to_the_primary() {
    // The pen is per-screen render terminal_state: the alternate inherits the primary's
    // pen on entry, and a color set on the alternate stays there after exit.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[31m"); // primary pen: red fg
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // enter the alternate (inherits red)
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Indexed(1)))
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[32m"); // alt changes pen to green fg
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // exit to the primary
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_foreground_color(Color::Indexed(1))) // primary still red — green did not leak
    );
}

#[test]
fn a_resize_on_the_alternate_keeps_the_primary_background() {
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047h"); // enter the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[44m"); // alternate sets a blue background
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    }); // resize while on the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1047l"); // exit to the primary
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_style(), Style::default()); // primary blanks stayed default, not blue
}

#[test]
fn dec_1049_round_trip_restores_the_gl_slot() {
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b)0"); // primary G1 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alt: saves the primary cursor incl GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x0f"); // change GL = G0 while on the alternate
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l"); // exit: restores the primary cursor, GL back to G1
    terminal_state.print('q'); // primary G1 line drawing still selected
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn mixed_mode_47_then_1049_keeps_the_charset() {
    // `CSI ? 47 ; 1049 h`: each entry clones the primary's designations. The
    // designation survives the mixed-mode entry.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47;1049h"); // ?47h flips active, ?1049h saves/switches/clears cells
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─')); // shared charset intact
}

// --- Cross-subsystem render-terminal_state integration (the render terminal_state vs other features) ---

#[test]
fn decsc_decrc_round_trip_the_whole_render_state_together() {
    // DECSC snapshots the pen, the charset designations, AND the GL slot as one
    // unit; DECRC restores all three together, not just one at a time.
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b)0"); // designate G1 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x0e"); // SO -> GL = G1
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // pen: bold + red fg
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC: snapshot pen + charsets + GL
    process_terminal_bytes(&mut terminal_state, b"\x0f"); // SI -> GL = G0
    process_terminal_bytes(&mut terminal_state, b"\x1b[0m"); // pen reset
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC: restore all three
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_foreground_color(Color::Indexed(1));
        })
    );
    assert_eq!(terminal_state.active_render().gl, 1);
    terminal_state.print('q'); // GL = G1 (DEC) -> box glyph
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn charset_applies_with_autowrap_off() {
    // The charset designation is independent of autowrap: with DECAWM off, the
    // overwrite-in-place glyph at the last column is still charset-mapped.
    let mut terminal_state = build_terminal_state(3, 8); // 3 columns
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l"); // autowrap off
    process_terminal_bytes(&mut terminal_state, b"qqqqq"); // 5 'q's: extras overwrite the last column in place
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('─')); // overwritten in place, still DEC-mapped
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // never wrapped to row 1
}

#[test]
fn erase_preserves_the_active_charset() {
    // EL and ED clear cells but leave the charset designation alone.
    let mut el = build_terminal_state(8, 2);
    process_terminal_bytes(&mut el, b"\x1b(0\x1b[2K"); // G0 = DEC, then EL 2 (erase line)
    el.print('q');
    assert_eq!(get_terminal_glyph(&el, 0, 0), Some('─'));

    let mut ed = build_terminal_state(8, 2);
    process_terminal_bytes(&mut ed, b"\x1b(0\x1b[2J"); // G0 = DEC, then ED 2 (erase display)
    ed.print('q');
    assert_eq!(get_terminal_glyph(&ed, 0, 0), Some('─'));
}

#[test]
fn resize_preserves_per_screen_charsets() {
    // Render terminal_state is per-screen and untouched by resize: the primary keeps its
    // designation and the alternate keeps its own, independently.
    let mut terminal_state = build_terminal_state(8, 4);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // primary G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter alt (clones primary's DEC)
    process_terminal_bytes(&mut terminal_state, b"\x1b(A"); // alt G0 = UK ('#' -> '£')
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    terminal_state.print('#'); // alt UK survived the resize
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('£'));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l"); // back to the primary
    terminal_state.print('q'); // primary DEC survived the resize, independent of the alt
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn setting_mouse_modes_does_not_touch_the_render_state() {
    // Enabling mouse tracking/encoding, alt-scroll, and bracketed paste changes
    // neither the pen nor the charset.
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;31m"); // pen: bold + red
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1000;1006;1007h"); // mouse tracking + SGR encoding + alt-scroll
    process_terminal_bytes(&mut terminal_state, b"\x1b[?2004h"); // bracketed paste
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| {
            style.set_bold(true);
            style.set_foreground_color(Color::Indexed(1));
        })
    );
    terminal_state.print('q'); // charset still DEC
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn charset_survives_a_region_scroll() {
    // Scrolling a region leaves the charset in place: glyphs printed after the
    // scroll are still mapped.
    let mut terminal_state = build_terminal_state(8, 4);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3r"); // DECSTBM: region rows 1-3 (1-based)
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"\x1b[2S"); // SU: scroll the region up 2 lines
    terminal_state.print('q'); // charset unaffected by the scroll
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn decsc_decrc_round_trips_the_charset_on_the_alternate_screen() {
    // The per-screen saved slot carries charsets on the alternate too, not only
    // the primary.
    let mut terminal_state = build_terminal_state(8, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // enter alt
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // alt G0 = DEC
    process_terminal_bytes(&mut terminal_state, b"\x1b7"); // DECSC on the alt: saves G0 = DEC
    process_terminal_bytes(&mut terminal_state, b"\x1b(B"); // alt G0 = ASCII
    process_terminal_bytes(&mut terminal_state, b"\x1b8"); // DECRC on the alt: restores G0 = DEC
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn dec_1048_saves_and_restores_the_charset() {
    // `?1048` saves/restores the cursor as DECSC/DECRC do, charsets included.
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // G0 = DEC
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1048h"); // save cursor (incl charsets)
    process_terminal_bytes(&mut terminal_state, b"\x1b(B"); // G0 = ASCII
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1048l"); // restore -> G0 = DEC
    terminal_state.print('q');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
}

#[test]
fn dec_line_drawing_survives_a_deferred_wrap() {
    // The charset remap runs before the wrap/width logic: a line-drawing row
    // wraps exactly like an ASCII one (every glyph is narrow).
    let mut terminal_state = build_terminal_state(3, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // GL = DEC line drawing
    process_terminal_bytes(&mut terminal_state, b"qqq"); // fills row 0 with ───, parks at the last column
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('─'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('─'));
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('q'); // forces the deferred wrap onto row 1
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('─'));
    assert_eq!(terminal_state.active_cursor().row, 1);
}

#[test]
fn dec_line_drawing_passes_multibyte_utf8_through() {
    // The table only remaps the ASCII range 0x5F-0x7E; a real Unicode glyph
    // (vte has already decoded the UTF-8) is printed unchanged.
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // GL = DEC line drawing
    process_terminal_bytes(&mut terminal_state, "é".as_bytes()); // multibyte, outside the table
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('é'));
}

#[test]
fn a_combining_mark_folds_onto_a_line_drawing_get_terminal_glyph() {
    // The remapped glyph anchors the grapheme cluster. A following combining
    // mark folds onto it as a single-cell cluster.
    let mut terminal_state = build_terminal_state(8, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b(0"); // GL = DEC line drawing
    terminal_state.print('q'); // '─' base at (0, 0)
    terminal_state.print('\u{0301}'); // combining acute: folds onto the base, no new cell
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), '─');
    assert_eq!(cell.list_combining_characters(), &['\u{0301}']);
    assert_eq!(terminal_state.active_cursor().column, 1); // cursor did not advance a 2nd column
}

// --- Autowrap (DECAWM ?7) ---

#[test]
fn autowrap_is_on_by_default() {
    let terminal_state = build_terminal_state(5, 3);
    assert!(terminal_state.is_autowrap_enabled());
}

#[test]
fn autowrap_on_wraps_at_the_last_column() {
    // Default mode: a glyph past the last column moves onto the next row.
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "abcdef"); // 6 chars into a 5-wide row
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('e'));
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('f')); // wrapped onto row 1
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (1, 1)
    );
}

#[test]
fn autowrap_off_overwrites_the_last_column_in_place() {
    // With ?7l the cursor parks at the last column and further glyphs overwrite
    // it; nothing wraps onto the next row.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    print_text(&mut terminal_state, "abcdef"); // 'e' fills column 4, 'f' overwrites it
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 3), Some('d'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('f')); // overwrote 'e' in place
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // nothing wrapped down
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (0, 4)
    );
}

#[test]
fn autowrap_round_trips() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    assert!(!terminal_state.is_autowrap_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7h");
    assert!(terminal_state.is_autowrap_enabled());
}

#[test]
fn disabling_autowrap_after_parking_does_not_wrap() {
    // The wrap decision is made when the parked latch is consumed, against the
    // mode in effect THEN — not when the glyph first parked. Filling the row with
    // autowrap on parks the latch; turning autowrap off before the next glyph
    // makes it overwrite in place instead of wrapping.
    let mut terminal_state = build_terminal_state(5, 3);
    print_text(&mut terminal_state, "abcde"); // parks at column 4 with the wrap latch set
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l"); // disable autowrap while parked
    terminal_state.print('f');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('f')); // overwrote 'e', did not wrap
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' '));
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (0, 4)
    );
}

#[test]
fn a_last_column_write_under_autowrap_off_arms_no_wrap_when_re_enabled() {
    // The deferred-wrap latch is armed only when a glyph lands on the last column
    // while autowrap is on. Writing the last column under ?7l arms nothing.
    // After ?7h the next glyph overwrites the last column; that write, under
    // autowrap, arms a wrap for the glyph after it.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    print_text(&mut terminal_state, "abcde"); // 'e' lands on column 4 under autowrap-off → no latch
    assert!(!terminal_state.active_cursor().pending_wrap);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7h"); // re-enable autowrap
    terminal_state.print('f');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 4), Some('f')); // overwrote 'e', did NOT wrap
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' '));
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (0, 4)
    );
    terminal_state.print('g'); // the latch is armed → this glyph wraps
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('g'));
    assert_eq!(terminal_state.active_cursor().row, 1);
}

#[test]
fn a_wide_glyph_at_the_last_column_is_dropped_when_autowrap_off() {
    // A 2-cell glyph cannot fit in the lone last column; with no wrap to move it
    // onto, it is dropped and the cursor stays pinned.
    let mut terminal_state = build_terminal_state(4, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    print_text(&mut terminal_state, "abc"); // cursor at column 3 (the last column)
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (0, 3)
    );
    terminal_state.print('中'); // wide; does not fit, autowrap off → dropped
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 3), Some(' ')); // last column untouched
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // nothing wrapped down
    assert_eq!(
        (
            terminal_state.active_cursor().row,
            terminal_state.active_cursor().column
        ),
        (0, 3)
    );
}

#[test]
fn a_wide_glyph_at_the_last_column_wraps_when_autowrap_on() {
    // Control for the ?7l case: with autowrap on the wide glyph wraps whole onto
    // the next row, blanking the freed last column.
    let mut terminal_state = build_terminal_state(4, 2);
    print_text(&mut terminal_state, "abc"); // cursor at column 3 (the last column)
    terminal_state.print('中');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 3), Some(' ')); // freed column blanked
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some('中')); // wrapped whole onto row 1
    assert_eq!(terminal_state.active_cursor().row, 1);
}

#[test]
fn enabling_autowrap_after_a_dropped_wide_glyph_overwrites_not_wraps() {
    // A wide glyph dropped at the last column under ?7l arms no wrap. After
    // ?7h the next glyph overwrites at the last column; that write arms the
    // wrap for the following glyph.
    let mut terminal_state = build_terminal_state(4, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    print_text(&mut terminal_state, "abc"); // cursor at column 3 (the last column)
    terminal_state.print('中'); // wide; does not fit, autowrap off → dropped, no wrap armed
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7h"); // re-enable autowrap
    terminal_state.print('x');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 3), Some('x')); // overwrote at the last column
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // did NOT wrap onto row 1
    assert_eq!(terminal_state.active_cursor().row, 0);
}

#[test]
fn a_dropped_wide_glyph_does_not_capture_a_following_combining_mark() {
    // Under ?7l a wide glyph dropped at the last column is its own new grapheme;
    // a following combining mark belongs to it (and is itself dropped), and must
    // NOT fold onto the previous cell.
    let mut terminal_state = build_terminal_state(4, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    print_text(&mut terminal_state, "abc"); // 'c' at column 2, cursor at column 3 (last column)
    terminal_state.print('中'); // wide; dropped at the last column
    terminal_state.print('\u{0301}'); // combining acute — belongs to the dropped glyph
    let c_cell = terminal_state
        .get_active_grid()
        .get_cell(0, 2)
        .expect("in bounds");
    assert_eq!(c_cell.get_character(), 'c');
    assert!(c_cell.list_combining_characters().is_empty()); // the acute did NOT attach to 'c'
}

#[test]
fn a_vs16_promotion_at_the_last_column_does_not_wrap_when_autowrap_off() {
    // A narrow text-presentation glyph in the last column has no room to widen
    // when VS16 asks for the wide emoji form. Under ?7l it stays in place;
    // nothing moves onto the next row.
    let mut terminal_state = build_terminal_state(3, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l");
    print_text(&mut terminal_state, "ab"); // cursor at column 2 (last column)
    terminal_state.print('\u{2764}'); // heart, text presentation, width 1, rests at column 2
    assert!(!terminal_state.active_cursor().pending_wrap); // autowrap off arms no wrap latch
    terminal_state.print('\u{FE0F}'); // VS16 wants the wide emoji form
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 2), Some('\u{2764}')); // stayed in place
    assert_eq!(get_terminal_glyph(&terminal_state, 1, 0), Some(' ')); // did NOT wrap onto row 1
    assert_eq!(terminal_state.active_cursor().row, 0);
}

// --- Application cursor keys / reverse video / cursor blink + unsupported modes ---

#[test]
fn the_new_dec_modes_start_at_their_defaults() {
    let terminal_state = build_terminal_state(5, 3);
    assert!(terminal_state.is_autowrap_enabled()); // ?7 defaults on
    assert!(!terminal_state.are_application_cursor_keys_enabled()); // ?1
    assert!(!terminal_state.is_reverse_video_enabled()); // ?5
    assert!(!terminal_state.is_cursor_blink_enabled()); // ?12
}

#[test]
fn application_cursor_keys_toggles() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1h");
    assert!(terminal_state.are_application_cursor_keys_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1l");
    assert!(!terminal_state.are_application_cursor_keys_enabled());
}

#[test]
fn reverse_video_toggles() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?5h");
    assert!(terminal_state.is_reverse_video_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?5l");
    assert!(!terminal_state.is_reverse_video_enabled());
}

#[test]
fn cursor_blink_toggles() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12h");
    assert!(terminal_state.is_cursor_blink_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12l");
    assert!(!terminal_state.is_cursor_blink_enabled());
}

// --- DECSCUSR cursor style ---

#[test]
fn a_fresh_pane_has_asked_for_no_cursor_shape() {
    // A pane that never sends DECSCUSR asks for no shape: the user's own
    // configured cursor stands.
    let terminal_state = build_terminal_state(5, 3);
    assert_eq!(terminal_state.get_cursor_shape(), None);
    assert!(!terminal_state.is_cursor_blink_enabled());
}

#[test]
fn decscusr_sets_every_style_it_names() {
    // The six styles of `CSI Ps SP q`: the odd values blink, the even ones are
    // steady. This is the table vim drives its modes with.
    let cursor_style_cases = [
        (&b"\x1b[1 q"[..], CursorShape::Block, true),
        (&b"\x1b[2 q"[..], CursorShape::Block, false),
        (&b"\x1b[3 q"[..], CursorShape::Underline, true),
        (&b"\x1b[4 q"[..], CursorShape::Underline, false),
        (&b"\x1b[5 q"[..], CursorShape::Bar, true),
        (&b"\x1b[6 q"[..], CursorShape::Bar, false),
    ];
    for (style_input_bytes, cursor_shape, is_cursor_blink_enabled) in cursor_style_cases {
        let mut terminal_state = build_terminal_state(5, 3);
        process_terminal_bytes(&mut terminal_state, style_input_bytes);
        assert_eq!(
            terminal_state.get_cursor_shape(),
            Some(cursor_shape),
            "{style_input_bytes:?}"
        );
        assert_eq!(
            terminal_state.is_cursor_blink_enabled(),
            is_cursor_blink_enabled,
            "{style_input_bytes:?}"
        );
    }
}

#[test]
fn decscusr_zero_gives_the_cursor_back_to_the_user() {
    // `CSI 0 SP q`, and the same sequence with the parameter omitted, return the
    // pane to asking for no shape and no blink: the user's own configured cursor
    // stands again.
    for cursor_style_input_bytes in [&b"\x1b[0 q"[..], &b"\x1b[ q"[..]] {
        let mut terminal_state = build_terminal_state(5, 3);
        process_terminal_bytes(&mut terminal_state, b"\x1b[5 q"); // vim's insert-mode blinking bar
        assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Bar));

        process_terminal_bytes(&mut terminal_state, cursor_style_input_bytes);
        assert_eq!(
            terminal_state.get_cursor_shape(),
            None,
            "{cursor_style_input_bytes:?}"
        );
        assert!(
            !terminal_state.is_cursor_blink_enabled(),
            "{cursor_style_input_bytes:?}"
        );
    }
}

#[test]
fn an_unknown_decscusr_value_changes_nothing() {
    // `CSI 9 SP q` names no style and leaves the one already set alone.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[5 q"); // blinking bar
    process_terminal_bytes(&mut terminal_state, b"\x1b[9 q");
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Bar));
    assert!(terminal_state.is_cursor_blink_enabled());
}

#[test]
fn a_steady_style_stops_a_blink_that_mode_12_started() {
    // `?12 h` and DECSCUSR write the same cursor blink setting. `?12 h` starts a blink;
    // a following "steady block" stops it.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12h");
    assert!(terminal_state.is_cursor_blink_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[2 q");
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Block));
    assert!(!terminal_state.is_cursor_blink_enabled());
}

#[test]
fn mode_12_blinks_the_shape_decscusr_chose() {
    // `?12` sets whether the cursor blinks and leaves its shape alone: the bar
    // stays a bar.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[6 q"); // steady bar
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12h");
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Bar));
    assert!(terminal_state.is_cursor_blink_enabled());
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12l");
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Bar));
    assert!(!terminal_state.is_cursor_blink_enabled());
}

#[test]
fn a_decrqm_query_for_mode_12_answers_what_decscusr_left_behind() {
    // DECRQM `?12` reads the cursor blink setting DECSCUSR wrote: after a blinking bar
    // the reply says set (1); after a steady block it says reset (2).
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[5 q"); // blinking bar
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12$p");
    assert_eq!(
        terminal_state.take_device_query_replies(),
        b"\x1b[?12;1$y".to_vec()
    );

    process_terminal_bytes(&mut terminal_state, b"\x1b[2 q"); // steady block
    process_terminal_bytes(&mut terminal_state, b"\x1b[?12$p");
    assert_eq!(
        terminal_state.take_device_query_replies(),
        b"\x1b[?12;2$y".to_vec()
    );
}

#[test]
fn a_cursor_style_survives_the_alternate_screen() {
    // The style is one global setting, not per-screen: vim sets its bar on the
    // alternate screen and the shape does not reset under it.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h");
    process_terminal_bytes(&mut terminal_state, b"\x1b[5 q");
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Bar));
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049l");
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Bar));
    assert!(terminal_state.is_cursor_blink_enabled());
}

#[test]
fn unsupported_dec_modes_are_ignored() {
    // ?2 (VT52), ?3 (132-column), ?8 (auto-repeat) are ignored: no panic, other
    // mode terminal_state untouched, printing still works afterward.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?2h");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?3h");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?8h");
    process_terminal_bytes(&mut terminal_state, b"\x1b[?8l");
    assert!(terminal_state.is_autowrap_enabled()); // untouched
    assert!(!terminal_state.are_application_cursor_keys_enabled());
    terminal_state.print('x');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('x'));
}

// --- Adversarial: malformed / hostile escape sequences ---

#[test]
fn csi_param_saturates_at_u16_max_and_clamps_to_the_grid() {
    // vte collects a parameter with saturating arithmetic: a value past u16::MAX
    // becomes 65535. koshi's saturating cursor math then clamps it to the grid.
    let mut terminal_state = build_terminal_state(10, 5); // last row 4, last column 9
    process_terminal_bytes(&mut terminal_state, b"\x1b[5;5H"); // (4, 4)
    process_terminal_bytes(&mut terminal_state, b"\x1b[99999999A"); // CUU by a saturated 65535 -> row 0
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 4));
    process_terminal_bytes(&mut terminal_state, b"\x1b[99999999C"); // CUF by 65535 -> last column
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 9));
}

#[test]
fn a_csi_with_too_many_parameters_is_dropped() {
    // vte holds at most 32 parameters; a 33rd flags the whole sequence
    // `ignore`, and koshi drops an ignored CSI. The cursor move never happens.
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // (2, 2)
    let mut csi_sequence_bytes = Vec::from(&b"\x1b["[..]);
    csi_sequence_bytes.extend(std::iter::repeat_n(&b"1;"[..], 40).flatten().copied());
    csi_sequence_bytes.push(b'H'); // 40 params + a final H — far past the 32 cap
    process_terminal_bytes(&mut terminal_state, &csi_sequence_bytes);
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 2)); // unmoved
}

#[test]
fn a_csi_at_the_max_parameter_count_still_dispatches() {
    // Exactly 32 parameters fit: the sequence is not flagged ignore and its
    // SGR applies. Thirty-two `1`s all mean bold.
    let mut terminal_state = build_terminal_state(5, 2);
    let mut csi_sequence_bytes = Vec::from(&b"\x1b["[..]);
    csi_sequence_bytes.extend(std::iter::repeat_n(&b"1;"[..], 31).flatten().copied());
    csi_sequence_bytes.push(b'1'); // 32nd parameter
    csi_sequence_bytes.push(b'm');
    process_terminal_bytes(&mut terminal_state, &csi_sequence_bytes);
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_bold(true))
    );
}

#[test]
fn a_carriage_return_inside_a_csi_executes_then_the_move_completes() {
    // A C0 control byte mid-CSI is executed in place; parameter collection then
    // resumes and the final byte dispatches. The CR homes the column, then CUD
    // moves down.
    let mut terminal_state = build_terminal_state(10, 3); // last row 2
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;5H"); // (2, 4)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2\x0dB"); // CR (column -> 0), then CUD 2 (clamped to row 2)
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 0));
}

#[test]
fn an_escape_inside_a_csi_cancels_the_pending_sequence() {
    // An ESC mid-CSI abandons the half-built sequence and starts a fresh escape.
    // The trailing `B` is a plain ESC final koshi ignores: the CUD never runs
    // and the cursor stays put.
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[5\x1bB"); // CSI 5 abandoned by ESC; ESC B ignored
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 2));
    terminal_state.print('z'); // the parser recovered: a glyph lands normally
    assert_eq!(get_terminal_glyph(&terminal_state, 2, 2), Some('z'));
}

#[test]
fn leading_zero_params_are_read_as_decimal() {
    // `007` is decimal seven, not octal — CUP maps it 1-based to row 6.
    let mut terminal_state = build_terminal_state(10, 10);
    process_terminal_bytes(&mut terminal_state, b"\x1b[007;003H");
    assert_eq!(terminal_state.get_active_cursor_position(), (6, 2));
}

#[test]
fn an_empty_leading_csi_parameter_uses_the_default() {
    // `CSI ; 5 H` — the missing row argument defaults to 1 (top), the column to 5.
    let mut terminal_state = build_terminal_state(10, 10);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // move away first
    process_terminal_bytes(&mut terminal_state, b"\x1b[;5H");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 4));
}

#[test]
fn an_unknown_csi_final_byte_leaves_the_cursor_unmoved() {
    // `W` (CTC) is not handled; it must be a silent no-op, not a stray move.
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[2W");
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 2));
}

#[test]
fn a_parameterized_decstr_is_ignored() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[1m");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1!p\x1b[0;0!p");
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 2));
    assert_eq!(
        terminal_state.active_render().style,
        build_style_with_mutator(|style| style.set_bold(true))
    );
}

#[test]
fn an_explicit_zero_decstr_soft_resets() {
    let mut terminal_state = build_terminal_state(10, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H\x1b[1m\x1b[0!p");
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 2));
    assert_eq!(terminal_state.active_render().style, Style::default());
}

#[test]
fn stored_tab_stops_drive_ht_cht_and_cbt() {
    let mut terminal_state = build_terminal_state(20, 2);
    assert!(terminal_state.tab_stops[0]);
    assert!(terminal_state.tab_stops[8]);
    assert!(terminal_state.tab_stops[16]);

    process_terminal_bytes(&mut terminal_state, b"\x1b[6G\x1bH\r\t");
    assert_eq!(terminal_state.active_cursor().column, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[I");
    assert_eq!(terminal_state.active_cursor().column, 8);
    process_terminal_bytes(&mut terminal_state, b"\x1b[Z");
    assert_eq!(terminal_state.active_cursor().column, 5);
}

#[test]
fn tbc_clears_current_or_all_and_reads_only_its_first_parameter() {
    let mut terminal_state = build_terminal_state(20, 2);

    process_terminal_bytes(&mut terminal_state, b"\x1b[6G\x1bH\x1b[g\r\t");
    assert_eq!(terminal_state.active_cursor().column, 8);

    process_terminal_bytes(&mut terminal_state, b"\x1b[0;3g\r\t");
    assert_eq!(terminal_state.active_cursor().column, 16);

    process_terminal_bytes(&mut terminal_state, b"\x1b[6G\x1bH\x1b[2g\r\t");
    assert_eq!(terminal_state.active_cursor().column, 5);

    process_terminal_bytes(&mut terminal_state, b"\x1b[3g\r\t");
    assert_eq!(terminal_state.active_cursor().column, 19);
    process_terminal_bytes(&mut terminal_state, b"\r\x1b[I");
    assert_eq!(terminal_state.active_cursor().column, 19);
    process_terminal_bytes(&mut terminal_state, b"\x1b[Z");
    assert_eq!(terminal_state.active_cursor().column, 0);
}

#[test]
fn tab_setup_and_clear_preserve_wrap_but_tab_motion_clears_it() {
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "abcde");
    assert!(terminal_state.active_cursor().pending_wrap);

    process_terminal_bytes(&mut terminal_state, b"\x1bH");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 4));
    assert!(terminal_state.active_cursor().pending_wrap);

    process_terminal_bytes(&mut terminal_state, b"\x1b[g");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 4));
    assert!(terminal_state.active_cursor().pending_wrap);

    process_terminal_bytes(&mut terminal_state, b"\x1b[I");
    assert!(!terminal_state.active_cursor().pending_wrap);
}

#[test]
fn resizing_tab_stops_preserves_survivors_and_defaults_new_columns() {
    let mut terminal_state = build_terminal_state(26, 2);
    process_terminal_bytes(&mut terminal_state, b"\x1b[9G\x1b[g");
    assert!(!terminal_state.tab_stops[8]);

    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 2,
    });
    terminal_state.resize_terminal_state(PtySize {
        column_count: 18,
        row_count: 2,
    });
    assert!(!terminal_state.tab_stops[8]);
    assert!(terminal_state.tab_stops[16]);

    process_terminal_bytes(&mut terminal_state, b"\x1b[6G\x1bH");
    terminal_state.resize_terminal_state(PtySize {
        column_count: 4,
        row_count: 2,
    });
    terminal_state.resize_terminal_state(PtySize {
        column_count: 10,
        row_count: 2,
    });
    assert!(!terminal_state.tab_stops[5]);
    assert!(terminal_state.tab_stops[8]);
}

#[test]
fn seven_and_eight_bit_index_controls_match() {
    let mut ind_esc = build_terminal_state(5, 4);
    process_terminal_bytes(&mut ind_esc, b"\x1b[2;3H");
    let mut ind_c1 = ind_esc.clone();
    process_terminal_bytes(&mut ind_esc, b"\x1bD");
    process_terminal_bytes(&mut ind_c1, &[0x84]);
    assert_eq!(ind_c1, ind_esc);

    let mut nel_esc = build_terminal_state(5, 4);
    process_terminal_bytes(&mut nel_esc, b"\x1b[2;3H");
    let mut nel_c1 = nel_esc.clone();
    process_terminal_bytes(&mut nel_esc, b"\x1bE");
    process_terminal_bytes(&mut nel_c1, &[0x85]);
    assert_eq!(nel_c1, nel_esc);
    assert_eq!(nel_esc.get_active_cursor_position(), (2, 0));

    let mut hts_esc = build_terminal_state(12, 2);
    process_terminal_bytes(&mut hts_esc, b"\x1b[6G");
    let mut hts_c1 = hts_esc.clone();
    process_terminal_bytes(&mut hts_esc, b"\x1bH");
    process_terminal_bytes(&mut hts_c1, &[0x88]);
    assert_eq!(hts_c1, hts_esc);

    let mut ri_esc = build_terminal_state(5, 4);
    process_terminal_bytes(&mut ri_esc, b"\x1b[3;2H");
    let mut ri_c1 = ri_esc.clone();
    process_terminal_bytes(&mut ri_esc, b"\x1bM");
    process_terminal_bytes(&mut ri_c1, &[0x8D]);
    assert_eq!(ri_c1, ri_esc);
}

#[test]
fn ind_and_nel_scroll_the_active_region_at_its_bottom() {
    let mut ind = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut ind);
    process_terminal_bytes(&mut ind, b"\x1b[2;3r\x1b[3;2H\x1bD");
    assert_eq!(get_row_text(&ind, 0), "abc");
    assert_eq!(get_row_text(&ind, 1), "ghi");
    assert_eq!(get_row_text(&ind, 2), "   ");
    assert_eq!(ind.get_active_cursor_position(), (2, 1));

    let mut nel = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut nel);
    process_terminal_bytes(&mut nel, b"\x1b[2;3r\x1b[3;2H\x1bE");
    assert_eq!(get_row_text(&nel, 0), "abc");
    assert_eq!(get_row_text(&nel, 1), "ghi");
    assert_eq!(get_row_text(&nel, 2), "   ");
    assert_eq!(nel.get_active_cursor_position(), (2, 0));
}

#[test]
fn decstr_resets_only_its_active_dec_state() {
    let mut terminal_state = build_terminal_state(10, 4);
    print_text(&mut terminal_state, "hello");
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b]2;kept title\x07\x1b]7;file://localhost/tmp\x07\x1b[c",
    );
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[6G\x1bH\x1b[1m\x1b[2;4r\x1b[3;4H\x1b7\x1b[?25l",
    );
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[?1;5;7;12;1000;1006;1007;2004h\x1b[5 q",
    );
    terminal_state.active_cursor_mut().pending_wrap = true;
    let history_row = terminal_state.get_active_grid().list_rows()[0].clone();
    terminal_state
        .scrollback
        .push_row(&history_row, RowMetadata::default());

    let grid = terminal_state.get_active_grid().clone();
    let tab_stops = terminal_state.tab_stops.clone();
    let terminal_title = terminal_state.title.clone();
    let reported_working_directory = terminal_state.reported_working_directory.clone();
    let scrollback_snapshot = terminal_state.scrollback.clone();
    let device_query_replies = terminal_state.device_query_replies.clone();
    let mouse_tracking = terminal_state.modes.mouse_tracking;
    let mouse_encoding = terminal_state.modes.mouse_encoding;
    let alternate_scroll = terminal_state.modes.alternate_scroll;
    let reverse_video = terminal_state.modes.reverse_video;
    let cursor_blink = terminal_state.modes.cursor_blink;
    let cursor_shape = terminal_state.modes.cursor_shape;

    process_terminal_bytes(&mut terminal_state, b"\x1b[!p");

    assert_eq!(terminal_state.get_active_grid(), &grid);
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 3));
    assert!(terminal_state.active_cursor().is_visible);
    assert!(!terminal_state.active_cursor().pending_wrap);
    assert_eq!(terminal_state.active_cursor().saved, None);
    assert_eq!(*terminal_state.active_render(), RenderState::fresh());
    assert_eq!(terminal_state.get_scroll_region(), None);
    assert!(!terminal_state.modes.application_cursor_keys);
    assert!(!terminal_state.modes.autowrap);
    assert_eq!(terminal_state.modes.mouse_tracking, mouse_tracking);
    assert_eq!(terminal_state.modes.mouse_encoding, mouse_encoding);
    assert_eq!(terminal_state.modes.alternate_scroll, alternate_scroll);
    assert_eq!(terminal_state.modes.reverse_video, reverse_video);
    assert_eq!(terminal_state.modes.cursor_blink, cursor_blink);
    assert_eq!(terminal_state.modes.cursor_shape, cursor_shape);
    assert!(terminal_state.modes.bracketed_paste);
    assert_eq!(terminal_state.tab_stops, tab_stops);
    assert_eq!(terminal_state.title, terminal_title);
    assert_eq!(
        terminal_state.reported_working_directory,
        reported_working_directory
    );
    assert_eq!(terminal_state.scrollback, scrollback_snapshot);
    assert_eq!(terminal_state.device_query_replies, device_query_replies);
    assert!(terminal_state.cluster.is_empty());
    assert_eq!(terminal_state.cluster_base, None);
}

#[test]
fn decstr_resets_only_the_active_screen() {
    let mut terminal_state = build_terminal_state(8, 4);
    print_text(&mut terminal_state, "primary");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1m\x1b[2;4r\x1b[3;4H");
    let primary_grid = terminal_state.primary.clone();
    let primary_cursor = terminal_state.primary_cursor;
    let primary_render = terminal_state.primary_render;
    let primary_region = terminal_state.primary_scroll_region;

    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[?47h\x1b[31m\x1b[2;3r\x1b[2;8H\x1b[?25l\x1b7",
    );
    terminal_state.print('a');
    process_terminal_bytes(&mut terminal_state, b"\x1b[!p");
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    assert_eq!(
        terminal_state
            .alternate
            .get_cell(1, 7)
            .map(Cell::get_character),
        Some('a')
    );
    assert_eq!(terminal_state.get_active_cursor_position(), (1, 7));
    assert!(terminal_state.alternate_cursor.is_visible);
    assert!(!terminal_state.alternate_cursor.pending_wrap);
    assert_eq!(terminal_state.alternate_cursor.saved, None);
    assert_eq!(terminal_state.alternate_render, RenderState::fresh());
    assert_eq!(terminal_state.alternate_scroll_region, None);
    assert_eq!(terminal_state.primary, primary_grid);
    assert_eq!(terminal_state.primary_cursor, primary_cursor);
    assert_eq!(terminal_state.primary_render, primary_render);
    assert_eq!(terminal_state.primary_scroll_region, primary_region);

    terminal_state.alternate_render.style = build_style_with_mutator(|style| style.set_bold(true));
    terminal_state.alternate_cursor.row = 2;
    terminal_state.alternate_scroll_region = Some((1, 2));
    let alternate_grid = terminal_state.alternate.clone();
    let alternate_cursor = terminal_state.alternate_cursor;
    let alternate_render = terminal_state.alternate_render;
    let alternate_region = terminal_state.alternate_scroll_region;
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47l\x1b[!p");
    assert_eq!(terminal_state.alternate, alternate_grid);
    assert_eq!(terminal_state.alternate_cursor, alternate_cursor);
    assert_eq!(terminal_state.alternate_render, alternate_render);
    assert_eq!(terminal_state.alternate_scroll_region, alternate_region);
}

#[test]
fn decstr_breaks_the_current_grapheme_cluster() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('e');
    assert!(!terminal_state.cluster.is_empty());
    process_terminal_bytes(&mut terminal_state, b"\x1b[!p");
    terminal_state.print('\u{0301}');
    assert_eq!(
        terminal_state
            .get_active_grid()
            .get_cell(0, 0)
            .expect("the base cell")
            .list_combining_characters(),
        []
    );
}

#[test]
fn ris_restores_display_state_but_keeps_session_metadata() {
    let pty_size = PtySize {
        column_count: 20,
        row_count: 3,
    };
    let mut terminal_state = TerminalState::with_scrollback(
        pty_size,
        ScrollbackLimit::from_line_and_byte_limits(1, 1024),
    );
    print_text(&mut terminal_state, "primary");
    let history_row = terminal_state.get_active_grid().list_rows()[0].clone();
    terminal_state
        .scrollback
        .push_row(&history_row, RowMetadata::default());
    terminal_state
        .scrollback
        .push_row(&history_row, RowMetadata::default());
    terminal_state
        .scrollback
        .push_row(&history_row, RowMetadata::default());
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b]2;reset me\x07\x1b]7;file://localhost/work\x07\x1b[c",
    );
    process_terminal_bytes(
        &mut terminal_state,
        b"\x1b[6G\x1bH\x1b[1m\x1b[2;3r\x1b[3;5H\x1b[?1;5;12;2004h",
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h");
    print_text(&mut terminal_state, "alternate");
    process_terminal_bytes(&mut terminal_state, b"\x1b[31m\x1b[2;3r\x1b[3;4H\x1b[?25l");

    let reported_working_directory = terminal_state.reported_working_directory.clone();
    let device_query_replies = terminal_state.device_query_replies.clone();
    let total_pushed = terminal_state.scrollback.get_total_pushed_line_count();
    let dropped_lines = terminal_state.scrollback.get_dropped_line_count();
    let dropped_bytes = terminal_state.scrollback.get_dropped_byte_count();

    process_terminal_bytes(&mut terminal_state, b"\x1bc");

    assert_eq!(terminal_state.active_screen, Screen::Primary);
    assert_eq!(terminal_state.primary.get_grid_dimensions(), (3, 20));
    assert_eq!(terminal_state.alternate.get_grid_dimensions(), (3, 20));
    for screen_grid in [
        terminal_state.primary.as_ref(),
        terminal_state.alternate.as_ref(),
    ] {
        for row_cells in screen_grid.list_rows() {
            for cell in row_cells {
                assert_eq!(cell.get_character(), ' ');
                assert_eq!(cell.get_display_width(), 1);
                assert_eq!(cell.get_style(), Style::default());
            }
        }
    }
    for screen_cursor in [
        terminal_state.primary_cursor,
        terminal_state.alternate_cursor,
    ] {
        assert_eq!((screen_cursor.row, screen_cursor.column), (0, 0));
        assert!(screen_cursor.is_visible);
        assert!(!screen_cursor.pending_wrap);
        assert_eq!(screen_cursor.saved, None);
    }
    assert_eq!(terminal_state.primary_render, RenderState::fresh());
    assert_eq!(terminal_state.alternate_render, RenderState::fresh());
    assert_eq!(terminal_state.modes, TerminalModes::default());
    assert_eq!(terminal_state.primary_scroll_region, None);
    assert_eq!(terminal_state.alternate_scroll_region, None);
    assert_eq!(
        terminal_state.tab_stops,
        (0..20).map(|column| column % 8 == 0).collect::<Vec<_>>()
    );
    assert_eq!(terminal_state.title, None);
    assert!(terminal_state.cluster.is_empty());
    assert_eq!(terminal_state.cluster_base, None);
    assert!(terminal_state.scrollback.is_empty());
    assert_eq!(
        terminal_state.scrollback.get_total_pushed_line_count(),
        total_pushed
    );
    assert_eq!(
        terminal_state.scrollback.get_dropped_line_count(),
        dropped_lines
    );
    assert_eq!(
        terminal_state.scrollback.get_dropped_byte_count(),
        dropped_bytes
    );
    assert_eq!(
        terminal_state.reported_working_directory,
        reported_working_directory
    );
    assert_eq!(terminal_state.device_query_replies, device_query_replies);

    let blank_row = terminal_state.primary.list_rows()[0].clone();
    terminal_state
        .scrollback
        .push_row(&blank_row, RowMetadata::default());
    terminal_state
        .scrollback
        .push_row(&blank_row, RowMetadata::default());
    assert_eq!(terminal_state.scrollback.get_retained_line_count(), 1);
}

#[test]
fn an_ignored_private_marker_csi_does_not_break_a_cluster() {
    // A private marker after a parameter (`CSI 1 < m`) routes vte straight to
    // ground with no Perform callback: it neither moves the cursor nor resets
    // the cluster. A combining mark printed afterward folds onto the preceding
    // glyph.
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('e');
    process_terminal_bytes(&mut terminal_state, b"\x1b[1<m"); // swallowed with no dispatch
    terminal_state.print('\u{0301}'); // combining acute accent
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.list_combining_characters(), ['\u{0301}']);
}

#[test]
fn unhandled_c1_control_bytes_are_inert() {
    // Unhandled C1 controls do not print, move the cursor, or panic.
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('a'); // (0, 1)
    process_terminal_bytes(&mut terminal_state, b"\x9b\x9c\x80"); // C1 CSI, ST, PAD — all ignored
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('a'));
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 1), Some(' '));
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 1));
}

#[test]
fn a_lone_invalid_high_byte_prints_the_replacement_char() {
    // An invalid UTF-8 lead byte above the C1 range decodes to U+FFFD, which
    // lands as an ordinary narrow glyph.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\xff");
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('\u{FFFD}'));
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 1));
}

#[test]
fn an_unterminated_osc_sets_no_title() {
    // An OSC with no string terminator never reaches `osc_dispatch`; no title
    // is recorded.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]0;hello"); // no BEL / ST
    assert_eq!(terminal_state.get_title(), None);
}

#[test]
fn a_very_long_osc_title_is_cut_to_the_cap_and_recovers() {
    // The parser returns to ground for the next command afterward.
    let mut terminal_state = build_terminal_state(5, 3);
    let mut osc_sequence_bytes = Vec::from(&b"\x1b]0;"[..]);
    osc_sequence_bytes.extend(std::iter::repeat_n(b'A', 2000));
    osc_sequence_bytes.push(0x07); // BEL terminator
    process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
    assert_eq!(
        terminal_state.get_title(),
        Some(
            "A".repeat(koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT)
                .as_str()
        )
    );
    // The parser recovered: a following sequence and glyph land normally.
    process_terminal_bytes(&mut terminal_state, b"\x1b[2J");
    terminal_state.print('z');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('z'));
}

// --- Adversarial: boundary geometry ---

#[test]
fn printing_into_a_one_by_one_grid_scrolls_each_glyph_into_history() {
    // A 1x1 screen with autowrap on: each glyph fills the only cell and parks;
    // the next glyph wraps, scrolling the parked one into scrollback.
    let mut terminal_state = build_terminal_state(1, 1);
    terminal_state.print('a'); // fills (0, 0), parks with the wrap latch armed
    assert!(terminal_state.active_cursor().pending_wrap);
    terminal_state.print('b'); // wraps: 'a' scrolls into history, 'b' takes the cell
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('b'));
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0));
    assert!(terminal_state.active_cursor().pending_wrap);
    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 1);
    assert_eq!(
        terminal_state
            .get_scrollback()
            .list_retained_lines()
            .front()
            .expect("a row")
            .0[0]
            .get_character(),
        'a'
    );
}

#[test]
fn a_one_by_one_grid_with_autowrap_off_overwrites_in_place() {
    // With autowrap off each glyph overwrites the sole cell; nothing scrolls.
    let mut terminal_state = build_terminal_state(1, 1);
    process_terminal_bytes(&mut terminal_state, b"\x1b[?7l"); // autowrap off
    terminal_state.print('a');
    assert!(!terminal_state.active_cursor().pending_wrap);
    terminal_state.print('b');
    assert_eq!(get_terminal_glyph(&terminal_state, 0, 0), Some('b'));
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0));
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn decstbm_clamps_a_bottom_margin_past_the_grid() {
    // A bottom margin past the last row is clamped to it. The top margin is
    // above the clamped bottom, and the region is set.
    let mut terminal_state = build_terminal_state(5, 5); // last row 4
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;99r"); // top 2 (1-based), bottom clamped to 4
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 4)));
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0)); // homed
}

#[test]
fn decstbm_with_a_top_past_the_last_row_is_ignored() {
    // A huge top margin clamps to the last row, equal to the clamped bottom.
    // An equal top and bottom is an invalid range: the request is dropped and
    // the prior region stands.
    let mut terminal_state = build_terminal_state(5, 5);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;4r"); // establish (1, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[99;5r"); // top and bottom both clamp to row 4 -> ignored
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 3)));
}

#[test]
fn cursor_moves_on_a_single_row_grid_stay_on_row_zero() {
    // Vertical moves on a one-row screen all clamp to the only row, while
    // horizontal moves still work.
    let mut terminal_state = build_terminal_state(5, 1); // one row, last column 4
    process_terminal_bytes(&mut terminal_state, b"\x1b[9B"); // CUD 9 -> clamped to row 0
    process_terminal_bytes(&mut terminal_state, b"\x1b[3G"); // CHA to column 3 (1-based)
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 2));
    process_terminal_bytes(&mut terminal_state, b"\x1b[9A"); // CUU 9 -> still row 0
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 2));
}

#[test]
fn a_resize_breaks_a_cluster_run() {
    // A resize moves cells to new rows and columns and ends the in-progress
    // cluster run: a combining mark printed after the resize has no base and is
    // dropped.
    let mut terminal_state = build_terminal_state(5, 2);
    print_text(&mut terminal_state, "e");
    terminal_state.resize_terminal_state(PtySize {
        column_count: 5,
        row_count: 2,
    });
    terminal_state.print('\u{301}');

    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.list_combining_characters(), [] as [char; 0]);
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 1));
}

// --- Adversarial: the title is program-controlled text ---

#[test]
fn an_enormous_osc_title_cannot_grow_the_stored_title() {
    let mut terminal_state = build_terminal_state(5, 3);
    let mut osc_sequence_bytes = Vec::from(&b"\x1b]2;"[..]);
    osc_sequence_bytes.extend(std::iter::repeat_n(b'A', 5_000_000));
    osc_sequence_bytes.push(0x07);
    process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
    assert_eq!(
        terminal_state.get_title(),
        Some(
            "A".repeat(koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT)
                .as_str()
        )
    );
}

#[test]
fn an_osc_title_carrying_del_stores_no_del() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;a\x7fb\x07");
    assert_eq!(terminal_state.get_title(), Some("ab"));
}

#[test]
fn an_osc_title_carrying_a_c1_control_stores_none_of_it() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, "\x1b]2;a\u{9b}b\x07".as_bytes());
    assert_eq!(terminal_state.get_title(), Some("ab"));
}

#[test]
fn an_osc_title_carrying_a_bidi_override_stores_none_of_it() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, "\x1b]2;\u{202e}gpj.exe\x07".as_bytes());
    assert_eq!(terminal_state.get_title(), Some("gpj.exe"));
}

#[test]
fn an_osc_title_of_only_control_characters_stores_an_empty_title() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(
        &mut terminal_state,
        "\x1b]2;\u{7f}\u{9b}\u{202e}\x07".as_bytes(),
    );
    assert_eq!(terminal_state.get_title(), Some(""));
}

#[test]
fn a_title_that_is_all_controls_then_text_keeps_the_text() {
    let mut terminal_state = build_terminal_state(5, 3);
    let mut osc_sequence_bytes = Vec::from(&b"\x1b]2;"[..]);
    osc_sequence_bytes.extend(std::iter::repeat_n(0x7f, 1_000));
    osc_sequence_bytes.extend_from_slice(b"shell");
    osc_sequence_bytes.push(0x07);
    process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
    assert_eq!(terminal_state.get_title(), Some("shell"));
}

#[test]
fn a_wide_glyph_title_is_never_cut_inside_a_character() {
    let mut terminal_state = build_terminal_state(5, 3);
    let mut osc_sequence_bytes = Vec::from(&b"\x1b]2;"[..]);
    osc_sequence_bytes.extend("\u{65e5}".repeat(1_000).as_bytes());
    osc_sequence_bytes.push(0x07);
    process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
    // Each character is three bytes: the cap keeps `MAX_REPORTED_TEXT_BYTE_COUNT / 3`
    // whole characters and cuts the next one whole.
    let kept_character_count = koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT / 3;
    assert_eq!(
        terminal_state.get_title(),
        Some("\u{65e5}".repeat(kept_character_count).as_str())
    );
}

#[test]
fn a_truncated_osc_7_uri_is_refused_rather_than_parsed_short() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file://localhost/tmp\x07");
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .map(|reported_working_directory| reported_working_directory
                .get_working_directory_path()
                .to_path_buf()),
        Some(std::path::PathBuf::from("/tmp"))
    );

    let mut osc_sequence_bytes = Vec::from(&b"\x1b]7;file://localhost/"[..]);
    osc_sequence_bytes.extend(std::iter::repeat_n(b'a', 20_000));
    osc_sequence_bytes.push(0x07);
    process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .map(|reported_working_directory| reported_working_directory
                .get_working_directory_path()
                .to_path_buf()),
        Some(std::path::PathBuf::from("/tmp")),
        "a truncated URI replaced the working directory"
    );
}

#[test]
fn an_osc_7_uri_over_the_limit_leaves_the_cwd_alone() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]7;file://localhost/tmp\x07");
    let mut osc_sequence_bytes = Vec::from(&b"\x1b]7;file://localhost/"[..]);
    osc_sequence_bytes.extend(std::iter::repeat_n(b'a', 5_000));
    osc_sequence_bytes.push(0x07);
    process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .map(|reported_working_directory| reported_working_directory
                .get_working_directory_path()
                .to_path_buf()),
        Some(std::path::PathBuf::from("/tmp"))
    );
}

#[test]
fn an_osc_7_host_carrying_a_control_character_stores_none_of_it() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(
        &mut terminal_state,
        "\x1b]7;file://ho\u{7f}st\u{202e}x/tmp\x07".as_bytes(),
    );
    assert_eq!(
        terminal_state
            .get_current_working_directory()
            .and_then(|reported_working_directory| reported_working_directory.get_host()),
        Some("hostx")
    );
}

#[test]
fn an_empty_osc_title_payload_stores_an_empty_title() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;\x07");
    assert_eq!(terminal_state.get_title(), Some(""));
}

#[test]
fn a_non_utf8_title_keeps_its_replacement_characters() {
    // Lossy decoding yields U+FFFD, which is not a refused character.
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;a\xffb\x07");
    assert_eq!(terminal_state.get_title(), Some("a\u{fffd}b"));
}

#[test]
fn a_title_at_the_limit_is_kept_whole_and_one_past_it_is_cut() {
    let max_title_byte_count = koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;
    for (title_byte_count, expected_byte_count) in [
        (max_title_byte_count - 1, max_title_byte_count - 1),
        (max_title_byte_count, max_title_byte_count),
        (max_title_byte_count + 1, max_title_byte_count),
    ] {
        let mut terminal_state = build_terminal_state(5, 3);
        let mut osc_sequence_bytes = Vec::from(&b"\x1b]2;"[..]);
        osc_sequence_bytes.extend(std::iter::repeat_n(b'A', title_byte_count));
        osc_sequence_bytes.push(0x07);
        process_terminal_bytes(&mut terminal_state, &osc_sequence_bytes);
        assert_eq!(
            terminal_state.get_title().map(str::len),
            Some(expected_byte_count),
            "a {title_byte_count}-byte title"
        );
    }
}

#[test]
fn an_osc_7_host_of_only_refused_characters_is_not_a_local_host() {
    // A host of only refused characters filters to `Some("")`. An empty
    // authority (`file:///tmp`) gives `None`. The two differ.
    let mut filtered = build_terminal_state(5, 3);
    process_terminal_bytes(
        &mut filtered,
        "\x1b]7;file://\u{7f}\u{202e}/tmp\x07".as_bytes(),
    );
    assert_eq!(
        filtered
            .get_current_working_directory()
            .and_then(|reported_working_directory| reported_working_directory.get_host()),
        Some("")
    );

    let mut empty_authority = build_terminal_state(5, 3);
    process_terminal_bytes(&mut empty_authority, b"\x1b]7;file:///tmp\x07");
    assert_eq!(
        empty_authority
            .get_current_working_directory()
            .and_then(|reported_working_directory| reported_working_directory.get_host()),
        None
    );
}

// --- Control bytes and the pending-wrap latch ---

#[test]
fn every_cursor_moving_control_byte_clears_the_pending_wrap_latch() {
    // LF, VT, FF, IND, NEL, CR, BS, HT, RI.
    for byte in [0x0Au8, 0x0B, 0x0C, 0x84, 0x85, 0x0D, 0x08, 0x09, 0x8D] {
        let mut terminal_state = build_terminal_state(3, 3);
        process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H"); // row 1, off both margins
        print_text(&mut terminal_state, "abc"); // parks at (1, 2) with the latch armed
        assert!(
            terminal_state.active_cursor().pending_wrap,
            "byte {byte:#x}"
        );
        terminal_state.execute(byte);
        assert!(
            !terminal_state.active_cursor().pending_wrap,
            "byte {byte:#x}"
        );
    }
}

#[test]
fn shift_bell_hts_and_unknown_control_bytes_preserve_the_pending_wrap_latch() {
    // SO, SI, BEL, HTS, SOH: none moves the cursor or clears the latch.
    for byte in [0x0Eu8, 0x0F, 0x07, 0x88, 0x01] {
        let mut terminal_state = build_terminal_state(3, 2);
        print_text(&mut terminal_state, "abc"); // parks at (0, 2) with the latch armed
        terminal_state.execute(byte);
        assert_eq!(
            terminal_state.get_active_cursor_position(),
            (0, 2),
            "byte {byte:#x}"
        );
        assert!(
            terminal_state.active_cursor().pending_wrap,
            "byte {byte:#x}"
        );
    }
}

#[test]
fn a_control_char_reaching_print_breaks_the_cluster_run() {
    let mut terminal_state = build_terminal_state(5, 3);
    terminal_state.print('e'); // base
    terminal_state.print('\u{0}'); // NUL: no display width, dropped, ends the run
    terminal_state.print('\u{301}'); // combining acute has no base → dropped
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.list_combining_characters(), [] as [char; 0]);
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 1));
}

// --- Line feed / reverse index outside the scroll region ---

#[test]
fn line_feed_below_the_region_descends_without_scrolling() {
    let mut terminal_state = build_terminal_state(3, 4); // 4 rows
    process_terminal_bytes(&mut terminal_state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2r"); // region rows 1..2 -> (0, 1); homes the cursor
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;1H"); // row 2, below the region
    terminal_state.execute(b'\n'); // descends to the last grid row, nothing scrolls
    assert_eq!(terminal_state.get_active_cursor_position(), (3, 0));
    terminal_state.execute(b'\n'); // on the last grid row: stays, still nothing scrolls
    assert_eq!(terminal_state.get_active_cursor_position(), (3, 0));
    assert_eq!(get_row_text(&terminal_state, 0), "AAA");
    assert_eq!(get_row_text(&terminal_state, 1), "BBB");
    assert_eq!(get_row_text(&terminal_state, 2), "CCC");
    assert_eq!(get_row_text(&terminal_state, 3), "DDD");
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn reverse_index_below_the_region_moves_up_without_scrolling() {
    let mut terminal_state = build_terminal_state(3, 4);
    process_terminal_bytes(&mut terminal_state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2r"); // region (0, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;1H"); // row 3, below the region
    process_terminal_bytes(&mut terminal_state, b"\x1bM"); // RI: row 3 -> 2, nothing scrolls
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 0));
    assert_eq!(get_row_text(&terminal_state, 0), "AAA");
    assert_eq!(get_row_text(&terminal_state, 1), "BBB");
    assert_eq!(get_row_text(&terminal_state, 2), "CCC");
    assert_eq!(get_row_text(&terminal_state, 3), "DDD");
}

#[test]
fn reverse_index_on_row_zero_above_the_region_stays_put() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3r"); // region rows 2..3 -> (1, 2); homes the cursor
    process_terminal_bytes(&mut terminal_state, b"\x1bM"); // RI on row 0, above the region top
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0));
    assert_eq!(get_row_text(&terminal_state, 0), "abc");
    assert_eq!(get_row_text(&terminal_state, 1), "def");
    assert_eq!(get_row_text(&terminal_state, 2), "ghi");
}

// --- Line and cell operations with an oversized count ---

#[test]
fn dl_outside_the_region_is_ignored() {
    let mut terminal_state = build_terminal_state(3, 4);
    process_terminal_bytes(&mut terminal_state, b"AAA\r\nBBB\r\nCCC\r\nDDD");
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3r"); // region rows 2..3 -> (1, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[4;2H"); // (3, 1), below the region
    process_terminal_bytes(&mut terminal_state, b"\x1b[M"); // DL ignored outside the region
    assert_eq!(get_row_text(&terminal_state, 0), "AAA");
    assert_eq!(get_row_text(&terminal_state, 1), "BBB");
    assert_eq!(get_row_text(&terminal_state, 2), "CCC");
    assert_eq!(get_row_text(&terminal_state, 3), "DDD");
    assert_eq!(terminal_state.get_active_cursor_position(), (3, 1)); // cursor untouched
    assert!(terminal_state.get_scrollback().is_empty());
}

#[test]
fn su_past_the_region_height_blanks_the_region_and_feeds_every_row() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;2H"); // cursor -> (1, 1)
    process_terminal_bytes(&mut terminal_state, b"\x1b[99S"); // SU 99 on a 3-row region: all three rows leave
    assert_eq!(get_row_text(&terminal_state, 0), "   ");
    assert_eq!(get_row_text(&terminal_state, 1), "   ");
    assert_eq!(get_row_text(&terminal_state, 2), "   ");
    assert_eq!(terminal_state.get_active_cursor_position(), (1, 1)); // cursor unmoved
    let scrollback_rows: Vec<String> = terminal_state
        .get_scrollback()
        .list_retained_lines()
        .iter()
        .map(|(row_cells, _)| row_cells.iter().map(Cell::get_character).collect())
        .collect();
    assert_eq!(scrollback_rows, vec!["abc", "def", "ghi"]); // oldest (top) first
}

#[test]
fn sd_past_the_region_height_blanks_the_region() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi; cursor parked at (2, 2)
    process_terminal_bytes(&mut terminal_state, b"\x1b[99T"); // SD 99 on a 3-row region: every row pushed off
    assert_eq!(get_row_text(&terminal_state, 0), "   ");
    assert_eq!(get_row_text(&terminal_state, 1), "   ");
    assert_eq!(get_row_text(&terminal_state, 2), "   ");
    assert_eq!(terminal_state.get_active_cursor_position(), (2, 2)); // cursor unmoved
    assert!(terminal_state.get_scrollback().is_empty()); // nothing left through the top
}

#[test]
fn il_past_the_region_height_blanks_the_region_from_the_cursor_row() {
    let mut terminal_state = build_terminal_state(3, 3);
    fill_three_by_three_grid(&mut terminal_state); // abc / def / ghi
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H"); // cursor -> (1, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b[99L"); // IL 99: rows 1..=2 pushed off, blanks inserted
    assert_eq!(get_row_text(&terminal_state, 0), "abc"); // above the cursor, untouched
    assert_eq!(get_row_text(&terminal_state, 1), "   ");
    assert_eq!(get_row_text(&terminal_state, 2), "   ");
    assert_eq!(terminal_state.get_active_cursor_position(), (1, 0)); // cursor unmoved
}

#[test]
fn ich_count_past_the_line_end_blanks_the_rest_of_the_line() {
    let mut terminal_state = build_terminal_state(5, 1);
    process_terminal_bytes(&mut terminal_state, b"abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;3H"); // cursor -> (0, 2) on 'c'
    process_terminal_bytes(&mut terminal_state, b"\x1b[99@"); // ICH 99: c, d, e all pushed off the edge
    assert_eq!(get_row_text(&terminal_state, 0), "ab   ");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 2));
}

#[test]
fn dch_count_past_the_line_end_blanks_the_rest_of_the_line() {
    let mut terminal_state = build_terminal_state(5, 1);
    process_terminal_bytes(&mut terminal_state, b"abcde");
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2H"); // cursor -> (0, 1) on 'b'
    process_terminal_bytes(&mut terminal_state, b"\x1b[99P"); // DCH 99: b..e removed, the tail padded
    assert_eq!(get_row_text(&terminal_state, 0), "a    ");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 1));
}

// --- DECSTBM / DECSCUSR parameter edges ---

#[test]
fn decstbm_bottom_zero_means_the_last_row() {
    let mut terminal_state = build_terminal_state(5, 5); // last row 4
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;3H"); // move away from home
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;0r"); // top 2 (1-based), bottom 0 -> the last row
    assert_eq!(terminal_state.primary_scroll_region, Some((1, 4)));
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0)); // homed
}

#[test]
fn decscusr_reads_only_its_first_parameter() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;5 q"); // 2 = steady block; the 5 is ignored
    assert_eq!(terminal_state.get_cursor_shape(), Some(CursorShape::Block));
    assert!(!terminal_state.is_cursor_blink_enabled());
}

// --- OSC edges: unparseable commands, missing payloads, shell-marker facts ---

#[test]
fn an_osc_with_a_non_utf8_command_number_is_ignored() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;keep\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]\xff;x\x07"); // command byte 0xFF is not UTF-8
    assert_eq!(terminal_state.get_title(), Some("keep"));
    assert!(terminal_state.get_current_working_directory().is_none());
}

#[test]
fn an_osc_title_command_with_no_payload_leaves_the_title_alone() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]2;keep\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b]2\x07"); // params = ["2"]: no payload
    assert_eq!(terminal_state.get_title(), Some("keep"));
}

#[test]
fn osc133_prompt_marks_the_cursor_row_not_row_zero() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;1H"); // cursor -> (1, 0)
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;A\x07");
    assert!(terminal_state.get_active_grid().has_prompt_mark(1));
    assert!(!terminal_state.get_active_grid().has_prompt_mark(0));
    assert!(!terminal_state.get_active_grid().has_prompt_mark(2));
}

#[test]
fn osc133_command_start_reports_one_started_fact_even_when_repeated() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;C\x07\x1b]133;C\x07");
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Running
    );
    assert_eq!(
        terminal_state.take_shell_integration_facts(),
        vec![ShellIntegrationFact::CommandStarted]
    );
}

#[test]
fn osc133_finish_reports_the_exit_code() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;C\x07\x1b]133;D;137\x07");
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(
        terminal_state.take_shell_integration_facts(),
        vec![
            ShellIntegrationFact::CommandStarted,
            ShellIntegrationFact::CommandFinished {
                exit_code: Some(137)
            },
        ]
    );
}

#[test]
fn osc133_finish_without_an_exit_code_reports_none() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;C\x07\x1b]133;D\x07");
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(
        terminal_state.take_shell_integration_facts(),
        vec![
            ShellIntegrationFact::CommandStarted,
            ShellIntegrationFact::CommandFinished { exit_code: None },
        ]
    );
}

#[test]
fn osc133_finish_without_a_running_command_reports_nothing() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;D;3\x07"); // no command started
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(terminal_state.take_shell_integration_facts(), vec![]);
}

// --- RIS and the shell marker terminal_state ---

#[test]
fn ris_returns_the_shell_marker_state_to_prompt_and_keeps_pending_facts() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;C\x07"); // a command is running
    process_terminal_bytes(&mut terminal_state, b"\x1bc"); // RIS
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(
        terminal_state.take_shell_integration_facts(),
        vec![ShellIntegrationFact::CommandStarted]
    );
    process_terminal_bytes(&mut terminal_state, b"\x1b]133;D;0\x07"); // the reset forgot the running command
    assert_eq!(
        terminal_state.shell_integration_state,
        ShellIntegrationState::Prompt
    );
    assert_eq!(terminal_state.take_shell_integration_facts(), vec![]);
}

// --- ESC / DCS edges ---

#[test]
fn decaln_is_ignored() {
    let mut terminal_state = build_terminal_state(3, 2);
    print_text(&mut terminal_state, "ab");
    process_terminal_bytes(&mut terminal_state, b"\x1b#8"); // DECALN: an ESC with an unhandled intermediate
    assert_eq!(get_row_text(&terminal_state, 0), "ab ");
    assert_eq!(get_row_text(&terminal_state, 1), "   ");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 2));
}

#[test]
fn an_ignored_esc_intermediate_breaks_a_cluster_run() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('e'); // base
    process_terminal_bytes(&mut terminal_state, b"\x1b#8"); // ignored, but ends the run
    terminal_state.print('\u{301}'); // combining acute has no base → dropped
    let cell = terminal_state
        .get_active_grid()
        .get_cell(0, 0)
        .expect("in bounds");
    assert_eq!(cell.get_character(), 'e');
    assert_eq!(cell.list_combining_characters(), [] as [char; 0]);
}

#[test]
fn a_dcs_payload_prints_nothing() {
    let mut terminal_state = build_terminal_state(5, 2);
    terminal_state.print('a');
    process_terminal_bytes(&mut terminal_state, b"\x1bPqhello\x1b\\"); // DCS q "hello" ST
    assert_eq!(get_row_text(&terminal_state, 0), "a    ");
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 1));
}

#[test]
fn an_overlong_sgr_is_dropped_without_touching_the_pen() {
    let mut terminal_state = build_terminal_state(5, 2);
    let mut seq = Vec::from(&b"\x1b["[..]);
    seq.extend(std::iter::repeat_n(&b"1;"[..], 40).flatten().copied());
    seq.push(b'm'); // 40 bold codes: past vte's 32-parameter cap, flagged ignore
    process_terminal_bytes(&mut terminal_state, &seq);
    assert_eq!(terminal_state.active_render().style, Style::default());
}

// --- Alternate screen: a `?1049` entry while already on the alternate ---

#[test]
fn dec_1049_entry_while_already_on_the_alternate_changes_nothing() {
    let mut terminal_state = build_terminal_state(5, 3);
    process_terminal_bytes(&mut terminal_state, b"\x1b[3;4H"); // primary cursor -> (2, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?47h"); // enter without saving or clearing
    process_terminal_bytes(&mut terminal_state, b"xyz"); // alternate row 0 = "xyz", cursor (0, 3)
    process_terminal_bytes(&mut terminal_state, b"\x1b[?1049h"); // already on the alternate
    assert_eq!(terminal_state.active_screen, Screen::Alternate);
    assert_eq!(get_row_text(&terminal_state, 0), "xyz  "); // not cleared
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 3)); // not re-seeded
    assert_eq!(terminal_state.primary_cursor.saved, None); // primary cursor not stashed
    assert_eq!(terminal_state.alternate_cursor.saved, None);
}

#[test]
fn a_wrap_on_the_last_row_below_the_region_leaves_the_row_hard() {
    // The cursor is on the last grid row and outside the scroll region, so the
    // line feed neither moves it nor scrolls: no row follows this one, so the
    // row is not marked as continuing into one.
    let mut terminal_state = build_terminal_state(3, 4);
    process_terminal_bytes(&mut terminal_state, b"\x1b[1;2r\x1b[4;1H");
    print_text(&mut terminal_state, "abcd");

    assert_eq!(terminal_state.get_active_cursor_position(), (3, 1));
    assert_eq!(
        terminal_state.get_active_grid().get_row_end(3),
        RowEnd::Hard
    );
}

#[test]
fn ed_two_clears_the_prompt_marks_of_every_row_it_erases() {
    let mut terminal_state = build_terminal_state(5, 3);
    for row_index in 1..=3 {
        process_terminal_bytes(
            &mut terminal_state,
            format!("\x1b[{row_index};1H").as_bytes(),
        );
        process_terminal_bytes(&mut terminal_state, b"\x1b]133;A\x07");
    }
    process_terminal_bytes(&mut terminal_state, b"\x1b[2J");
    assert!(
        (0..3).all(|row_index| { !terminal_state.get_active_grid().has_prompt_mark(row_index) })
    );
}

#[test]
fn ed_zero_clears_the_prompt_marks_below_the_cursor_and_keeps_the_cursor_row() {
    let mut terminal_state = build_terminal_state(5, 3);
    for row_index in 1..=3 {
        process_terminal_bytes(
            &mut terminal_state,
            format!("\x1b[{row_index};1H").as_bytes(),
        );
        process_terminal_bytes(&mut terminal_state, b"\x1b]133;A\x07");
    }
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3H\x1b[0J"); // cursor on row 1, column 2
    assert!(terminal_state.get_active_grid().has_prompt_mark(0));
    assert!(terminal_state.get_active_grid().has_prompt_mark(1));
    assert!(!terminal_state.get_active_grid().has_prompt_mark(2));
}

#[test]
fn ed_one_clears_the_prompt_marks_above_the_cursor_and_keeps_the_cursor_row() {
    let mut terminal_state = build_terminal_state(5, 3);
    for row_index in 1..=3 {
        process_terminal_bytes(
            &mut terminal_state,
            format!("\x1b[{row_index};1H").as_bytes(),
        );
        process_terminal_bytes(&mut terminal_state, b"\x1b]133;A\x07");
    }
    process_terminal_bytes(&mut terminal_state, b"\x1b[2;3H\x1b[1J"); // cursor on row 1, column 2
    assert!(!terminal_state.get_active_grid().has_prompt_mark(0));
    assert!(terminal_state.get_active_grid().has_prompt_mark(1));
    assert!(terminal_state.get_active_grid().has_prompt_mark(2));
}

#[test]
fn el_two_clears_the_row_prompt_mark_and_the_partial_erases_keep_it() {
    for (sequence, kept) in [
        (b"\x1b[2K".as_slice(), false),
        (b"\x1b[0K".as_slice(), true),
        (b"\x1b[1K".as_slice(), true),
    ] {
        let mut terminal_state = build_terminal_state(5, 3);
        process_terminal_bytes(&mut terminal_state, b"\x1b[2;3H\x1b]133;A\x07");
        process_terminal_bytes(&mut terminal_state, sequence);
        assert_eq!(
            terminal_state.get_active_grid().has_prompt_mark(1),
            kept,
            "{sequence:?}"
        );
    }
}

#[test]
fn a_cleared_screen_shrinks_without_pushing_blank_rows_into_history() {
    // The bottom row carries a prompt mark, `ED 2` erases every row, and the
    // pane then shrinks from 24 rows to 10. With the mark gone the reflow
    // drops the 14 surplus blank rows; a mark left on a blanked row would
    // have parked all 14 in history instead.
    let mut terminal_state = TerminalState::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    process_terminal_bytes(&mut terminal_state, b"\x1b[24;1H\x1b]133;A\x07");
    process_terminal_bytes(&mut terminal_state, b"\x1b[H\x1b[2J");

    terminal_state.resize_terminal_state(PtySize {
        column_count: 80,
        row_count: 10,
    });

    assert_eq!(terminal_state.get_scrollback().get_retained_line_count(), 0);
    assert_eq!(
        terminal_state
            .get_scrollback()
            .get_total_pushed_line_count(),
        0
    );
    assert_eq!(
        terminal_state.get_active_grid().get_grid_dimensions(),
        (10, 80)
    );
    assert_eq!(terminal_state.get_active_cursor_position(), (0, 0));
}
