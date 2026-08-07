//! Crossterm keyboard boundary: the two halves of one key press.
//!
//! [`decode_key`] turns one host key event into a canonical [`KeyChord`]. A
//! chord is one key plus the modifiers held with it, such as `<C-a>`. The chord
//! is the form the keymap matches keybindings against. [`encode`] turns a chord
//! back into the bytes a program running inside a pane expects, for the keys no
//! keybinding consumed.
//!
//! Encoding depends on the chord *and* the receiving pane's mode, so it happens
//! where the bytes are written. A bare Up arrow is `ESC [ A` to a shell but
//! `ESC O A` to vim, which turned on application-cursor-keys mode (DECCKM,
//! `ESC [ ? 1 h`). The chord `<Up>` is the same press in both cases; only the
//! bytes differ.
//!
//! # Byte forms
//!
//! The sequences follow the terminfo capabilities every terminal program reads
//! (`kcuu1`, `kf1`, `kEND`, …), which is xterm's table:
//!
//! - Control characters carry their modifiers in the byte itself: `Ctrl-a` is
//!   `0x01`, and Alt prefixes an `ESC` (`Alt-a` is `ESC a`).
//! - Cursor and editing keys carry theirs in a CSI parameter: `Ctrl-Right` is
//!   `ESC [ 1 ; 5 C`, where `5` = 1 + 4 (Control). Shift adds 1, Alt 2,
//!   Control 4, Super 8.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use koshi_core::key::{fold_uppercase, Key, KeyChord, ModFlags, NamedKey};

/// The escape byte that opens every control sequence.
const ESC: u8 = 0x1b;

/// The value a modifier parameter takes when nothing is held: the parameter is
/// a bitmap of the held modifiers offset by one.
const UNMODIFIED: u8 = 1;

/// Decode one press or repeat into its canonical chord. Releases yield `None`,
/// so one physical press is delivered once, as do keys this model has no name
/// for: media keys, `CapsLock`, `Menu`.
#[must_use]
pub fn decode_key(event: KeyEvent) -> Option<KeyChord> {
    if matches!(event.kind, KeyEventKind::Release) {
        return None;
    }

    let key = decode_code(event.code)?;
    let mut mods = decode_mods(event.modifiers);
    // BackTab IS Shift+Tab: some hosts report the key without also setting the
    // Shift modifier, so the chord carries it unconditionally.
    if matches!(event.code, KeyCode::BackTab) {
        mods = mods.union(ModFlags::SHIFT);
    }
    Some(normalize(key, mods, event.modifiers))
}

/// Encode a chord as the bytes the focused pane's program expects.
///
/// `app_cursor_keys` is the receiving pane's application-cursor-keys state
/// (DECCKM): an unmodified cursor key is `ESC O A` while it is on and
/// `ESC [ A` while it is off. It changes no other key.
///
/// Every chord encodes to something.
///
/// Super rides along only where a sequence has room for it. A CSI key carries
/// Super in the modifier parameter, the same slot Shift and Control use:
/// `<D-Up>` → `ESC [ 1 ; 9 A`. A C0 key has room for Control and Alt only, so
/// `<D-a>` reaches the pane as a plain `a`.
///
/// Shift splits the same way. It folds into the character (`<S-a>` → `A`), and
/// it rides the parameter on a named key.
#[must_use]
pub fn encode(chord: KeyChord, app_cursor_keys: bool) -> Vec<u8> {
    match chord.key {
        Key::Char(c) => encode_char(c, chord.mods),
        Key::Named(key) => encode_named(key, chord.mods, app_cursor_keys),
    }
}

/// The chord's key, with its modifiers still to be applied. `BackTab` is the
/// Tab key — [`decode_key`] adds the Shift that tells the two apart.
fn decode_code(code: KeyCode) -> Option<Key> {
    let key = match code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Named(NamedKey::Enter),
        KeyCode::Backspace => Key::Named(NamedKey::Backspace),
        KeyCode::Tab | KeyCode::BackTab => Key::Named(NamedKey::Tab),
        KeyCode::Esc => Key::Named(NamedKey::Esc),
        KeyCode::Up => Key::Named(NamedKey::Up),
        KeyCode::Down => Key::Named(NamedKey::Down),
        KeyCode::Right => Key::Named(NamedKey::Right),
        KeyCode::Left => Key::Named(NamedKey::Left),
        KeyCode::Home => Key::Named(NamedKey::Home),
        KeyCode::End => Key::Named(NamedKey::End),
        KeyCode::Insert => Key::Named(NamedKey::Insert),
        KeyCode::Delete => Key::Named(NamedKey::Delete),
        KeyCode::PageUp => Key::Named(NamedKey::PageUp),
        KeyCode::PageDown => Key::Named(NamedKey::PageDown),
        KeyCode::F(n @ 1..=24) => Key::Named(NamedKey::F(n)),
        _ => return None,
    };
    Some(key)
}

/// The host's modifier set, minus Shift: whether Shift belongs in the chord
/// depends on the key it is held with, which [`normalize`] decides. The mouse
/// decoder builds on this, adding Shift back ([`crate::mouse`]).
pub(crate) fn decode_mods(modifiers: KeyModifiers) -> ModFlags {
    let mut mods = ModFlags::NONE;
    if modifiers.contains(KeyModifiers::CONTROL) {
        mods = mods.union(ModFlags::CTRL);
    }
    if modifiers.contains(KeyModifiers::ALT) {
        mods = mods.union(ModFlags::ALT);
    }
    if modifiers.contains(KeyModifiers::SUPER) || modifiers.contains(KeyModifiers::META) {
        mods = mods.union(ModFlags::SUPER);
    }
    mods
}

