//! Unit tests for the Kitty keyboard sequences driven through the parser:
//! push, pop and set on the active screen, the query reply, the per-screen
//! split across alternate-screen entry and exit, and the resets.

use super::*;
use crate::state::Screen;
use koshi_core::process::PtySize;

/// Build terminal state for one pane of `column_count × row_count`.
fn build_terminal_state(column_count: u16, row_count: u16) -> TerminalState {
    TerminalState::from_pty_size(PtySize {
        column_count,
        row_count,
    })
}

/// Feed `input_bytes` through a fresh parser into `terminal_state`.
fn advance_vte_parser(terminal_state: &mut TerminalState, input_bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    parser.advance(terminal_state, input_bytes);
}

/// Feed `input_bytes` into a fresh 8×4 terminal state and return it.
fn build_terminal_state_after(input_bytes: &[u8]) -> TerminalState {
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(&mut terminal_state, input_bytes);
    terminal_state
}

#[test]
fn bare_push_adds_an_all_off_entry() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[>u");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);

    advance_vte_parser(&mut terminal_state, b"\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn push_keeps_every_known_flag_bit() {
    let terminal_state = build_terminal_state_after(b"\x1b[>17u");

    assert_eq!(terminal_state.get_keyboard_flags(), 17);
}

#[test]
fn push_drops_the_bits_above_the_known_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>33u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn push_past_the_depth_bound_drops_the_oldest_entry() {
    let mut terminal_state = build_terminal_state_after(
        b"\x1b[>1u\x1b[>2u\x1b[>3u\x1b[>4u\x1b[>5u\x1b[>6u\x1b[>7u\x1b[>8u\x1b[>9u",
    );

    for expected_flags in [9, 8, 7, 6, 5, 4, 3, 2] {
        assert_eq!(terminal_state.get_keyboard_flags(), expected_flags);
        advance_vte_parser(&mut terminal_state, b"\x1b[<u");
    }

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn bare_pop_removes_one_entry() {
    let terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[>8u\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn pop_of_zero_removes_one_entry() {
    let terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[>8u\x1b[<0u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn pop_past_the_entries_held_leaves_no_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[<3u");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn pop_with_no_entries_leaves_no_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn set_then_push_then_pop_restores_the_flags_the_push_covered() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[=1;1u\x1b[>8u\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);

    advance_vte_parser(&mut terminal_state, b"\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn set_on_an_empty_stack_creates_the_entry() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[=4u");

    assert_eq!(terminal_state.get_keyboard_flags(), 4);

    advance_vte_parser(&mut terminal_state, b"\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn set_with_no_mode_replaces_the_current_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>9u\x1b[=4u");

    assert_eq!(terminal_state.get_keyboard_flags(), 4);
}

#[test]
fn set_mode_zero_replaces_the_current_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>9u\x1b[=4;0u");

    assert_eq!(terminal_state.get_keyboard_flags(), 4);
}

#[test]
fn set_mode_two_adds_to_the_current_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>9u\x1b[=4;2u");

    assert_eq!(terminal_state.get_keyboard_flags(), 13);
}

#[test]
fn set_mode_three_clears_from_the_current_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>13u\x1b[=4;3u");

    assert_eq!(terminal_state.get_keyboard_flags(), 9);
}

#[test]
fn set_with_an_unknown_mode_changes_nothing() {
    let terminal_state = build_terminal_state_after(b"\x1b[>9u\x1b[=4;7u");

    assert_eq!(terminal_state.get_keyboard_flags(), 9);
}

#[test]
fn set_leaves_the_preceding_entries_as_they_are() {
    let mut terminal_state =
        build_terminal_state_after(b"\x1b[>1u\x1b[>2u\x1b[=4;1u\x1b[=8;2u\x1b[=4;3u");

    assert_eq!(terminal_state.get_keyboard_flags(), 8);

    advance_vte_parser(&mut terminal_state, b"\x1b[<u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn query_with_no_entries_reports_no_flags() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[?u");

    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[?0u");
}

#[test]
fn query_reports_the_active_screen_flags() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>17u\x1b[?u");

    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[?17u");
}

#[test]
fn query_on_the_alternate_screen_reports_that_screen_flags() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?1049h\x1b[>8u\x1b[?u");

    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[?8u");
}

#[test]
fn alternate_screen_flags_never_reach_the_primary() {
    let terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?1049h\x1b[>8u\x1b[?1049l");

    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn an_alternate_buffer_reset_empties_the_alternate_stack() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?1049h\x1b[>8u\x1b[?1049l");

    advance_vte_parser(&mut terminal_state, b"\x1b[?1049h");

    assert_eq!(terminal_state.get_active_screen(), Screen::Alternate);
    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn a_repeated_alternate_entry_leaves_the_alternate_stack_as_it_is() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[?1049h\x1b[>8u");

    advance_vte_parser(&mut terminal_state, b"\x1b[?1049h");

    assert_eq!(terminal_state.get_active_screen(), Screen::Alternate);
    assert_eq!(terminal_state.get_keyboard_flags(), 8);
}

#[test]
fn a_buffer_switch_without_a_reset_keeps_each_screen_stack() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?47h\x1b[>8u\x1b[?47l");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);

    advance_vte_parser(&mut terminal_state, b"\x1b[?47h");

    assert_eq!(terminal_state.get_keyboard_flags(), 8);
}

#[test]
fn hard_reset_empties_both_screen_stacks() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?1049h\x1b[>8u\x1bc");

    assert_eq!(terminal_state.get_active_screen(), Screen::Primary);
    assert_eq!(terminal_state.get_keyboard_flags(), 0);

    advance_vte_parser(&mut terminal_state, b"\x1b[?47h");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn soft_reset_leaves_both_screen_stacks_as_they_are() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?1049h\x1b[>8u\x1b[!p");

    assert_eq!(terminal_state.get_active_screen(), Screen::Alternate);
    assert_eq!(terminal_state.get_keyboard_flags(), 8);

    advance_vte_parser(&mut terminal_state, b"\x1b[?1049l");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn a_resize_leaves_both_screen_stacks_as_they_are() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[?1049h\x1b[>8u");

    terminal_state.resize_terminal_state(PtySize {
        column_count: 20,
        row_count: 10,
    });

    assert_eq!(terminal_state.get_keyboard_flags(), 8);

    advance_vte_parser(&mut terminal_state, b"\x1b[?1049l");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn csi_u_with_no_intermediate_restores_the_saved_cursor() {
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(
        &mut terminal_state,
        b"\x1b[2;3H\x1b[s\x1b[1;1H\x1b[>8u\x1b[u",
    );

    assert_eq!(terminal_state.get_active_cursor_position(), (1, 2));
    assert_eq!(terminal_state.get_keyboard_flags(), 8);
    assert_eq!(terminal_state.take_device_query_replies(), b"");
}

#[test]
fn a_query_after_a_set_on_an_empty_stack_reports_the_new_flags() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[=4u\x1b[?u");

    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[?4u");
}

#[test]
fn a_query_after_a_pop_to_empty_reports_no_flags() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>8u\x1b[<u\x1b[?u");

    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[?0u");
}

#[test]
fn a_push_reads_only_its_first_parameter() {
    let terminal_state = build_terminal_state_after(b"\x1b[>1;2u");

    assert_eq!(terminal_state.get_keyboard_flags(), 1);
}

#[test]
fn a_set_with_a_subparameter_mode_replaces_the_current_flags() {
    let terminal_state = build_terminal_state_after(b"\x1b[>9u\x1b[=4:2u");

    assert_eq!(terminal_state.get_keyboard_flags(), 4);
}

#[test]
fn a_pop_of_every_representable_entry_empties_the_stack() {
    let terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[>8u\x1b[<65535u");

    assert_eq!(terminal_state.get_keyboard_flags(), 0);
}

#[test]
fn a_sequence_with_too_many_markers_changes_nothing() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>8u\x1b[>>>1u\x1b[<<<u\x1b[???u");

    assert_eq!(terminal_state.get_keyboard_flags(), 8);
    assert_eq!(terminal_state.take_device_query_replies(), b"");
}

#[test]
fn a_query_reply_queues_behind_the_other_device_replies() {
    let mut terminal_state = build_terminal_state_after(b"\x1b[>1u\x1b[c\x1b[?u");

    assert_eq!(
        terminal_state.take_device_query_replies(),
        b"\x1b[?62;22c\x1b[?1u"
    );
}
