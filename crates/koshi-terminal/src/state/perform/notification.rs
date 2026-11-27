//! OSC parsing for the desktop notifications a pane's program sends: OSC 9,
//! OSC 777 `notify`, and the parts of an OSC 99 notification.
//!
//! Every title and body passes [`sanitize_reported_text`], which keeps at
//! most [`MAX_REPORTED_TEXT_BYTE_COUNT`](koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT) bytes, and a title that is empty
//! after it is `None`.

use base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT;
use base64::Engine as _;
use koshi_core::text::sanitize_reported_text;

use crate::state::ProgramFact;

/// The OSC 9 first parameters that name a ConEmu subcommand. An OSC 9 whose
/// first parameter is one of them is not a notification: `OSC 9 ; 4 ; 1 ; 40`
/// is a progress report.
const CONEMU_OSC9_SUBCOMMAND_NUMBERS: [&[u8]; 12] = [
    b"1", b"2", b"3", b"4", b"5", b"6", b"7", b"8", b"9", b"10", b"11", b"12",
];

/// The most bytes an OSC 99 `i` key keeps once its refused bytes are removed.
const MAX_OSC99_NOTIFICATION_ID_BYTE_COUNT: usize = 512;

/// An OSC 99 notification whose parts are still arriving: the last part it
/// took carried `d=0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Osc99Notification {
    /// The notification's `i` key, or `None` when its parts carry none.
    notification_id: Option<String>,
    /// The title parts gathered so far, joined, sanitized and cut to
    /// [`MAX_REPORTED_TEXT_BYTE_COUNT`](koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT) bytes.
    title: String,
    /// The body parts gathered so far, joined, sanitized and cut to
    /// [`MAX_REPORTED_TEXT_BYTE_COUNT`](koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT) bytes.
    body: String,
}

/// What the `p` key of an OSC 99 sequence says its payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Osc99PayloadType {
    /// `p=title`, and a sequence with no `p` key.
    Title,
    /// `p=body`.
    Body,
    /// `p=close`: the program withdraws the notification its `i` key names.
    Close,
    /// `p=?` and `p=alive`: a question to the terminal, which is not part of
    /// a notification.
    Query,
    /// `p=icon`, `p=buttons` and every other value: a part of a notification
    /// that carries no title or body text.
    UntextedPart,
}

/// The metadata keys of one OSC 99 sequence that koshi reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Osc99Metadata {
    /// The `i` key with every byte outside `[a-zA-Z0-9_+.-]` removed, cut to
    /// [`MAX_OSC99_NOTIFICATION_ID_BYTE_COUNT`] bytes. `None` when the key is
    /// absent or nothing is left.
    notification_id: Option<String>,
    /// The `p` key.
    payload_type: Osc99PayloadType,
    /// `false` for `d=0`, and `true` for any other `d` and for no `d`.
    is_done: bool,
    /// `true` for `e=1`: the payload is base64.
    is_base64: bool,
}

/// The notification an OSC 9 sequence carries, or `None` when it carries
/// none. `osc_parameters` is the sequence split on `;`, starting with `9`.
///
/// The text after `9 ;` is the body, with its `;` kept. A sequence with no
/// parameter after `9`, or whose first one is a ConEmu subcommand number
/// from `1` to `12`, carries none:
///
/// - `9 ; hello` → `Notify { title: None, body: "hello", notification_id: None }`
/// - `9 ; a ; b` → body `"a;b"`
/// - `9 ; 4 ; 1 ; 40` and `9 ; 12` → `None`
pub(super) fn parse_osc9_notification(osc_parameters: &[&[u8]]) -> Option<ProgramFact> {
    let [_, first_parameter, ..] = osc_parameters else {
        return None;
    };
    if CONEMU_OSC9_SUBCOMMAND_NUMBERS.contains(first_parameter) {
        return None;
    }
    Some(ProgramFact::Notify {
        title: None,
        body: sanitize_notification_text(&osc_parameters[1..].join(&b';')),
        notification_id: None,
    })
}

