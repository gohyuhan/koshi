//! Keyboard-boundary tests: canonical chords, passthrough bytes, modifiers,
//! named keys, function keys, unsupported keys, and release suppression.

use super::*;
use crossterm::event::{MediaKeyCode, ModifierKeyCode};

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
fn spacebar_decodes_as_the_named_space_key() {
    // The chord form matches the config parser's `<Space>`; the passthrough
    // byte stays the plain space the pane expects.
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::NONE)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Space)),
            raw_bytes: vec![b' '],
        })
    );
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Space)),
            raw_bytes: vec![0x00],
        })
    );
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::SHIFT)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Space)),
            raw_bytes: vec![b' '],
        })
    );
}

#[test]
fn non_ascii_letters_fold_like_the_config_parser() {
    // `É` folds to `é` plus Shift — the exact chord the config parser stores
    // for a binding written `É` or `<S-é>` — while the passthrough bytes keep
    // the typed character's UTF-8.
    assert_eq!(
        decode_key(press(KeyCode::Char('É'), KeyModifiers::NONE)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::SHIFT, Key::Char('é')),
            raw_bytes: "É".as_bytes().to_vec(),
        })
    );
    // A lowercase non-ASCII letter with the Shift modifier held reports Shift.
    assert_eq!(
        decode_key(press(KeyCode::Char('é'), KeyModifiers::SHIFT))
            .expect("Shift+é decodes")
            .chord,
        KeyChord::new(ModFlags::SHIFT, Key::Char('é')),
    );
    // `İ` lowercases to two characters, so it stands as it is, unshifted —
    // the same rule the config parser applies.
    assert_eq!(
        decode_key(press(KeyCode::Char('İ'), KeyModifiers::NONE))
            .expect("İ decodes")
            .chord,
        KeyChord::new(ModFlags::NONE, Key::Char('İ')),
    );
}

