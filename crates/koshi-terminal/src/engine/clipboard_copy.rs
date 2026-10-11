//! The `OSC 52` clipboard copies that a program in a pane writes.
//!
//! A copy is `ESC ] 52 ; <targets> ; <payload>`, ended by `BEL` or by
//! `ESC \`. [`ClipboardCopyParser`] reads the bytes that the VTE parser reads,
//! and keeps each finished copy as a [`ClipboardCopy`]. It keeps nothing of any
//! other sequence.
//!
//! The parser finds the start and the end of a sequence as the VTE parser
//! does:
//!
//! - `ESC` starts an escape sequence in every state, and `ESC ]` opens an
//!   operating system command. The bytes `0x00` to `0x17`, `0x19`, `0x1b` to
//!   `0x1f` and every byte from `0x7f` up keep the escape open: `ESC DEL ]`
//!   opens a command too.
//! - Inside the command, the bytes `0x00` to `0x06`, `0x08` to `0x17`, `0x19`
//!   and `0x1c` to `0x1f` are skipped. A payload that is broken over lines
//!   decodes as one payload.
//! - `CAN` (`0x18`) or `SUB` (`0x1a`) ends the command, and makes no copy.
//! - `ESC` followed by a byte other than `\` ends the command and makes no
//!   copy. That `ESC` starts the next sequence. The bytes the VTE parser runs
//!   inside an escape (`0x00` to `0x17`, `0x19`, `0x1c` to `0x1f`) can come
//!   between the `ESC` and the `\`.
//!
//! The command is a copy only when the bytes before its first `;` are exactly
//! `52`. The targets field runs to the second `;`, and the payload runs to the
//! end of the command:
//!
//! - A command with no second `;` makes no copy.
//! - The payload `?` asks to read the clipboard, and makes no copy.
//! - A payload that is not base64, with or without `=` padding, makes an empty
//!   copy. The empty payload makes an empty copy too.
//! - A payload longer than `maximum_copy_byte_count` bytes rounded up to a
//!   multiple of 3, then multiplied by 4/3, makes no copy, and the parser holds
//!   none of its bytes. A payload whose decoded bytes are more than
//!   `maximum_copy_byte_count` makes no copy.
//!
//! Example: `ESC ] 52 ; c ; aGk= BEL` gives a copy of the two bytes `hi` for
//! the target `c`. `ESC ] 52 ; c ; aGk= CAN` gives nothing.

use std::mem;

use base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT;
use base64::Engine as _;
use koshi_core::clipboard::push_clipboard_target;

use super::{CANCEL_BYTE, ESCAPE_BYTE, SUBSTITUTE_BYTE};

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

/// One finished `OSC 52` copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCopy {
    /// The targets the copy names, as
    /// [`push_clipboard_target`] keeps them: each of `c`, `p`, `q`, `s` and `0`
    /// to `7`, once, in the order of its first use. Empty when the targets
    /// field names none. The outer terminal then uses its own default targets.
    pub clipboard_targets: String,
    /// The decoded payload. Empty for the empty payload and for a payload that
    /// is not base64: the outer terminal clears the targets.
    pub copied_bytes: Vec<u8>,
}

/// Where [`ClipboardCopyParser`] stands in the bytes it read.
#[derive(Debug, Default)]
enum ClipboardCopyParserState {
    /// Outside every sequence the parser reads.
    #[default]
    Ground,
    /// After an `ESC` that opened no sequence yet.
    Escape,
    /// In an operating system command, before its first `;`. The first
    /// `matched_byte_count` bytes of the command are the first bytes of `52`.
    CommandNumber { matched_byte_count: usize },
    /// In the targets field of a copy.
    Targets { clipboard_targets: String },
    /// In the payload of a copy.
    Payload {
        clipboard_targets: String,
        payload_bytes: Vec<u8>,
    },
    /// After an `ESC` in the payload of a copy.
    PayloadEscape {
        clipboard_targets: String,
        payload_bytes: Vec<u8>,
    },
    /// In an operating system command that makes no copy, until its end.
    SkippedCommand,
}

/// Reads the `OSC 52` copies in terminal bytes, one chunk after another. A
/// copy split across two chunks completes in the second chunk.
#[derive(Debug)]
pub(crate) struct ClipboardCopyParser {
    /// The most decoded bytes that one copy can carry.
    maximum_copy_byte_count: usize,
    /// The most payload bytes that the parser holds for one copy:
    /// `maximum_copy_byte_count` divided by 3, rounded up, then multiplied by
    /// 4.
    maximum_payload_byte_count: usize,
    /// Where the parser stands.
    parser_state: ClipboardCopyParserState,
    /// The finished copies that
    /// [`take_clipboard_copies`](Self::take_clipboard_copies) did not take yet,
    /// in the order their sequences ended.
    clipboard_copies: Vec<ClipboardCopy>,
}

