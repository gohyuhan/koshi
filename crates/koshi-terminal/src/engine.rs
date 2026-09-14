//! The per-pane terminal engine: a VTE parser — the state machine that decodes
//! raw terminal escape-sequence bytes into actions — paired with the
//! [`TerminalState`] it drives.
//!
//! One [`TerminalEngine`] backs one pane. PTY output arrives in read-sized
//! chunks that can split an escape sequence or a multi-byte UTF-8 code point
//! at any byte; the parser carries such a partial decode from one chunk to
//! the next. Each [`process_pty_output`](TerminalEngine::process_pty_output) call also hands back
//! the reply bytes the chunk's device queries produced, for the caller to
//! write into the PTY.
//!
//! The engine also keeps the bytes that put another parser where this one
//! stands — see [`undecoded_terminal_bytes`](TerminalEngine::undecoded_terminal_bytes) and
//! [`undecoded_graphics_bytes`](TerminalEngine::undecoded_graphics_bytes). A process-image
//! swap carries those bytes to the next image's parsers, and a sequence the
//! swap cut in half completes there. Graphics wrapper nesting and a transfer
//! that cannot be rebuilt within 64 KiB use the complete transport state.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::mem;
use std::ops::Range;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::process::PtySize;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub use crate::graphics::GraphicsTransportState;
use crate::graphics::{GraphicsError, GraphicsParser, ImageRecord, MAX_IMAGE_BYTE_COUNT};
use crate::scrollback::ScrollbackLimit;
use crate::state::{ShellIntegrationFact, TerminalState};

/// The byte every escape sequence starts with: `ESC`, `0x1b`.
const ESCAPE_BYTE: u8 = 0x1b;

/// `CAN`, `0x18`: abandons the sequence in progress from any parser state.
const CANCEL_BYTE: u8 = 0x18;

/// `SUB`, `0x1a`: abandons the sequence in progress from any parser state.
const SUBSTITUTE_BYTE: u8 = 0x1a;

/// The second byte of `ESC X`, which opens a start of string.
const START_OF_STRING_BYTE: u8 = 0x58;

/// The second byte of `ESC ^`, which opens a privacy message.
const PRIVACY_MESSAGE_BYTE: u8 = 0x5e;

/// The second byte of `ESC _`, which opens an application program command.
const APPLICATION_COMMAND_BYTE: u8 = 0x5f;

/// The bytes one of those three openings takes: `ESC` and the byte after it.
const STRING_OPENING_BYTE_COUNT: usize = 2;

/// The most bytes of a UTF-8 code point that can be missing at the end of a
/// chunk: a four-byte code point whose last byte has not arrived.
const CODE_POINT_TAIL_BYTE_COUNT: usize = 3;

/// The most bytes [`undecoded`](TerminalEngine::undecoded) holds, 64 KiB. The
/// engine stops holding a sequence that passes this size and reports nothing
/// until that sequence ends.
pub(crate) const MAX_UNDECODED_BYTE_COUNT: usize = 64 * 1024;

/// The largest number of image events held before the caller drains them.
pub const MAX_GRAPHICS_EVENT_COUNT: usize = 64;

/// The largest batch returned by [`TerminalEngine::take_graphics_events`], including
/// one queue-full report.
pub const MAX_GRAPHICS_EVENT_BATCH_COUNT: usize = MAX_GRAPHICS_EVENT_COUNT + 1;

/// One ordered image event produced by the terminal decoder.
pub type GraphicsEvent = Result<ImageRecord, GraphicsError>;

/// The maximum time an open synchronized-output update remains buffered.
pub const SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION: Duration = Duration::from_millis(150);

/// The most normalized terminal bytes held by one synchronized-output update.
pub const MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT: usize = 0x20_0000;

const BEGIN_SYNCHRONIZED_OUTPUT_BYTES: &[u8; 8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_OUTPUT_BYTES: &[u8; 8] = b"\x1b[?2026l";

/// Synchronized-output bytes and deadline carried across a process-image swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynchronizedOutputTransport {
    terminal_input: C1InputNormalizer,
    #[serde(rename = "bytes")]
    normalized_bytes: Vec<u8>,
    deadline: Option<SystemTime>,
}

impl SynchronizedOutputTransport {
    /// The normalized bytes held inside the open synchronized update.
    pub fn get_normalized_bytes(&self) -> &[u8] {
        &self.normalized_bytes
    }

    /// The wall-clock deadline for releasing the open update.
    pub fn get_deadline(&self) -> Option<SystemTime> {
        self.deadline
    }
}

impl<'de> Deserialize<'de> for SynchronizedOutputTransport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedSynchronizedOutputTransport {
            terminal_input: C1InputNormalizer,
            #[serde(deserialize_with = "deserialize_synchronized_output_bytes")]
            #[serde(rename = "bytes")]
            normalized_bytes: Vec<u8>,
            deadline: Option<SystemTime>,
        }

        let serialized_transport =
            SerializedSynchronizedOutputTransport::deserialize(deserializer)?;
        let terminal_input = serialized_transport.terminal_input;
        if terminal_input.remaining_utf8_continuation_count > 3 {
            return Err(de::Error::custom(
                "terminal-input UTF-8 continuation count is invalid",
            ));
        }
        if terminal_input.trailing_byte_count > terminal_input.trailing_bytes.len()
            || terminal_input.trailing_start_index >= terminal_input.trailing_bytes.len()
            || (terminal_input.trailing_byte_count < terminal_input.trailing_bytes.len()
                && terminal_input.trailing_start_index != terminal_input.trailing_byte_count)
        {
            return Err(de::Error::custom("terminal-input scanner tail is invalid"));
        }
        if serialized_transport.deadline.is_some()
            && serialized_transport.normalized_bytes.is_empty()
        {
            return Err(de::Error::custom(
                "synchronized-output deadline has no bytes",
            ));
        }
        if serialized_transport.deadline.is_none()
            && !serialized_transport.normalized_bytes.is_empty()
        {
            return Err(de::Error::custom(
                "synchronized-output bytes have no deadline",
            ));
        }
        Ok(Self {
            terminal_input,
            normalized_bytes: serialized_transport.normalized_bytes,
            deadline: serialized_transport.deadline,
        })
    }
}

fn deserialize_synchronized_output_bytes<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    struct SynchronizedOutputBytesVisitor;

    impl<'de> Visitor<'de> for SynchronizedOutputBytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("at most 2097152 synchronized-output bytes")
        }

        fn visit_seq<A>(self, mut byte_sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let byte_capacity = byte_sequence
                .size_hint()
                .unwrap_or(0)
                .min(MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT);
            let mut normalized_bytes = Vec::with_capacity(byte_capacity);
            while let Some(normalized_byte) = byte_sequence.next_element::<u8>()? {
                if normalized_bytes.len() == MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT {
                    return Err(de::Error::custom(
                        "synchronized-output bytes exceed 2097152",
                    ));
                }
                normalized_bytes.push(normalized_byte);
            }
            Ok(normalized_bytes)
        }
    }

    deserializer.deserialize_seq(SynchronizedOutputBytesVisitor)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SynchronizedControl {
    Begin,
    End,
}

#[derive(Default)]
struct SynchronizedOutput {
    buffered_bytes: Vec<u8>,
    deadline: Option<Instant>,
}

