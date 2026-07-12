//! Keyboard chord model: a modifier bitmap plus one key.
//!
//! A [`KeyChord`] is the unit a keybinding matches on, and a [`KeySequence`]
//! is the ordered run of chords one binding triggers on. Config text parses
//! into chords, terminal input events normalize into chords, and the keymap
//! compares the two. This module owns the value types and their canonical
//! string form; it does no parsing.
//!
//! # Canonical form
//!
//! A printable letter is stored **lowercase**, with its case carried by
//! [`ModFlags::SHIFT`]: `<A-H>` and `<A-S-h>` are the same chord. `SHIFT` is
//! never set alongside a non-letter character — the shifted character stands
//! for itself (`!`, not shift-plus-`1`). A named key carries `SHIFT` like any
//! other modifier: `<S-Tab>` is Shift+Tab. The input layer normalizes inbound
//! events to this same form, which matters because hosts disagree: a terminal
//! without the kitty keyboard protocol reports Alt+Shift+h as `Char('H')`
//! carrying only ALT, while the Windows console reports `Char('h')` carrying
//! ALT and SHIFT.

use std::fmt;

/// The modifier keys held down as part of a chord, packed one per bit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModFlags(u8);

impl ModFlags {
    /// No modifiers held.
    pub const NONE: Self = Self(0);
    /// The Control key.
    pub const CTRL: Self = Self(1 << 0);
    /// The Alt (Option) key.
    pub const ALT: Self = Self(1 << 1);
    /// The Shift key.
    pub const SHIFT: Self = Self(1 << 2);
    /// The Super (Command, Windows) key.
    pub const SUPER: Self = Self(1 << 3);

    /// The raw bit pattern.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True when no modifier is held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True when every modifier in `other` is held.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// True when at least one modifier in `other` is held.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// The modifiers held in either set.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for ModFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl fmt::Display for ModFlags {
    /// Writes the modifier prefix run in canonical `C-A-S-D-` order, empty when
    /// no modifier is held.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.contains(Self::CTRL) {
            f.write_str("C-")?;
        }
        if self.contains(Self::ALT) {
            f.write_str("A-")?;
        }
        if self.contains(Self::SHIFT) {
            f.write_str("S-")?;
        }
        if self.contains(Self::SUPER) {
            f.write_str("D-")?;
        }
        Ok(())
    }
}

/// The modifiers that make a chord something ordinary typing cannot produce.
/// Shift is absent: Shift plus a key is still typing — it gives the key's
/// capital or shifted variant.
const NON_TEXT: ModFlags = ModFlags(ModFlags::CTRL.0 | ModFlags::ALT.0 | ModFlags::SUPER.0);

/// A key that is not a printable character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamedKey {
    /// Return / Enter.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Esc,
    /// The space bar, when bound as a key rather than typed as a character.
    Space,
    /// Insert.
    Insert,
    /// Forward delete.
    Delete,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Function key `F1` through `F24`.
    F(u8),
}

impl fmt::Display for NamedKey {
    /// Writes the single canonical spelling the chord parser accepts for this key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enter => f.write_str("CR"),
            Self::Tab => f.write_str("Tab"),
            Self::Backspace => f.write_str("BS"),
            Self::Esc => f.write_str("Esc"),
            Self::Space => f.write_str("Space"),
            Self::Insert => f.write_str("Insert"),
            Self::Delete => f.write_str("Del"),
            Self::Home => f.write_str("Home"),
            Self::End => f.write_str("End"),
            Self::PageUp => f.write_str("PageUp"),
            Self::PageDown => f.write_str("PageDown"),
            Self::Left => f.write_str("Left"),
            Self::Right => f.write_str("Right"),
            Self::Up => f.write_str("Up"),
            Self::Down => f.write_str("Down"),
            Self::F(n) => write!(f, "F{n}"),
        }
    }
}

/// The key part of a chord, with the modifiers stripped off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A printable character, lowercase when it has a single-character
    /// lowercase mapping; the capital is carried by [`ModFlags::SHIFT`].
    Char(char),
    /// A key with a name rather than a character.
    Named(NamedKey),
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(c) => write!(f, "{c}"),
            Self::Named(n) => write!(f, "{n}"),
        }
    }
}

