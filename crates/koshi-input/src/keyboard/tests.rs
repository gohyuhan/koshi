//! Keyboard-boundary tests: the complete-event decode (host event → stored
//! event), the decode table (host event → canonical chord) and the encode
//! table (chord → the bytes a program in a pane expects), with modifiers,
//! named keys, function keys, unnamed keys, release suppression, and
//! application-cursor-keys mode.

use super::*;
use crate::host::{KeyCode, KeyEventKind};

#[derive(Clone, Copy)]
struct KeyModifiers(Modifiers);

impl KeyModifiers {
    const NONE: Self = Self(Modifiers::empty());
    const SHIFT: Self = Self(Modifiers::SHIFT);
    const CONTROL: Self = Self(Modifiers::CONTROL);
    const ALT: Self = Self(Modifiers::ALT);
    const SUPER: Self = Self(Modifiers::SUPER);
    const HYPER: Self = Self(Modifiers::HYPER);
    const META: Self = Self(Modifiers::META);
}

impl std::ops::BitOr for KeyModifiers {
    type Output = Self;

    fn bitor(self, right_key_modifiers: Self) -> Self::Output {
        Self(self.0.combine_modifiers(right_key_modifiers.0))
    }
}

/// The bytes this chord sends to a pane in the ordinary (non-application)
/// cursor-key mode, which is every pane's state until a program changes it.
fn encode_key_chord_bytes(modifier_flags: ModFlags, key: Key) -> Vec<u8> {
    encode_key_chord(KeyChord::from_parts(modifier_flags, key), false)
}

/// The bytes this chord sends to a pane whose program turned on
/// application-cursor-keys mode (DECCKM) — vim, less, and most full-screen
/// programs do.
fn encode_application_key_chord_bytes(modifier_flags: ModFlags, key: Key) -> Vec<u8> {
    encode_key_chord(KeyChord::from_parts(modifier_flags, key), true)
}

fn build_key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::from_key_code_and_modifiers(code, modifiers.0)
}

/// The chord one host key event resolves a keybinding against, or `None` when
/// no binding can name it. This is the pair the viewer uses: decode the whole
/// event, then project it.
fn decode_key(host_key_event: KeyEvent) -> Option<KeyChord> {
    decode_key_event(host_key_event).to_binding_chord()
}

fn build_optional_key_chord(modifier_flags: ModFlags, key: Key) -> Option<KeyChord> {
    Some(KeyChord::from_parts(modifier_flags, key))
}

// ---------------------------------------------------------------- decode ----

#[test]
fn characters_decode_to_their_chord() {
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('a'), KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::NONE, Key::Char('a'))
    );
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        build_optional_key_chord(ModFlags::CTRL, Key::Char('c'))
    );
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('b'), KeyModifiers::ALT)),
        build_optional_key_chord(ModFlags::ALT, Key::Char('b'))
    );
}

#[test]
fn uppercase_host_forms_normalize_to_shift_plus_lowercase() {
    // A terminal with no keyboard protocol reports Alt+Shift+h as the capital
    // with only Alt held; the Windows console reports the lowercase with both
    // Alt and Shift. Both are the same chord.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('H'), KeyModifiers::ALT)),
        build_optional_key_chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'))
    );
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Char('h'),
            KeyModifiers::ALT | KeyModifiers::SHIFT
        )),
        build_optional_key_chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'))
    );
}

#[test]
fn a_capital_that_cannot_be_rebuilt_is_never_folded() {
    // `ẞ` (capital sharp S) lowercases to the single char `ß`, and `ß`
    // uppercases to "SS": `Shift + ß` does not rebuild `ẞ`. The chord keeps
    // `ẞ` with no Shift, and the pane receives `ẞ`.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('ẞ'), KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::NONE, Key::Char('ẞ'))
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('ẞ')),
        "ẞ".as_bytes()
    );

    // The same for `İ`, whose lowercase is two chars.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('İ'), KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::NONE, Key::Char('İ'))
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('İ')),
        "İ".as_bytes()
    );
}

#[test]
fn every_folded_capital_reaches_the_pane_as_the_character_typed() {
    // Every capital the decoder folds to `lowercase + Shift`, the encoder
    // rebuilds byte-for-byte; a capital the decoder keeps goes out as itself.
    for typed_character in ['A', 'Z', 'É', 'Ø', 'ẞ', 'İ', 'Å'] {
        let chord = decode_key(build_key_event(
            KeyCode::Char(typed_character),
            KeyModifiers::NONE,
        ))
        .expect("decodes");
        assert_eq!(
            encode_key_chord(chord, false),
            typed_character.to_string().as_bytes(),
            "{typed_character} (U+{:04X}) must reach the pane unchanged",
            typed_character as u32
        );
    }
}

#[test]
fn shifted_non_letter_stands_for_itself() {
    // Shift+1 is `!`, not Shift plus `1`.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('!'), KeyModifiers::SHIFT)),
        build_optional_key_chord(ModFlags::NONE, Key::Char('!'))
    );
}

#[test]
fn spacebar_decodes_to_the_named_key_bindings_spell() {
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char(' '), KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space))
    );
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        build_optional_key_chord(ModFlags::CTRL, Key::Named(NamedKey::Space))
    );
}

#[test]
fn named_keys_decode_exactly() {
    let named_key_cases = [
        (KeyCode::Enter, NamedKey::Enter),
        (KeyCode::Backspace, NamedKey::Backspace),
        (KeyCode::Tab, NamedKey::Tab),
        (KeyCode::Escape, NamedKey::Esc),
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
        (KeyCode::Function(1), NamedKey::F(1)),
        (KeyCode::Function(24), NamedKey::F(24)),
    ];
    for (host_key_code, named_key) in named_key_cases {
        assert_eq!(
            decode_key(build_key_event(host_key_code, KeyModifiers::NONE)),
            build_optional_key_chord(ModFlags::NONE, Key::Named(named_key)),
            "{host_key_code:?}"
        );
    }
}

#[test]
fn named_keys_carry_shift_like_any_other_modifier() {
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Up,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL
        )),
        build_optional_key_chord(ModFlags::SHIFT | ModFlags::CTRL, Key::Named(NamedKey::Up))
    );
}

#[test]
fn backtab_is_shift_tab_even_when_the_host_omits_the_modifier() {
    assert_eq!(
        decode_key(build_key_event(KeyCode::BackTab, KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab))
    );
}

#[test]
fn super_and_meta_both_decode_to_super() {
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('k'), KeyModifiers::SUPER)),
        build_optional_key_chord(ModFlags::SUPER, Key::Char('k'))
    );
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('k'), KeyModifiers::META)),
        build_optional_key_chord(ModFlags::SUPER, Key::Char('k'))
    );
}

#[test]
fn repeat_decodes_and_release_does_not() {
    let mut repeat = build_key_event(KeyCode::Char('a'), KeyModifiers::NONE);
    repeat.key_event_kind = KeyEventKind::Repeat;
    assert_eq!(
        decode_key(repeat),
        build_optional_key_chord(ModFlags::NONE, Key::Char('a'))
    );

    let mut release = build_key_event(KeyCode::Char('a'), KeyModifiers::NONE);
    release.key_event_kind = KeyEventKind::Release;
    assert_eq!(decode_key(release), None);
}

#[test]
fn keys_the_chord_model_cannot_name_are_not_input() {
    let unsupported_host_key_codes = [KeyCode::Function(25), KeyCode::Codepoint(57_441)];
    for host_key_code in unsupported_host_key_codes {
        assert_eq!(
            decode_key(build_key_event(host_key_code, KeyModifiers::NONE)),
            None,
            "{host_key_code:?}"
        );
    }
}

// ---------------------------------------------------------------- encode ----

#[test]
fn characters_encode_to_their_bytes() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('a')),
        vec![b'a']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Char('a')),
        vec![b'A']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('!')),
        vec![b'!']
    );
    // A multi-byte character keeps every byte of its UTF-8 form.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('é')),
        vec![0xc3, 0xa9]
    );
}