impl SynchronizedOutput {
    fn process_normalized_bytes<F>(
        &mut self,
        normalized_input_bytes: &[u8],
        synchronized_controls: &[(usize, SynchronizedControl)],
        monotonic_timestamp: Instant,
        mut release_bytes: F,
    ) -> bool
    where
        F: FnMut(&[u8]),
    {
        let mut has_released_bytes = if self
            .deadline
            .is_some_and(|deadline| monotonic_timestamp >= deadline)
        {
            self.release_buffered_bytes(&mut release_bytes)
        } else {
            false
        };
        let mut input_byte_index = 0;
        let mut next_control_index = 0;
        while input_byte_index < normalized_input_bytes.len() {
            if self.deadline.is_none() {
                let Some((control_index, control_end_index)) = synchronized_controls
                    [next_control_index..]
                    .iter()
                    .enumerate()
                    .find_map(|(relative_control_index, (control_end_index, control))| {
                        (*control == SynchronizedControl::Begin
                            && *control_end_index > input_byte_index)
                            .then_some((
                                next_control_index + relative_control_index,
                                *control_end_index,
                            ))
                    })
                else {
                    release_bytes(&normalized_input_bytes[input_byte_index..]);
                    return has_released_bytes || input_byte_index < normalized_input_bytes.len();
                };
                let held_byte_count = BEGIN_SYNCHRONIZED_OUTPUT_BYTES
                    .len()
                    .min(control_end_index - input_byte_index);
                let direct_release_end_index = control_end_index - held_byte_count;
                release_bytes(&normalized_input_bytes[input_byte_index..direct_release_end_index]);
                has_released_bytes |= input_byte_index < direct_release_end_index;
                if self.buffered_bytes.try_reserve(held_byte_count).is_err() {
                    release_bytes(
                        &normalized_input_bytes[direct_release_end_index..control_end_index],
                    );
                    has_released_bytes = true;
                } else {
                    self.buffered_bytes.extend_from_slice(
                        &normalized_input_bytes[direct_release_end_index..control_end_index],
                    );
                    self.deadline =
                        Some(monotonic_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION);
                }
                input_byte_index = control_end_index;
                next_control_index = control_index + 1;
                continue;
            }

            let available_byte_count =
                MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT - self.buffered_bytes.len();
            let read_byte_count =
                available_byte_count.min(normalized_input_bytes.len() - input_byte_index);
            if self.buffered_bytes.try_reserve(read_byte_count).is_err() {
                has_released_bytes |= self.release_buffered_bytes(&mut release_bytes);
                continue;
            }
            let buffered_byte_count = self.buffered_bytes.len();
            let input_end_index = input_byte_index + read_byte_count;
            self.buffered_bytes
                .extend_from_slice(&normalized_input_bytes[input_byte_index..input_end_index]);
            let mut last_begin_index = None;
            let mut last_end_index = None;
            while let Some((control_end_index, control)) =
                synchronized_controls.get(next_control_index).copied()
            {
                if control_end_index > input_end_index {
                    break;
                }
                if control_end_index > input_byte_index {
                    let control_start_index = (buffered_byte_count + control_end_index
                        - input_byte_index)
                        .checked_sub(BEGIN_SYNCHRONIZED_OUTPUT_BYTES.len())
                        .expect("a synchronized control starts in held bytes");
                    match control {
                        SynchronizedControl::Begin => last_begin_index = Some(control_start_index),
                        SynchronizedControl::End => last_end_index = Some(control_start_index),
                    }
                }
                next_control_index += 1;
            }
            self.apply_buffered_controls(
                last_begin_index,
                last_end_index,
                monotonic_timestamp,
                &mut release_bytes,
                &mut has_released_bytes,
            );
            input_byte_index = input_end_index;

            if self.deadline.is_some()
                && self.buffered_bytes.len() == MAX_SYNCHRONIZED_OUTPUT_BYTE_COUNT
            {
                has_released_bytes |= self.release_buffered_bytes(&mut release_bytes);
            }
        }
        has_released_bytes
    }

    fn apply_buffered_controls<F>(
        &mut self,
        last_begin_index: Option<usize>,
        last_end_index: Option<usize>,
        monotonic_timestamp: Instant,
        release_bytes: &mut F,
        has_released_bytes: &mut bool,
    ) where
        F: FnMut(&[u8]),
    {
        let Some(end_index) = last_end_index else {
            if last_begin_index.is_some() {
                self.deadline = Some(monotonic_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION);
            }
            return;
        };
        if let Some(begin_index) = last_begin_index.filter(|begin_index| *begin_index > end_index) {
            let retained_bytes = self.buffered_bytes.split_off(begin_index);
            let complete_bytes = mem::replace(&mut self.buffered_bytes, retained_bytes);
            release_bytes(&complete_bytes);
            *has_released_bytes = true;
            self.deadline = Some(monotonic_timestamp + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION);
        } else {
            *has_released_bytes |= self.release_buffered_bytes(release_bytes);
        }
    }

    fn release_buffered_bytes<F>(&mut self, release_bytes: &mut F) -> bool
    where
        F: FnMut(&[u8]),
    {
        self.deadline = None;
        if self.buffered_bytes.is_empty() {
            return false;
        }
        let released_bytes = mem::take(&mut self.buffered_bytes);
        release_bytes(&released_bytes);
        true
    }

    fn release_expired_bytes<F>(
        &mut self,
        monotonic_timestamp: Instant,
        mut release_bytes: F,
    ) -> bool
    where
        F: FnMut(&[u8]),
    {
        if self
            .deadline
            .is_none_or(|deadline| monotonic_timestamp < deadline)
        {
            return false;
        }
        self.release_buffered_bytes(&mut release_bytes)
    }

