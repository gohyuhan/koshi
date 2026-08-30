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
fn a_capital_that_cannot_be_rebuilt_is_never_folded() {
    // `ẞ` (capital sharp S) lowercases to the single char `ß`, and `ß`
    // uppercases to "SS": `Shift + ß` does not rebuild `ẞ`. The chord keeps
    // `ẞ` with no Shift, and the pane receives `ẞ`.
    assert_eq!(
        decode_key(press(KeyCode::Char('ẞ'), KeyModifiers::NONE)),
        chord(ModFlags::NONE, Key::Char('ẞ'))
    );
    assert_eq!(bytes(ModFlags::NONE, Key::Char('ẞ')), "ẞ".as_bytes());

    // The same for `İ`, whose lowercase is two chars.
    assert_eq!(
        decode_key(press(KeyCode::Char('İ'), KeyModifiers::NONE)),
        chord(ModFlags::NONE, Key::Char('İ'))
    );
    assert_eq!(bytes(ModFlags::NONE, Key::Char('İ')), "İ".as_bytes());
}

#[test]
fn every_folded_capital_reaches_the_pane_as_the_character_typed() {
    // Every capital the decoder folds to `lowercase + Shift`, the encoder
    // rebuilds byte-for-byte; a capital the decoder keeps goes out as itself.
    for typed in ['A', 'Z', 'É', 'Ø', 'ẞ', 'İ', 'Å'] {
        let chord = decode_key(press(KeyCode::Char(typed), KeyModifiers::NONE)).expect("decodes");
        assert_eq!(
            encode(chord, false),
            typed.to_string().as_bytes(),
            "{typed} (U+{:04X}) must reach the pane unchanged",
            typed as u32
        );
    }
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
    // Control plus Shift plus a letter sends the same C0 byte as Control plus
    // the letter.
    assert_eq!(
        bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a')),
        vec![0x01]
    );
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('[')), vec![0x1b]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('?')), vec![0x7f]);
}

#[test]
fn control_plus_a_character_with_no_c0_byte_sends_the_character() {
    // No control code stands for `<C-1>`: the digit goes out by itself.
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('1')), vec![b'1']);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('9')), vec![b'9']);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char(';')), vec![b';']);
}

#[test]
fn the_control_digits_carry_the_codes_the_letter_run_cannot_reach() {
    // Control clears the top bits, which covers `@`..`_` — and terminals hand
    // the leftover control codes to the digit row. `2` is NUL, `3` is ESC,
    // `4`..`7` are 0x1c..0x1f, and `8` is DEL.
    let cases = [
        ('2', 0x00),
        ('3', 0x1b),
        ('4', 0x1c),
        ('5', 0x1d),
        ('6', 0x1e),
        ('7', 0x1f),
        ('8', 0x7f),
    ];
    for (digit, byte) in cases {
        assert_eq!(
            bytes(ModFlags::CTRL, Key::Char(digit)),
            vec![byte],
            "<C-{digit}>"
        );
    }

    // Alt composes with them exactly as it does with a letter.
    assert_eq!(
        bytes(ModFlags::ALT | ModFlags::CTRL, Key::Char('4')),
        vec![ESC, 0x1c]
    );
}

