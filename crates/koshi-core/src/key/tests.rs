//! Tests for the key chord model: modifier bit operations, the canonical text
//! form each type renders, and the typeable predicate that guards transparent
//! modes from swallowing input.

use super::*;

#[test]
fn none_is_empty_and_every_flag_is_a_distinct_bit() {
    assert!(ModFlags::NONE.is_empty());
    assert_eq!(ModFlags::NONE.bits(), 0);
    assert_eq!(ModFlags::CTRL.bits(), 1);
    assert_eq!(ModFlags::ALT.bits(), 2);
    assert_eq!(ModFlags::SHIFT.bits(), 4);
    assert_eq!(ModFlags::SUPER.bits(), 8);
    assert!(!ModFlags::CTRL.is_empty());
}

#[test]
fn union_sets_both_bits() {
    let both = ModFlags::CTRL.union(ModFlags::SHIFT);
    assert_eq!(both.bits(), 5);
    assert_eq!(both, ModFlags::CTRL | ModFlags::SHIFT);
}

#[test]
fn contains_is_subset_and_intersects_is_overlap() {
    let ctrl_shift = ModFlags::CTRL | ModFlags::SHIFT;

    assert!(ctrl_shift.contains(ModFlags::CTRL));
    assert!(ctrl_shift.contains(ModFlags::SHIFT));
    assert!(ctrl_shift.contains(ctrl_shift));
    assert!(ctrl_shift.contains(ModFlags::NONE));
    assert!(!ctrl_shift.contains(ModFlags::ALT));
    assert!(!ctrl_shift.contains(ModFlags::CTRL | ModFlags::ALT));

    assert!(ctrl_shift.intersects(ModFlags::CTRL));
    assert!(ctrl_shift.intersects(ModFlags::CTRL | ModFlags::ALT));
    assert!(!ctrl_shift.intersects(ModFlags::ALT));
    assert!(!ctrl_shift.intersects(ModFlags::NONE));
}

#[test]
fn mod_flags_display_uses_canonical_order() {
    assert_eq!(ModFlags::NONE.to_string(), "");
    assert_eq!(ModFlags::CTRL.to_string(), "C-");
    assert_eq!(ModFlags::ALT.to_string(), "A-");
    assert_eq!(ModFlags::SHIFT.to_string(), "S-");
    assert_eq!(ModFlags::SUPER.to_string(), "D-");
    assert_eq!((ModFlags::SHIFT | ModFlags::CTRL).to_string(), "C-S-");
    assert_eq!((ModFlags::SUPER | ModFlags::ALT).to_string(), "A-D-");
    assert_eq!(
        (ModFlags::SUPER | ModFlags::SHIFT | ModFlags::ALT | ModFlags::CTRL).to_string(),
        "C-A-S-D-"
    );
}

#[test]
fn named_key_display_spells_every_variant() {
    assert_eq!(NamedKey::Enter.to_string(), "CR");
    assert_eq!(NamedKey::Tab.to_string(), "Tab");
    assert_eq!(NamedKey::Backspace.to_string(), "BS");
    assert_eq!(NamedKey::Esc.to_string(), "Esc");
    assert_eq!(NamedKey::Space.to_string(), "Space");
    assert_eq!(NamedKey::Insert.to_string(), "Insert");
    assert_eq!(NamedKey::Delete.to_string(), "Del");
    assert_eq!(NamedKey::Home.to_string(), "Home");
    assert_eq!(NamedKey::End.to_string(), "End");
    assert_eq!(NamedKey::PageUp.to_string(), "PageUp");
    assert_eq!(NamedKey::PageDown.to_string(), "PageDown");
    assert_eq!(NamedKey::Left.to_string(), "Left");
    assert_eq!(NamedKey::Right.to_string(), "Right");
    assert_eq!(NamedKey::Up.to_string(), "Up");
    assert_eq!(NamedKey::Down.to_string(), "Down");
    assert_eq!(NamedKey::F(1).to_string(), "F1");
    assert_eq!(NamedKey::F(24).to_string(), "F24");
}

#[test]
fn key_display_forwards_to_the_character_or_the_name() {
    assert_eq!(Key::Char('p').to_string(), "p");
    assert_eq!(Key::Char('-').to_string(), "-");
    assert_eq!(Key::Named(NamedKey::PageUp).to_string(), "PageUp");
}

#[test]
fn unmodified_character_chords_render_bare() {
    assert_eq!(
        KeyChord::new(ModFlags::NONE, Key::Char('n')).to_string(),
        "n"
    );
    assert_eq!(
        KeyChord::new(ModFlags::NONE, Key::Char('-')).to_string(),
        "-"
    );
    assert_eq!(
        KeyChord::new(ModFlags::NONE, Key::Char('>')).to_string(),
        ">"
    );
}

#[test]
fn a_bare_open_bracket_is_still_bracketed_so_it_can_be_read_back() {
    assert_eq!(
        KeyChord::new(ModFlags::NONE, Key::Char('<')).to_string(),
        "<<>"
    );
}

#[test]
fn modified_and_named_chords_render_bracketed() {
    assert_eq!(
        KeyChord::new(ModFlags::CTRL, Key::Char('p')).to_string(),
        "<C-p>"
    );
    assert_eq!(
        KeyChord::new(ModFlags::ALT | ModFlags::SHIFT, Key::Char('n')).to_string(),
        "<A-S-n>"
    );
    assert_eq!(
        KeyChord::new(ModFlags::SUPER, Key::Char('x')).to_string(),
        "<D-x>"
    );
    assert_eq!(
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Space)).to_string(),
        "<Space>"
    );
    assert_eq!(
        KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab)).to_string(),
        "<S-Tab>"
    );
    assert_eq!(
        KeyChord::new(ModFlags::CTRL, Key::Char('-')).to_string(),
        "<C-->"
    );
    assert_eq!(
        KeyChord::new(ModFlags::CTRL, Key::Char('<')).to_string(),
        "<C-<>"
    );
}

#[test]
fn characters_are_typeable_whatever_their_case() {
    assert!(KeyChord::new(ModFlags::NONE, Key::Char('n')).is_typeable());
    assert!(KeyChord::new(ModFlags::SHIFT, Key::Char('a')).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Char('!')).is_typeable());
}

#[test]
fn the_keys_a_running_program_expects_are_typeable() {
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Space)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Tab)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Enter)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Backspace)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)).is_typeable());
    assert!(KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab)).is_typeable());
}

#[test]
fn control_alt_and_super_make_a_chord_untypeable() {
    assert!(!KeyChord::new(ModFlags::CTRL, Key::Char('p')).is_typeable());
    assert!(!KeyChord::new(ModFlags::ALT, Key::Char('n')).is_typeable());
    assert!(!KeyChord::new(ModFlags::SUPER, Key::Char('x')).is_typeable());
    assert!(!KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Space)).is_typeable());
    assert!(!KeyChord::new(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')).is_typeable());
}

#[test]
fn keys_no_program_needs_verbatim_are_not_typeable() {
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::F(5))).is_typeable());
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Left)).is_typeable());
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Delete)).is_typeable());
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::PageUp)).is_typeable());
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Insert)).is_typeable());
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Home)).is_typeable());
    assert!(!KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::End)).is_typeable());
}
