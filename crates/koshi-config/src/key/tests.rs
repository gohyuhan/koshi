//! Tests for the chord and leader parsers: the accepted grammar, the case fold
//! into the Shift bit, every rejection, and the round trip through the canonical
//! text form a chord renders.

use super::*;

/// Builds the key chord a test expects, keeping the assertions readable.
fn build_key_chord(modifier_flags: ModFlags, key: Key) -> KeyChord {
    KeyChord::from_parts(modifier_flags, key)
}

// -- accepted chords ------------------------------------------------------

#[test]
fn a_bare_character_is_an_unmodified_chord() {
    assert_eq!(
        parse_chord("n"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('n')))
    );
    assert_eq!(
        parse_chord("!"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('!')))
    );
    assert_eq!(
        parse_chord("-"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('-')))
    );
    assert_eq!(
        parse_chord(">"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('>')))
    );
    assert_eq!(
        parse_chord(","),
        Ok(build_key_chord(ModFlags::NONE, Key::Char(',')))
    );
}

#[test]
fn a_bare_capital_folds_into_the_shift_bit() {
    assert_eq!(
        parse_chord("N"),
        Ok(build_key_chord(ModFlags::SHIFT, Key::Char('n')))
    );
}

#[test]
fn each_modifier_letter_sets_its_bit() {
    assert_eq!(
        parse_chord("<C-p>"),
        Ok(build_key_chord(ModFlags::CTRL, Key::Char('p')))
    );
    assert_eq!(
        parse_chord("<A-p>"),
        Ok(build_key_chord(ModFlags::ALT, Key::Char('p')))
    );
    assert_eq!(
        parse_chord("<S-p>"),
        Ok(build_key_chord(ModFlags::SHIFT, Key::Char('p')))
    );
    assert_eq!(
        parse_chord("<D-p>"),
        Ok(build_key_chord(ModFlags::SUPER, Key::Char('p')))
    );
}

#[test]
fn modifier_letters_are_case_insensitive() {
    assert_eq!(parse_chord("<c-p>"), parse_chord("<C-p>"));
    assert_eq!(parse_chord("<a-s-h>"), parse_chord("<A-S-h>"));
    assert_eq!(parse_chord("<d-x>"), parse_chord("<D-x>"));
}

#[test]
fn modifiers_combine_in_any_written_order() {
    let expected_chord = build_key_chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'));
    assert_eq!(parse_chord("<A-S-h>"), Ok(expected_chord));
    assert_eq!(parse_chord("<S-A-h>"), Ok(expected_chord));

    assert_eq!(
        parse_chord("<C-A-S-D-x>"),
        Ok(build_key_chord(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Char('x')
        ))
    );
}

#[test]
fn a_capital_and_an_explicit_shift_name_the_same_chord() {
    let expected_chord = build_key_chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'));
    assert_eq!(parse_chord("<A-H>"), Ok(expected_chord));
    assert_eq!(parse_chord("<A-S-h>"), Ok(expected_chord));
    assert_eq!(parse_chord("<A-S-H>"), Ok(expected_chord));
}

#[test]
fn every_named_key_resolves() {
    let cases = [
        ("<CR>", NamedKey::Enter),
        ("<Tab>", NamedKey::Tab),
        ("<BS>", NamedKey::Backspace),
        ("<Esc>", NamedKey::Esc),
        ("<Space>", NamedKey::Space),
        ("<Insert>", NamedKey::Insert),
        ("<Del>", NamedKey::Delete),
        ("<Home>", NamedKey::Home),
        ("<End>", NamedKey::End),
        ("<PageUp>", NamedKey::PageUp),
        ("<PageDown>", NamedKey::PageDown),
        ("<Left>", NamedKey::Left),
        ("<Right>", NamedKey::Right),
        ("<Up>", NamedKey::Up),
        ("<Down>", NamedKey::Down),
    ];
    for (chord_text, key_value) in cases {
        assert_eq!(
            parse_chord(chord_text),
            Ok(build_key_chord(ModFlags::NONE, Key::Named(key_value))),
            "parsing {chord_text}"
        );
    }
}