/// Folds an uppercase letter into the `lowercase + Shift` form every
/// [`Key::Char`] chord is stored in — the one rule shared by the config
/// parser and the input decoder.
///
/// - `'A'` → `('a', true)` — the `true` means "Shift is part of this key".
/// - `'a'`, `'!'`, `'1'` → unchanged, `false` — nothing to fold.
/// - `'İ'` → `('İ', false)` — Unicode lowercasing can produce MORE than one
///   character (`'İ'` lowercases to `'i'` plus a combining dot), and a
///   [`Key::Char`] holds exactly one, so such a letter cannot be modeled as
///   `Shift + one lowercase char` and stands as it is.
#[must_use]
pub fn fold_uppercase(c: char) -> (char, bool) {
    if !c.is_uppercase() {
        return (c, false);
    }
    // `to_lowercase()` is an iterator because a lowercase mapping may be
    // several chars; `(Some(l), None)` = the mapping is exactly one char.
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => (l, true),
        _ => (c, false),
    }
}

/// One key press: the modifiers held, and the key itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyChord {
    /// The modifier keys held down.
    pub mods: ModFlags,
    /// The key pressed.
    pub key: Key,
}

impl KeyChord {
    /// Builds a chord from its parts. Callers are responsible for the canonical
    /// form described in the module documentation; the config crate's chord
    /// parser produces it.
    pub const fn new(mods: ModFlags, key: Key) -> Self {
        Self { mods, key }
    }

    /// True when this chord is something ordinary typing produces: no
    /// Control, Alt, or Super is held. Characters, Enter, arrows, editing
    /// keys, and function keys all count, with or without Shift.
    ///
    /// This classifies, it does not forbid. Outside lock mode a key goes to
    /// the keymap before the pane, so any binding — a typeable one included —
    /// takes its key away from the pane until the client locks. The value
    /// exists so the keybinding layer can keep shipped defaults and the
    /// lock-mode unlock chord off keys that plain typing would hit.
    pub fn is_typeable(&self) -> bool {
        !self.mods.intersects(NON_TEXT)
    }
}

impl fmt::Display for KeyChord {
    /// Writes the canonical text form, which parses back to an equal chord.
    ///
    /// Wraps the chord in `<...>` whenever a modifier is held, the key is a
    /// named key (e.g. `Tab`, `Left`), or the key is the literal `<`
    /// character — bracketing a lone `<` keeps it from being misread as the
    /// start of a bracketed chord. Anything else (a plain lowercase letter or
    /// other character with no modifiers) is written unbracketed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bracketed = !self.mods.is_empty() || matches!(self.key, Key::Named(_) | Key::Char('<'));
        if bracketed {
            write!(f, "<{}{}>", self.mods, self.key)
        } else {
            write!(f, "{}", self.key)
        }
    }
}

/// An ordered run of chords pressed one after another to trigger one binding.
///
/// Most bindings are a single chord; leader- and prefix-style bindings
/// (`<C-p> n`) run several. A sequence holds at least one chord by
/// construction: `new` takes the first chord separately from the rest. The
/// configured chord-depth cap is enforced where sequences are parsed and
/// validated, not by this type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeySequence(Vec<KeyChord>);

impl KeySequence {
    /// Builds a sequence from its chords in press order: the first chord,
    /// then any that follow it.
    pub fn new(first: KeyChord, rest: Vec<KeyChord>) -> Self {
        let mut chords = Vec::with_capacity(1 + rest.len());
        chords.push(first);
        chords.extend(rest);
        Self(chords)
    }

    /// The chords in press order; never empty.
    pub fn chords(&self) -> &[KeyChord] {
        &self.0
    }
}

impl From<KeyChord> for KeySequence {
    /// Wraps a single chord as a one-chord sequence.
    fn from(chord: KeyChord) -> Self {
        Self(vec![chord])
    }
}

impl fmt::Display for KeySequence {
    /// Writes each chord's canonical text form, space-separated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for chord in &self.0 {
            if !first {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
            first = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
