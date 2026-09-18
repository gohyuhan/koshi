//! Host keyboard boundary: the two halves of one key press.
//!
//! [`decode_key_event`] turns one host key event into a [`KeyInput`], which
//! keeps every field the terminal reported. [`KeyInput::to_binding_chord`]
//! projects that event onto a canonical [`KeyChord`]: one key plus the
//! modifiers held with it, such as `<C-a>`, in the form the keymap matches
//! keybindings against. [`encode_key_chord`] turns a chord back into the bytes
//! a program running inside a pane expects, for the keys no keybinding
//! consumed.
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

use crate::host::{KeyCode as HostKey, KeyEvent, Modifiers};
use koshi_core::key::{
    ExtendedKeysMode, Key, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
    ModFlags, NamedKey, TEXT_ONLY_KEY_CODEPOINT,
};

/// The escape byte that opens every control sequence.
const ESCAPE_BYTE: u8 = 0x1b;

/// The modifier parameter with nothing held. The parameter is one plus a
/// bitmap of the held modifiers.
const UNMODIFIED_PARAMETER: u8 = 1;

/// Decode one host key event into the complete event Koshi stores.
///
/// Nothing the host reported is dropped: the key keeps its identity even when
/// no keybinding can name it, both alternatives and the associated text carry
/// through, and the modifier bitmap keeps all eight bits including Caps Lock
/// and Num Lock.
///
/// `BackTab` becomes Tab with Shift held, because the host reports Shift+Tab
/// as one key rather than as Tab plus a modifier.
///
/// `CSI 97:65;2u` becomes key `'a'`, shifted key `'A'`, kind
/// [`KeyEventKind::Press`], Shift held.
#[must_use]
pub fn decode_key_event(host_key_event: KeyEvent) -> KeyInput {
    let mut modifier_flags = host_key_event.modifiers.to_key_modifier_flags();
    if host_key_event.code == HostKey::BackTab {
        modifier_flags |= KeyModifierFlags::SHIFT;
    }
    KeyInput {
        key: decode_key_identity(host_key_event.code),
        key_event_kind: host_key_event.key_event_kind,
        shifted_key: host_key_event.shifted_key,
        base_layout_key: host_key_event.base_layout_key,
        associated_text: host_key_event.associated_text,
        modifier_flags,
    }
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

/// The stored identity a host code stands for.
///
/// A code Koshi names becomes [`KeyIdentity::Key`]. A code the terminal
/// reported by codepoint keeps that codepoint. A function key above `F24` has
/// neither form and becomes [`KeyIdentity::Unnamed`].
fn decode_key_identity(host_key_code: HostKey) -> KeyIdentity {
    let named_key = match host_key_code {
        HostKey::Char(character) => return KeyIdentity::Key(Key::Char(character)),
        HostKey::Codepoint(codepoint) => return KeyIdentity::Codepoint(codepoint),
        HostKey::Function(function_number @ 1..=24) => NamedKey::F(function_number),
        HostKey::Function(_) => return KeyIdentity::Unnamed,
        HostKey::Enter => NamedKey::Enter,
        HostKey::Backspace => NamedKey::Backspace,
        HostKey::Tab | HostKey::BackTab => NamedKey::Tab,
        HostKey::Escape => NamedKey::Esc,
        HostKey::Up => NamedKey::Up,
        HostKey::Down => NamedKey::Down,
        HostKey::Right => NamedKey::Right,
        HostKey::Left => NamedKey::Left,
        HostKey::Home => NamedKey::Home,
        HostKey::End => NamedKey::End,
        HostKey::Insert => NamedKey::Insert,
        HostKey::Delete => NamedKey::Delete,
        HostKey::PageUp => NamedKey::PageUp,
        HostKey::PageDown => NamedKey::PageDown,
    };
    KeyIdentity::Key(Key::Named(named_key))
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
        NamedKey::F(function_number) => encode_function_key(function_number, modifier_flags),
        named_key => match find_functional_key_form(named_key) {
            Some((FunctionalKeyForm::Cursor(final_byte), _)) => encode_cursor_key(
                final_byte,
                modifier_parameter,
                is_application_cursor_keys_enabled,
            ),
            Some((FunctionalKeyForm::Ss3(final_byte), _)) => {
                encode_ss3_key(final_byte, modifier_parameter)
            }
            Some((FunctionalKeyForm::Tilde(tilde_key_code), _)) => {
                encode_tilde_key(tilde_key_code, modifier_parameter)
            }
            None => unreachable!("the C0 keys and F keys are matched above"),
        },
    }
}

