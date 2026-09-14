//! Tests for the key sequence parser: tokenizing, leader substitution and
//! merging, the chord-depth cap, every rejection, and the round trip through
//! the canonical text form a sequence renders.

use koshi_core::key::NamedKey;

use super::*;

/// Builds the key chord a test expects, keeping the assertions readable.
fn build_key_chord(modifier_flags: ModFlags, key: Key) -> KeyChord {
    KeyChord::from_parts(modifier_flags, key)
}

/// Builds the key sequence a test expects from its chords.
fn build_key_sequence(key_chords: &[KeyChord]) -> KeySequence {
    KeySequence::from_first_and_rest(key_chords[0], key_chords[1..].to_vec())
}

/// The default leader, a `C-` modifier run.
fn build_control_leader() -> Leader {
    Leader::Mods(ModFlags::CTRL)
}

/// A Space chord leader.
fn build_space_leader() -> Leader {
    Leader::Chord(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space)))
}

// -- accepted sequences ---------------------------------------------------

#[test]
fn a_single_bare_character_is_a_one_chord_sequence() {
    assert_eq!(
        parse_sequence("q", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::NONE,
            Key::Char('q'),
        )]))
    );
}

#[test]
fn a_single_bracketed_chord_is_a_one_chord_sequence() {
    assert_eq!(
        parse_sequence("<C-p>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL,
            Key::Char('p'),
        )]))
    );
}

#[test]
fn whitespace_separates_chords() {
    assert_eq!(
        parse_sequence("<C-p> n", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('p')),
            build_key_chord(ModFlags::NONE, Key::Char('n')),
        ]))
    );
}

#[test]
fn any_whitespace_separates_and_leading_trailing_whitespace_is_ignored() {
    assert_eq!(
        parse_sequence("  a\tb ", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('b')),
        ]))
    );
}

#[test]
fn adjacent_bare_characters_are_one_chord_each() {
    assert_eq!(
        parse_sequence("gg", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('g')),
            build_key_chord(ModFlags::NONE, Key::Char('g')),
        ]))
    );
}

#[test]
fn adjacent_bracketed_tokens_are_one_chord_each() {
    assert_eq!(
        parse_sequence("<F2><Tab>", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(2))),
            build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        ]))
    );
}

#[test]
fn a_bracketed_token_followed_by_a_bare_character_needs_no_space() {
    assert_eq!(
        parse_sequence("<C-p>n", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('p')),
            build_key_chord(ModFlags::NONE, Key::Char('n')),
        ]))
    );
}

#[test]
fn an_uppercase_bare_character_folds_into_the_shift_bit() {
    assert_eq!(
        parse_sequence("G", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::SHIFT,
            Key::Char('g'),
        )]))
    );
}

#[test]
fn a_multibyte_bare_character_is_one_chord() {
    assert_eq!(
        parse_sequence("é", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::NONE,
            Key::Char('é'),
        )]))
    );
}

#[test]
fn adjacent_multibyte_characters_split_on_character_boundaries() {
    assert_eq!(
        parse_sequence("é☃", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('é')),
            build_key_chord(ModFlags::NONE, Key::Char('☃')),
        ]))
    );
}

#[test]
fn a_newline_separates_chords() {
    assert_eq!(
        parse_sequence("a\nb", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('b')),
        ]))
    );
}

// -- the `>` key inside a sequence ----------------------------------------

#[test]
fn a_modified_greater_than_key_extends_through_the_real_closer() {
    assert_eq!(
        parse_sequence("<C->>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL,
            Key::Char('>'),
        )]))
    );
    assert_eq!(
        parse_sequence("<C->> a", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('>')),
            build_key_chord(ModFlags::NONE, Key::Char('a')),
        ]))
    );
}

#[test]
fn a_bare_greater_than_after_a_bracketed_chord_is_its_own_chord() {
    assert_eq!(
        parse_sequence("<C-a>>", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('>')),
        ]))
    );
}

#[test]
fn a_modified_dash_key_does_not_swallow_a_following_greater_than() {
    assert_eq!(
        parse_sequence("<C-->>", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('-')),
            build_key_chord(ModFlags::NONE, Key::Char('>')),
        ]))
    );
}

