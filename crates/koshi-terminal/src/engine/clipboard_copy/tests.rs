//! Tests for `ClipboardCopyParser`: the two endings, the payload rules, the
//! targets filter, the size limits, the bytes the VTE parser skips, and the
//! sequences that end a copy before it finishes.

use super::*;
use crate::engine::tests::build_clipboard_copy;

/// The most decoded bytes the tests allow per copy, unless a test names its own.
const TEST_MAXIMUM_COPY_BYTE_COUNT: usize = 1024;

/// The copies `terminal_bytes` finish, read in one chunk by a parser that keeps
/// copies of at most `maximum_copy_byte_count` decoded bytes.
fn read_clipboard_copies(
    maximum_copy_byte_count: usize,
    terminal_bytes: &[u8],
) -> Vec<ClipboardCopy> {
    let mut clipboard_copy_parser =
        ClipboardCopyParser::with_maximum_copy_byte_count(maximum_copy_byte_count);
    clipboard_copy_parser.process_terminal_bytes(terminal_bytes);
    clipboard_copy_parser.take_clipboard_copies()
}

#[test]
fn a_copy_ended_by_bel_decodes_its_payload() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;aGk=\x07"),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn a_copy_ended_by_the_string_terminator_decodes_its_payload() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;aGk=\x1b\\"),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn a_copy_split_at_every_byte_gives_one_copy() {
    let copy_bytes = b"\x1b]52;pc;aGVsbG8=\x1b\\";
    for split_index in 0..=copy_bytes.len() {
        let mut clipboard_copy_parser =
            ClipboardCopyParser::with_maximum_copy_byte_count(TEST_MAXIMUM_COPY_BYTE_COUNT);
        clipboard_copy_parser.process_terminal_bytes(&copy_bytes[..split_index]);
        clipboard_copy_parser.process_terminal_bytes(&copy_bytes[split_index..]);
        assert_eq!(
            clipboard_copy_parser.take_clipboard_copies(),
            vec![build_clipboard_copy("pc", b"hello")],
            "split at byte {split_index}"
        );
    }
}

#[test]
fn a_copy_read_one_byte_at_a_time_gives_one_copy() {
    let mut clipboard_copy_parser =
        ClipboardCopyParser::with_maximum_copy_byte_count(TEST_MAXIMUM_COPY_BYTE_COUNT);
    for copy_byte in b"\x1b]52;c;aGVsbG8=\x07" {
        clipboard_copy_parser.process_terminal_bytes(std::slice::from_ref(copy_byte));
    }
    assert_eq!(
        clipboard_copy_parser.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hello")]
    );
}

#[test]
fn a_payload_without_padding_decodes() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;aGk\x07"),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn a_payload_that_decodes_to_bytes_that_are_not_utf8_keeps_those_bytes() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;/w==\x07"),
        vec![build_clipboard_copy("c", &[0xff])]
    );
}

#[test]
fn a_read_request_makes_no_copy() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;?\x07"),
        Vec::new()
    );
}

#[test]
fn an_empty_payload_makes_an_empty_copy() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;\x07"),
        vec![build_clipboard_copy("c", b"")]
    );
}

#[test]
fn a_payload_that_is_not_base64_makes_an_empty_copy() {
    for payload_bytes in [&b"!!!!"[..], b"aGk=;", b"a Gk=", b"aGk=\xc3\xa9", b"?\x7f"] {
        let mut copy_bytes = b"\x1b]52;p;".to_vec();
        copy_bytes.extend_from_slice(payload_bytes);
        copy_bytes.push(0x07);
        assert_eq!(
            read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, &copy_bytes),
            vec![build_clipboard_copy("p", b"")],
            "payload {payload_bytes:?}"
        );
    }
}

#[test]
fn the_targets_keep_each_target_byte_once_in_order() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;pXcp7 8;aGk=\x07"),
        vec![build_clipboard_copy("pc7", b"hi")]
    );
}

