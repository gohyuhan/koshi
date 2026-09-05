//! Incremental parser for input from the terminal that contains Koshi.

use std::collections::VecDeque;

use super::{
    Event, KeyCode, KeyEvent, KeyEventKind, KittyGraphicsReply, Modifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

const ESC: u8 = 0x1b;
const CONTROL_STRING_LIMIT: usize = 4_096;
const CSI_LIMIT: usize = 128;
const PASTE_LIMIT: usize = 16 * 1024 * 1024;
const PASTE_END: &[u8] = b"\x1b[201~";

/// An incremental parser for host-terminal input.
#[derive(Debug)]
pub struct Parser {
    state: State,
    sequence: Vec<u8>,
    paste: Vec<u8>,
    paste_match: usize,
    events: VecDeque<Event>,
    alt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Ss3,
    Csi,
    CsiX10,
    ApcStart,
    Apc,
    DiscardCsi,
    DiscardSt { escape_seen: bool },
    DiscardOsc { escape_seen: bool },
    Paste,
    DiscardPaste,
    Utf8 { expected: u8 },
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Ground,
            sequence: Vec::with_capacity(32),
            paste: Vec::new(),
            paste_match: 0,
            events: VecDeque::with_capacity(32),
            alt: false,
        }
    }
}

impl Parser {
    /// Parse every byte in `bytes` and queue complete events.
    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.push_byte(byte);
        }
    }

    /// Resolve a pending Escape key and discard other incomplete input.
    pub fn finish_pending(&mut self) {
        match self.state {
            State::Escape => self.emit_key(KeyCode::Escape, Modifiers::NONE),
            State::ApcStart => self.emit_key(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT),
            State::Ground => return,
            State::Ss3
            | State::Csi
            | State::CsiX10
            | State::Apc
            | State::DiscardCsi
            | State::DiscardSt { .. }
            | State::DiscardOsc { .. }
            | State::Paste
            | State::DiscardPaste
            | State::Utf8 { .. } => {}
        }
        self.reset();
    }

    /// Return whether an incomplete byte sequence is stored.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.state != State::Ground
    }

    /// Return whether inactivity must resolve an incomplete byte sequence.
    #[must_use]
    pub fn needs_sequence_timeout(&self) -> bool {
        matches!(
            self.state,
            State::Escape
                | State::Ss3
                | State::Csi
                | State::CsiX10
                | State::ApcStart
                | State::Apc
                | State::DiscardCsi
                | State::DiscardSt { .. }
                | State::DiscardOsc { .. }
        )
    }

    /// Remove the oldest complete event.
    pub fn pop(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn push_byte(&mut self, byte: u8) {
        match self.state {
            State::Ground => self.push_ground(byte),
            State::Escape => self.push_escape(byte),
            State::Ss3 => self.push_ss3(byte),
            State::Csi => self.push_csi(byte),
            State::CsiX10 => self.push_x10(byte),
            State::ApcStart => self.push_apc_start(byte),
            State::Apc => self.push_apc(byte),
            State::DiscardCsi => {
                if is_csi_final(byte) {
                    self.reset();
                }
            }
            State::DiscardSt { escape_seen } => self.push_discard_st(byte, escape_seen),
            State::DiscardOsc { escape_seen } => self.push_discard_osc(byte, escape_seen),
            State::Paste => self.push_paste(byte, false),
            State::DiscardPaste => self.push_paste(byte, true),
            State::Utf8 { expected } => self.push_utf8(byte, expected),
        }
    }

    fn push_ground(&mut self, byte: u8) {
        match byte {
            ESC => self.state = State::Escape,
            b'\r' => self.emit_key(KeyCode::Enter, Modifiers::NONE),
            b'\t' => self.emit_key(KeyCode::Tab, Modifiers::NONE),
            0x7f => self.emit_key(KeyCode::Backspace, Modifiers::NONE),
            0 => self.emit_key(KeyCode::Char(' '), Modifiers::CONTROL),
            byte @ 0x01..=0x1a => self.emit_key(
                KeyCode::Char(char::from(byte - 1 + b'a')),
                Modifiers::CONTROL,
            ),
            byte @ 0x1c..=0x1f => self.emit_key(
                KeyCode::Char(char::from(byte - 0x1c + b'4')),
                Modifiers::CONTROL,
            ),
            0x20..=0x7e => self.emit_char(char::from(byte)),
            _ => match utf8_width(byte) {
                Some(expected) => {
                    self.sequence.clear();
                    self.sequence.push(byte);
                    self.state = State::Utf8 { expected };
                }
                None => self.alt = false,
            },
        }
    }

    fn push_escape(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.sequence.clear();
                self.state = State::Csi;
            }
            b'O' => self.state = State::Ss3,
            b']' => self.state = State::DiscardOsc { escape_seen: false },
            b'P' => self.state = State::DiscardSt { escape_seen: false },
            b'_' => self.state = State::ApcStart,
            ESC => {
                self.emit_key(KeyCode::Escape, Modifiers::NONE);
                self.state = State::Escape;
            }
            _ => {
                self.state = State::Ground;
                self.alt = true;
                self.push_ground(byte);
            }
        }
    }

    fn push_ss3(&mut self, byte: u8) {
        let code = match byte {
            b'A' => Some(KeyCode::Up),
            b'B' => Some(KeyCode::Down),
            b'C' => Some(KeyCode::Right),
            b'D' => Some(KeyCode::Left),
            b'F' => Some(KeyCode::End),
            b'H' => Some(KeyCode::Home),
            b'P'..=b'S' => Some(KeyCode::Function(byte - b'P' + 1)),
            _ => None,
        };
        self.state = State::Ground;
        if let Some(code) = code {
            self.emit_key(code, Modifiers::NONE);
        }
    }

    fn push_csi(&mut self, byte: u8) {
        self.sequence.push(byte);
        if self.sequence == b"200~" {
            self.sequence.clear();
            self.paste.clear();
            self.paste_match = 0;
            self.state = State::Paste;
            return;
        }
        if self.sequence.len() == 1 && byte == b'M' {
            self.state = State::CsiX10;
            return;
        }
        if self.sequence.first() == Some(&b'[') && self.sequence.len() == 1 {
            return;
        }
        if is_csi_final(byte) {
            let event = parse_csi(&self.sequence);
            self.reset();
            if let Some(event) = event {
                self.events.push_back(event);
            }
        } else if self.sequence.len() >= CSI_LIMIT {
            self.sequence.clear();
            self.state = State::DiscardCsi;
        }
    }

    fn push_x10(&mut self, byte: u8) {
        self.sequence.push(byte);
        if self.sequence.len() == 4 {
            let event = parse_x10_mouse(&self.sequence);
            self.reset();
            if let Some(event) = event {
                self.events.push_back(event);
            }
        }
    }

    fn push_apc_start(&mut self, byte: u8) {
        if byte == b'G' {
            self.sequence.clear();
            self.sequence.push(byte);
            self.state = State::Apc;
        } else {
            self.emit_key(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT);
            self.state = State::Ground;
            self.push_ground(byte);
        }
    }

    fn push_apc(&mut self, byte: u8) {
        self.sequence.push(byte);
        if self.sequence.ends_with(b"\x1b\\") || byte == 0x9c {
            let payload_len = if byte == 0x9c {
                self.sequence.len() - 1
            } else {
                self.sequence.len() - 2
            };
            let event = parse_kitty_reply(&self.sequence[..payload_len]);
            self.reset();
            if let Some(event) = event {
                self.events.push_back(Event::KittyGraphicsReply(event));
            }
        } else if self.sequence.len() >= CONTROL_STRING_LIMIT {
            let escape_seen = byte == ESC;
            self.sequence.clear();
            self.state = State::DiscardSt { escape_seen };
        }
    }

    fn push_discard_st(&mut self, byte: u8, escape_seen: bool) {
        if byte == 0x9c || (escape_seen && byte == b'\\') {
            self.reset();
        } else {
            self.state = State::DiscardSt {
                escape_seen: byte == ESC,
            };
        }
    }

    fn push_discard_osc(&mut self, byte: u8, escape_seen: bool) {
        if byte == 0x07 || byte == 0x9c || (escape_seen && byte == b'\\') {
            self.reset();
        } else {
            self.state = State::DiscardOsc {
                escape_seen: byte == ESC,
            };
        }
    }

    fn push_paste(&mut self, byte: u8, discard: bool) {
        if byte == PASTE_END[self.paste_match] {
            self.paste_match += 1;
            if self.paste_match == PASTE_END.len() {
                if !discard {
                    let bytes = std::mem::take(&mut self.paste);
                    self.events
                        .push_back(Event::Paste(String::from_utf8_lossy(&bytes).into_owned()));
                }
                self.reset();
            }
            return;
        }

        if self.paste_match != 0 {
            if !discard {
                self.paste.extend_from_slice(&PASTE_END[..self.paste_match]);
            }
            self.paste_match = 0;
            if byte == PASTE_END[0] {
                self.paste_match = 1;
                return;
            }
        }

        if !discard {
            self.paste.push(byte);
            if self.paste.len() > PASTE_LIMIT {
                self.paste.clear();
                self.state = State::DiscardPaste;
            }
        }
    }

    fn push_utf8(&mut self, byte: u8, expected: u8) {
        if byte & 0xc0 != 0x80 {
            self.sequence.clear();
            self.state = State::Ground;
            self.alt = false;
            self.push_ground(byte);
            return;
        }
        self.sequence.push(byte);
        if self.sequence.len() == usize::from(expected) {
            let character = std::str::from_utf8(&self.sequence)
                .ok()
                .and_then(|text| text.chars().next());
            self.sequence.clear();
            self.state = State::Ground;
            if let Some(character) = character {
                self.emit_char(character);
            } else {
                self.alt = false;
            }
        }
    }

    fn emit_char(&mut self, character: char) {
        let mut modifiers = if character.is_uppercase() {
            Modifiers::SHIFT
        } else {
            Modifiers::NONE
        };
        if self.alt {
            modifiers |= Modifiers::ALT;
            self.alt = false;
        }
        self.emit_key(KeyCode::Char(character), modifiers);
    }

    fn emit_key(&mut self, code: KeyCode, mut modifiers: Modifiers) {
        if self.alt {
            modifiers |= Modifiers::ALT;
            self.alt = false;
        }
        self.events
            .push_back(Event::Key(KeyEvent::new(code, modifiers)));
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.sequence.clear();
        self.paste.clear();
        self.paste_match = 0;
        self.alt = false;
    }
}