#[test]
fn a_bracketed_less_than_key_is_one_chord() {
    assert_eq!(
        parse_sequence("<<>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::NONE,
            Key::Char('<'),
        )]))
    );
}

// -- leader substitution --------------------------------------------------

#[test]
fn a_modifier_run_leader_merges_into_the_following_chord() {
    assert_eq!(
        parse_sequence("<leader>wq", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('w')),
            build_key_chord(ModFlags::NONE, Key::Char('q')),
        ]))
    );
}

#[test]
fn whitespace_after_the_leader_does_not_stop_the_merge() {
    assert_eq!(
        parse_sequence("<leader> wq", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('w')),
            build_key_chord(ModFlags::NONE, Key::Char('q')),
        ]))
    );
}

#[test]
fn a_chord_leader_stands_as_its_own_opening_chord() {
    assert_eq!(
        parse_sequence("<leader>gd", build_space_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space)),
            build_key_chord(ModFlags::NONE, Key::Char('g')),
            build_key_chord(ModFlags::NONE, Key::Char('d')),
        ]))
    );
}

#[test]
fn a_chord_leader_alone_is_a_one_chord_sequence() {
    assert_eq!(
        parse_sequence("<leader>", build_space_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::NONE,
            Key::Named(NamedKey::Space),
        )]))
    );
}

#[test]
fn the_leader_token_matches_case_insensitively() {
    assert_eq!(
        parse_sequence("<Leader>x", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL,
            Key::Char('x'),
        )]))
    );
}

#[test]
fn merging_a_modifier_the_chord_already_holds_changes_nothing() {
    assert_eq!(
        parse_sequence("<leader><C-x>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL,
            Key::Char('x'),
        )]))
    );
}

#[test]
fn a_modifier_run_leader_unions_with_the_chords_own_modifiers() {
    assert_eq!(
        parse_sequence("<leader><A-x>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL | ModFlags::ALT,
            Key::Char('x')
        )]))
    );
}

#[test]
fn a_modifier_run_leader_merges_into_a_named_key() {
    assert_eq!(
        parse_sequence("<leader><Tab>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL,
            Key::Named(NamedKey::Tab),
        )]))
    );
}

#[test]
fn a_shift_only_leader_merges_into_a_letter() {
    assert_eq!(
        parse_sequence("<leader>l", Leader::Mods(ModFlags::SHIFT), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::SHIFT,
            Key::Char('l'),
        )]))
    );
}

#[test]
fn a_shift_only_leader_merges_into_a_named_key() {
    assert_eq!(
        parse_sequence("<leader><Tab>", Leader::Mods(ModFlags::SHIFT), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::SHIFT,
            Key::Named(NamedKey::Tab),
        )]))
    );
}

// -- the chord-depth cap --------------------------------------------------

#[test]
fn a_sequence_at_the_cap_parses() {
    assert_eq!(
        parse_sequence("abcd", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('b')),
            build_key_chord(ModFlags::NONE, Key::Char('c')),
            build_key_chord(ModFlags::NONE, Key::Char('d')),
        ]))
    );
}

#[test]
fn a_cap_of_zero_rejects_every_sequence() {
    // A one-chord sequence still holds one chord, which is already past a
    // cap of zero.
    assert_eq!(
        parse_sequence("a", build_control_leader(), 0),
        Err(KeyParseError {
            key_token: "a".to_string(),
            error_kind: KeyParseErrorKind::SequenceTooLong {
                chord_count: 1,
                max_chord_depth: 0,
            },
        })
    );
}

#[test]
fn a_sequence_past_the_cap_is_rejected() {
    assert_eq!(
        parse_sequence("abcde", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "abcde".to_string(),
            error_kind: KeyParseErrorKind::SequenceTooLong {
                chord_count: 5,
                max_chord_depth: 4,
            },
        })
    );
}

#[test]
fn a_modifier_run_leader_adds_no_chord_toward_the_cap() {
    assert_eq!(
        parse_sequence("<leader>abcd", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('b')),
            build_key_chord(ModFlags::NONE, Key::Char('c')),
            build_key_chord(ModFlags::NONE, Key::Char('d')),
        ]))
    );
}

