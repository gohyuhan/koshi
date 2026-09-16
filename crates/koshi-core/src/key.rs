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
//! events to this same form. Hosts differ on what they send: a terminal
//! without the kitty keyboard protocol reports Alt+Shift+h as `Char('H')`
//! carrying only ALT, while the Windows console reports `Char('h')` carrying
//! ALT and SHIFT.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Instant;

/// The modifier keys held down as part of a chord, packed one per bit.
///
/// Decoding refuses a bit that names no modifier; a decoded value holds only
/// the four below.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(try_from = "u8")]
pub struct ModFlags(u8);

/// Every bit the four modifiers occupy; the rest name nothing.
const MOD_FLAG_BITS: u8 = 0b0000_1111;

impl TryFrom<u8> for ModFlags {
    type Error = String;

    /// Accepts a bit pattern drawn only from the four modifier bits: Control,
    /// Alt, Shift and Super.
    fn try_from(modifier_bits: u8) -> Result<Self, Self::Error> {
        if modifier_bits & !MOD_FLAG_BITS == 0 {
            Ok(Self(modifier_bits))
        } else {
            Err(format!(
                "modifier bits {modifier_bits:#010b} name no modifier; the modifiers are {MOD_FLAG_BITS:#010b}"
            ))
        }
    }
}

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
    pub const fn has_all_modifiers(self, required_modifiers: Self) -> bool {
        self.0 & required_modifiers.0 == required_modifiers.0
    }

    /// True when at least one modifier in `other` is held.
    pub const fn has_shared_modifier(self, candidate_modifiers: Self) -> bool {
        self.0 & candidate_modifiers.0 != 0
    }

    /// The modifiers held in either set.
    pub const fn union(self, other_modifier_flags: Self) -> Self {
        Self(self.0 | other_modifier_flags.0)
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
        if self.has_all_modifiers(Self::CTRL) {
            f.write_str("C-")?;
        }
        if self.has_all_modifiers(Self::ALT) {
            f.write_str("A-")?;
        }
        if self.has_all_modifiers(Self::SHIFT) {
            f.write_str("S-")?;
        }
        if self.has_all_modifiers(Self::SUPER) {
            f.write_str("D-")?;
        }
        Ok(())
    }
}

/// The modifiers that make a chord something ordinary typing cannot produce.
/// Shift is absent: Shift plus a key is still typing — it gives the key's
/// capital or shifted variant.
const NON_TEXT_MODIFIER_FLAGS: ModFlags =
    ModFlags(ModFlags::CTRL.0 | ModFlags::ALT.0 | ModFlags::SUPER.0);

impl ModFlags {
    /// True when plain typing can produce a key held with exactly these
    /// modifiers: none of Control, Alt, or Super is held. Shift alone still
    /// types — it gives the key's capital or shifted variant.
    #[must_use]
    pub const fn is_typing(self) -> bool {
        !self.has_shared_modifier(NON_TEXT_MODIFIER_FLAGS)
    }
}

/// A key that is not a printable character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    F(#[serde(deserialize_with = "function_key_number")] u8),
}

/// The lowest and highest function key a terminal names.
const FIRST_FUNCTION_KEY: u8 = 1;
const LAST_FUNCTION_KEY: u8 = 24;

/// Decode a [`NamedKey::F`] number, refusing one no function key carries.
fn function_key_number<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let function_key_number_value = u8::deserialize(deserializer)?;
    if (FIRST_FUNCTION_KEY..=LAST_FUNCTION_KEY).contains(&function_key_number_value) {
        Ok(function_key_number_value)
    } else {
        Err(serde::de::Error::custom(format!(
            "F{function_key_number_value} is not a function key; they run F{FIRST_FUNCTION_KEY} through F{LAST_FUNCTION_KEY}"
        )))
    }
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
            Self::F(function_key_number) => write!(f, "F{function_key_number}"),
        }
    }
}

/// The key part of a chord, with the modifiers stripped off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
            Self::Char(character) => write!(f, "{character}"),
            Self::Named(named_key) => write!(f, "{named_key}"),
        }
    }
}

