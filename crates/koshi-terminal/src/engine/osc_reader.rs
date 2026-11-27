//! The operating system commands (OSC) whose whole text koshi reads beside
//! the VTE parser: the `OSC 52` clipboard copies, the text of every OSC whose
//! command is an [`OscTextCommand`] (`OSC 0`, `1`, `2`, `7`, `9`, `777` and
//! `99`), and the `OSC 7501` program status sequences.
//!
//! The VTE parser hands its performer at most 16 parameters of one OSC, and
//! no byte after the 16th `;`. [`OscReader`] reads the bytes that the VTE
//! parser reads, before the parser reads them, and keeps:
//!
//! - an [`OscText`] for each finished OSC whose command is an
//!   `OscTextCommand`: every byte after the first `;`, with each `;` after it
//!   kept;
//! - a [`ClipboardCopy`] for each finished `OSC 52` copy, by the rules of the
//!   module `clipboard_copy`, once copies are read;
//! - a [`ProgramStatusSequence`] for each finished `OSC 7501`: every byte
//!   after its first `;`, as the program wrote it.
//!
//! The reader finds the start and the end of a sequence as the VTE parser
//! does:
//!
//! - `ESC` starts an escape sequence in every state, and `ESC ]` opens an
//!   operating system command. The bytes `0x00` to `0x17`, `0x19`, `0x1b` to
//!   `0x1f` and every byte from `0x7f` up keep the escape open: `ESC DEL ]`
//!   opens a command too.
//! - Inside the command, the bytes `0x00` to `0x06`, `0x08` to `0x17`, `0x19`
//!   and `0x1c` to `0x1f` are skipped. A text or a payload that is broken
//!   over lines reads as one.
//! - `BEL` (`0x07`), `CAN` (`0x18`), `SUB` (`0x1a`) and `ESC` end the
//!   command. That `ESC` starts the next sequence.
//!
//! Every one of the four endings gives an `OscText`, at the byte where the
//! VTE parser dispatches the command. A command with no `;` gives
//! `argument_bytes: None`. The reader keeps at most
//! [`OSC_BUFFER_BYTE_CAPACITY`] argument bytes of one command, and drops the
//! rest.
//!
//! A copy finishes only at `BEL` and at the string terminator `ESC \`.
//! `CAN`, `SUB`, and `ESC` followed by a byte other than `\`, make no copy.
//! The bytes the VTE parser runs inside an escape (`0x00` to `0x17`, `0x19`,
//! `0x1c` to `0x1f`) can come between the `ESC` and the `\`.
//!
//! An `OSC 7501` follows the bytes of the Program Status Protocol instead:
//!
//! - It opens only at the 7 bytes `ESC ] 7501 ;` in a row. A byte between
//!   the `ESC` and the `]`, or a byte below `0x20` inside `7501 ;`, makes
//!   the command skipped. An `OSC 7501` with no `;` gives nothing.
//! - Every byte after the `;` is kept, also every byte below `0x20` other
//!   than the endings, up to [`OSC_BUFFER_BYTE_CAPACITY`] bytes.
//! - It finishes only at `BEL` and at the 2 bytes `ESC \` in a row. `CAN`,
//!   `SUB`, and `ESC` followed by any byte other than `\`, end it with no
//!   sequence, and that byte is read as it is read after any other `ESC`.
//!
//! Example: `ESC ] 9 ; a ; b BEL` gives the text `a;b` for
//! `Osc9Notification`, `ESC ] 52 ; c ; aGk= BEL` gives a copy of `hi`
//! for the target `c`, and `ESC ] 7501 ; id=a<TAB>b BEL` gives a program
//! status sequence with the argument bytes `id=a<TAB>b`.

use std::mem;

use koshi_core::clipboard::push_clipboard_target;

use super::clipboard_copy::{ClipboardCopy, ClipboardCopyLimit};
use super::{CANCEL_BYTE, ESCAPE_BYTE, OSC_BUFFER_BYTE_CAPACITY, SUBSTITUTE_BYTE};
use crate::state::{OscText, OscTextCommand};

/// `BEL`, `0x07`: ends an operating system command.
const BELL_BYTE: u8 = 0x07;

/// The byte after `ESC` that opens an operating system command: `]`.
const OPERATING_SYSTEM_COMMAND_BYTE: u8 = b']';

