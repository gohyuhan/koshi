//! Host keyboard boundary: the two halves of one key press.
//!
//! [`decode_key`] turns one host key event into a canonical [`KeyChord`]: one
//! key plus the modifiers held with it, such as `<C-a>`, in the form the keymap
//! matches keybindings against. [`encode`] turns a chord back into the bytes a
//! program running inside a pane expects, for the keys no keybinding consumed.
//!
//! Encoding reads the chord and the receiving pane's application-cursor-keys
//! mode (DECCKM, `ESC [ ? 1 h`). A bare Up arrow is `ESC [ A` with the mode off
//! and `ESC O A` with it on; the chord `<Up>` is the same in both cases.
//!
//! # Byte forms
//!
//! The sequences are xterm's, the ones terminfo lists for every terminal
//! program (`kcuu1`, `kf1`, `kEND`, …):
//!
//! - A control character carries its modifiers in the byte itself: `Ctrl-a` is
//!   `0x01`, and Alt prefixes an `ESC` (`Alt-a` is `ESC a`).
//! - A cursor, editing, or function key carries them in a CSI parameter:
//!   `Ctrl-Right` is `ESC [ 1 ; 5 C`, where `5` = 1 + 4 (Control). Shift adds
//!   1, Alt 2, Control 4, Super 8.

use crate::host::{KeyCode as HostKey, KeyEvent, KeyEventKind, Modifiers};
use koshi_core::key::{fold_uppercase, Key, KeyChord, ModFlags, NamedKey};

/// The escape byte that opens every control sequence.
const ESC: u8 = 0x1b;

/// The modifier parameter with nothing held. The parameter is one plus a
/// bitmap of the held modifiers.
const UNMODIFIED: u8 = 1;

/// Decode one press or repeat into its canonical chord.
///
/// Returns `None` for a release, for a function key above F24, and for a host
/// key added after this boundary that has no [`Key`] form. `BackTab` supplies
/// Shift even when the host flag is absent. Meta counts as Super; Hyper and
/// lock-state flags are dropped.
#[must_use]
pub fn decode_key(event: KeyEvent) -> Option<KeyChord> {
    if event.kind == KeyEventKind::Release {
        return None;
    }

    let modifiers = event.modifiers;
    let shift_held = modifiers.contains(Modifiers::SHIFT) || event.code == HostKey::BackTab;
    let key = decode_code(event.code)?;
    let mods = decode_mods(modifiers);
    Some(normalize(key, mods, shift_held))
}

/// Encode a chord as the bytes the focused pane's program expects.
///
/// `app_cursor_keys` is the receiving pane's application-cursor-keys state
/// (DECCKM). With it on, an unmodified cursor key or Home/End opens with
/// `ESC O` in place of `ESC [`: `<Up>` is `ESC O A`. It changes no other key.
///
/// Every chord encodes to at least one byte.
///
/// Super rides along only where a sequence has room for it. A CSI key carries
/// Super in the modifier parameter, the same slot Shift and Control use:
/// `<D-Up>` → `ESC [ 1 ; 9 A`. A C0 key has room for Control and Alt only:
/// `<D-a>` reaches the pane as a plain `a`.
///
/// Shift splits the same way: it folds into the character (`<S-a>` → `A`),
/// and it rides the parameter on a named key (`<S-Up>` → `ESC [ 1 ; 2 A`).
///
/// # Panics
///
/// Panics when `chord.key` is `NamedKey::F(n)` with `n` outside `1..=24`.
#[must_use]
pub fn encode(chord: KeyChord, app_cursor_keys: bool) -> Vec<u8> {
    match chord.key {
        Key::Char(c) => encode_char(c, chord.mods),
        Key::Named(key) => encode_named(key, chord.mods, app_cursor_keys),
    }
}

/// The [`Key`] a host code stands for, or `None` for a code with no
/// [`Key`] form.
fn decode_code(code: HostKey) -> Option<Key> {
    let key = match code {
        HostKey::Char(c) => Key::Char(c),
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
        HostKey::Function(n @ 1..=24) => Key::Named(NamedKey::F(n)),
        HostKey::Function(_) => return None,
        HostKey::Unsupported => return None,
    };
    Some(key)
}

/// The host's Control, Alt and Super as [`ModFlags`]. Meta counts as Super;
/// Hyper is dropped. Shift is not carried: [`normalize`] adds it for a key
/// press, and [`crate::mouse`] adds it for a mouse event.
pub(crate) fn decode_mods(modifiers: Modifiers) -> ModFlags {
    let mut mods = ModFlags::NONE;
    if modifiers.contains(Modifiers::CONTROL) {
        mods = mods.union(ModFlags::CTRL);
    }
    if modifiers.contains(Modifiers::ALT) {
        mods = mods.union(ModFlags::ALT);
    }
    if modifiers.contains(Modifiers::SUPER) || modifiers.contains(Modifiers::META) {
        mods = mods.union(ModFlags::SUPER);
    }
    mods
}