    fn compute_release_delay(&self, monotonic_timestamp: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(monotonic_timestamp))
    }

    fn build_synchronized_output_transport(
        &self,
        monotonic_timestamp: Instant,
        wall_clock_timestamp: SystemTime,
        terminal_input: C1InputNormalizer,
    ) -> Option<SynchronizedOutputTransport> {
        if self.deadline.is_none() && !terminal_input.is_transport_required() {
            return None;
        }
        Some(SynchronizedOutputTransport {
            terminal_input,
            normalized_bytes: self.buffered_bytes.clone(),
            deadline: self
                .compute_release_delay(monotonic_timestamp)
                .map(|delay| wall_clock_timestamp + delay),
        })
    }

    fn restore_synchronized_output_transport(
        &mut self,
        synchronized_output_transport: SynchronizedOutputTransport,
        monotonic_timestamp: Instant,
        wall_clock_timestamp: SystemTime,
    ) {
        self.buffered_bytes = synchronized_output_transport.normalized_bytes;
        self.deadline = synchronized_output_transport.deadline.map(|deadline| {
            let remaining_duration = deadline
                .duration_since(wall_clock_timestamp)
                .unwrap_or(Duration::ZERO)
                .min(SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION);
            monotonic_timestamp + remaining_duration
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum C1StringKind {
    Dcs,
    Osc,
    Dropped,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum C1InputState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    String(C1StringKind),
    StringEscape(C1StringKind),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct C1InputNormalizer {
    #[serde(rename = "state")]
    input_state: C1InputState,
    #[serde(rename = "utf8_continuations")]
    remaining_utf8_continuation_count: u8,
    #[serde(rename = "tail")]
    trailing_bytes: [u8; 8],
    #[serde(rename = "tail_len")]
    trailing_byte_count: usize,
    #[serde(rename = "tail_next")]
    trailing_start_index: usize,
}

struct NormalizedTerminalInput<'a> {
    normalized_bytes: Cow<'a, [u8]>,
    synchronized_controls: Vec<(usize, SynchronizedControl)>,
}

impl<'a> NormalizedTerminalInput<'a> {
    fn get_normalized_bytes(&self) -> &[u8] {
        &self.normalized_bytes
    }
}

fn remove_terminal_inert_bytes<'a>(
    input_bytes: &'a [u8],
    terminal_inert_ranges: &[Range<usize>],
) -> Cow<'a, [u8]> {
    if terminal_inert_ranges.is_empty() {
        return Cow::Borrowed(input_bytes);
    }

    let removed_byte_count = terminal_inert_ranges
        .iter()
        .map(std::ops::Range::len)
        .sum::<usize>();
    let mut compacted_bytes = Vec::with_capacity(input_bytes.len() - removed_byte_count);
    let mut input_byte_index = 0;
    for inert_range in terminal_inert_ranges {
        debug_assert!(
            input_byte_index <= inert_range.start
                && inert_range.start <= inert_range.end
                && inert_range.end <= input_bytes.len()
        );
        compacted_bytes.extend_from_slice(&input_bytes[input_byte_index..inert_range.start]);
        input_byte_index = inert_range.end;
    }
    compacted_bytes.extend_from_slice(&input_bytes[input_byte_index..]);
    Cow::Owned(compacted_bytes)
}

fn compute_terminal_byte_offset_without_inert_ranges(
    raw_end_byte_index: usize,
    terminal_inert_ranges: &[Range<usize>],
) -> usize {
    let removed_byte_count = terminal_inert_ranges
        .iter()
        .take_while(|range| range.start < raw_end_byte_index)
        .map(|range| range.end.min(raw_end_byte_index) - range.start)
        .sum::<usize>();
    raw_end_byte_index - removed_byte_count
}

impl C1InputNormalizer {
    fn is_transport_required(&self) -> bool {
        self.input_state != C1InputState::Ground || self.remaining_utf8_continuation_count != 0
    }

    fn normalize_terminal_input_bytes<'a>(
        &mut self,
        terminal_input_bytes: &'a [u8],
    ) -> NormalizedTerminalInput<'a> {
        let mut normalized_bytes: Option<Vec<u8>> = None;
        let mut synchronized_controls = Vec::new();
        let mut byte_index = 0;

        while byte_index < terminal_input_bytes.len() {
            let plain_byte_count =
                self.get_plain_terminal_input_byte_count(&terminal_input_bytes[byte_index..]);
            if plain_byte_count != 0 {
                self.push_trailing_bytes(
                    &terminal_input_bytes[byte_index..byte_index + plain_byte_count],
                );
                if let Some(normalized_byte_buffer) = normalized_bytes.as_mut() {
                    normalized_byte_buffer.extend_from_slice(
                        &terminal_input_bytes[byte_index..byte_index + plain_byte_count],
                    );
                }
                byte_index += plain_byte_count;
                continue;
            }

            let terminal_input_byte = terminal_input_bytes[byte_index];
            let (replacement_bytes, synchronized_control) =
                self.normalize_terminal_input_byte(terminal_input_byte);
            if let Some(normalized_byte_buffer) = normalized_bytes.as_mut() {
                if let Some(replacement_bytes) = replacement_bytes {
                    normalized_byte_buffer.extend_from_slice(replacement_bytes);
                } else {
                    normalized_byte_buffer.push(terminal_input_byte);
                }
            } else if let Some(replacement_bytes) = replacement_bytes {
                let mut normalized_byte_buffer =
                    Vec::with_capacity(terminal_input_bytes.len().saturating_add(1));
                normalized_byte_buffer.extend_from_slice(&terminal_input_bytes[..byte_index]);
                normalized_byte_buffer.extend_from_slice(replacement_bytes);
                normalized_bytes = Some(normalized_byte_buffer);
            }
            let tail_bytes =
                replacement_bytes.unwrap_or(std::slice::from_ref(&terminal_input_byte));
            self.push_trailing_bytes(tail_bytes);
            if let Some(synchronized_control) = synchronized_control
                .filter(|control| self.is_synchronized_control_in_trailing_bytes(*control))
            {
                let control_end_byte_index =
                    normalized_bytes.as_ref().map_or(byte_index + 1, Vec::len);
                synchronized_controls.push((control_end_byte_index, synchronized_control));
            }
            byte_index += 1;
        }

        match normalized_bytes {
            Some(normalized_bytes) => NormalizedTerminalInput {
                normalized_bytes: Cow::Owned(normalized_bytes),
                synchronized_controls,
            },
            None => NormalizedTerminalInput {
                normalized_bytes: Cow::Borrowed(terminal_input_bytes),
                synchronized_controls,
            },
        }
    }

    /// Return the leading bytes that cannot change the normalizer state.
    fn get_plain_terminal_input_byte_count(&self, terminal_input_bytes: &[u8]) -> usize {
        if self.remaining_utf8_continuation_count != 0 {
            return 0;
        }
        let changes_input_state = |terminal_input_byte: u8| match self.input_state {
            C1InputState::Ground => {
                terminal_input_byte == ESCAPE_BYTE || terminal_input_byte >= 0x80
            }
            C1InputState::String(string_kind) => {
                matches!(
                    terminal_input_byte,
                    CANCEL_BYTE | SUBSTITUTE_BYTE | ESCAPE_BYTE
                ) || terminal_input_byte >= 0x80
                    || (terminal_input_byte == 0x07 && matches!(string_kind, C1StringKind::Osc))
            }
            C1InputState::Escape
            | C1InputState::EscapeIntermediate
            | C1InputState::Csi
            | C1InputState::StringEscape(_) => true,
        };
        terminal_input_bytes
            .iter()
            .position(|terminal_input_byte| changes_input_state(*terminal_input_byte))
            .unwrap_or(terminal_input_bytes.len())
    }

    fn normalize_terminal_input_byte(
        &mut self,
        terminal_input_byte: u8,
    ) -> (Option<&'static [u8]>, Option<SynchronizedControl>) {
        if matches!(
            self.input_state,
            C1InputState::Ground | C1InputState::String(_)
        ) {
            if self.remaining_utf8_continuation_count != 0 {
                if (terminal_input_byte & 0xc0) == 0x80 {
                    self.remaining_utf8_continuation_count -= 1;
                    return (None, None);
                }
                self.remaining_utf8_continuation_count = 0;
            }
            self.remaining_utf8_continuation_count = match terminal_input_byte {
                0xc2..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf4 => 3,
                _ => 0,
            };
            if self.remaining_utf8_continuation_count != 0 {
                return (None, None);
            }
        } else {
            self.remaining_utf8_continuation_count = 0;
        }

        match terminal_input_byte {
            0x90 if matches!(self.input_state, C1InputState::Ground) => {
                self.input_state = C1InputState::String(C1StringKind::Dcs);
                (Some(b"\x1bP"), None)
            }
            0x98 | 0x9e if matches!(self.input_state, C1InputState::Ground) => {
                self.input_state = C1InputState::String(C1StringKind::Dropped);
                (
                    Some(if terminal_input_byte == 0x98 {
                        b"\x1bX"
                    } else {
                        b"\x1b^"
                    }),
                    None,
                )
            }
            0x9d if matches!(self.input_state, C1InputState::Ground) => {
                self.input_state = C1InputState::String(C1StringKind::Osc);
                (Some(b"\x1b]"), None)
            }
            0x9b if matches!(self.input_state, C1InputState::Ground) => {
                self.input_state = C1InputState::Csi;
                (Some(b"\x1b["), None)
            }
            0x9f if matches!(self.input_state, C1InputState::Ground) => {
                self.input_state = C1InputState::String(C1StringKind::Dropped);
                (Some(b"\x1b_"), None)
            }
            0x9c if matches!(
                self.input_state,
                C1InputState::String(_) | C1InputState::StringEscape(_)
            ) =>
            {
                self.input_state = C1InputState::Ground;
                (Some(b"\x1b\\"), None)
            }
            _ => (None, self.advance_terminal_input_state(terminal_input_byte)),
        }
    }

    fn advance_terminal_input_state(
        &mut self,
        terminal_input_byte: u8,
    ) -> Option<SynchronizedControl> {
        let mut control = None;
        self.input_state = match self.input_state {
            C1InputState::Ground => match terminal_input_byte {
                ESCAPE_BYTE => C1InputState::Escape,
                _ => C1InputState::Ground,
            },
            C1InputState::Escape => Self::advance_escape(terminal_input_byte),
            C1InputState::EscapeIntermediate => match terminal_input_byte {
                CANCEL_BYTE | SUBSTITUTE_BYTE | 0x30..=0x7e => C1InputState::Ground,
                ESCAPE_BYTE => C1InputState::Escape,
                _ => C1InputState::EscapeIntermediate,
            },
            C1InputState::Csi => match terminal_input_byte {
                CANCEL_BYTE | SUBSTITUTE_BYTE => C1InputState::Ground,
                ESCAPE_BYTE => C1InputState::Escape,
                0x40..=0x7e => {
                    control = match terminal_input_byte {
                        b'h' => Some(SynchronizedControl::Begin),
                        b'l' => Some(SynchronizedControl::End),
                        _ => None,
                    };
                    C1InputState::Ground
                }
                _ => C1InputState::Csi,
            },
            C1InputState::String(string_kind) => match terminal_input_byte {
                CANCEL_BYTE | SUBSTITUTE_BYTE => C1InputState::Ground,
                ESCAPE_BYTE => C1InputState::StringEscape(string_kind),
                0x07 if matches!(string_kind, C1StringKind::Osc) => C1InputState::Ground,
                _ => C1InputState::String(string_kind),
            },
            C1InputState::StringEscape(string_kind) => match terminal_input_byte {
                CANCEL_BYTE | SUBSTITUTE_BYTE | b'\\' => C1InputState::Ground,
                ESCAPE_BYTE => C1InputState::StringEscape(string_kind),
                _ => C1InputState::String(string_kind),
            },
        };
        control
    }

    fn advance_escape(terminal_input_byte: u8) -> C1InputState {
        match terminal_input_byte {
            CANCEL_BYTE | SUBSTITUTE_BYTE => C1InputState::Ground,
            ESCAPE_BYTE => C1InputState::Escape,
            0x20..=0x2f => C1InputState::EscapeIntermediate,
            0x50 => C1InputState::String(C1StringKind::Dcs),
            0x58 | 0x5e | 0x5f => C1InputState::String(C1StringKind::Dropped),
            0x5b => C1InputState::Csi,
            0x5d => C1InputState::String(C1StringKind::Osc),
            0x30..=0x7e => C1InputState::Ground,
            _ => C1InputState::Escape,
        }
    }

    fn push_trailing_bytes(&mut self, trailing_bytes: &[u8]) {
        let trailing_byte_capacity = self.trailing_bytes.len();
        if trailing_bytes.len() >= trailing_byte_capacity {
            self.trailing_bytes
                .copy_from_slice(&trailing_bytes[trailing_bytes.len() - trailing_byte_capacity..]);
            self.trailing_byte_count = trailing_byte_capacity;
            self.trailing_start_index = 0;
            return;
        }
        for trailing_byte in trailing_bytes {
            self.trailing_bytes[self.trailing_start_index] = *trailing_byte;
            self.trailing_start_index = (self.trailing_start_index + 1) % trailing_byte_capacity;
            self.trailing_byte_count = (self.trailing_byte_count + 1).min(trailing_byte_capacity);
        }
    }

    fn is_synchronized_control_in_trailing_bytes(&self, control: SynchronizedControl) -> bool {
        if self.trailing_byte_count != self.trailing_bytes.len() {
            return false;
        }
        let expected_control_bytes = match control {
            SynchronizedControl::Begin => BEGIN_SYNCHRONIZED_OUTPUT_BYTES,
            SynchronizedControl::End => END_SYNCHRONIZED_OUTPUT_BYTES,
        };
        expected_control_bytes
            .iter()
            .enumerate()
            .all(|(trailing_byte_index, expected_byte)| {
                self.trailing_bytes
                    [(self.trailing_start_index + trailing_byte_index) % self.trailing_bytes.len()]
                    == *expected_byte
            })
    }
}