/// The control-sequence shape one functional key takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionalKeyForm {
    /// A cursor or Home/End key: `ESCAPE_BYTE [ <final>` unmodified outside
    /// application-cursor-keys mode, `ESCAPE_BYTE O <final>` inside it.
    Cursor(u8),
    /// An SS3 key: `ESCAPE_BYTE O <final>` unmodified, whatever the pane's
    /// cursor-key mode is.
    Ss3(u8),
    /// A key of the `ESCAPE_BYTE [ <code> ~` family.
    Tilde(u8),
}

/// The shape `named_key` takes, and the modifiers its encoding adds, or `None`
/// for a key that carries its modifiers in one byte: Enter, Tab, Backspace,
/// Esc and Space.
///
/// `NamedKey::Up` gives `Cursor(b'A')` and no modifier. `NamedKey::Delete`
/// gives `Tilde(3)`. `NamedKey::F(13)` gives `Ss3(b'P')` with
/// `ModFlags::SHIFT`, because F13 encodes as Shift plus F1.
///
/// # Panics
///
/// Panics when `named_key` is `NamedKey::F(function_number)` with
/// `function_number` outside `1..=24`.
fn find_functional_key_form(named_key: NamedKey) -> Option<(FunctionalKeyForm, ModFlags)> {
    let functional_key_form = match named_key {
        NamedKey::Up => FunctionalKeyForm::Cursor(b'A'),
        NamedKey::Down => FunctionalKeyForm::Cursor(b'B'),
        NamedKey::Right => FunctionalKeyForm::Cursor(b'C'),
        NamedKey::Left => FunctionalKeyForm::Cursor(b'D'),
        NamedKey::End => FunctionalKeyForm::Cursor(b'F'),
        NamedKey::Home => FunctionalKeyForm::Cursor(b'H'),
        NamedKey::Insert => FunctionalKeyForm::Tilde(2),
        NamedKey::Delete => FunctionalKeyForm::Tilde(3),
        NamedKey::PageUp => FunctionalKeyForm::Tilde(5),
        NamedKey::PageDown => FunctionalKeyForm::Tilde(6),
        NamedKey::F(function_number) => return Some(get_function_key_form(function_number)),
        NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace | NamedKey::Esc | NamedKey::Space => {
            return None
        }
    };
    Some((functional_key_form, ModFlags::NONE))
}

/// The shape function key `function_number` takes, and the modifiers its
/// encoding adds. F13 through F24 encode as Shift plus F1 through F12, so
/// `get_function_key_form(13)` gives the F1 shape and `ModFlags::SHIFT`.
///
/// # Panics
///
/// Panics when `function_number` is `0`, or above `24`.
fn get_function_key_form(function_number: u8) -> (FunctionalKeyForm, ModFlags) {
    let (function_number, added_modifier_flags) = if function_number > 12 {
        (function_number - 12, ModFlags::SHIFT)
    } else {
        (function_number, ModFlags::NONE)
    };
    let functional_key_form = match function_number {
        // The four final bytes run in key order: `P`, `Q`, `R`, `S`.
        1..=4 => FunctionalKeyForm::Ss3(b'P' + (function_number - 1)),
        5 => FunctionalKeyForm::Tilde(15),
        6..=9 => FunctionalKeyForm::Tilde(11 + function_number),
        10 => FunctionalKeyForm::Tilde(21),
        11 => FunctionalKeyForm::Tilde(23),
        12 => FunctionalKeyForm::Tilde(24),
        _ => unreachable!("decode_key and the chord parser both bound F to 1..=24"),
    };
    (functional_key_form, added_modifier_flags)
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
    append_decimal(&mut encoded_bytes, u32::from(modifier_parameter));
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
    append_decimal(&mut encoded_bytes, u32::from(tilde_key_code));
    if modifier_parameter != UNMODIFIED_PARAMETER {
        encoded_bytes.push(b';');
        append_decimal(&mut encoded_bytes, u32::from(modifier_parameter));
    }
    encoded_bytes.push(b'~');
    encoded_bytes
}

