//! Parses key chords and the leader prefix from config text.
//!
//! The grammar is Neovim's. One chord is either a bare printable character
//! (`n`), or an angle-bracketed token carrying an optional modifier run
//! (`<C-p>`, `<A-S-n>`, `<F5>`, `<Space>`). Modifiers are `C-` Control, `A-`
//! Alt, `S-` Shift, `D-` Super, each written once, in any order, and matched
//! case-insensitively. Splitting a multi-chord sequence such as `<C-p>n` into
//! tokens, and substituting `<leader>`, happen in the sequence parser; here
//! `<leader>` is refused.
//!
//! Case folds into the Shift bit: `<A-H>` and `<A-S-h>` both parse to
//! `ALT|SHIFT` plus `Char('h')`. `S-` is rejected on a character that is not
//! lowercase: `<S-1>` fails, and the shifted character is written itself,
//! `!`. A named key accepts `S-`: `<S-Tab>` is Shift+Tab. A raw whitespace or
//! control character (a literal tab in the config text) is refused; those
//! keys are written by name, `<Tab>`.

use std::fmt;

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::key::{fold_uppercase_character, Key, KeyChord, ModFlags, NamedKey};
use thiserror::Error;

/// A key token that does not name a chord, with the failed token and reason.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid key `{key_token}`: {error_kind}")]
pub struct KeyParseError {
    /// The token as written in the config.
    pub key_token: String,
    /// Why it failed.
    pub error_kind: KeyParseErrorKind,
}

impl DomainError for KeyParseError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Config
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// The reason a key token failed to parse.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyParseErrorKind {
    /// The token was the empty string.
    #[error("empty key")]
    Empty,
    /// The token opened with `<` and never closed.
    #[error("missing closing `>`")]
    UnclosedBracket,
    /// Modifiers were given with no key after them, as in `<C->`.
    #[error("no key after the modifiers")]
    MissingKey,
    /// A modifier letter that is not one of `C`, `A`, `S`, `D`.
    #[error("unknown modifier `{modifier}-`; use `C-`, `A-`, `S-`, or `D-`")]
    UnknownModifier {
        /// The unrecognized modifier letter.
        modifier: char,
    },
    /// The same modifier was written twice, as in `<C-C-a>`.
    #[error("modifier `{modifier}-` given twice")]
    DuplicateModifier {
        /// The repeated modifier letter.
        modifier: char,
    },
    /// A bracketed multi-character key that names no known key.
    #[error("unknown key name `{key_name}`")]
    UnknownNamedKey {
        /// The unrecognized name.
        key_name: String,
    },
    /// Several characters with no brackets, as in `Ctrl-g` or `Tab`.
    #[error("a multi-character key must be bracketed, as in `<Tab>`")]
    UnbracketedMultiChar,
    /// `S-` applied to a character that is not lowercase.
    #[error(
        "`S-` applies to letters only, not `{key_character}`; write the shifted character itself"
    )]
    ShiftOnNonLetter {
        /// The key the shift was applied to.
        key_character: char,
    },
    /// A function key outside `F1..=F24`.
    #[error("function keys run F1 to F24, got `F{function_key_number_text}`")]
    FunctionKeyOutOfRange {
        /// The number as written.
        function_key_number_text: String,
    },
    /// A raw whitespace or control character where a key was expected.
    #[error(
        "the character {key_character:?} is written by its key name, such as `<Space>` or `<Tab>`"
    )]
    RawWhitespaceOrControl {
        /// The character as written.
        key_character: char,
    },
    /// `<leader>` where a single chord was expected.
    #[error("`<leader>` stands for a prefix, not a chord")]
    LeaderNotAChord,
    /// `<leader>` in any sequence position other than the first.
    #[error("`<leader>` may only open a sequence")]
    LeaderNotFirst,
    /// A modifier-run leader standing alone, with no chord after it to merge
    /// into.
    #[error("the leader's modifiers need a key after them")]
    DanglingLeaderMods,
    /// A sequence with more chords than the configured cap.
    #[error("the sequence has {chord_count} chords; the cap is {max_chord_depth}")]
    SequenceTooLong {
        /// The number of chords written.
        chord_count: usize,
        /// The configured `max_chord_depth`.
        max_chord_depth: u8,
    },
}