#[test]
fn named_keys_are_case_insensitive() {
    assert_eq!(parse_chord("<esc>"), parse_chord("<Esc>"));
    assert_eq!(parse_chord("<pageup>"), parse_chord("<PageUp>"));
    assert_eq!(parse_chord("<f5>"), parse_chord("<F5>"));
}

#[test]
fn function_keys_span_f1_to_f24() {
    assert_eq!(
        parse_chord("<F1>"),
        Ok(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(1))))
    );
    assert_eq!(
        parse_chord("<F24>"),
        Ok(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(24))))
    );
}

#[test]
fn a_named_key_may_carry_modifiers() {
    assert_eq!(
        parse_chord("<S-Tab>"),
        Ok(build_key_chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab)))
    );
    assert_eq!(
        parse_chord("<C-A-F5>"),
        Ok(build_key_chord(
            ModFlags::CTRL | ModFlags::ALT,
            Key::Named(NamedKey::F(5))
        ))
    );
}

#[test]
fn the_modifier_run_stops_before_a_key_that_is_not_a_separator() {
    // `Space` begins with `S` but the next character is not `-`, so no modifier
    // is consumed.
    assert_eq!(
        parse_chord("<Space>"),
        Ok(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space)))
    );
    // `C--` is Control plus the `-` key: the run eats `C-`, the rest is the key.
    assert_eq!(
        parse_chord("<C-->"),
        Ok(build_key_chord(ModFlags::CTRL, Key::Char('-')))
    );
}

#[test]
fn the_angle_brackets_themselves_can_be_bound() {
    assert_eq!(
        parse_chord("<<>"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('<')))
    );
    assert_eq!(
        parse_chord("<C-<>"),
        Ok(build_key_chord(ModFlags::CTRL, Key::Char('<')))
    );
    assert_eq!(
        parse_chord("<C->>"),
        Ok(build_key_chord(ModFlags::CTRL, Key::Char('>')))
    );
}

#[test]
fn a_capital_with_no_single_character_lowercase_stands_as_written() {
    // U+0130 lowercases to two characters, so no fold and no Shift bit.
    assert_eq!(
        parse_chord("\u{0130}"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('\u{0130}')))
    );
}

// -- rejected chords ------------------------------------------------------

#[test]
fn the_empty_token_is_refused() {
    assert_eq!(
        parse_chord(""),
        Err(KeyParseError {
            key_token: String::new(),
            error_kind: KeyParseErrorKind::Empty,
        })
    );
}

