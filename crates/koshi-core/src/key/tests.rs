//! Tests for the key chord model: modifier bit operations, the canonical text
//! form each type renders, the typeable predicate, uppercase folding, and the
//! serde wire form a chord travels in.

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
fn every_unmodified_key_a_pane_reads_is_typeable() {
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Space)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Tab)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Enter)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Backspace)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Left)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Home)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::End)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Delete)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Insert)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::PageUp)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::PageDown)).is_typeable());
    assert!(KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::F(5))).is_typeable());
}

#[test]
fn shift_keeps_a_chord_typeable() {
    assert!(KeyChord::new(ModFlags::SHIFT, Key::Char('a')).is_typeable());
    assert!(KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab)).is_typeable());
    assert!(KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Left)).is_typeable());
}

#[test]
fn control_alt_and_super_make_a_chord_untypeable() {
    assert!(!KeyChord::new(ModFlags::CTRL, Key::Char('p')).is_typeable());
    assert!(!KeyChord::new(ModFlags::ALT, Key::Char('n')).is_typeable());
    assert!(!KeyChord::new(ModFlags::SUPER, Key::Char('x')).is_typeable());
    assert!(!KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Space)).is_typeable());
    assert!(!KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Left)).is_typeable());
    assert!(!KeyChord::new(ModFlags::ALT, Key::Named(NamedKey::F(5))).is_typeable());
    assert!(!KeyChord::new(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')).is_typeable());
}

#[test]
fn key_sequence_exposes_chords_in_press_order() {
    let first = KeyChord::new(ModFlags::CTRL, Key::Char('p'));
    let second = KeyChord::new(ModFlags::NONE, Key::Char('n'));
    let sequence = KeySequence::new(first, vec![second]);
    assert_eq!(sequence.chords(), &[first, second]);
}

#[test]
fn key_sequence_from_a_single_chord_holds_that_chord() {
    let chord = KeyChord::new(ModFlags::ALT, Key::Char('t'));
    assert_eq!(KeySequence::from(chord).chords(), &[chord]);
}

#[test]
fn key_sequence_displays_chords_space_separated() {
    let sequence = KeySequence::new(
        KeyChord::new(ModFlags::CTRL, Key::Char('p')),
        vec![
            KeyChord::new(ModFlags::NONE, Key::Char('n')),
            KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Enter)),
        ],
    );
    assert_eq!(sequence.to_string(), "<C-p> n <CR>");
}

#[test]
fn key_sequence_display_of_one_chord_is_that_chord() {
    let sequence = KeySequence::from(KeyChord::new(ModFlags::NONE, Key::Char('g')));
    assert_eq!(sequence.to_string(), "g");
}

#[test]
fn fold_uppercase_folds_single_char_lowercase_letters_only() {
    // ASCII and non-ASCII uppercase letters fold to lowercase plus Shift.
    assert_eq!(fold_uppercase('A'), ('a', true));
    assert_eq!(fold_uppercase('É'), ('é', true));
    // Already-lowercase and non-letter characters stand as they are.
    assert_eq!(fold_uppercase('a'), ('a', false));
    assert_eq!(fold_uppercase('é'), ('é', false));
    assert_eq!(fold_uppercase('!'), ('!', false));
    assert_eq!(fold_uppercase('1'), ('1', false));
    // An uppercase letter whose lowercase form is more than one character
    // stands as it is, unshifted.
    assert_eq!(fold_uppercase('İ'), ('İ', false));
}

#[test]
fn fold_uppercase_folds_uppercase_letters_outside_latin_script() {
    // Greek capital sigma lowercases to one char, and uppercases back to
    // itself: folds like any other letter.
    assert_eq!(fold_uppercase('Σ'), ('σ', true));
    // Roman numeral four is an uppercase letter whose lowercase form is a
    // single different character, not a case variant of a Latin letter. It
    // uppercases back to itself, so it folds.
    assert_eq!(fold_uppercase('Ⅳ'), ('ⅳ', true));
}

