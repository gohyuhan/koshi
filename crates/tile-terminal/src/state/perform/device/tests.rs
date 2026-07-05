//! Unit tests for device-query replies: DA1/DA2/DA3 identity bytes, DSR
//! operating status, CPR/DECXCPR cursor position, the DEC-form DSR family
//! (printer/UDK/keyboard/locator/macro/checksum/integrity/session), DECRQM/RQM
//! mode reports, version packing, and reply-queue accumulation and draining.

use super::*;
use tile_core::process::PtySize;

/// Build per-pane state of `cols × rows`.
fn state(cols: u16, rows: u16) -> TerminalState {
    TerminalState::new(PtySize { cols, rows })
}

/// Feed `bytes` through a fresh parser into `state`.
fn feed(state: &mut TerminalState, bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    parser.advance(state, bytes);
}

/// Feed `bytes` into a fresh state and return the drained replies.
fn replies_for(bytes: &[u8]) -> Vec<u8> {
    let mut state = state(8, 4);
    feed(&mut state, bytes);
    state.take_replies()
}

#[test]
fn version_number_packs_two_digits_per_component() {
    assert_eq!(version_number("1.16.2"), 11602);
    assert_eq!(version_number("0.1.0"), 100);
    assert_eq!(version_number("12.34.56"), 123456);
}

#[test]
fn version_number_counts_an_unparseable_component_as_zero() {
    // "0-alpha" fails to parse, so the patch digit contributes zero.
    assert_eq!(version_number("0.1.0-alpha"), 100);
    assert_eq!(version_number("dev"), 0);
}

#[test]
fn da1_identifies_a_vt220_with_ansi_color() {
    assert_eq!(replies_for(b"\x1b[c"), b"\x1b[?62;22c");
}

#[test]
fn da1_with_an_explicit_zero_parameter_replies() {
    assert_eq!(replies_for(b"\x1b[0c"), b"\x1b[?62;22c");
}

#[test]
fn da1_with_a_nonzero_parameter_gets_no_reply() {
    assert_eq!(replies_for(b"\x1b[1c"), b"");
}

#[test]
fn da2_reports_type_version_and_zero_cartridge() {
    let expected = format!("\x1b[>1;{};0c", version_number(env!("CARGO_PKG_VERSION")));
    assert_eq!(replies_for(b"\x1b[>c"), expected.as_bytes());
}

#[test]
fn da2_with_a_nonzero_parameter_gets_no_reply() {
    assert_eq!(replies_for(b"\x1b[>1c"), b"");
}

#[test]
fn da3_reports_an_all_zero_unit_id() {
    assert_eq!(replies_for(b"\x1b[=c"), b"\x1bP!|00000000\x1b\\");
    assert_eq!(replies_for(b"\x1b[=0c"), b"\x1bP!|00000000\x1b\\");
}

#[test]
fn da3_with_a_nonzero_parameter_gets_no_reply() {
    assert_eq!(replies_for(b"\x1b[=1c"), b"");
}

#[test]
fn dsr_5_reports_operating_status_ok() {
    assert_eq!(replies_for(b"\x1b[5n"), b"\x1b[0n");
}

#[test]
fn dsr_6_reports_the_home_position_one_based() {
    assert_eq!(replies_for(b"\x1b[6n"), b"\x1b[1;1R");
}

#[test]
fn dsr_6_reports_the_cursor_after_motion() {
    // CUP to row 3, column 5 (1-based), then query.
    assert_eq!(replies_for(b"\x1b[3;5H\x1b[6n"), b"\x1b[3;5R");
}

#[test]
fn dsr_6_reports_the_alternate_screens_cursor_while_active() {
    let mut state = state(8, 4);
    // Move on the primary, enter the alternate (fresh cursor seeded from the
    // primary's position), then move on the alternate and query.
    feed(&mut state, b"\x1b[3;5H\x1b[?1049h\x1b[2;2H\x1b[6n");
    assert_eq!(state.take_replies(), b"\x1b[2;2R");
}

#[test]
fn dsr_with_an_unknown_parameter_gets_no_reply() {
    assert_eq!(replies_for(b"\x1b[7n"), b"");
}

#[test]
fn dsr_with_no_parameter_gets_no_reply() {
    assert_eq!(replies_for(b"\x1b[n"), b"");
}