#[test]
fn a_chord_leader_filling_the_cap_exactly_parses() {
    assert_eq!(
        parse_sequence("<leader>abc", build_space_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space)),
            build_key_chord(ModFlags::NONE, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('b')),
            build_key_chord(ModFlags::NONE, Key::Char('c')),
        ]))
    );
}

#[test]
fn a_chord_leader_plus_one_chord_is_past_a_cap_of_one() {
    assert_eq!(
        parse_sequence("<leader>a", build_space_leader(), 1),
        Err(KeyParseError {
            key_token: "<leader>a".to_string(),
            error_kind: KeyParseErrorKind::SequenceTooLong {
                chord_count: 2,
                max_chord_depth: 1,
            },
        })
    );
}

#[test]
fn a_chord_leader_counts_toward_the_cap() {
    assert_eq!(
        parse_sequence("<leader>abcd", build_space_leader(), 4),
        Err(KeyParseError {
            key_token: "<leader>abcd".to_string(),
            error_kind: KeyParseErrorKind::SequenceTooLong {
                chord_count: 5,
                max_chord_depth: 4,
            },
        })
    );
}

// -- rejections -----------------------------------------------------------

#[test]
fn an_empty_sequence_is_rejected() {
    assert_eq!(
        parse_sequence("", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "".to_string(),
            error_kind: KeyParseErrorKind::Empty,
        })
    );
    assert_eq!(
        parse_sequence("  ", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "  ".to_string(),
            error_kind: KeyParseErrorKind::Empty,
        })
    );
}

#[test]
fn a_leader_past_the_first_position_is_rejected() {
    assert_eq!(
        parse_sequence("g<leader>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<leader>".to_string(),
            error_kind: KeyParseErrorKind::LeaderNotFirst,
        })
    );
}

#[test]
fn a_second_leader_is_rejected() {
    assert_eq!(
        parse_sequence("<leader><leader>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<leader>".to_string(),
            error_kind: KeyParseErrorKind::LeaderNotFirst,
        })
    );
}

#[test]
fn a_modifier_run_leader_alone_is_rejected() {
    assert_eq!(
        parse_sequence("<leader>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<leader>".to_string(),
            error_kind: KeyParseErrorKind::DanglingLeaderMods,
        })
    );
}

#[test]
fn a_shift_only_leader_merging_into_a_non_letter_is_rejected() {
    assert_eq!(
        parse_sequence("<leader>1", Leader::Mods(ModFlags::SHIFT), 4),
        Err(KeyParseError {
            key_token: "1".to_string(),
            error_kind: KeyParseErrorKind::ShiftOnNonLetter { key_character: '1' },
        })
    );
}

#[test]
fn a_shift_only_leader_merging_into_a_modified_non_letter_is_rejected() {
    // The merge check runs on the already-parsed chord. The failing token is
    // the whole `<C-1>`, not the bare `1`.
    assert_eq!(
        parse_sequence("<leader><C-1>", Leader::Mods(ModFlags::SHIFT), 4),
        Err(KeyParseError {
            key_token: "<C-1>".to_string(),
            error_kind: KeyParseErrorKind::ShiftOnNonLetter { key_character: '1' },
        })
    );
}

#[test]
fn a_bracketed_leader_name_carrying_modifiers_is_not_the_leader_token() {
    assert_eq!(
        parse_sequence("<C-leader>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<C-leader>".to_string(),
            error_kind: KeyParseErrorKind::UnknownNamedKey {
                key_name: "leader".to_string(),
            },
        })
    );
}

