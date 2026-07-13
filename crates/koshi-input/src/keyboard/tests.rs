//! Keyboard-boundary tests: the decode table (host event → canonical chord)
//! and the encode table (chord → the bytes a program in a pane expects), with
//! modifiers, named keys, function keys, unsupported keys, release
//! suppression, and application-cursor-keys mode.

use super::*;
use crossterm::event::{MediaKeyCode, ModifierKeyCode};

/// The bytes this chord sends to a pane in the ordinary (non-application)
/// cursor-key mode, which is every pane's state until a program changes it.
fn bytes(mods: ModFlags, key: Key) -> Vec<u8> {
    encode(KeyChord::new(mods, key), false)
}

/// The bytes this chord sends to a pane whose program turned on
/// application-cursor-keys mode (DECCKM) — vim, less, and most full-screen
/// programs do.
fn app_bytes(mods: ModFlags, key: Key) -> Vec<u8> {
    encode(KeyChord::new(mods, key), true)
}

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn chord(mods: ModFlags, key: Key) -> Option<KeyChord> {
    Some(KeyChord::new(mods, key))
}

// ---------------------------------------------------------------- decode ----

#[test]
fn characters_decode_to_their_chord() {
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::NONE)),
        chord(ModFlags::NONE, Key::Char('a'))
    );
    assert_eq!(
        decode_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        chord(ModFlags::CTRL, Key::Char('c'))
    );
    assert_eq!(
        decode_key(press(KeyCode::Char('b'), KeyModifiers::ALT)),
        chord(ModFlags::ALT, Key::Char('b'))
    );
}

#[test]
fn uppercase_host_forms_normalize_to_shift_plus_lowercase() {
    // A terminal with no keyboard protocol reports Alt+Shift+h as the capital
    // with only Alt held; the Windows console reports the lowercase with both
    // Alt and Shift. Both are the same chord.
    assert_eq!(
        decode_key(press(KeyCode::Char('H'), KeyModifiers::ALT)),
        chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'))
    );
    assert_eq!(
        decode_key(press(
            KeyCode::Char('h'),
            KeyModifiers::ALT | KeyModifiers::SHIFT
        )),
        chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'))
    );
}

#[test]
fn shifted_non_letter_stands_for_itself() {
    // Shift+1 is `!`, not Shift plus `1`.
    assert_eq!(
        decode_key(press(KeyCode::Char('!'), KeyModifiers::SHIFT)),
        chord(ModFlags::NONE, Key::Char('!'))
    );
}

#[test]
fn spacebar_decodes_to_the_named_key_bindings_spell() {
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::NONE)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Space))
    );
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        chord(ModFlags::CTRL, Key::Named(NamedKey::Space))
    );
}

#[test]
fn named_keys_decode_exactly() {
    let cases = [
        (KeyCode::Enter, NamedKey::Enter),
        (KeyCode::Backspace, NamedKey::Backspace),
        (KeyCode::Tab, NamedKey::Tab),
        (KeyCode::Esc, NamedKey::Esc),
        (KeyCode::Up, NamedKey::Up),
        (KeyCode::Down, NamedKey::Down),
        (KeyCode::Left, NamedKey::Left),
        (KeyCode::Right, NamedKey::Right),
        (KeyCode::Home, NamedKey::Home),
        (KeyCode::End, NamedKey::End),
        (KeyCode::Insert, NamedKey::Insert),
        (KeyCode::Delete, NamedKey::Delete),
        (KeyCode::PageUp, NamedKey::PageUp),
        (KeyCode::PageDown, NamedKey::PageDown),
        (KeyCode::F(1), NamedKey::F(1)),
        (KeyCode::F(24), NamedKey::F(24)),
    ];
    for (code, named) in cases {
        assert_eq!(
            decode_key(press(code, KeyModifiers::NONE)),
            chord(ModFlags::NONE, Key::Named(named)),
            "{code:?}"
        );
    }
}

#[test]
fn named_keys_carry_shift_like_any_other_modifier() {
    assert_eq!(
        decode_key(press(
            KeyCode::Up,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL
        )),
        chord(ModFlags::SHIFT | ModFlags::CTRL, Key::Named(NamedKey::Up))
    );
}

#[test]
fn backtab_is_shift_tab_even_when_the_host_omits_the_modifier() {
    assert_eq!(
        decode_key(press(KeyCode::BackTab, KeyModifiers::NONE)),
        chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab))
    );
}

#[test]
fn super_and_meta_both_decode_to_super() {
    assert_eq!(
        decode_key(press(KeyCode::Char('k'), KeyModifiers::SUPER)),
        chord(ModFlags::SUPER, Key::Char('k'))
    );
    assert_eq!(
        decode_key(press(KeyCode::Char('k'), KeyModifiers::META)),
        chord(ModFlags::SUPER, Key::Char('k'))
    );
}

