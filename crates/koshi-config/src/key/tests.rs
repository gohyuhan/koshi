//! Tests for the chord and leader parsers: the accepted grammar, the case fold
//! into the Shift bit, every rejection, and the round trip through the canonical
//! text form a chord renders.

use super::*;

/// Builds the chord a test expects, keeping the assertions readable.
fn chord(mods: ModFlags, key: Key) -> KeyChord {
    KeyChord::new(mods, key)
}

// -- accepted chords ------------------------------------------------------

#[test]
fn a_bare_character_is_an_unmodified_chord() {
    assert_eq!(parse_chord("n"), Ok(chord(ModFlags::NONE, Key::Char('n'))));
    assert_eq!(parse_chord("!"), Ok(chord(ModFlags::NONE, Key::Char('!'))));
    assert_eq!(parse_chord("-"), Ok(chord(ModFlags::NONE, Key::Char('-'))));
    assert_eq!(parse_chord(">"), Ok(chord(ModFlags::NONE, Key::Char('>'))));
    assert_eq!(parse_chord(","), Ok(chord(ModFlags::NONE, Key::Char(','))));
}

#[test]
fn a_bare_capital_folds_into_the_shift_bit() {
    assert_eq!(parse_chord("N"), Ok(chord(ModFlags::SHIFT, Key::Char('n'))));
}

