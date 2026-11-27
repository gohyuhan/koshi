//! The `OSC 52` clipboard copies that a program in a pane writes.
//!
//! A copy is `ESC ] 52 ; <targets> ; <payload>`, ended by `BEL` or by
//! `ESC \`. The engine's OSC reader (module `osc_reader`) finds each copy in
//! the bytes that the VTE parser reads, and keeps it as a [`ClipboardCopy`].
//! It keeps nothing of any other sequence.
//!
//! The command is a copy only when the bytes before its first `;` are exactly
//! `52`. The targets field runs to the second `;`, and the payload runs to the
//! end of the command:
//!
//! - A command with no second `;` makes no copy.
//! - The payload `?` asks to read the clipboard, and makes no copy.
//! - A payload that is not base64, with or without `=` padding, makes an empty
//!   copy. The empty payload makes an empty copy too.
//! - A payload whose decoded bytes are not UTF-8 makes a copy of those bytes,
//!   unchanged: `ESC ] 52 ; c ; /w== BEL` gives a copy of the one byte `0xff`.
//! - A payload longer than `maximum_copy_byte_count` bytes rounded up to a
//!   multiple of 3, then multiplied by 4/3, makes no copy, and the reader holds
//!   none of its bytes. A payload whose decoded bytes are more than
//!   `maximum_copy_byte_count` makes no copy.
//!
//! Example: `ESC ] 52 ; c ; aGk= BEL` gives a copy of the two bytes `hi` for
//! the target `c`. `ESC ] 52 ; c ; aGk= CAN` gives nothing.

use base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT;
use base64::Engine as _;

/// One finished `OSC 52` copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCopy {
    /// The targets the copy names, as
    /// [`push_clipboard_target`](koshi_core::clipboard::push_clipboard_target)
    /// keeps them: each of `c`, `p`, `q`, `s` and `0` to `7`, once, in the
    /// order of its first use. Empty when the targets field names none. The
    /// outer terminal then uses its own default targets.
    pub clipboard_targets: String,
    /// The decoded payload. Empty for the empty payload and for a payload that
    /// is not base64: the outer terminal clears the targets.
    pub copied_bytes: Vec<u8>,
}

/// The size limits of the copies an OSC reader keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClipboardCopyLimit {
    /// The most decoded bytes that one copy can carry.
    maximum_copy_byte_count: usize,
    /// The most payload bytes that the reader holds for one copy:
    /// `maximum_copy_byte_count` divided by 3, rounded up, then multiplied by
    /// 4.
    pub(crate) maximum_payload_byte_count: usize,
}

impl ClipboardCopyLimit {
    /// The limits for copies of at most `maximum_copy_byte_count` decoded
    /// bytes. `4` gives `maximum_payload_byte_count` `8`.
    pub(crate) fn with_maximum_copy_byte_count(maximum_copy_byte_count: usize) -> Self {
        Self {
            maximum_copy_byte_count,
            maximum_payload_byte_count: maximum_copy_byte_count.div_ceil(3).saturating_mul(4),
        }
    }

    /// The copy that a payload of `payload_bytes` for `clipboard_targets`
    /// gives, or `None` for the payload `?` and for a payload whose decoded
    /// bytes are more than `maximum_copy_byte_count`.
    pub(crate) fn parse_clipboard_copy(
        &self,
        clipboard_targets: String,
        payload_bytes: &[u8],
    ) -> Option<ClipboardCopy> {
        if payload_bytes == b"?" {
            return None;
        }
        let copied_bytes = STANDARD_PAD_INDIFFERENT
            .decode(payload_bytes)
            .unwrap_or_default();
        if copied_bytes.len() > self.maximum_copy_byte_count {
            return None;
        }
        Some(ClipboardCopy {
            clipboard_targets,
            copied_bytes,
        })
    }
}

#[cfg(test)]
mod tests;
