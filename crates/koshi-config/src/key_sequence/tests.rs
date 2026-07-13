//! Tests for the key sequence parser: tokenizing, leader substitution and
//! merging, the chord-depth cap, every rejection, and the round trip through
//! the canonical text form a sequence renders.

use koshi_core::key::NamedKey;

use super::*;

/// Builds the chord a test expects, keeping the assertions readable.
fn chord(mods: ModFlags, key: Key) -> KeyChord {
    KeyChord::new(mods, key)
}

/// Builds the sequence a test expects from its chords.
fn seq(chords: &[KeyChord]) -> KeySequence {
    KeySequence::new(chords[0], chords[1..].to_vec())
}

/// The default leader, a `C-` modifier run.
fn ctrl_leader() -> Leader {
    Leader::Mods(ModFlags::CTRL)
}

/// A Space chord leader.
fn space_leader() -> Leader {
    Leader::Chord(chord(ModFlags::NONE, Key::Named(NamedKey::Space)))
}

// -- accepted sequences ---------------------------------------------------

#[test]
fn a_single_bare_character_is_a_one_chord_sequence() {
    assert_eq!(
        parse_sequence("q", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::NONE, Key::Char('q'))]))
    );
}

#[test]
fn a_single_bracketed_chord_is_a_one_chord_sequence() {
    assert_eq!(
        parse_sequence("<C-p>", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::CTRL, Key::Char('p'))]))
    );
}

#[test]
fn whitespace_separates_chords() {
    assert_eq!(
        parse_sequence("<C-p> n", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('p')),
            chord(ModFlags::NONE, Key::Char('n')),
        ]))
    );
}

#[test]
fn any_whitespace_separates_and_leading_trailing_whitespace_is_ignored() {
    assert_eq!(
        parse_sequence("  a\tb ", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Char('a')),
            chord(ModFlags::NONE, Key::Char('b')),
        ]))
    );
}

#[test]
fn adjacent_bare_characters_are_one_chord_each() {
    assert_eq!(
        parse_sequence("gg", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Char('g')),
            chord(ModFlags::NONE, Key::Char('g')),
        ]))
    );
}

#[test]
fn adjacent_bracketed_tokens_are_one_chord_each() {
    assert_eq!(
        parse_sequence("<F2><Tab>", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Named(NamedKey::F(2))),
            chord(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        ]))
    );
}

#[test]
fn a_bracketed_token_followed_by_a_bare_character_needs_no_space() {
    assert_eq!(
        parse_sequence("<C-p>n", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('p')),
            chord(ModFlags::NONE, Key::Char('n')),
        ]))
    );
}

#[test]
fn an_uppercase_bare_character_folds_into_the_shift_bit() {
    assert_eq!(
        parse_sequence("G", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::SHIFT, Key::Char('g'))]))
    );
}

#[test]
fn a_multibyte_bare_character_is_one_chord() {
    assert_eq!(
        parse_sequence("é", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::NONE, Key::Char('é'))]))
    );
}

// -- the `>` key inside a sequence ----------------------------------------

#[test]
fn a_modified_greater_than_key_extends_through_the_real_closer() {
    assert_eq!(
        parse_sequence("<C->>", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::CTRL, Key::Char('>'))]))
    );
    assert_eq!(
        parse_sequence("<C->> a", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('>')),
            chord(ModFlags::NONE, Key::Char('a')),
        ]))
    );
}

#[test]
fn a_bare_greater_than_after_a_bracketed_chord_is_its_own_chord() {
    assert_eq!(
        parse_sequence("<C-a>>", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('a')),
            chord(ModFlags::NONE, Key::Char('>')),
        ]))
    );
}

#[test]
fn a_modified_dash_key_does_not_swallow_a_following_greater_than() {
    assert_eq!(
        parse_sequence("<C-->>", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('-')),
            chord(ModFlags::NONE, Key::Char('>')),
        ]))
    );
}

#[test]
fn a_bracketed_less_than_key_is_one_chord() {
    assert_eq!(
        parse_sequence("<<>", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::NONE, Key::Char('<'))]))
    );
}

// -- leader substitution --------------------------------------------------