fn is_csi_final(byte: u8) -> bool {
    (0x40..=0x7e).contains(&byte)
}

fn utf8_width(first: u8) -> Option<u8> {
    match first {
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn parse_csi(sequence: &[u8]) -> Option<Event> {
    let (&final_byte, body) = sequence.split_last()?;
    if body == b"[" && (b'A'..=b'E').contains(&final_byte) {
        return Some(Event::Key(KeyCode::Function(final_byte - b'A' + 1).into()));
    }
    if body.is_empty() {
        let event = match final_byte {
            b'A' => Event::Key(KeyCode::Up.into()),
            b'B' => Event::Key(KeyCode::Down.into()),
            b'C' => Event::Key(KeyCode::Right.into()),
            b'D' => Event::Key(KeyCode::Left.into()),
            b'F' => Event::Key(KeyCode::End.into()),
            b'H' => Event::Key(KeyCode::Home.into()),
            b'I' => Event::FocusIn,
            b'O' => Event::FocusOut,
            b'P' => Event::Key(KeyCode::Function(1).into()),
            b'Q' => Event::Key(KeyCode::Function(2).into()),
            b'S' => Event::Key(KeyCode::Function(4).into()),
            b'Z' => Event::Key(KeyEvent::new(KeyCode::BackTab, Modifiers::SHIFT)),
            _ => return None,
        };
        return Some(event);
    }
    if body[0] == b'?' && final_byte == b'c' && valid_da1(&body[1..]) {
        return Some(Event::PrimaryDeviceAttributes);
    }
    if body[0] == b'<' && matches!(final_byte, b'M' | b'm') {
        return parse_sgr_mouse(&body[1..], final_byte).map(Event::Mouse);
    }
    match final_byte {
        b'A' | b'B' | b'C' | b'D' | b'F' | b'H' | b'P' | b'Q' | b'R' | b'S' => {
            parse_modified_key(body, final_byte).map(Event::Key)
        }
        b'M' => parse_rxvt_mouse(body).map(Event::Mouse),
        b'~' => parse_tilde_key(body).map(Event::Key),
        b'u' if body.first() != Some(&b'?') => parse_kitty_key(body).map(Event::Key),
        _ => None,
    }
}

fn valid_da1(body: &[u8]) -> bool {
    !body.is_empty()
        && body
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
        && body
            .split(|byte| *byte == b';')
            .all(|part| !part.is_empty())
}

fn parse_modified_key(body: &[u8], final_byte: u8) -> Option<KeyEvent> {
    let mut fields = body.split(|byte| *byte == b';');
    let first = fields.next()?;
    if !first.is_empty() && decimal(first)? != 1 {
        return None;
    }
    let (modifiers, kind) = match fields.next() {
        Some(field) => parse_modifier_field(field)?,
        None => (Modifiers::NONE, KeyEventKind::Press),
    };
    if fields.next().is_some() {
        return None;
    }
    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'F' => KeyCode::End,
        b'H' => KeyCode::Home,
        b'P' => KeyCode::Function(1),
        b'Q' => KeyCode::Function(2),
        b'R' => KeyCode::Function(3),
        b'S' => KeyCode::Function(4),
        _ => return None,
    };
    Some(KeyEvent {
        code,
        kind,
        modifiers,
    })
}

fn parse_tilde_key(body: &[u8]) -> Option<KeyEvent> {
    let mut fields = body.split(|byte| *byte == b';');
    let number = decimal(fields.next()?)?;
    let (modifiers, kind) = match fields.next() {
        Some(field) => parse_modifier_field(field)?,
        None => (Modifiers::NONE, KeyEventKind::Press),
    };
    if fields.next().is_some() {
        return None;
    }
    let code = match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::Function(u8::try_from(number - 10).ok()?),
        17..=21 => KeyCode::Function(u8::try_from(number - 11).ok()?),
        23..=26 => KeyCode::Function(u8::try_from(number - 12).ok()?),
        28..=29 => KeyCode::Function(u8::try_from(number - 15).ok()?),
        31..=34 => KeyCode::Function(u8::try_from(number - 17).ok()?),
        _ => return None,
    };
    Some(KeyEvent {
        code,
        kind,
        modifiers,
    })
}

