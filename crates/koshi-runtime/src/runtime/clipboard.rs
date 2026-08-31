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

/// The OSC 52 sequence that puts `text` on the clipboard: `ESC ] 52 ; c ;
/// <base64 of text> BEL`. The `c` names the clipboard selection.
///
/// `hello` → `\x1b]52;c;aGVsbG8=\x07`. `""` → `\x1b]52;c;\x07`.
#[must_use]
pub(crate) fn osc52_copy(text: &str) -> Vec<u8> {
    let mut bytes = b"\x1b]52;c;".to_vec();
    bytes.extend_from_slice(STANDARD.encode(text).as_bytes());
    bytes.push(0x07);
    bytes
}

impl Server {
    /// Write `text` to the clipboard the copy command named.
    ///
    /// `target` comes from the command, which the viewer filled in from its own
    /// `copy.clipboard` setting: two viewers of one session send their copies to
    /// the clipboards their own settings name.
    ///
    /// [`CopyTarget::Osc52`] queues the escape for `client_id`'s outer terminal,
    /// behind anything already queued for that client. An empty `text` still
    /// queues the sequence. [`CopyTarget::Native`] queues nothing: koshi builds
    /// no native operating-system clipboard backend.
    pub(crate) fn copy_to_clipboard(
        &mut self,
        client_id: ClientId,
        target: CopyTarget,
        text: &str,
    ) {
        match target {
            CopyTarget::Osc52 => self.queue_host_write(client_id, &osc52_copy(text)),
            CopyTarget::Native => {}
        }
    }
}

/// The bytes a paste writes into a pane's PTY: `text` with every line break as
/// a carriage return, the byte the Enter key sends. `bracketed` true wraps the
/// payload in the bracketed-paste markers `ESC [ 200 ~` … `ESC [ 201 ~`; the
/// caller sets it when the pane turned that mode on.
///
/// Every line-break spelling becomes ONE return: `\r\n`, `\n` and `\r` each
/// leave a single `\r`. `"a\r\nb"` gives `a\rb` unbracketed, and
/// `\x1b[200~a\rb\x1b[201~` bracketed. Every other byte of `text` reaches the
/// PTY unchanged, an `ESC [ 201 ~` spelled inside `text` included.
#[must_use]
pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let payload = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut bytes = Vec::with_capacity(payload.len() + 12);
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(payload.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
}

#[cfg(test)]
mod tests;