#[test]
fn control_characters_fold_into_their_c0_byte() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('a')),
        vec![0x01]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('c')),
        vec![0x03]
    );
    // Control plus Shift plus a letter sends the same C0 byte as Control plus
    // the letter.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a')),
        vec![0x01]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('[')),
        vec![0x1b]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('?')),
        vec![0x7f]
    );
}

#[test]
fn control_plus_a_character_with_no_c0_byte_sends_the_character() {
    // No control code stands for `<C-1>`: the digit goes out by itself.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('1')),
        vec![b'1']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('9')),
        vec![b'9']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char(';')),
        vec![b';']
    );
}

#[test]
fn the_control_digits_carry_the_codes_the_letter_run_cannot_reach() {
    // Control clears the top bits, which covers `@`..`_` — and terminals hand
    // the leftover control codes to the digit row. `2` is NUL, `3` is ESCAPE_BYTE,
    // `4`..`7` are 0x1c..0x1f, and `8` is DEL.
    let control_digit_cases = [
        ('2', 0x00),
        ('3', 0x1b),
        ('4', 0x1c),
        ('5', 0x1d),
        ('6', 0x1e),
        ('7', 0x1f),
        ('8', 0x7f),
    ];
    for (digit, encoded_byte) in control_digit_cases {
        assert_eq!(
            encode_key_chord_bytes(ModFlags::CTRL, Key::Char(digit)),
            vec![encoded_byte],
            "<C-{digit}>"
        );
    }

    // Alt composes with them exactly as it does with a letter.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT | ModFlags::CTRL, Key::Char('4')),
        vec![ESCAPE_BYTE, 0x1c]
    );
}

#[test]
fn the_two_spellings_of_one_control_code_send_the_same_byte() {
    // One key press has two host spellings: VT input maps the terminal's
    // `0x1c` to `<C-4>`, while enhanced input can report the key's own
    // character as `<C-\>`. Both leave here as `0x1c`.
    for (digit, punctuation) in [('4', '\\'), ('5', ']'), ('6', '^'), ('7', '_')] {
        assert_eq!(
            encode_key_chord_bytes(ModFlags::CTRL, Key::Char(digit)),
            encode_key_chord_bytes(ModFlags::CTRL, Key::Char(punctuation)),
            "<C-{digit}> and <C-{punctuation}> are one key press"
        );
    }
}

#[test]
fn alt_prefixes_escape_and_composes_with_control() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Char('b')),
        vec![ESCAPE_BYTE, b'b']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')),
        vec![ESCAPE_BYTE, b'H']
    );
    // Alt+Ctrl+a is the ESCAPE_BYTE prefix in front of Ctrl+a's byte.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT | ModFlags::CTRL, Key::Char('a')),
        vec![ESCAPE_BYTE, 0x01]
    );
}

#[test]
fn super_rides_the_parameter_but_has_no_c0_form() {
    // A C0 byte has no field for Super: the key arrives bare.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SUPER, Key::Char('a')),
        vec![b'a']
    );
    // A CSI key has the modifier parameter, and Super is its bit 8.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SUPER, Key::Named(NamedKey::Up)),
        b"\x1b[1;9A".to_vec()
    );
}

#[test]
fn c0_named_keys_encode_to_their_bytes() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Enter)),
        vec![b'\r']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        vec![b'\t']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        vec![ESCAPE_BYTE]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Space)),
        vec![b' ']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Backspace)),
        vec![0x7f]
    );
}

#[test]
fn control_and_alt_reshape_the_c0_named_keys() {
    // Ctrl+Backspace is the BS byte: a shell reads it as "erase a word",
    // where the plain DEL byte erases one character.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Backspace)),
        vec![0x08]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Named(NamedKey::Backspace)),
        vec![ESCAPE_BYTE, 0x7f]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Space)),
        vec![0x00]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Named(NamedKey::Enter)),
        vec![ESCAPE_BYTE, b'\r']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Named(NamedKey::Esc)),
        vec![ESCAPE_BYTE, ESCAPE_BYTE]
    );
}

#[test]
fn shift_tab_has_a_sequence_of_its_own() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESCAPE_BYTE, b'[', b'Z']
    );
}

#[test]
fn alt_shift_tab_keeps_the_alt_prefix() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESCAPE_BYTE, ESCAPE_BYTE, b'[', b'Z']
    );
}

#[test]
fn cursor_keys_follow_the_panes_application_mode() {
    let cursor_key_cases = [
        (NamedKey::Up, b'A'),
        (NamedKey::Down, b'B'),
        (NamedKey::Right, b'C'),
        (NamedKey::Left, b'D'),
        (NamedKey::End, b'F'),
        (NamedKey::Home, b'H'),
    ];
    for (named_key, final_byte) in cursor_key_cases {
        assert_eq!(
            encode_key_chord_bytes(ModFlags::NONE, Key::Named(named_key)),
            vec![ESCAPE_BYTE, b'[', final_byte],
            "{named_key:?}"
        );
        assert_eq!(
            encode_application_key_chord_bytes(ModFlags::NONE, Key::Named(named_key)),
            vec![ESCAPE_BYTE, b'O', final_byte],
            "{named_key:?}"
        );
    }
}

#[test]
fn a_modified_cursor_key_is_a_csi_sequence_in_either_mode() {
    // `<C-Right>` is `ESCAPE_BYTE [ 1 ; 5 C` — 5 = 1 + 4 (Control). Application mode
    // sends the same bytes for a modified key.
    let expected_cursor_key_bytes = b"\x1b[1;5C".to_vec();
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Right)),
        expected_cursor_key_bytes
    );
    assert_eq!(
        encode_application_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Right)),
        expected_cursor_key_bytes
    );
}

#[test]
fn every_modifier_lands_in_the_parameter() {
    // Shift 1, Alt 2, Control 4, Super 8, all offset by one.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Named(NamedKey::Up)),
        b"\x1b[1;2A".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Named(NamedKey::Left)),
        b"\x1b[1;3D".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Home)),
        b"\x1b[1;6H".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Named(NamedKey::End)
        ),
        b"\x1b[1;16F".to_vec()
    );
}

#[test]
fn editing_keys_encode_to_the_tilde_family() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Insert)),
        b"\x1b[2~".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::Delete)),
        b"\x1b[3~".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::PageUp)),
        b"\x1b[5~".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::PageDown)),
        b"\x1b[6~".to_vec()
    );
    // The modifier joins as a second parameter.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Delete)),
        b"\x1b[3;5~".to_vec()
    );
}

#[test]
fn function_keys_match_the_terminfo_table() {
    // F1–F4 have sequences of their own; F5–F12 use `~` codes whose run skips
    // 16 and 22. These are terminfo's kf1…kf12 for xterm.
    let function_key_cases: [(u8, &[u8]); 12] = [
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
    for (function_number, expected_bytes) in function_key_cases {
        assert_eq!(
            encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(function_number)),),
            expected_bytes.to_vec(),
            "F{function_number}"
        );
    }
}

#[test]
fn a_modified_function_key_carries_its_parameter() {
    // terminfo kf13 (Shift+F1) is `ESCAPE_BYTE [ 1 ; 2 P`, and kf25 (Ctrl+F1) is
    // `ESCAPE_BYTE [ 1 ; 5 P`.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(1))),
        b"\x1b[1;2P".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::F(1))),
        b"\x1b[1;5P".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::F(5))),
        b"\x1b[15;5~".to_vec()
    );
}

#[test]
fn the_high_function_keys_encode_as_the_shifted_low_ones() {
    // terminfo lists F13–F24 as Shift plus F1–F12; a program reads those
    // bytes back.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(13))),
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(1)))
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(17))),
        b"\x1b[15;2~".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(24))),
        b"\x1b[24;2~".to_vec()
    );
}