fn parse_kitty_key(body: &[u8]) -> Option<KeyEvent> {
    let mut fields = body.split(|byte| *byte == b';');
    let key_field = fields.next()?;
    let mut key_codes = key_field.split(|byte| *byte == b':');
    let codepoint = decimal(key_codes.next()?)?;
    let shifted = key_codes
        .next()
        .filter(|field| !field.is_empty())
        .and_then(decimal);
    let (mut modifiers, kind) = match fields.next() {
        Some(field) => parse_modifier_field(field)?,
        None => (Modifiers::NONE, KeyEventKind::Press),
    };
    let mut code = functional_key(codepoint).or_else(|| {
        let character = char::from_u32(codepoint)?;
        Some(match character {
            '\x1b' => KeyCode::Escape,
            '\r' => KeyCode::Enter,
            '\t' if modifiers.contains(Modifiers::SHIFT) => KeyCode::BackTab,
            '\t' => KeyCode::Tab,
            '\x7f' => KeyCode::Backspace,
            character => KeyCode::Char(character),
        })
    })?;
    if modifiers.contains(Modifiers::SHIFT) {
        if let Some(character) = shifted.and_then(char::from_u32) {
            code = KeyCode::Char(character);
            modifiers = without(modifiers, Modifiers::SHIFT);
        }
    }
    Some(KeyEvent {
        code,
        kind,
        modifiers,
    })
}