/// Attaches the failing `key_token` to an `error_kind`.
pub(crate) fn create_key_parse_error(
    key_token: &str,
    error_kind: KeyParseErrorKind,
) -> KeyParseError {
    KeyParseError {
        key_token: key_token.to_string(),
        error_kind,
    }
}

/// Maps a modifier letter to its bit, accepting either case.
fn resolve_modifier_flag(modifier_character: char) -> Option<ModFlags> {
    match modifier_character {
        'C' | 'c' => Some(ModFlags::CTRL),
        'A' | 'a' => Some(ModFlags::ALT),
        'S' | 's' => Some(ModFlags::SHIFT),
        'D' | 'd' => Some(ModFlags::SUPER),
        _ => None,
    }
}

/// Consumes leading `X-` modifier pairs from `key_text`, returning the modifiers
/// and the unconsumed remainder. `key_token` is the whole key token, carried into any
/// error. A leading pair whose first character is not a modifier letter is an
/// error. Anything that is not an `X-` pair ends the run: `Space` leaves the
/// whole word (`S` is not followed by `-`), and `C--` yields
/// [`ModFlags::CTRL`] with `-` left.
fn split_modifier_flags<'a>(
    key_token: &str,
    key_text: &'a str,
) -> Result<(ModFlags, &'a str), KeyParseError> {
    let mut modifier_flags = ModFlags::NONE;
    let mut remaining_key_text = key_text;
    loop {
        let mut key_text_characters = remaining_key_text.chars();
        let (Some(modifier_character), Some('-')) =
            (key_text_characters.next(), key_text_characters.next())
        else {
            // Not an `X-` pair: too short, or the second character is not a
            // dash. The modifier run is over.
            return Ok((modifier_flags, remaining_key_text));
        };
        let Some(modifier_flag) = resolve_modifier_flag(modifier_character) else {
            return Err(create_key_parse_error(
                key_token,
                KeyParseErrorKind::UnknownModifier {
                    modifier: modifier_character,
                },
            ));
        };
        if modifier_flags.has_all_modifiers(modifier_flag) {
            return Err(create_key_parse_error(
                key_token,
                KeyParseErrorKind::DuplicateModifier {
                    modifier: modifier_character,
                },
            ));
        }
        modifier_flags = modifier_flags.union(modifier_flag);
        // Drop the consumed `X-` pair and look for another one.
        remaining_key_text = key_text_characters.as_str();
    }
}

/// Folds a single-character key into canonical form: an uppercase letter becomes
/// its lowercase plus [`ModFlags::SHIFT`]. Rejects `SHIFT` on a character that
/// is not lowercase (`<S-1>`), and rejects any whitespace or control character.
fn finish_key_character(
    key_token: &str,
    mut modifier_flags: ModFlags,
    key_character: char,
) -> Result<KeyChord, KeyParseError> {
    if key_character.is_whitespace() || key_character.is_control() {
        return Err(create_key_parse_error(
            key_token,
            KeyParseErrorKind::RawWhitespaceOrControl { key_character },
        ));
    }
    let (canonical_key_character, needs_shift_modifier) = fold_uppercase_character(key_character);
    if needs_shift_modifier {
        modifier_flags = modifier_flags.union(ModFlags::SHIFT);
    }
    if modifier_flags.has_all_modifiers(ModFlags::SHIFT) && !canonical_key_character.is_lowercase()
    {
        return Err(create_key_parse_error(
            key_token,
            KeyParseErrorKind::ShiftOnNonLetter {
                key_character: canonical_key_character,
            },
        ));
    }
    Ok(KeyChord::from_parts(
        modifier_flags,
        Key::Char(canonical_key_character),
    ))
}