#[test]
fn a_decoded_key_round_trips_through_the_encoder() {
    // What the host reports and what the pane receives are two ends of one
    // press: every chord the decoder produces has bytes to send.
    let host_key_events = [
        build_key_event(KeyCode::Char('a'), KeyModifiers::NONE),
        build_key_event(KeyCode::Char('H'), KeyModifiers::ALT),
        build_key_event(KeyCode::Char('1'), KeyModifiers::CONTROL),
        build_key_event(KeyCode::Tab, KeyModifiers::SHIFT),
        build_key_event(KeyCode::Right, KeyModifiers::CONTROL),
        build_key_event(KeyCode::Function(6), KeyModifiers::NONE),
    ];
    let expected_encoded_key_bytes: [&[u8]; 6] =
        [b"a", b"\x1bH", b"1", b"\x1b[Z", b"\x1b[1;5C", b"\x1b[17~"];
    for (host_key_event, expected_key_bytes) in
        host_key_events.into_iter().zip(expected_encoded_key_bytes)
    {
        let key_chord = decode_key(host_key_event.clone()).expect("decodes");
        assert_eq!(
            encode_key_chord(key_chord, false),
            expected_key_bytes.to_vec(),
            "{host_key_event:?}"
        );
    }
}

// ------------------------------------------------ decode: modifier matrix ----

#[test]
fn shift_plus_lowercase_letter_carries_shift() {
    // A host may report Shift+a as the lowercase char with Shift held (as the
    // Windows console does). The chord carries the Shift.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('a'), KeyModifiers::SHIFT)),
        build_optional_key_chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn a_bare_capital_folds_to_shift_plus_lowercase() {
    // The other host form of the same press: the capital with no Shift held.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('A'), KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn a_capital_with_shift_also_held_stays_one_shift() {
    // A host that reports both the capital and the Shift modifier yields one
    // Shift: the bitmap is idempotent.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('A'), KeyModifiers::SHIFT)),
        build_optional_key_chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn control_and_alt_together_decode_on_a_letter() {
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )),
        build_optional_key_chord(ModFlags::CTRL | ModFlags::ALT, Key::Char('c'))
    );
}

#[test]
fn every_modifier_at_once_decodes_on_a_lowercase_letter() {
    // Ctrl+Alt+Super reported by the host, plus the Shift the lowercase form
    // needs: all four land, and the letter stays folded lowercase.
    let held_modifiers =
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::SHIFT;
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('h'), held_modifiers)),
        build_optional_key_chord(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SUPER | ModFlags::SHIFT,
            Key::Char('h')
        )
    );
}

#[test]
fn a_control_digit_keeps_its_control_on_decode() {
    // The digit is not folded and Shift is not a letter's here; only Control
    // rides along.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('4'), KeyModifiers::CONTROL)),
        build_optional_key_chord(ModFlags::CTRL, Key::Char('4'))
    );
}

#[test]
fn a_held_shift_is_dropped_from_a_non_letter_character() {
    // Shift is a letter's case only. A host that reports `!` while Shift is
    // still held yields `<C-!>`, not `<C-S-!>`.
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Char('!'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        build_optional_key_chord(ModFlags::CTRL, Key::Char('!'))
    );
}

#[test]
fn the_spacebar_carries_its_modifiers_as_a_named_key() {
    // The space character becomes the named key, and a held Shift joins it like
    // any other named-key modifier.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char(' '), KeyModifiers::SHIFT)),
        build_optional_key_chord(ModFlags::SHIFT, Key::Named(NamedKey::Space))
    );
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )),
        build_optional_key_chord(ModFlags::CTRL | ModFlags::ALT, Key::Named(NamedKey::Space))
    );
}

#[test]
fn backtab_folds_shift_in_alongside_other_modifiers() {
    // BackTab is Shift+Tab; a Control held with it lands beside that Shift.
    assert_eq!(
        decode_key(build_key_event(KeyCode::BackTab, KeyModifiers::CONTROL)),
        build_optional_key_chord(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Tab))
    );
}

#[test]
fn backtab_with_shift_already_held_stays_one_shift() {
    // A host that sets the Shift modifier on BackTab too yields one Shift.
    assert_eq!(
        decode_key(build_key_event(KeyCode::BackTab, KeyModifiers::SHIFT)),
        build_optional_key_chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab))
    );
}

#[test]
fn a_function_key_carries_its_modifier_on_decode() {
    assert_eq!(
        decode_key(build_key_event(KeyCode::Function(6), KeyModifiers::CONTROL)),
        build_optional_key_chord(ModFlags::CTRL, Key::Named(NamedKey::F(6)))
    );
}

#[test]
fn a_modified_repeat_decodes_and_a_modified_release_does_not() {
    // The release/repeat rule is independent of which modifiers are held.
    let mut repeat = build_key_event(KeyCode::Char('a'), KeyModifiers::CONTROL);
    repeat.key_event_kind = KeyEventKind::Repeat;
    assert_eq!(
        decode_key(repeat),
        build_optional_key_chord(ModFlags::CTRL, Key::Char('a'))
    );

    let mut release = build_key_event(KeyCode::Char('a'), KeyModifiers::CONTROL);
    release.key_event_kind = KeyEventKind::Release;
    assert_eq!(decode_key(release), None);
}

#[test]
fn hyper_is_dropped_on_decode() {
    // The chord model has no Hyper: a held Hyper leaves nothing in the chord,
    // and the other modifiers land as usual.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('a'), KeyModifiers::HYPER)),
        build_optional_key_chord(ModFlags::NONE, Key::Char('a'))
    );
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Up,
            KeyModifiers::HYPER | KeyModifiers::CONTROL
        )),
        build_optional_key_chord(ModFlags::CTRL, Key::Named(NamedKey::Up))
    );
}

#[test]
fn a_release_of_a_named_key_is_not_input() {
    let mut release = build_key_event(KeyCode::Up, KeyModifiers::NONE);
    release.key_event_kind = KeyEventKind::Release;
    assert_eq!(decode_key(release), None);
}

#[test]
fn a_capital_with_control_folds_and_keeps_control() {
    // The capital folds to lowercase plus Shift, and Control lands beside it.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('A'), KeyModifiers::CONTROL)),
        build_optional_key_chord(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a'))
    );
}

// --------------------------------------------- decode: hostile characters ----

#[test]
fn a_control_byte_arriving_as_a_character_decodes_without_panic() {
    // A raw C0 byte handed through as `Char` decodes as the plain character,
    // not as the named key that sends that byte. NUL, DEL, and ESCAPE_BYTE as
    // characters:
    for character in ['\u{0}', '\u{7f}', '\u{1b}'] {
        assert_eq!(
            decode_key(build_key_event(
                KeyCode::Char(character),
                KeyModifiers::NONE
            )),
            build_optional_key_chord(ModFlags::NONE, Key::Char(character)),
            "U+{:04X}",
            character as u32
        );
    }
}

#[test]
fn a_c1_range_character_decodes_to_itself() {
    // A byte in the C1 range (0x80) as a character folds nowhere and carries no
    // implicit modifier.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Char('\u{80}'), KeyModifiers::NONE)),
        build_optional_key_chord(ModFlags::NONE, Key::Char('\u{80}'))
    );
}

#[test]
fn the_top_of_the_character_range_decodes_without_panic() {
    // `char::MAX` (U+10FFFF) is not uppercase: it folds nowhere and decodes as
    // itself.
    assert_eq!(
        decode_key(build_key_event(
            KeyCode::Char(char::MAX),
            KeyModifiers::NONE
        )),
        build_optional_key_chord(ModFlags::NONE, Key::Char(char::MAX))
    );
}

// --------------------------------------------- encode: hostile characters ----