#[test]
fn the_two_spellings_of_one_control_code_send_the_same_byte() {
    // One key press has two host spellings: on unix crossterm decodes the
    // terminal's `0x1c` to `Char('4')`, and `Ctrl-\` arrives as `<C-4>`; on
    // Windows the key's own character arrives, and it is `<C-\>`. Both leave
    // here as `0x1c`.
    for (digit, punctuation) in [('4', '\\'), ('5', ']'), ('6', '^'), ('7', '_')] {
        assert_eq!(
            bytes(ModFlags::CTRL, Key::Char(digit)),
            bytes(ModFlags::CTRL, Key::Char(punctuation)),
            "<C-{digit}> and <C-{punctuation}> are one key press"
        );
    }
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
    // A C0 byte has no field for Super: the key arrives bare.
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
fn alt_shift_tab_keeps_the_alt_prefix() {
    assert_eq!(
        bytes(ModFlags::ALT | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESC, ESC, b'[', b'Z']
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
    // sends the same bytes for a modified key.
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
    // terminfo kf13 (Shift+F1) is `ESC [ 1 ; 2 P`, and kf25 (Ctrl+F1) is
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
    // terminfo lists F13–F24 as Shift plus F1–F12; a program reads those
    // bytes back.
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

// ------------------------------------------------ decode: modifier matrix ----

#[test]
fn shift_plus_lowercase_letter_carries_shift() {
    // A host may report Shift+a as the lowercase char with Shift held (as the
    // Windows console does). The chord carries the Shift.
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::SHIFT)),
        chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn a_bare_capital_folds_to_shift_plus_lowercase() {
    // The other host form of the same press: the capital with no Shift held.
    assert_eq!(
        decode_key(press(KeyCode::Char('A'), KeyModifiers::NONE)),
        chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn a_capital_with_shift_also_held_stays_one_shift() {
    // A host that reports both the capital and the Shift modifier yields one
    // Shift: the bitmap is idempotent.
    assert_eq!(
        decode_key(press(KeyCode::Char('A'), KeyModifiers::SHIFT)),
        chord(ModFlags::SHIFT, Key::Char('a'))
    );
}

#[test]
fn control_and_alt_together_decode_on_a_letter() {
    assert_eq!(
        decode_key(press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )),
        chord(ModFlags::CTRL | ModFlags::ALT, Key::Char('c'))
    );
}

#[test]
fn every_modifier_at_once_decodes_on_a_lowercase_letter() {
    // Ctrl+Alt+Super reported by the host, plus the Shift the lowercase form
    // needs: all four land, and the letter stays folded lowercase.
    let held =
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::SHIFT;
    assert_eq!(
        decode_key(press(KeyCode::Char('h'), held)),
        chord(
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
        decode_key(press(KeyCode::Char('4'), KeyModifiers::CONTROL)),
        chord(ModFlags::CTRL, Key::Char('4'))
    );
}

#[test]
fn a_held_shift_is_dropped_from_a_non_letter_character() {
    // Shift is a letter's case only. A host that reports `!` while Shift is
    // still held yields `<C-!>`, not `<C-S-!>`.
    assert_eq!(
        decode_key(press(
            KeyCode::Char('!'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        chord(ModFlags::CTRL, Key::Char('!'))
    );
}

#[test]
fn the_spacebar_carries_its_modifiers_as_a_named_key() {
    // The space character becomes the named key, and a held Shift joins it like
    // any other named-key modifier.
    assert_eq!(
        decode_key(press(KeyCode::Char(' '), KeyModifiers::SHIFT)),
        chord(ModFlags::SHIFT, Key::Named(NamedKey::Space))
    );
    assert_eq!(
        decode_key(press(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )),
        chord(ModFlags::CTRL | ModFlags::ALT, Key::Named(NamedKey::Space))
    );
}

#[test]
fn backtab_folds_shift_in_alongside_other_modifiers() {
    // BackTab is Shift+Tab; a Control held with it lands beside that Shift.
    assert_eq!(
        decode_key(press(KeyCode::BackTab, KeyModifiers::CONTROL)),
        chord(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Tab))
    );
}

#[test]
fn backtab_with_shift_already_held_stays_one_shift() {
    // A host that sets the Shift modifier on BackTab too yields one Shift.
    assert_eq!(
        decode_key(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
        chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab))
    );
}

#[test]
fn a_function_key_carries_its_modifier_on_decode() {
    assert_eq!(
        decode_key(press(KeyCode::F(6), KeyModifiers::CONTROL)),
        chord(ModFlags::CTRL, Key::Named(NamedKey::F(6)))
    );
}

#[test]
fn a_modified_repeat_decodes_and_a_modified_release_does_not() {
    // The release/repeat rule is independent of which modifiers are held.
    let mut repeat = press(KeyCode::Char('a'), KeyModifiers::CONTROL);
    repeat.kind = KeyEventKind::Repeat;
    assert_eq!(decode_key(repeat), chord(ModFlags::CTRL, Key::Char('a')));

    let mut release = press(KeyCode::Char('a'), KeyModifiers::CONTROL);
    release.kind = KeyEventKind::Release;
    assert_eq!(decode_key(release), None);
}

#[test]
fn hyper_is_dropped_on_decode() {
    // The chord model has no Hyper: a held Hyper leaves nothing in the chord,
    // and the other modifiers land as usual.
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::HYPER)),
        chord(ModFlags::NONE, Key::Char('a'))
    );
    assert_eq!(
        decode_key(press(
            KeyCode::Up,
            KeyModifiers::HYPER | KeyModifiers::CONTROL
        )),
        chord(ModFlags::CTRL, Key::Named(NamedKey::Up))
    );
}

#[test]
fn a_release_of_a_named_key_is_not_input() {
    let mut release = press(KeyCode::Up, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(decode_key(release), None);
}

#[test]
fn a_capital_with_control_folds_and_keeps_control() {
    // The capital folds to lowercase plus Shift, and Control lands beside it.
    assert_eq!(
        decode_key(press(KeyCode::Char('A'), KeyModifiers::CONTROL)),
        chord(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a'))
    );
}

// --------------------------------------------- decode: hostile characters ----

#[test]
fn a_control_byte_arriving_as_a_character_decodes_without_panic() {
    // A raw C0 byte handed through as `Char` decodes as the plain character,
    // not as the named key that sends that byte. NUL, DEL, and ESC as
    // characters:
    for c in ['\u{0}', '\u{7f}', '\u{1b}'] {
        assert_eq!(
            decode_key(press(KeyCode::Char(c), KeyModifiers::NONE)),
            chord(ModFlags::NONE, Key::Char(c)),
            "U+{:04X}",
            c as u32
        );
    }
}

#[test]
fn a_c1_range_character_decodes_to_itself() {
    // A byte in the C1 range (0x80) as a character folds nowhere and carries no
    // implicit modifier.
    assert_eq!(
        decode_key(press(KeyCode::Char('\u{80}'), KeyModifiers::NONE)),
        chord(ModFlags::NONE, Key::Char('\u{80}'))
    );
}

#[test]
fn the_top_of_the_character_range_decodes_without_panic() {
    // `char::MAX` (U+10FFFF) is not uppercase: it folds nowhere and decodes as
    // itself.
    assert_eq!(
        decode_key(press(KeyCode::Char(char::MAX), KeyModifiers::NONE)),
        chord(ModFlags::NONE, Key::Char(char::MAX))
    );
}

// --------------------------------------------- encode: hostile characters ----

#[test]
fn a_control_character_with_no_c0_mapping_encodes_as_its_own_bytes() {
    // NUL, DEL, and ESC as characters have no entry in the control table: with
    // Control held they send their own byte.
    assert_eq!(bytes(ModFlags::NONE, Key::Char('\u{0}')), vec![0x00]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('\u{0}')), vec![0x00]);
    assert_eq!(bytes(ModFlags::NONE, Key::Char('\u{7f}')), vec![0x7f]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('\u{7f}')), vec![0x7f]);
    assert_eq!(bytes(ModFlags::NONE, Key::Char('\u{1b}')), vec![0x1b]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('\u{1b}')), vec![0x1b]);
}

#[test]
fn a_c1_and_max_character_encode_to_their_utf8_bytes() {
    assert_eq!(bytes(ModFlags::NONE, Key::Char('\u{80}')), vec![0xc2, 0x80]);
    assert_eq!(
        bytes(ModFlags::NONE, Key::Char(char::MAX)),
        vec![0xf4, 0x8f, 0xbf, 0xbf]
    );
}

#[test]
fn a_tab_character_and_the_tab_key_send_the_same_byte() {
    // `\t` arriving as a plain character (not the Tab key) still encodes to the
    // tab byte, matching the named key.
    assert_eq!(bytes(ModFlags::NONE, Key::Char('\t')), vec![0x09]);
}

#[test]
fn shift_is_dropped_when_the_capital_cannot_be_rebuilt() {
    // `ß` uppercases to the two-char "SS": Shift restores no single capital.
    // The encoder sends `ß` and nothing for the Shift.
    assert_eq!(bytes(ModFlags::SHIFT, Key::Char('ß')), vec![0xc3, 0x9f]);
}

// ---------------------------------------- encode: modifier combinations ------

#[test]
fn control_is_ignored_on_the_c0_keys_that_have_no_control_form() {
    // Enter, Tab, and Esc carry only Alt in the byte stream; with Control held
    // they send the bare byte.
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::Enter)),
        vec![b'\r']
    );
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::Tab)),
        vec![b'\t']
    );
    assert_eq!(bytes(ModFlags::CTRL, Key::Named(NamedKey::Esc)), vec![ESC]);
}