/// The byte after `ESC` that ends a string as the string terminator `ESC \`.
const STRING_TERMINATOR_BYTE: u8 = b'\\';

/// The byte between two fields of an operating system command: `;`.
const FIELD_SEPARATOR_BYTE: u8 = b';';

/// The command number of a clipboard copy, `52`.
const CLIPBOARD_COMMAND_NUMBER_BYTES: &[u8; 2] = b"52";

/// The command number of a program status sequence, `7501`.
const PROGRAM_STATUS_COMMAND_NUMBER_BYTES: &[u8; 4] = b"7501";

/// The 7 bytes that open a program status sequence: `ESC ] 7501 ;`.
pub(crate) const PROGRAM_STATUS_INTRODUCER_BYTES: &[u8; 7] = b"\x1b]7501;";

/// The most command number bytes the reader holds: the length of `7501`. A
/// longer command number names no command the reader reads.
const MAX_READ_COMMAND_NUMBER_BYTE_COUNT: usize = 4;

/// Where [`OscReader`] stands in the bytes it read.
#[derive(Debug, Default)]
enum OscReaderState {
    /// Outside every sequence the reader reads.
    #[default]
    Ground,
    /// After an `ESC` that opened no sequence yet. `has_skipped_byte` is set
    /// once a byte the reader skips came after the `ESC`.
    Escape { has_skipped_byte: bool },
    /// In an operating system command, before its first `;`. The first
    /// `command_number_byte_count` bytes of `command_number_bytes` are the
    /// command bytes read so far. `has_skipped_byte` is set once a byte the
    /// reader skips came between the `ESC` and the `]`, or a byte below
    /// `0x20` came in the command number.
    CommandNumber {
        command_number_bytes: [u8; MAX_READ_COMMAND_NUMBER_BYTE_COUNT],
        command_number_byte_count: usize,
        has_skipped_byte: bool,
    },
    /// In the targets field of a copy.
    ClipboardTargets { clipboard_targets: String },
    /// In the payload of a copy.
    ClipboardPayload {
        clipboard_targets: String,
        payload_bytes: Vec<u8>,
    },
    /// After an `ESC` in the payload of a copy. `has_skipped_byte` is set
    /// once a byte the reader skips came after the `ESC`.
    ClipboardPayloadEscape {
        clipboard_targets: String,
        payload_bytes: Vec<u8>,
        has_skipped_byte: bool,
    },
    /// After the first `;` of a command whose text the reader keeps.
    TextArguments {
        osc_text_command: OscTextCommand,
        argument_bytes: Vec<u8>,
    },
    /// In an operating system command that the reader keeps nothing of, until
    /// its end.
    SkippedCommand,
    /// After `ESC ] 7501 ;`, before the end of the sequence.
    ProgramStatusArguments { argument_bytes: Vec<u8> },
    /// After an `ESC` that follows the argument bytes of a program status
    /// sequence.
    ProgramStatusEscape { argument_bytes: Vec<u8> },
}

/// One finished `OSC 7501`: `ESC ] 7501 ;`, the argument bytes, then `BEL` or
/// `ESC \`.
///
/// `ESC ] 7501 ; state=idle BEL`, read in one chunk, gives the argument bytes
/// `state=idle`, `is_bell_terminated: true` and `end_byte_index: 18`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgramStatusSequence {
    /// Every byte between the `;` and the terminator, cut to
    /// [`OSC_BUFFER_BYTE_CAPACITY`] bytes.
    pub(crate) argument_bytes: Vec<u8>,
    /// Whether the sequence ended with `BEL`. `false` when it ended with
    /// `ESC \`.
    pub(crate) is_bell_terminated: bool,
    /// The index right after the last byte of the sequence, in the bytes of
    /// the [`OscReader::process_terminal_bytes`] call that finished it.
    pub(crate) end_byte_index: usize,
}

/// Reads the `OSC 52` copies and the [`OscText`]s in terminal bytes, one
/// chunk after another. A sequence split across two chunks completes in the
/// second chunk.
#[derive(Debug, Default)]
pub(crate) struct OscReader {
    /// The size limits of the copies the reader keeps, or `None` when it
    /// keeps no copy.
    clipboard_copy_limit: Option<ClipboardCopyLimit>,
    /// Where the reader stands.
    reader_state: OscReaderState,
    /// The finished copies that
    /// [`take_clipboard_copies`](Self::take_clipboard_copies) did not take
    /// yet, in the order their sequences ended.
    clipboard_copies: Vec<ClipboardCopy>,
    /// The finished texts that [`take_osc_texts`](Self::take_osc_texts) did
    /// not take yet, in the order their sequences ended.
    osc_texts: Vec<OscText>,
    /// The finished program status sequences that
    /// [`take_program_status_sequences`](Self::take_program_status_sequences)
    /// did not take yet, in the order they ended.
    program_status_sequences: Vec<ProgramStatusSequence>,
}

