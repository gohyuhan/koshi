//! Crossterm keyboard boundary: one host key event becomes a canonical Koshi
//! chord plus the bytes a terminal pane receives when no binding consumes it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use koshi_core::key::{fold_uppercase, Key, KeyChord, ModFlags, NamedKey};

/// One normalized key press at the outer-terminal boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedKey {
    /// Canonical keybinding chord.
    pub chord: KeyChord,
    /// Terminal bytes used when key resolution falls through to the pane.
    pub raw_bytes: Vec<u8>,
}

/// Decode one press or repeat; releases and unsupported media keys yield
/// `None` so one physical press cannot be delivered twice.
#[must_use]
pub fn decode_key(event: KeyEvent) -> Option<DecodedKey> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    let (key, raw_bytes) = decode_code(event.code, event.modifiers)?;
    let (key, mut mods) = normalize(key, event.modifiers);
    // BackTab IS Shift+Tab: some hosts report the key without also setting
    // the Shift modifier, so the chord carries it unconditionally.
    if matches!(event.code, KeyCode::BackTab) {
        mods = mods.union(ModFlags::SHIFT);
    }
    Some(DecodedKey {
        chord: KeyChord::new(mods, key),
        raw_bytes,
    })
}

fn decode_code(code: KeyCode, modifiers: KeyModifiers) -> Option<(Key, Vec<u8>)> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let decoded = match code {
        KeyCode::Char(c) => {
            let bytes = if ctrl {
                vec![control_byte(c)?]
            } else {
                let mut bytes = if alt { vec![0x1b] } else { Vec::new() };
                let mut encoded = [0; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
                bytes
            };
            (Key::Char(c), bytes)
        }
        KeyCode::Enter => (Key::Named(NamedKey::Enter), vec![b'\r']),
        KeyCode::Backspace => (Key::Named(NamedKey::Backspace), vec![0x7f]),
        KeyCode::Tab => (Key::Named(NamedKey::Tab), vec![b'\t']),
        KeyCode::BackTab => (Key::Named(NamedKey::Tab), vec![0x1b, b'[', b'Z']),
        KeyCode::Esc => (Key::Named(NamedKey::Esc), vec![0x1b]),
        KeyCode::Up => (Key::Named(NamedKey::Up), csi(b'A')),
        KeyCode::Down => (Key::Named(NamedKey::Down), csi(b'B')),
        KeyCode::Right => (Key::Named(NamedKey::Right), csi(b'C')),
        KeyCode::Left => (Key::Named(NamedKey::Left), csi(b'D')),
        KeyCode::Home => (Key::Named(NamedKey::Home), csi(b'H')),
        KeyCode::End => (Key::Named(NamedKey::End), csi(b'F')),
        KeyCode::Insert => (Key::Named(NamedKey::Insert), tilde(2)),
        KeyCode::Delete => (Key::Named(NamedKey::Delete), tilde(3)),
        KeyCode::PageUp => (Key::Named(NamedKey::PageUp), tilde(5)),
        KeyCode::PageDown => (Key::Named(NamedKey::PageDown), tilde(6)),
        KeyCode::F(n @ 1..=24) => (Key::Named(NamedKey::F(n)), function_key(n)),
        _ => return None,
    };
    Some(decoded)
}

fn normalize(key: Key, modifiers: KeyModifiers) -> (Key, ModFlags) {
    let mut mods = ModFlags::NONE;
    if modifiers.contains(KeyModifiers::CONTROL) {
        mods = mods.union(ModFlags::CTRL);
    }
    if modifiers.contains(KeyModifiers::ALT) {
        mods = mods.union(ModFlags::ALT);
    }
    if modifiers.contains(KeyModifiers::SUPER) || modifiers.contains(KeyModifiers::META) {
        mods = mods.union(ModFlags::SUPER);
    }

    // The spacebar arrives as the character `' '`; bindings spell it
    // `<Space>`, so the chord carries the named key.
    let key = match key {
        Key::Char(' ') => Key::Named(NamedKey::Space),
        other => other,
    };

    match key {
        Key::Named(key) => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                mods = mods.union(ModFlags::SHIFT);
            }
            (Key::Named(key), mods)
        }
        // The chord carries the config parser's canonical character form: an
        // uppercase letter folds to lowercase plus Shift, and a held Shift is
        // reported only on a letter key.
        Key::Char(c) => {
            let (folded, shifted) = fold_uppercase(c);
            if shifted || (folded.is_lowercase() && modifiers.contains(KeyModifiers::SHIFT)) {
                mods = mods.union(ModFlags::SHIFT);
            }
            (Key::Char(folded), mods)
        }
    }
}

fn csi(final_byte: u8) -> Vec<u8> {
    vec![0x1b, b'[', final_byte]
}

fn tilde(n: u8) -> Vec<u8> {
    let mut bytes = vec![0x1b, b'['];
    bytes.extend_from_slice(n.to_string().as_bytes());
    bytes.push(b'~');
    bytes
}

fn function_key(n: u8) -> Vec<u8> {
    match n {
        1 => vec![0x1b, b'O', b'P'],
        2 => vec![0x1b, b'O', b'Q'],
        3 => vec![0x1b, b'O', b'R'],
        4 => vec![0x1b, b'O', b'S'],
        5..=8 => tilde(n + 10),
        9..=10 => tilde(n + 11),
        11..=12 => tilde(n + 12),
        13..=14 => tilde(n + 15),
        15..=16 => tilde(n + 16),
        17..=24 => tilde(n + 17),
        _ => unreachable!("function key range checked by caller"),
    }
}

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