#[test]
fn repeat_decodes_and_release_does_not() {
    let mut repeat = press(KeyCode::Char('a'), KeyModifiers::NONE);
    repeat.kind = KeyEventKind::Repeat;
    assert_eq!(decode_key(repeat), chord(ModFlags::NONE, Key::Char('a')));

    let mut release = press(KeyCode::Char('a'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(decode_key(release), None);
}

#[test]
fn keys_the_chord_model_cannot_name_are_not_input() {
    let cases = [
        KeyCode::CapsLock,
        KeyCode::ScrollLock,
        KeyCode::NumLock,
        KeyCode::PrintScreen,
        KeyCode::Pause,
        KeyCode::Menu,
        KeyCode::KeypadBegin,
        KeyCode::Null,
        KeyCode::F(25),
        KeyCode::Media(MediaKeyCode::Play),
        KeyCode::Modifier(ModifierKeyCode::LeftControl),
    ];
    for code in cases {
        assert_eq!(
            decode_key(press(code, KeyModifiers::NONE)),
            None,
            "{code:?}"
        );
    }
}

// ---------------------------------------------------------------- encode ----

#[test]
fn characters_encode_to_their_bytes() {
    assert_eq!(bytes(ModFlags::NONE, Key::Char('a')), vec![b'a']);
    assert_eq!(bytes(ModFlags::SHIFT, Key::Char('a')), vec![b'A']);
    assert_eq!(bytes(ModFlags::NONE, Key::Char('!')), vec![b'!']);
    // A multi-byte character keeps every byte of its UTF-8 form.
    assert_eq!(bytes(ModFlags::NONE, Key::Char('é')), vec![0xc3, 0xa9]);
}

#[test]
fn control_characters_fold_into_their_c0_byte() {
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('a')), vec![0x01]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('c')), vec![0x03]);
    // Control plus Shift plus a letter is the same C0 byte: no terminal
    // sequence tells them apart.
    assert_eq!(
        bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a')),
        vec![0x01]
    );
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('[')), vec![0x1b]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('?')), vec![0x7f]);
}

#[test]
fn control_plus_a_character_with_no_c0_byte_sends_the_character() {
    // `<C-1>` is a bindable chord, but a terminal has no byte for it and
    // sends the digit alone — the key must still reach the pane.
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('1')), vec![b'1']);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char(';')), vec![b';']);
}

#[test]
fn alt_prefixes_escape_and_composes_with_control() {
    assert_eq!(bytes(ModFlags::ALT, Key::Char('b')), vec![ESC, b'b']);
    assert_eq!(
        bytes(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')),
        vec![ESC, b'H']
    );
    // Alt+Ctrl+a is the ESC prefix in front of Ctrl+a's byte.
    assert_eq!(
        bytes(ModFlags::ALT | ModFlags::CTRL, Key::Char('a')),
        vec![ESC, 0x01]
    );
}

#[test]
fn super_rides_the_parameter_but_has_no_c0_form() {
    // A C0 byte has no field for Super, so the key arrives bare — what it
    // sends in a terminal running no multiplexer.
    assert_eq!(bytes(ModFlags::SUPER, Key::Char('a')), vec![b'a']);
    // A CSI key has the modifier parameter, and Super is its bit 8.
    assert_eq!(
        bytes(ModFlags::SUPER, Key::Named(NamedKey::Up)),
        b"\x1b[1;9A".to_vec()
    );
}

#[test]
fn c0_named_keys_encode_to_their_bytes() {
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::Enter)),
        vec![b'\r']
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        vec![b'\t']
    );
    assert_eq!(bytes(ModFlags::NONE, Key::Named(NamedKey::Esc)), vec![ESC]);
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::Space)),
        vec![b' ']
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::Backspace)),
        vec![0x7f]
    );
}

#[test]
fn control_and_alt_reshape_the_c0_named_keys() {
    // Ctrl+Backspace is the BS byte: a shell reads it as "erase a word",
    // where the plain DEL byte erases one character.
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::Backspace)),
        vec![0x08]
    );
    assert_eq!(
        bytes(ModFlags::ALT, Key::Named(NamedKey::Backspace)),
        vec![ESC, 0x7f]
    );
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::Space)),
        vec![0x00]
    );
    assert_eq!(
        bytes(ModFlags::ALT, Key::Named(NamedKey::Enter)),
        vec![ESC, b'\r']
    );
    assert_eq!(
        bytes(ModFlags::ALT, Key::Named(NamedKey::Esc)),
        vec![ESC, ESC]
    );
}

#[test]
fn shift_tab_has_a_sequence_of_its_own() {
    assert_eq!(
        bytes(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESC, b'[', b'Z']
    );
}

