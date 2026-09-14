//! Parses multi-chord key sequences from config text.
//!
//! A binding's key text names one or more chords pressed in order:
//! `"<C-p> n"` is Ctrl+p then `n`, `"gg"` is `g` twice. A token is an
//! angle-bracketed run through its closing `>`, or a single bare character;
//! whitespace separates tokens and carries no meaning of its own. A bare
//! word in the dash form (`Ctrl-g`) is refused, the same spelling rule the
//! chord grammar enforces: a modified key is written bracketed, `<C-g>`.
//!
//! `<leader>` may stand as the first token only. A [`Leader::Chord`] becomes
//! the opening chord; a [`Leader::Mods`] run merges its modifiers into the
//! chord that follows, so with the default `C-` leader, `<leader>wq` is
//! Ctrl+w then `q`.

use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};

use crate::key::{create_key_parse_error, parse_chord, KeyParseError, KeyParseErrorKind, Leader};

/// True when `word_fragment` holds a `-` between two alphanumeric characters.
fn holds_dash_form(word_fragment: &str) -> bool {
    let fragment_characters: Vec<char> = word_fragment.chars().collect();
    fragment_characters.windows(3).any(|character_window| {
        character_window[0].is_alphanumeric()
            && character_window[1] == '-'
            && character_window[2].is_alphanumeric()
    })
}

/// True when `word` is written in the dash form the grammar rejects, such as
/// `Ctrl-g`: a `-` between two alphanumeric characters, outside every
/// angle-bracketed run. A bare `-` chord next to others stays legal when
/// whitespace-separated (`a - b`) or at a word's edge (`g-`).
///
/// Each `<…>` run is stepped over, so `<C-p>` is not a dash form and
/// `<leader>Ctrl-g` is. A `<` that never closes ends the walk: the unclosed
/// bracket is reported by [`split_token`] instead.
fn is_dash_form(word_text: &str) -> bool {
    let mut remaining_word_text = word_text;
    while let Some(open_bracket_index) = remaining_word_text.find('<') {
        if holds_dash_form(&remaining_word_text[..open_bracket_index]) {
            return true;
        }
        let Some(close_bracket_relative_index) =
            remaining_word_text[open_bracket_index..].find('>')
        else {
            return false;
        };
        remaining_word_text =
            &remaining_word_text[open_bracket_index + close_bracket_relative_index + 1..];
    }
    holds_dash_form(remaining_word_text)
}

/// Splits the next key token off `remaining_sequence_text`: a `<...>` run
/// through its first closing `>`, or a single bare character. Returns the key
/// token and the remaining sequence text.
///
/// `remaining_sequence_text` must not be empty; an empty value panics. A `<`
/// with no `>` after it returns [`KeyParseErrorKind::UnclosedBracket`] carrying
/// the remaining sequence text as the key token.
fn split_key_token(remaining_sequence_text: &str) -> Result<(&str, &str), KeyParseError> {
    if let Some(bracketed_body) = remaining_sequence_text.strip_prefix('<') {
        match bracketed_body.find('>') {
            // `<` plus the inner run plus `>`.
            Some(closing_bracket_relative_index) => {
                Ok(remaining_sequence_text.split_at(closing_bracket_relative_index + 2))
            }
            None => Err(create_key_parse_error(
                remaining_sequence_text,
                KeyParseErrorKind::UnclosedBracket,
            )),
        }
    } else {
        let first_key_character = remaining_sequence_text
            .chars()
            .next()
            .expect("remaining_sequence_text is not empty");
        Ok(remaining_sequence_text.split_at(first_key_character.len_utf8()))
    }
}

/// True when `key_token` is the `<leader>` placeholder, matched
/// case-insensitively.
fn is_leader_token(key_token: &str) -> bool {
    key_token
        .strip_prefix('<')
        .and_then(|token_body| token_body.strip_suffix('>'))
        .is_some_and(|token_body| token_body.eq_ignore_ascii_case("leader"))
}

/// Merges a modifier-run leader into the chord that follows it. Rejects a
/// merge that lands `SHIFT` on a [`Key::Char`] that is not lowercase; a named
/// key takes `SHIFT` unchanged.
fn merge_leader_modifier_flags(
    key_token: &str,
    leader_modifier_flags: ModFlags,
    key_chord: KeyChord,
) -> Result<KeyChord, KeyParseError> {
    let merged_modifier_flags = key_chord.modifier_flags.union(leader_modifier_flags);
    if let Key::Char(key_character) = key_chord.key {
        if merged_modifier_flags.has_all_modifiers(ModFlags::SHIFT) && !key_character.is_lowercase()
        {
            return Err(create_key_parse_error(
                key_token,
                KeyParseErrorKind::ShiftOnNonLetter { key_character },
            ));
        }
    }
    Ok(KeyChord::from_parts(merged_modifier_flags, key_chord.key))
}