#[test]
fn alt_prefixes_escape_on_the_c0_named_keys() {
    assert_eq!(
        bytes(ModFlags::ALT, Key::Named(NamedKey::Tab)),
        vec![ESC, b'\t']
    );
    assert_eq!(
        bytes(ModFlags::ALT, Key::Named(NamedKey::Space)),
        vec![ESC, b' ']
    );
}

#[test]
fn control_plus_alt_reshapes_the_byte_then_prefixes_escape() {
    // Control picks the special byte (NUL for Space, BS for Backspace) and Alt
    // wraps the ESC in front of it.
    assert_eq!(
        bytes(ModFlags::CTRL | ModFlags::ALT, Key::Named(NamedKey::Space)),
        vec![ESC, 0x00]
    );
    assert_eq!(
        bytes(
            ModFlags::CTRL | ModFlags::ALT,
            Key::Named(NamedKey::Backspace)
        ),
        vec![ESC, 0x08]
    );
}

#[test]
fn super_and_alt_together_on_a_character_keep_only_the_escape_prefix() {
    // A C0 character has no field for Super: Alt's ESC is all that goes out.
    assert_eq!(
        bytes(ModFlags::ALT | ModFlags::SUPER, Key::Char('a')),
        vec![ESC, b'a']
    );
}

