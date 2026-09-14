//! Host keyboard boundary: the two halves of one key press.
//!
//! [`decode_key`] turns one host key event into a canonical [`KeyChord`]: one
//! key plus the modifiers held with it, such as `<C-a>`, in the form the keymap
//! matches keybindings against. [`encode_key_chord`] turns a chord back into the bytes a
//! program running inside a pane expects, for the keys no keybinding consumed.
//!
//! Encoding reads the chord and the receiving pane's application-cursor-keys
//! mode (DECCKM, `ESCAPE_BYTE [ ? 1 h`). A bare Up arrow is `ESCAPE_BYTE [ A` with the mode off
//! and `ESCAPE_BYTE O A` with it on; the chord `<Up>` is the same in both cases.
//!
//! # Byte forms
//!
//! The sequences are xterm's, the ones terminfo lists for every terminal
//! program (`kcuu1`, `kf1`, `kEND`, …):
//!
//! - A control character carries its modifiers in the byte itself: `Ctrl-a` is
//!   `0x01`, and Alt prefixes an `ESCAPE_BYTE` (`Alt-a` is `ESCAPE_BYTE a`).
//! - A cursor, editing, or function key carries them in a CSI parameter:
//!   `Ctrl-Right` is `ESCAPE_BYTE [ 1 ; 5 C`, where `5` = 1 + 4 (Control). Shift adds
//!   1, Alt 2, Control 4, Super 8.

use crate::host::{KeyCode as HostKey, KeyEvent, KeyEventKind, Modifiers};
use koshi_core::key::{fold_uppercase_character, Key, KeyChord, ModFlags, NamedKey};

/// The escape byte that opens every control sequence.
const ESCAPE_BYTE: u8 = 0x1b;

/// The modifier parameter with nothing held. The parameter is one plus a
/// bitmap of the held modifiers.
const UNMODIFIED_PARAMETER: u8 = 1;

/// Decode one press or repeat into its canonical chord.
///
/// Returns `None` for a release, for a function key above F24, and for a host
/// key added after this boundary that has no [`Key`] form. `BackTab` supplies
/// Shift even when the host flag is absent. Meta counts as Super; Hyper and
/// lock-state flags are dropped.
#[must_use]
pub fn decode_key(host_key_event: KeyEvent) -> Option<KeyChord> {
    if host_key_event.key_event_kind == KeyEventKind::Release {
        return None;
    }

    let host_modifiers = host_key_event.modifiers;
    let is_shift_held = host_modifiers.has_all_modifiers(Modifiers::SHIFT)
        || host_key_event.code == HostKey::BackTab;
    let decoded_key = decode_host_key(host_key_event.code)?;
    let modifier_flags = decode_modifiers(host_modifiers);
    Some(normalize_key_chord(
        decoded_key,
        modifier_flags,
        is_shift_held,
    ))
}

/// Encode a chord as the bytes the focused pane's program expects.
///
/// `is_application_cursor_keys_enabled` is the receiving pane's application-cursor-keys state
/// (DECCKM). With it on, an unmodified cursor key or Home/End opens with
/// `ESCAPE_BYTE O` in place of `ESCAPE_BYTE [`: `<Up>` is `ESCAPE_BYTE O A`. It changes no other key.
///
/// Every chord encodes to at least one byte.
///
/// Super rides along only where a sequence has room for it. A CSI key carries
/// Super in the modifier parameter, the same slot Shift and Control use:
/// `<D-Up>` → `ESCAPE_BYTE [ 1 ; 9 A`. A C0 key has room for Control and Alt only:
/// `<D-a>` reaches the pane as a plain `a`.
///
/// Shift splits the same way: it folds into the character (`<S-a>` → `A`),
/// and it rides the parameter on a named key (`<S-Up>` → `ESCAPE_BYTE [ 1 ; 2 A`).
///
/// # Panics
///
/// Panics when `chord.key` is `NamedKey::F(function_number)` with
/// `function_number` outside `1..=24`.
#[must_use]
pub fn encode_key_chord(chord: KeyChord, is_application_cursor_keys_enabled: bool) -> Vec<u8> {
    match chord.key {
        Key::Char(character) => encode_character(character, chord.modifier_flags),
        Key::Named(named_key) => encode_named_key(
            named_key,
            chord.modifier_flags,
            is_application_cursor_keys_enabled,
        ),
    }
}