impl OscReader {
    /// A reader in the ground state that keeps each copy of at most
    /// `maximum_copy_byte_count` decoded bytes, and every text.
    pub(crate) fn with_maximum_copy_byte_count(maximum_copy_byte_count: usize) -> Self {
        Self {
            clipboard_copy_limit: Some(ClipboardCopyLimit::with_maximum_copy_byte_count(
                maximum_copy_byte_count,
            )),
            ..Self::default()
        }
    }

    /// Read `terminal_bytes`, the next bytes the VTE parser reads, and keep
    /// each copy and each text they finish.
    pub(crate) fn process_terminal_bytes(&mut self, terminal_bytes: &[u8]) {
        let mut byte_index = 0;
        while byte_index < terminal_bytes.len() {
            byte_index += self.process_next_bytes(&terminal_bytes[byte_index..], byte_index);
        }
    }

    /// Hand back the finished copies in the order their sequences ended, and
    /// keep none of them.
    pub(crate) fn take_clipboard_copies(&mut self) -> Vec<ClipboardCopy> {
        mem::take(&mut self.clipboard_copies)
    }

    /// Hand back the finished texts in the order their sequences ended, and
    /// keep none of them.
    pub(crate) fn take_osc_texts(&mut self) -> Vec<OscText> {
        mem::take(&mut self.osc_texts)
    }

    /// Hand back the finished program status sequences in the order they
    /// ended, and keep none of them.
    pub(crate) fn take_program_status_sequences(&mut self) -> Vec<ProgramStatusSequence> {
        mem::take(&mut self.program_status_sequences)
    }

    /// The bytes that put another reader where this one stands while it is
    /// inside a program status sequence: `ESC ] 7501 ;`, the argument bytes
    /// kept so far, and the `ESC` when the last byte read is the `ESC` after
    /// them. `None` in every other state.
    ///
    /// Example: after `ESC ] 7501 ; state=idle ESC`, the bytes are
    /// `ESC ] 7501 ; state=idle ESC`.
    pub(crate) fn serialize_open_program_status_sequence(&self) -> Option<Vec<u8>> {
        let (argument_bytes, is_after_escape) = match &self.reader_state {
            OscReaderState::ProgramStatusArguments { argument_bytes } => (argument_bytes, false),
            OscReaderState::ProgramStatusEscape { argument_bytes } => (argument_bytes, true),
            _ => return None,
        };
        let mut sequence_bytes = PROGRAM_STATUS_INTRODUCER_BYTES.to_vec();
        sequence_bytes.extend_from_slice(argument_bytes);
        if is_after_escape {
            sequence_bytes.push(ESCAPE_BYTE);
        }
        Some(sequence_bytes)
    }