/// The most bytes one OSC sequence accumulates. The parser drops every byte
/// past this and dispatches what it holds when the sequence ends.
pub(crate) const OSC_BUFFER_BYTE_CAPACITY: usize = 8 * 1024;

/// One pane's emulation engine: the byte decoder and the screen model it
/// feeds.
pub struct TerminalEngine {
    /// The VTE state machine. Holds any partial escape sequence or split
    /// UTF-8 code point between [`process_pty_output`](TerminalEngine::process_pty_output) calls.
    parser: vte::Parser<OSC_BUFFER_BYTE_CAPACITY>,
    /// The screen model the parser's decoded actions mutate.
    terminal_state: TerminalState,
    /// The canonical bytes that put another parser where `parser` stands, as
    /// [`undecoded_terminal_bytes`](TerminalEngine::undecoded_terminal_bytes) describes them. Eight-bit
    /// string controls use their seven-bit `ESC` forms so the VTE parser can
    /// replay them.
    undecoded_terminal_bytes: Vec<u8>,
    /// The raw bytes that put the graphics parser where it stands.
    undecoded_graphics_bytes: Vec<u8>,
    /// A second parser fed the same bytes as `parser`, driving no screen. It
    /// reports where each sequence ends. One chunk costs one pass over that
    /// chunk, however long the sequence it continues.
    undecoded_parser: vte::Parser<OSC_BUFFER_BYTE_CAPACITY>,
    /// Set while `undecoded_parser` sits on a sequence boundary, where `undecoded_terminal_bytes`
    /// holds at most the first bytes of a UTF-8 code point.
    is_at_sequence_boundary: bool,
    /// Set while `undecoded_parser` sits in the body of a string whose bytes
    /// `undecoded_terminal_bytes` does not hold: a device control string, a start of string, a
    /// privacy message, an application program command, or any sequence that
    /// passed [`MAX_UNDECODED_BYTE_COUNT`]. `undecoded` holds the opening bytes of the
    /// first four kinds and nothing of the fifth.
    is_in_string_body: bool,
    /// The raw terminal-image parser that observes the same bytes as the VTE
    /// parser without changing the terminal model.
    graphics_parser: GraphicsParser,
    /// Complete image records and recoverable image errors waiting for the
    /// terminal caller.
    graphics_events: VecDeque<GraphicsEvent>,
    /// Converts 8-bit string controls to the 7-bit forms supported by `vte`.
    terminal_input_normalizer: C1InputNormalizer,
    /// Holds complete top-level DEC synchronized-output groups before either parser sees them.
    synchronized_output: SynchronizedOutput,
    /// RGBA bytes held by successful image events.
    queued_graphics_rgba_byte_count: usize,
    /// Number of events dropped after the bounded graphics queue filled.
    dropped_graphics_event_count: usize,
    /// Number of dropped events that were graphics errors.
    dropped_graphics_error_count: usize,
    /// Set when the next DCS is a GNU Screen continuation wrapper.
    is_graphics_screen_continuation: bool,
    /// Set when the carried bytes belong to an open GNU Screen wrapper.
    is_graphics_screen_wrapper_active: bool,
    /// Set when the next DCS is a tmux continuation wrapper.
    is_graphics_tmux_continuation: bool,
    /// Set when the carried bytes belong to an open tmux wrapper.
    is_graphics_tmux_wrapper_active: bool,
}

impl TerminalEngine {
    /// An engine for a fresh pane of `pty_size`: an idle parser and a blank
    /// [`TerminalState`].
    pub fn from_pty_size(pty_size: PtySize) -> Self {
        Self::with_scrollback(pty_size, ScrollbackLimit::default())
    }

    /// Like [`from_pty_size`](Self::from_pty_size), with `scrollback_limit` as
    /// the scrollback limit.
    pub fn with_scrollback(pty_size: PtySize, scrollback_limit: ScrollbackLimit) -> Self {
        Self::from_idle_parsers(TerminalState::with_scrollback(pty_size, scrollback_limit))
    }

    /// An engine around `terminal_state` with both parsers idle and nothing held.
    fn from_idle_parsers(terminal_state: TerminalState) -> Self {
        TerminalEngine {
            parser: vte::Parser::<OSC_BUFFER_BYTE_CAPACITY>::new_with_size(),
            terminal_state,
            undecoded_terminal_bytes: Vec::new(),
            undecoded_graphics_bytes: Vec::new(),
            undecoded_parser: vte::Parser::<OSC_BUFFER_BYTE_CAPACITY>::new_with_size(),
            is_at_sequence_boundary: true,
            is_in_string_body: false,
            graphics_parser: GraphicsParser::default(),
            graphics_events: VecDeque::new(),
            terminal_input_normalizer: C1InputNormalizer::default(),
            synchronized_output: SynchronizedOutput::default(),
            queued_graphics_rgba_byte_count: 0,
            dropped_graphics_event_count: 0,
            dropped_graphics_error_count: 0,
            is_graphics_screen_continuation: false,
            is_graphics_screen_wrapper_active: false,
            is_graphics_tmux_continuation: false,
            is_graphics_tmux_wrapper_active: false,
        }
    }