/// Resolves a bracketed multi-character key name.
fn resolve_named_key(key_token: &str, key_name: &str) -> Result<NamedKey, KeyParseError> {
    let function_key_number_text = key_name
        .strip_prefix('F')
        .or_else(|| key_name.strip_prefix('f'))
        .filter(|function_key_number_text| {
            !function_key_number_text.is_empty()
                && function_key_number_text
                    .chars()
                    .all(|key_character| key_character.is_ascii_digit())
        });
    if let Some(function_key_number_text) = function_key_number_text {
        return function_key_number_text
            .parse::<u8>()
            .ok()
            .filter(|function_key_number| (1..=24).contains(function_key_number))
            .map(NamedKey::F)
            .ok_or_else(|| {
                create_key_parse_error(
                    key_token,
                    KeyParseErrorKind::FunctionKeyOutOfRange {
                        function_key_number_text: function_key_number_text.to_string(),
                    },
                )
            });
    }
    Ok(match key_name.len() {
        2 if key_name.eq_ignore_ascii_case("cr") => NamedKey::Enter,
        2 if key_name.eq_ignore_ascii_case("bs") => NamedKey::Backspace,
        2 if key_name.eq_ignore_ascii_case("up") => NamedKey::Up,
        3 if key_name.eq_ignore_ascii_case("tab") => NamedKey::Tab,
        3 if key_name.eq_ignore_ascii_case("esc") => NamedKey::Esc,
        3 if key_name.eq_ignore_ascii_case("del") => NamedKey::Delete,
        3 if key_name.eq_ignore_ascii_case("end") => NamedKey::End,
        4 if key_name.eq_ignore_ascii_case("left") => NamedKey::Left,
        4 if key_name.eq_ignore_ascii_case("home") => NamedKey::Home,
        4 if key_name.eq_ignore_ascii_case("down") => NamedKey::Down,
        5 if key_name.eq_ignore_ascii_case("space") => NamedKey::Space,
        5 if key_name.eq_ignore_ascii_case("right") => NamedKey::Right,
        6 if key_name.eq_ignore_ascii_case("insert") => NamedKey::Insert,
        6 if key_name.eq_ignore_ascii_case("pageup") => NamedKey::PageUp,
        8 if key_name.eq_ignore_ascii_case("pagedown") => NamedKey::PageDown,
        _ => {
            return Err(create_key_parse_error(
                key_token,
                KeyParseErrorKind::UnknownNamedKey {
                    key_name: key_name.to_string(),
                },
            ));
        }
    })
}

/// Parses one key chord from its config text form.
///
/// Accepts a bare printable character (`n`) or an angle-bracketed token with an
/// optional modifier run (`<C-p>`, `<A-S-n>`, `<F5>`, `<Space>`). An uppercase
/// letter folds to lowercase plus [`ModFlags::SHIFT`]. `<leader>` is refused: it
/// stands for a prefix, which only the sequence parser can substitute.
///
/// # Errors
/// Returns a [`KeyParseError`] naming the [`KeyParseErrorKind`] the token
/// violates: [`Empty`](KeyParseErrorKind::Empty) for `""`,
/// [`UnclosedBracket`](KeyParseErrorKind::UnclosedBracket) for a `<` with no
/// closing `>`, [`MissingKey`](KeyParseErrorKind::MissingKey) for `<>` and
/// `<C->`, [`LeaderNotAChord`](KeyParseErrorKind::LeaderNotAChord) for
/// `<leader>`, [`UnknownModifier`](KeyParseErrorKind::UnknownModifier) for
/// `<x-a>`, [`DuplicateModifier`](KeyParseErrorKind::DuplicateModifier) for
/// `<C-C-a>`, [`UnbracketedMultiChar`](KeyParseErrorKind::UnbracketedMultiChar)
/// for `Ctrl-g`, [`UnknownNamedKey`](KeyParseErrorKind::UnknownNamedKey) for
/// `<Nope>`, [`FunctionKeyOutOfRange`](KeyParseErrorKind::FunctionKeyOutOfRange)
/// for `<F25>`, [`ShiftOnNonLetter`](KeyParseErrorKind::ShiftOnNonLetter) for
/// `<S-1>`, and
/// [`RawWhitespaceOrControl`](KeyParseErrorKind::RawWhitespaceOrControl) for a
/// literal tab.
pub fn parse_chord(chord_text: &str) -> Result<KeyChord, KeyParseError> {
    if chord_text.is_empty() {
        return Err(create_key_parse_error(chord_text, KeyParseErrorKind::Empty));
    }

    // No leading `<`: a single bare printable character.
    let Some(bracketed_body) = chord_text.strip_prefix('<') else {
        let mut chord_characters = chord_text.chars();
        let key_character = chord_characters.next().expect("chord_text is not empty");
        if chord_characters.next().is_some() {
            return Err(create_key_parse_error(
                chord_text,
                KeyParseErrorKind::UnbracketedMultiChar,
            ));
        }
        return finish_key_character(chord_text, ModFlags::NONE, key_character);
    };

    // Bracketed form: must close with `>`.
    let Some(bracketed_key_text) = bracketed_body.strip_suffix('>') else {
        return Err(create_key_parse_error(
            chord_text,
            KeyParseErrorKind::UnclosedBracket,
        ));
    };
    if bracketed_key_text.is_empty() {
        return Err(create_key_parse_error(
            chord_text,
            KeyParseErrorKind::MissingKey,
        ));
    }
    if bracketed_key_text.eq_ignore_ascii_case("leader") {
        return Err(create_key_parse_error(
            chord_text,
            KeyParseErrorKind::LeaderNotAChord,
        ));
    }

    // Strip any `X-` modifier pairs, leaving the key itself.
    let (modifier_flags, remaining_key_text) =
        split_modifier_flags(chord_text, bracketed_key_text)?;
    if remaining_key_text.is_empty() {
        return Err(create_key_parse_error(
            chord_text,
            KeyParseErrorKind::MissingKey,
        ));
    }

    // One character left: a single (possibly modified) key. More than one:
    // a bracketed name such as `Tab` or `F5`.
    let mut key_characters = remaining_key_text.chars();
    let key_character = key_characters
        .next()
        .expect("remaining_key_text is not empty");
    if key_characters.next().is_none() {
        finish_key_character(chord_text, modifier_flags, key_character)
    } else {
        Ok(KeyChord::from_parts(
            modifier_flags,
            Key::Named(resolve_named_key(chord_text, remaining_key_text)?),
        ))
    }
}