    /// Read the bytes at the start of `terminal_bytes`, which holds at least
    /// one byte, and return how many it read. `first_byte_index` is the index
    /// of `terminal_bytes[0]` in the bytes of the
    /// [`process_terminal_bytes`](Self::process_terminal_bytes) call.
    ///
    /// In the ground state, every byte up to and including the next `ESC` is
    /// read in one step. In a payload, a text, the arguments of a program
    /// status sequence or a skipped command, every byte from `0x20` up to the
    /// next lower byte is read in one step:
    ///
    /// - a payload takes the bytes, or drops every byte of the payload when it
    ///   then holds more than `maximum_payload_byte_count` bytes;
    /// - a text and program status arguments take the bytes up to
    ///   [`OSC_BUFFER_BYTE_CAPACITY`] argument bytes, and drop the rest;
    /// - a skipped command drops them.
    ///
    /// Every other byte is read alone by
    /// [`process_terminal_byte`](Self::process_terminal_byte).
    fn process_next_bytes(&mut self, terminal_bytes: &[u8], first_byte_index: usize) -> usize {
        let first_byte = terminal_bytes[0];
        match &mut self.reader_state {
            OscReaderState::Ground => {
                let Some(escape_byte_index) = memchr::memchr(ESCAPE_BYTE, terminal_bytes) else {
                    return terminal_bytes.len();
                };
                self.reader_state = OscReaderState::Escape {
                    has_skipped_byte: false,
                };
                escape_byte_index + 1
            }
            OscReaderState::ClipboardPayload { payload_bytes, .. }
                if is_command_content_byte(first_byte) =>
            {
                let content_byte_count = count_command_content_bytes(terminal_bytes);
                let maximum_payload_byte_count =
                    self.clipboard_copy_limit.map_or(0, |clipboard_copy_limit| {
                        clipboard_copy_limit.maximum_payload_byte_count
                    });
                if content_byte_count > maximum_payload_byte_count - payload_bytes.len() {
                    self.reader_state = OscReaderState::SkippedCommand;
                } else {
                    payload_bytes.extend_from_slice(&terminal_bytes[..content_byte_count]);
                }
                content_byte_count
            }
            OscReaderState::TextArguments { argument_bytes, .. }
            | OscReaderState::ProgramStatusArguments { argument_bytes }
                if is_command_content_byte(first_byte) =>
            {
                let content_byte_count = count_command_content_bytes(terminal_bytes);
                let kept_byte_count =
                    content_byte_count.min(OSC_BUFFER_BYTE_CAPACITY - argument_bytes.len());
                argument_bytes.extend_from_slice(&terminal_bytes[..kept_byte_count]);
                content_byte_count
            }
            OscReaderState::SkippedCommand if is_command_content_byte(first_byte) => {
                count_command_content_bytes(terminal_bytes)
            }
            _ => {
                self.process_terminal_byte(first_byte, first_byte_index + 1);
                1
            }
        }
    }

    /// Read one byte. `next_byte_index` is the index right after
    /// `terminal_byte` in the bytes of the
    /// [`process_terminal_bytes`](Self::process_terminal_bytes) call. A content
    /// byte from `0x20` up in a payload, a text, program status arguments or a
    /// skipped command is never read here:
    /// [`process_next_bytes`](Self::process_next_bytes) reads it.
    ///
    /// In program status arguments, `BEL` finishes the sequence, `ESC` waits
    /// for the `\` that finishes it, `CAN` and `SUB` end it with no sequence,
    /// and every other byte is kept. After that `ESC`, `\` finishes the
    /// sequence, and every other byte ends it with no sequence and is read as
    /// it is read after any other `ESC`.
    fn process_terminal_byte(&mut self, terminal_byte: u8, next_byte_index: usize) {
        let reader_state = mem::take(&mut self.reader_state);
        self.reader_state = match reader_state {
            OscReaderState::Ground if terminal_byte == ESCAPE_BYTE => OscReaderState::Escape {
                has_skipped_byte: false,
            },
            OscReaderState::Ground => OscReaderState::Ground,
            OscReaderState::Escape { has_skipped_byte } => {
                advance_after_escape(terminal_byte, has_skipped_byte)
            }
            OscReaderState::ClipboardPayloadEscape {
                clipboard_targets,
                payload_bytes,
                has_skipped_byte,
            } => match terminal_byte {
                STRING_TERMINATOR_BYTE => {
                    self.finish_clipboard_copy(clipboard_targets, &payload_bytes);
                    OscReaderState::Ground
                }
                0x00..=0x17 | 0x19 | 0x1c..=0x1f => OscReaderState::ClipboardPayloadEscape {
                    clipboard_targets,
                    payload_bytes,
                    has_skipped_byte: true,
                },
                _ => advance_after_escape(terminal_byte, has_skipped_byte),
            },
            OscReaderState::ProgramStatusArguments { mut argument_bytes } => match terminal_byte {
                BELL_BYTE => {
                    self.program_status_sequences.push(ProgramStatusSequence {
                        argument_bytes,
                        is_bell_terminated: true,
                        end_byte_index: next_byte_index,
                    });
                    OscReaderState::Ground
                }
                ESCAPE_BYTE => OscReaderState::ProgramStatusEscape { argument_bytes },
                CANCEL_BYTE | SUBSTITUTE_BYTE => OscReaderState::Ground,
                control_byte => {
                    if argument_bytes.len() < OSC_BUFFER_BYTE_CAPACITY {
                        argument_bytes.push(control_byte);
                    }
                    OscReaderState::ProgramStatusArguments { argument_bytes }
                }
            },
            OscReaderState::ProgramStatusEscape { argument_bytes } => {
                if terminal_byte == STRING_TERMINATOR_BYTE {
                    self.program_status_sequences.push(ProgramStatusSequence {
                        argument_bytes,
                        is_bell_terminated: false,
                        end_byte_index: next_byte_index,
                    });
                    OscReaderState::Ground
                } else {
                    advance_after_escape(terminal_byte, false)
                }
            }
            command_state => self.advance_in_command(command_state, terminal_byte),
        };
    }

