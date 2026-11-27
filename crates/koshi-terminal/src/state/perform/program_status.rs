//! OSC 7501, the Program Status Protocol, revision 0.4: the reports in which a
//! pane's program says what it is doing, and the query that asks whether the
//! terminal reads them.
//!
//! A report is `OSC 7501 ; key=value(:key=value)* ST`, and ST is `ESC \` or
//! BEL. The engine's OSC reader hands over every byte after `ESC ] 7501 ;` as
//! the program wrote it, control bytes too, and only for a sequence that ended
//! with `ESC \` or BEL. [`TerminalState::apply_program_status_sequence`]
//! answers the query `?`, and passes every other text to
//! [`parse_program_status_report`], which checks the report against every limit
//! of the protocol and gives one [`ProgramStatusFact`] or nothing.
//!
//! - A pair with no `=`, an empty key, an unknown key, or a value byte
//!   outside `A-Z a-z 0-9 _ . , + / = -` is skipped, and the rest of the
//!   report applies. The bytes space, tab, LF, FF and CR around a key and a
//!   value are removed. The last value of a repeated key wins.
//! - The whole report gives nothing when its state is missing or unknown, or
//!   when one pair breaks a limit, also a pair that a following pair of the
//!   same key replaces.
//!
//! Example: `ESC ] 7501 ; state=blocked:kind=permission:app=terraform:msg=QXBwbHk/ ESC \`
//! gives a `Reported` fact with the state `Blocked`, the kind `Permission`,
//! the app name `terraform` and the message `Apply?`.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
use koshi_core::program_status::{BlockedKind, ProgramStatusState};
use koshi_core::text::sanitize_reported_text;

use crate::engine::osc_reader::PROGRAM_STATUS_INTRODUCER_BYTES;
use crate::state::{ProgramFact, ProgramStatusFact, TerminalState};

/// The text of the query `OSC 7501 ; ? ST`.
const PROGRAM_STATUS_QUERY_BYTES: &[u8] = b"?";

/// The reply to the query, without its terminator. The reply ends with the
/// terminator that the query used: BEL or `ESC \`.
const PROGRAM_STATUS_QUERY_REPLY_BYTES: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth";

/// The most bytes one report holds, from `ESC ]` through its terminator. An
/// 8-bit sequence counts as its 7-bit form: `0x9D` as the 2 bytes `ESC ]`,
/// and `0x9C` as the 2 bytes `ESC \`.
const MAX_REPORT_BYTE_COUNT: usize = 4096;

/// The most bytes one key holds, a known key or an unknown one.
const MAX_KEY_BYTE_COUNT: usize = 16;

/// The most bytes one name holds: an `app` value, or one part of an `id`.
const MAX_NAME_BYTE_COUNT: usize = 32;

/// The most bytes one `id` holds.
const MAX_RECORD_ID_BYTE_COUNT: usize = 128;

/// The most parts one `id` holds.
const MAX_RECORD_ID_PART_COUNT: usize = 8;

/// The most bytes one `title` value holds in base64.
const MAX_ENCODED_TITLE_BYTE_COUNT: usize = 256;

/// The most bytes one `title` value decodes to.
const MAX_DECODED_TITLE_BYTE_COUNT: usize = 192;

/// The most bytes one `msg` value holds in base64.
const MAX_ENCODED_MESSAGE_BYTE_COUNT: usize = 2732;

/// The most bytes one `msg` value decodes to.
const MAX_DECODED_MESSAGE_BYTE_COUNT: usize = 2048;

/// The byte between two pairs of a report: `:`.
const PAIR_SEPARATOR_BYTE: u8 = b':';

/// The byte between the key and the value of a pair: `=`.
const KEY_VALUE_SEPARATOR_BYTE: u8 = b'=';

/// The byte between two parts of an `id`: `/`.
const RECORD_ID_PART_SEPARATOR_BYTE: u8 = b'/';

