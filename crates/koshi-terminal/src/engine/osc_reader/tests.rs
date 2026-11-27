//! Tests for `OscReader`'s OSC texts: every field kept, the command numbers
//! it reads, the endings at which the VTE parser dispatches, the bytes the
//! VTE parser skips, the size limit, chunk splits, and a check that the
//! reader finds every text command the VTE parser dispatches, in the same
//! order and with the same text.

use super::*;
use crate::engine::tests::build_clipboard_copy;

/// The texts `terminal_bytes` finish, read in one chunk by a reader that keeps
/// no copy.
fn read_osc_texts(terminal_bytes: &[u8]) -> Vec<OscText> {
    let mut osc_reader = OscReader::default();
    osc_reader.process_terminal_bytes(terminal_bytes);
    osc_reader.take_osc_texts()
}

/// The text of `osc_text_command` with the argument bytes `argument_bytes`.
fn build_osc_text(osc_text_command: OscTextCommand, argument_bytes: &[u8]) -> OscText {
    OscText {
        osc_text_command,
        argument_bytes: Some(argument_bytes.to_vec()),
    }
}

#[test]
fn a_text_keeps_every_field_after_its_first_separator() {
    let field_letters: Vec<String> = ('a'..='t').map(String::from).collect();
    let joined_fields = field_letters.join(";");
    assert_eq!(
        read_osc_texts(format!("\x1b]2;{joined_fields}\x07").as_bytes()),
        vec![build_osc_text(
            OscTextCommand::Title,
            joined_fields.as_bytes()
        )]
    );
}

#[test]
fn each_text_command_number_gives_its_command() {
    for (command_number, osc_text_command) in [
        ("0", OscTextCommand::Title),
        ("1", OscTextCommand::Title),
        ("2", OscTextCommand::Title),
        ("7", OscTextCommand::WorkingDirectory),
        ("9", OscTextCommand::Osc9Notification),
        ("777", OscTextCommand::Osc777Notification),
        ("99", OscTextCommand::Osc99Notification),
    ] {
        assert_eq!(
            read_osc_texts(format!("\x1b]{command_number};a;b\x07").as_bytes()),
            vec![build_osc_text(osc_text_command, b"a;b")],
            "command {command_number}"
        );
    }
}

#[test]
fn every_other_command_number_gives_no_text() {
    for command_number in ["", "3", "8", "52", "90", "133", "09", "7777", "9a", "9\x7f"] {
        assert_eq!(
            read_osc_texts(format!("\x1b]{command_number};a\x07").as_bytes()),
            Vec::new(),
            "command {command_number:?}"
        );
    }
}

#[test]
fn every_ending_the_vte_parser_dispatches_at_gives_the_text() {
    for ending_bytes in [&b"\x07"[..], b"\x1b\\", b"\x18", b"\x1a", b"\x1b[0m"] {
        let mut sequence_bytes = b"\x1b]9;hi".to_vec();
        sequence_bytes.extend_from_slice(ending_bytes);
        assert_eq!(
            read_osc_texts(&sequence_bytes),
            vec![build_osc_text(OscTextCommand::Osc9Notification, b"hi")],
            "ending {ending_bytes:?}"
        );
    }
}

#[test]
fn a_text_ended_by_an_escape_lets_that_escape_open_the_next_text() {
    assert_eq!(
        read_osc_texts(b"\x1b]9;first\x1b]9;second\x07"),
        vec![
            build_osc_text(OscTextCommand::Osc9Notification, b"first"),
            build_osc_text(OscTextCommand::Osc9Notification, b"second"),
        ]
    );
}

#[test]
fn a_text_command_with_no_separator_gives_no_argument_bytes() {
    assert_eq!(
        read_osc_texts(b"\x1b]9\x07\x1b]777\x1b\\"),
        vec![
            OscText {
                osc_text_command: OscTextCommand::Osc9Notification,
                argument_bytes: None,
            },
            OscText {
                osc_text_command: OscTextCommand::Osc777Notification,
                argument_bytes: None,
            },
        ]
    );
}