#[test]
fn cursor_keys_follow_the_panes_application_mode() {
    let cases = [
        (NamedKey::Up, b'A'),
        (NamedKey::Down, b'B'),
        (NamedKey::Right, b'C'),
        (NamedKey::Left, b'D'),
        (NamedKey::End, b'F'),
        (NamedKey::Home, b'H'),
    ];
    for (key, final_byte) in cases {
        assert_eq!(
            bytes(ModFlags::NONE, Key::Named(key)),
            vec![ESC, b'[', final_byte],
            "{key:?}"
        );
        assert_eq!(
            app_bytes(ModFlags::NONE, Key::Named(key)),
            vec![ESC, b'O', final_byte],
            "{key:?}"
        );
    }
}

#[test]
fn a_modified_cursor_key_is_a_csi_sequence_in_either_mode() {
    // `<C-Right>` is `ESC [ 1 ; 5 C` — 5 = 1 + 4 (Control). Application mode
    // has no modified form of its own, so it sends the same bytes.
    let expected = b"\x1b[1;5C".to_vec();
    assert_eq!(bytes(ModFlags::CTRL, Key::Named(NamedKey::Right)), expected);
    assert_eq!(
        app_bytes(ModFlags::CTRL, Key::Named(NamedKey::Right)),
        expected
    );
}

#[test]
fn every_modifier_lands_in_the_parameter() {
    // Shift 1, Alt 2, Control 4, Super 8, all offset by one.
    assert_eq!(
        bytes(ModFlags::SHIFT, Key::Named(NamedKey::Up)),
        b"\x1b[1;2A".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::ALT, Key::Named(NamedKey::Left)),
        b"\x1b[1;3D".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Home)),
        b"\x1b[1;6H".to_vec()
    );
    assert_eq!(
        bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Named(NamedKey::End)
        ),
        b"\x1b[1;16F".to_vec()
    );
}

#[test]
fn editing_keys_encode_to_the_tilde_family() {
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::Insert)),
        b"\x1b[2~".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::Delete)),
        b"\x1b[3~".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::PageUp)),
        b"\x1b[5~".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::PageDown)),
        b"\x1b[6~".to_vec()
    );
    // The modifier joins as a second parameter.
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::Delete)),
        b"\x1b[3;5~".to_vec()
    );
}

#[test]
fn function_keys_match_the_terminfo_table() {
    // F1–F4 have sequences of their own; F5–F12 use `~` codes whose run skips
    // 16 and 22. These are terminfo's kf1…kf12 for xterm.
    let cases: [(u8, &[u8]); 12] = [
        (1, b"\x1bOP"),
        (2, b"\x1bOQ"),
        (3, b"\x1bOR"),
        (4, b"\x1bOS"),
        (5, b"\x1b[15~"),
        (6, b"\x1b[17~"),
        (7, b"\x1b[18~"),
        (8, b"\x1b[19~"),
        (9, b"\x1b[20~"),
        (10, b"\x1b[21~"),
        (11, b"\x1b[23~"),
        (12, b"\x1b[24~"),
    ];
    for (n, expected) in cases {
        assert_eq!(
            bytes(ModFlags::NONE, Key::Named(NamedKey::F(n))),
            expected.to_vec(),
            "F{n}"
        );
    }
}

#[test]
fn a_modified_function_key_carries_its_parameter() {
    // terminfo kf13 (Shift+F1) IS `ESC [ 1 ; 2 P`, and kf25 (Ctrl+F1) is
    // `ESC [ 1 ; 5 P`.
    assert_eq!(
        bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(1))),
        b"\x1b[1;2P".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::F(1))),
        b"\x1b[1;5P".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::F(5))),
        b"\x1b[15;5~".to_vec()
    );
}

#[test]
fn the_high_function_keys_encode_as_the_shifted_low_ones() {
    // No terminal gives F13–F24 sequences of their own: terminfo spends those
    // slots on Shift plus F1–F12, and that is what a program reads back.
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::F(13))),
        bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(1)))
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::F(17))),
        b"\x1b[15;2~".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::NONE, Key::Named(NamedKey::F(24))),
        b"\x1b[24;2~".to_vec()
    );
}

#[test]
fn a_decoded_key_round_trips_through_the_encoder() {
    // What the host reports and what the pane receives are two ends of one
    // press: every chord the decoder produces has bytes to send.
    let events = [
        press(KeyCode::Char('a'), KeyModifiers::NONE),
        press(KeyCode::Char('H'), KeyModifiers::ALT),
        press(KeyCode::Char('1'), KeyModifiers::CONTROL),
        press(KeyCode::BackTab, KeyModifiers::NONE),
        press(KeyCode::Right, KeyModifiers::CONTROL),
        press(KeyCode::F(6), KeyModifiers::NONE),
    ];
    let expected: [&[u8]; 6] = [b"a", b"\x1bH", b"1", b"\x1b[Z", b"\x1b[1;5C", b"\x1b[17~"];
    for (event, expected) in events.into_iter().zip(expected) {
        let chord = decode_key(event).expect("decodes");
        assert_eq!(encode(chord, false), expected.to_vec(), "{event:?}");
    }
}