/// The last value of each key of one report, out of the pairs whose value
/// holds value bytes only. `title` and `message` hold the decoded text.
#[derive(Default)]
struct ReportPairs<'report> {
    /// The `state` value, such as `working`.
    program_status_state_bytes: Option<&'report [u8]>,
    /// The `id` value, such as `deploy/us-east`.
    record_id_bytes: Option<&'report [u8]>,
    /// The `kind` value, such as `permission`.
    blocked_kind_bytes: Option<&'report [u8]>,
    /// The `progress` value, such as `40`.
    progress_bytes: Option<&'report [u8]>,
    /// The `app` value, such as `cargo`.
    app_name_bytes: Option<&'report [u8]>,
    /// The `title` value, decoded and passed through
    /// [`sanitize_reported_text`].
    title: Option<String>,
    /// The `msg` value, decoded and passed through
    /// [`sanitize_reported_text`].
    message: Option<String>,
}

/// The fact that `report_bytes`, the text of one `OSC 7501` after its first
/// `;`, gives, or `None` when the report breaks a limit. The report ended with
/// BEL when `is_bell_terminated`, and with `ESC \` otherwise.
///
/// The limits that give `None`:
///
/// - the sequence holds more than 4096 bytes, from `ESC ]` through its
///   terminator, counted in its 7-bit form;
/// - the state is missing, or is not `idle`, `working`, `done`, `blocked`,
///   `error` or `clear`;
/// - a key holds more than 16 bytes;
/// - an `app` holds more than 32 bytes;
/// - an `id` holds more than 128 bytes or more than 8 parts, or a part that is
///   not 1 to 32 bytes of `A-Z a-z 0-9 _ . + -`: `a//b`, `a/`, `/a`, `a b`;
/// - a `title` holds more than 256 bytes, or a `msg` more than 2732 bytes;
/// - a `title` or a `msg` is not base64, decodes to more than 192 or 2048
///   bytes, to bytes that are not UTF-8, or to a control character (U+0000 to
///   U+001F, U+007F, U+0080 to U+009F).
///
/// `state=clear:id=deploy` gives `Cleared { record_id: "deploy" }`, and
/// `state=clear:id=a//b` and `state=clear:id=a<TAB>b` give `None`.
fn parse_program_status_report(
    report_bytes: &[u8],
    is_bell_terminated: bool,
) -> Option<ProgramStatusFact> {
    let terminator_byte_count = if is_bell_terminated { 1 } else { 2 };
    if PROGRAM_STATUS_INTRODUCER_BYTES.len() + report_bytes.len() + terminator_byte_count
        > MAX_REPORT_BYTE_COUNT
    {
        return None;
    }
    let report_pairs = read_report_pairs(report_bytes)?;
    build_program_status_fact(report_pairs)
}

/// The last value of each key in `report_bytes`, or `None` when one pair
/// breaks a limit of its key. Every pair is checked, also a pair that a following
/// pair of the same key replaces.
fn read_report_pairs(report_bytes: &[u8]) -> Option<ReportPairs<'_>> {
    let mut report_pairs = ReportPairs::default();
    for pair_bytes in report_bytes.split(|report_byte| *report_byte == PAIR_SEPARATOR_BYTE) {
        let Some(key_value_separator_index) = pair_bytes
            .iter()
            .position(|pair_byte| *pair_byte == KEY_VALUE_SEPARATOR_BYTE)
        else {
            continue;
        };
        let key_bytes = pair_bytes[..key_value_separator_index].trim_ascii();
        let value_bytes = pair_bytes[key_value_separator_index + 1..].trim_ascii();
        if key_bytes.len() > MAX_KEY_BYTE_COUNT || should_discard_report(key_bytes, value_bytes) {
            return None;
        }
        if !value_bytes
            .iter()
            .all(|value_byte| is_value_byte(*value_byte))
        {
            continue;
        }
        match key_bytes {
            b"state" => report_pairs.program_status_state_bytes = Some(value_bytes),
            b"id" => report_pairs.record_id_bytes = Some(value_bytes),
            b"kind" => report_pairs.blocked_kind_bytes = Some(value_bytes),
            b"progress" => report_pairs.progress_bytes = Some(value_bytes),
            b"app" => report_pairs.app_name_bytes = Some(value_bytes),
            b"title" => {
                report_pairs.title = Some(decode_program_status_text(
                    value_bytes,
                    MAX_DECODED_TITLE_BYTE_COUNT,
                )?);
            }
            b"msg" => {
                report_pairs.message = Some(decode_program_status_text(
                    value_bytes,
                    MAX_DECODED_MESSAGE_BYTE_COUNT,
                )?);
            }
            _ => {}
        }
    }
    Some(report_pairs)
}

