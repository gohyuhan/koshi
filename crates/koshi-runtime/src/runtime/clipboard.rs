//! Clipboard writes for copied text.
//!
//! OSC 52 is the terminal escape that sets the clipboard. It travels to the
//! **outer terminal** — the program koshi itself runs in — which owns the real
//! clipboard. The payload is base64. Base64 carries every byte value.
//!
//! OSC 52 is the only clipboard koshi writes to. A copy naming
//! `CopyTarget::Native` writes nothing.
//!
//! The copy command carries which clipboard it means: the viewer that decided
//! the copy fills it in from its own `copy.clipboard` setting, and the session
//! writes where the command says.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use koshi_core::command::CopyTarget;
use koshi_core::ids::ClientId;

use crate::server::Server;

/// The OSC 52 sequence that puts `copied_text` on the clipboard: `ESC ] 52 ; c ;
/// <base64 of copied_text> BEL`. The `c` names the clipboard selection.
///
/// `hello` → `\x1b]52;c;aGVsbG8=\x07`. `""` → `\x1b]52;c;\x07`.
#[must_use]
pub(crate) fn osc52_copy(copied_text: &str) -> Vec<u8> {
    let mut osc52_sequence_bytes = b"\x1b]52;c;".to_vec();
    osc52_sequence_bytes.extend_from_slice(STANDARD.encode(copied_text).as_bytes());
    osc52_sequence_bytes.push(0x07);
    osc52_sequence_bytes
}

impl Server {
    /// Write `copied_text` to the clipboard the copy command named.
    ///
    /// `clipboard_target` comes from the command, which the viewer filled in from its own
    /// `copy.clipboard` setting: two viewers of one session send their copies to
    /// the clipboards their own settings name.
    ///
    /// [`CopyTarget::Osc52`] queues the escape for `client_id`'s outer terminal,
    /// behind anything already queued for that client. An empty `copied_text` still
    /// queues the sequence. [`CopyTarget::Native`] queues nothing: koshi builds
    /// no native operating-system clipboard backend.
    pub(crate) fn copy_to_clipboard(
        &mut self,
        client_id: ClientId,
        clipboard_target: CopyTarget,
        copied_text: &str,
    ) {
        match clipboard_target {
            CopyTarget::Osc52 => self.queue_host_write(client_id, &osc52_copy(copied_text)),
            CopyTarget::Native => {}
        }
    }
}

/// The bytes a paste writes into a pane's PTY: `pasted_text` with every line break as
/// a carriage return, the byte the Enter key sends. `is_bracketed_paste_enabled` true wraps the
/// payload in the bracketed-paste markers `ESC [ 200 ~` … `ESC [ 201 ~`; the
/// caller sets it when the pane turned that mode on.
///
/// Every line-break spelling becomes ONE return: `\r\n`, `\n` and `\r` each
/// leave a single `\r`. `"a\r\nb"` gives `a\rb` unbracketed, and
/// `\x1b[200~a\rb\x1b[201~` bracketed. Every other byte of `pasted_text` reaches the
/// PTY unchanged, an `ESC [ 201 ~` spelled inside `pasted_text` included.
#[must_use]
pub(crate) fn build_paste_bytes(pasted_text: &str, is_bracketed_paste_enabled: bool) -> Vec<u8> {
    let normalized_paste_text = pasted_text.replace("\r\n", "\r").replace('\n', "\r");
    let mut paste_output_bytes = Vec::with_capacity(normalized_paste_text.len() + 12);
    if is_bracketed_paste_enabled {
        paste_output_bytes.extend_from_slice(b"\x1b[200~");
    }
    paste_output_bytes.extend_from_slice(normalized_paste_text.as_bytes());
    if is_bracketed_paste_enabled {
        paste_output_bytes.extend_from_slice(b"\x1b[201~");
    }
    paste_output_bytes
}

#[cfg(test)]
mod tests;