#[test]
fn fold_uppercase_refuses_a_fold_it_could_not_undo() {
    // Capital sharp S (`ẞ`) lowercases to the single-char `ß` — but `ß`
    // uppercases to the two-char `"SS"`, so `Shift + ß` cannot rebuild `ẞ`.
    // A chord is all the input layer keeps of a key press: if it folded here,
    // an unbound `ẞ` would reach the pane as `ß` and silently change the user's
    // text. So the fold only happens when the capital comes back.
    assert_eq!(fold_uppercase('ẞ'), ('ẞ', false));
}

#[test]
fn fold_uppercase_at_the_top_of_the_char_range_is_a_no_op() {
    // `char::MAX` is unassigned, so it is not uppercase and stands as-is —
    // exercises the boundary of the full `char` domain the function accepts.
    assert_eq!(fold_uppercase(char::MAX), (char::MAX, false));
}

#[test]
fn named_key_f_key_number_boundaries_display_exactly() {
    assert_eq!(NamedKey::F(0).to_string(), "F0");
    assert_eq!(NamedKey::F(255).to_string(), "F255");
}

#[test]
fn a_chord_survives_a_serde_round_trip() {
    for chord in [
        KeyChord::new(ModFlags::NONE, Key::Char('a')),
        KeyChord::new(ModFlags::CTRL | ModFlags::SHIFT, Key::Char('a')),
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::F(12))),
        KeyChord::new(ModFlags::ALT | ModFlags::SUPER, Key::Named(NamedKey::Enter)),
    ] {
        let json = serde_json::to_string(&chord).expect("serialize");
        let restored: KeyChord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, chord);
    }
}