#[test]
fn a_control_character_with_no_c0_mapping_encodes_as_its_own_bytes() {
    // NUL, DEL, and ESCAPE_BYTE as characters have no entry in the control table: with
    // Control held they send their own byte.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('\u{0}')),
        vec![0x00]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('\u{0}')),
        vec![0x00]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('\u{7f}')),
        vec![0x7f]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('\u{7f}')),
        vec![0x7f]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('\u{1b}')),
        vec![0x1b]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('\u{1b}')),
        vec![0x1b]
    );
}

#[test]
fn a_c1_and_max_character_encode_to_their_utf8_bytes() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('\u{80}')),
        vec![0xc2, 0x80]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char(char::MAX)),
        vec![0xf4, 0x8f, 0xbf, 0xbf]
    );
}

#[test]
fn a_tab_character_and_the_tab_key_send_the_same_byte() {
    // `\t` arriving as a plain character (not the Tab key) still encodes to the
    // tab byte, matching the named key.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::NONE, Key::Char('\t')),
        vec![0x09]
    );
}

#[test]
fn shift_is_dropped_when_the_capital_cannot_be_rebuilt() {
    // `ß` uppercases to the two-char "SS": Shift restores no single capital.
    // The encoder sends `ß` and nothing for the Shift.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Char('ß')),
        vec![0xc3, 0x9f]
    );
}

// ---------------------------------------- encode: modifier combinations ------

#[test]
fn control_is_ignored_on_the_c0_keys_that_have_no_control_form() {
    // Enter, Tab, and Esc carry only Alt in the byte stream; with Control held
    // they send the bare byte.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Enter)),
        vec![b'\r']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Tab)),
        vec![b'\t']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::Esc)),
        vec![ESCAPE_BYTE]
    );
}

#[test]
fn alt_prefixes_escape_on_the_c0_named_keys() {
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Named(NamedKey::Tab)),
        vec![ESCAPE_BYTE, b'\t']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT, Key::Named(NamedKey::Space)),
        vec![ESCAPE_BYTE, b' ']
    );
}

#[test]
fn control_plus_alt_reshapes_the_byte_then_prefixes_escape() {
    // Control picks the special byte (NUL for Space, BS for Backspace) and Alt
    // wraps the ESCAPE_BYTE in front of it.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL | ModFlags::ALT, Key::Named(NamedKey::Space)),
        vec![ESCAPE_BYTE, 0x00]
    );
    assert_eq!(
        encode_key_chord_bytes(
            ModFlags::CTRL | ModFlags::ALT,
            Key::Named(NamedKey::Backspace)
        ),
        vec![ESCAPE_BYTE, 0x08]
    );
}

#[test]
fn super_and_alt_together_on_a_character_keep_only_the_escape_prefix() {
    // A C0 character has no field for Super: Alt's ESCAPE_BYTE is all that goes out.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::ALT | ModFlags::SUPER, Key::Char('a')),
        vec![ESCAPE_BYTE, b'a']
    );
}

#[test]
fn control_plus_super_on_a_cursor_key_sums_in_the_parameter() {
    // Super is bit 8, Control bit 4: the parameter is 1 + 4 + 8 = 13.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL | ModFlags::SUPER, Key::Named(NamedKey::Up)),
        b"\x1b[1;13A".to_vec()
    );
}

#[test]
fn every_modifier_on_an_editing_key_fills_the_parameter() {
    // Shift 1 + Alt 2 + Control 4 + Super 8, offset by one, is 16.
    assert_eq!(
        encode_key_chord_bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Named(NamedKey::Delete)
        ),
        b"\x1b[3;16~".to_vec()
    );
}

#[test]
fn every_modifier_on_a_low_function_key_fills_the_parameter() {
    assert_eq!(
        encode_key_chord_bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Named(NamedKey::F(1))
        ),
        b"\x1b[1;16P".to_vec()
    );
}

#[test]
fn a_modified_high_function_key_adds_its_modifier_to_the_shift() {
    // F13 already carries the Shift that stands for it; a Control held with it
    // joins that Shift — parameter 1 + 1 (Shift) + 4 (Control) = 6.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Named(NamedKey::F(13))),
        b"\x1b[1;6P".to_vec()
    );
}

#[test]
fn shift_on_a_high_function_key_does_not_double_the_shift() {
    // F13 is Shift+F1; a Shift held on top of it is still one Shift.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(13))),
        b"\x1b[1;2P".to_vec()
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(13))),
        encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(13)))
    );
}

#[test]
fn every_modifier_on_a_high_function_key_fills_the_parameter() {
    // F24 is Shift+F12; Control, Alt and Super join that Shift:
    // 1 + 1 + 2 + 4 + 8 = 16.
    assert_eq!(
        encode_key_chord_bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SUPER,
            Key::Named(NamedKey::F(24))
        ),
        b"\x1b[24;16~".to_vec()
    );
}

#[test]
fn control_and_super_have_no_place_in_shift_tab() {
    // `ESCAPE_BYTE [ Z` carries no modifier parameter: Shift+Tab sends it with Control
    // or Super held too, and Alt keeps its `ESCAPE_BYTE` prefix.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESCAPE_BYTE, b'[', b'Z']
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::SUPER | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESCAPE_BYTE, b'[', b'Z']
    );
    assert_eq!(
        encode_key_chord_bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT,
            Key::Named(NamedKey::Tab)
        ),
        vec![ESCAPE_BYTE, ESCAPE_BYTE, b'[', b'Z']
    );
}

#[test]
fn application_mode_changes_only_the_cursor_keys() {
    // DECCKM moves the cursor keys and Home/End between `ESCAPE_BYTE [` and `ESCAPE_BYTE O`.
    // Every other chord sends the same bytes in both modes.
    let application_mode_key_cases: [(ModFlags, Key, &[u8]); 9] = [
        (ModFlags::NONE, Key::Char('a'), b"a"),
        (ModFlags::CTRL, Key::Char('a'), b"\x01"),
        (ModFlags::NONE, Key::Named(NamedKey::Enter), b"\r"),
        (ModFlags::NONE, Key::Named(NamedKey::Tab), b"\t"),
        (ModFlags::SHIFT, Key::Named(NamedKey::Tab), b"\x1b[Z"),
        (ModFlags::NONE, Key::Named(NamedKey::Insert), b"\x1b[2~"),
        (ModFlags::CTRL, Key::Named(NamedKey::Delete), b"\x1b[3;5~"),
        (ModFlags::NONE, Key::Named(NamedKey::F(1)), b"\x1bOP"),
        (ModFlags::NONE, Key::Named(NamedKey::F(5)), b"\x1b[15~"),
    ];
    for (modifier_flags, key, expected_bytes) in application_mode_key_cases {
        assert_eq!(
            encode_application_key_chord_bytes(modifier_flags, key),
            expected_bytes.to_vec(),
            "{modifier_flags}{key}"
        );
        assert_eq!(
            encode_key_chord_bytes(modifier_flags, key),
            expected_bytes.to_vec(),
            "{modifier_flags}{key}"
        );
    }
}

// ------------------------------------------ round trip: hostile decode → encode ----

#[test]
fn hostile_and_edge_characters_round_trip_to_their_own_bytes() {
    // Whatever the decoder keeps of an odd character, the encoder sends the
    // character's own bytes back.
    for typed_character in ['\u{0}', '\u{7f}', '\u{1b}', '\u{80}', '\t', char::MAX] {
        let chord = decode_key(build_key_event(
            KeyCode::Char(typed_character),
            KeyModifiers::NONE,
        ))
        .expect("decodes");
        assert_eq!(
            encode_key_chord(chord, false),
            typed_character.to_string().as_bytes(),
            "U+{:04X} must round-trip",
            typed_character as u32
        );
    }
}

// ------------------------------------------------------- table boundaries ----

#[test]
fn function_key_zero_is_not_a_key_the_model_names() {
    // `F(24)` is the top of the run and `F(25)` is rejected; `F(0)` is the
    // bottom of the same bound and is rejected too.
    assert_eq!(
        decode_key(build_key_event(KeyCode::Function(0), KeyModifiers::NONE)),
        None
    );
}