#[test]
fn the_control_bytes_the_vte_parser_skips_are_skipped_inside_a_text() {
    assert_eq!(
        read_osc_texts(b"\x1b]9\x01;a\r\nb\x1f\x7fc\xc3\xa9\x07"),
        vec![build_osc_text(
            OscTextCommand::Osc9Notification,
            b"ab\x7fc\xc3\xa9"
        )]
    );
}

#[test]
fn a_text_split_at_every_byte_gives_one_text() {
    let sequence_bytes = b"\x1b]777;notify;Build;done\x1b\\";
    for split_index in 0..=sequence_bytes.len() {
        let mut osc_reader = OscReader::default();
        osc_reader.process_terminal_bytes(&sequence_bytes[..split_index]);
        osc_reader.process_terminal_bytes(&sequence_bytes[split_index..]);
        assert_eq!(
            osc_reader.take_osc_texts(),
            vec![build_osc_text(
                OscTextCommand::Osc777Notification,
                b"notify;Build;done"
            )],
            "split at byte {split_index}"
        );
    }
}

#[test]
fn a_text_keeps_at_most_the_osc_buffer_capacity_of_argument_bytes() {
    let mut sequence_bytes = b"\x1b]9;".to_vec();
    sequence_bytes.extend(std::iter::repeat_n(b'x', OSC_BUFFER_BYTE_CAPACITY + 100));
    sequence_bytes.push(0x07);
    let mut osc_reader = OscReader::default();
    osc_reader.process_terminal_bytes(&sequence_bytes[..OSC_BUFFER_BYTE_CAPACITY / 2]);
    osc_reader.process_terminal_bytes(&sequence_bytes[OSC_BUFFER_BYTE_CAPACITY / 2..]);
    assert_eq!(
        osc_reader.take_osc_texts(),
        vec![build_osc_text(
            OscTextCommand::Osc9Notification,
            &vec![b'x'; OSC_BUFFER_BYTE_CAPACITY]
        )]
    );
}

#[test]
fn copies_and_texts_in_one_chunk_come_back_in_their_own_lists() {
    let mut osc_reader = OscReader::with_maximum_copy_byte_count(1024);
    osc_reader.process_terminal_bytes(b"\x1b]2;title\x07\x1b]52;c;aGk=\x07\x1b]9;done\x1b\\");
    assert_eq!(
        osc_reader.take_osc_texts(),
        vec![
            build_osc_text(OscTextCommand::Title, b"title"),
            build_osc_text(OscTextCommand::Osc9Notification, b"done"),
        ]
    );
    assert_eq!(
        osc_reader.take_clipboard_copies(),
        vec![build_clipboard_copy("c", b"hi")]
    );
    assert_eq!(osc_reader.take_osc_texts(), Vec::new());
}

#[test]
fn a_reader_that_keeps_no_copy_skips_a_copy() {
    let mut osc_reader = OscReader::default();
    osc_reader.process_terminal_bytes(b"\x1b]52;c;aGk=\x07\x1b]9;after\x07");
    assert_eq!(osc_reader.take_clipboard_copies(), Vec::new());
    assert_eq!(
        osc_reader.take_osc_texts(),
        vec![build_osc_text(OscTextCommand::Osc9Notification, b"after")]
    );
}

/// One OSC dispatch of a text command as the VTE parser hands it over.
#[derive(Debug)]
struct DispatchedOscText {
    osc_text_command: OscTextCommand,
    osc_parameters: Vec<Vec<u8>>,
}

/// A [`vte::Perform`] that records each OSC dispatch whose command is an
/// [`OscTextCommand`].
#[derive(Default)]
struct OscTextDispatchRecorder {
    dispatched_osc_texts: Vec<DispatchedOscText>,
}

impl vte::Perform for OscTextDispatchRecorder {
    fn osc_dispatch(&mut self, osc_parameters: &[&[u8]], _is_bell_terminated: bool) {
        let Some(osc_text_command) = osc_parameters.first().and_then(|command_number_bytes| {
            OscTextCommand::from_command_number(command_number_bytes)
        }) else {
            return;
        };
        self.dispatched_osc_texts.push(DispatchedOscText {
            osc_text_command,
            osc_parameters: osc_parameters
                .iter()
                .map(|osc_parameter| osc_parameter.to_vec())
                .collect(),
        });
    }
}