#[test]
fn decxcpr_reports_the_cursor_in_the_dec_form_without_a_page() {
    assert_eq!(replies_for(b"\x1b[3;5H\x1b[?6n"), b"\x1b[?3;5R");
}

#[test]
fn dec_dsr_reports_no_printer() {
    assert_eq!(replies_for(b"\x1b[?15n"), b"\x1b[?13n");
}

#[test]
fn dec_dsr_reports_udks_locked() {
    assert_eq!(replies_for(b"\x1b[?25n"), b"\x1b[?21n");
}

#[test]
fn dec_dsr_reports_the_keyboard_ready() {
    assert_eq!(replies_for(b"\x1b[?26n"), b"\x1b[?27;1;0;0n");
}

#[test]
fn dec_dsr_reports_no_locator_on_both_status_forms() {
    assert_eq!(replies_for(b"\x1b[?53n"), b"\x1b[?53n");
    assert_eq!(replies_for(b"\x1b[?55n"), b"\x1b[?53n");
}

#[test]
fn dec_dsr_reports_an_unidentifiable_locator_type() {
    assert_eq!(replies_for(b"\x1b[?56n"), b"\x1b[?57;0n");
}

#[test]
fn dec_dsr_reports_zero_macro_space() {
    assert_eq!(replies_for(b"\x1b[?62n"), b"\x1b[0*{");
}

#[test]
fn dec_dsr_reports_a_zero_memory_checksum_echoing_the_request_id() {
    assert_eq!(replies_for(b"\x1b[?63n"), b"\x1bP0!~0000\x1b\\");
    assert_eq!(replies_for(b"\x1b[?63;7n"), b"\x1bP7!~0000\x1b\\");
}

#[test]
fn dec_dsr_reports_data_integrity_ok() {
    assert_eq!(replies_for(b"\x1b[?75n"), b"\x1b[?70n");
}

#[test]
fn dec_dsr_reports_no_multi_session_support() {
    assert_eq!(replies_for(b"\x1b[?85n"), b"\x1b[?83n");
}

#[test]
fn dec_dsr_with_an_unknown_parameter_gets_no_reply() {
    assert_eq!(replies_for(b"\x1b[?5n"), b"");
    assert_eq!(replies_for(b"\x1b[?99n"), b"");
}

#[test]
fn decrqm_reports_a_default_reset_mode_as_reset() {
    assert_eq!(replies_for(b"\x1b[?2004$p"), b"\x1b[?2004;2$y");
}

#[test]
fn decrqm_reports_a_set_mode_as_set() {
    assert_eq!(replies_for(b"\x1b[?2004h\x1b[?2004$p"), b"\x1b[?2004;1$y");
}

#[test]
fn decrqm_reports_default_on_autowrap_as_set_then_reset_after_disable() {
    assert_eq!(replies_for(b"\x1b[?7$p"), b"\x1b[?7;1$y");
    assert_eq!(replies_for(b"\x1b[?7l\x1b[?7$p"), b"\x1b[?7;2$y");
}

#[test]
fn decrqm_reports_cursor_visibility_per_active_screen() {
    assert_eq!(replies_for(b"\x1b[?25$p"), b"\x1b[?25;1$y");
    assert_eq!(replies_for(b"\x1b[?25l\x1b[?25$p"), b"\x1b[?25;2$y");

    // Visibility is tracked per screen: hiding on the alternate reports
    // hidden there, and the untouched primary reports visible again on exit.
    let mut state = state(8, 4);
    feed(
        &mut state,
        b"\x1b[?1049h\x1b[?25l\x1b[?25$p\x1b[?1049l\x1b[?25$p",
    );
    assert_eq!(state.take_replies(), b"\x1b[?25;2$y\x1b[?25;1$y");
}

#[test]
fn decrqm_reports_every_alt_screen_mode_from_the_active_screen() {
    for mode in ["47", "1047", "1049"] {
        let query = format!("\x1b[?{mode}$p");
        let on_primary = replies_for(query.as_bytes());
        assert_eq!(on_primary, format!("\x1b[?{mode};2$y").as_bytes());

        let entered = format!("\x1b[?1049h\x1b[?{mode}$p");
        let on_alternate = replies_for(entered.as_bytes());
        assert_eq!(on_alternate, format!("\x1b[?{mode};1$y").as_bytes());
    }
}