#[test]
fn an_unclosed_bracket_is_refused() {
    assert_eq!(
        parse_chord("<C-p"),
        Err(KeyParseError {
            key_token: "<C-p".to_string(),
            error_kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn a_sequence_is_not_a_chord() {
    // `<C-p>n` is two chords; a chord is one token, so the bracket never closes
    // at the end of the token.
    assert_eq!(
        parse_chord("<C-p>n"),
        Err(KeyParseError {
            key_token: "<C-p>n".to_string(),
            error_kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn modifiers_with_no_key_are_refused() {
    assert_eq!(
        parse_chord("<>"),
        Err(KeyParseError {
            key_token: "<>".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
    assert_eq!(
        parse_chord("<C->"),
        Err(KeyParseError {
            key_token: "<C->".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
    assert_eq!(
        parse_chord("<C-S->"),
        Err(KeyParseError {
            key_token: "<C-S->".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn an_unknown_modifier_letter_is_refused() {
    assert_eq!(
        parse_chord("<x-a>"),
        Err(KeyParseError {
            key_token: "<x-a>".to_string(),
            error_kind: KeyParseErrorKind::UnknownModifier { modifier: 'x' },
        })
    );
}

#[test]
fn a_repeated_modifier_is_refused() {
    assert_eq!(
        parse_chord("<C-C-a>"),
        Err(KeyParseError {
            key_token: "<C-C-a>".to_string(),
            error_kind: KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
        })
    );
    // The repeat is caught across cases, since the letters are folded.
    assert_eq!(
        parse_chord("<C-c-a>"),
        Err(KeyParseError {
            key_token: "<C-c-a>".to_string(),
            error_kind: KeyParseErrorKind::DuplicateModifier { modifier: 'c' },
        })
    );
}

#[test]
fn a_digit_run_with_a_trailing_letter_is_not_a_function_key() {
    // `F1a` starts with `F` and digits, but the trailing `a` means the
    // digit-only check fails, so it falls through to the named-key table
    // and is refused there, not as an out-of-range function key.
    assert_eq!(
        parse_chord("<F1a>"),
        Err(KeyParseError {
            key_token: "<F1a>".to_string(),
            error_kind: KeyParseErrorKind::UnknownNamedKey {
                key_name: "F1a".to_string()
            },
        })
    );
}

#[test]
fn an_unknown_key_name_is_refused() {
    assert_eq!(
        parse_chord("<Nope>"),
        Err(KeyParseError {
            key_token: "<Nope>".to_string(),
            error_kind: KeyParseErrorKind::UnknownNamedKey {
                key_name: "Nope".to_string()
            },
        })
    );
    // `Enter` is not the accepted spelling; `CR` is.
    assert_eq!(
        parse_chord("<Enter>"),
        Err(KeyParseError {
            key_token: "<Enter>".to_string(),
            error_kind: KeyParseErrorKind::UnknownNamedKey {
                key_name: "Enter".to_string()
            },
        })
    );
}

#[test]
fn the_dash_form_is_not_the_grammar() {
    assert_eq!(
        parse_chord("Ctrl-g"),
        Err(KeyParseError {
            key_token: "Ctrl-g".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
    assert_eq!(
        parse_chord("Tab"),
        Err(KeyParseError {
            key_token: "Tab".to_string(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn shift_on_a_non_letter_is_refused() {
    assert_eq!(
        parse_chord("<S-1>"),
        Err(KeyParseError {
            key_token: "<S-1>".to_string(),
            error_kind: KeyParseErrorKind::ShiftOnNonLetter { key_character: '1' },
        })
    );
    assert_eq!(
        parse_chord("<S-->"),
        Err(KeyParseError {
            key_token: "<S-->".to_string(),
            error_kind: KeyParseErrorKind::ShiftOnNonLetter { key_character: '-' },
        })
    );
}

#[test]
fn a_function_key_outside_the_range_is_refused() {
    assert_eq!(
        parse_chord("<F0>"),
        Err(KeyParseError {
            key_token: "<F0>".to_string(),
            error_kind: KeyParseErrorKind::FunctionKeyOutOfRange {
                function_key_number_text: "0".to_string(),
            },
        })
    );
    assert_eq!(
        parse_chord("<F25>"),
        Err(KeyParseError {
            key_token: "<F25>".to_string(),
            error_kind: KeyParseErrorKind::FunctionKeyOutOfRange {
                function_key_number_text: "25".to_string()
            },
        })
    );
    // Wider than a byte, and still reported as out of range rather than unknown.
    assert_eq!(
        parse_chord("<F1000>"),
        Err(KeyParseError {
            key_token: "<F1000>".to_string(),
            error_kind: KeyParseErrorKind::FunctionKeyOutOfRange {
                function_key_number_text: "1000".to_string()
            },
        })
    );
}

#[test]
fn a_raw_whitespace_or_control_character_is_refused() {
    // A KDL escape like "\t" reaches the parser as the literal character; the
    // named spelling is the one representation of that key.
    assert_eq!(
        parse_chord("\t"),
        Err(KeyParseError {
            key_token: "\t".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\t',
            },
        })
    );
    assert_eq!(
        parse_chord(" "),
        Err(KeyParseError {
            key_token: " ".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl { key_character: ' ' },
        })
    );
    assert_eq!(
        parse_chord("\r"),
        Err(KeyParseError {
            key_token: "\r".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\r',
            },
        })
    );
    assert_eq!(
        parse_chord("\u{1b}"),
        Err(KeyParseError {
            key_token: "\u{1b}".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\u{1b}',
            },
        })
    );
    // The bracketed and modified positions go through the same fold.
    assert_eq!(
        parse_chord("< >"),
        Err(KeyParseError {
            key_token: "< >".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl { key_character: ' ' },
        })
    );
    assert_eq!(
        parse_chord("<C-\t>"),
        Err(KeyParseError {
            key_token: "<C-\t>".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\t',
            },
        })
    );
}

#[test]
fn leader_is_not_a_chord() {
    assert_eq!(
        parse_chord("<leader>"),
        Err(KeyParseError {
            key_token: "<leader>".to_string(),
            error_kind: KeyParseErrorKind::LeaderNotAChord,
        })
    );
    assert_eq!(
        parse_chord("<Leader>"),
        Err(KeyParseError {
            key_token: "<Leader>".to_string(),
            error_kind: KeyParseErrorKind::LeaderNotAChord,
        })
    );
}

// -- round trip -----------------------------------------------------------

#[test]
fn every_chord_parses_back_from_the_text_it_renders() {
    let expected_chords = [
        build_key_chord(ModFlags::NONE, Key::Char('n')),
        build_key_chord(ModFlags::NONE, Key::Char('!')),
        build_key_chord(ModFlags::NONE, Key::Char('-')),
        build_key_chord(ModFlags::NONE, Key::Char('<')),
        build_key_chord(ModFlags::NONE, Key::Char('>')),
        build_key_chord(ModFlags::SHIFT, Key::Char('n')),
        build_key_chord(ModFlags::CTRL, Key::Char('p')),
        build_key_chord(ModFlags::ALT, Key::Char('n')),
        build_key_chord(ModFlags::SUPER, Key::Char('x')),
        build_key_chord(ModFlags::CTRL, Key::Char('-')),
        build_key_chord(ModFlags::CTRL, Key::Char('<')),
        build_key_chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')),
        build_key_chord(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Char('x'),
        ),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Enter)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Backspace)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Insert)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Delete)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Home)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::End)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::PageUp)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::PageDown)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Left)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Right)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Up)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Down)),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(1))),
        build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(24))),
        build_key_chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        build_key_chord(ModFlags::CTRL | ModFlags::ALT, Key::Named(NamedKey::F(5))),
    ];
    for expected_chord in expected_chords {
        let chord_text = expected_chord.to_string();
        assert_eq!(
            parse_chord(&chord_text),
            Ok(expected_chord),
            "round trip via {chord_text:?}"
        );
    }
}

// -- leader ---------------------------------------------------------------

#[test]
fn a_trailing_dash_names_a_modifier_run() {
    assert_eq!(parse_leader("C-"), Ok(Leader::Mods(ModFlags::CTRL)));
    assert_eq!(parse_leader("c-"), Ok(Leader::Mods(ModFlags::CTRL)));
    assert_eq!(
        parse_leader("A-S-"),
        Ok(Leader::Mods(ModFlags::ALT | ModFlags::SHIFT))
    );
    assert_eq!(parse_leader("D-"), Ok(Leader::Mods(ModFlags::SUPER)));
}

#[test]
fn anything_else_is_a_chord_leader() {
    assert_eq!(
        parse_leader("<Space>"),
        Ok(Leader::Chord(build_key_chord(
            ModFlags::NONE,
            Key::Named(NamedKey::Space)
        )))
    );
    assert_eq!(
        parse_leader(","),
        Ok(Leader::Chord(build_key_chord(
            ModFlags::NONE,
            Key::Char(','),
        )))
    );
    assert_eq!(
        parse_leader("<C-p>"),
        Ok(Leader::Chord(build_key_chord(
            ModFlags::CTRL,
            Key::Char('p'),
        )))
    );
}

#[test]
fn a_lone_dash_is_the_dash_key_not_an_empty_modifier_run() {
    assert_eq!(
        parse_leader("-"),
        Ok(Leader::Chord(build_key_chord(
            ModFlags::NONE,
            Key::Char('-'),
        )))
    );
}

#[test]
fn a_bad_modifier_run_reports_the_modifier_rather_than_the_chord() {
    assert_eq!(
        parse_leader("x-"),
        Err(KeyParseError {
            key_token: "x-".to_string(),
            error_kind: KeyParseErrorKind::UnknownModifier { modifier: 'x' },
        })
    );
}

#[test]
fn the_empty_leader_is_refused() {
    assert_eq!(
        parse_leader(""),
        Err(KeyParseError {
            key_token: String::new(),
            error_kind: KeyParseErrorKind::Empty,
        })
    );
}

#[test]
fn the_default_leader_is_control() {
    assert_eq!(Leader::default(), Leader::Mods(ModFlags::CTRL));
    assert_eq!(Leader::default(), parse_leader("C-").unwrap());
}

#[test]
fn a_leader_renders_the_text_it_was_parsed_from() {
    assert_eq!(Leader::Mods(ModFlags::CTRL).to_string(), "C-");
    assert_eq!(
        Leader::Mods(ModFlags::ALT | ModFlags::SHIFT).to_string(),
        "A-S-"
    );
    assert_eq!(
        Leader::Chord(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space),)).to_string(),
        "<Space>"
    );
    assert_eq!(
        Leader::Chord(build_key_chord(ModFlags::NONE, Key::Char(','),)).to_string(),
        ","
    );
}

#[test]
fn a_leader_parses_back_from_the_text_it_renders() {
    let leader_cases = [
        Leader::Mods(ModFlags::CTRL),
        Leader::Mods(ModFlags::ALT | ModFlags::SHIFT),
        Leader::Chord(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::Space))),
        Leader::Chord(build_key_chord(ModFlags::NONE, Key::Char(','))),
        Leader::Chord(build_key_chord(ModFlags::CTRL, Key::Char('p'))),
    ];
    for expected_leader in leader_cases {
        let leader_text = expected_leader.to_string();
        assert_eq!(
            parse_leader(&leader_text),
            Ok(expected_leader),
            "round trip via {leader_text:?}"
        );
    }
}

// -- error classification -------------------------------------------------

#[test]
fn a_key_parse_error_is_a_recoverable_config_error() {
    let parse_error = parse_chord("Ctrl-g").unwrap_err();
    assert_eq!(parse_error.category(), DomainCategory::Config);
    assert_eq!(parse_error.get_severity(), Severity::Recoverable);
}

#[test]
fn an_error_names_the_token_and_the_reason() {
    assert_eq!(
        parse_chord("<S-1>").unwrap_err().to_string(),
        "invalid key `<S-1>`: `S-` applies to letters only, not `1`; write the shifted character itself"
    );
    assert_eq!(
        parse_chord("Ctrl-g").unwrap_err().to_string(),
        "invalid key `Ctrl-g`: a multi-character key must be bracketed, as in `<Tab>`"
    );
    assert_eq!(
        parse_chord("<leader>").unwrap_err().to_string(),
        "invalid key `<leader>`: `<leader>` stands for a prefix, not a chord"
    );
    assert_eq!(
        parse_chord("\t").unwrap_err().to_string(),
        "invalid key `\t`: the character '\\t' is written by its key name, such as `<Space>` or `<Tab>`"
    );
}

// -- adversarial: hostile bytes and unicode --------------------------------

#[test]
fn a_nul_byte_is_refused_as_a_control_character() {
    // A literal nul reaching the parser is a control character, so it is
    // refused the same way a raw tab is, never panicking.
    assert_eq!(
        parse_chord("\0"),
        Err(KeyParseError {
            key_token: "\0".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\0',
            },
        })
    );
    // Inside a modified bracket the fold is the same.
    assert_eq!(
        parse_chord("<C-\0>"),
        Err(KeyParseError {
            key_token: "<C-\0>".to_string(),
            error_kind: KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\0',
            },
        })
    );
}