/// The [`Key`] a host code stands for, or `None` for a code with no
/// [`Key`] form.
fn decode_host_key(host_key_code: HostKey) -> Option<Key> {
    let decoded_key = match host_key_code {
        HostKey::Char(character) => Key::Char(character),
        HostKey::Enter => Key::Named(NamedKey::Enter),
        HostKey::Backspace => Key::Named(NamedKey::Backspace),
        HostKey::Tab => Key::Named(NamedKey::Tab),
        HostKey::Escape => Key::Named(NamedKey::Esc),
        HostKey::Up => Key::Named(NamedKey::Up),
        HostKey::Down => Key::Named(NamedKey::Down),
        HostKey::Right => Key::Named(NamedKey::Right),
        HostKey::Left => Key::Named(NamedKey::Left),
        HostKey::Home => Key::Named(NamedKey::Home),
        HostKey::End => Key::Named(NamedKey::End),
        HostKey::Insert => Key::Named(NamedKey::Insert),
        HostKey::Delete => Key::Named(NamedKey::Delete),
        HostKey::PageUp => Key::Named(NamedKey::PageUp),
        HostKey::PageDown => Key::Named(NamedKey::PageDown),
        HostKey::BackTab => Key::Named(NamedKey::Tab),
        HostKey::Function(function_number @ 1..=24) => Key::Named(NamedKey::F(function_number)),
        HostKey::Function(_) => return None,
        HostKey::Unsupported => return None,
    };
    Some(decoded_key)
}

/// The host's Control, Alt and Super as [`ModFlags`]. Meta counts as Super;
/// Hyper is dropped. Shift is not carried: [`normalize_key_chord`] adds it for a key
/// press, and [`crate::mouse`] adds it for a mouse event.
pub(crate) fn decode_modifiers(host_modifiers: Modifiers) -> ModFlags {
    let mut modifier_flags = ModFlags::NONE;
    if host_modifiers.has_all_modifiers(Modifiers::CONTROL) {
        modifier_flags = modifier_flags.union(ModFlags::CTRL);
    }
    if host_modifiers.has_all_modifiers(Modifiers::ALT) {
        modifier_flags = modifier_flags.union(ModFlags::ALT);
    }
    if host_modifiers.has_all_modifiers(Modifiers::SUPER)
        || host_modifiers.has_all_modifiers(Modifiers::META)
    {
        modifier_flags = modifier_flags.union(ModFlags::SUPER);
    }
    modifier_flags
}

