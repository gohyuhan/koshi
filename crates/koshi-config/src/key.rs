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
use koshi_core::key::{fold_uppercase, Key, KeyChord, ModFlags, NamedKey};
use thiserror::Error;

/// A key token that does not name a chord, with the token that failed.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid key `{token}`: {kind}")]
pub struct KeyParseError {
    /// The token as written in the config.
    pub token: String,
    /// Why it failed.
    pub kind: KeyParseErrorKind,
}

impl DomainError for KeyParseError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Config
    }

    fn severity(&self) -> Severity {
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
    #[error("unknown key name `{name}`")]
    UnknownNamedKey {
        /// The unrecognized name.
        name: String,
    },
    /// Several characters with no brackets, as in `Ctrl-g` or `Tab`.
    #[error("a multi-character key must be bracketed, as in `<Tab>`")]
    UnbracketedMultiChar,
    /// `S-` applied to a character that is not lowercase.
    #[error("`S-` applies to letters only, not `{ch}`; write the shifted character itself")]
    ShiftOnNonLetter {
        /// The key the shift was applied to.
        ch: char,
    },
    /// A function key outside `F1..=F24`.
    #[error("function keys run F1 to F24, got `F{n}`")]
    FunctionKeyOutOfRange {
        /// The number as written.
        n: String,
    },
    /// A raw whitespace or control character where a key was expected.
    #[error("the character {ch:?} is written by its key name, such as `<Space>` or `<Tab>`")]
    RawWhitespaceOrControl {
        /// The character as written.
        ch: char,
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
    #[error("the sequence has {len} chords; the cap is {max}")]
    SequenceTooLong {
        /// The number of chords written.
        len: usize,
        /// The configured `max_chord_depth`.
        max: u8,
    },
}

/// Attaches the failing `token` to a `kind`.
pub(crate) fn err(token: &str, kind: KeyParseErrorKind) -> KeyParseError {
    KeyParseError {
        token: token.to_string(),
        kind,
    }
}

/// Maps a modifier letter to its bit, accepting either case.
fn mod_flag(c: char) -> Option<ModFlags> {
    match c {
        'C' | 'c' => Some(ModFlags::CTRL),
        'A' | 'a' => Some(ModFlags::ALT),
        'S' | 's' => Some(ModFlags::SHIFT),
        'D' | 'd' => Some(ModFlags::SUPER),
        _ => None,
    }
}

/// Consumes leading `X-` modifier pairs from `s`, returning the modifiers and
/// the unconsumed remainder. `token` is the whole key token, carried into any
/// error. A leading pair whose first character is not a modifier letter is an
/// error. Anything that is not an `X-` pair ends the run: `Space` leaves the
/// whole word (`S` is not followed by `-`), and `C--` yields
/// [`ModFlags::CTRL`] with `-` left.
fn split_mods<'a>(token: &str, s: &'a str) -> Result<(ModFlags, &'a str), KeyParseError> {
    let mut mods = ModFlags::NONE;
    let mut rest = s;
    loop {
        let mut chars = rest.chars();
        let (Some(c), Some('-')) = (chars.next(), chars.next()) else {
            // Not an `X-` pair: too short, or the second character is not a
            // dash. The modifier run is over.
            return Ok((mods, rest));
        };
        let Some(flag) = mod_flag(c) else {
            return Err(err(
                token,
                KeyParseErrorKind::UnknownModifier { modifier: c },
            ));
        };
        if mods.contains(flag) {
            return Err(err(
                token,
                KeyParseErrorKind::DuplicateModifier { modifier: c },
            ));
        }
        mods = mods.union(flag);
        // Drop the consumed `X-` pair and look for another one.
        rest = chars.as_str();
    }
}