#[test]
fn a_non_ascii_letter_is_a_bare_chord_and_may_carry_modifiers() {
    // A multi-byte printable letter is one chord; it is already lowercase, so
    // no Shift bit is folded in.
    assert_eq!(
        parse_chord("é"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('é')))
    );
    assert_eq!(
        parse_chord("<C-é>"),
        Ok(build_key_chord(ModFlags::CTRL, Key::Char('é')))
    );
}

#[test]
fn a_byte_order_mark_is_a_bare_chord_not_whitespace() {
    // U+FEFF is neither control nor whitespace in Rust, so it parses as an
    // ordinary one-character key rather than being refused.
    assert_eq!(
        parse_chord("\u{feff}"),
        Ok(build_key_chord(ModFlags::NONE, Key::Char('\u{feff}')))
    );
}

#[test]
fn an_absurdly_long_bare_token_is_the_unbracketed_multichar_error() {
    // A thousand characters with no brackets is still just "more than one bare
    // character", reported once, not a panic or a hang.
    let key_token = "a".repeat(1000);
    assert_eq!(
        parse_chord(&key_token),
        Err(KeyParseError {
            key_token: key_token.clone(),
            error_kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn a_third_modifier_repeat_is_caught_in_any_order() {
    // The duplicate is flagged at the second occurrence of the same letter,
    // whichever modifiers sit between them.
    assert_eq!(
        parse_chord("<C-A-C-x>"),
        Err(KeyParseError {
            key_token: "<C-A-C-x>".to_string(),
            error_kind: KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
        })
    );
    // A repeat that lands after all four distinct modifiers is still caught.
    assert_eq!(
        parse_chord("<C-A-S-D-A-x>"),
        Err(KeyParseError {
            key_token: "<C-A-S-D-A-x>".to_string(),
            error_kind: KeyParseErrorKind::DuplicateModifier { modifier: 'A' },
        })
    );
}

#[test]
fn a_two_space_bracket_is_read_as_an_unknown_named_key() {
    // A single space inside brackets is one raw key (refused as whitespace),
    // but two characters make it a multi-character name, so it takes the
    // named-key table path and is refused there as unknown — the two inner
    // spaces are the reported name.
    assert_eq!(
        parse_chord("<  >"),
        Err(KeyParseError {
            key_token: "<  >".to_string(),
            error_kind: KeyParseErrorKind::UnknownNamedKey {
                key_name: "  ".to_string(),
            },
        })
    );
}

#[test]
fn every_modifier_and_no_key_is_refused() {
    assert_eq!(
        parse_chord("<C-A-S-D->"),
        Err(KeyParseError {
            key_token: "<C-A-S-D->".to_string(),
            error_kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn a_dash_where_a_modifier_letter_belongs_is_an_unknown_modifier() {
    // `<-->` opens with the pair `--`, and `-` is not one of `C`, `A`, `S`, `D`.
    assert_eq!(
        parse_chord("<-->"),
        Err(KeyParseError {
            key_token: "<-->".to_string(),
            error_kind: KeyParseErrorKind::UnknownModifier { modifier: '-' },
        })
    );
}

#[test]
fn a_function_key_number_with_leading_zeros_names_the_same_key() {
    assert_eq!(
        parse_chord("<F01>"),
        Ok(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(1))))
    );
    assert_eq!(
        parse_chord("<F0024>"),
        Ok(build_key_chord(ModFlags::NONE, Key::Named(NamedKey::F(24))))
    );
}

#[test]
fn a_non_ascii_capital_folds_into_the_shift_bit() {
    assert_eq!(
        parse_chord("È"),
        Ok(build_key_chord(ModFlags::SHIFT, Key::Char('è')))
    );
    assert_eq!(
        parse_chord("<A-È>"),
        Ok(build_key_chord(
            ModFlags::ALT | ModFlags::SHIFT,
            Key::Char('è'),
        ))
    );
}

#[test]
fn a_repeated_modifier_in_a_leader_run_is_refused() {
    assert_eq!(
        parse_leader("C-C-"),
        Err(KeyParseError {
            key_token: "C-C-".to_string(),
            error_kind: KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
        })
    );
}

#[test]
fn a_leader_that_never_closes_its_bracket_is_refused() {
    assert_eq!(
        parse_leader("<C-p"),
        Err(KeyParseError {
            key_token: "<C-p".to_string(),
            error_kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn a_lone_f_in_brackets_is_the_shifted_letter_not_a_function_key() {
    // The function-key path needs at least one digit after the `F`, so a bare
    // `F` takes the single-character path and folds into the Shift bit.
    assert_eq!(
        parse_chord("<F>"),
        Ok(build_key_chord(ModFlags::SHIFT, Key::Char('f')))
    );
}

#[test]
fn a_bracketed_digit_run_with_no_f_is_an_unknown_key_name() {
    assert_eq!(
        parse_chord("<12>"),
        Err(KeyParseError {
            key_token: "<12>".to_string(),
            error_kind: KeyParseErrorKind::UnknownNamedKey {
                key_name: "12".to_string(),
            },
        })
    );
}

#[test]
fn an_explicit_shift_on_a_non_ascii_capital_folds_to_one_shift_bit() {
    assert_eq!(
        parse_chord("<S-È>"),
        Ok(build_key_chord(ModFlags::SHIFT, Key::Char('è')))
    );
}

#[test]
fn a_capital_leader_folds_into_the_shift_bit() {
    assert_eq!(
        parse_leader("N"),
        Ok(Leader::Chord(build_key_chord(
            ModFlags::SHIFT,
            Key::Char('n'),
        )))
    );
}

#[test]
fn a_bracketed_leader_carries_the_chord_parsers_rejection_unchanged() {
    assert_eq!(
        parse_leader("<S-1>"),
        parse_chord("<S-1>").map(Leader::Chord)
    );
    assert_eq!(
        parse_leader("<S-1>"),
        Err(KeyParseError {
            key_token: "<S-1>".to_string(),
            error_kind: KeyParseErrorKind::ShiftOnNonLetter { key_character: '1' },
        })
    );
}

#[test]
fn every_error_kind_renders_its_own_message() {
    let error_cases = [
        (KeyParseErrorKind::Empty, "empty key"),
        (KeyParseErrorKind::UnclosedBracket, "missing closing `>`"),
        (KeyParseErrorKind::MissingKey, "no key after the modifiers"),
        (
            KeyParseErrorKind::UnknownModifier { modifier: 'x' },
            "unknown modifier `x-`; use `C-`, `A-`, `S-`, or `D-`",
        ),
        (
            KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
            "modifier `C-` given twice",
        ),
        (
            KeyParseErrorKind::UnknownNamedKey {
                key_name: "Nope".to_string(),
            },
            "unknown key name `Nope`",
        ),
        (
            KeyParseErrorKind::UnbracketedMultiChar,
            "a multi-character key must be bracketed, as in `<Tab>`",
        ),
        (
            KeyParseErrorKind::ShiftOnNonLetter { key_character: '1' },
            "`S-` applies to letters only, not `1`; write the shifted character itself",
        ),
        (
            KeyParseErrorKind::FunctionKeyOutOfRange {
                function_key_number_text: "25".to_string(),
            },
            "function keys run F1 to F24, got `F25`",
        ),
        (
            KeyParseErrorKind::RawWhitespaceOrControl {
                key_character: '\t',
            },
            "the character '\\t' is written by its key name, such as `<Space>` or `<Tab>`",
        ),
        (
            KeyParseErrorKind::LeaderNotAChord,
            "`<leader>` stands for a prefix, not a chord",
        ),
        (
            KeyParseErrorKind::LeaderNotFirst,
            "`<leader>` may only open a sequence",
        ),
        (
            KeyParseErrorKind::DanglingLeaderMods,
            "the leader's modifiers need a key after them",
        ),
        (
            KeyParseErrorKind::SequenceTooLong {
                chord_count: 5,
                max_chord_depth: 4,
            },
            "the sequence has 5 chords; the cap is 4",
        ),
    ];
    for (error_kind, expected_message) in error_cases {
        assert_eq!(error_kind.to_string(), expected_message);
    }
}