/// Folds an uppercase letter into the `lowercase + Shift` form every
/// [`Key::Char`] chord is stored in.
///
/// Returns the character to store and whether Shift is part of the key. A
/// letter folds only when its lowercase form is exactly one character and
/// that character uppercases back to exactly the original.
///
/// - `'A'` → `('a', true)`.
/// - `'a'`, `'!'`, `'1'` → unchanged, `false`.
/// - `'İ'` → `('İ', false)`: its lowercase form is two characters (`'i'` plus
///   a combining dot), and a [`Key::Char`] holds exactly one.
/// - `'ẞ'` → `('ẞ', false)`: it lowercases to `'ß'`, and `'ß'` uppercases to
///   `"SS"`, not back to `'ẞ'`.
/// - `'\u{212A}'` (the Kelvin sign) → unchanged, `false`: it lowercases to
///   `'k'`, and `'k'` uppercases to the Latin `'K'`, a different character.
#[must_use]
pub fn fold_uppercase_character(character: char) -> (char, bool) {
    if !character.is_uppercase() {
        return (character, false);
    }
    // `to_lowercase()` yields one or more chars; `(Some(l), None)` is exactly
    // one.
    let mut lowercase_characters = character.to_lowercase();
    let (Some(lowercase_character), None) =
        (lowercase_characters.next(), lowercase_characters.next())
    else {
        return (character, false);
    };
    // Fold only when the capital comes back: uppercasing the lowered form must
    // yield exactly the character that was typed.
    let mut uppercase_characters = lowercase_character.to_uppercase();
    match (uppercase_characters.next(), uppercase_characters.next()) {
        (Some(restored_character), None) if restored_character == character => {
            (lowercase_character, true)
        }
        _ => (character, false),
    }
}

/// One key press: the modifiers held, and the key itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KeyChord {
    /// The modifier keys held down.
    #[serde(rename = "mods")]
    pub modifier_flags: ModFlags,
    /// The key pressed.
    pub key: Key,
}

impl KeyChord {
    /// Builds a chord from its parts. Callers are responsible for the canonical
    /// form described in the module documentation; the config crate's chord
    /// parser produces it.
    pub const fn from_parts(modifier_flags: ModFlags, key: Key) -> Self {
        Self {
            modifier_flags,
            key,
        }
    }

    /// True when this chord is something ordinary typing produces: no
    /// Control, Alt, or Super is held. Characters, Enter, arrows, editing
    /// keys, and function keys all count, with or without Shift.
    pub fn is_typeable(&self) -> bool {
        self.modifier_flags.is_typing()
    }
}

impl fmt::Display for KeyChord {
    /// Writes the canonical text form, which parses back to an equal chord.
    ///
    /// Wraps the chord in `<...>` whenever a modifier is held, the key is a
    /// named key (e.g. `Tab`, `Left`), or the key is the literal `<`
    /// character (`<<>`). Any other character with no modifiers is written
    /// bare: `n`, `-`, `>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let is_bracketed =
            !self.modifier_flags.is_empty() || matches!(self.key, Key::Named(_) | Key::Char('<'));
        if is_bracketed {
            write!(f, "<{}{}>", self.modifier_flags, self.key)
        } else {
            write!(f, "{}", self.key)
        }
    }
}

/// An ordered run of chords pressed one after another to trigger one binding.
///
/// Most bindings are a single chord; leader- and prefix-style bindings
/// (`<C-p> n`) run several. A sequence holds at least one chord by
/// construction: `from_first_and_rest` takes the first chord separately from the rest. The
/// configured chord-depth cap is enforced where sequences are parsed and
/// validated, not by this type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeySequence(Vec<KeyChord>);

impl KeySequence {
    /// Builds a sequence from its chords in press order: the first chord,
    /// then any that follow it.
    pub fn from_first_and_rest(first_chord: KeyChord, remaining_chords: Vec<KeyChord>) -> Self {
        let mut chords = Vec::with_capacity(1 + remaining_chords.len());
        chords.push(first_chord);
        chords.extend(remaining_chords);
        Self(chords)
    }

