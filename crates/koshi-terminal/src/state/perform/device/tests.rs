//! Unit tests for device-query replies: DA1/DA2/DA3 identity bytes, DSR
//! operating status, CPR/DECXCPR cursor position, the DEC-form DSR family
//! (printer/UDK/keyboard/locator/macro/checksum/integrity/session), DECRQM/RQM
//! mode reports, version packing, and reply-queue accumulation and draining.

use super::*;
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

/// Feed `input_bytes` into a fresh terminal state and return its drained replies.
fn collect_device_replies_for(input_bytes: &[u8]) -> Vec<u8> {
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(&mut terminal_state, input_bytes);
    terminal_state.take_device_query_replies()
}

#[test]
fn compute_version_number_packs_two_digits_per_component() {
    assert_eq!(compute_version_number("1.16.2"), 11602);
    assert_eq!(compute_version_number("0.1.0"), 100);
    assert_eq!(compute_version_number("12.34.56"), 123456);
}

#[test]
fn compute_version_number_counts_an_unparseable_component_as_zero() {
    assert_eq!(compute_version_number("1.x.2"), 10002);
    assert_eq!(compute_version_number("dev"), 0);
    assert_eq!(compute_version_number(""), 0);
}

#[test]
fn da1_identifies_a_vt220_with_ansi_color() {
    assert_eq!(collect_device_replies_for(b"\x1b[c"), b"\x1b[?62;22c");
}

#[test]
fn da1_with_an_explicit_zero_parameter_replies() {
    assert_eq!(collect_device_replies_for(b"\x1b[0c"), b"\x1b[?62;22c");
}

#[test]
fn da1_with_a_nonzero_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[1c"), b"");
}

/// The reply carries whatever version this build is, so the expected reply bytes are
/// built from the same packing. What that packing produces is pinned
/// independently by the `compute_version_number_*` tests below.
#[test]
fn da2_reports_type_version_and_zero_cartridge() {
    let expected_device_reply_text = format!(
        "\x1b[>1;{};0c",
        compute_version_number(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        collect_device_replies_for(b"\x1b[>c"),
        expected_device_reply_text.as_bytes()
    );
}

#[test]
fn compute_version_number_cuts_a_prerelease_or_build_suffix() {
    // A prerelease packs as the version it precedes, keeping its patch digit:
    // `0.2.0-pr.1` is 0.2.0, not a fourth component.
    assert_eq!(compute_version_number("0.2.0-pr.1"), 200);
    assert_eq!(compute_version_number("0.2.0-rc.10"), 200);
    assert_eq!(compute_version_number("0.2.0-nightly.20260806"), 200);
    // A suffix carrying no dot, and one on a nonzero patch.
    assert_eq!(compute_version_number("0.1.0-alpha"), 100);
    assert_eq!(compute_version_number("0.1.2-alpha"), 102);
    // A prerelease may itself hold `-`; the first one still starts the suffix.
    assert_eq!(compute_version_number("0.2.0-pre-release.1"), 200);
    // Build metadata, with and without a prerelease before it.
    assert_eq!(compute_version_number("1.16.2+build.7"), 11602);
    assert_eq!(compute_version_number("1.16.2-rc.1+build.7"), 11602);
}

#[test]
fn da2_with_a_nonzero_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[>1c"), b"");
}

#[test]
fn da3_reports_an_all_zero_unit_id() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[=c"),
        b"\x1bP!|00000000\x1b\\"
    );
    assert_eq!(
        collect_device_replies_for(b"\x1b[=0c"),
        b"\x1bP!|00000000\x1b\\"
    );
}

#[test]
fn da3_with_a_nonzero_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[=1c"), b"");
}

#[test]
fn dsr_5_reports_operating_status_ok() {
    assert_eq!(collect_device_replies_for(b"\x1b[5n"), b"\x1b[0n");
}

#[test]
fn dsr_6_reports_the_home_position_one_based() {
    assert_eq!(collect_device_replies_for(b"\x1b[6n"), b"\x1b[1;1R");
}