/// What `<leader>` in a binding stands for.
///
/// A modifier run merges into the chord that follows it: with [`Leader::Mods`]
/// holding Control, `<leader>l` is one chord, `<C-l>`. A chord leader stands
/// alone: with [`Leader::Chord`] holding Space, `<leader>l` is two chords,
/// Space then `l`.
///
/// A leader that [`KeyChord::is_typeable`] reports as typeable, or a modifier
/// run that [`ModFlags::is_typing`] reports as typing, puts every
/// leader-relative binding on a key plain typing produces, and those keys
/// stop reaching the pane while the client is unlocked. The default is `C-`,
/// which plain typing never produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leader {
    /// Modifiers that merge into the following chord, written `C-`.
    Mods(ModFlags),
    /// A chord of its own, written like any other chord.
    Chord(KeyChord),
}

impl Default for Leader {
    fn default() -> Self {
        Self::Mods(ModFlags::CTRL)
    }
}

impl fmt::Display for Leader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mods(modifier_flags) => write!(f, "{modifier_flags}"),
            Self::Chord(key_chord) => write!(f, "{key_chord}"),
        }
    }
}

/// Parses the configured leader: either a bare modifier run such as `C-`, or a
/// single chord such as `<Space>` or `,`.
///
/// # Errors
/// Returns a [`KeyParseError`] for the empty string. A trailing-dash run
/// holding an unknown or repeated modifier letter reports that modifier
/// (`x-` gives [`KeyParseErrorKind::UnknownModifier`]). Any other input
/// reports what [`parse_chord`] rejects it for.
pub fn parse_leader(leader_text: &str) -> Result<Leader, KeyParseError> {
    if leader_text.is_empty() {
        return Err(create_key_parse_error(
            leader_text,
            KeyParseErrorKind::Empty,
        ));
    }
    if !leader_text.starts_with('<') && leader_text.ends_with('-') {
        let (modifier_flags, remaining_key_text) = split_modifier_flags(leader_text, leader_text)?;
        if remaining_key_text.is_empty() && !modifier_flags.is_empty() {
            return Ok(Leader::Mods(modifier_flags));
        }
    }
    parse_chord(leader_text).map(Leader::Chord)
}

#[cfg(test)]
mod tests;