#[test]
#[should_panic(expected = "bound F to 1..=24")]
fn encoding_function_key_zero_panics() {
    let _ = encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(0)));
}

#[test]
#[should_panic(expected = "bound F to 1..=24")]
fn encoding_a_function_key_above_the_run_panics() {
    let _ = encode_key_chord_bytes(ModFlags::NONE, Key::Named(NamedKey::F(25)));
}

#[test]
fn the_control_fold_covers_its_run_and_stops_at_both_ends() {
    // Control clears the top bits over `@`..`_`: `@` opens the run at NUL and
    // `_` closes it at 0x1f. A letter is its capital's version of that fold, so
    // `z` ends the letter run at 0x1a.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('@')),
        vec![0x00]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('_')),
        vec![0x1f]
    );
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('z')),
        vec![0x1a]
    );
    // The backtick sits between the two runs and belongs to neither: it sends
    // its own byte, not the NUL that `@` sends.
    assert_eq!(
        encode_key_chord_bytes(ModFlags::CTRL, Key::Char('`')),
        vec![b'`']
    );
}

#[test]
fn append_decimal_writes_every_digit_of_the_decimal_number() {
    let decimal_number_cases: [(u32, &[u8]); 8] = [
        (0, b"0"),
        (9, b"9"),
        (10, b"10"),
        (99, b"99"),
        (100, b"100"),
        (255, b"255"),
        (256, b"256"),
        (1_114_109, b"1114109"),
    ];
    for (decimal_number, expected_bytes) in decimal_number_cases {
        let mut decimal_digits = Vec::new();
        append_decimal(&mut decimal_digits, decimal_number);
        assert_eq!(decimal_digits, expected_bytes.to_vec(), "{decimal_number}");
    }
}

// ------------------------------------------ decode: the complete event ----

#[test]
fn the_complete_event_keeps_every_field_the_host_reported() {
    let host_key_event = KeyEvent {
        code: KeyCode::Char('q'),
        key_event_kind: KeyEventKind::Release,
        modifiers: Modifiers::SHIFT | Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK,
        shifted_key: Some('"'),
        base_layout_key: Some('\''),
        associated_text: "q".to_string(),
    };
    assert_eq!(
        decode_key_event(host_key_event),
        KeyInput {
            key: KeyIdentity::Key(Key::Char('q')),
            key_event_kind: KeyEventKind::Release,
            shifted_key: Some('"'),
            base_layout_key: Some('\''),
            associated_text: "q".to_string(),
            modifier_flags: KeyModifierFlags::SHIFT
                | KeyModifierFlags::CAPS_LOCK
                | KeyModifierFlags::NUM_LOCK,
        }
    );
}

#[test]
fn a_release_keeps_its_kind_in_the_stored_event() {
    let mut host_key_event = build_key_event(KeyCode::Char('a'), KeyModifiers::NONE);
    host_key_event.key_event_kind = KeyEventKind::Release;
    let key_input = decode_key_event(host_key_event);
    assert_eq!(key_input.key_event_kind, KeyEventKind::Release);
    assert_eq!(key_input.key, KeyIdentity::Key(Key::Char('a')));
    // Only the binding projection refuses a release.
    assert_eq!(key_input.to_binding_chord(), None);
}

#[test]
fn every_host_modifier_bit_reaches_the_stored_bitmap() {
    let host_modifiers = Modifiers::SHIFT
        | Modifiers::ALT
        | Modifiers::CONTROL
        | Modifiers::SUPER
        | Modifiers::HYPER
        | Modifiers::META
        | Modifiers::CAPS_LOCK
        | Modifiers::NUM_LOCK;
    let key_input = decode_key_event(KeyEvent::from_key_code_and_modifiers(
        KeyCode::Char('a'),
        host_modifiers,
    ));
    assert_eq!(key_input.modifier_flags.bits(), 0b1111_1111);
}