#[test]
fn dsr_6_reports_the_cursor_after_motion() {
    // CUP to row 3, column 5 (1-based), then query.
    assert_eq!(
        collect_device_replies_for(b"\x1b[3;5H\x1b[6n"),
        b"\x1b[3;5R"
    );
}

#[test]
fn dsr_6_reports_the_alternate_screens_cursor_while_active() {
    let mut terminal_state = build_terminal_state(8, 4);
    // Move on the primary, enter the alternate (fresh cursor seeded from the
    // primary's position), then move on the alternate and query.
    advance_vte_parser(&mut terminal_state, b"\x1b[3;5H\x1b[?1049h\x1b[2;2H\x1b[6n");
    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[2;2R");
}

#[test]
fn dsr_with_an_unknown_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[7n"), b"");
}

#[test]
fn dsr_with_no_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[n"), b"");
}

#[test]
fn decxcpr_reports_the_cursor_in_the_dec_form_without_a_page() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[3;5H\x1b[?6n"),
        b"\x1b[?3;5R"
    );
}

#[test]
fn dec_dsr_reports_no_printer() {
    assert_eq!(collect_device_replies_for(b"\x1b[?15n"), b"\x1b[?13n");
}

#[test]
fn dec_dsr_reports_udks_locked() {
    assert_eq!(collect_device_replies_for(b"\x1b[?25n"), b"\x1b[?21n");
}

#[test]
fn dec_dsr_reports_the_keyboard_ready() {
    assert_eq!(collect_device_replies_for(b"\x1b[?26n"), b"\x1b[?27;1;0;0n");
}

#[test]
fn dec_dsr_reports_no_locator_on_both_status_forms() {
    assert_eq!(collect_device_replies_for(b"\x1b[?53n"), b"\x1b[?53n");
    assert_eq!(collect_device_replies_for(b"\x1b[?55n"), b"\x1b[?53n");
}

#[test]
fn dec_dsr_reports_an_unidentifiable_locator_type() {
    assert_eq!(collect_device_replies_for(b"\x1b[?56n"), b"\x1b[?57;0n");
}

#[test]
fn dec_dsr_reports_zero_macro_space() {
    assert_eq!(collect_device_replies_for(b"\x1b[?62n"), b"\x1b[0*{");
}

#[test]
fn dec_dsr_reports_a_zero_memory_checksum_echoing_the_request_id() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[?63n"),
        b"\x1bP0!~0000\x1b\\"
    );
    assert_eq!(
        collect_device_replies_for(b"\x1b[?63;7n"),
        b"\x1bP7!~0000\x1b\\"
    );
}

#[test]
fn dec_dsr_reports_data_integrity_ok() {
    assert_eq!(collect_device_replies_for(b"\x1b[?75n"), b"\x1b[?70n");
}

#[test]
fn dec_dsr_reports_no_multi_session_support() {
    assert_eq!(collect_device_replies_for(b"\x1b[?85n"), b"\x1b[?83n");
}

#[test]
fn dec_dsr_with_an_unknown_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[?5n"), b"");
    assert_eq!(collect_device_replies_for(b"\x1b[?99n"), b"");
}

#[test]
fn decrqm_reports_a_default_reset_mode_as_reset() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[?2004$p"),
        b"\x1b[?2004;2$y"
    );
}

#[test]
fn decrqm_reports_a_set_mode_as_set() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[?2004h\x1b[?2004$p"),
        b"\x1b[?2004;1$y"
    );
}

#[test]
fn decrqm_reports_default_on_autowrap_as_set_then_reset_after_disable() {
    assert_eq!(collect_device_replies_for(b"\x1b[?7$p"), b"\x1b[?7;1$y");
    assert_eq!(
        collect_device_replies_for(b"\x1b[?7l\x1b[?7$p"),
        b"\x1b[?7;2$y"
    );
}