    /// The chords in press order; never empty.
    pub fn list_chords(&self) -> &[KeyChord] {
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
        for (chord_index, chord) in self.0.iter().enumerate() {
            if chord_index > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

/// One incomplete multi-chord keybinding: the chords typed into it so far, and
/// the instant an ambiguous one resolves.
///
/// A chord held here is never written to a pane: it fires a binding, or it is
/// dropped when the sequence is left. The pane a chord was typed into, and the
/// byte form it would have taken there, are not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingKeySequence {
    /// Canonical chords pressed so far.
    pub sequence: KeySequence,
    /// Disambiguation instant, set only when the chords so far are BOTH a
    /// complete binding and the prefix of a longer one — reaching it fires the
    /// complete binding. A prefix-only sequence carries `None` and waits for
    /// the next chord indefinitely.
    pub deadline: Option<Instant>,
}

/// The modifier keys the outer terminal reported with one keyboard event,
/// packed one per bit.
///
/// This is the stored bitmap: it keeps every modifier the Kitty keyboard
/// protocol names, including the two lock states a pane encoding leaves out.
/// [`ModFlags`] is the narrower projection a keybinding matches on.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct KeyModifierFlags(u8);

impl KeyModifierFlags {
    /// No modifier key held.
    pub const NONE: Self = Self(0);
    /// Shift.
    pub const SHIFT: Self = Self(1 << 0);
    /// Alt or Option.
    pub const ALT: Self = Self(1 << 1);
    /// Control.
    pub const CTRL: Self = Self(1 << 2);
    /// Super, Command, or Windows.
    pub const SUPER: Self = Self(1 << 3);
    /// Hyper.
    pub const HYPER: Self = Self(1 << 4);
    /// Meta.
    pub const META: Self = Self(1 << 5);
    /// Caps Lock, reported as a held state rather than a press.
    pub const CAPS_LOCK: Self = Self(1 << 6);
    /// Num Lock, reported as a held state rather than a press.
    pub const NUM_LOCK: Self = Self(1 << 7);

    /// The raw bit pattern. Every one of the eight bits names a modifier.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The modifiers a raw bit pattern names. Every bit names a modifier, so
    /// no pattern is refused.
    #[must_use]
    pub const fn from_bits(modifier_bits: u8) -> Self {
        Self(modifier_bits)
    }

    /// True when every modifier in `required_modifiers` is held.
    #[must_use]
    pub const fn has_all_modifiers(self, required_modifiers: Self) -> bool {
        self.0 & required_modifiers.0 == required_modifiers.0
    }

    /// The modifiers held in either set.
    #[must_use]
    pub const fn union(self, other_modifier_flags: Self) -> Self {
        Self(self.0 | other_modifier_flags.0)
    }

    /// The four modifiers a keybinding matches on.
    ///
    /// Control, Alt and Shift carry across unchanged. Meta counts as Super,
    /// the same fold the chord parser applies. Hyper, Caps Lock and Num Lock
    /// name no binding modifier and are dropped.
    ///
    /// `CTRL | META | CAPS_LOCK` becomes `ModFlags::CTRL | ModFlags::SUPER`.
    #[must_use]
    pub const fn to_binding_modifiers(self) -> ModFlags {
        let mut binding_bits = 0;
        if self.has_all_modifiers(Self::CTRL) {
            binding_bits |= ModFlags::CTRL.0;
        }
        if self.has_all_modifiers(Self::ALT) {
            binding_bits |= ModFlags::ALT.0;
        }
        if self.has_all_modifiers(Self::SHIFT) {
            binding_bits |= ModFlags::SHIFT.0;
        }
        if self.has_all_modifiers(Self::SUPER) || self.has_all_modifiers(Self::META) {
            binding_bits |= ModFlags::SUPER.0;
        }
        ModFlags(binding_bits)
    }
}

impl std::ops::BitOr for KeyModifierFlags {
    type Output = Self;

    fn bitor(self, right_modifier_flags: Self) -> Self {
        self.union(right_modifier_flags)
    }
}

impl std::ops::BitOrAssign for KeyModifierFlags {
    fn bitor_assign(&mut self, right_modifier_flags: Self) {
        self.0 |= right_modifier_flags.0;
    }
}

/// The physical action one keyboard event reports.
///
/// The Kitty keyboard protocol numbers these 1, 2 and 3, and a missing event
/// type means [`KeyEventKind::Press`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyEventKind {
    /// A key went down.
    Press,
    /// A held key repeated.
    Repeat,
    /// A key came up.
    Release,
}

/// The key a keyboard event reports.
///
/// A key the keybinding grammar can name arrives as [`KeyIdentity::Key`].
/// Anything else keeps its reported codepoint rather than losing its identity:
/// Left Shift (`57441`), Menu (`57363`) and the media keys have no [`Key`]
/// form, and codepoint `0` is the value the Kitty keyboard protocol uses for
/// an event that carries only text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyIdentity {
    /// A key the keybinding grammar names.
    Key(Key),
    /// A reported codepoint with no [`Key`] form. `0` means the event carries
    /// only text.
    Codepoint(u32),
    /// A key the terminal named that has neither a [`Key`] form nor a
    /// reported codepoint, such as a function key above `F24`.
    Unnamed,
}