    /// Feed one chunk of PTY output through the parser into the state, and
    /// return the reply bytes any device queries in the chunk produced —
    /// answers to DA (Device Attributes), DSR (Device Status Report), and
    /// DECRQM (Request Mode) queries the app sent; empty when the chunk held
    /// no query. The caller writes the replies back into the pane's PTY.
    ///
    /// Chunks may split an escape sequence or a UTF-8 code point at any byte;
    /// the parser resumes the partial decode on the next call, and
    /// [`undecoded_terminal_bytes`](Self::undecoded_terminal_bytes) is set to the canonical bytes that put
    /// another parser where this one now stands. This method drains shell-integration
    /// facts without returning them; use
    /// [`Self::process_pty_output_with_shell_integration`] when the caller handles those facts.
    #[must_use = "undelivered replies hang the querying app"]
    pub fn process_pty_output(&mut self, pty_output_bytes: &[u8]) -> Vec<u8> {
        let (reply_bytes, _) = self.process_pty_output_with_shell_integration(pty_output_bytes);
        reply_bytes
    }

    /// Feed one chunk through the parser and return device replies plus the
    /// shell-integration facts that the chunk produced. A `C` marker when the
    /// shell is not already running a command returns
    /// [`ShellIntegrationFact::CommandStarted`], and a matched `D` marker
    /// returns [`ShellIntegrationFact::CommandFinished`] with its exit code.
    /// The facts contain no command text. `ESC ] 133 ; C` followed by
    /// `ESC ] 133 ; D ; 137` returns both facts in that order.
    #[must_use = "undelivered replies or shell facts are lost"]
    pub fn process_pty_output_with_shell_integration(
        &mut self,
        pty_output_bytes: &[u8],
    ) -> (Vec<u8>, Vec<ShellIntegrationFact>) {
        let (reply_bytes, shell_integration_facts, _) =
            self.process_pty_output_with_shell_integration_at(pty_output_bytes, Instant::now());
        (reply_bytes, shell_integration_facts)
    }

    /// Feed one chunk at `now` and report whether bytes reached the terminal parsers.
    #[must_use = "undelivered replies or shell facts are lost"]
    pub fn process_pty_output_with_shell_integration_at(
        &mut self,
        pty_output_bytes: &[u8],
        monotonic_timestamp: Instant,
    ) -> (Vec<u8>, Vec<ShellIntegrationFact>, bool) {
        let normalized_terminal_input = self
            .terminal_input_normalizer
            .normalize_terminal_input_bytes(pty_output_bytes);
        let mut synchronized_output = mem::take(&mut self.synchronized_output);
        let has_advanced = synchronized_output.process_normalized_bytes(
            normalized_terminal_input.get_normalized_bytes(),
            &normalized_terminal_input.synchronized_controls,
            monotonic_timestamp,
            |normalized_bytes| self.process_normalized_terminal_bytes(normalized_bytes),
        );
        self.synchronized_output = synchronized_output;
        (
            self.terminal_state.take_device_query_replies(),
            self.terminal_state.take_shell_integration_facts(),
            has_advanced,
        )
    }

    fn process_normalized_terminal_bytes(&mut self, normalized_bytes: &[u8]) {
        let graphics_advance = self
            .graphics_parser
            .process_graphics_operations_with_offsets(normalized_bytes);
        let terminal_input =
            remove_terminal_inert_bytes(normalized_bytes, &graphics_advance.terminal_inert_ranges);
        let terminal_bytes = terminal_input.as_ref();
        let mut parser_byte_index = 0;
        for (graphics_byte_offset, graphics_event) in graphics_advance.completed_graphics_events {
            let graphics_event_end_index = compute_terminal_byte_offset_without_inert_ranges(
                graphics_byte_offset + 1,
                &graphics_advance.terminal_inert_ranges,
            );
            if graphics_event_end_index > parser_byte_index {
                self.parser.advance(
                    &mut self.terminal_state,
                    &terminal_bytes[parser_byte_index..graphics_event_end_index],
                );
                parser_byte_index = graphics_event_end_index;
            }
            let cursor_position = self.terminal_state.get_active_cursor_position();
            self.process_graphics_operation(graphics_event, cursor_position);
        }
        if parser_byte_index < terminal_bytes.len() {
            self.parser.advance(
                &mut self.terminal_state,
                &terminal_bytes[parser_byte_index..],
            );
        }
        self.capture_undecoded_terminal_bytes(terminal_bytes);
        self.update_graphics_transport_state();
    }

    /// An engine wrapped around an existing `terminal_state`, with a parser fed
    /// `terminal_undecoded_bytes` — the bytes that put a parser where the previous
    /// engine's parser stood, from [`undecoded_terminal_bytes`](Self::undecoded_terminal_bytes).
    ///
    /// The replay reaches no screen: every action those bytes dispatch is
    /// dropped, and `terminal_state` stays as passed. The replay leaves the parser at
    /// the previous parser's position, and the rest of a sequence that was cut
    /// in half completes here. Pass an empty slice for a state that was not
    /// carried out of a running engine.
    pub fn from_terminal_state(
        terminal_state: TerminalState,
        terminal_undecoded_bytes: &[u8],
    ) -> Self {
        Self::from_terminal_state_with_graphics(terminal_state, terminal_undecoded_bytes, &[])
    }

    /// An engine around `terminal_state` with the VTE and graphics parser positions
    /// carried from another engine.
    pub fn from_terminal_state_with_graphics(
        terminal_state: TerminalState,
        terminal_undecoded_bytes: &[u8],
        graphics_undecoded_bytes: &[u8],
    ) -> Self {
        Self::from_terminal_state_with_graphics_and_events(
            terminal_state,
            terminal_undecoded_bytes,
            graphics_undecoded_bytes,
            &[],
        )
    }

    /// An engine around `terminal_state` with parser positions and queued graphics
    /// events carried from another engine.
    pub fn from_terminal_state_with_graphics_and_events(
        terminal_state: TerminalState,
        terminal_undecoded_bytes: &[u8],
        graphics_undecoded_bytes: &[u8],
        graphics_events: &[GraphicsEvent],
    ) -> Self {
        Self::from_terminal_state_with_graphics_and_events_and_screen(
            terminal_state,
            terminal_undecoded_bytes,
            graphics_undecoded_bytes,
            graphics_events,
            false,
            false,
        )
    }

    /// An engine around `terminal_state` with parser positions, queued graphics events,
    /// and GNU Screen continuation state carried from another engine. This is
    /// the compatibility form for the two legacy wrapper flags; use
    /// [`from_terminal_state_with_graphics_and_events_and_wrappers`](Self::from_terminal_state_with_graphics_and_events_and_wrappers)
    /// when nested parser state or bounded-transfer graphics_abandonment is present.
    pub fn from_terminal_state_with_graphics_and_events_and_screen(
        terminal_state: TerminalState,
        terminal_undecoded_bytes: &[u8],
        graphics_undecoded_bytes: &[u8],
        graphics_events: &[GraphicsEvent],
        is_graphics_screen_continuation: bool,
        is_graphics_screen_wrapper_active: bool,
    ) -> Self {
        Self::from_terminal_state_with_graphics_and_events_and_wrappers(
            terminal_state,
            terminal_undecoded_bytes,
            graphics_undecoded_bytes,
            graphics_events,
            GraphicsTransportState {
                is_screen_continuation: is_graphics_screen_continuation,
                is_screen_wrapper_active: is_graphics_screen_wrapper_active,
                ..GraphicsTransportState::default()
            },
        )
    }