/// Put the press in the one canonical form the config parser also produces, so
/// a pressed key and a written binding compare equal.
fn normalize(key: Key, mods: ModFlags, modifiers: KeyModifiers) -> KeyChord {
    let shift_held = modifiers.contains(KeyModifiers::SHIFT);

    // The spacebar arrives as the character `' '`; bindings spell it
    // `<Space>`, so the chord carries the named key.
    let key = match key {
        Key::Char(' ') => Key::Named(NamedKey::Space),
        other => other,
    };

    match key {
        // A named key carries Shift like any other modifier: `<S-Tab>`.
        Key::Named(named) => {
            let mods = if shift_held {
                mods.union(ModFlags::SHIFT)
            } else {
                mods
            };
            KeyChord::new(mods, Key::Named(named))
        }
        // An uppercase letter folds to lowercase plus Shift, and a held Shift
        // is reported only on a letter key — a shifted `1` is `!`, which
        // stands for itself.
        Key::Char(c) => {
            let (folded, shifted) = fold_uppercase(c);
            let mods = if shifted || (folded.is_lowercase() && shift_held) {
                mods.union(ModFlags::SHIFT)
            } else {
                mods
            };
            KeyChord::new(mods, Key::Char(folded))
        }
    }
}

/// A character key: Shift restores the capital, Control folds the character
/// into its C0 byte, and Alt prefixes `ESC`.
///
/// `<C-a>` → `0x01`. `<A-a>` → `ESC a`. `<A-C-a>` → `ESC 0x01`. `<S-a>` → `A`.
/// `<C-4>` → `0x1c`, one of the control codes the digit row carries (see
/// [`control_byte`]). `<C-1>` → `1`: no control code stands for it, so the
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
    // `filter` keeps the C0 byte only when Control is actually held: the
    // character's own bytes stand for it otherwise.
    match control_byte(c).filter(|_| mods.contains(ModFlags::CTRL)) {
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
        // Backspace sends DEL; Control makes it the ASCII BS byte, which is
        // how a shell tells "erase a character" from "erase a word".
        NamedKey::Backspace => c0(if ctrl { 0x08 } else { 0x7f }, mods),
        NamedKey::Space => c0(if ctrl { 0x00 } else { b' ' }, mods),
        // Shift+Tab is the one Tab form with a sequence of its own.
        NamedKey::Tab if mods.contains(ModFlags::SHIFT) => vec![ESC, b'[', b'Z'],
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

/// A C0 key's byte, with Alt written as the `ESC` prefix that stands for it.
/// Control is already folded into `byte` by the caller, and Shift has no C0
/// form.
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
/// modifier forces the CSI form, the only one with room for the parameter that
/// carries it.
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
/// `ESC O <final>`. A held modifier takes the CSI form `ESC [ 1 ; <param>
/// <final>`, which has room for the parameter that carries it.
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
/// `3` appends one digit, `3`. `16` appends two, `1` then `6`.
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
/// F13–F24 have no sequence of their own: terminfo spends those slots on Shift
/// plus F1–F12, so `kf13` IS `ESC [ 1 ; 2 P`, Shift+F1. `<F13>` encodes as
/// Shift+F1.
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
/// `<C-Right>` → `5` (1 + 4), which is why that sequence reads `ESC [ 1 ; 5 C`.
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

/// Restore the capital a chord's Shift stands for, undoing the fold every
/// [`Key::Char`] is stored under.
///
/// `'a'` → `'A'`. A character whose uppercase mapping runs to more than one
/// character (`'ß'` → `"SS"`) stands as it is: one key press sends one
/// character.
fn unfold_shift(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// The C0 control byte Control plus this character sends, when one stands for
/// it. `'a'` → `0x01`; `'['` → `0x1b`; `'4'` → `0x1c`; `'1'` has none.
///
/// # The letter run, and the digits that finish it
///
/// Control clears the top bits of the character: `'A' & 0x1f` is `0x01`. That
/// covers `@` through `_` — 32 characters for the 32 C0 codes — and a letter is
/// just its capital's version of that.
///
/// The digit row carries the leftovers, the control codes whose punctuation is
/// hard to reach: `2` sends NUL, `3` sends ESC, `4`–`7` send `0x1c`–`0x1f`, and
/// `8` sends DEL. The same byte therefore has two spellings — `<C-4>` and
/// `<C-\>` both send `0x1c` — and which one arrives depends on the host:
///
/// - On unix the terminal sends the byte, and crossterm decodes `0x1c`–`0x1f`
///   back to `Char('4')`–`Char('7')` — so `Ctrl+\` reaches koshi as `<C-4>`.
///   (`0x00`, `0x1b`, `0x7f` never reach the digit arm there: Space, Esc, and
///   Backspace claim them first.)
/// - On Windows there is no byte to decode; crossterm reports the key's own
///   character, so `Ctrl+4` arrives as `<C-4>` and `Ctrl+\` as `<C-\>`.
///
/// Both spellings leave here as the same byte.
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