#[test]
fn an_empty_targets_field_stays_empty() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;;aGk=\x07"),
        vec![build_clipboard_copy("", b"hi")]
    );
}

#[test]
fn a_command_with_no_second_separator_makes_no_copy() {
    for copy_bytes in [&b"\x1b]52;c\x07"[..], b"\x1b]52;c\x1b\\", b"\x1b]52\x07"] {
        assert_eq!(
            read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, copy_bytes),
            Vec::new(),
            "sequence {copy_bytes:?}"
        );
    }
}

#[test]
fn a_command_number_other_than_52_makes_no_copy() {
    for copy_bytes in [
        &b"\x1b]5;c;aGk=\x07"[..],
        b"\x1b]520;c;aGk=\x07",
        b"\x1b]052;c;aGk=\x07",
        b"\x1b]53;c;aGk=\x07",
        b"\x1b]2;title\x07",
        b"\x1b];c;aGk=\x07",
    ] {
        assert_eq!(
            read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, copy_bytes),
            Vec::new(),
            "sequence {copy_bytes:?}"
        );
    }
}

#[test]
fn a_copy_after_another_command_is_read() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b]2;title\x07text\x1b]7;file://host/home/user\x1b\\\x1b]52;c;aGk=\x07"
        ),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn two_copies_in_one_chunk_come_back_in_order() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b]52;c;aGk=\x07plain text\x1b]52;p;aGVsbG8=\x1b\\"
        ),
        vec![
            build_clipboard_copy("c", b"hi"),
            build_clipboard_copy("p", b"hello"),
        ]
    );
}

#[test]
fn taking_the_copies_leaves_none() {
    let mut clipboard_copy_parser =
        ClipboardCopyParser::with_maximum_copy_byte_count(TEST_MAXIMUM_COPY_BYTE_COUNT);
    clipboard_copy_parser.process_terminal_bytes(b"\x1b]52;c;aGk=\x07");
    assert_eq!(
        clipboard_copy_parser.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hi")]
    );
    assert_eq!(clipboard_copy_parser.take_clipboard_copies(), Vec::new());
}

#[test]
fn cancel_or_substitute_in_any_part_of_a_copy_makes_no_copy() {
    for copy_bytes in [
        &b"\x1b]5\x182;c;aGk=\x07"[..],
        b"\x1b]52;c\x18;aGk=\x07",
        b"\x1b]52;c;aG\x18k=\x07",
        b"\x1b]52;c;aGk=\x1a\x07",
        b"\x1b]52;c;aGk=\x1b\x18\\",
    ] {
        assert_eq!(
            read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, copy_bytes),
            Vec::new(),
            "sequence {copy_bytes:?}"
        );
    }
}

#[test]
fn a_cancelled_copy_leaves_the_next_copy_readable() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b]52;c;aGk=\x18\x1b]52;p;aGVsbG8=\x07"
        ),
        vec![build_clipboard_copy("p", b"hello")]
    );
}

#[test]
fn an_escape_not_followed_by_the_terminator_makes_no_copy_and_starts_the_next_sequence() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b]52;c;aGk=\x1b]52;p;aGVsbG8=\x07\x1b]52;q;aGk=\x1b[0m"
        ),
        vec![build_clipboard_copy("p", b"hello")]
    );
}

#[test]
fn the_control_bytes_the_vte_parser_skips_are_skipped_inside_a_copy() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b]5\x002;c\x01;aGVs\r\nbG8=\x1f\x07"
        ),
        vec![build_clipboard_copy("c", b"hello")]
    );
}

#[test]
fn control_bytes_between_the_escape_and_the_backslash_keep_the_terminator() {
    assert_eq!(
        read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, b"\x1b]52;c;aGk=\x1b\n\x07\\"),
        vec![build_clipboard_copy("c", b"hi")]
    );
}

#[test]
fn control_bytes_and_a_second_escape_before_the_bracket_still_open_a_command() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b\x1b]52;c;aGk=\x07\x1b\n]52;p;aGk=\x07"
        ),
        vec![
            build_clipboard_copy("c", b"hi"),
            build_clipboard_copy("p", b"hi"),
        ]
    );
}

