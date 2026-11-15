//! What a session records about a client that is not its view state: where it
//! connected from, the label people know it by, and the number it prints as.

use serde::{Deserialize, Serialize};

use crate::display_width::compute_text_display_width;
use crate::naming::{generate_name, NameKind};
use crate::text::{is_refused_character, MAX_REPORTED_TEXT_BYTE_COUNT};

/// Where a client connected from. The session sets it at attach from the
/// connection: the account the OS reports for the connection, and whether a
/// Hello that presented the session owner's token named a caller on another
/// machine.
///
/// The type has no `Default`: a value that is absent is not
/// [`Local`](ClientOrigin::Local). Carry an absent origin as
/// `Option<ClientOrigin>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientOrigin {
    /// Connected over this machine's own unix socket or named pipe.
    Local {
        /// `true` when the client process runs as the user who started this
        /// koshi, the host account. `false` for another account of this
        /// machine.
        is_same_user: bool,
    },
    /// Connected from another machine.
    Remote,
}

/// The label of a client of the host account: `host`.
pub const HOST_CLIENT_LABEL: &str = "host";

/// The most terminal cells a client label takes: `32`.
pub const MAX_CLIENT_LABEL_CELL_COUNT: usize = 32;

/// `raw_label` as a client label:
///
/// 1. Each run of whitespace ([`char::is_whitespace`]: space, tab, newline,
///    `U+0085`, `U+2028` and the rest) becomes one space.
/// 2. Every character [`sanitize_reported_text`](crate::text::sanitize_reported_text)
///    removes is removed: control, bidi-control, noncharacter, interlinear
///    annotation and tag characters.
/// 3. Leading and trailing spaces are removed.
/// 4. The label is cut, on a whole character, to at most
///    [`MAX_CLIENT_LABEL_CELL_COUNT`] cells, measured with
///    [`compute_text_display_width`], and at most
///    [`MAX_REPORTED_TEXT_BYTE_COUNT`] bytes. A space the cut leaves at the
///    end is removed.
///
/// The result is empty when nothing is left.
///
/// - `"Bob\tSmith\n"` → `"Bob Smith"`
/// - `"\u{1b}[31mbob"` → `"[31mbob"`
/// - `"ali\u{202E}nimda"` → `"alinimda"`
/// - `"a \u{202E} b"` → `"a b"`
/// - 40 `a` → 32 `a`; 20 `日` → 16 `日`; 31 `a` then `日` → 31 `a`
/// - `"   "` → `""`
#[must_use]
pub fn sanitize_client_label(raw_label: &str) -> String {
    let mut folded_label = String::new();
    let mut is_space_pending = false;
    for character in raw_label.chars() {
        if folded_label.len() > MAX_REPORTED_TEXT_BYTE_COUNT {
            break;
        }
        if character.is_whitespace() {
            is_space_pending = !folded_label.is_empty();
            continue;
        }
        if is_refused_character(character) {
            continue;
        }
        if is_space_pending {
            folded_label.push(' ');
            is_space_pending = false;
        }
        folded_label.push(character);
    }
    cut_client_label(
        &folded_label,
        MAX_CLIENT_LABEL_CELL_COUNT,
        MAX_REPORTED_TEXT_BYTE_COUNT,
    )
    .to_owned()
}

/// The longest start of `label` that takes at most `max_cell_count` cells,
/// measured with [`compute_text_display_width`], and at most `max_byte_count`
/// bytes, ending on a whole character, with its trailing spaces removed.
///
/// `"bob smith"` cut to 4 cells → `"bob"`; `"日日"` cut to 3 cells → `"日"`.
fn cut_client_label(label: &str, max_cell_count: usize, max_byte_count: usize) -> &str {
    let mut kept_byte_count = 0;
    for (character_start, character) in label.char_indices() {
        let character_end = character_start + character.len_utf8();
        if character_end > max_byte_count
            || compute_text_display_width(&label[..character_end]) > max_cell_count
        {
            break;
        }
        kept_byte_count = character_end;
    }
    label[..kept_byte_count].trim_end_matches(' ')
}

/// The label a client takes when it is registered, given `client_origin`, the
/// grant identity its connection carried, and `is_label_held`, which reports a
/// label another client of the session already holds.
///
/// The base label is:
///
/// - for [`ClientOrigin::Remote`] with a `granted_identity`,
///   [`sanitize_client_label`] of that identity;
/// - for [`ClientOrigin::Local`] with `is_same_user: true`,
///   [`HOST_CLIENT_LABEL`];
/// - for every other client, none. `granted_identity` is read for a remote
///   client only.
///
/// A client with no base label, or with a base label that is empty, takes a
/// generated `C-<adjective>-<noun>` that is not held ([`generate_name`]). A
/// base label that is not held is the label. A held base label takes the first
/// suffix of `-2`, `-3`, … that gives a label that is not held. The base is cut
/// first ([`MAX_CLIENT_LABEL_CELL_COUNT`] and [`MAX_REPORTED_TEXT_BYTE_COUNT`]
/// less the suffix), so base and suffix fit both limits.
///
/// - remote `"bob"`, nothing held → `bob`; `bob` held → `bob-2`; `bob` and
///   `bob-2` held → `bob-3`
/// - local host account, `host` held → `host-2`
/// - remote 16 `日` (32 cells), held → 15 `日` then `-2`
/// - local other account, or remote `"   "` → `C-swift-otter`
#[must_use]
pub fn resolve_client_label(
    client_origin: ClientOrigin,
    granted_identity: Option<&str>,
    is_label_held: impl Fn(&str) -> bool,
) -> String {
    let base_label = match (client_origin, granted_identity) {
        (ClientOrigin::Remote, Some(granted_identity)) => sanitize_client_label(granted_identity),
        (ClientOrigin::Local { is_same_user: true }, _) => HOST_CLIENT_LABEL.to_owned(),
        (ClientOrigin::Remote, None)
        | (
            ClientOrigin::Local {
                is_same_user: false,
            },
            _,
        ) => String::new(),
    };
    if base_label.is_empty() {
        return generate_name(NameKind::Client, is_label_held);
    }
    if !is_label_held(&base_label) {
        return base_label;
    }
    let mut suffix_number: u64 = 2;
    loop {
        let suffix = format!("-{suffix_number}");
        let cut_base_label = cut_client_label(
            &base_label,
            MAX_CLIENT_LABEL_CELL_COUNT - suffix.len(),
            MAX_REPORTED_TEXT_BYTE_COUNT - suffix.len(),
        );
        let candidate_label = format!("{cut_base_label}{suffix}");
        if !is_label_held(&candidate_label) {
            return candidate_label;
        }
        suffix_number += 1;
    }
}

/// The number every surface prints for a client whose `color_index` is
/// `color_index`: `color_index + 1`. `0` → `1`, `11` → `12`, `4294967295` →
/// `4294967296`.
#[must_use]
pub fn compute_client_number(color_index: u32) -> u64 {
    u64::from(color_index) + 1
}

#[cfg(test)]
mod tests;