#[test]
fn decrqm_reports_cursor_visibility_per_active_screen() {
    assert_eq!(collect_device_replies_for(b"\x1b[?25$p"), b"\x1b[?25;1$y");
    assert_eq!(
        collect_device_replies_for(b"\x1b[?25l\x1b[?25$p"),
        b"\x1b[?25;2$y"
    );

    // Visibility is tracked per screen: hiding on the alternate reports
    // hidden there, and the untouched primary reports visible again on exit.
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(
        &mut terminal_state,
        b"\x1b[?1049h\x1b[?25l\x1b[?25$p\x1b[?1049l\x1b[?25$p",
    );
    assert_eq!(
        terminal_state.take_device_query_replies(),
        b"\x1b[?25;2$y\x1b[?25;1$y"
    );
}

#[test]
fn decrqm_reports_every_alt_screen_mode_from_the_active_screen() {
    for mode_number_text in ["47", "1047", "1049"] {
        let mode_query_sequence = format!("\x1b[?{mode_number_text}$p");
        let primary_reply_bytes = collect_device_replies_for(mode_query_sequence.as_bytes());
        assert_eq!(
            primary_reply_bytes,
            format!("\x1b[?{mode_number_text};2$y").as_bytes()
        );

        let alternate_mode_query_sequence = format!("\x1b[?1049h\x1b[?{mode_number_text}$p");
        let alternate_reply_bytes =
            collect_device_replies_for(alternate_mode_query_sequence.as_bytes());
        assert_eq!(
            alternate_reply_bytes,
            format!("\x1b[?{mode_number_text};1$y").as_bytes()
        );
    }
}

#[test]
fn decrqm_reports_the_active_mouse_tracking_level_and_only_it() {
    let levels = ["9", "1000", "1002", "1003"];
    // Enable each level in turn and query all four: only the active one is
    // set.
    for active_mouse_tracking_level_text in levels {
        let mut terminal_state = build_terminal_state(8, 4);
        let mut input_sequence = format!("\x1b[?{active_mouse_tracking_level_text}h");
        let mut expected_device_reply_text = String::new();
        for mouse_tracking_level_text in levels {
            input_sequence.push_str(&format!("\x1b[?{mouse_tracking_level_text}$p"));
            let mode_state_value = if mouse_tracking_level_text == active_mouse_tracking_level_text
            {
                1
            } else {
                2
            };
            expected_device_reply_text.push_str(&format!(
                "\x1b[?{mouse_tracking_level_text};{mode_state_value}$y"
            ));
        }
        advance_vte_parser(&mut terminal_state, input_sequence.as_bytes());
        assert_eq!(
            terminal_state.take_device_query_replies(),
            expected_device_reply_text.as_bytes()
        );
    }
}

#[test]
fn decrqm_reports_the_active_mouse_encoding_and_only_it() {
    let encodings = ["1005", "1006", "1015"];
    for active_mouse_encoding_text in encodings {
        let mut terminal_state = build_terminal_state(8, 4);
        let mut input_sequence = format!("\x1b[?{active_mouse_encoding_text}h");
        let mut expected_device_reply_text = String::new();
        for mouse_encoding_text in encodings {
            input_sequence.push_str(&format!("\x1b[?{mouse_encoding_text}$p"));
            let mode_state_value = if mouse_encoding_text == active_mouse_encoding_text {
                1
            } else {
                2
            };
            expected_device_reply_text
                .push_str(&format!("\x1b[?{mouse_encoding_text};{mode_state_value}$y"));
        }
        advance_vte_parser(&mut terminal_state, input_sequence.as_bytes());
        assert_eq!(
            terminal_state.take_device_query_replies(),
            expected_device_reply_text.as_bytes()
        );
    }
}