#[test]
fn an_empty_bracketed_token_is_rejected() {
    assert_eq!(
        parse_sequence("<>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<>".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn an_unclosed_bracket_is_rejected_with_the_rest_of_the_text() {
    assert_eq!(
        parse_sequence("a <C-p", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<C-p".to_string(),
            error_kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn a_bad_token_mid_sequence_names_that_token() {
    assert_eq!(
        parse_sequence("a <C-C-b>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<C-C-b>".to_string(),
            error_kind: KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
        })
    );
}

#[test]
fn missing_key_in_a_bracketed_token_is_rejected_when_no_closer_follows() {
    assert_eq!(
        parse_sequence("<C-> a", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<C->".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn a_bare_word_in_the_dash_form_is_rejected() {
    for sequence_text in ["Ctrl-g", "Alt-Shift-N", "a-b", "x Ctrl-g"] {
        let key_token = sequence_text
            .split_whitespace()
            .last()
            .expect("sequence_text is not empty");
        assert_eq!(
            parse_sequence(sequence_text, build_control_leader(), 8),
            Err(KeyParseError {
                key_token: key_token.to_string(),
                error_kind: KeyParseErrorKind::UnbracketedMultiChar,
            }),
            "sequence `{sequence_text}`"
        );
    }
}

#[test]
fn the_first_dash_form_word_names_the_error() {
    assert_eq!(
        parse_sequence("a-b c-d", build_control_leader(), 8),
        Err(KeyParseError {
            key_token: "a-b".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn the_dash_form_check_runs_before_tokenizing() {
    // `<C-p` never closes. The dash-form scan covers the whole text before
    // tokenizing: the reported token is `a-b`, not `<C-p`.
    assert_eq!(
        parse_sequence("a-b <C-p", build_control_leader(), 8),
        Err(KeyParseError {
            key_token: "a-b".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn a_dash_chord_is_legal_when_separated_or_at_a_word_edge() {
    assert_eq!(
        parse_sequence("a - b", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('a')),
            build_key_chord(ModFlags::NONE, Key::Char('-')),
            build_key_chord(ModFlags::NONE, Key::Char('b')),
        ]))
    );
    assert_eq!(
        parse_sequence("g-", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('g')),
            build_key_chord(ModFlags::NONE, Key::Char('-')),
        ]))
    );
    assert_eq!(
        parse_sequence("-g", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::NONE, Key::Char('-')),
            build_key_chord(ModFlags::NONE, Key::Char('g')),
        ]))
    );
}

#[test]
fn a_word_holding_a_bracketed_token_is_exempt_from_the_dash_form_rule() {
    assert_eq!(
        parse_sequence("<C-p>-x", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('p')),
            build_key_chord(ModFlags::NONE, Key::Char('-')),
            build_key_chord(ModFlags::NONE, Key::Char('x')),
        ]))
    );
}

#[test]
fn a_dash_form_outside_a_bracketed_run_is_refused_wherever_it_sits() {
    // The scan steps over each `<…>` run and reads what is left: `a-b` before
    // a bracket and `Ctrl-g` after one are both the dash form.
    assert_eq!(
        parse_sequence("a-b<C-p>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "a-b<C-p>".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
    assert_eq!(
        parse_sequence("<leader>Ctrl-g", build_control_leader(), 8),
        Err(KeyParseError {
            key_token: "<leader>Ctrl-g".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn a_bracketed_run_is_never_read_as_the_dash_form() {
    // `<C-p>` holds a dash between two alphanumerics, and it is bracketed.
    assert_eq!(
        parse_sequence("<C-p><S-a>", build_control_leader(), 4),
        Ok(build_key_sequence(&[
            build_key_chord(ModFlags::CTRL, Key::Char('p')),
            build_key_chord(ModFlags::SHIFT, Key::Char('a')),
        ]))
    );
}

#[test]
fn an_empty_bracket_before_a_close_is_still_refused() {
    // `<>` names no modifier run, so the `<C->>` recovery does not extend it
    // through the next `>`.
    assert_eq!(
        parse_sequence("<>>", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<>".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn a_dash_form_word_of_non_ascii_letters_is_rejected() {
    // The dash-form scan reads Unicode alphanumerics, not ASCII only.
    assert_eq!(
        parse_sequence("é-ü", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "é-ü".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn a_lone_open_bracket_is_an_unclosed_bracket() {
    assert_eq!(
        parse_sequence("<", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<".to_string(),
            error_kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn an_unclosed_bracket_swallows_the_words_after_it() {
    // Nothing closes `<C-p`, so the reported token runs to the end of the
    // text, the following ` a` included.
    assert_eq!(
        parse_sequence("<C-p a", build_control_leader(), 4),
        Err(KeyParseError {
            key_token: "<C-p a".to_string(),
            error_kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

// -- round trips ----------------------------------------------------------

#[test]
fn the_canonical_text_form_parses_back_to_an_equal_sequence() {
    for sequence_text in ["<C-p> n", "g g", "<F2> <Tab>", "<C-w> q", "<S-Tab>", "<<>"] {
        let parsed_sequence =
            parse_sequence(sequence_text, build_control_leader(), 4).expect(sequence_text);
        assert_eq!(
            parse_sequence(&parsed_sequence.to_string(), build_control_leader(), 4),
            Ok(parsed_sequence),
            "round trip of `{sequence_text}`"
        );
    }
}

#[test]
fn the_canonical_text_form_of_awkward_keys_parses_back_to_an_equal_sequence() {
    // The bracket characters, a bare `>`, and a full modifier run.
    for sequence_text in [
        "<C->>",
        "<C-->",
        "<<>",
        "> a",
        "<C-A-S-D-a>",
        "<S-Tab> <F12>",
    ] {
        let parsed_sequence =
            parse_sequence(sequence_text, build_control_leader(), 4).expect(sequence_text);
        assert_eq!(
            parse_sequence(&parsed_sequence.to_string(), build_control_leader(), 4),
            Ok(parsed_sequence),
            "round trip of `{sequence_text}`"
        );
    }
}

#[test]
fn a_leader_substituted_sequence_round_trips_through_its_text_form() {
    // The canonical form carries no `<leader>`, so it re-parses to the same
    // sequence under any leader: `<leader>wq` renders as `<C-w> q`.
    let parsed_sequence =
        parse_sequence("<leader>wq", build_control_leader(), 4).expect("leader merge parses");
    assert_eq!(parsed_sequence.to_string(), "<C-w> q");
    assert_eq!(
        parse_sequence(&parsed_sequence.to_string(), build_space_leader(), 4),
        Ok(parsed_sequence)
    );
}

// -- adversarial: hostile length and bytes --------------------------------

#[test]
fn an_absurdly_long_sequence_reports_its_length_without_panicking() {
    // A thousand single-character chords, well past the largest possible cap
    // of 255, is counted and reported once — not a panic, hang, or overflow.
    let sequence_text = "a ".repeat(1000);
    let parse_error = parse_sequence(&sequence_text, build_control_leader(), u8::MAX).unwrap_err();
    assert_eq!(parse_error.key_token, sequence_text);
    assert_eq!(
        parse_error.error_kind,
        KeyParseErrorKind::SequenceTooLong {
            chord_count: 1000,
            max_chord_depth: 255,
        }
    );
}

#[test]
fn a_nul_byte_token_is_refused_as_a_control_character() {
    let parse_error = parse_sequence("\0", build_control_leader(), 4).unwrap_err();
    assert_eq!(parse_error.key_token, "\0");
    assert_eq!(
        parse_error.error_kind,
        KeyParseErrorKind::RawWhitespaceOrControl {
            key_character: '\0',
        }
    );
}

#[test]
fn a_modifier_run_leader_merges_into_a_greater_than_key_that_extends() {
    // The `<C->>` "the key is `>` itself" recovery must still fire when a
    // modifier-run leader is waiting to merge into it: the result is one
    // chord, Ctrl+`>`.
    assert_eq!(
        parse_sequence("<leader><C->>", build_control_leader(), 4),
        Ok(build_key_sequence(&[build_key_chord(
            ModFlags::CTRL,
            Key::Char('>'),
        )]))
    );
}

#[test]
fn a_cap_at_the_largest_byte_value_still_rejects_one_more() {
    // 256 chords against a cap of 255 is the boundary just past the widest
    // representable cap.
    let sequence_text = "a".repeat(256);
    let parse_error = parse_sequence(&sequence_text, build_control_leader(), u8::MAX).unwrap_err();
    assert_eq!(parse_error.key_token, sequence_text);
    assert_eq!(
        parse_error.error_kind,
        KeyParseErrorKind::SequenceTooLong {
            chord_count: 256,
            max_chord_depth: 255,
        }
    );
    // Exactly 255 chords fits.
    let sequence_at_cap = "a".repeat(255);
    assert_eq!(
        parse_sequence(&sequence_at_cap, build_control_leader(), u8::MAX)
            .expect("255 chords fits the cap")
            .list_chords(),
        vec![build_key_chord(ModFlags::NONE, Key::Char('a')); 255].as_slice()
    );
}