/// Append a control sequence number — a key code, a modifier parameter or a
/// codepoint — as its decimal digits.
///
/// `3` appends `3`; `16` appends `1` then `6`; `1114109` appends its seven
/// digits.
fn append_decimal(encoded_bytes: &mut Vec<u8>, decimal_number: u32) {
    let mut digit_divisor = 1;
    while decimal_number / digit_divisor >= 10 {
        digit_divisor *= 10;
    }
    let mut remaining_number = decimal_number;
    while digit_divisor > 0 {
        let digit = remaining_number / digit_divisor;
        encoded_bytes.push(b'0' + u8::try_from(digit).unwrap_or(0));
        remaining_number -= digit * digit_divisor;
        digit_divisor /= 10;
    }
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
    let (functional_key_form, added_modifier_flags) = get_function_key_form(function_number);
    let modifier_parameter = encode_modifier_parameter(modifier_flags.union(added_modifier_flags));
    match functional_key_form {
        FunctionalKeyForm::Ss3(final_byte) | FunctionalKeyForm::Cursor(final_byte) => {
            encode_ss3_key(final_byte, modifier_parameter)
        }
        FunctionalKeyForm::Tilde(tilde_key_code) => {
            encode_tilde_key(tilde_key_code, modifier_parameter)
        }
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

// ------------------------------------- the complete event, for one pane ----

/// Disambiguate escape codes: a key that produces no text takes an escape
/// code. Enter, Tab and Backspace keep their legacy bytes.
const DISAMBIGUATE_ESCAPE_CODES_FLAG: u8 = 1;
/// Report event types: a repeat and a release each name their kind.
const REPORT_EVENT_TYPES_FLAG: u8 = 2;
/// Report alternate keys: an escape-coded event names the shifted key and the
/// base-layout key.
const REPORT_ALTERNATE_KEYS_FLAG: u8 = 4;
/// Report all keys as escape codes: a text key and a modifier key each take an
/// escape code.
const REPORT_ALL_KEYS_FLAG: u8 = 8;
/// Report associated text: an escape-coded text key carries the text it
/// produced.
const REPORT_ASSOCIATED_TEXT_FLAG: u8 = 16;

/// The Kitty key number Enter reports.
const ENTER_KEY_NUMBER: u32 = 13;
/// The Kitty key number Tab reports.
const TAB_KEY_NUMBER: u32 = 9;
/// The Kitty key number Backspace reports.
const BACKSPACE_KEY_NUMBER: u32 = 127;
/// The Kitty key number Esc reports.
const ESC_KEY_NUMBER: u32 = 27;
/// The Kitty key number Space reports.
const SPACE_KEY_NUMBER: u32 = 32;

/// Encode one complete keyboard event as the bytes the receiving pane asked
/// for.
///
/// `keyboard_flags` is the pane's active screen's Kitty keyboard flags:
/// disambiguate (`1`), report event types (`2`), report alternate keys (`4`),
/// report all keys (`8`) and report associated text (`16`).
/// `is_application_cursor_keys_enabled` is that pane's DECCKM state.
/// `extended_keys_mode` is `terminal.extended-keys` from `koshi.kdl`.
///
/// Returns no bytes for an event the pane asked not to receive: a release
/// whose kind the report cannot name, a modifier key without flag `8`, and a
/// key the terminal named that has neither a key form nor a codepoint.
///
/// A report names a kind only for a key that takes an escape code, so typing
/// `a` writes `a` on a repeat and nothing on a release under flag `2` alone,
/// and writes `ESCAPE_BYTE [ 9 7 ; 1 : 2 u` and `ESCAPE_BYTE [ 9 7 ; 1 : 3 u`
/// under flags `2|8`.
///
/// With flags `0` and [`ExtendedKeysMode::OnRequest`], every event encodes as
/// [`encode_key_chord`] encodes its chord, and an event carrying text writes
/// that text: Shift+Enter writes `\r`, and Option+`a` reporting `å` writes
/// `å`.
///
/// With flag `8`, Shift+Enter writes `ESCAPE_BYTE [ 1 3 ; 2 u`. With
/// [`ExtendedKeysMode::Always`] and no flag, Shift+Enter writes the same bytes
/// and Tab still writes `\t`.
#[must_use]
pub fn encode_key_input(
    key_input: &KeyInput,
    keyboard_flags: u8,
    is_application_cursor_keys_enabled: bool,
    extended_keys_mode: ExtendedKeysMode,
) -> Vec<u8> {
    if !is_event_written(key_input, keyboard_flags) {
        return Vec::new();
    }
    match key_input.key {
        KeyIdentity::Unnamed => Vec::new(),
        KeyIdentity::Codepoint(TEXT_ONLY_KEY_CODEPOINT) => {
            encode_text_only_event(key_input, keyboard_flags)
        }
        KeyIdentity::Codepoint(codepoint) => {
            if keyboard_flags & REPORT_ALL_KEYS_FLAG == 0 {
                return Vec::new();
            }
            encode_csi_u_event(key_input, codepoint, keyboard_flags)
        }
        KeyIdentity::Key(Key::Named(named_key)) => match find_functional_key_form(named_key) {
            Some(functional_key_form) => encode_functional_key_event(
                key_input,
                functional_key_form,
                keyboard_flags,
                is_application_cursor_keys_enabled,
            ),
            None => encode_csi_u_or_legacy_event(
                key_input,
                Key::Named(named_key),
                keyboard_flags,
                is_application_cursor_keys_enabled,
                extended_keys_mode,
            ),
        },
        KeyIdentity::Key(key) => encode_csi_u_or_legacy_event(
            key_input,
            key,
            keyboard_flags,
            is_application_cursor_keys_enabled,
            extended_keys_mode,
        ),
    }
}

/// Whether the pane receives this event at all.
///
/// A press and a repeat are always written. A release is written only when the
/// report names its kind, so a pane that asked for no event kinds, and a key
/// that takes no escape code under the flags in force, both write nothing on
/// release.
fn is_event_written(key_input: &KeyInput, keyboard_flags: u8) -> bool {
    key_input.key_event_kind != KeyEventKind::Release
        || is_event_kind_reported(key_input, keyboard_flags)
}

/// Whether the report names this event's kind.
///
/// The pane must have asked for event kinds with flag `2`, and the key must
/// take an escape code, because legacy bytes have no field for a kind. Typing
/// `a` under flag `2` alone names no kind; under flags `2|8` it names `1:2`
/// for a repeat.
fn is_event_kind_reported(key_input: &KeyInput, keyboard_flags: u8) -> bool {
    keyboard_flags & REPORT_EVENT_TYPES_FLAG != 0 && is_key_escape_coded(key_input, keyboard_flags)
}

/// Whether this key takes an escape code under `keyboard_flags`.
///
/// Flag `8` escape-codes every key. A key that produces text, and Enter, Tab
/// and Backspace, keep their legacy bytes without it. A cursor, editing or
/// function key is a control sequence under every flag. Every other key takes
/// an escape code with flag `1`.
fn is_key_escape_coded(key_input: &KeyInput, keyboard_flags: u8) -> bool {
    if keyboard_flags & REPORT_ALL_KEYS_FLAG != 0 {
        return true;
    }
    if is_text_producing_event(key_input) || is_legacy_exception_key(key_input.key) {
        return false;
    }
    if let KeyIdentity::Key(Key::Named(named_key)) = key_input.key {
        if find_functional_key_form(named_key).is_some() {
            return true;
        }
    }
    keyboard_flags & DISAMBIGUATE_ESCAPE_CODES_FLAG != 0
}

/// Whether this event takes the `CSI u` form rather than its legacy bytes.
///
/// The flags decide it, and [`ExtendedKeysMode::Always`] adds the keys whose
/// legacy bytes another key also owns.
fn is_csi_u_written(
    key_input: &KeyInput,
    keyboard_flags: u8,
    extended_keys_mode: ExtendedKeysMode,
) -> bool {
    is_key_escape_coded(key_input, keyboard_flags)
        || (extended_keys_mode == ExtendedKeysMode::Always
            && is_key_lost_by_legacy_encoding(key_input))
}

/// Whether `key` is Enter, Tab or Backspace, the three keys that keep their
/// legacy bytes under flag `1`.
fn is_legacy_exception_key(key: KeyIdentity) -> bool {
    matches!(
        key,
        KeyIdentity::Key(Key::Named(
            NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace
        ))
    )
}

/// Whether the event produces text.
///
/// Reported text says so on its own, which also covers an event that carries
/// text and no key. A character key and Space produce text when no Control,
/// Alt or Super is held: `a` and Shift+`a` produce text, and Ctrl+`a` does
/// not.
fn is_text_producing_event(key_input: &KeyInput) -> bool {
    if !key_input.associated_text.is_empty() {
        return true;
    }
    let is_text_key = matches!(
        key_input.key,
        KeyIdentity::Key(Key::Char(_) | Key::Named(NamedKey::Space))
    );
    if !is_text_key {
        return false;
    }
    let binding_modifiers = key_input.modifier_flags.to_binding_modifiers();
    !binding_modifiers.has_all_modifiers(ModFlags::CTRL)
        && !binding_modifiers.has_all_modifiers(ModFlags::ALT)
        && !binding_modifiers.has_all_modifiers(ModFlags::SUPER)
}

/// Whether the legacy bytes for this event are bytes another key also sends.
///
/// The legacy encoding maps several keys onto one C0 byte. Ctrl+`i` sends
/// `0x09`, which Tab sends; Shift+Enter sends `\r`, which Enter sends. Both
/// lose which key was pressed, so both are true here. Shift+Tab sends
/// `ESCAPE_BYTE [ Z`, which only Shift+Tab sends, so it is false.
fn is_key_lost_by_legacy_encoding(key_input: &KeyInput) -> bool {
    let Some(chord) = key_input.to_binding_chord() else {
        return false;
    };
    let legacy_bytes = encode_key_chord(chord, false);
    let Some(owner_key) = find_c0_byte_owner(&legacy_bytes) else {
        return false;
    };
    if owner_key != chord.key {
        return true;
    }
    // The owner's own key loses a modifier when the modifier changes nothing:
    // Shift+Enter and Enter both send `\r`.
    let unmodified_bytes = encode_key_chord(KeyChord::from_parts(ModFlags::NONE, chord.key), false);
    chord.modifier_flags != ModFlags::NONE && legacy_bytes == unmodified_bytes
}

/// The key that owns one C0 byte, or `None` when the bytes are not a C0 byte
/// two keys share.
///
/// `0x09` belongs to Tab, `0x0d` to Enter, `0x1b` to Esc, `0x7f` to Backspace,
/// `0x00` to Space and `0x08` to `h`.
fn find_c0_byte_owner(legacy_bytes: &[u8]) -> Option<Key> {
    let [single_byte] = legacy_bytes else {
        return None;
    };
    let owner_key = match single_byte {
        0x09 => Key::Named(NamedKey::Tab),
        0x0d => Key::Named(NamedKey::Enter),
        0x1b => Key::Named(NamedKey::Esc),
        0x7f => Key::Named(NamedKey::Backspace),
        0x00 => Key::Named(NamedKey::Space),
        0x08 => Key::Char('h'),
        _ => return None,
    };
    Some(owner_key)
}

/// The key number a `CSI u` report names `key` by.
///
/// The five C0 keys take the codepoints of the bytes they send: Enter `13`,
/// Tab `9`, Backspace `127`, Esc `27` and Space `32`. A character key takes
/// its own codepoint.
fn get_csi_u_key_number(key: Key) -> u32 {
    match key {
        Key::Char(character) => character as u32,
        Key::Named(NamedKey::Enter) => ENTER_KEY_NUMBER,
        Key::Named(NamedKey::Tab) => TAB_KEY_NUMBER,
        Key::Named(NamedKey::Backspace) => BACKSPACE_KEY_NUMBER,
        Key::Named(NamedKey::Esc) => ESC_KEY_NUMBER,
        Key::Named(NamedKey::Space) => SPACE_KEY_NUMBER,
        Key::Named(_) => unreachable!("a functional key is encoded by its own form"),
    }
}

/// The legacy bytes for one event: the text it produced, or the bytes its
/// chord encodes to.
///
/// Reported text wins, so Option+`a` reporting `å` writes `å` rather than
/// `ESCAPE_BYTE a`. An event no chord can name writes nothing.
fn encode_legacy_event(key_input: &KeyInput, is_application_cursor_keys_enabled: bool) -> Vec<u8> {
    if !key_input.associated_text.is_empty() {
        return key_input.associated_text.as_bytes().to_vec();
    }
    match key_input.to_binding_chord() {
        Some(chord) => encode_key_chord(chord, is_application_cursor_keys_enabled),
        None => Vec::new(),
    }
}

/// A key with one legacy byte: Enter, Tab, Backspace, Esc, Space, or a
/// character key. The flags decide between its `CSI u` report and those bytes.
///
/// Shift+Enter gives `\r` with no flag, and `ESCAPE_BYTE [ 1 3 ; 2 u` with
/// flag `8`.
fn encode_csi_u_or_legacy_event(
    key_input: &KeyInput,
    key: Key,
    keyboard_flags: u8,
    is_application_cursor_keys_enabled: bool,
    extended_keys_mode: ExtendedKeysMode,
) -> Vec<u8> {
    if is_csi_u_written(key_input, keyboard_flags, extended_keys_mode) {
        return encode_csi_u_event(key_input, get_csi_u_key_number(key), keyboard_flags);
    }
    encode_legacy_event(key_input, is_application_cursor_keys_enabled)
}

/// A cursor, editing or function key. It keeps its canonical form under every
/// flag; a reported event kind rides its modifier field.
///
/// `<Up>` gives `ESCAPE_BYTE [ A`. An Up release under flag `2` gives
/// `ESCAPE_BYTE [ 1 ; 1 : 3 A`.
fn encode_functional_key_event(
    key_input: &KeyInput,
    functional_key_form: (FunctionalKeyForm, ModFlags),
    keyboard_flags: u8,
    is_application_cursor_keys_enabled: bool,
) -> Vec<u8> {
    let Some(event_kind_number) = find_event_kind_number(key_input, keyboard_flags) else {
        return encode_legacy_event(key_input, is_application_cursor_keys_enabled);
    };
    let (functional_key_form, added_modifier_flags) = functional_key_form;
    let mut modifier_flags = key_input.modifier_flags;
    if added_modifier_flags.has_all_modifiers(ModFlags::SHIFT) {
        modifier_flags = modifier_flags.union(KeyModifierFlags::SHIFT);
    }

    let mut encoded_bytes = vec![ESCAPE_BYTE, b'['];
    let key_parameter = match functional_key_form {
        FunctionalKeyForm::Cursor(_) | FunctionalKeyForm::Ss3(_) => 1,
        FunctionalKeyForm::Tilde(tilde_key_code) => u32::from(tilde_key_code),
    };
    append_decimal(&mut encoded_bytes, key_parameter);
    encoded_bytes.push(b';');
    append_decimal(
        &mut encoded_bytes,
        u32::from(modifier_flags.to_kitty_parameter()),
    );
    encoded_bytes.push(b':');
    append_decimal(&mut encoded_bytes, event_kind_number);
    match functional_key_form {
        FunctionalKeyForm::Cursor(final_byte) | FunctionalKeyForm::Ss3(final_byte) => {
            encoded_bytes.push(final_byte);
        }
        FunctionalKeyForm::Tilde(_) => encoded_bytes.push(b'~'),
    }
    encoded_bytes
}

/// An event that carries text and no key: `ESCAPE_BYTE [ 0 ; ; <codepoints> u`
/// with flags `8` and `16`, and the text itself otherwise.
fn encode_text_only_event(key_input: &KeyInput, keyboard_flags: u8) -> Vec<u8> {
    if keyboard_flags & REPORT_ALL_KEYS_FLAG == 0
        || keyboard_flags & REPORT_ASSOCIATED_TEXT_FLAG == 0
    {
        return key_input.associated_text.as_bytes().to_vec();
    }
    encode_csi_u_event(key_input, TEXT_ONLY_KEY_CODEPOINT, keyboard_flags)
}

/// One `CSI u` report:
/// `ESCAPE_BYTE [ <number>[:<shifted>[:<base>]] [; <modifiers>[:<kind>]] [; <text>] u`.
///
/// The modifier field is left out when no modifier is held, no kind is
/// reported and no text follows. It is left empty when text follows and no
/// modifier is held: a text-only `å` gives `ESCAPE_BYTE [ 0 ; ; 2 2 9 u`.
fn encode_csi_u_event(key_input: &KeyInput, key_number: u32, keyboard_flags: u8) -> Vec<u8> {
    let mut encoded_bytes = vec![ESCAPE_BYTE, b'['];
    append_decimal(&mut encoded_bytes, key_number);
    if keyboard_flags & REPORT_ALTERNATE_KEYS_FLAG != 0 {
        append_alternate_keys(&mut encoded_bytes, key_input);
    }

    let text_codepoints = get_reported_text_codepoints(key_input, keyboard_flags);
    let event_kind_number = find_event_kind_number(key_input, keyboard_flags);
    let modifier_parameter = get_reported_modifier_parameter(key_input);
    let is_modifier_field_needed =
        modifier_parameter != 1 || event_kind_number.is_some() || !text_codepoints.is_empty();
    if is_modifier_field_needed {
        encoded_bytes.push(b';');
        if modifier_parameter != 1 || event_kind_number.is_some() {
            append_decimal(&mut encoded_bytes, u32::from(modifier_parameter));
        }
        if let Some(event_kind_number) = event_kind_number {
            encoded_bytes.push(b':');
            append_decimal(&mut encoded_bytes, event_kind_number);
        }
    }
    if !text_codepoints.is_empty() {
        encoded_bytes.push(b';');
        for (codepoint_index, text_codepoint) in text_codepoints.iter().enumerate() {
            if codepoint_index > 0 {
                encoded_bytes.push(b':');
            }
            append_decimal(&mut encoded_bytes, *text_codepoint);
        }
    }
    encoded_bytes.push(b'u');
    encoded_bytes
}

/// Append `:shifted`, `::base` or `:shifted:base` for the alternatives the
/// event reported.
///
/// The shifted key is appended only with Shift held. A base-layout key equal
/// to the key itself is left out.
fn append_alternate_keys(encoded_bytes: &mut Vec<u8>, key_input: &KeyInput) {
    let is_shift_held = key_input
        .modifier_flags
        .has_all_modifiers(KeyModifierFlags::SHIFT);
    let shifted_key = key_input.shifted_key.filter(|_| is_shift_held);
    let base_layout_key = key_input
        .base_layout_key
        .filter(|base_layout_key| KeyIdentity::Key(Key::Char(*base_layout_key)) != key_input.key);
    if shifted_key.is_none() && base_layout_key.is_none() {
        return;
    }
    encoded_bytes.push(b':');
    if let Some(shifted_key) = shifted_key {
        append_decimal(encoded_bytes, shifted_key as u32);
    }
    if let Some(base_layout_key) = base_layout_key {
        encoded_bytes.push(b':');
        append_decimal(encoded_bytes, base_layout_key as u32);
    }
}

/// The event kind a report names, or `None` when the report names none.
///
/// A press names none. A repeat names `2` and a release names `3`, both only
/// with flag `2`.
fn find_event_kind_number(key_input: &KeyInput, keyboard_flags: u8) -> Option<u32> {
    if !is_event_kind_reported(key_input, keyboard_flags) {
        return None;
    }
    match key_input.key_event_kind {
        KeyEventKind::Press => None,
        KeyEventKind::Repeat => Some(2),
        KeyEventKind::Release => Some(3),
    }
}

/// The codepoints of the text a report carries, empty when it carries none.
///
/// Text rides a report only with flags `8` and `16` together.
fn get_reported_text_codepoints(key_input: &KeyInput, keyboard_flags: u8) -> Vec<u32> {
    if keyboard_flags & REPORT_ALL_KEYS_FLAG == 0
        || keyboard_flags & REPORT_ASSOCIATED_TEXT_FLAG == 0
    {
        return Vec::new();
    }
    key_input
        .associated_text
        .chars()
        .map(|text_character| text_character as u32)
        .collect()
}

/// The modifier parameter a report names: one plus the reported bitmap.
///
/// Caps Lock and Num Lock are dropped from an event that produces text, so
/// typing `a` with Caps Lock held gives `1`. Every modifier held on a key that
/// produces no text gives `256`.
fn get_reported_modifier_parameter(key_input: &KeyInput) -> u16 {
    let mut modifier_flags = key_input.modifier_flags;
    if is_text_producing_event(key_input) {
        let lock_modifiers = KeyModifierFlags::CAPS_LOCK
            .union(KeyModifierFlags::NUM_LOCK)
            .bits();
        modifier_flags = KeyModifierFlags::from_bits(modifier_flags.bits() & !lock_modifiers);
    }
    modifier_flags.to_kitty_parameter()
}

#[cfg(test)]
mod tests;