#[test]
fn decrqm_reports_the_remaining_stored_flags() {
    // ?1 DECCKM, ?5 DECSCNM, ?12 cursor blink, ?1007 alt scroll: default
    // reset, set after their DECSET.
    assert_eq!(collect_device_replies_for(b"\x1b[?1$p"), b"\x1b[?1;2$y");
    assert_eq!(
        collect_device_replies_for(b"\x1b[?1h\x1b[?1$p"),
        b"\x1b[?1;1$y"
    );
    assert_eq!(collect_device_replies_for(b"\x1b[?5$p"), b"\x1b[?5;2$y");
    assert_eq!(
        collect_device_replies_for(b"\x1b[?5h\x1b[?5$p"),
        b"\x1b[?5;1$y"
    );
    assert_eq!(collect_device_replies_for(b"\x1b[?12$p"), b"\x1b[?12;2$y");
    assert_eq!(
        collect_device_replies_for(b"\x1b[?12h\x1b[?12$p"),
        b"\x1b[?12;1$y"
    );
    assert_eq!(
        collect_device_replies_for(b"\x1b[?1007$p"),
        b"\x1b[?1007;2$y"
    );
    assert_eq!(
        collect_device_replies_for(b"\x1b[?1007h\x1b[?1007$p"),
        b"\x1b[?1007;1$y"
    );
}

#[test]
fn decrqm_reports_an_unstored_mode_as_not_recognized() {
    // ?2/?3/?8 are traced no-ops, ?1048 keeps no queryable mode state, ?9999 is
    // unknown: all report 0.
    for mode_number_text in ["2", "3", "8", "1048", "9999"] {
        let mode_query_sequence = format!("\x1b[?{mode_number_text}$p");
        let expected_device_reply_text = format!("\x1b[?{mode_number_text};0$y");
        assert_eq!(
            collect_device_replies_for(mode_query_sequence.as_bytes()),
            expected_device_reply_text.as_bytes()
        );
    }
}

#[test]
fn ansi_rqm_reports_every_mode_as_not_recognized() {
    assert_eq!(collect_device_replies_for(b"\x1b[4$p"), b"\x1b[4;0$y");
    assert_eq!(collect_device_replies_for(b"\x1b[20$p"), b"\x1b[20;0$y");
}

#[test]
fn replies_accumulate_in_query_order() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[5n\x1b[c"),
        b"\x1b[0n\x1b[?62;22c"
    );
}

