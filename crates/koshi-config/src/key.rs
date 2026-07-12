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
//! `ALT|SHIFT` plus `Char('h')`. `S-` is rejected on a non-letter character,
//! because "shift plus `1`" names no character without knowing the keyboard
//! layout — write `!` instead. A named key accepts `S-`: `<S-Tab>` is
//! Shift+Tab. A raw whitespace or control character (a literal tab in the
//! config text) is refused: those keys are written by name, keeping one
//! representation per physical key.

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
    /// `S-` applied to something with no capital form.
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
/// the unconsumed remainder. A leading pair whose first character is not a
/// modifier letter is an error; anything that is not a pair at all ends the run,
/// which is what leaves `<Space>` and `<C-->` alone.
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
/// its lowercase plus [`ModFlags::SHIFT`]. Rejects a `SHIFT` that lands on a
/// character with no capital form, and a whitespace or control character, whose
/// key has a named spelling.
fn finish_char(token: &str, mods: ModFlags, c: char) -> Result<KeyChord, KeyParseError> {
    if c.is_whitespace() || c.is_control() {
        return Err(err(
            token,
            KeyParseErrorKind::RawWhitespaceOrControl { ch: c },
        ));
    }
    let mut mods = mods;
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
    let digits = name.strip_prefix('F').or_else(|| name.strip_prefix('f'));
    if let Some(digits) = digits {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
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
/// violates: empty input, unbalanced angle brackets, an unknown modifier or
/// key name, a duplicated modifier, `<leader>` used as a chord, or similar
/// malformed input.
pub fn parse_chord(s: &str) -> Result<KeyChord, KeyParseError> {
    if s.is_empty() {
        return Err(err(s, KeyParseErrorKind::Empty));
    }

    // No leading `<`: a single bare printable character.
    let Some(open) = s.strip_prefix('<') else {
        let mut chars = s.chars();
        let c = chars.next().expect("s is not empty");
        if chars.next().is_some() {
            return Err(err(s, KeyParseErrorKind::UnbracketedMultiChar));
        }
        return finish_char(s, ModFlags::NONE, c);
    };

    // Bracketed form: must close with `>`.
    let Some(inner) = open.strip_suffix('>') else {
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
/// A modifier run merges into the chord that follows it, so with [`Leader::Mods`]
/// holding Control, `<leader>l` is one chord, `<C-l>`. A chord leader stands
/// alone, so with [`Leader::Chord`] holding Space, `<leader>l` is two chords,
/// Space then `l`.
///
/// Every binding starts with the leader, so a leader on a key that
/// [`KeyChord::is_typeable`] reports as typeable steals that key from the pane
/// whenever the client is not locked. The default is a modifier run, which
/// plain typing can never hit.
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
/// Returns a [`KeyParseError`] under the same conditions as [`parse_chord`]
/// when the input is not a bare modifier run.
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