#[test]
fn back_tab_becomes_tab_with_shift_held() {
    let key_input = decode_key_event(build_key_event(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(key_input.key, KeyIdentity::Key(Key::Named(NamedKey::Tab)));
    assert!(key_input
        .modifier_flags
        .has_all_modifiers(KeyModifierFlags::SHIFT));
}

#[test]
fn an_unnamed_key_keeps_its_codepoint_through_the_decode() {
    assert_eq!(
        decode_key_event(build_key_event(
            KeyCode::Codepoint(57_441),
            KeyModifiers::NONE
        ))
        .key,
        KeyIdentity::Codepoint(57_441)
    );
    // Key number 0 marks an event that carries only text.
    let text_only = decode_key_event(KeyEvent {
        associated_text: "å".to_string(),
        ..build_key_event(KeyCode::Codepoint(0), KeyModifiers::NONE)
    });
    assert_eq!(
        text_only.key,
        KeyIdentity::Codepoint(koshi_core::key::TEXT_ONLY_KEY_CODEPOINT)
    );
    assert_eq!(text_only.associated_text, "å");
    assert_eq!(text_only.shifted_key, None);
    assert_eq!(text_only.base_layout_key, None);
    assert_eq!(text_only.modifier_flags, KeyModifierFlags::NONE);
}

#[test]
fn a_function_key_above_the_last_one_is_unnamed_rather_than_dropped() {
    let key_input = decode_key_event(build_key_event(KeyCode::Function(25), KeyModifiers::NONE));
    assert_eq!(key_input.key, KeyIdentity::Unnamed);
    assert_eq!(key_input.to_binding_chord(), None);
}

#[test]
fn a_shifted_alternative_leaves_the_binding_projection_unchanged() {
    // Shift plus `1` reports `!` as the shifted key; the binding still sees `!`.
    let shifted_digit = KeyEvent {
        shifted_key: Some('!'),
        ..KeyEvent::from_key_code_and_modifiers(KeyCode::Char('1'), Modifiers::SHIFT)
    };
    assert_eq!(
        decode_key(shifted_digit),
        build_optional_key_chord(ModFlags::NONE, Key::Char('!'))
    );

    // Shift plus `a` reports `A`; the binding still sees `<S-a>`.
    let shifted_letter = KeyEvent {
        shifted_key: Some('A'),
        ..KeyEvent::from_key_code_and_modifiers(KeyCode::Char('a'), Modifiers::SHIFT)
    };
    assert_eq!(
        decode_key(shifted_letter),
        build_optional_key_chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn associated_text_never_reaches_the_binding_chord() {
    // A key that carries text still binds on the key, not on the text.
    let key_with_text = KeyEvent {
        associated_text: "a".to_string(),
        ..build_key_event(KeyCode::Char('a'), KeyModifiers::CONTROL)
    };
    assert_eq!(
        decode_key(key_with_text),
        build_optional_key_chord(ModFlags::CTRL, Key::Char('a'))
    );
}

#[test]
fn the_host_and_stored_modifier_bitmaps_use_the_same_bit_for_the_same_modifier() {
    // `to_key_modifier_flags` reinterprets the raw byte, so a bit that moves
    // in one bitmap and not the other would remap every modifier silently.
    let paired_modifiers = [
        (Modifiers::SHIFT, KeyModifierFlags::SHIFT),
        (Modifiers::ALT, KeyModifierFlags::ALT),
        (Modifiers::CONTROL, KeyModifierFlags::CTRL),
        (Modifiers::SUPER, KeyModifierFlags::SUPER),
        (Modifiers::HYPER, KeyModifierFlags::HYPER),
        (Modifiers::META, KeyModifierFlags::META),
        (Modifiers::CAPS_LOCK, KeyModifierFlags::CAPS_LOCK),
        (Modifiers::NUM_LOCK, KeyModifierFlags::NUM_LOCK),
    ];
    for (host_modifier, stored_modifier) in paired_modifiers {
        assert_eq!(
            host_modifier.to_key_modifier_flags(),
            stored_modifier,
            "{host_modifier:?} and {stored_modifier:?} must share one bit"
        );
    }
}

// ------------------------------- end to end: host bytes to the pane bytes ----

/// The bytes a pane receives for one run of terminal input, with the keys no
/// binding consumed encoded in order.
///
/// This is the pair the runtime uses at `handle_key_press`: [`decode_key`] for
/// the chord, then [`encode_key_chord`] for the bytes it writes to the pane.
fn encode_terminal_input_for_pane(terminal_input_bytes: &[u8]) -> Vec<u8> {
    let mut parser = crate::host::Parser::default();
    parser.process_input_bytes(terminal_input_bytes);
    let mut pane_bytes = Vec::new();
    while let Some(host_event) = parser.remove_next_pending_event() {
        let crate::host::Event::Key(host_key_event) = host_event else {
            continue;
        };
        if let Some(key_chord) = decode_key(host_key_event) {
            pane_bytes.extend(encode_key_chord(key_chord, false));
        }
    }
    pane_bytes
}

#[test]
fn a_lock_modifier_is_captured_and_still_reaches_the_pane_as_the_plain_key() {
    // The modifier field reports Caps Lock as 64 and Num Lock as 128, always
    // one more in the escape code. Both are stored and neither reaches a pane.
    assert_eq!(encode_terminal_input_for_pane(b"\x1b[97;65u"), b"a");
    assert_eq!(encode_terminal_input_for_pane(b"\x1b[97;129u"), b"a");
    assert_eq!(encode_terminal_input_for_pane(b"\x1b[97;193u"), b"a");
    // Control still lands beside a held lock: 69 is 1 + 4 + 64.
    assert_eq!(encode_terminal_input_for_pane(b"\x1b[97;69u"), b"\x01");
}

#[test]
fn every_reachable_key_report_writes_the_bytes_it_always_wrote() {
    let key_reports_and_pane_bytes: [(&[u8], &[u8]); 9] = [
        // A plain character on the byte path.
        (b"a", b"a"),
        // The same character as a disambiguated escape code.
        (b"\x1b[97u", b"a"),
        // Shift with the terminal's shifted alternative.
        (b"\x1b[97:65;2u", b"A"),
        // Shift on a digit: the shifted alternative stands for itself.
        (b"\x1b[49:33;2u", b"!"),
        // Control folds into the C0 byte.
        (b"\x1b[97;5u", b"\x01"),
        // A release writes nothing.
        (b"\x1b[97;1:3u", b""),
        // A repeat writes the key again.
        (b"\x1b[97;1:2u", b"a"),
        // Shift+Tab keeps its own sequence.
        (b"\x1b[Z", b"\x1b[Z"),
        // A key no binding can name writes nothing.
        (b"\x1b[57441u", b""),
    ];
    for (terminal_input_bytes, expected_pane_bytes) in key_reports_and_pane_bytes {
        assert_eq!(
            encode_terminal_input_for_pane(terminal_input_bytes),
            expected_pane_bytes,
            "{terminal_input_bytes:?}"
        );
    }
}

#[test]
fn a_text_parameter_koshi_cannot_use_never_costs_the_pane_its_key() {
    // Enter reported with its own byte as text, and with a number that is no
    // character at all, both still write a carriage return.
    assert_eq!(encode_terminal_input_for_pane(b"\x1b[13;;13u"), b"\r");
    assert_eq!(encode_terminal_input_for_pane(b"\x1b[13;;1114112u"), b"\r");
}

// ------------------------------ encode: the complete event, for one pane ----

/// A press of `key` with `modifier_flags` held, reporting no alternatives and
/// no text.
fn build_key_press(key: Key, modifier_flags: KeyModifierFlags) -> KeyInput {
    KeyInput {
        key: KeyIdentity::Key(key),
        key_event_kind: koshi_core::key::KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags,
    }
}

#[test]
fn a_pane_that_pushed_no_flag_reads_the_bytes_it_reads_today() {
    let shift_enter = build_key_press(Key::Named(NamedKey::Enter), KeyModifierFlags::SHIFT);
    assert_eq!(
        encode_key_input(&shift_enter, 0, false, ExtendedKeysMode::OnRequest),
        b"\r".to_vec()
    );

    let control_i = build_key_press(Key::Char('i'), KeyModifierFlags::CTRL);
    assert_eq!(
        encode_key_input(&control_i, 0, false, ExtendedKeysMode::OnRequest),
        vec![0x09]
    );

    let tab = build_key_press(Key::Named(NamedKey::Tab), KeyModifierFlags::NONE);
    assert_eq!(
        encode_key_input(&tab, 0, false, ExtendedKeysMode::OnRequest),
        vec![0x09]
    );
}

#[test]
fn flag_one_keeps_enter_legacy_and_escape_codes_every_other_silent_key() {
    let shift_enter = build_key_press(Key::Named(NamedKey::Enter), KeyModifierFlags::SHIFT);
    assert_eq!(
        encode_key_input(&shift_enter, 1, false, ExtendedKeysMode::OnRequest),
        b"\r".to_vec()
    );

    let shift_escape = build_key_press(Key::Named(NamedKey::Esc), KeyModifierFlags::SHIFT);
    assert_eq!(
        encode_key_input(&shift_escape, 1, false, ExtendedKeysMode::OnRequest),
        b"\x1b[27;2u".to_vec()
    );

    let typed_a = build_key_press(Key::Char('a'), KeyModifierFlags::NONE);
    assert_eq!(
        encode_key_input(&typed_a, 1, false, ExtendedKeysMode::OnRequest),
        b"a".to_vec()
    );
}

#[test]
fn flag_eight_escape_codes_every_key_and_keeps_functional_forms() {
    let shift_enter = build_key_press(Key::Named(NamedKey::Enter), KeyModifierFlags::SHIFT);
    assert_eq!(
        encode_key_input(&shift_enter, 8, false, ExtendedKeysMode::OnRequest),
        b"\x1b[13;2u".to_vec()
    );

    let space = build_key_press(Key::Named(NamedKey::Space), KeyModifierFlags::NONE);
    assert_eq!(
        encode_key_input(&space, 8, false, ExtendedKeysMode::OnRequest),
        b"\x1b[32u".to_vec()
    );

    let up = build_key_press(Key::Named(NamedKey::Up), KeyModifierFlags::NONE);
    assert_eq!(
        encode_key_input(&up, 8, false, ExtendedKeysMode::OnRequest),
        b"\x1b[A".to_vec()
    );

    let every_modifier = build_key_press(Key::Char('a'), KeyModifierFlags::from_bits(0xff));
    assert_eq!(
        encode_key_input(&every_modifier, 8, false, ExtendedKeysMode::OnRequest),
        b"\x1b[97;256u".to_vec()
    );
}

#[test]
fn flag_two_names_the_event_kind_and_holds_releases_back_without_it() {
    let mut repeated_a = build_key_press(Key::Char('a'), KeyModifierFlags::NONE);
    repeated_a.key_event_kind = koshi_core::key::KeyEventKind::Repeat;
    assert_eq!(
        encode_key_input(&repeated_a, 10, false, ExtendedKeysMode::OnRequest),
        b"\x1b[97;1:2u".to_vec()
    );

    let mut released_a = build_key_press(Key::Char('a'), KeyModifierFlags::NONE);
    released_a.key_event_kind = koshi_core::key::KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_a, 10, false, ExtendedKeysMode::OnRequest),
        b"\x1b[97;1:3u".to_vec()
    );
    assert_eq!(
        encode_key_input(&released_a, 8, false, ExtendedKeysMode::OnRequest),
        Vec::<u8>::new()
    );

    let mut released_enter = build_key_press(Key::Named(NamedKey::Enter), KeyModifierFlags::NONE);
    released_enter.key_event_kind = koshi_core::key::KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_enter, 2, false, ExtendedKeysMode::OnRequest),
        Vec::<u8>::new()
    );
    assert_eq!(
        encode_key_input(&released_enter, 10, false, ExtendedKeysMode::OnRequest),
        b"\x1b[13;1:3u".to_vec()
    );

    let mut released_up = build_key_press(Key::Named(NamedKey::Up), KeyModifierFlags::NONE);
    released_up.key_event_kind = koshi_core::key::KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_up, 2, false, ExtendedKeysMode::OnRequest),
        b"\x1b[1;1:3A".to_vec()
    );
}