#[test]
fn control_plus_super_on_a_cursor_key_sums_in_the_parameter() {
    // Super is bit 8, Control bit 4: the parameter is 1 + 4 + 8 = 13.
    assert_eq!(
        bytes(ModFlags::CTRL | ModFlags::SUPER, Key::Named(NamedKey::Up)),
        b"\x1b[1;13A".to_vec()
    );
}

#[test]
fn every_modifier_on_an_editing_key_fills_the_parameter() {
    // Shift 1 + Alt 2 + Control 4 + Super 8, offset by one, is 16.
    assert_eq!(
        bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Named(NamedKey::Delete)
        ),
        b"\x1b[3;16~".to_vec()
    );
}

#[test]
fn every_modifier_on_a_low_function_key_fills_the_parameter() {
    assert_eq!(
        bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Named(NamedKey::F(1))
        ),
        b"\x1b[1;16P".to_vec()
    );
}

#[test]
fn a_modified_high_function_key_adds_its_modifier_to_the_shift() {
    // F13 already carries the Shift that stands for it; a Control held with it
    // joins that Shift — param 1 + 1 (Shift) + 4 (Control) = 6.
    assert_eq!(
        bytes(ModFlags::CTRL, Key::Named(NamedKey::F(13))),
        b"\x1b[1;6P".to_vec()
    );
}

