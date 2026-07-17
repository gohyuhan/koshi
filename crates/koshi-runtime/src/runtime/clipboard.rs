//! Clipboard escape encoding: turning copied text into the OSC 52 sequence
//! the outer terminal reads.
//!
//! OSC 52 is the terminal escape for "put this on the clipboard". It travels
//! to the **outer terminal** — the program koshi itself runs in — which owns
//! the real clipboard, so it works over SSH and needs no OS clipboard
//! dependency. The payload is base64 so any bytes survive the trip.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

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