#[test]
fn delete_and_bytes_from_0x80_up_after_an_escape_keep_the_escape_open() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"\x1b\x7f]52;c;aGk=\x07\x1b\xc3\xa9]52;p;aGk=\x07\x1b]52;q;aGk=\x1b\x7f]52;s;aGk=\x07"
        ),
        vec![
            build_clipboard_copy("c", b"hi"),
            build_clipboard_copy("p", b"hi"),
            build_clipboard_copy("s", b"hi"),
        ]
    );
}

#[test]
fn an_escape_followed_by_another_sequence_opens_no_copy() {
    for copy_bytes in [
        &b"\x1b[52;c;aGk=\x07"[..],
        b"\x1b ]52;c;aGk=\x07",
        b"\x1bP]52;c;aGk=\x07",
    ] {
        assert_eq!(
            read_clipboard_copies(TEST_MAXIMUM_COPY_BYTE_COUNT, copy_bytes),
            Vec::new(),
            "sequence {copy_bytes:?}"
        );
    }
}

#[test]
fn a_copy_of_exactly_the_largest_size_is_kept() {
    assert_eq!(
        read_clipboard_copies(5, b"\x1b]52;c;aGVsbG8=\x07"),
        vec![build_clipboard_copy("c", b"hello")]
    );
}

#[test]
fn a_copy_whose_decoded_bytes_pass_the_largest_size_makes_no_copy() {
    // `ceil(4 / 3) * 4` is 8 payload bytes: `aGVsbG8=` is held, and decodes to
    // 5 bytes.
    assert_eq!(
        read_clipboard_copies(4, b"\x1b]52;c;aGVsbG8=\x07"),
        Vec::new()
    );
    assert_eq!(read_clipboard_copies(1, b"\x1b]52;c;aGk=\x07"), Vec::new());
}

#[test]
fn a_payload_longer_than_the_held_limit_makes_no_copy_and_the_next_copy_is_read() {
    // `ceil(3 / 3) * 4` is 4 payload bytes, and `aGVsbG8=` is 8.
    assert_eq!(
        read_clipboard_copies(3, b"\x1b]52;c;aGVsbG8=\x07\x1b]52;p;aGk=\x07"),
        vec![build_clipboard_copy("p", b"hi")]
    );
}

#[test]
fn a_payload_that_passes_the_held_limit_across_chunks_makes_no_copy() {
    let mut clipboard_copy_parser = ClipboardCopyParser::with_maximum_copy_byte_count(3);
    clipboard_copy_parser.process_terminal_bytes(b"\x1b]52;c;aGVs");
    clipboard_copy_parser.process_terminal_bytes(b"b");
    clipboard_copy_parser.process_terminal_bytes(b"G8=\x07\x1b]52;p;aGk=\x07");
    assert_eq!(
        clipboard_copy_parser.take_clipboard_copies(),
        vec![build_clipboard_copy("p", b"hi")]
    );
}

#[test]
fn the_largest_size_rounds_the_held_payload_up_to_whole_base64_groups() {
    assert_eq!(
        ClipboardCopyParser::with_maximum_copy_byte_count(4).maximum_payload_byte_count,
        8
    );
    assert_eq!(
        ClipboardCopyParser::with_maximum_copy_byte_count(6).maximum_payload_byte_count,
        8
    );
    assert_eq!(
        ClipboardCopyParser::with_maximum_copy_byte_count(usize::MAX).maximum_payload_byte_count,
        usize::MAX
    );
}

#[test]
fn plain_text_and_other_sequences_make_no_copy() {
    assert_eq!(
        read_clipboard_copies(
            TEST_MAXIMUM_COPY_BYTE_COUNT,
            b"plain text \xc3\xa9\x07\x1b[31mred\x1b[0m\x1bPq#0\x1b\\\x1b_Gf=32;AAAA\x1b\\"
        ),
        Vec::new()
    );
}
