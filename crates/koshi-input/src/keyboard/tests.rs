//! Keyboard-boundary tests: canonical chords, passthrough bytes, modifiers,
//! named keys, function keys, unsupported keys, and release suppression.

use super::*;

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn plain_and_control_characters_keep_chord_and_passthrough_forms() {
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::NONE)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::NONE, Key::Char('a')),
            raw_bytes: vec![b'a'],
        })
    );
    assert_eq!(
        decode_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::CTRL, Key::Char('c')),
            raw_bytes: vec![0x03],
        })
    );
}

#[test]
fn uppercase_host_forms_normalize_to_shift_plus_lowercase() {
    assert_eq!(
        decode_key(press(KeyCode::Char('H'), KeyModifiers::ALT)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')),
            raw_bytes: vec![0x1b, b'H'],
        })
    );
}

#[test]
fn alt_character_keeps_escape_prefixed_passthrough() {
    assert_eq!(
        decode_key(press(KeyCode::Char('b'), KeyModifiers::ALT)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::ALT, Key::Char('b')),
            raw_bytes: vec![0x1b, b'b'],
        })
    );
}

#[test]
fn named_keys_decode_exactly() {
    let cases = [
        (KeyCode::Enter, NamedKey::Enter, vec![b'\r']),
        (KeyCode::Backspace, NamedKey::Backspace, vec![0x7f]),
        (KeyCode::Tab, NamedKey::Tab, vec![b'\t']),
        (KeyCode::Esc, NamedKey::Esc, vec![0x1b]),
        (KeyCode::Up, NamedKey::Up, vec![0x1b, b'[', b'A']),
        (
            KeyCode::Delete,
            NamedKey::Delete,
            vec![0x1b, b'[', b'3', b'~'],
        ),
    ];
    for (code, key, raw_bytes) in cases {
        assert_eq!(
            decode_key(press(code, KeyModifiers::NONE)),
            Some(DecodedKey {
                chord: KeyChord::new(ModFlags::NONE, Key::Named(key)),
                raw_bytes,
            })
        );
    }
}

#[test]
fn shifted_tab_uses_one_canonical_named_chord() {
    assert_eq!(
        decode_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
            raw_bytes: vec![0x1b, b'[', b'Z'],
        })
    );
    // BackTab IS Shift+Tab even when the host omits the modifier flag.
    assert_eq!(
        decode_key(press(KeyCode::BackTab, KeyModifiers::NONE)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
            raw_bytes: vec![0x1b, b'[', b'Z'],
        })
    );
}

#[test]
fn function_key_bytes_cover_short_and_numeric_forms() {
    assert_eq!(
        decode_key(press(KeyCode::F(1), KeyModifiers::NONE))
            .expect("F1")
            .raw_bytes,
        vec![0x1b, b'O', b'P']
    );
    assert_eq!(
        decode_key(press(KeyCode::F(12), KeyModifiers::NONE))
            .expect("F12")
            .raw_bytes,
        vec![0x1b, b'[', b'2', b'4', b'~']
    );
}

#[test]
fn releases_and_unsupported_keys_are_ignored() {
    assert_eq!(
        decode_key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )),
        None
    );
    assert_eq!(decode_key(press(KeyCode::Null, KeyModifiers::NONE)), None);
}

#[test]
fn control_byte_table_is_exact() {
    let cases: [(char, u8); 6] = [
        ('a', 0x01),
        ('z', 0x1a),
        (' ', 0x00),
        ('?', 0x7f),
        ('@', 0x00),
        ('_', 0x1f),
    ];
    for (ch, byte) in cases {
        assert_eq!(
            decode_key(press(KeyCode::Char(ch), KeyModifiers::CONTROL))
                .unwrap_or_else(|| panic!("Ctrl+{ch} decodes"))
                .raw_bytes,
            vec![byte],
            "Ctrl+{ch}"
        );
    }
    // A character with no control-byte mapping yields no key at all.
    assert_eq!(
        decode_key(press(KeyCode::Char('é'), KeyModifiers::CONTROL)),
        None
    );
}

#[test]
fn super_and_meta_modifiers_map_to_the_super_flag() {
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::SUPER))
            .expect("Super+a decodes")
            .chord,
        KeyChord::new(ModFlags::SUPER, Key::Char('a')),
    );
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::META))
            .expect("Meta+a decodes")
            .chord,
        KeyChord::new(ModFlags::SUPER, Key::Char('a')),
    );
}

#[test]
fn repeat_events_decode_like_presses() {
    assert_eq!(
        decode_key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::NONE, Key::Char('a')),
            raw_bytes: vec![b'a'],
        })
    );
}