/// The notification an OSC 777 sequence carries, or `None` when its
/// subcommand is not `notify`. `osc_parameters` is the sequence split on
/// `;`, starting with `777`.
///
/// The parameter after `notify` is the title, and the text after the next
/// `;` is the body, with its `;` kept:
///
/// - `777 ; notify ; Build ; done ; 3 s` → `Notify { title: Some("Build"),
///   body: "done;3 s", notification_id: None }`
/// - `777 ; notify` → `Notify { title: None, body: "", notification_id: None }`
/// - `777 ; preexec ; ls` → `None`
pub(super) fn parse_osc777_notification(osc_parameters: &[&[u8]]) -> Option<ProgramFact> {
    let [_, b"notify", notify_parameters @ ..] = osc_parameters else {
        return None;
    };
    let title_text = match notify_parameters.first() {
        Some(title_bytes) => sanitize_notification_text(title_bytes),
        None => String::new(),
    };
    let body_parameters = notify_parameters.get(1..).unwrap_or(&[]);
    Some(ProgramFact::Notify {
        title: build_notification_title(title_text),
        body: sanitize_notification_text(&body_parameters.join(&b';')),
        notification_id: None,
    })
}

/// Apply one OSC 99 sequence to the pane's open notification, and return the
/// fact it completes. `osc_parameters` is the sequence split on `;`,
/// starting with `99`: the metadata, then the payload, whose own `;` are
/// kept. A sequence with no payload has an empty one.
///
/// - `p=?` and `p=alive` return `None` and leave `open_osc99_notification`
///   as it is.
/// - `p=close` returns `NotificationWithdrawn` naming its `i`, or `None`
///   with no `i`, and leaves `open_osc99_notification` as it is.
/// - Any other `p` is a part. It joins the open notification when that one
///   has the same `i`, or no `i` when the part has none; otherwise the part
///   starts a new notification and the open one is dropped. A `title` or
///   `body` part appends its payload, decoded from base64 under `e=1`, to that
///   field. A base64 payload that does not decode appends nothing.
/// - A part with `d=0` leaves its notification open and returns `None`.
///   Any other part ends it: `open_osc99_notification` becomes `None`, and
///   it returns `Notify`, or `None` when the title and the body are both
///   empty.
///
/// `i=1:d=0 ; Hello`, then `i=1:p=body ; This is cool` →
/// `Notify { title: Some("Hello"), body: "This is cool", notification_id: Some("1") }`.
pub(super) fn apply_osc99_sequence(
    open_osc99_notification: &mut Option<Osc99Notification>,
    osc_parameters: &[&[u8]],
) -> Option<ProgramFact> {
    let [_, metadata_bytes, payload_parameters @ ..] = osc_parameters else {
        return None;
    };
    let osc99_metadata = parse_osc99_metadata(metadata_bytes);
    match osc99_metadata.payload_type {
        Osc99PayloadType::Query => return None,
        Osc99PayloadType::Close => {
            return osc99_metadata
                .notification_id
                .map(|notification_id| ProgramFact::NotificationWithdrawn { notification_id });
        }
        Osc99PayloadType::Title | Osc99PayloadType::Body | Osc99PayloadType::UntextedPart => {}
    }
    let mut osc99_notification = match open_osc99_notification.take() {
        Some(open_notification)
            if open_notification.notification_id == osc99_metadata.notification_id =>
        {
            open_notification
        }
        _ => Osc99Notification {
            notification_id: osc99_metadata.notification_id,
            title: String::new(),
            body: String::new(),
        },
    };
    let text_field = match osc99_metadata.payload_type {
        Osc99PayloadType::Title => Some(&mut osc99_notification.title),
        Osc99PayloadType::Body => Some(&mut osc99_notification.body),
        _ => None,
    };
    if let Some(text_field) = text_field {
        let payload_bytes = payload_parameters.join(&b';');
        if let Some(payload_text) = decode_osc99_payload(&payload_bytes, osc99_metadata.is_base64) {
            let joined_text = format!("{text_field}{payload_text}");
            *text_field = sanitize_reported_text(&joined_text);
        }
    }
    if !osc99_metadata.is_done {
        *open_osc99_notification = Some(osc99_notification);
        return None;
    }
    if osc99_notification.title.is_empty() && osc99_notification.body.is_empty() {
        return None;
    }
    Some(ProgramFact::Notify {
        title: build_notification_title(osc99_notification.title),
        body: osc99_notification.body,
        notification_id: osc99_notification.notification_id,
    })
}