#[test]
fn a_modifier_run_leader_merges_into_the_following_chord() {
    assert_eq!(
        parse_sequence("<leader>wq", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('w')),
            chord(ModFlags::NONE, Key::Char('q')),
        ]))
    );
}

#[test]
fn whitespace_after_the_leader_does_not_stop_the_merge() {
    assert_eq!(
        parse_sequence("<leader> wq", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('w')),
            chord(ModFlags::NONE, Key::Char('q')),
        ]))
    );
}

#[test]
fn a_chord_leader_stands_as_its_own_opening_chord() {
    assert_eq!(
        parse_sequence("<leader>gd", space_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Named(NamedKey::Space)),
            chord(ModFlags::NONE, Key::Char('g')),
            chord(ModFlags::NONE, Key::Char('d')),
        ]))
    );
}

#[test]
fn a_chord_leader_alone_is_a_one_chord_sequence() {
    assert_eq!(
        parse_sequence("<leader>", space_leader(), 4),
        Ok(seq(&[chord(ModFlags::NONE, Key::Named(NamedKey::Space))]))
    );
}

#[test]
fn the_leader_token_matches_case_insensitively() {
    assert_eq!(
        parse_sequence("<Leader>x", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::CTRL, Key::Char('x'))]))
    );
}

#[test]
fn merging_a_modifier_the_chord_already_holds_changes_nothing() {
    assert_eq!(
        parse_sequence("<leader><C-x>", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::CTRL, Key::Char('x'))]))
    );
}

#[test]
fn a_modifier_run_leader_merges_into_a_named_key() {
    assert_eq!(
        parse_sequence("<leader><Tab>", ctrl_leader(), 4),
        Ok(seq(&[chord(ModFlags::CTRL, Key::Named(NamedKey::Tab))]))
    );
}

#[test]
fn a_shift_only_leader_merges_into_a_letter() {
    assert_eq!(
        parse_sequence("<leader>l", Leader::Mods(ModFlags::SHIFT), 4),
        Ok(seq(&[chord(ModFlags::SHIFT, Key::Char('l'))]))
    );
}

#[test]
fn a_shift_only_leader_merges_into_a_named_key() {
    assert_eq!(
        parse_sequence("<leader><Tab>", Leader::Mods(ModFlags::SHIFT), 4),
        Ok(seq(&[chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab))]))
    );
}

// -- the chord-depth cap --------------------------------------------------

#[test]
fn a_sequence_at_the_cap_parses() {
    assert_eq!(
        parse_sequence("abcd", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Char('a')),
            chord(ModFlags::NONE, Key::Char('b')),
            chord(ModFlags::NONE, Key::Char('c')),
            chord(ModFlags::NONE, Key::Char('d')),
        ]))
    );
}

#[test]
fn a_cap_of_zero_rejects_every_sequence() {
    // A one-chord sequence still holds one chord, which is already past a
    // cap of zero.
    assert_eq!(
        parse_sequence("a", ctrl_leader(), 0),
        Err(KeyParseError {
            token: "a".to_string(),
            kind: KeyParseErrorKind::SequenceTooLong { len: 1, max: 0 },
        })
    );
}

#[test]
fn a_sequence_past_the_cap_is_rejected() {
    assert_eq!(
        parse_sequence("abcde", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "abcde".to_string(),
            kind: KeyParseErrorKind::SequenceTooLong { len: 5, max: 4 },
        })
    );
}

#[test]
fn a_modifier_run_leader_adds_no_chord_toward_the_cap() {
    assert_eq!(
        parse_sequence("<leader>abcd", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('a')),
            chord(ModFlags::NONE, Key::Char('b')),
            chord(ModFlags::NONE, Key::Char('c')),
            chord(ModFlags::NONE, Key::Char('d')),
        ]))
    );
}

#[test]
fn a_chord_leader_counts_toward_the_cap() {
    assert_eq!(
        parse_sequence("<leader>abcd", space_leader(), 4),
        Err(KeyParseError {
            token: "<leader>abcd".to_string(),
            kind: KeyParseErrorKind::SequenceTooLong { len: 5, max: 4 },
        })
    );
}

// -- rejections -----------------------------------------------------------