#[test]
fn each_modifier_letter_sets_its_bit() {
    assert_eq!(
        parse_chord("<C-p>"),
        Ok(chord(ModFlags::CTRL, Key::Char('p')))
    );
    assert_eq!(
        parse_chord("<A-p>"),
        Ok(chord(ModFlags::ALT, Key::Char('p')))
    );
    assert_eq!(
        parse_chord("<S-p>"),
        Ok(chord(ModFlags::SHIFT, Key::Char('p')))
    );
    assert_eq!(
        parse_chord("<D-p>"),
        Ok(chord(ModFlags::SUPER, Key::Char('p')))
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
    let expected = chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'));
    assert_eq!(parse_chord("<A-S-h>"), Ok(expected));
    assert_eq!(parse_chord("<S-A-h>"), Ok(expected));

    assert_eq!(
        parse_chord("<C-A-S-D-x>"),
        Ok(chord(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Char('x')
        ))
    );
}

#[test]
fn a_capital_and_an_explicit_shift_name_the_same_chord() {
    let expected = chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h'));
    assert_eq!(parse_chord("<A-H>"), Ok(expected));
    assert_eq!(parse_chord("<A-S-h>"), Ok(expected));
    assert_eq!(parse_chord("<A-S-H>"), Ok(expected));
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
    for (text, key) in cases {
        assert_eq!(
            parse_chord(text),
            Ok(chord(ModFlags::NONE, Key::Named(key))),
            "parsing {text}"
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
        Ok(chord(ModFlags::NONE, Key::Named(NamedKey::F(1))))
    );
    assert_eq!(
        parse_chord("<F24>"),
        Ok(chord(ModFlags::NONE, Key::Named(NamedKey::F(24))))
    );
}

#[test]
fn a_named_key_may_carry_modifiers() {
    assert_eq!(
        parse_chord("<S-Tab>"),
        Ok(chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab)))
    );
    assert_eq!(
        parse_chord("<C-A-F5>"),
        Ok(chord(
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
        Ok(chord(ModFlags::NONE, Key::Named(NamedKey::Space)))
    );
    // `C--` is Control plus the `-` key: the run eats `C-`, the rest is the key.
    assert_eq!(
        parse_chord("<C-->"),
        Ok(chord(ModFlags::CTRL, Key::Char('-')))
    );
}

#[test]
fn the_angle_brackets_themselves_can_be_bound() {
    assert_eq!(
        parse_chord("<<>"),
        Ok(chord(ModFlags::NONE, Key::Char('<')))
    );
    assert_eq!(
        parse_chord("<C-<>"),
        Ok(chord(ModFlags::CTRL, Key::Char('<')))
    );
    assert_eq!(
        parse_chord("<C->>"),
        Ok(chord(ModFlags::CTRL, Key::Char('>')))
    );
}

#[test]
fn a_capital_with_no_single_character_lowercase_stands_as_written() {
    // U+0130 lowercases to two characters, so no fold and no Shift bit.
    assert_eq!(
        parse_chord("\u{0130}"),
        Ok(chord(ModFlags::NONE, Key::Char('\u{0130}')))
    );
}

// -- rejected chords ------------------------------------------------------

#[test]
fn the_empty_token_is_refused() {
    assert_eq!(
        parse_chord(""),
        Err(KeyParseError {
            token: String::new(),
            kind: KeyParseErrorKind::Empty,
        })
    );
}

#[test]
fn an_unclosed_bracket_is_refused() {
    assert_eq!(
        parse_chord("<C-p"),
        Err(KeyParseError {
            token: "<C-p".to_string(),
            kind: KeyParseErrorKind::UnclosedBracket,
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
            token: "<C-p>n".to_string(),
            kind: KeyParseErrorKind::UnclosedBracket,
        })
    );
}

#[test]
fn modifiers_with_no_key_are_refused() {
    assert_eq!(
        parse_chord("<>"),
        Err(KeyParseError {
            token: "<>".to_string(),
            kind: KeyParseErrorKind::MissingKey,
        })
    );
    assert_eq!(
        parse_chord("<C->"),
        Err(KeyParseError {
            token: "<C->".to_string(),
            kind: KeyParseErrorKind::MissingKey,
        })
    );
    assert_eq!(
        parse_chord("<C-S->"),
        Err(KeyParseError {
            token: "<C-S->".to_string(),
            kind: KeyParseErrorKind::MissingKey,
        })
    );
}

#[test]
fn an_unknown_modifier_letter_is_refused() {
    assert_eq!(
        parse_chord("<x-a>"),
        Err(KeyParseError {
            token: "<x-a>".to_string(),
            kind: KeyParseErrorKind::UnknownModifier { modifier: 'x' },
        })
    );
}

#[test]
fn a_repeated_modifier_is_refused() {
    assert_eq!(
        parse_chord("<C-C-a>"),
        Err(KeyParseError {
            token: "<C-C-a>".to_string(),
            kind: KeyParseErrorKind::DuplicateModifier { modifier: 'C' },
        })
    );
    // The repeat is caught across cases, since the letters are folded.
    assert_eq!(
        parse_chord("<C-c-a>"),
        Err(KeyParseError {
            token: "<C-c-a>".to_string(),
            kind: KeyParseErrorKind::DuplicateModifier { modifier: 'c' },
        })
    );
}

#[test]
fn an_unknown_key_name_is_refused() {
    assert_eq!(
        parse_chord("<Nope>"),
        Err(KeyParseError {
            token: "<Nope>".to_string(),
            kind: KeyParseErrorKind::UnknownNamedKey {
                name: "Nope".to_string()
            },
        })
    );
    // `Enter` is not the accepted spelling; `CR` is.
    assert_eq!(
        parse_chord("<Enter>"),
        Err(KeyParseError {
            token: "<Enter>".to_string(),
            kind: KeyParseErrorKind::UnknownNamedKey {
                name: "Enter".to_string()
            },
        })
    );
}

#[test]
fn the_dash_form_is_not_the_grammar() {
    assert_eq!(
        parse_chord("Ctrl-g"),
        Err(KeyParseError {
            token: "Ctrl-g".to_string(),
            kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
    assert_eq!(
        parse_chord("Tab"),
        Err(KeyParseError {
            token: "Tab".to_string(),
            kind: KeyParseErrorKind::UnbracketedMultiChar,
        })
    );
}

#[test]
fn shift_on_a_non_letter_is_refused() {
    assert_eq!(
        parse_chord("<S-1>"),
        Err(KeyParseError {
            token: "<S-1>".to_string(),
            kind: KeyParseErrorKind::ShiftOnNonLetter { ch: '1' },
        })
    );
    assert_eq!(
        parse_chord("<S-->"),
        Err(KeyParseError {
            token: "<S-->".to_string(),
            kind: KeyParseErrorKind::ShiftOnNonLetter { ch: '-' },
        })
    );
}

#[test]
fn a_function_key_outside_the_range_is_refused() {
    assert_eq!(
        parse_chord("<F0>"),
        Err(KeyParseError {
            token: "<F0>".to_string(),
            kind: KeyParseErrorKind::FunctionKeyOutOfRange { n: "0".to_string() },
        })
    );
    assert_eq!(
        parse_chord("<F25>"),
        Err(KeyParseError {
            token: "<F25>".to_string(),
            kind: KeyParseErrorKind::FunctionKeyOutOfRange {
                n: "25".to_string()
            },
        })
    );
    // Wider than a byte, and still reported as out of range rather than unknown.
    assert_eq!(
        parse_chord("<F1000>"),
        Err(KeyParseError {
            token: "<F1000>".to_string(),
            kind: KeyParseErrorKind::FunctionKeyOutOfRange {
                n: "1000".to_string()
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
            token: "\t".to_string(),
            kind: KeyParseErrorKind::RawWhitespaceOrControl { ch: '\t' },
        })
    );
    assert_eq!(
        parse_chord(" "),
        Err(KeyParseError {
            token: " ".to_string(),
            kind: KeyParseErrorKind::RawWhitespaceOrControl { ch: ' ' },
        })
    );
    assert_eq!(
        parse_chord("\r"),
        Err(KeyParseError {
            token: "\r".to_string(),
            kind: KeyParseErrorKind::RawWhitespaceOrControl { ch: '\r' },
        })
    );
    assert_eq!(
        parse_chord("\u{1b}"),
        Err(KeyParseError {
            token: "\u{1b}".to_string(),
            kind: KeyParseErrorKind::RawWhitespaceOrControl { ch: '\u{1b}' },
        })
    );
    // The bracketed and modified positions go through the same fold.
    assert_eq!(
        parse_chord("< >"),
        Err(KeyParseError {
            token: "< >".to_string(),
            kind: KeyParseErrorKind::RawWhitespaceOrControl { ch: ' ' },
        })
    );
    assert_eq!(
        parse_chord("<C-\t>"),
        Err(KeyParseError {
            token: "<C-\t>".to_string(),
            kind: KeyParseErrorKind::RawWhitespaceOrControl { ch: '\t' },
        })
    );
}

#[test]
fn leader_is_not_a_chord() {
    assert_eq!(
        parse_chord("<leader>"),
        Err(KeyParseError {
            token: "<leader>".to_string(),
            kind: KeyParseErrorKind::LeaderNotAChord,
        })
    );
    assert_eq!(
        parse_chord("<Leader>"),
        Err(KeyParseError {
            token: "<Leader>".to_string(),
            kind: KeyParseErrorKind::LeaderNotAChord,
        })
    );
}

// -- round trip -----------------------------------------------------------

#[test]
fn every_chord_parses_back_from_the_text_it_renders() {
    let cases = [
        chord(ModFlags::NONE, Key::Char('n')),
        chord(ModFlags::NONE, Key::Char('!')),
        chord(ModFlags::NONE, Key::Char('-')),
        chord(ModFlags::NONE, Key::Char('<')),
        chord(ModFlags::NONE, Key::Char('>')),
        chord(ModFlags::SHIFT, Key::Char('n')),
        chord(ModFlags::CTRL, Key::Char('p')),
        chord(ModFlags::ALT, Key::Char('n')),
        chord(ModFlags::SUPER, Key::Char('x')),
        chord(ModFlags::CTRL, Key::Char('-')),
        chord(ModFlags::CTRL, Key::Char('<')),
        chord(ModFlags::ALT | ModFlags::SHIFT, Key::Char('h')),
        chord(
            ModFlags::CTRL | ModFlags::ALT | ModFlags::SHIFT | ModFlags::SUPER,
            Key::Char('x'),
        ),
        chord(ModFlags::NONE, Key::Named(NamedKey::Enter)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Backspace)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Space)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Insert)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Delete)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Home)),
        chord(ModFlags::NONE, Key::Named(NamedKey::End)),
        chord(ModFlags::NONE, Key::Named(NamedKey::PageUp)),
        chord(ModFlags::NONE, Key::Named(NamedKey::PageDown)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Left)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Right)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Up)),
        chord(ModFlags::NONE, Key::Named(NamedKey::Down)),
        chord(ModFlags::NONE, Key::Named(NamedKey::F(1))),
        chord(ModFlags::NONE, Key::Named(NamedKey::F(24))),
        chord(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
        chord(ModFlags::CTRL | ModFlags::ALT, Key::Named(NamedKey::F(5))),
    ];
    for expected in cases {
        let text = expected.to_string();
        assert_eq!(parse_chord(&text), Ok(expected), "round trip via {text:?}");
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
        Ok(Leader::Chord(chord(
            ModFlags::NONE,
            Key::Named(NamedKey::Space)
        )))
    );
    assert_eq!(
        parse_leader(","),
        Ok(Leader::Chord(chord(ModFlags::NONE, Key::Char(','))))
    );
    assert_eq!(
        parse_leader("<C-p>"),
        Ok(Leader::Chord(chord(ModFlags::CTRL, Key::Char('p'))))
    );
}