    /// An engine around `terminal_state` with parser positions, queued graphics events,
    /// and the complete graphics transport state carried from another engine.
    /// A split Screen wrapper such as `ESC P ESC ] 1337;File=... ESC \` is
    /// restored from its nested parser record before the next PTY bytes arrive.
    pub fn from_terminal_state_with_graphics_and_events_and_wrappers(
        terminal_state: TerminalState,
        terminal_undecoded_bytes: &[u8],
        graphics_undecoded_bytes: &[u8],
        graphics_events: &[GraphicsEvent],
        graphics_transport_state: GraphicsTransportState,
    ) -> Self {
        Self::from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
            terminal_state,
            terminal_undecoded_bytes,
            graphics_undecoded_bytes,
            graphics_events,
            graphics_transport_state,
            None,
            Instant::now(),
            SystemTime::now(),
        )
    }

    /// Restore parser, graphics, and synchronized-output transport state.
    #[allow(clippy::too_many_arguments)]
    pub fn from_terminal_state_with_graphics_events_wrappers_and_synchronized_output(
        terminal_state: TerminalState,
        terminal_undecoded_bytes: &[u8],
        graphics_undecoded_bytes: &[u8],
        graphics_events: &[GraphicsEvent],
        graphics_transport_state: GraphicsTransportState,
        synchronized_output_transport: Option<SynchronizedOutputTransport>,
        monotonic_timestamp: Instant,
        wall_clock_timestamp: SystemTime,
    ) -> Self {
        let mut terminal_engine = Self::from_idle_parsers(terminal_state);
        terminal_engine
            .graphics_parser
            .restore_graphics_carry_state(graphics_undecoded_bytes, graphics_transport_state);
        let normalized_terminal_input_bytes = if synchronized_output_transport.is_some() {
            C1InputNormalizer::default()
                .normalize_terminal_input_bytes(terminal_undecoded_bytes)
                .normalized_bytes
                .into_owned()
        } else {
            terminal_engine
                .terminal_input_normalizer
                .normalize_terminal_input_bytes(terminal_undecoded_bytes)
                .normalized_bytes
                .into_owned()
        };
        terminal_engine
            .parser
            .advance(&mut NoScreen, &normalized_terminal_input_bytes);
        terminal_engine.capture_undecoded_terminal_bytes(&normalized_terminal_input_bytes);
        terminal_engine.update_graphics_transport_state();
        for graphics_event in graphics_events {
            if let Err(GraphicsError::QueueFull {
                dropped_event_count,
            }) = graphics_event
            {
                terminal_engine.dropped_graphics_event_count = terminal_engine
                    .dropped_graphics_event_count
                    .saturating_add(*dropped_event_count);
            } else {
                terminal_engine.enqueue_graphics_event(graphics_event.clone());
            }
        }
        if let Some(synchronized_output_transport) = synchronized_output_transport {
            terminal_engine.terminal_input_normalizer =
                synchronized_output_transport.terminal_input;
            terminal_engine
                .synchronized_output
                .restore_synchronized_output_transport(
                    synchronized_output_transport,
                    monotonic_timestamp,
                    wall_clock_timestamp,
                );
        }
        terminal_engine
    }

    /// Take the engine apart and hand back its screen model, dropping the
    /// parser. Read [`undecoded_terminal_bytes`](Self::undecoded_terminal_bytes),
    /// [`undecoded_graphics_bytes`](Self::undecoded_graphics_bytes), and
    /// [`take_graphics_events`](Self::take_graphics_events) first to carry parser positions
    /// and queued image events.
    pub fn into_terminal_state(self) -> TerminalState {
        self.terminal_state
    }

    /// Iterate the queued image records and recoverable image errors in the
    /// order their protocol terminators reached the terminal parser, without
    /// removing them. An event dropped after the bounded queue filled is not
    /// visible here; [`get_dropped_graphics_error_count`](Self::get_dropped_graphics_error_count)
    /// and [`take_graphics_events`](Self::take_graphics_events) report dropped events.
    pub fn list_graphics_events(&self) -> impl Iterator<Item = &GraphicsEvent> {
        self.graphics_events.iter()
    }

    /// Return the number of graphics errors dropped because the event queue
    /// reached its count or image-byte limit.
    pub fn get_dropped_graphics_error_count(&self) -> usize {
        self.dropped_graphics_error_count
    }

    /// Drain complete image records and recoverable image errors in the order
    /// their protocol terminators reached the terminal parser. When the queue
    /// dropped records, one `QueueFull` report follows the held events.
    ///
    /// A display record is applied to image state before it is made available
    /// to the caller; a malformed or unplaceable record returns a typed error.
    pub fn take_graphics_events(&mut self) -> Vec<GraphicsEvent> {
        let mut graphics_events: Vec<GraphicsEvent> = self.graphics_events.drain(..).collect();
        self.queued_graphics_rgba_byte_count = 0;
        if self.dropped_graphics_event_count != 0 {
            graphics_events.push(Err(GraphicsError::QueueFull {
                dropped_event_count: self.dropped_graphics_event_count,
            }));
            self.dropped_graphics_event_count = 0;
        }
        self.dropped_graphics_error_count = 0;
        graphics_events
    }

    /// Finish the graphics stream, report any incomplete transfer, and drain
    /// all image events already queued by the engine.
    ///
    /// A stream ending after `ESC _ Gf=32,s=1,v=1;` returns one typed
    /// `Truncated` error and leaves the terminal cells unchanged.
    pub fn finish_graphics_stream(&mut self) -> Vec<GraphicsEvent> {
        let mut synchronized_output = mem::take(&mut self.synchronized_output);
        synchronized_output.release_buffered_bytes(&mut |normalized_bytes| {
            self.process_normalized_terminal_bytes(normalized_bytes)
        });
        self.synchronized_output = synchronized_output;
        let cursor_position = self.terminal_state.get_active_cursor_position();
        for graphics_event in self.graphics_parser.finish_graphics_stream() {
            self.process_graphics_operation(graphics_event, cursor_position);
        }
        self.undecoded_graphics_bytes.clear();
        self.is_graphics_screen_continuation = false;
        self.is_graphics_screen_wrapper_active = false;
        self.is_graphics_tmux_continuation = false;
        self.is_graphics_tmux_wrapper_active = false;
        self.terminal_input_normalizer = C1InputNormalizer::default();
        self.take_graphics_events()
    }

    /// The canonical bytes that put another parser where this one stands: one escape
    /// sequence that has no final byte yet, the opening of a string whose body
    /// is still arriving, or the first bytes of a UTF-8 code point. Empty when
    /// the parser sits on a sequence boundary, with one exception: a control
    /// sequence the parser ignores (`ESC [ 3 ? m`) dispatches nothing, and
    /// the scan holds it, and every C0 or C1 control byte after it, until the
    /// next escape byte, printed character, `CAN`, or `SUB`. Replaying it
    /// leaves the parser on a sequence boundary.
    ///
    /// A caller carries these across a process-image swap and hands them to
    /// [`from_terminal_state`](Self::from_terminal_state). Eight-bit string controls are stored
    /// in their seven-bit `ESC` forms.
    ///
    /// Example: the chunk ends with `ESC ] 7 ; file://host/Users/yuhan/Proj` →
    /// those bytes, and the pane's reported directory is still whatever the
    /// last finished report set.
    ///
    /// Four string kinds keep only their opening bytes. The parser hands each
    /// body byte straight on or drops it, holding none of it:
    ///
    /// - a device control string — `ESC P q # 0 ; 2 ; 0` → `ESC P q`; a sixel
    ///   image of any size adds nothing here;
    /// - a start of string, `ESC X` — `ESC X hello` → `ESC X`;
    /// - a privacy message, `ESC ^` — `ESC ^ hello` → `ESC ^`;
    /// - an application program command, `ESC _` — `ESC _ G a=T,f=100;<image>`
    ///   → `ESC _`; a kitty graphics image of any size adds nothing here.
    ///
    /// An operating system command keeps its whole body; the parser reads the
    /// body and dispatches it at the terminator. Once any sequence passes
    /// 64 KiB the engine stops holding it and reports empty until it ends. A
    /// swap in that window leaves the next parser on a sequence boundary, and
    /// the rest of the body prints as text.
    pub fn undecoded_terminal_bytes(&self) -> &[u8] {
        &self.undecoded_terminal_bytes
    }

    /// The raw bytes that put the graphics parser where it stands.
    ///
    /// A caller carrying a simple process-image swap passes these bytes to
    /// [`from_terminal_state_with_graphics`](Self::from_terminal_state_with_graphics). A swap
    /// that cuts a tmux or GNU Screen wrapper, or abandons a large transfer,
    /// uses [`get_graphics_transport_state`](Self::get_graphics_transport_state).
    /// Ordinary VTE parser bytes are returned by
    /// [`undecoded_terminal_bytes`](Self::undecoded_terminal_bytes).
    pub fn undecoded_graphics_bytes(&self) -> &[u8] {
        &self.undecoded_graphics_bytes
    }

    /// The complete graphics-parser state needed by a process-image swap.
    ///
    /// Example: a split Screen wrapper returns a state with `screen_inner`;
    /// `undecoded_graphics_bytes` contains the raw bytes without wrapper state.
    pub fn get_graphics_transport_state(&self) -> Option<GraphicsTransportState> {
        self.graphics_parser.get_graphics_transport_state()
    }

    /// Return the synchronized-output bytes and remaining deadline at the monotonic timestamp.
    pub fn get_synchronized_output_transport(
        &self,
        monotonic_timestamp: Instant,
    ) -> Option<SynchronizedOutputTransport> {
        self.get_synchronized_output_transport_at(monotonic_timestamp, SystemTime::now())
    }

    fn get_synchronized_output_transport_at(
        &self,
        monotonic_timestamp: Instant,
        wall_clock_timestamp: SystemTime,
    ) -> Option<SynchronizedOutputTransport> {
        self.synchronized_output
            .build_synchronized_output_transport(
                monotonic_timestamp,
                wall_clock_timestamp,
                self.terminal_input_normalizer,
            )
    }

    /// Return the time until an open synchronized-output group must be released.
    #[must_use]
    pub fn get_next_synchronized_output_delay(
        &self,
        monotonic_timestamp: Instant,
    ) -> Option<Duration> {
        self.synchronized_output
            .compute_release_delay(monotonic_timestamp)
    }

    /// Release an expired synchronized-output group through both terminal parsers.
    #[must_use = "undelivered replies or shell facts are lost"]
    pub fn expire_synchronized_output(
        &mut self,
        monotonic_timestamp: Instant,
    ) -> Option<(Vec<u8>, Vec<ShellIntegrationFact>)> {
        let mut synchronized_output = mem::take(&mut self.synchronized_output);
        let has_advanced = synchronized_output
            .release_expired_bytes(monotonic_timestamp, |normalized_bytes| {
                self.process_normalized_terminal_bytes(normalized_bytes)
            });
        self.synchronized_output = synchronized_output;
        has_advanced.then(|| {
            (
                self.terminal_state.take_device_query_replies(),
                self.terminal_state.take_shell_integration_facts(),
            )
        })
    }

    /// Whether the next DCS belongs to an unfinished GNU Screen wrapper.
    pub fn is_graphics_screen_continuation(&self) -> bool {
        self.is_graphics_screen_continuation
    }

    /// Whether a carried GNU Screen wrapper is still open.
    pub fn is_graphics_screen_wrapper_active(&self) -> bool {
        self.is_graphics_screen_wrapper_active
    }

    /// Whether the next DCS belongs to an unfinished tmux continuation.
    pub fn is_graphics_tmux_continuation(&self) -> bool {
        self.is_graphics_tmux_continuation
    }

    /// Whether a carried tmux wrapper is still open.
    pub fn is_graphics_tmux_wrapper_active(&self) -> bool {
        self.is_graphics_tmux_wrapper_active
    }

    /// The screen model, for reads (rendering, cursor and mode queries).
    pub fn get_terminal_state(&self) -> &TerminalState {
        &self.terminal_state
    }

    /// Set the shared pixel-to-cell measurement for new image placements and queries.
    pub fn set_cell_size(&mut self, pixel_cell_size: koshi_core::geometry::PixelCellSize) {
        self.terminal_state.set_cell_size(pixel_cell_size);
    }

    /// Return the time until the next retained image animation frame is due.
    #[must_use]
    pub fn get_next_image_animation_delay(&self) -> Option<Duration> {
        self.terminal_state.get_next_image_animation_delay()
    }

    /// Advance retained image animations and report whether visible pixels changed.
    pub fn advance_image_animations(&mut self, elapsed_duration: Duration) -> bool {
        self.terminal_state
            .advance_image_animations(elapsed_duration)
    }

    /// Resize the terminal state to `pty_size` (see [`TerminalState::resize_terminal_state`]).
    ///
    /// The parser keeps any partial decode: a sequence split across the
    /// resize still completes.
    pub fn resize_terminal_state(&mut self, pty_size: PtySize) {
        self.terminal_state.resize_terminal_state(pty_size);
    }

    fn process_graphics_operation(
        &mut self,
        graphics_result: Result<crate::graphics::GraphicsOperation, GraphicsError>,
        cursor_position: (u16, u16),
    ) {
        match graphics_result {
            Ok(crate::graphics::GraphicsOperation::Failure {
                image_display,
                graphics_error,
            }) => {
                self.terminal_state
                    .reply_to_kitty_failure(&image_display, &graphics_error);
                self.enqueue_graphics_event(Err(graphics_error));
            }
            Ok(crate::graphics::GraphicsOperation::Command(graphics_command)) => {
                if let Err(placement_error) =
                    self.terminal_state.apply_kitty_command(&graphics_command)
                {
                    self.enqueue_graphics_event(Err(GraphicsError::PlacementRejected {
                        protocol: crate::graphics::GraphicsProtocol::Kitty,
                        placement_error,
                    }));
                }
            }
            Ok(crate::graphics::GraphicsOperation::Image(decoded_image)) => {
                if decoded_image.is_query {
                    self.terminal_state
                        .reply_to_kitty(&decoded_image.display, None, true);
                    return;
                }
                let image_record = ImageRecord {
                    protocol: decoded_image.protocol,
                    image: self
                        .terminal_state
                        .get_or_share_image_pixels(decoded_image.image.into()),
                    animation: decoded_image.animation.map(std::sync::Arc::new),
                    action: decoded_image.action,
                    display: decoded_image.display,
                    anchor: cursor_position,
                };
                let image_rgba_byte_count = image_record.image.rgba_bytes.len();
                let image_protocol = image_record.protocol;
                let placement_result = self.terminal_state.apply_image_record(&image_record);
                let display = if placement_result.is_ok() {
                    self.terminal_state
                        .get_kitty_reply_display(&image_record.display)
                } else {
                    image_record.display.clone()
                };
                self.terminal_state.reply_to_kitty(
                    &display,
                    placement_result.as_ref().err().copied(),
                    image_protocol == crate::graphics::GraphicsProtocol::Kitty,
                );
                let graphics_event =
                    placement_result
                        .map(|()| image_record)
                        .map_err(|placement_error| GraphicsError::PlacementRejected {
                            protocol: image_protocol,
                            placement_error,
                        });
                let queued_image_rgba_byte_count = if graphics_event.is_ok() {
                    image_rgba_byte_count
                } else {
                    0
                };
                self.enqueue_graphics_event_with_rgba_byte_count(
                    graphics_event,
                    queued_image_rgba_byte_count,
                );
            }
            Ok(crate::graphics::GraphicsOperation::Sixel(sixel_graphic)) => {
                match self
                    .terminal_state
                    .apply_sixel_graphic(sixel_graphic, cursor_position)
                {
                    Ok(Some(image_record)) => {
                        let image_rgba_byte_count = image_record.image.rgba_bytes.len();
                        self.enqueue_graphics_event_with_rgba_byte_count(
                            Ok(image_record),
                            image_rgba_byte_count,
                        );
                    }
                    Ok(None) => {}
                    Err(graphics_error) => self.enqueue_graphics_event(Err(graphics_error)),
                }
            }
            Err(graphics_error) => self.enqueue_graphics_event(Err(graphics_error)),
        }
    }

    fn enqueue_graphics_event(&mut self, graphics_event: GraphicsEvent) {
        let image_rgba_byte_count = match &graphics_event {
            Ok(image_record) => image_record.image.rgba_bytes.len(),
            Err(_) => 0,
        };
        self.enqueue_graphics_event_with_rgba_byte_count(graphics_event, image_rgba_byte_count);
    }

    fn enqueue_graphics_event_with_rgba_byte_count(
        &mut self,
        graphics_event: GraphicsEvent,
        image_rgba_byte_count: usize,
    ) {
        if self.graphics_events.len() == MAX_GRAPHICS_EVENT_COUNT
            || self
                .queued_graphics_rgba_byte_count
                .checked_add(image_rgba_byte_count)
                .is_none_or(|total_rgba_byte_count| total_rgba_byte_count > MAX_IMAGE_BYTE_COUNT)
        {
            self.dropped_graphics_event_count = self.dropped_graphics_event_count.saturating_add(1);
            if graphics_event.is_err() {
                self.dropped_graphics_error_count =
                    self.dropped_graphics_error_count.saturating_add(1);
            }
            return;
        }
        self.queued_graphics_rgba_byte_count += image_rgba_byte_count;
        self.graphics_events.push_back(graphics_event);
    }

    fn update_graphics_transport_state(&mut self) {
        self.undecoded_graphics_bytes.clear();
        if let Some(graphics_carry_bytes) = self.graphics_parser.get_graphics_carry_bytes() {
            self.undecoded_graphics_bytes
                .extend_from_slice(graphics_carry_bytes);
        }
        self.is_graphics_screen_continuation = self.graphics_parser.is_screen_continuation();
        self.is_graphics_screen_wrapper_active = self.graphics_parser.is_screen_wrapper_active();
        self.is_graphics_tmux_continuation = self.graphics_parser.is_tmux_continuation();
        self.is_graphics_tmux_wrapper_active = self.graphics_parser.is_tmux_wrapper_active();
    }

    /// Move `undecoded_parser` over `normalized_bytes` and update
    /// [`undecoded_terminal_bytes`](Self::undecoded_terminal_bytes) from where it stops.
    ///
    /// A parser leaves a sequence boundary only at [`ESCAPE_BYTE`]. The scan drops
    /// what it holds and restarts on a fresh parser at the last `ESCAPE_BYTE` in
    /// `pty_output_chunk`; the last sequence opens there and everything before it is
    /// decoded. A chunk with no `ESCAPE_BYTE` carries on from where the previous
    /// chunk stopped. A sequence spread over many chunks is read once.
    ///
    /// The scan holds at most [`MAX_UNDECODED_BYTE_COUNT`] bytes of one sequence. Past
    /// that it releases the buffer and holds nothing more until the sequence
    /// ends.
    fn capture_undecoded_terminal_bytes(&mut self, normalized_bytes: &[u8]) {
        let mut normalized_byte_index = 0;
        if let Some(last_escape_byte_index) = normalized_bytes
            .iter()
            .rposition(|normalized_byte| *normalized_byte == ESCAPE_BYTE)
        {
            self.undecoded_parser = vte::Parser::<OSC_BUFFER_BYTE_CAPACITY>::new_with_size();
            self.undecoded_terminal_bytes.clear();
            self.is_at_sequence_boundary = false;
            self.is_in_string_body = false;
            normalized_byte_index = last_escape_byte_index;
        }
        // Each round runs to the action that ends a sequence or opens the body
        // of a device control string, or to the end of the normalized bytes.
        while normalized_byte_index < normalized_bytes.len() {
            let mut probe = ActionProbe::default();
            let consumed_byte_count = self
                .undecoded_parser
                .advance_until_terminated(&mut probe, &normalized_bytes[normalized_byte_index..]);
            let stop_byte_index = normalized_byte_index + consumed_byte_count;
            if probe.is_at_sequence_boundary {
                self.undecoded_terminal_bytes.clear();
                self.is_at_sequence_boundary = true;
                self.is_in_string_body = false;
            } else if !self.is_at_sequence_boundary && !self.is_in_string_body {
                self.undecoded_terminal_bytes
                    .extend_from_slice(&normalized_bytes[normalized_byte_index..stop_byte_index]);
                if is_dropped_string_opening(&self.undecoded_terminal_bytes) {
                    // The opening alone puts another parser inside the body.
                    self.undecoded_terminal_bytes
                        .truncate(STRING_OPENING_BYTE_COUNT);
                    self.is_in_string_body = true;
                } else if self.undecoded_terminal_bytes.len() > MAX_UNDECODED_BYTE_COUNT {
                    // A fresh `Vec` frees the buffer's capacity.
                    self.undecoded_terminal_bytes = Vec::new();
                    self.is_in_string_body = true;
                } else {
                    self.is_in_string_body = probe.is_string_started;
                }
            }
            normalized_byte_index = stop_byte_index;
        }
        if self.is_at_sequence_boundary {
            // Only the first bytes of a UTF-8 code point can be left over.
            let code_point_trailing_start_index = normalized_bytes
                .len()
                .saturating_sub(CODE_POINT_TAIL_BYTE_COUNT);
            self.undecoded_terminal_bytes
                .extend_from_slice(&normalized_bytes[code_point_trailing_start_index..]);
            let incomplete_byte_count =
                find_incomplete_utf8_code_point_bytes(&self.undecoded_terminal_bytes).len();
            let decoded_byte_count = self.undecoded_terminal_bytes.len() - incomplete_byte_count;
            self.undecoded_terminal_bytes.drain(..decoded_byte_count);
        }
    }
}