#[test]
fn an_empty_sequence_is_rejected() {
    assert_eq!(
        parse_sequence("", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "".to_string(),
            kind: KeyParseErrorKind::Empty,
        })
    );
    assert_eq!(
        parse_sequence("  ", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "  ".to_string(),
            kind: KeyParseErrorKind::Empty,
        })
    );
}

#[test]
fn a_leader_past_the_first_position_is_rejected() {
    assert_eq!(
        parse_sequence("g<leader>", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "<leader>".to_string(),
            kind: KeyParseErrorKind::LeaderNotFirst,
        })
    );
}

#[test]
fn a_second_leader_is_rejected() {
    assert_eq!(
        parse_sequence("<leader><leader>", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "<leader>".to_string(),
            kind: KeyParseErrorKind::LeaderNotFirst,
        })
    );
}

#[test]
fn a_modifier_run_leader_alone_is_rejected() {
    assert_eq!(
        parse_sequence("<leader>", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "<leader>".to_string(),
            kind: KeyParseErrorKind::DanglingLeaderMods,
        })
    );
}

#[test]
fn a_shift_only_leader_merging_into_a_non_letter_is_rejected() {
    assert_eq!(
        parse_sequence("<leader>1", Leader::Mods(ModFlags::SHIFT), 4),
        Err(KeyParseError {
            token: "1".to_string(),
            kind: KeyParseErrorKind::ShiftOnNonLetter { ch: '1' },
        })
    );
}

#[test]
fn an_unclosed_bracket_is_rejected_with_the_rest_of_the_text() {
    assert_eq!(
        parse_sequence("a <C-p", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "<C-p".to_string(),
            kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn a_bad_token_mid_sequence_names_that_token() {
    assert_eq!(
        parse_sequence("a <C-C-b>", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "<C-C-b>".to_string(),
            kind: KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
        })
    );
}

#[test]
fn missing_key_in_a_bracketed_token_is_rejected_when_no_closer_follows() {
    assert_eq!(
        parse_sequence("<C-> a", ctrl_leader(), 4),
        Err(KeyParseError {
            token: "<C->".to_string(),
            kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn a_bare_word_in_the_dash_form_is_rejected() {
    for text in ["Ctrl-g", "Alt-Shift-N", "a-b", "x Ctrl-g"] {
        let word = text.split_whitespace().last().expect("non-empty");
        assert_eq!(
            parse_sequence(text, ctrl_leader(), 8),
            Err(KeyParseError {
                token: word.to_string(),
                kind: KeyParseErrorKind::UnbracketedMultiChar,
            }),
            "input `{text}`"
        );
    }
}

#[test]
fn a_dash_chord_is_legal_when_separated_or_at_a_word_edge() {
    assert_eq!(
        parse_sequence("a - b", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Char('a')),
            chord(ModFlags::NONE, Key::Char('-')),
            chord(ModFlags::NONE, Key::Char('b')),
        ]))
    );
    assert_eq!(
        parse_sequence("g-", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Char('g')),
            chord(ModFlags::NONE, Key::Char('-')),
        ]))
    );
    assert_eq!(
        parse_sequence("-g", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::NONE, Key::Char('-')),
            chord(ModFlags::NONE, Key::Char('g')),
        ]))
    );
}

#[test]
fn a_word_holding_a_bracketed_token_is_exempt_from_the_dash_form_rule() {
    assert_eq!(
        parse_sequence("<C-p>-x", ctrl_leader(), 4),
        Ok(seq(&[
            chord(ModFlags::CTRL, Key::Char('p')),
            chord(ModFlags::NONE, Key::Char('-')),
            chord(ModFlags::NONE, Key::Char('x')),
        ]))
    );
}

// -- round trips ----------------------------------------------------------

#[test]
fn the_canonical_text_form_parses_back_to_an_equal_sequence() {
    for text in ["<C-p> n", "g g", "<F2> <Tab>", "<C-w> q", "<S-Tab>", "<<>"] {
        let parsed = parse_sequence(text, ctrl_leader(), 4).expect(text);
        assert_eq!(
            parse_sequence(&parsed.to_string(), ctrl_leader(), 4),
            Ok(parsed),
            "round trip of `{text}`"
        );
    }
}