#[test]
fn a_lone_dash_is_the_dash_key_not_an_empty_modifier_run() {
    assert_eq!(
        parse_leader("-"),
        Ok(Leader::Chord(chord(ModFlags::NONE, Key::Char('-'))))
    );
}

#[test]
fn a_bad_modifier_run_reports_the_modifier_rather_than_the_chord() {
    assert_eq!(
        parse_leader("x-"),
        Err(KeyParseError {
            token: "x-".to_string(),
            kind: KeyParseErrorKind::UnknownModifier { modifier: 'x' },
        })
    );
}

#[test]
fn the_empty_leader_is_refused() {
    assert_eq!(
        parse_leader(""),
        Err(KeyParseError {
            token: String::new(),
            kind: KeyParseErrorKind::Empty,
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
        Leader::Chord(chord(ModFlags::NONE, Key::Named(NamedKey::Space))).to_string(),
        "<Space>"
    );
    assert_eq!(
        Leader::Chord(chord(ModFlags::NONE, Key::Char(','))).to_string(),
        ","
    );
}

#[test]
fn a_leader_parses_back_from_the_text_it_renders() {
    let cases = [
        Leader::Mods(ModFlags::CTRL),
        Leader::Mods(ModFlags::ALT | ModFlags::SHIFT),
        Leader::Chord(chord(ModFlags::NONE, Key::Named(NamedKey::Space))),
        Leader::Chord(chord(ModFlags::NONE, Key::Char(','))),
        Leader::Chord(chord(ModFlags::CTRL, Key::Char('p'))),
    ];
    for expected in cases {
        let text = expected.to_string();
        assert_eq!(parse_leader(&text), Ok(expected), "round trip via {text:?}");
    }
}

// -- error classification -------------------------------------------------

#[test]
fn a_key_parse_error_is_a_recoverable_config_error() {
    let e = parse_chord("Ctrl-g").unwrap_err();
    assert_eq!(e.category(), DomainCategory::Config);
    assert_eq!(e.severity(), Severity::Recoverable);
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