impl ClipboardCopyParser {
    /// A parser in the ground state that keeps each copy of at most
    /// `maximum_copy_byte_count` decoded bytes.
    pub(crate) fn with_maximum_copy_byte_count(maximum_copy_byte_count: usize) -> Self {
        Self {
            maximum_copy_byte_count,
            maximum_payload_byte_count: maximum_copy_byte_count.div_ceil(3).saturating_mul(4),
            parser_state: ClipboardCopyParserState::Ground,
            clipboard_copies: Vec::new(),
        }
    }

    /// Read `terminal_bytes`, the next bytes the VTE parser reads, and keep
    /// each copy they finish.
    pub(crate) fn process_terminal_bytes(&mut self, terminal_bytes: &[u8]) {
        let mut byte_index = 0;
        while byte_index < terminal_bytes.len() {
            byte_index += self.process_next_bytes(&terminal_bytes[byte_index..]);
        }
    }

    /// Hand back the finished copies in the order their sequences ended, and
    /// keep none of them.
    pub(crate) fn take_clipboard_copies(&mut self) -> Vec<ClipboardCopy> {
        mem::take(&mut self.clipboard_copies)
    }

    /// Read the bytes at the start of `terminal_bytes`, which holds at least
    /// one byte, and return how many it read.
    ///
    /// In the ground state, every byte up to and including the next `ESC` is
    /// read in one step. In a payload, every byte from `0x20` up to the next
    /// lower byte is read in one step: it goes to the payload, or every byte of
    /// the payload is dropped when the payload then holds more than
    /// `maximum_payload_byte_count` bytes. Every other byte is read alone by
    /// [`process_terminal_byte`](Self::process_terminal_byte).
    fn process_next_bytes(&mut self, terminal_bytes: &[u8]) -> usize {
        match &mut self.parser_state {
            ClipboardCopyParserState::Ground => {
                let Some(escape_byte_index) = terminal_bytes
                    .iter()
                    .position(|&terminal_byte| terminal_byte == ESCAPE_BYTE)
                else {
                    return terminal_bytes.len();
                };
                self.parser_state = ClipboardCopyParserState::Escape;
                escape_byte_index + 1
            }
            ClipboardCopyParserState::Payload { payload_bytes, .. }
                if is_command_content_byte(terminal_bytes[0]) =>
            {
                let content_byte_count = terminal_bytes
                    .iter()
                    .position(|&terminal_byte| !is_command_content_byte(terminal_byte))
                    .unwrap_or(terminal_bytes.len());
                if content_byte_count > self.maximum_payload_byte_count - payload_bytes.len() {
                    self.parser_state = ClipboardCopyParserState::SkippedCommand;
                } else {
                    payload_bytes.extend_from_slice(&terminal_bytes[..content_byte_count]);
                }
                content_byte_count
            }
            _ => {
                self.process_terminal_byte(terminal_bytes[0]);
                1
            }
        }
    }

    /// Read one byte. A payload byte from `0x20` up is never read here:
    /// [`process_next_bytes`](Self::process_next_bytes) reads it.
    fn process_terminal_byte(&mut self, terminal_byte: u8) {
        let parser_state = mem::take(&mut self.parser_state);
        self.parser_state = match parser_state {
            ClipboardCopyParserState::Ground if terminal_byte == ESCAPE_BYTE => {
                ClipboardCopyParserState::Escape
            }
            ClipboardCopyParserState::Ground => ClipboardCopyParserState::Ground,
            ClipboardCopyParserState::Escape => advance_after_escape(terminal_byte),
            ClipboardCopyParserState::PayloadEscape {
                clipboard_targets,
                payload_bytes,
            } => match terminal_byte {
                STRING_TERMINATOR_BYTE => {
                    self.finish_clipboard_copy(clipboard_targets, &payload_bytes);
                    ClipboardCopyParserState::Ground
                }
                CANCEL_BYTE | SUBSTITUTE_BYTE | ESCAPE_BYTE => advance_after_escape(terminal_byte),
                0x00..=0x1f => ClipboardCopyParserState::PayloadEscape {
                    clipboard_targets,
                    payload_bytes,
                },
                _ => advance_after_escape(terminal_byte),
            },
            command_state => self.advance_in_command(command_state, terminal_byte),
        };
    }