fn functional_key(codepoint: u32) -> Option<KeyCode> {
    let code = match codepoint {
        57_376..=57_387 => KeyCode::Function(u8::try_from(codepoint - 57_376 + 13).ok()?),
        57_388..=57_398 | 57_358..=57_363 | 57_428..=57_454 => KeyCode::Unsupported,
        57_399..=57_408 => KeyCode::Char(char::from_digit(codepoint - 57_399, 10)?),
        57_409 => KeyCode::Char('.'),
        57_410 => KeyCode::Char('/'),
        57_411 => KeyCode::Char('*'),
        57_412 => KeyCode::Char('-'),
        57_413 => KeyCode::Char('+'),
        57_414 => KeyCode::Enter,
        57_415 => KeyCode::Char('='),
        57_416 => KeyCode::Char(','),
        57_417 => KeyCode::Left,
        57_418 => KeyCode::Right,
        57_419 => KeyCode::Up,
        57_420 => KeyCode::Down,
        57_421 => KeyCode::PageUp,
        57_422 => KeyCode::PageDown,
        57_423 => KeyCode::Home,
        57_424 => KeyCode::End,
        57_425 => KeyCode::Insert,
        57_426 => KeyCode::Delete,
        57_427 => KeyCode::Unsupported,
        _ => return None,
    };
    Some(code)
}