/// Read the `i`, `p`, `d` and `e` keys of OSC 99 metadata: `:`-separated
/// `key=value` pairs. An entry with no `=` and every other key are skipped,
/// and a repeated key keeps its last value.
///
/// `i=build:d=0:p=body` → `notification_id: Some("build")`, `Body`,
/// `is_done: false`, `is_base64: false`.
fn parse_osc99_metadata(metadata_bytes: &[u8]) -> Osc99Metadata {
    let mut osc99_metadata = Osc99Metadata {
        notification_id: None,
        payload_type: Osc99PayloadType::Title,
        is_done: true,
        is_base64: false,
    };
    for metadata_entry in metadata_bytes.split(|metadata_byte| *metadata_byte == b':') {
        let Some(separator_index) = metadata_entry
            .iter()
            .position(|metadata_byte| *metadata_byte == b'=')
        else {
            continue;
        };
        let metadata_value = &metadata_entry[separator_index + 1..];
        match &metadata_entry[..separator_index] {
            b"i" => osc99_metadata.notification_id = sanitize_osc99_notification_id(metadata_value),
            b"p" => osc99_metadata.payload_type = parse_osc99_payload_type(metadata_value),
            b"d" => osc99_metadata.is_done = metadata_value != b"0",
            b"e" => osc99_metadata.is_base64 = metadata_value == b"1",
            _ => {}
        }
    }
    osc99_metadata
}

/// The payload type a `p` value names: `title`, `body`, `close`, `?` and
/// `alive` by name, and [`Osc99PayloadType::UntextedPart`] for any other.
fn parse_osc99_payload_type(payload_type_bytes: &[u8]) -> Osc99PayloadType {
    match payload_type_bytes {
        b"title" => Osc99PayloadType::Title,
        b"body" => Osc99PayloadType::Body,
        b"close" => Osc99PayloadType::Close,
        b"?" | b"alive" => Osc99PayloadType::Query,
        _ => Osc99PayloadType::UntextedPart,
    }
}

/// `raw_notification_id` with every byte outside `[a-zA-Z0-9_+.-]` removed,
/// cut to [`MAX_OSC99_NOTIFICATION_ID_BYTE_COUNT`] bytes, or `None` when
/// nothing is left. `ab;c` → `Some("abc")`, and `%%` → `None`.
fn sanitize_osc99_notification_id(raw_notification_id: &[u8]) -> Option<String> {
    let notification_id: String = raw_notification_id
        .iter()
        .filter(|id_byte| {
            id_byte.is_ascii_alphanumeric() || matches!(id_byte, b'_' | b'+' | b'.' | b'-')
        })
        .take(MAX_OSC99_NOTIFICATION_ID_BYTE_COUNT)
        .map(|id_byte| char::from(*id_byte))
        .collect();
    if notification_id.is_empty() {
        return None;
    }
    Some(notification_id)
}

/// The text an OSC 99 payload carries: the bytes themselves, or, when
/// `is_base64`, the bytes they decode to, with or without `=` padding. Bytes
/// that are not UTF-8 become U+FFFD. `None` when `is_base64` and the payload
/// does not decode.
fn decode_osc99_payload(payload_bytes: &[u8], is_base64: bool) -> Option<String> {
    if !is_base64 {
        return Some(String::from_utf8_lossy(payload_bytes).into_owned());
    }
    let decoded_bytes = STANDARD_PAD_INDIFFERENT.decode(payload_bytes).ok()?;
    Some(String::from_utf8_lossy(&decoded_bytes).into_owned())
}

/// `text_bytes` decoded lossily, then passed through [`sanitize_reported_text`].
fn sanitize_notification_text(text_bytes: &[u8]) -> String {
    sanitize_reported_text(&String::from_utf8_lossy(text_bytes))
}

/// `Some(sanitized_title)`, or `None` when it is empty.
fn build_notification_title(sanitized_title: String) -> Option<String> {
    if sanitized_title.is_empty() {
        return None;
    }
    Some(sanitized_title)
}

#[cfg(test)]
mod tests;