/// Folds a single-character key into canonical form: an uppercase letter becomes
/// its lowercase plus [`ModFlags::SHIFT`]. Rejects `SHIFT` on a character that
/// is not lowercase (`<S-1>`), and rejects any whitespace or control character.
fn finish_char(token: &str, mut mods: ModFlags, c: char) -> Result<KeyChord, KeyParseError> {
    if c.is_whitespace() || c.is_control() {
        return Err(err(
            token,
            KeyParseErrorKind::RawWhitespaceOrControl { ch: c },
        ));
    }
    let (key_char, shifted) = fold_uppercase(c);
    if shifted {
        mods = mods.union(ModFlags::SHIFT);
    }
    if mods.contains(ModFlags::SHIFT) && !key_char.is_lowercase() {
        return Err(err(
            token,
            KeyParseErrorKind::ShiftOnNonLetter { ch: key_char },
        ));
    }
    Ok(KeyChord::new(mods, Key::Char(key_char)))
}

/// Resolves a bracketed multi-character key name.
fn named_key(token: &str, name: &str) -> Result<NamedKey, KeyParseError> {
    let function_key_digits = name
        .strip_prefix('F')
        .or_else(|| name.strip_prefix('f'))
        .filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
    if let Some(digits) = function_key_digits {
        return digits
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=24).contains(n))
            .map(NamedKey::F)
            .ok_or_else(|| {
                err(
                    token,
                    KeyParseErrorKind::FunctionKeyOutOfRange {
                        n: digits.to_string(),
                    },
                )
            });
    }
    // ponytail: allocates a lowercase copy per name; this runs at config load.
    let key = match name.to_ascii_lowercase().as_str() {
        "cr" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "bs" => NamedKey::Backspace,
        "esc" => NamedKey::Esc,
        "space" => NamedKey::Space,
        "insert" => NamedKey::Insert,
        "del" => NamedKey::Delete,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" => NamedKey::PageUp,
        "pagedown" => NamedKey::PageDown,
        "left" => NamedKey::Left,
        "right" => NamedKey::Right,
        "up" => NamedKey::Up,
        "down" => NamedKey::Down,
        _ => {
            return Err(err(
                token,
                KeyParseErrorKind::UnknownNamedKey {
                    name: name.to_string(),
                },
            ));
        }
    };
    Ok(key)
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
pub fn parse_chord(s: &str) -> Result<KeyChord, KeyParseError> {
    if s.is_empty() {
        return Err(err(s, KeyParseErrorKind::Empty));
    }

    // No leading `<`: a single bare printable character.
    let Some(after_open) = s.strip_prefix('<') else {
        let mut chars = s.chars();
        let c = chars.next().expect("s is not empty");
        if chars.next().is_some() {
            return Err(err(s, KeyParseErrorKind::UnbracketedMultiChar));
        }
        return finish_char(s, ModFlags::NONE, c);
    };

    // Bracketed form: must close with `>`.
    let Some(inner) = after_open.strip_suffix('>') else {
        return Err(err(s, KeyParseErrorKind::UnclosedBracket));
    };
    if inner.is_empty() {
        return Err(err(s, KeyParseErrorKind::MissingKey));
    }
    if inner.eq_ignore_ascii_case("leader") {
        return Err(err(s, KeyParseErrorKind::LeaderNotAChord));
    }

    // Strip any `X-` modifier pairs, leaving the key itself.
    let (mods, rest) = split_mods(s, inner)?;
    if rest.is_empty() {
        return Err(err(s, KeyParseErrorKind::MissingKey));
    }

    // One character left: a single (possibly modified) key. More than one:
    // a bracketed name such as `Tab` or `F5`.
    let mut chars = rest.chars();
    let c = chars.next().expect("rest is not empty");
    if chars.next().is_none() {
        finish_char(s, mods, c)
    } else {
        Ok(KeyChord::new(mods, Key::Named(named_key(s, rest)?)))
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
            Self::Mods(m) => write!(f, "{m}"),
            Self::Chord(c) => write!(f, "{c}"),
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
pub fn parse_leader(s: &str) -> Result<Leader, KeyParseError> {
    if s.is_empty() {
        return Err(err(s, KeyParseErrorKind::Empty));
    }
    if !s.starts_with('<') && s.ends_with('-') {
        let (mods, rest) = split_mods(s, s)?;
        if rest.is_empty() && !mods.is_empty() {
            return Ok(Leader::Mods(mods));
        }
    }
    parse_chord(s).map(Leader::Chord)
}

#[cfg(test)]
mod tests;