#[test]
fn shift_on_a_non_letter_is_not_reported_in_the_chord() {
    // `!` is already the shifted form; the chord is the bare character, the
    // same shape the config parser accepts (`<S-!>` is not writable).
    assert_eq!(
        decode_key(press(KeyCode::Char('!'), KeyModifiers::SHIFT))
            .expect("Shift+1 decodes")
            .chord,
        KeyChord::new(ModFlags::NONE, Key::Char('!')),
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

#[test]
fn function_key_band_boundaries_use_exact_xterm_codes() {
    // Covers the SS3→tilde transition (F4/F5) and every internal band edge in
    // `function_key`'s offset table, plus the top of the whole F1..=F24 range.
    let cases: [(u8, Vec<u8>); 10] = [
        (4, vec![0x1b, b'O', b'S']),
        (5, vec![0x1b, b'[', b'1', b'5', b'~']),
        (8, vec![0x1b, b'[', b'1', b'8', b'~']),
        (9, vec![0x1b, b'[', b'2', b'0', b'~']),
        (10, vec![0x1b, b'[', b'2', b'1', b'~']),
        (11, vec![0x1b, b'[', b'2', b'3', b'~']),
        (13, vec![0x1b, b'[', b'2', b'8', b'~']),
        (16, vec![0x1b, b'[', b'3', b'2', b'~']),
        (17, vec![0x1b, b'[', b'3', b'4', b'~']),
        (24, vec![0x1b, b'[', b'4', b'1', b'~']),
    ];
    for (n, raw_bytes) in cases {
        assert_eq!(
            decode_key(press(KeyCode::F(n), KeyModifiers::NONE))
                .unwrap_or_else(|| panic!("F{n} decodes"))
                .raw_bytes,
            raw_bytes,
            "F{n}"
        );
    }
}

#[test]
fn function_key_numbers_outside_one_to_twenty_four_are_unmapped() {
    // `decode_code` only matches `F(n @ 1..=24)`; F(0) and F(25) fall to the
    // wildcard arm and decode to nothing, same as any other unsupported key.
    assert_eq!(decode_key(press(KeyCode::F(0), KeyModifiers::NONE)), None);
    assert_eq!(decode_key(press(KeyCode::F(25), KeyModifiers::NONE)), None);
}

#[test]
fn other_unmapped_key_codes_are_ignored_like_null() {
    // The wildcard arm in `decode_code` covers every other unhandled
    // `KeyCode` variant, not just `Null` — check a lock key and the two enum
    // wrapper variants (`Media`, `Modifier`) crossterm can report when the
    // keyboard enhancement protocol is active.
    assert_eq!(
        decode_key(press(KeyCode::CapsLock, KeyModifiers::NONE)),
        None
    );
    assert_eq!(
        decode_key(press(
            KeyCode::Media(MediaKeyCode::Play),
            KeyModifiers::NONE
        )),
        None
    );
    assert_eq!(
        decode_key(press(
            KeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::NONE
        )),
        None
    );
}

#[test]
fn control_byte_boundary_characters_outside_defined_ranges_yield_none() {
    // '`' (0x60) sits directly between the '@'..='_' and 'a'..='z' ranges in
    // `control_byte`; neither arm matches it, so it decodes to nothing.
    assert_eq!(
        decode_key(press(KeyCode::Char('`'), KeyModifiers::CONTROL)),
        None
    );
}

#[test]
fn control_plus_shift_on_a_letter_reports_both_modifiers_with_the_control_byte() {
    // Some hosts report Ctrl+Shift+A as `Char('A')` carrying only CONTROL.
    assert_eq!(
        decode_key(press(KeyCode::Char('A'), KeyModifiers::CONTROL)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a')),
            raw_bytes: vec![0x01],
        })
    );
    // Other hosts report the same physical combo as `Char('a')` carrying both
    // CONTROL and SHIFT.
    assert_eq!(
        decode_key(press(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a')),
            raw_bytes: vec![0x01],
        })
    );
}

#[test]
fn every_modifier_held_at_once_still_decodes_the_control_byte() {
    assert_eq!(
        decode_key(press(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER
        )),
        Some(DecodedKey {
            chord: KeyChord::new(
                ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
                Key::Char('a')
            ),
            raw_bytes: vec![0x01],
        })
    );
}

#[test]
fn control_plus_alt_passthrough_bytes_omit_the_escape_prefix() {
    // `decode_code` only prefixes ESC in the non-Ctrl branch, so holding ALT
    // alongside CONTROL leaves the passthrough byte as the bare control byte.
    assert_eq!(
        decode_key(press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ))
        .expect("Ctrl+Alt+c decodes")
        .raw_bytes,
        vec![0x03]
    );
}

#[test]
fn ctrl_backtab_keeps_the_shift_flag_and_adds_control() {
    // BackTab always carries SHIFT unconditionally (decode_key's BackTab
    // special case); an extra CONTROL modifier from the host must still
    // come through in the chord alongside it.
    assert_eq!(
        decode_key(press(KeyCode::BackTab, KeyModifiers::CONTROL)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
            raw_bytes: vec![0x1b, b'[', b'Z'],
        })
    );
}

#[test]
fn alt_space_keeps_the_escape_prefixed_byte_after_remapping_to_named_space() {
    // The passthrough bytes are computed from `Char(' ')` before `normalize`
    // remaps the chord key to `Named(Space)`; the ESC prefix from ALT must
    // survive that remap.
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::ALT)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::ALT, Key::Named(NamedKey::Space)),
            raw_bytes: vec![0x1b, b' '],
        })
    );
}

#[test]
fn four_byte_utf8_character_encodes_without_truncation() {
    // `encode_utf8` writes into a 4-byte stack buffer; a character needing the
    // full width must not be truncated.
    assert_eq!(
        decode_key(press(KeyCode::Char('🎉'), KeyModifiers::NONE)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::NONE, Key::Char('🎉')),
            raw_bytes: "🎉".as_bytes().to_vec(),
        })
    );
}

#[test]
fn shift_plus_lowercase_letter_with_no_other_modifier_reports_shift() {
    // A host reporting Shift+a as `Char('a')` carrying SHIFT (rather than
    // `Char('A')`) must still surface SHIFT in the chord; the passthrough
    // byte is unaffected since raw-byte encoding never reads SHIFT.
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::SHIFT)),
        Some(DecodedKey {
            chord: KeyChord::new(ModFlags::SHIFT, Key::Char('a')),
            raw_bytes: vec![b'a'],
        })
    );
}