#[test]
fn decrqm_reports_the_active_mouse_tracking_level_and_only_it() {
    let levels = ["9", "1000", "1002", "1003"];
    // Enable each level in turn and query all four: only the active one is
    // set.
    for active in levels {
        let mut state = state(8, 4);
        let mut bytes = format!("\x1b[?{active}h");
        let mut expected = String::new();
        for level in levels {
            bytes.push_str(&format!("\x1b[?{level}$p"));
            let value = if level == active { 1 } else { 2 };
            expected.push_str(&format!("\x1b[?{level};{value}$y"));
        }
        feed(&mut state, bytes.as_bytes());
        assert_eq!(state.take_replies(), expected.as_bytes());
    }
}

#[test]
fn decrqm_reports_the_active_mouse_encoding_and_only_it() {
    let encodings = ["1005", "1006", "1015"];
    for active in encodings {
        let mut state = state(8, 4);
        let mut bytes = format!("\x1b[?{active}h");
        let mut expected = String::new();
        for encoding in encodings {
            bytes.push_str(&format!("\x1b[?{encoding}$p"));
            let value = if encoding == active { 1 } else { 2 };
            expected.push_str(&format!("\x1b[?{encoding};{value}$y"));
        }
        feed(&mut state, bytes.as_bytes());
        assert_eq!(state.take_replies(), expected.as_bytes());
    }
}

#[test]
fn decrqm_reports_the_remaining_stored_flags() {
    // ?1 DECCKM, ?5 DECSCNM, ?12 cursor blink, ?1007 alt scroll: default
    // reset, set after their DECSET.
    assert_eq!(replies_for(b"\x1b[?1$p"), b"\x1b[?1;2$y");
    assert_eq!(replies_for(b"\x1b[?1h\x1b[?1$p"), b"\x1b[?1;1$y");
    assert_eq!(replies_for(b"\x1b[?5$p"), b"\x1b[?5;2$y");
    assert_eq!(replies_for(b"\x1b[?5h\x1b[?5$p"), b"\x1b[?5;1$y");
    assert_eq!(replies_for(b"\x1b[?12$p"), b"\x1b[?12;2$y");
    assert_eq!(replies_for(b"\x1b[?12h\x1b[?12$p"), b"\x1b[?12;1$y");
    assert_eq!(replies_for(b"\x1b[?1007$p"), b"\x1b[?1007;2$y");
    assert_eq!(replies_for(b"\x1b[?1007h\x1b[?1007$p"), b"\x1b[?1007;1$y");
}

#[test]
fn decrqm_reports_an_unstored_mode_as_not_recognized() {
    // ?2/?3/?8 are traced no-ops, ?1048 keeps no queryable state, ?9999 is
    // unknown: all report 0.
    for mode in ["2", "3", "8", "1048", "9999"] {
        let query = format!("\x1b[?{mode}$p");
        let expected = format!("\x1b[?{mode};0$y");
        assert_eq!(replies_for(query.as_bytes()), expected.as_bytes());
    }
}

#[test]
fn ansi_rqm_reports_every_mode_as_not_recognized() {
    assert_eq!(replies_for(b"\x1b[4$p"), b"\x1b[4;0$y");
    assert_eq!(replies_for(b"\x1b[20$p"), b"\x1b[20;0$y");
}

#[test]
fn replies_accumulate_in_query_order() {
    assert_eq!(replies_for(b"\x1b[5n\x1b[c"), b"\x1b[0n\x1b[?62;22c");
}

#[test]
fn take_replies_drains_the_queue() {
    let mut state = state(8, 4);
    feed(&mut state, b"\x1b[5n");
    assert_eq!(state.take_replies(), b"\x1b[0n");
    assert_eq!(state.take_replies(), b"");
}

#[test]
fn a_query_flagged_ignore_by_the_parser_gets_no_reply() {
    // 40 parameters overflow vte's parameter list, so the sequence arrives
    // with `ignore` set and is dropped before dispatch.
    let mut query = String::from("\x1b[");
    query.push_str(&"5;".repeat(40));
    query.push('n');
    assert_eq!(replies_for(query.as_bytes()), b"");
}

#[test]
fn plain_output_produces_no_replies() {
    assert_eq!(replies_for(b"hello \x1b[31mworld\x1b[0m\r\n"), b"");
}