/// The canonical chord for one press, the form the config parser produces.
/// `is_shift_held` is the host's Shift state. `' '` becomes [`NamedKey::Space`].
/// A named key takes `shift_held` as a modifier. A capital that
/// [`fold_uppercase_character`] folds becomes lowercase plus Shift; a lowercase letter
/// takes `shift_held`; any other character drops it.
fn normalize_key_chord(input_key: Key, modifier_flags: ModFlags, is_shift_held: bool) -> KeyChord {
    let (normalized_key, is_shift_active) = match input_key {
        Key::Char(' ') => (Key::Named(NamedKey::Space), is_shift_held),
        Key::Named(_) => (input_key, is_shift_held),
        // An uppercase letter folds to lowercase plus Shift. A held Shift
        // counts only on a lowercase letter: a shifted `1` arrives as `!`.
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

/// A character key: Shift restores the capital, Control folds the character
/// into its C0 byte, and Alt prefixes `ESCAPE_BYTE`.
///
/// `<C-a>` → `0x01`. `<A-a>` → `ESCAPE_BYTE a`. `<A-C-a>` → `ESCAPE_BYTE 0x01`. `<S-a>` → `A`.
/// `<C-4>` → `0x1c`, one of the control codes the digit row carries (see
/// [`encode_control_byte`]). `<C-1>` → `1`: no control code stands for it, and the
/// character goes as itself.
fn encode_character(character: char, modifier_flags: ModFlags) -> Vec<u8> {
    let character = if modifier_flags.has_all_modifiers(ModFlags::SHIFT) {
        unfold_shift(character)
    } else {
        character
    };

    let mut encoded_bytes = Vec::new();
    if modifier_flags.has_all_modifiers(ModFlags::ALT) {
        encoded_bytes.push(ESCAPE_BYTE);
    }
    let control_byte = if modifier_flags.has_all_modifiers(ModFlags::CTRL) {
        encode_control_byte(character)
    } else {
        None
    };
    match control_byte {
        Some(control_byte) => encoded_bytes.push(control_byte),
        None => {
            let mut utf8_buffer = [0; 4];
            encoded_bytes.extend_from_slice(character.encode_utf8(&mut utf8_buffer).as_bytes());
        }
    }
    encoded_bytes
}

/// A named key: the C0 keys carry their modifiers in the byte itself, the
/// cursor, editing, and function keys in a control-sequence parameter.
fn encode_named_key(
    named_key: NamedKey,
    modifier_flags: ModFlags,
    is_application_cursor_keys_enabled: bool,
) -> Vec<u8> {
    let is_control_held = modifier_flags.has_all_modifiers(ModFlags::CTRL);
    let modifier_parameter = encode_modifier_parameter(modifier_flags);

    match named_key {
        NamedKey::Enter => encode_c0_key(b'\r', modifier_flags),
        NamedKey::Esc => encode_c0_key(ESCAPE_BYTE, modifier_flags),
        // Backspace sends DEL (`0x7f`), or BS (`0x08`) with Control held.
        NamedKey::Backspace => {
            encode_c0_key(if is_control_held { 0x08 } else { 0x7f }, modifier_flags)
        }
        NamedKey::Space => encode_c0_key(if is_control_held { 0x00 } else { b' ' }, modifier_flags),
        // Shift+Tab has a sequence of its own, with no modifier parameter:
        // `<S-Tab>` → `ESCAPE_BYTE [ Z`, `<A-S-Tab>` → `ESCAPE_BYTE ESCAPE_BYTE [ Z`, `<C-S-Tab>` →
        // `ESCAPE_BYTE [ Z`.
        NamedKey::Tab if modifier_flags.has_all_modifiers(ModFlags::SHIFT) => {
            if modifier_flags.has_all_modifiers(ModFlags::ALT) {
                vec![ESCAPE_BYTE, ESCAPE_BYTE, b'[', b'Z']
            } else {
                vec![ESCAPE_BYTE, b'[', b'Z']
            }
        }
        NamedKey::Tab => encode_c0_key(b'\t', modifier_flags),
        NamedKey::Up => {
            encode_cursor_key(b'A', modifier_parameter, is_application_cursor_keys_enabled)
        }
        NamedKey::Down => {
            encode_cursor_key(b'B', modifier_parameter, is_application_cursor_keys_enabled)
        }
        NamedKey::Right => {
            encode_cursor_key(b'C', modifier_parameter, is_application_cursor_keys_enabled)
        }
        NamedKey::Left => {
            encode_cursor_key(b'D', modifier_parameter, is_application_cursor_keys_enabled)
        }
        NamedKey::End => {
            encode_cursor_key(b'F', modifier_parameter, is_application_cursor_keys_enabled)
        }
        NamedKey::Home => {
            encode_cursor_key(b'H', modifier_parameter, is_application_cursor_keys_enabled)
        }
        NamedKey::Insert => encode_tilde_key(2, modifier_parameter),
        NamedKey::Delete => encode_tilde_key(3, modifier_parameter),
        NamedKey::PageUp => encode_tilde_key(5, modifier_parameter),
        NamedKey::PageDown => encode_tilde_key(6, modifier_parameter),
        NamedKey::F(function_number) => encode_function_key(function_number, modifier_flags),
    }
}

/// A C0 key's byte, with an `ESCAPE_BYTE` prefix when Alt is held. The caller folds
/// Control into `control_byte`; Shift and Super are dropped.
///
/// `Enter` → `\r`. `<A-CR>` → `ESCAPE_BYTE \r`.
fn encode_c0_key(control_byte: u8, modifier_flags: ModFlags) -> Vec<u8> {
    if modifier_flags.has_all_modifiers(ModFlags::ALT) {
        vec![ESCAPE_BYTE, control_byte]
    } else {
        vec![control_byte]
    }
}

/// A cursor or Home/End key. Unmodified, its introducer follows the pane's
/// DECCKM state — `ESCAPE_BYTE O A` in application mode, `ESCAPE_BYTE [ A` outside it. Any
/// modifier sends the CSI form in either mode.
///
/// `<Up>` → `ESCAPE_BYTE [ A`; `<Up>` into an application-mode pane → `ESCAPE_BYTE O A`;
/// `<C-Up>` → `ESCAPE_BYTE [ 1 ; 5 A` into either.
fn encode_cursor_key(
    final_byte: u8,
    modifier_parameter: u8,
    is_application_cursor_keys_enabled: bool,
) -> Vec<u8> {
    if modifier_parameter == UNMODIFIED_PARAMETER && !is_application_cursor_keys_enabled {
        return vec![ESCAPE_BYTE, b'[', final_byte];
    }
    encode_ss3_key(final_byte, modifier_parameter)
}

/// A key of the SS3 family — the `ESCAPE_BYTE O` introducer. Unmodified, the key is
/// `ESCAPE_BYTE O <final>`. A held modifier takes the CSI form
/// `ESCAPE_BYTE [ 1 ; <modifier_parameter> <final>`.
///
/// `<F1>` → `ESCAPE_BYTE O P`; `<C-F1>` → `ESCAPE_BYTE [ 1 ; 5 P`.
fn encode_ss3_key(final_byte: u8, modifier_parameter: u8) -> Vec<u8> {
    if modifier_parameter == UNMODIFIED_PARAMETER {
        return vec![ESCAPE_BYTE, b'O', final_byte];
    }
    // `ESCAPE_BYTE [ 1 ;` plus the modifier parameter and the final byte.
    let mut encoded_bytes = Vec::with_capacity(7);
    encoded_bytes.extend_from_slice(&[ESCAPE_BYTE, b'[', b'1', b';']);
    append_decimal(&mut encoded_bytes, modifier_parameter);
    encoded_bytes.push(final_byte);
    encoded_bytes
}

/// An editing or function key of the `ESCAPE_BYTE [ <code> ~` family, with its
/// modifier parameter when one is held.
///
/// `<Del>` → `ESCAPE_BYTE [ 3 ~`; `<C-Del>` → `ESCAPE_BYTE [ 3 ; 5 ~`.
fn encode_tilde_key(tilde_key_code: u8, modifier_parameter: u8) -> Vec<u8> {
    // `ESCAPE_BYTE [` plus the key code, an optional modifier parameter, and `~`.
    let mut encoded_bytes = Vec::with_capacity(8);
    encoded_bytes.extend_from_slice(&[ESCAPE_BYTE, b'[']);
    append_decimal(&mut encoded_bytes, tilde_key_code);
    if modifier_parameter != UNMODIFIED_PARAMETER {
        encoded_bytes.push(b';');
        append_decimal(&mut encoded_bytes, modifier_parameter);
    }
    encoded_bytes.push(b'~');
    encoded_bytes
}

/// Append a control sequence number — a key code or a modifier parameter — as
/// its decimal digits.
///
/// `3` appends `3`; `16` appends `1` then `6`; `100` appends `1`, `0`, `0`.
fn append_decimal(encoded_bytes: &mut Vec<u8>, decimal_number: u8) {
    if decimal_number >= 100 {
        encoded_bytes.push(b'0' + decimal_number / 100);
    }
    if decimal_number >= 10 {
        encoded_bytes.push(b'0' + decimal_number / 10 % 10);
    }
    encoded_bytes.push(b'0' + decimal_number % 10);
}

/// A function key. F1–F4 have sequences of their own (`ESCAPE_BYTE O P` … `ESCAPE_BYTE O S`,
/// and `ESCAPE_BYTE [ 1 ; <modifier_parameter> P` … once modified); F5–F12 join the `~` family
/// under the codes terminfo lists, whose run skips 16 and 22.
///
/// F13–F24 encode as Shift plus F1–F12: `<F13>` sends `ESCAPE_BYTE [ 1 ; 2 P`, which is
/// terminfo's `kf13`.
///
/// # Panics
///
/// Panics when `function_number` is `0`, or above `24`.
fn encode_function_key(function_number: u8, modifier_flags: ModFlags) -> Vec<u8> {
    let (function_number, modifier_flags) = if function_number > 12 {
        (function_number - 12, modifier_flags.union(ModFlags::SHIFT))
    } else {
        (function_number, modifier_flags)
    };
    let modifier_parameter = encode_modifier_parameter(modifier_flags);

    match function_number {
        // The four final bytes run in key order: `P`, `Q`, `R`, `S`.
        1..=4 => encode_ss3_key(b'P' + (function_number - 1), modifier_parameter),
        5 => encode_tilde_key(15, modifier_parameter),
        6..=9 => encode_tilde_key(11 + function_number, modifier_parameter),
        10 => encode_tilde_key(21, modifier_parameter),
        11 => encode_tilde_key(23, modifier_parameter),
        12 => encode_tilde_key(24, modifier_parameter),
        _ => unreachable!("decode_key and the chord parser both bound F to 1..=24"),
    }
}

/// The CSI parameter that carries a chord's modifiers: one plus a bitmap of
/// Shift (1), Alt (2), Control (4), and Super (8).
///
/// `<C-Right>` → `5` (1 + 4); that sequence reads `ESCAPE_BYTE [ 1 ; 5 C`.
fn encode_modifier_parameter(modifier_flags: ModFlags) -> u8 {
    let mut modifier_parameter = UNMODIFIED_PARAMETER;
    if modifier_flags.has_all_modifiers(ModFlags::SHIFT) {
        modifier_parameter += 1;
    }
    if modifier_flags.has_all_modifiers(ModFlags::ALT) {
        modifier_parameter += 2;
    }
    if modifier_flags.has_all_modifiers(ModFlags::CTRL) {
        modifier_parameter += 4;
    }
    if modifier_flags.has_all_modifiers(ModFlags::SUPER) {
        modifier_parameter += 8;
    }
    modifier_parameter
}

/// The capital a chord's Shift stands for: `'a'` → `'A'`. A character whose
/// uppercase mapping is more than one character (`'ß'` → `"SS"`) stands as it
/// is.
fn unfold_shift(character: char) -> char {
    let mut uppercase_characters = character.to_uppercase();
    match (uppercase_characters.next(), uppercase_characters.next()) {
        (Some(uppercase_character), None) => uppercase_character,
        _ => character,
    }
}

/// The C0 control byte Control plus this character sends, or `None` when no
/// control code stands for it. `'a'` → `0x01`; `'['` → `0x1b`; `'4'` → `0x1c`;
/// `'1'` → `None`.
///
/// `@` through `_` clear their top bits: `'A' & 0x1f` is `0x01`, and the 32
/// characters cover the 32 C0 codes. A lowercase letter sends its capital's
/// byte. `?` sends DEL.
///
/// The digit row sends the codes the letters do not: `2` sends NUL, `3` sends
/// ESCAPE_BYTE, `4`–`7` send `0x1c`–`0x1f`, and `8` sends DEL. One byte has two
/// spellings — `<C-4>` and `<C-\>` both send `0x1c` — and which one arrives
/// depends on the host:
///
/// - VT input maps `0x1c`–`0x1f` to `<C-4>` through `<C-7>`.
///   `0x00`, `0x1b`, and `0x7f` become Ctrl+Space, Esc, and Backspace.
/// - Enhanced keyboard input identifies the key directly: `Ctrl+4` arrives as
///   `<C-4>` and `Ctrl+\` as `<C-\>`.
fn encode_control_byte(character: char) -> Option<u8> {
    match character {
        '@'..='_' => Some((character as u8) & 0x1f),
        'a'..='z' => Some((character.to_ascii_uppercase() as u8) & 0x1f),
        '?' => Some(0x7f),
        '2' => Some(0x00),
        '3' => Some(0x1b),
        '4' => Some(0x1c),
        '5' => Some(0x1d),
        '6' => Some(0x1e),
        '7' => Some(0x1f),
        '8' => Some(0x7f),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
