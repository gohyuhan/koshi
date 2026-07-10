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

use crate::key::{err, parse_chord, KeyParseError, KeyParseErrorKind, Leader};

/// True when `word` is written in the dash form the grammar rejects, such as
/// `Ctrl-g`: a word with no angle bracket holding a `-` between two
/// alphanumeric characters. A bare `-` chord next to others stays legal when
/// whitespace-separated (`a - b`) or at a word's edge (`g-`).
fn is_dash_form(word: &str) -> bool {
    if word.contains('<') {
        return false;
    }
    let chars: Vec<char> = word.chars().collect();
    chars
        .windows(3)
        .any(|w| w[0].is_alphanumeric() && w[1] == '-' && w[2].is_alphanumeric())
}

/// Splits the next token off `rest`: a `<...>` run through its first closing
/// `>`, or a single bare character. `rest` must not be empty.
fn split_token(rest: &str) -> Result<(&str, &str), KeyParseError> {
    if let Some(after_open) = rest.strip_prefix('<') {
        match after_open.find('>') {
            // `<` plus the inner run plus `>`.
            Some(i) => Ok(rest.split_at(i + 2)),
            None => Err(err(rest, KeyParseErrorKind::UnclosedBracket)),
        }
    } else {
        let c = rest.chars().next().expect("rest is not empty");
        Ok(rest.split_at(c.len_utf8()))
    }
}

/// True when `token` is the `<leader>` placeholder, matched case-insensitively.
fn is_leader_token(token: &str) -> bool {
    token
        .strip_prefix('<')
        .and_then(|t| t.strip_suffix('>'))
        .is_some_and(|inner| inner.eq_ignore_ascii_case("leader"))
}

/// Merges a modifier-run leader into the chord that follows it. Rejects a
/// merge that lands `SHIFT` on a character with no capital form, the same
/// canonical-form rule the chord parser enforces.
fn merge_leader_mods(
    token: &str,
    leader_mods: ModFlags,
    chord: KeyChord,
) -> Result<KeyChord, KeyParseError> {
    let mods = chord.mods.union(leader_mods);
    if let Key::Char(c) = chord.key {
        if mods.contains(ModFlags::SHIFT) && !c.is_lowercase() {
            return Err(err(token, KeyParseErrorKind::ShiftOnNonLetter { ch: c }));
        }
    }
    Ok(KeyChord::new(mods, chord.key))
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
/// in `<C-g>`), any token [`parse_chord`] rejects, `<leader>` past the first
/// position, a modifier-run leader with no chord after it, a merge landing
/// `S-` on a non-letter character, or more chords than `max_chord_depth`.
pub fn parse_sequence(
    s: &str,
    leader: Leader,
    max_chord_depth: u8,
) -> Result<KeySequence, KeyParseError> {
    for word in s.split_whitespace() {
        if is_dash_form(word) {
            return Err(err(word, KeyParseErrorKind::UnbracketedMultiChar));
        }
    }

    let mut chords: Vec<KeyChord> = Vec::new();
    // Modifiers from a modifier-run leader, waiting to merge into the next chord.
    let mut pending_mods = ModFlags::NONE;
    let mut first_token = true;
    let mut rest = s.trim_start();

    while !rest.is_empty() {
        let (mut token, mut after) = split_token(rest)?;

        if is_leader_token(token) {
            if !first_token {
                return Err(err(token, KeyParseErrorKind::LeaderNotFirst));
            }
            match leader {
                Leader::Chord(chord) => chords.push(chord),
                Leader::Mods(mods) => pending_mods = mods,
            }
        } else {
            let mut chord = match parse_chord(token) {
                Ok(chord) => chord,
                // `<C->>`: the key is `>` itself, so the first `>` closed
                // nothing. Extend the token through the real closer and
                // parse again.
                Err(e)
                    if matches!(e.kind, KeyParseErrorKind::MissingKey)
                        && after.starts_with('>') =>
                {
                    token = &rest[..token.len() + 1];
                    after = &after[1..];
                    parse_chord(token)?
                }
                Err(e) => return Err(e),
            };
            if !pending_mods.is_empty() {
                chord = merge_leader_mods(token, pending_mods, chord)?;
                pending_mods = ModFlags::NONE;
            }
            chords.push(chord);
        }

        first_token = false;
        rest = after.trim_start();
    }

    if !pending_mods.is_empty() {
        // The whole sequence was `<leader>` with a modifier-run leader.
        return Err(err(s, KeyParseErrorKind::DanglingLeaderMods));
    }
    if chords.is_empty() {
        return Err(err(s, KeyParseErrorKind::Empty));
    }
    let len = chords.len();
    if len > usize::from(max_chord_depth) {
        return Err(err(
            s,
            KeyParseErrorKind::SequenceTooLong {
                len,
                max: max_chord_depth,
            },
        ));
    }

    let mut it = chords.into_iter();
    let first = it.next().expect("chords is not empty");
    Ok(KeySequence::new(first, it.collect()))
}

#[cfg(test)]
mod tests;
