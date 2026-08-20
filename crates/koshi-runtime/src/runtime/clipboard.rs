//! Clipboard writes for copied text.
//!
//! OSC 52 is the terminal escape for "put this on the clipboard". It travels
//! to the **outer terminal** — the program koshi itself runs in — which owns
//! the real clipboard, so it works over SSH and needs no OS clipboard
//! dependency. The payload is base64 so any bytes survive the trip.
//!
//! `Osc52Clipboard` is the only `ClipboardWriter` koshi builds. A copy naming
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

/// One destination that can receive copied text.
pub(crate) trait ClipboardWriter {
    /// Write `text`, returning whether the destination accepted it.
    fn write(&mut self, text: &str) -> bool;
}

/// Collects one OSC 52 write for the client's outer terminal.
#[derive(Default)]
struct Osc52Clipboard {
    bytes: Vec<u8>,
}

impl ClipboardWriter for Osc52Clipboard {
    fn write(&mut self, text: &str) -> bool {
        self.bytes = osc52_copy(text);
        true
    }
}

/// The OSC 52 sequence that puts `text` on the clipboard: `ESC ] 52 ; c ;
/// <base64 of text> BEL`. The `c` selects the clipboard proper (as opposed to
/// the X11 primary selection).
///
/// `hello` → `\x1b]52;c;aGVsbG8=\x07`.
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
    /// [`CopyTarget::Osc52`] queues the escape for `client_id`'s outer terminal.
    /// [`CopyTarget::Native`] writes nothing: koshi builds no native
    /// operating-system clipboard backend, so there is nothing to write to.
    pub(crate) fn copy_to_clipboard(
        &mut self,
        client_id: ClientId,
        target: CopyTarget,
        text: &str,
    ) {
        match target {
            CopyTarget::Osc52 => {
                let mut clipboard = Osc52Clipboard::default();
                if clipboard.write(text) {
                    self.queue_host_write(client_id, &clipboard.bytes);
                }
            }
            CopyTarget::Native => {}
        }
    }
}

/// The bytes a paste writes into a pane's PTY: `text` with line breaks as
/// carriage returns — the byte the Enter key sends, which is how every
/// terminal pastes them — wrapped in the bracketed-paste markers
/// (`ESC [ 200 ~` … `ESC [ 201 ~`) when the pane turned that mode on, so the
/// program can tell a paste from typing.
///
/// Every line-break spelling becomes ONE return: `\r\n`, `\n` and `\r` each
/// leave a single `\r`, so `"a\r\nb"` pastes as `a\rb`.
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