#[test]
fn take_device_query_replies_drains_the_queue() {
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(&mut terminal_state, b"\x1b[5n");
    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[0n");
    assert_eq!(terminal_state.take_device_query_replies(), b"");
}

#[test]
fn a_query_flagged_ignore_by_the_parser_gets_no_reply() {
    // 40 parameters overflow vte's parameter list, so the sequence arrives
    // with `ignore` set and is dropped before dispatch.
    let mut ignored_query_sequence = String::from("\x1b[");
    ignored_query_sequence.push_str(&"5;".repeat(40));
    ignored_query_sequence.push('n');
    assert_eq!(
        collect_device_replies_for(ignored_query_sequence.as_bytes()),
        b""
    );
}

#[test]
fn plain_output_produces_no_replies() {
    assert_eq!(
        collect_device_replies_for(b"hello \x1b[31mworld\x1b[0m\r\n"),
        b""
    );
}

#[test]
fn compute_version_number_packs_the_two_digit_boundary_and_saturates_past_u32() {
    assert_eq!(compute_version_number("99.99.99"), 999_999);
    assert_eq!(compute_version_number("4294967295.0.0"), u32::MAX);
}

#[test]
fn compute_version_number_with_a_leading_suffix_marker_packs_to_zero() {
    assert_eq!(compute_version_number("-1.2.3"), 0);
    assert_eq!(compute_version_number("+1.2.3"), 0);
}

#[test]
fn da1_reads_the_first_primary_parameter_past_a_subparameter() {
    // `CSI 0:1 c` — the primary value is 0, the `:1` subparameter is ignored.
    assert_eq!(collect_device_replies_for(b"\x1b[0:1c"), b"\x1b[?62;22c");
}

#[test]
fn da2_with_an_explicit_zero_parameter_replies() {
    let expected_device_reply_text = format!(
        "\x1b[>1;{};0c",
        compute_version_number(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        collect_device_replies_for(b"\x1b[>0c"),
        expected_device_reply_text.as_bytes()
    );
}

#[test]
fn dsr_6_reports_the_parked_column_while_a_wrap_is_pending() {
    // Eight glyphs fill the 8-column row: the cursor parks on column 8 with
    // the wrap latch armed, and CPR reports that column.
    assert_eq!(collect_device_replies_for(b"abcdefgh\x1b[6n"), b"\x1b[1;8R");
}

#[test]
fn dsr_6_reports_the_last_row_after_the_screen_scrolls() {
    // Five line feeds on a 4-row screen: the cursor stays on row 4.
    assert_eq!(
        collect_device_replies_for(b"\n\n\n\n\n\x1b[6n"),
        b"\x1b[4;1R"
    );
}

#[test]
fn decxcpr_reports_the_alternate_screens_cursor_while_active() {
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(
        &mut terminal_state,
        b"\x1b[3;5H\x1b[?1049h\x1b[2;2H\x1b[?6n",
    );
    assert_eq!(terminal_state.take_device_query_replies(), b"\x1b[?2;2R");
}

#[test]
fn dec_dsr_with_no_parameter_gets_no_reply() {
    assert_eq!(collect_device_replies_for(b"\x1b[?n"), b"");
}

#[test]
fn dec_dsr_63_clamps_the_request_id_to_u16() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[?63;65535n"),
        b"\x1bP65535!~0000\x1b\\"
    );
    // The parser saturates an oversized parameter at 65535.
    assert_eq!(
        collect_device_replies_for(b"\x1b[?63;70000n"),
        b"\x1bP65535!~0000\x1b\\"
    );
}

#[test]
fn decrqm_with_no_parameter_reports_mode_zero_as_not_recognized() {
    assert_eq!(collect_device_replies_for(b"\x1b[?$p"), b"\x1b[?0;0$y");
}

#[test]
fn ansi_rqm_with_no_parameter_reports_mode_zero_as_not_recognized() {
    assert_eq!(collect_device_replies_for(b"\x1b[$p"), b"\x1b[0;0$y");
}

#[test]
fn decrqm_reports_1048_as_not_recognized_even_after_a_save() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[?1048h\x1b[?1048$p"),
        b"\x1b[?1048;0$y"
    );
}

#[test]
fn decrqm_reports_a_mode_from_the_first_parameter_only() {
    // `?2004;7 $p` asks about 2004; the trailing 7 is ignored.
    assert_eq!(
        collect_device_replies_for(b"\x1b[?2004;7$p"),
        b"\x1b[?2004;2$y"
    );
}

#[test]
fn decrqm_reports_every_mouse_level_reset_after_tracking_is_turned_off() {
    assert_eq!(
        collect_device_replies_for(
            b"\x1b[?1003h\x1b[?1003l\x1b[?9$p\x1b[?1000$p\x1b[?1002$p\x1b[?1003$p"
        ),
        b"\x1b[?9;2$y\x1b[?1000;2$y\x1b[?1002;2$y\x1b[?1003;2$y"
    );
}

#[test]
fn decrqm_reports_every_encoding_reset_after_the_active_one_is_turned_off() {
    assert_eq!(
        collect_device_replies_for(b"\x1b[?1006h\x1b[?1006l\x1b[?1005$p\x1b[?1006$p\x1b[?1015$p"),
        b"\x1b[?1005;2$y\x1b[?1006;2$y\x1b[?1015;2$y"
    );
}

#[test]
fn replies_queue_across_a_screen_switch_in_order() {
    // One device-global queue: a query on the alternate screen and one after
    // the exit drain together, in query order.
    let mut terminal_state = build_terminal_state(8, 4);
    advance_vte_parser(&mut terminal_state, b"\x1b[?1049h\x1b[5n\x1b[?1049l\x1b[c");
    assert_eq!(
        terminal_state.take_device_query_replies(),
        b"\x1b[0n\x1b[?62;22c"
    );
}

#[test]
fn a_hard_reset_keeps_queued_replies() {
    assert_eq!(collect_device_replies_for(b"\x1b[5n\x1bc"), b"\x1b[0n");
}