    /// The state after `terminal_byte` in `command_state`, one of the states
    /// inside an operating system command.
    ///
    /// - `BEL` finishes a payload as a copy, and a text command as a text.
    /// - `ESC` in a payload waits for the `\` that finishes the copy. In
    ///   every other command state it finishes a text command as a text, and
    ///   starts an escape.
    /// - `CAN` or `SUB` finishes a text command as a text, and makes no copy.
    /// - Every other byte below `0x20` is skipped. In the command number it
    ///   sets `has_skipped_byte`.
    fn advance_in_command(
        &mut self,
        command_state: OscReaderState,
        terminal_byte: u8,
    ) -> OscReaderState {
        match terminal_byte {
            BELL_BYTE => {
                if let OscReaderState::ClipboardPayload {
                    clipboard_targets,
                    payload_bytes,
                } = command_state
                {
                    self.finish_clipboard_copy(clipboard_targets, &payload_bytes);
                } else {
                    self.finish_osc_text(command_state);
                }
                OscReaderState::Ground
            }
            ESCAPE_BYTE => match command_state {
                OscReaderState::ClipboardPayload {
                    clipboard_targets,
                    payload_bytes,
                } => OscReaderState::ClipboardPayloadEscape {
                    clipboard_targets,
                    payload_bytes,
                    has_skipped_byte: false,
                },
                other_command_state => {
                    self.finish_osc_text(other_command_state);
                    OscReaderState::Escape {
                        has_skipped_byte: false,
                    }
                }
            },
            CANCEL_BYTE | SUBSTITUTE_BYTE => {
                self.finish_osc_text(command_state);
                OscReaderState::Ground
            }
            0x00..=0x1f => match command_state {
                OscReaderState::CommandNumber {
                    command_number_bytes,
                    command_number_byte_count,
                    ..
                } => OscReaderState::CommandNumber {
                    command_number_bytes,
                    command_number_byte_count,
                    has_skipped_byte: true,
                },
                other_command_state => other_command_state,
            },
            _ => self.advance_command_content(command_state, terminal_byte),
        }
    }

    /// The state after the content byte `terminal_byte` in the command number
    /// or the targets field. Any other state is handed back as it is.
    ///
    /// In the command number, `;` opens the targets field of a copy after
    /// `52` while the reader keeps copies, opens the arguments of a program
    /// status sequence after `7501` with no skipped byte, opens the text after
    /// the number of an [`OscTextCommand`], and makes every other command
    /// skipped. A fifth command number byte makes the command skipped. In the
    /// targets field, `;` opens the payload, and every other byte goes through
    /// [`push_clipboard_target`].
    fn advance_command_content(
        &self,
        command_state: OscReaderState,
        terminal_byte: u8,
    ) -> OscReaderState {
        match command_state {
            OscReaderState::CommandNumber {
                mut command_number_bytes,
                command_number_byte_count,
                has_skipped_byte,
            } => {
                if terminal_byte == FIELD_SEPARATOR_BYTE {
                    let read_command_number_bytes =
                        &command_number_bytes[..command_number_byte_count];
                    if read_command_number_bytes == CLIPBOARD_COMMAND_NUMBER_BYTES
                        && self.clipboard_copy_limit.is_some()
                    {
                        return OscReaderState::ClipboardTargets {
                            clipboard_targets: String::new(),
                        };
                    }
                    if read_command_number_bytes == PROGRAM_STATUS_COMMAND_NUMBER_BYTES
                        && !has_skipped_byte
                    {
                        return OscReaderState::ProgramStatusArguments {
                            argument_bytes: Vec::new(),
                        };
                    }
                    return match OscTextCommand::from_command_number(read_command_number_bytes) {
                        Some(osc_text_command) => OscReaderState::TextArguments {
                            osc_text_command,
                            argument_bytes: Vec::new(),
                        },
                        None => OscReaderState::SkippedCommand,
                    };
                }
                if command_number_byte_count == MAX_READ_COMMAND_NUMBER_BYTE_COUNT {
                    return OscReaderState::SkippedCommand;
                }
                command_number_bytes[command_number_byte_count] = terminal_byte;
                OscReaderState::CommandNumber {
                    command_number_bytes,
                    command_number_byte_count: command_number_byte_count + 1,
                    has_skipped_byte,
                }
            }
            OscReaderState::ClipboardTargets {
                mut clipboard_targets,
            } => {
                if terminal_byte == FIELD_SEPARATOR_BYTE {
                    return OscReaderState::ClipboardPayload {
                        clipboard_targets,
                        payload_bytes: Vec::new(),
                    };
                }
                push_clipboard_target(&mut clipboard_targets, terminal_byte);
                OscReaderState::ClipboardTargets { clipboard_targets }
            }
            other_state => other_state,
        }
    }

