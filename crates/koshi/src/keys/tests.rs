//! `keys.rs` is a compatibility re-export (`pub use
//! koshi_input::keyboard::decode_key;`) with no logic of its own — the
//! decoding rules are `koshi-input`'s and are exhaustively tested
//! there. This is a wiring test only: it proves the re-exported path
//! (`crate::keys::decode_key`) produces exactly what calling
//! `koshi_input::keyboard::decode_key` directly produces, for one
//! representative event each on the `Some` and `None` paths. It is not a
//! re-test of `koshi-input`'s decoding behavior.

use super::*;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use koshi_core::key::{Key, KeyChord, ModFlags};

#[test]
fn the_reexport_decodes_a_press_identically_to_the_wrapped_function() {
    let event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);

    let via_reexport = decode_key(event);
    let via_wrapped = koshi_input::keyboard::decode_key(event);

    assert_eq!(via_wrapped, via_reexport, "the re-export must not diverge");
    assert_eq!(
        via_reexport,
        Some(KeyChord::new(ModFlags::CTRL, Key::Char('q')))
    );
}

#[test]
fn the_reexport_drops_a_release_identically_to_the_wrapped_function() {
    let event = KeyEvent {
        code: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    };

    let via_reexport = decode_key(event);
    let via_wrapped = koshi_input::keyboard::decode_key(event);

    assert_eq!(via_wrapped, via_reexport, "the re-export must not diverge");
    assert_eq!(via_reexport, None, "a key release decodes to nothing");
}
