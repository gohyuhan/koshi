//! Minimal keyboard encoding: a crossterm key event to the bytes a terminal
//! sends its child, plus the one intercepted binding (Ctrl-Q) that quits.
//!
//! This is a passthrough stub, not keybinding dispatch: every key except the
//! quit chord is turned into the raw bytes a real terminal would emit and
//! written straight to the focused pane. Ctrl-C and friends therefore reach the
//! shell as their control bytes.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What the loop should do with one key press.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// The quit chord (Ctrl-Q) — stop the loop.
    Quit,
    /// The bytes to write to the focused pane's child.
    Bytes(Vec<u8>),
    /// Nothing to send (key release, or an unmapped key).
    Ignore,
}

/// Decode one key event into an action. Only presses and repeats produce bytes;
/// releases (sent on some platforms) are ignored so a keypress isn't doubled.
pub fn decode_key(key: KeyEvent) -> KeyAction {
    if matches!(key.kind, KeyEventKind::Release) {
        return KeyAction::Ignore;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // The one intercepted binding.
    if ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
        return KeyAction::Quit;
    }

    let bytes: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                match control_byte(c) {
                    Some(b) => return KeyAction::Bytes(vec![b]),
                    None => return KeyAction::Ignore,
                }
            }
            // Alt prefixes the character with ESC, matching xterm meta.
            let mut out = if alt { vec![0x1b] } else { Vec::new() };
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            out
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi(b'A'),
        KeyCode::Down => csi(b'B'),
        KeyCode::Right => csi(b'C'),
        KeyCode::Left => csi(b'D'),
        KeyCode::Home => csi(b'H'),
        KeyCode::End => csi(b'F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        _ => return KeyAction::Ignore,
    };
    KeyAction::Bytes(bytes)
}

/// A `CSI <final>` sequence (`ESC [ x`), used for the arrow and Home/End keys.
fn csi(final_byte: u8) -> Vec<u8> {
    vec![0x1b, b'[', final_byte]
}

/// A `CSI <n> ~` sequence (`ESC [ n ~`), used for Insert/Delete/Page keys.
fn tilde(n: u8) -> Vec<u8> {
    vec![0x1b, b'[', b'0' + n, b'~']
}

/// The control byte a `Ctrl`-modified character produces, or `None` when the
/// character has no control mapping.
fn control_byte(c: char) -> Option<u8> {
    match c {
        '@'..='_' => Some((c as u8) & 0x1f),
        'a'..='z' => Some((c.to_ascii_uppercase() as u8) & 0x1f),
        ' ' => Some(0),
        '?' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