    /// Keep the text that `command_state` gives at the end of its command: the
    /// argument bytes of a text, and `argument_bytes: None` for a command
    /// number that names an [`OscTextCommand`]. Every other state keeps
    /// nothing.
    fn finish_osc_text(&mut self, command_state: OscReaderState) {
        let osc_text = match command_state {
            OscReaderState::TextArguments {
                osc_text_command,
                argument_bytes,
            } => OscText {
                osc_text_command,
                argument_bytes: Some(argument_bytes),
            },
            OscReaderState::CommandNumber {
                command_number_bytes,
                command_number_byte_count,
                ..
            } => {
                let Some(osc_text_command) = OscTextCommand::from_command_number(
                    &command_number_bytes[..command_number_byte_count],
                ) else {
                    return;
                };
                OscText {
                    osc_text_command,
                    argument_bytes: None,
                }
            }
            _ => return,
        };
        self.osc_texts.push(osc_text);
    }

    /// Keep the copy that a payload of `payload_bytes` for `clipboard_targets`
    /// gives, when the reader keeps copies and the payload gives one.
    fn finish_clipboard_copy(&mut self, clipboard_targets: String, payload_bytes: &[u8]) {
        let Some(clipboard_copy_limit) = self.clipboard_copy_limit else {
            return;
        };
        if let Some(clipboard_copy) =
            clipboard_copy_limit.parse_clipboard_copy(clipboard_targets, payload_bytes)
        {
            self.clipboard_copies.push(clipboard_copy);
        }
    }
}

/// Whether `terminal_byte` is content inside an operating system command:
/// every byte from `0x20` up.
fn is_command_content_byte(terminal_byte: u8) -> bool {
    terminal_byte >= 0x20
}

/// The number of content bytes at the start of `terminal_bytes`, up to the
/// first byte below `0x20`.
fn count_command_content_bytes(terminal_bytes: &[u8]) -> usize {
    terminal_bytes
        .iter()
        .position(|&terminal_byte| !is_command_content_byte(terminal_byte))
        .unwrap_or(terminal_bytes.len())
}

/// The state after `terminal_byte`, read right after an `ESC` that
/// `has_skipped_byte` says a skipped byte followed or not: `]` opens an
/// operating system command that carries `has_skipped_byte`, `CAN` or `SUB`
/// goes back to the ground state, `ESC` opens a new escape, every other byte
/// below `0x20` and every byte from `0x7f` up keep the escape open with
/// `has_skipped_byte` set, and every other byte goes back to the ground
/// state.
fn advance_after_escape(terminal_byte: u8, has_skipped_byte: bool) -> OscReaderState {
    match terminal_byte {
        OPERATING_SYSTEM_COMMAND_BYTE => OscReaderState::CommandNumber {
            command_number_bytes: [0; MAX_READ_COMMAND_NUMBER_BYTE_COUNT],
            command_number_byte_count: 0,
            has_skipped_byte,
        },
        CANCEL_BYTE | SUBSTITUTE_BYTE => OscReaderState::Ground,
        ESCAPE_BYTE => OscReaderState::Escape {
            has_skipped_byte: false,
        },
        0x00..=0x1f | 0x7f..=0xff => OscReaderState::Escape {
            has_skipped_byte: true,
        },
        _ => OscReaderState::Ground,
    }
}

#[cfg(test)]
mod tests;