/// The canonical chord for one press, the form the config parser produces.
/// `shift_held` is the host's Shift state. `' '` becomes [`NamedKey::Space`].
/// A named key takes `shift_held` as a modifier. A capital that
/// [`fold_uppercase`] folds becomes lowercase plus Shift; a lowercase letter
/// takes `shift_held`; any other character drops it.
fn normalize(key: Key, mods: ModFlags, shift_held: bool) -> KeyChord {
    let (key, shift) = match key {
        Key::Char(' ') => (Key::Named(NamedKey::Space), shift_held),
        Key::Named(_) => (key, shift_held),
        // An uppercase letter folds to lowercase plus Shift. A held Shift
        // counts only on a lowercase letter: a shifted `1` arrives as `!`.
        Key::Char(c) => {
            let (folded, shifted) = fold_uppercase(c);
            let shift = shifted || (folded.is_lowercase() && shift_held);
            (Key::Char(folded), shift)
        }
    };
    let mods = if shift {
        mods.union(ModFlags::SHIFT)
    } else {
        mods
    };
    KeyChord::new(mods, key)
}

/// A character key: Shift restores the capital, Control folds the character
/// into its C0 byte, and Alt prefixes `ESC`.
///
/// `<C-a>` → `0x01`. `<A-a>` → `ESC a`. `<A-C-a>` → `ESC 0x01`. `<S-a>` → `A`.
/// `<C-4>` → `0x1c`, one of the control codes the digit row carries (see
/// [`control_byte`]). `<C-1>` → `1`: no control code stands for it, and the
/// character goes as itself.
fn encode_char(c: char, mods: ModFlags) -> Vec<u8> {
    let c = if mods.contains(ModFlags::SHIFT) {
        unfold_shift(c)
    } else {
        c
    };

    let mut bytes = Vec::new();
    if mods.contains(ModFlags::ALT) {
        bytes.push(ESC);
    }
    let control = if mods.contains(ModFlags::CTRL) {
        control_byte(c)
    } else {
        None
    };
    match control {
        Some(byte) => bytes.push(byte),
        None => {
            let mut buf = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    bytes
}

/// A named key: the C0 keys carry their modifiers in the byte itself, the
/// cursor, editing, and function keys in a control-sequence parameter.
fn encode_named(key: NamedKey, mods: ModFlags, app_cursor_keys: bool) -> Vec<u8> {
    let ctrl = mods.contains(ModFlags::CTRL);
    let param = modifier_param(mods);

    match key {
        NamedKey::Enter => c0(b'\r', mods),
        NamedKey::Esc => c0(ESC, mods),
        // Backspace sends DEL (`0x7f`), or BS (`0x08`) with Control held.
        NamedKey::Backspace => c0(if ctrl { 0x08 } else { 0x7f }, mods),
        NamedKey::Space => c0(if ctrl { 0x00 } else { b' ' }, mods),
        // Shift+Tab has a sequence of its own, with no modifier parameter:
        // `<S-Tab>` → `ESC [ Z`, `<A-S-Tab>` → `ESC ESC [ Z`, `<C-S-Tab>` →
        // `ESC [ Z`.
        NamedKey::Tab if mods.contains(ModFlags::SHIFT) => {
            if mods.contains(ModFlags::ALT) {
                vec![ESC, ESC, b'[', b'Z']
            } else {
                vec![ESC, b'[', b'Z']
            }
        }
        NamedKey::Tab => c0(b'\t', mods),
        NamedKey::Up => cursor_key(b'A', param, app_cursor_keys),
        NamedKey::Down => cursor_key(b'B', param, app_cursor_keys),
        NamedKey::Right => cursor_key(b'C', param, app_cursor_keys),
        NamedKey::Left => cursor_key(b'D', param, app_cursor_keys),
        NamedKey::End => cursor_key(b'F', param, app_cursor_keys),
        NamedKey::Home => cursor_key(b'H', param, app_cursor_keys),
        NamedKey::Insert => tilde(2, param),
        NamedKey::Delete => tilde(3, param),
        NamedKey::PageUp => tilde(5, param),
        NamedKey::PageDown => tilde(6, param),
        NamedKey::F(n) => function_key(n, mods),
    }
}

/// A C0 key's byte, with an `ESC` prefix when Alt is held. The caller folds
/// Control into `byte`; Shift and Super are dropped.
///
/// `Enter` → `\r`. `<A-CR>` → `ESC \r`.
fn c0(byte: u8, mods: ModFlags) -> Vec<u8> {
    if mods.contains(ModFlags::ALT) {
        vec![ESC, byte]
    } else {
        vec![byte]
    }
}

/// A cursor or Home/End key. Unmodified, its introducer follows the pane's
/// DECCKM state — `ESC O A` in application mode, `ESC [ A` outside it. Any
/// modifier sends the CSI form in either mode.
///
/// `<Up>` → `ESC [ A`; `<Up>` into an application-mode pane → `ESC O A`;
/// `<C-Up>` → `ESC [ 1 ; 5 A` into either.
fn cursor_key(final_byte: u8, param: u8, app_cursor_keys: bool) -> Vec<u8> {
    if param == UNMODIFIED && !app_cursor_keys {
        return vec![ESC, b'[', final_byte];
    }
    ss3_key(final_byte, param)
}

/// A key of the SS3 family — the `ESC O` introducer. Unmodified, the key is
/// `ESC O <final>`. A held modifier takes the CSI form
/// `ESC [ 1 ; <param> <final>`.
///
/// `<F1>` → `ESC O P`; `<C-F1>` → `ESC [ 1 ; 5 P`.
fn ss3_key(final_byte: u8, param: u8) -> Vec<u8> {
    if param == UNMODIFIED {
        return vec![ESC, b'O', final_byte];
    }
    // `ESC [ 1 ;` plus a two-digit parameter and the final byte.
    let mut bytes = Vec::with_capacity(7);
    bytes.extend_from_slice(&[ESC, b'[', b'1', b';']);
    push_decimal(&mut bytes, param);
    bytes.push(final_byte);
    bytes
}

/// An editing or function key of the `ESC [ <code> ~` family, with its
/// modifier parameter when one is held.
///
/// `<Del>` → `ESC [ 3 ~`; `<C-Del>` → `ESC [ 3 ; 5 ~`.
fn tilde(code: u8, param: u8) -> Vec<u8> {
    // `ESC [` plus a two-digit code, `;`, a two-digit parameter, and `~`.
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&[ESC, b'[']);
    push_decimal(&mut bytes, code);
    if param != UNMODIFIED {
        bytes.push(b';');
        push_decimal(&mut bytes, param);
    }
    bytes.push(b'~');
    bytes
}

/// Append a control sequence number — a key code or a modifier parameter — as
/// its decimal digits.
///
/// `3` appends `3`; `16` appends `1` then `6`; `100` appends `1`, `0`, `0`.
fn push_decimal(bytes: &mut Vec<u8>, value: u8) {
    if value >= 100 {
        bytes.push(b'0' + value / 100);
    }
    if value >= 10 {
        bytes.push(b'0' + value / 10 % 10);
    }
    bytes.push(b'0' + value % 10);
}

/// A function key. F1–F4 have sequences of their own (`ESC O P` … `ESC O S`,
/// and `ESC [ 1 ; <param> P` … once modified); F5–F12 join the `~` family
/// under the codes terminfo lists, whose run skips 16 and 22.
///
/// F13–F24 encode as Shift plus F1–F12: `<F13>` sends `ESC [ 1 ; 2 P`, which is
/// terminfo's `kf13`.
///
/// # Panics
///
/// Panics when `n` is `0`, or above `24`.
fn function_key(n: u8, mods: ModFlags) -> Vec<u8> {
    let (n, mods) = if n > 12 {
        (n - 12, mods.union(ModFlags::SHIFT))
    } else {
        (n, mods)
    };
    let param = modifier_param(mods);

    match n {
        // The four final bytes run in key order: `P`, `Q`, `R`, `S`.
        1..=4 => ss3_key(b'P' + (n - 1), param),
        5 => tilde(15, param),
        6..=9 => tilde(11 + n, param),
        10 => tilde(21, param),
        11 => tilde(23, param),
        12 => tilde(24, param),
        _ => unreachable!("decode_key and the chord parser both bound F to 1..=24"),
    }
}

/// The CSI parameter that carries a chord's modifiers: one plus a bitmap of
/// Shift (1), Alt (2), Control (4), and Super (8).
///
/// `<C-Right>` → `5` (1 + 4); that sequence reads `ESC [ 1 ; 5 C`.
fn modifier_param(mods: ModFlags) -> u8 {
    let mut param = UNMODIFIED;
    if mods.contains(ModFlags::SHIFT) {
        param += 1;
    }
    if mods.contains(ModFlags::ALT) {
        param += 2;
    }
    if mods.contains(ModFlags::CTRL) {
        param += 4;
    }
    if mods.contains(ModFlags::SUPER) {
        param += 8;
    }
    param
}

/// The capital a chord's Shift stands for: `'a'` → `'A'`. A character whose
/// uppercase mapping is more than one character (`'ß'` → `"SS"`) stands as it
/// is.
fn unfold_shift(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u,
        _ => c,
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
/// ESC, `4`–`7` send `0x1c`–`0x1f`, and `8` sends DEL. One byte has two
/// spellings — `<C-4>` and `<C-\>` both send `0x1c` — and which one arrives
/// depends on the host:
///
/// - VT input maps `0x1c`–`0x1f` to `<C-4>` through `<C-7>`.
///   `0x00`, `0x1b`, and `0x7f` become Ctrl+Space, Esc, and Backspace.
/// - Enhanced keyboard input identifies the key directly: `Ctrl+4` arrives as
///   `<C-4>` and `Ctrl+\` as `<C-\>`.
fn control_byte(c: char) -> Option<u8> {
    match c {
        '@'..='_' => Some((c as u8) & 0x1f),
        'a'..='z' => Some((c.to_ascii_uppercase() as u8) & 0x1f),
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