#[test]
fn flag_four_names_the_alternate_keys_and_flag_sixteen_names_the_text() {
    let mut shifted_a = build_key_press(Key::Char('a'), KeyModifierFlags::SHIFT);
    shifted_a.shifted_key = Some('A');
    assert_eq!(
        encode_key_input(&shifted_a, 12, false, ExtendedKeysMode::OnRequest),
        b"\x1b[97:65;2u".to_vec()
    );

    shifted_a.associated_text = "A".to_string();
    assert_eq!(
        encode_key_input(&shifted_a, 24, false, ExtendedKeysMode::OnRequest),
        b"\x1b[97;2;65u".to_vec()
    );
    assert_eq!(
        encode_key_input(&shifted_a, 16, false, ExtendedKeysMode::OnRequest),
        b"A".to_vec()
    );

    let mut composed_text = build_key_press(Key::Char('e'), KeyModifierFlags::NONE);
    composed_text.associated_text = "e\u{301}".to_string();
    assert_eq!(
        encode_key_input(&composed_text, 24, false, ExtendedKeysMode::OnRequest),
        b"\x1b[101;;101:769u".to_vec()
    );
}

#[test]
fn a_text_only_event_carries_its_codepoints_and_falls_back_to_the_text() {
    let text_only = KeyInput {
        key: KeyIdentity::Codepoint(TEXT_ONLY_KEY_CODEPOINT),
        key_event_kind: koshi_core::key::KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: "å".to_string(),
        modifier_flags: KeyModifierFlags::NONE,
    };
    assert_eq!(
        encode_key_input(&text_only, 24, false, ExtendedKeysMode::OnRequest),
        b"\x1b[0;;229u".to_vec()
    );
    assert_eq!(
        encode_key_input(&text_only, 0, false, ExtendedKeysMode::OnRequest),
        "å".as_bytes().to_vec()
    );
}

#[test]
fn a_modifier_key_reaches_a_pane_only_with_flag_eight() {
    let left_shift = KeyInput {
        key: KeyIdentity::Codepoint(57441),
        key_event_kind: koshi_core::key::KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags: KeyModifierFlags::SHIFT,
    };
    assert_eq!(
        encode_key_input(&left_shift, 8, false, ExtendedKeysMode::OnRequest),
        b"\x1b[57441;2u".to_vec()
    );
    assert_eq!(
        encode_key_input(&left_shift, 7, false, ExtendedKeysMode::OnRequest),
        Vec::<u8>::new()
    );
}

#[test]
fn always_escape_codes_only_the_keys_legacy_encoding_loses() {
    let always = ExtendedKeysMode::Always;
    let lost_key_cases: [(Key, KeyModifierFlags, &[u8]); 6] = [
        (
            Key::Named(NamedKey::Enter),
            KeyModifierFlags::SHIFT,
            b"\x1b[13;2u",
        ),
        (
            Key::Named(NamedKey::Enter),
            KeyModifierFlags::CTRL,
            b"\x1b[13;5u",
        ),
        (Key::Char('i'), KeyModifierFlags::CTRL, b"\x1b[105;5u"),
        (Key::Char('m'), KeyModifierFlags::CTRL, b"\x1b[109;5u"),
        (
            Key::Named(NamedKey::Esc),
            KeyModifierFlags::SHIFT,
            b"\x1b[27;2u",
        ),
        (
            Key::Named(NamedKey::Backspace),
            KeyModifierFlags::CTRL,
            b"\x1b[127;5u",
        ),
    ];
    for (key, modifier_flags, expected_bytes) in lost_key_cases {
        let key_press = build_key_press(key, modifier_flags);
        assert_eq!(
            encode_key_input(&key_press, 0, false, always),
            expected_bytes.to_vec(),
            "{key:?} with {modifier_flags:?}"
        );
    }

    let kept_key_cases: [(Key, KeyModifierFlags, &[u8]); 5] = [
        (Key::Named(NamedKey::Tab), KeyModifierFlags::NONE, b"\t"),
        (Key::Named(NamedKey::Enter), KeyModifierFlags::NONE, b"\r"),
        (
            Key::Named(NamedKey::Tab),
            KeyModifierFlags::SHIFT,
            b"\x1b[Z",
        ),
        (Key::Char('h'), KeyModifierFlags::CTRL, b"\x08"),
        (
            Key::Named(NamedKey::Right),
            KeyModifierFlags::CTRL,
            b"\x1b[1;5C",
        ),
    ];
    for (key, modifier_flags, expected_bytes) in kept_key_cases {
        let key_press = build_key_press(key, modifier_flags);
        assert_eq!(
            encode_key_input(&key_press, 0, false, always),
            expected_bytes.to_vec(),
            "{key:?} with {modifier_flags:?}"
        );
    }
}

#[test]
fn always_writes_presses_and_repeats_and_never_a_release() {
    let mut shift_enter = build_key_press(Key::Named(NamedKey::Enter), KeyModifierFlags::SHIFT);
    shift_enter.key_event_kind = koshi_core::key::KeyEventKind::Repeat;
    assert_eq!(
        encode_key_input(&shift_enter, 0, false, ExtendedKeysMode::Always),
        b"\x1b[13;2u".to_vec()
    );

    shift_enter.key_event_kind = koshi_core::key::KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&shift_enter, 0, false, ExtendedKeysMode::Always),
        Vec::<u8>::new()
    );
}

#[test]
fn reported_text_reaches_a_legacy_pane_as_that_text() {
    let mut composed_a = build_key_press(Key::Char('a'), KeyModifierFlags::ALT);
    composed_a.associated_text = "å".to_string();
    assert_eq!(
        encode_key_input(&composed_a, 0, false, ExtendedKeysMode::OnRequest),
        "å".as_bytes().to_vec()
    );

    let control_a = build_key_press(Key::Char('a'), KeyModifierFlags::CTRL);
    assert_eq!(
        encode_key_input(&control_a, 0, false, ExtendedKeysMode::OnRequest),
        vec![0x01]
    );
}

#[test]
fn a_cursor_key_follows_the_panes_cursor_key_mode() {
    let up = build_key_press(Key::Named(NamedKey::Up), KeyModifierFlags::NONE);
    assert_eq!(
        encode_key_input(&up, 8, true, ExtendedKeysMode::OnRequest),
        b"\x1bOA".to_vec()
    );
    assert_eq!(
        encode_key_input(&up, 8, false, ExtendedKeysMode::OnRequest),
        b"\x1b[A".to_vec()
    );
}

#[test]
fn every_flag_combination_leaves_plain_typing_as_the_character() {
    for keyboard_flags in 0..32u8 {
        let mut typed_a = build_key_press(Key::Char('a'), KeyModifierFlags::NONE);
        typed_a.associated_text = "a".to_string();
        let encoded_bytes =
            encode_key_input(&typed_a, keyboard_flags, false, ExtendedKeysMode::OnRequest);
        let is_escape_coded = keyboard_flags & 8 != 0;
        if is_escape_coded {
            let expected_bytes = if keyboard_flags & 16 != 0 {
                b"\x1b[97;;97u".to_vec()
            } else {
                b"\x1b[97u".to_vec()
            };
            assert_eq!(encoded_bytes, expected_bytes, "flags {keyboard_flags}");
        } else {
            assert_eq!(encoded_bytes, b"a".to_vec(), "flags {keyboard_flags}");
        }
    }
}