/// The codepoint the Kitty keyboard protocol uses when an event carries text
/// and no key.
pub const TEXT_ONLY_KEY_CODEPOINT: u32 = 0;

/// One complete keyboard event, holding everything the outer terminal
/// reported about a single key action.
///
/// [`KeyChord`] is the projection a keybinding matches on and drops what a
/// binding cannot name. This type drops nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInput {
    /// The key the event reports.
    pub key: KeyIdentity,
    /// Whether the key went down, repeated, or came up.
    pub key_event_kind: KeyEventKind,
    /// The character the key produces with Shift, when the terminal reports
    /// it. Shift+`a` on a US layout reports `'A'`.
    pub shifted_key: Option<char>,
    /// The character the physical key produces on the standard layout, when
    /// the terminal reports it. With a Dvorak layout active, the key labelled
    /// `Q` produces `'`, and this field holds `'q'`.
    pub base_layout_key: Option<char>,
    /// The text the key produced, empty when the event produced none.
    ///
    /// Holds more than one character when one key press produces several: `e`
    /// with a combining acute accent is `"e\u{301}"`. A control character is
    /// not text, so a key that reports one produces empty text here.
    pub associated_text: String,
    /// Every modifier the terminal reported, including Caps Lock and Num Lock.
    pub modifier_flags: KeyModifierFlags,
}

impl KeyInput {
    /// The chord a keybinding matches this event against, or `None` when no
    /// binding can name it.
    ///
    /// Returns `None` for a release, and for an event whose key has no
    /// [`Key`] form. A press and a repeat both produce a chord.
    ///
    /// The chord is the canonical form the config parser produces: `' '`
    /// becomes [`NamedKey::Space`], a capital folds to lowercase plus Shift,
    /// and a named key takes Shift as a modifier. `Ctrl+Shift+A` with Caps
    /// Lock held projects to `<C-S-a>`.
    ///
    /// With Shift held on a character key, a reported [`KeyInput::shifted_key`]
    /// replaces the key and consumes the Shift: key `'1'` with shifted key
    /// `'!'` projects to `!`, and key `'a'` with shifted key `'A'` projects to
    /// `<S-a>`.
    #[must_use]
    pub fn to_binding_chord(&self) -> Option<KeyChord> {
        if self.key_event_kind == KeyEventKind::Release {
            return None;
        }
        let KeyIdentity::Key(key) = self.key else {
            return None;
        };
        let binding_modifiers = self.modifier_flags.to_binding_modifiers();
        let is_shift_held = binding_modifiers.has_all_modifiers(ModFlags::SHIFT);
        let modifiers_without_shift = ModFlags(binding_modifiers.0 & !ModFlags::SHIFT.0);
        // A reported shifted character stands for the key itself: Shift plus
        // `1` reports `!`, and `!` is the character a binding names.
        if let (true, Key::Char(_), Some(shifted_character)) =
            (is_shift_held, key, self.shifted_key)
        {
            return Some(build_canonical_chord(
                Key::Char(shifted_character),
                modifiers_without_shift,
                false,
            ));
        }
        Some(build_canonical_chord(
            key,
            modifiers_without_shift,
            is_shift_held,
        ))
    }
}

/// The canonical chord for one key and the modifiers held with it.
///
/// `' '` becomes [`NamedKey::Space`]. A named key takes `is_shift_held` as a
/// modifier. A capital that [`fold_uppercase_character`] folds becomes
/// lowercase plus Shift; a lowercase letter takes `is_shift_held`; any other
/// character drops it, because a shifted `1` arrives as `!`.
#[must_use]
fn build_canonical_chord(
    input_key: Key,
    modifier_flags: ModFlags,
    is_shift_held: bool,
) -> KeyChord {
    let (normalized_key, is_shift_active) = match input_key {
        Key::Char(' ') => (Key::Named(NamedKey::Space), is_shift_held),
        Key::Named(_) => (input_key, is_shift_held),
        Key::Char(character) => {
            let (folded_character, was_shifted) = fold_uppercase_character(character);
            let is_shift_active = was_shifted || (folded_character.is_lowercase() && is_shift_held);
            (Key::Char(folded_character), is_shift_active)
        }
    };
    let modifier_flags = if is_shift_active {
        modifier_flags.union(ModFlags::SHIFT)
    } else {
        modifier_flags
    };
    KeyChord::from_parts(modifier_flags, normalized_key)
}

#[cfg(test)]
mod tests;