/// Parses a whole key sequence from its config text form.
///
/// Each token parses with [`parse_chord`]; a leading `<leader>` substitutes
/// the configured `leader`. The finished sequence holds at most
/// `max_chord_depth` chords, counted after the leader substitutes — a
/// modifier-run leader adds no chord of its own, a chord leader adds one.
///
/// # Errors
/// Returns a [`KeyParseError`] carrying the failing token: an empty sequence,
/// a bare word in the dash form (`Ctrl-g` — modified keys are bracketed, as
/// in `<C-g>`), a `<` that never closes, any token [`parse_chord`] rejects,
/// `<leader>` past the first position, a modifier-run leader with no chord
/// after it, a merge landing `S-` on a non-letter character, or more chords
/// than `max_chord_depth`.
pub fn parse_sequence(
    sequence_text: &str,
    leader: Leader,
    max_chord_depth: u8,
) -> Result<KeySequence, KeyParseError> {
    for sequence_word in sequence_text.split_whitespace() {
        if is_dash_form(sequence_word) {
            return Err(create_key_parse_error(
                sequence_word,
                KeyParseErrorKind::UnbracketedMultiChar,
            ));
        }
    }

    let mut chords: Vec<KeyChord> = Vec::new();
    // Modifiers from a modifier-run leader, waiting to merge into the next chord.
    let mut pending_leader_modifier_flags = ModFlags::NONE;
    let mut is_first_token = true;
    let mut remaining_sequence_text = sequence_text.trim_start();

    while !remaining_sequence_text.is_empty() {
        let (mut key_token, mut remaining_after_token) = split_key_token(remaining_sequence_text)?;

        if is_leader_token(key_token) {
            if !is_first_token {
                return Err(create_key_parse_error(
                    key_token,
                    KeyParseErrorKind::LeaderNotFirst,
                ));
            }
            match leader {
                Leader::Chord(leader_chord) => chords.push(leader_chord),
                Leader::Mods(leader_modifier_flags) => {
                    pending_leader_modifier_flags = leader_modifier_flags;
                }
            }
        } else {
            let mut key_chord = match parse_chord(key_token) {
                Ok(key_chord) => key_chord,
                // `<C->>`: the key is `>` itself, and the first `>` closes
                // nothing. Extend the token through the next `>` and parse
                // again. `<>` names no modifier run, so `<>>` is not extended.
                Err(parse_error)
                    if matches!(parse_error.error_kind, KeyParseErrorKind::MissingKey)
                        && key_token != "<>"
                        && remaining_after_token.starts_with('>') =>
                {
                    key_token = &remaining_sequence_text[..key_token.len() + 1];
                    remaining_after_token = &remaining_after_token[1..];
                    parse_chord(key_token)?
                }
                Err(parse_error) => return Err(parse_error),
            };
            if !pending_leader_modifier_flags.is_empty() {
                key_chord = merge_leader_modifier_flags(
                    key_token,
                    pending_leader_modifier_flags,
                    key_chord,
                )?;
                pending_leader_modifier_flags = ModFlags::NONE;
            }
            chords.push(key_chord);
        }

        is_first_token = false;
        remaining_sequence_text = remaining_after_token.trim_start();
    }

    if !pending_leader_modifier_flags.is_empty() {
        // The whole sequence was `<leader>` with a modifier-run leader.
        return Err(create_key_parse_error(
            sequence_text,
            KeyParseErrorKind::DanglingLeaderMods,
        ));
    }
    if chords.is_empty() {
        return Err(create_key_parse_error(
            sequence_text,
            KeyParseErrorKind::Empty,
        ));
    }
    let chord_count = chords.len();
    if chord_count > usize::from(max_chord_depth) {
        return Err(create_key_parse_error(
            sequence_text,
            KeyParseErrorKind::SequenceTooLong {
                chord_count,
                max_chord_depth,
            },
        ));
    }

    let first_chord = chords.remove(0);
    Ok(KeySequence::from_first_and_rest(first_chord, chords))
}

#[cfg(test)]
mod tests;
