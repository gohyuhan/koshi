//! Clipboard writes for copied text.
//!
//! OSC 52 is the terminal escape for "put this on the clipboard". It travels
//! to the **outer terminal** — the program koshi itself runs in — which owns
//! the real clipboard, so it works over SSH and needs no OS clipboard
//! dependency. The payload is base64 so any bytes survive the trip.
//!
//! `ClipboardWriter` is the seam an operating-system clipboard backend plugs
//! into. Today `Osc52Clipboard` is the only writer and `ClipboardBackend` has
//! the single `Osc52` variant; a native backend adds a writer plus its own
//! variant without reshaping this flow.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use koshi_config::types::ClipboardBackend;
use koshi_core::ids::ClientId;

use crate::runtime::state::Runtime;

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

impl Runtime {
    /// Write `text` to the clipboard backend selected by the current config.
    ///
    /// OSC 52 is the only backend koshi builds today; a native backend plugs a
    /// new `ClipboardWriter` in behind a new `ClipboardBackend` variant.
    pub(crate) fn copy_to_clipboard(&mut self, client_id: ClientId, text: &str) {
        match self.config.copy.clipboard {
            ClipboardBackend::Osc52 => {
                let mut clipboard = Osc52Clipboard::default();
                if clipboard.write(text) {
                    self.queue_host_write(client_id, &clipboard.bytes);
                }
            }
        }
    }
}

/// The bytes a paste writes into a pane's PTY: `text` with line breaks as
/// carriage returns — the byte the Enter key sends, which is how every
/// terminal pastes them — wrapped in the bracketed-paste markers
/// (`ESC [ 200 ~` … `ESC [ 201 ~`) when the pane turned that mode on, so the
/// program can tell a paste from typing.
///
/// Every line-break spelling becomes ONE return: Windows clipboard text
/// carries `\r\n`, and converting its `\n` alone would send two Enters per
/// break.
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