#[test]
fn a_text_key_reports_no_event_kind_without_the_all_keys_flag() {
    // Flag 2 alone names kinds only for keys that take an escape code. A text
    // key takes one with flag 8, so under flag 2 alone a repeat writes the
    // character again and a release writes nothing.
    let mut repeated_a = build_key_press(Key::Char('a'), KeyModifierFlags::NONE);
    repeated_a.key_event_kind = KeyEventKind::Repeat;
    assert_eq!(
        encode_key_input(&repeated_a, 2, false, ExtendedKeysMode::OnRequest),
        b"a".to_vec()
    );

    let mut released_a = build_key_press(Key::Char('a'), KeyModifierFlags::NONE);
    released_a.key_event_kind = KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_a, 2, false, ExtendedKeysMode::OnRequest),
        Vec::<u8>::new()
    );
}

#[test]
fn a_silent_key_reports_no_event_kind_without_an_escape_coded_form() {
    // Esc takes an escape code with flag 1. Under flag 2 alone it has none, so
    // a repeat writes the legacy byte and a release writes nothing.
    let mut repeated_escape = build_key_press(Key::Named(NamedKey::Esc), KeyModifierFlags::NONE);
    repeated_escape.key_event_kind = KeyEventKind::Repeat;
    assert_eq!(
        encode_key_input(&repeated_escape, 2, false, ExtendedKeysMode::OnRequest),
        vec![0x1b]
    );

    let mut released_escape = build_key_press(Key::Named(NamedKey::Esc), KeyModifierFlags::NONE);
    released_escape.key_event_kind = KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_escape, 2, false, ExtendedKeysMode::OnRequest),
        Vec::<u8>::new()
    );
    assert_eq!(
        encode_key_input(&released_escape, 3, false, ExtendedKeysMode::OnRequest),
        b"\x1b[27;1:3u".to_vec()
    );
}

#[test]
fn a_text_only_event_writes_nothing_on_release() {
    let mut released_text = KeyInput {
        key: KeyIdentity::Codepoint(TEXT_ONLY_KEY_CODEPOINT),
        key_event_kind: KeyEventKind::Release,
        shifted_key: None,
        base_layout_key: None,
        associated_text: "å".to_string(),
        modifier_flags: KeyModifierFlags::NONE,
    };
    assert_eq!(
        encode_key_input(&released_text, 2, false, ExtendedKeysMode::OnRequest),
        Vec::<u8>::new()
    );

    released_text.key_event_kind = KeyEventKind::Press;
    assert_eq!(
        encode_key_input(&released_text, 2, false, ExtendedKeysMode::OnRequest),
        "å".as_bytes().to_vec()
    );
}

#[test]
fn a_functional_key_reports_its_kind_on_flag_two_alone() {
    let mut released_delete = build_key_press(Key::Named(NamedKey::Delete), KeyModifierFlags::NONE);
    released_delete.key_event_kind = KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_delete, 2, false, ExtendedKeysMode::OnRequest),
        b"\x1b[3;1:3~".to_vec()
    );

    let mut released_f13 = build_key_press(Key::Named(NamedKey::F(13)), KeyModifierFlags::NONE);
    released_f13.key_event_kind = KeyEventKind::Release;
    assert_eq!(
        encode_key_input(&released_f13, 2, false, ExtendedKeysMode::OnRequest),
        b"\x1b[1;2:3P".to_vec()
    );
}

#[test]
fn always_adds_to_the_flags_rather_than_replacing_them() {
    // Flag 1 keeps Enter legacy. `Always` names Shift+Enter as a key legacy
    // encoding loses, so the two together give the report.
    let shift_enter = build_key_press(Key::Named(NamedKey::Enter), KeyModifierFlags::SHIFT);
    assert_eq!(
        encode_key_input(&shift_enter, 1, false, ExtendedKeysMode::Always),
        b"\x1b[13;2u".to_vec()
    );

    // A key flag 1 already escape-codes is unchanged by `Always`.
    let shift_escape = build_key_press(Key::Named(NamedKey::Esc), KeyModifierFlags::SHIFT);
    assert_eq!(
        encode_key_input(&shift_escape, 1, false, ExtendedKeysMode::Always),
        encode_key_input(&shift_escape, 1, false, ExtendedKeysMode::OnRequest)
    );
}

#[test]
fn a_key_outside_the_basic_plane_reports_its_whole_codepoint() {
    let mut emoji_key = build_key_press(Key::Char('😀'), KeyModifierFlags::NONE);
    emoji_key.associated_text = "😀".to_string();
    assert_eq!(
        encode_key_input(&emoji_key, 24, false, ExtendedKeysMode::OnRequest),
        b"\x1b[128512;;128512u".to_vec()
    );
    assert_eq!(
        encode_key_input(&emoji_key, 0, false, ExtendedKeysMode::OnRequest),
        "😀".as_bytes().to_vec()
    );
}

#[test]
fn always_converts_exactly_the_chords_legacy_encoding_cannot_tell_apart() {
    // The list is built from the encoder, not by hand: every printable
    // character and every C0 key, under each modifier set, encoded twice. A
    // chord belongs here when its legacy bytes are bytes another key also
    // sends. A new legacy encoding that adds a collision fails this test.
    let modifier_sets: [(&str, KeyModifierFlags); 4] = [
        ("", KeyModifierFlags::NONE),
        ("Shift+", KeyModifierFlags::SHIFT),
        ("Ctrl+", KeyModifierFlags::CTRL),
        (
            "Ctrl+Shift+",
            KeyModifierFlags::CTRL.union(KeyModifierFlags::SHIFT),
        ),
    ];
    let named_keys: [(&str, NamedKey); 5] = [
        ("Enter", NamedKey::Enter),
        ("Tab", NamedKey::Tab),
        ("Backspace", NamedKey::Backspace),
        ("Escape", NamedKey::Esc),
        ("Space", NamedKey::Space),
    ];

    let mut converted_chords: Vec<String> = Vec::new();
    for (modifier_label, modifier_flags) in modifier_sets {
        for character in ' '..='~' {
            let key_press = build_key_press(Key::Char(character), modifier_flags);
            if is_csi_u_report(&encode_key_input(
                &key_press,
                0,
                false,
                ExtendedKeysMode::Always,
            )) {
                converted_chords.push(format!("{modifier_label}{character}"));
            }
        }
        for (key_label, named_key) in named_keys {
            let key_press = build_key_press(Key::Named(named_key), modifier_flags);
            if is_csi_u_report(&encode_key_input(
                &key_press,
                0,
                false,
                ExtendedKeysMode::Always,
            )) {
                converted_chords.push(format!("{modifier_label}{key_label}"));
            }
        }
    }

    assert_eq!(
        converted_chords,
        vec![
            "Shift+Enter",
            "Shift+Backspace",
            "Shift+Escape",
            "Ctrl+2",
            "Ctrl+3",
            "Ctrl+8",
            "Ctrl+?",
            "Ctrl+@",
            "Ctrl+I",
            "Ctrl+M",
            "Ctrl+[",
            "Ctrl+i",
            "Ctrl+m",
            "Ctrl+Enter",
            "Ctrl+Tab",
            "Ctrl+Backspace",
            "Ctrl+Escape",
            "Ctrl+Shift+2",
            "Ctrl+Shift+3",
            "Ctrl+Shift+8",
            "Ctrl+Shift+?",
            "Ctrl+Shift+@",
            "Ctrl+Shift+I",
            "Ctrl+Shift+M",
            "Ctrl+Shift+[",
            "Ctrl+Shift+i",
            "Ctrl+Shift+m",
            "Ctrl+Shift+Enter",
            "Ctrl+Shift+Backspace",
            "Ctrl+Shift+Escape",
        ]
    );
}

/// Whether `encoded_bytes` is a `CSI u` report rather than legacy bytes.
fn is_csi_u_report(encoded_bytes: &[u8]) -> bool {
    encoded_bytes.starts_with(b"\x1b[") && encoded_bytes.ends_with(b"u")
}
