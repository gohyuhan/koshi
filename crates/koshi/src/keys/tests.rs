//! Tests for key decoding: characters, control chords, named keys, the quit
//! chord, and ignored releases.

use super::*;

/// A pressed key event with the given code and modifiers.
fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn plain_character_is_its_utf8_bytes() {
    assert_eq!(
        decode_key(press(KeyCode::Char('a'), KeyModifiers::NONE)),
        KeyAction::Bytes(vec![b'a'])
    );
}

#[test]
fn ctrl_c_is_the_interrupt_byte() {
    assert_eq!(
        decode_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        KeyAction::Bytes(vec![0x03])
    );
}

#[test]
fn ctrl_q_quits() {
    assert_eq!(
        decode_key(press(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        KeyAction::Quit
    );
    assert_eq!(
        decode_key(press(KeyCode::Char('Q'), KeyModifiers::CONTROL)),
        KeyAction::Quit
    );
}

#[test]
fn enter_backspace_tab_esc_map_to_control_bytes() {
    assert_eq!(
        decode_key(press(KeyCode::Enter, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![b'\r'])
    );
    assert_eq!(
        decode_key(press(KeyCode::Backspace, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![0x7f])
    );
    assert_eq!(
        decode_key(press(KeyCode::Tab, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![b'\t'])
    );
    assert_eq!(
        decode_key(press(KeyCode::Esc, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![0x1b])
    );
}

#[test]
fn arrows_emit_csi_sequences() {
    assert_eq!(
        decode_key(press(KeyCode::Up, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![0x1b, b'[', b'A'])
    );
    assert_eq!(
        decode_key(press(KeyCode::Left, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![0x1b, b'[', b'D'])
    );
}

#[test]
fn delete_emits_a_tilde_sequence() {
    assert_eq!(
        decode_key(press(KeyCode::Delete, KeyModifiers::NONE)),
        KeyAction::Bytes(vec![0x1b, b'[', b'3', b'~'])
    );
}

#[test]
fn alt_prefixes_the_character_with_escape() {
    assert_eq!(
        decode_key(press(KeyCode::Char('b'), KeyModifiers::ALT)),
        KeyAction::Bytes(vec![0x1b, b'b'])
    );
}

#[test]
fn key_release_is_ignored() {
    let release = KeyEvent::new_with_kind(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    );
    assert_eq!(decode_key(release), KeyAction::Ignore);
}