fn parse_modifier_field(field: &[u8]) -> Option<(Modifiers, KeyEventKind)> {
    let mut parts = field.split(|byte| *byte == b':');
    let encoded = decimal(parts.next()?)?;
    let bits = encoded.checked_sub(1)?;
    let mut modifiers = Modifiers::NONE;
    for (bit, modifier) in [
        (1, Modifiers::SHIFT),
        (2, Modifiers::ALT),
        (4, Modifiers::CONTROL),
        (8, Modifiers::SUPER),
        (16, Modifiers::HYPER),
        (32, Modifiers::META),
    ] {
        if bits & bit != 0 {
            modifiers |= modifier;
        }
    }
    let kind = match parts.next() {
        None => KeyEventKind::Press,
        Some(part) => match decimal(part)? {
            1 => KeyEventKind::Press,
            2 => KeyEventKind::Repeat,
            3 => KeyEventKind::Release,
            _ => return None,
        },
    };
    if parts.next().is_some() {
        return None;
    }
    Some((modifiers, kind))
}

fn without(modifiers: Modifiers, removed: Modifiers) -> Modifiers {
    Modifiers(modifiers.0 & !removed.0)
}

fn parse_sgr_mouse(body: &[u8], final_byte: u8) -> Option<MouseEvent> {
    let mut fields = body.split(|byte| *byte == b';');
    let cb = u8::try_from(decimal(fields.next()?)?).ok()?;
    let column = u16::try_from(decimal(fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    let row = u16::try_from(decimal(fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    if fields.next().is_some() {
        return None;
    }
    let (mut kind, modifiers) = mouse_code(cb)?;
    if final_byte == b'm' {
        if let MouseEventKind::Down(button) = kind {
            kind = MouseEventKind::Up(button);
        }
    }
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_rxvt_mouse(body: &[u8]) -> Option<MouseEvent> {
    let mut fields = body.split(|byte| *byte == b';');
    let cb = u8::try_from(decimal(fields.next()?)?)
        .ok()?
        .checked_sub(32)?;
    let column = u16::try_from(decimal(fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    let row = u16::try_from(decimal(fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    if fields.next().is_some() {
        return None;
    }
    let (kind, modifiers) = mouse_code(cb)?;
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_x10_mouse(sequence: &[u8]) -> Option<Event> {
    let [b'M', cb, column, row] = sequence else {
        return None;
    };
    let (kind, modifiers) = mouse_code(cb.checked_sub(32)?)?;
    Some(Event::Mouse(MouseEvent {
        kind,
        column: u16::from(column.checked_sub(33)?),
        row: u16::from(row.checked_sub(33)?),
        modifiers,
    }))
}

fn mouse_code(cb: u8) -> Option<(MouseEventKind, Modifiers)> {
    let button = (cb & 0b11) | ((cb & 0b1100_0000) >> 4);
    let drag = cb & 0b0010_0000 != 0;
    let kind = match (button, drag) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = Modifiers::NONE;
    if cb & 4 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if cb & 8 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if cb & 16 != 0 {
        modifiers |= Modifiers::CONTROL;
    }
    Some((kind, modifiers))
}

fn parse_kitty_reply(payload: &[u8]) -> Option<KittyGraphicsReply> {
    let payload = payload.strip_prefix(b"G")?;
    let separator = payload.iter().position(|byte| *byte == b';')?;
    let (control, message) = payload.split_at(separator);
    let message = message.get(1..)?;
    if message.is_empty() || !message.iter().all(|byte| (b' '..=b'~').contains(byte)) {
        return None;
    }
    let mut image_id = None;
    for pair in control.split(|byte| *byte == b',') {
        let equals = pair.iter().position(|byte| *byte == b'=')?;
        let (key, value) = pair.split_at(equals);
        if key == b"i" {
            if image_id.is_some() {
                return None;
            }
            image_id = Some(decimal(value.get(1..)?)?);
        }
    }
    Some(KittyGraphicsReply {
        image_id: image_id?,
        ok: message == b"OK",
    })
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        value
            .checked_mul(10)?
            .checked_add(u32::from(byte.checked_sub(b'0')?))
            .filter(|_| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests;