/// A [`vte::Perform`] that drops every action. Replaying carried bytes through
/// it moves a parser without touching a screen.
struct NoScreen;

impl vte::Perform for NoScreen {}

/// A [`vte::Perform`] that records two things: whether an action put the
/// parser back on a sequence boundary, and whether the parser opened a device
/// control string. It touches no screen.
///
/// Every action listed here leaves the parser on a sequence boundary, as long
/// as the bytes scanned hold no [`ESCAPE_BYTE`] past their first byte: `ESCAPE_BYTE`
/// alone ends an operating system command or a device control string into the
/// next sequence instead of into the ground state.
#[derive(Default)]
struct ActionProbe {
    /// Set when an action put the parser back on a sequence boundary.
    is_at_sequence_boundary: bool,
    /// Set when the parser opened a device control string.
    is_string_started: bool,
}

impl vte::Perform for ActionProbe {
    fn print(&mut self, _c: char) {
        self.is_at_sequence_boundary = true;
    }

    fn execute(&mut self, control_byte: u8) {
        if control_byte == CANCEL_BYTE || control_byte == SUBSTITUTE_BYTE {
            self.is_at_sequence_boundary = true;
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.is_string_started = true;
    }

    fn unhook(&mut self) {
        self.is_at_sequence_boundary = true;
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.is_at_sequence_boundary = true;
    }

    fn csi_dispatch(
        &mut self,
        _params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
        self.is_at_sequence_boundary = true;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.is_at_sequence_boundary = true;
    }

    /// Stops [`vte::Parser::advance_until_terminated`] at each sequence
    /// boundary and at the start of a device control string body.
    fn terminated(&self) -> bool {
        self.is_at_sequence_boundary || self.is_string_started
    }
}

/// True when `input_bytes` opens a start of string, a privacy message or an
/// application program command: `ESC X`, `ESC ^` or `ESC _`. The parser reads
/// the body of each one and keeps none of it.
fn is_dropped_string_opening(input_bytes: &[u8]) -> bool {
    matches!(
        input_bytes,
        [
            ESCAPE_BYTE,
            START_OF_STRING_BYTE | PRIVACY_MESSAGE_BYTE | APPLICATION_COMMAND_BYTE,
            ..
        ]
    )
}

/// The bytes at the end of `input_bytes` that begin a UTF-8 code point without
/// completing it, or empty when the last code point is whole.
fn find_incomplete_utf8_code_point_bytes(input_bytes: &[u8]) -> &[u8] {
    let mut incomplete_bytes =
        &input_bytes[input_bytes.len().saturating_sub(CODE_POINT_TAIL_BYTE_COUNT)..];
    loop {
        let Err(utf8_error) = std::str::from_utf8(incomplete_bytes) else {
            return &[];
        };
        let Some(invalid_byte_count) = utf8_error.error_len() else {
            return &incomplete_bytes[utf8_error.valid_up_to()..];
        };
        incomplete_bytes = &incomplete_bytes[utf8_error.valid_up_to() + invalid_byte_count..];
    }
}

#[cfg(test)]
mod tests;