/// Whether `value_bytes`, the value of the key `key_bytes`, breaks a limit
/// that discards the whole report: an `id` that [`is_record_id_valid`]
/// refuses, an `app` over 32 bytes, a `title` over 256 bytes, or a `msg`
/// over 2732 bytes. A value outside the value bytes counts too: `id=a b`
/// discards the report.
fn should_discard_report(key_bytes: &[u8], value_bytes: &[u8]) -> bool {
    match key_bytes {
        b"id" => !is_record_id_valid(value_bytes),
        b"app" => value_bytes.len() > MAX_NAME_BYTE_COUNT,
        b"title" => value_bytes.len() > MAX_ENCODED_TITLE_BYTE_COUNT,
        b"msg" => value_bytes.len() > MAX_ENCODED_MESSAGE_BYTE_COUNT,
        _ => false,
    }
}

/// The fact that the last values in `report_pairs` give, or `None` when
/// the state is missing or unknown.
///
/// `kind` is read for `Blocked` only, and `progress` for `Working` and
/// `Blocked` only. An unknown `kind`, a `progress` that is not a whole
/// number from 0 to 100, an `app` that is not a name, and a `title` or a
/// `msg` that is empty read as `None`.
fn build_program_status_fact(report_pairs: ReportPairs<'_>) -> Option<ProgramStatusFact> {
    let record_id =
        String::from_utf8_lossy(report_pairs.record_id_bytes.unwrap_or_default()).into_owned();
    let program_status_state = match report_pairs.program_status_state_bytes? {
        b"idle" => ProgramStatusState::Idle,
        b"working" => ProgramStatusState::Working,
        b"done" => ProgramStatusState::Done,
        b"blocked" => ProgramStatusState::Blocked,
        b"error" => ProgramStatusState::Error,
        b"clear" => return Some(ProgramStatusFact::Cleared { record_id }),
        _ => return None,
    };
    let blocked_kind = match (program_status_state, report_pairs.blocked_kind_bytes) {
        (ProgramStatusState::Blocked, Some(b"permission")) => Some(BlockedKind::Permission),
        (ProgramStatusState::Blocked, Some(b"question")) => Some(BlockedKind::Question),
        (ProgramStatusState::Blocked, Some(b"auth")) => Some(BlockedKind::Auth),
        _ => None,
    };
    let progress_percent = match program_status_state {
        ProgramStatusState::Working | ProgramStatusState::Blocked => {
            report_pairs.progress_bytes.and_then(parse_progress_percent)
        }
        ProgramStatusState::Idle | ProgramStatusState::Done | ProgramStatusState::Error => None,
    };
    let app_name = report_pairs
        .app_name_bytes
        .filter(|app_name_bytes| is_name(app_name_bytes))
        .map(|app_name_bytes| String::from_utf8_lossy(app_name_bytes).into_owned());
    Some(ProgramStatusFact::Reported {
        record_id,
        program_status_state,
        blocked_kind,
        progress_percent,
        app_name,
        title: report_pairs.title.filter(|title| !title.is_empty()),
        message: report_pairs.message.filter(|message| !message.is_empty()),
    })
}

/// Whether `record_id_bytes` is an `id`: at most 128 bytes of at most 8
/// names joined by single `/`. `deploy/us-east` is one, and ``, `a//b`,
/// `a/`, `/a` and `a b` are not.
fn is_record_id_valid(record_id_bytes: &[u8]) -> bool {
    record_id_bytes.len() <= MAX_RECORD_ID_BYTE_COUNT
        && record_id_bytes
            .split(|record_id_byte| *record_id_byte == RECORD_ID_PART_SEPARATOR_BYTE)
            .enumerate()
            .all(|(record_id_part_index, record_id_part_bytes)| {
                record_id_part_index < MAX_RECORD_ID_PART_COUNT && is_name(record_id_part_bytes)
            })
}