#[test]
fn shift_on_a_high_function_key_does_not_double_the_shift() {
    // F13 is Shift+F1; a Shift held on top of it is still one Shift.
    assert_eq!(
        bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(13))),
        b"\x1b[1;2P".to_vec()
    );
    assert_eq!(
        bytes(ModFlags::SHIFT, Key::Named(NamedKey::F(13))),
        bytes(ModFlags::NONE, Key::Named(NamedKey::F(13)))
    );
}

#[test]
fn every_modifier_on_a_high_function_key_fills_the_parameter() {
    // F24 is Shift+F12; Control, Alt and Super join that Shift:
    // 1 + 1 + 2 + 4 + 8 = 16.
    assert_eq!(
        bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SUPER,
            Key::Named(NamedKey::F(24))
        ),
        b"\x1b[24;16~".to_vec()
    );
}

#[test]
fn control_and_super_have_no_place_in_shift_tab() {
    // `ESC [ Z` carries no modifier parameter: Shift+Tab sends it with Control
    // or Super held too, and Alt keeps its `ESC` prefix.
    assert_eq!(
        bytes(ModFlags::CTRL | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESC, b'[', b'Z']
    );
    assert_eq!(
        bytes(ModFlags::SUPER | ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        vec![ESC, b'[', b'Z']
    );
    assert_eq!(
        bytes(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT,
            Key::Named(NamedKey::Tab)
        ),
        vec![ESC, ESC, b'[', b'Z']
    );
}

#[test]
fn application_mode_changes_only_the_cursor_keys() {
    // DECCKM moves the cursor keys and Home/End between `ESC [` and `ESC O`.
    // Every other chord sends the same bytes in both modes.
    let cases: [(ModFlags, Key, &[u8]); 9] = [
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
    for (mods, key, expected) in cases {
        assert_eq!(app_bytes(mods, key), expected.to_vec(), "{mods}{key}");
        assert_eq!(bytes(mods, key), expected.to_vec(), "{mods}{key}");
    }
}

// ------------------------------------------ round trip: hostile decode → encode ----

#[test]
fn hostile_and_edge_characters_round_trip_to_their_own_bytes() {
    // Whatever the decoder keeps of an odd character, the encoder sends the
    // character's own bytes back.
    for typed in ['\u{0}', '\u{7f}', '\u{1b}', '\u{80}', '\t', char::MAX] {
        let chord = decode_key(press(KeyCode::Char(typed), KeyModifiers::NONE)).expect("decodes");
        assert_eq!(
            encode(chord, false),
            typed.to_string().as_bytes(),
            "U+{:04X} must round-trip",
            typed as u32
        );
    }
}

// ------------------------------------------------------- table boundaries ----

#[test]
fn function_key_zero_is_not_a_key_the_model_names() {
    // `F(24)` is the top of the run and `F(25)` is rejected; `F(0)` is the
    // bottom of the same bound and is rejected too.
    assert_eq!(decode_key(press(KeyCode::F(0), KeyModifiers::NONE)), None);
}

#[test]
fn the_control_fold_covers_its_run_and_stops_at_both_ends() {
    // Control clears the top bits over `@`..`_`: `@` opens the run at NUL and
    // `_` closes it at 0x1f. A letter is its capital's version of that fold, so
    // `z` ends the letter run at 0x1a.
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('@')), vec![0x00]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('_')), vec![0x1f]);
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('z')), vec![0x1a]);
    // The backtick sits between the two runs and belongs to neither: it sends
    // its own byte, not the NUL that `@` sends.
    assert_eq!(bytes(ModFlags::CTRL, Key::Char('`')), vec![b'`']);
}

#[test]
fn push_decimal_writes_every_digit_of_the_value() {
    let cases: [(u8, &[u8]); 6] = [
        (0, b"0"),
        (9, b"9"),
        (10, b"10"),
        (99, b"99"),
        (100, b"100"),
        (255, b"255"),
    ];
    for (value, expected) in cases {
        let mut digits = Vec::new();
        push_decimal(&mut digits, value);
        assert_eq!(digits, expected.to_vec(), "{value}");
    }
}