    /// The state after `terminal_byte` in `command_state`, one of the states
    /// inside an operating system command. `BEL` finishes a payload as a copy,
    /// `ESC` ends the command or waits for the `\` that finishes a payload, and
    /// `CAN` or `SUB` ends the command with no copy.
    fn advance_in_command(
        &mut self,
        command_state: ClipboardCopyParserState,
        terminal_byte: u8,
    ) -> ClipboardCopyParserState {
        match terminal_byte {
            BELL_BYTE => {
                if let ClipboardCopyParserState::Payload {
                    clipboard_targets,
                    payload_bytes,
                } = command_state
                {
                    self.finish_clipboard_copy(clipboard_targets, &payload_bytes);
                }
                ClipboardCopyParserState::Ground
            }
            ESCAPE_BYTE => match command_state {
                ClipboardCopyParserState::Payload {
                    clipboard_targets,
                    payload_bytes,
                } => ClipboardCopyParserState::PayloadEscape {
                    clipboard_targets,
                    payload_bytes,
                },
                _ => ClipboardCopyParserState::Escape,
            },
            CANCEL_BYTE | SUBSTITUTE_BYTE => ClipboardCopyParserState::Ground,
            0x00..=0x1f => command_state,
            _ => advance_command_content(command_state, terminal_byte),
        }
    }

    /// Keep the copy that a payload of `payload_bytes` for `clipboard_targets`
    /// gives, when it gives one.
    fn finish_clipboard_copy(&mut self, clipboard_targets: String, payload_bytes: &[u8]) {
        if payload_bytes == b"?" {
            return;
        }
        let copied_bytes = STANDARD_PAD_INDIFFERENT
            .decode(payload_bytes)
            .unwrap_or_default();
        if copied_bytes.len() > self.maximum_copy_byte_count {
            return;
        }
        self.clipboard_copies.push(ClipboardCopy {
            clipboard_targets,
            copied_bytes,
        });
    }
}

/// Whether `terminal_byte` is content inside an operating system command:
/// every byte from `0x20` up.
fn is_command_content_byte(terminal_byte: u8) -> bool {
    terminal_byte >= 0x20
}

/// The state after `terminal_byte`, read right after an `ESC`: `]` opens an
/// operating system command, `CAN` or `SUB` goes back to the ground state,
/// `ESC`, every other byte below `0x20` and every byte from `0x7f` up keep the
/// escape open, and every other byte goes back to the ground state.
fn advance_after_escape(terminal_byte: u8) -> ClipboardCopyParserState {
    match terminal_byte {
        OPERATING_SYSTEM_COMMAND_BYTE => ClipboardCopyParserState::CommandNumber {
            matched_byte_count: 0,
        },
        CANCEL_BYTE | SUBSTITUTE_BYTE => ClipboardCopyParserState::Ground,
        0x00..=0x1f | 0x7f..=0xff => ClipboardCopyParserState::Escape,
        _ => ClipboardCopyParserState::Ground,
    }
}

/// The state after the content byte `terminal_byte` in the command number or
/// the targets field. Any other state is handed back as it is.
///
/// In the command number, `52` followed by `;` opens the targets field, and
/// any other byte makes the command one that makes no copy. In the targets
/// field, `;` opens the payload, and every other byte goes through
/// [`push_clipboard_target`].
fn advance_command_content(
    command_state: ClipboardCopyParserState,
    terminal_byte: u8,
) -> ClipboardCopyParserState {
    match command_state {
        ClipboardCopyParserState::CommandNumber { matched_byte_count } => {
            if terminal_byte == FIELD_SEPARATOR_BYTE {
                if matched_byte_count == CLIPBOARD_COMMAND_NUMBER_BYTES.len() {
                    return ClipboardCopyParserState::Targets {
                        clipboard_targets: String::new(),
                    };
                }
                return ClipboardCopyParserState::SkippedCommand;
            }
            if CLIPBOARD_COMMAND_NUMBER_BYTES.get(matched_byte_count) == Some(&terminal_byte) {
                return ClipboardCopyParserState::CommandNumber {
                    matched_byte_count: matched_byte_count + 1,
                };
            }
            ClipboardCopyParserState::SkippedCommand
        }
        ClipboardCopyParserState::Targets {
            mut clipboard_targets,
        } => {
            if terminal_byte == FIELD_SEPARATOR_BYTE {
                return ClipboardCopyParserState::Payload {
                    clipboard_targets,
                    payload_bytes: Vec::new(),
                };
            }
            push_clipboard_target(&mut clipboard_targets, terminal_byte);
            ClipboardCopyParserState::Targets { clipboard_targets }
        }
        other_state => other_state,
    }
}

#[cfg(test)]
mod tests;