/// Whether `name_bytes` is a name: 1 to 32 bytes of `A-Z a-z 0-9 _ . + -`.
/// `cargo` and `claude-code` are names, and `a/b` and `` are not.
fn is_name(name_bytes: &[u8]) -> bool {
    (1..=MAX_NAME_BYTE_COUNT).contains(&name_bytes.len())
        && name_bytes.iter().all(|name_byte| is_name_byte(*name_byte))
}

/// Whether `name_byte` may stand in a name: `A-Z a-z 0-9 _ . + -`.
fn is_name_byte(name_byte: u8) -> bool {
    name_byte.is_ascii_alphanumeric() || matches!(name_byte, b'_' | b'.' | b'+' | b'-')
}

/// Whether `value_byte` may stand in a value: `A-Z a-z 0-9 _ . , + / = -`.
fn is_value_byte(value_byte: u8) -> bool {
    is_name_byte(value_byte) || matches!(value_byte, b',' | b'/' | b'=')
}

/// The whole number from 0 to 100 that `progress_bytes` writes in decimal
/// digits, or `None` for every other value: `40` gives `40`, `007` gives
/// `7`, and `101`, `+1`, `4.5` and an empty value give `None`.
fn parse_progress_percent(progress_bytes: &[u8]) -> Option<u8> {
    if !progress_bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let progress_percent: u8 = String::from_utf8_lossy(progress_bytes).parse().ok()?;
    (progress_percent <= 100).then_some(progress_percent)
}

/// The text of a `title` or `msg` value: `encoded_text_bytes` decoded from
/// base64, then passed through [`sanitize_reported_text`], which keeps at
/// most 512 bytes. `None` when the value is not base64, or decodes to more
/// than `max_decoded_byte_count` bytes, to bytes that are not UTF-8, or to a
/// control character (U+0000 to U+001F, U+007F, U+0080 to U+009F).
///
/// A value that ends with `=` decodes with complete padding, and any other
/// value with no padding: `QQ==` and `QQ` give `A`, and `QQ=` gives `None`.
fn decode_program_status_text(
    encoded_text_bytes: &[u8],
    max_decoded_byte_count: usize,
) -> Option<String> {
    let base64_engine = if encoded_text_bytes.ends_with(b"=") {
        STANDARD
    } else {
        STANDARD_NO_PAD
    };
    let decoded_text_bytes = base64_engine.decode(encoded_text_bytes).ok()?;
    if decoded_text_bytes.len() > max_decoded_byte_count {
        return None;
    }
    let decoded_text = String::from_utf8(decoded_text_bytes).ok()?;
    if decoded_text.chars().any(char::is_control) {
        return None;
    }
    Some(sanitize_reported_text(&decoded_text))
}

impl TerminalState {
    /// Apply the program status sequence `ESC ] 7501 ; <argument_bytes>`, which
    /// ended with BEL when `is_bell_terminated` and with `ESC \` otherwise.
    ///
    /// - `argument_bytes` of `?` queues the query reply on the device reply
    ///   queue, ended with the sequence's own terminator.
    /// - Any other `argument_bytes` that [`parse_program_status_report`]
    ///   accepts adds its [`ProgramFact::ProgramStatus`] fact, and writes no
    ///   reply.
    ///
    /// `?` with `is_bell_terminated` queues
    /// `ESC ] 7501 ; ?:states=idle,working,done,blocked,error:kinds=permission,question,auth BEL`.
    pub(crate) fn apply_program_status_sequence(
        &mut self,
        argument_bytes: &[u8],
        is_bell_terminated: bool,
    ) {
        if argument_bytes == PROGRAM_STATUS_QUERY_BYTES {
            let query_terminator_bytes: &[u8] = if is_bell_terminated {
                b"\x07"
            } else {
                b"\x1b\\"
            };
            self.device_query_replies
                .extend_from_slice(PROGRAM_STATUS_QUERY_REPLY_BYTES);
            self.device_query_replies
                .extend_from_slice(query_terminator_bytes);
            return;
        }
        if let Some(program_status_fact) =
            parse_program_status_report(argument_bytes, is_bell_terminated)
        {
            self.push_program_fact(ProgramFact::ProgramStatus(program_status_fact));
        }
    }
}

#[cfg(test)]
mod tests;
