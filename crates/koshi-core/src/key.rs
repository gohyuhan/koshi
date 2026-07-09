//! Keyboard chord model: a modifier bitmap plus one key.
//!
//! A [`KeyChord`] is the unit a keybinding matches on. Config text parses into
//! chords, terminal input events normalize into chords, and the keymap compares
//! the two. This module owns the value type and its canonical string form; it
//! does no parsing.
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

/// The modifiers that lift a chord out of the focused pane's ordinary input
/// stream. Shift is absent: shift plus a key is that key's capital or shifted
/// variant, which the pane still expects to receive.
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

    /// True when the focused pane expects to receive this chord as ordinary
    /// input, so binding it as the first chord of a sequence in a transparent
    /// mode would swallow input the pane is waiting for. A program running in
    /// the pane may be reading any unmodified key — characters, Enter, arrows,
    /// editing keys, function keys — and Shift only selects a key's capital or
    /// shifted variant. Control, Alt, and Super each lift a chord out of the
    /// pane's input stream.
    pub fn is_typeable(&self) -> bool {
        !self.mods.intersects(NON_TEXT)
    }
}

impl fmt::Display for KeyChord {
    /// Writes the canonical text form, which parses back to an equal chord.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bracketed = !self.mods.is_empty() || matches!(self.key, Key::Named(_) | Key::Char('<'));
        if bracketed {
            write!(f, "<{}{}>", self.mods, self.key)
        } else {
            write!(f, "{}", self.key)
        }
    }
}

#[cfg(test)]
mod tests;
