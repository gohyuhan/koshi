//! Tests for the runtime-directory fixture and the key-event fixture.

use koshi_core::key::{Key, NamedKey};

use super::*;

#[test]
fn a_built_key_input_projects_back_to_the_chord_it_was_built_from() {
    for modifier_bits in 0..=0b1111 {
        let modifier_flags = ModFlags::try_from(modifier_bits).expect("four modifier bits");
        for key in [Key::Char('p'), Key::Named(NamedKey::Enter)] {
            let chord = KeyChord::from_parts(modifier_flags, key);

            assert_eq!(
                build_key_input_for_chord(chord).to_binding_chord(),
                Some(chord),
                "the event built for {chord:?} projects back to it"
            );
        }
    }
}

#[test]
fn a_built_key_input_reports_the_modifiers_the_chord_names() {
    let chord = KeyChord::from_parts(ModFlags::CTRL, Key::Char('p'));

    assert_eq!(
        build_key_input_for_chord(chord).modifier_flags,
        KeyModifierFlags::CTRL
    );
}

#[test]
fn runtime_directory_exists_until_its_handle_drops() {
    let runtime_directory_path = {
        let directory = build_test_runtime_directory();
        assert!(directory.path().is_dir());
        directory.path().to_owned()
    };

    assert!(!runtime_directory_path.exists());
}