/// The bytes the agreement check draws from: every byte that opens, ends or
/// changes an escape sequence or an OSC, command number digits, the field
/// separator, and plain, UTF-8 and C1 bytes.
const AGREEMENT_ALPHABET_BYTES: [u8; 27] = [
    0x1b, b']', b'0', b'2', b'5', b'7', b'9', b';', 0x07, 0x18, 0x1a, b'\\', 0x01, b'\n', 0x7f,
    0xc3, 0xa9, b'a', b'[', b'P', b'X', b'^', b'_', b' ', 0x9c, 0x9d, b'=',
];

/// The next value of the xorshift sequence after `random_state`.
fn advance_xorshift(random_state: &mut u64) -> u64 {
    *random_state ^= *random_state << 13;
    *random_state ^= *random_state >> 7;
    *random_state ^= *random_state << 17;
    *random_state
}

#[test]
fn the_reader_finds_every_text_command_the_vte_parser_dispatches() {
    let mut random_state: u64 = 0x5eed_1234_abcd_0001;
    for case_index in 0..5000 {
        let byte_count = 1 + (advance_xorshift(&mut random_state) % 96) as usize;
        let terminal_bytes: Vec<u8> = (0..byte_count)
            .map(|_| {
                let alphabet_index = (advance_xorshift(&mut random_state)
                    % AGREEMENT_ALPHABET_BYTES.len() as u64)
                    as usize;
                AGREEMENT_ALPHABET_BYTES[alphabet_index]
            })
            .collect();
        let split_index = (advance_xorshift(&mut random_state) % (byte_count as u64 + 1)) as usize;
        let mut osc_reader = if case_index % 2 == 0 {
            OscReader::default()
        } else {
            OscReader::with_maximum_copy_byte_count(16)
        };
        osc_reader.process_terminal_bytes(&terminal_bytes[..split_index]);
        osc_reader.process_terminal_bytes(&terminal_bytes[split_index..]);
        let osc_texts = osc_reader.take_osc_texts();
        let mut vte_parser = vte::Parser::<OSC_BUFFER_BYTE_CAPACITY>::new_with_size();
        let mut dispatch_recorder = OscTextDispatchRecorder::default();
        vte_parser.advance(&mut dispatch_recorder, &terminal_bytes[..split_index]);
        vte_parser.advance(&mut dispatch_recorder, &terminal_bytes[split_index..]);

        assert_eq!(
            osc_texts.len(),
            dispatch_recorder.dispatched_osc_texts.len(),
            "case {case_index}: {terminal_bytes:?}"
        );
        for (osc_text, dispatched_osc_text) in osc_texts
            .iter()
            .zip(&dispatch_recorder.dispatched_osc_texts)
        {
            assert_eq!(
                osc_text.osc_text_command, dispatched_osc_text.osc_text_command,
                "case {case_index}: {terminal_bytes:?}"
            );
            let dispatched_argument_bytes = match dispatched_osc_text.osc_parameters.as_slice() {
                [_] => None,
                [_, argument_fields @ ..] => Some(argument_fields.join(&b';')),
                [] => unreachable!("the VTE parser dispatches the command number"),
            };
            if dispatched_osc_text.osc_parameters.len() < 16 {
                assert_eq!(
                    osc_text.argument_bytes, dispatched_argument_bytes,
                    "case {case_index}: {terminal_bytes:?}"
                );
            } else {
                let argument_bytes = osc_text.argument_bytes.as_deref().unwrap_or_default();
                let dispatched_argument_bytes = dispatched_argument_bytes.unwrap_or_default();
                assert!(
                    argument_bytes.starts_with(&dispatched_argument_bytes),
                    "case {case_index}: {terminal_bytes:?}"
                );
            }
        }
    }
}