#[test]
fn decoding_refuses_a_modifier_bit_that_names_no_modifier() {
    // Bit 4 is the lowest one past Super, the highest modifier.
    let refused = serde_json::from_str::<KeyChord>(r#"{"mods":16,"key":{"Char":"q"}}"#)
        .expect_err("bit 4 names no modifier");
    assert_eq!(
        refused.to_string(),
        "modifier bits 0b00010000 name no modifier; the modifiers are 0b00001111 \
         at line 1 column 10"
    );

    // Every bit the four modifiers do occupy still decodes.
    let all_four = serde_json::from_str::<KeyChord>(r#"{"mods":15,"key":{"Char":"q"}}"#)
        .expect("the four modifier bits together");
    assert_eq!(
        all_four,
        KeyChord::new(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Char('q')
        )
    );
}

#[test]
fn decoding_refuses_a_function_key_number_no_terminal_names() {
    // The column is where the number ends, so it grows with the number's digits.
    for (number, column) in [(0_u8, 32), (25, 33), (255, 34)] {
        let json = format!(r#"{{"mods":0,"key":{{"Named":{{"F":{number}}}}}}}"#);
        let refused = serde_json::from_str::<KeyChord>(&json)
            .expect_err("a function key outside F1 through F24");
        assert_eq!(
            refused.to_string(),
            format!(
                "F{number} is not a function key; they run F1 through F24 \
                 at line 1 column {column}"
            )
        );
    }

    // The two ends of the range still decode.
    for number in [1_u8, 24] {
        let json = format!(r#"{{"mods":0,"key":{{"Named":{{"F":{number}}}}}}}"#);
        let decoded: KeyChord = serde_json::from_str(&json).expect("a real function key");
        assert_eq!(
            decoded,
            KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::F(number)))
        );
    }
}

#[test]
fn the_chord_wire_form_is_the_field_names_the_modifier_bits_and_the_variant_name() {
    assert_eq!(
        serde_json::to_string(&KeyChord::new(ModFlags::CTRL, Key::Char('q'))).expect("serialize"),
        r#"{"mods":1,"key":{"Char":"q"}}"#
    );
}

#[test]
fn every_combination_of_non_text_modifiers_makes_a_chord_untypeable() {
    let non_text_combos = [
        ModFlags::CTRL | ModFlags::ALT,
        ModFlags::CTRL | ModFlags::SUPER,
        ModFlags::ALT | ModFlags::SUPER,
        ModFlags::CTRL | ModFlags::ALT | ModFlags::SUPER,
        ModFlags::CTRL | ModFlags::ALT | ModFlags::SUPER | ModFlags::SHIFT,
    ];
    for mods in non_text_combos {
        assert!(
            !KeyChord::new(mods, Key::Char('p')).is_typeable(),
            "{mods} should be untypeable"
        );
    }
}

#[test]
fn mod_flags_default_is_none() {
    assert_eq!(ModFlags::default(), ModFlags::NONE);
}

#[test]
fn try_from_accepts_the_four_modifier_bits_and_refuses_every_other() {
    assert_eq!(ModFlags::try_from(0), Ok(ModFlags::NONE));
    assert_eq!(
        ModFlags::try_from(15),
        Ok(ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER)
    );
    assert_eq!(
        ModFlags::try_from(16),
        Err("modifier bits 0b00010000 name no modifier; the modifiers are 0b00001111".to_string())
    );
    assert_eq!(
        ModFlags::try_from(255),
        Err("modifier bits 0b11111111 name no modifier; the modifiers are 0b00001111".to_string())
    );
}

#[test]
fn mod_flags_serde_wire_form_is_the_bit_number() {
    let ctrl_super = ModFlags::CTRL | ModFlags::SUPER;
    assert_eq!(serde_json::to_string(&ctrl_super).expect("serialize"), "9");
    assert_eq!(
        serde_json::from_str::<ModFlags>("9").expect("deserialize"),
        ctrl_super
    );
}

#[test]
fn decoding_refuses_a_negative_modifier_number() {
    let refused = serde_json::from_str::<ModFlags>("-1").expect_err("a u8 is never negative");
    assert_eq!(
        refused.to_string(),
        "invalid value: integer `-1`, expected u8 at line 1 column 2"
    );
}

#[test]
fn the_named_key_wire_form_is_the_variant_name_with_the_function_key_number() {
    assert_eq!(
        serde_json::to_string(&KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Space)))
            .expect("serialize"),
        r#"{"mods":0,"key":{"Named":"Space"}}"#
    );
    assert_eq!(
        serde_json::to_string(&KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::F(12))))
            .expect("serialize"),
        r#"{"mods":4,"key":{"Named":{"F":12}}}"#
    );
}

#[test]
fn key_sequence_with_no_rest_holds_only_the_first_chord() {
    let chord = KeyChord::new(ModFlags::CTRL, Key::Char('x'));
    assert_eq!(KeySequence::new(chord, Vec::new()).chords(), &[chord]);
}

#[test]
fn fold_uppercase_leaves_a_capital_whose_lowercase_uppercases_to_another_capital() {
    // The Kelvin sign lowercases to the Latin `k`, which uppercases to the
    // Latin `K`, not back to the Kelvin sign.
    assert_eq!(fold_uppercase('\u{212A}'), ('\u{212A}', false));
    // The Ohm sign lowercases to `ω`, which uppercases to the Greek `Ω`.
    assert_eq!(fold_uppercase('\u{2126}'), ('\u{2126}', false));
}

#[test]
fn fold_uppercase_leaves_a_titlecase_letter_and_folds_its_uppercase_form() {
    // `ǅ` is titlecase, not uppercase: it stands as it is.
    assert_eq!(fold_uppercase('\u{01C5}'), ('\u{01C5}', false));
    // `Ǆ` is uppercase and lowercases to the single `ǆ`, which uppercases back.
    assert_eq!(fold_uppercase('\u{01C4}'), ('\u{01C6}', true));
}
