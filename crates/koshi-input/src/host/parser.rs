//! Incremental parser for input from the terminal that contains Koshi.
//!
//! The parser emits complete events and retains incomplete UTF-8, control, and
//! paste sequences while waiting for more bytes. `finish_pending_input` resolves
//! timeout-eligible prefixes and discards other incomplete input. For example,
//! `ESCAPE_BYTE [ 1 ; 5 C` becomes a Right key with Control held.

use std::collections::VecDeque;

use koshi_core::key::TEXT_ONLY_KEY_CODEPOINT;

use super::{
    Event, GraphicAttributeError, GraphicAttributeReply, KeyCode, KeyEvent, KeyEventKind,
    KittyGraphicsReply, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};

const ESCAPE_BYTE: u8 = 0x1b;
const MAX_CONTROL_STRING_BYTE_COUNT: usize = 4_096;
const MAX_CSI_BYTE_COUNT: usize = 128;
const MAX_PASTE_BYTE_COUNT: usize = 16 * 1024 * 1024;
const PASTE_END_SEQUENCE_BYTES: &[u8] = b"\x1b[201~";

/// An incremental parser for host-terminal input.
#[derive(Debug)]
pub struct Parser {
    parser_state: ParserState,
    control_sequence: Vec<u8>,
    paste_bytes: Vec<u8>,
    paste_marker_match_length: usize,
    pending_events: VecDeque<Event>,
    is_alt_held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Ss3,
    Csi,
    PrivateCsi,
    CsiX10,
    ApcStart,
    Apc,
    Osc { is_escape_seen: bool },
    DiscardCsi,
    DiscardSt { is_escape_seen: bool },
    DiscardOsc { is_escape_seen: bool },
    Paste,
    DiscardPaste,
    Utf8 { expected_utf8_byte_count: u8 },
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            parser_state: ParserState::Ground,
            control_sequence: Vec::with_capacity(32),
            paste_bytes: Vec::new(),
            paste_marker_match_length: 0,
            pending_events: VecDeque::with_capacity(32),
            is_alt_held: false,
        }
    }
}

impl Parser {
    /// Parse every byte in `bytes` and queue complete events.
    pub fn process_input_bytes(&mut self, input_bytes: &[u8]) {
        for &input_byte in input_bytes {
            self.process_input_byte(input_byte);
        }
    }

    /// Resolve timeout-eligible prefixes and discard other incomplete input.
    /// Identified control strings remain pending until their terminator.
    pub fn finish_pending_input(&mut self) {
        match self.parser_state {
            ParserState::Escape => self.emit_key(KeyCode::Escape, Modifiers::NONE),
            ParserState::ApcStart => {
                self.emit_key(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT)
            }
            ParserState::Ground
            | ParserState::Apc
            | ParserState::Osc { .. }
            | ParserState::DiscardCsi
            | ParserState::DiscardSt { .. }
            | ParserState::DiscardOsc { .. } => return,
            ParserState::PrivateCsi => return,
            ParserState::Ss3
            | ParserState::Csi
            | ParserState::CsiX10
            | ParserState::Paste
            | ParserState::DiscardPaste
            | ParserState::Utf8 { .. } => {}
        }
        self.reset_input_parser();
    }

    /// Return whether an incomplete byte sequence is stored.
    #[must_use]
    pub fn has_pending_input(&self) -> bool {
        self.parser_state != ParserState::Ground
    }

    /// Return whether inactivity must resolve an incomplete byte sequence.
    #[must_use]
    pub fn needs_input_sequence_timeout(&self) -> bool {
        matches!(
            self.parser_state,
            ParserState::Escape
                | ParserState::Ss3
                | ParserState::Csi
                | ParserState::CsiX10
                | ParserState::ApcStart
        )
    }

    /// Remove the oldest complete event.
    pub fn remove_next_pending_event(&mut self) -> Option<Event> {
        self.pending_events.pop_front()
    }

    fn process_input_byte(&mut self, input_byte: u8) {
        match self.parser_state {
            ParserState::Ground => self.process_ground_byte(input_byte),
            ParserState::Escape => self.process_escape_byte(input_byte),
            ParserState::Ss3 => self.process_ss3_byte(input_byte),
            ParserState::Csi | ParserState::PrivateCsi => self.process_csi_byte(input_byte),
            ParserState::CsiX10 => self.process_x10_byte(input_byte),
            ParserState::ApcStart => self.process_apc_start_byte(input_byte),
            ParserState::Apc => self.process_apc_byte(input_byte),
            ParserState::Osc { is_escape_seen } => {
                self.process_osc_byte(input_byte, is_escape_seen)
            }
            ParserState::DiscardCsi => match input_byte {
                0x18 | 0x1a => self.reset_input_parser(),
                ESCAPE_BYTE => {
                    self.reset_input_parser();
                    self.parser_state = ParserState::Escape;
                }
                input_byte if is_csi_final(input_byte) => self.reset_input_parser(),
                _ => {}
            },
            ParserState::DiscardSt { is_escape_seen } => {
                self.process_discard_st_byte(input_byte, is_escape_seen)
            }
            ParserState::DiscardOsc { is_escape_seen } => {
                self.process_discard_osc_byte(input_byte, is_escape_seen)
            }
            ParserState::Paste => self.process_paste_byte(input_byte, false),
            ParserState::DiscardPaste => self.process_paste_byte(input_byte, true),
            ParserState::Utf8 {
                expected_utf8_byte_count,
            } => self.process_utf8_byte(input_byte, expected_utf8_byte_count),
        }
    }

    fn process_ground_byte(&mut self, input_byte: u8) {
        match input_byte {
            ESCAPE_BYTE => self.parser_state = ParserState::Escape,
            b'\r' => self.emit_key(KeyCode::Enter, Modifiers::NONE),
            b'\t' => self.emit_key(KeyCode::Tab, Modifiers::NONE),
            0x7f => self.emit_key(KeyCode::Backspace, Modifiers::NONE),
            0 => self.emit_key(KeyCode::Char(' '), Modifiers::CONTROL),
            input_byte @ 0x01..=0x1a => self.emit_key(
                KeyCode::Char(char::from(input_byte - 1 + b'a')),
                Modifiers::CONTROL,
            ),
            input_byte @ 0x1c..=0x1f => self.emit_key(
                KeyCode::Char(char::from(input_byte - 0x1c + b'4')),
                Modifiers::CONTROL,
            ),
            0x20..=0x7e => self.emit_char(char::from(input_byte)),
            _ => match compute_utf8_byte_width(input_byte) {
                Some(expected_utf8_byte_count) => {
                    self.control_sequence.clear();
                    self.control_sequence.push(input_byte);
                    self.parser_state = ParserState::Utf8 {
                        expected_utf8_byte_count,
                    };
                }
                None => self.is_alt_held = false,
            },
        }
    }

    fn process_escape_byte(&mut self, input_byte: u8) {
        match input_byte {
            b'[' => {
                self.control_sequence.clear();
                self.parser_state = ParserState::Csi;
            }
            b'O' => self.parser_state = ParserState::Ss3,
            b']' => {
                self.control_sequence.clear();
                self.parser_state = ParserState::Osc {
                    is_escape_seen: false,
                };
            }
            b'P' => {
                self.parser_state = ParserState::DiscardSt {
                    is_escape_seen: false,
                }
            }
            b'_' => self.parser_state = ParserState::ApcStart,
            ESCAPE_BYTE => {
                self.emit_key(KeyCode::Escape, Modifiers::NONE);
                self.parser_state = ParserState::Escape;
            }
            _ => {
                self.parser_state = ParserState::Ground;
                self.is_alt_held = true;
                self.process_ground_byte(input_byte);
            }
        }
    }

    fn process_ss3_byte(&mut self, input_byte: u8) {
        let key_code = match input_byte {
            b'A' => Some(KeyCode::Up),
            b'B' => Some(KeyCode::Down),
            b'C' => Some(KeyCode::Right),
            b'D' => Some(KeyCode::Left),
            b'F' => Some(KeyCode::End),
            b'H' => Some(KeyCode::Home),
            b'P'..=b'S' => Some(KeyCode::Function(input_byte - b'P' + 1)),
            _ => None,
        };
        self.parser_state = ParserState::Ground;
        if let Some(key_code) = key_code {
            self.emit_key(key_code, Modifiers::NONE);
        }
    }

    fn process_csi_byte(&mut self, input_byte: u8) {
        if matches!(input_byte, 0x18 | 0x1a) {
            self.reset_input_parser();
            return;
        }
        if input_byte == ESCAPE_BYTE {
            self.reset_input_parser();
            self.parser_state = ParserState::Escape;
            return;
        }
        self.control_sequence.push(input_byte);
        if self.control_sequence == b"200~" {
            self.control_sequence.clear();
            self.paste_bytes.clear();
            self.paste_marker_match_length = 0;
            self.parser_state = ParserState::Paste;
            return;
        }
        if self.control_sequence.len() == 1 && input_byte == b'M' {
            self.parser_state = ParserState::CsiX10;
            return;
        }
        if self.control_sequence.len() == 1 && input_byte == b'?' {
            self.parser_state = ParserState::PrivateCsi;
            return;
        }
        if self.control_sequence.first() == Some(&b'[') && self.control_sequence.len() == 1 {
            return;
        }
        if is_csi_final(input_byte) {
            let parsed_event = parse_csi(&self.control_sequence);
            self.reset_input_parser();
            if let Some(parsed_event) = parsed_event {
                self.pending_events.push_back(parsed_event);
            }
        } else if self.control_sequence.len() >= MAX_CSI_BYTE_COUNT {
            self.control_sequence.clear();
            self.parser_state = ParserState::DiscardCsi;
        }
    }

    fn process_x10_byte(&mut self, input_byte: u8) {
        self.control_sequence.push(input_byte);
        if self.control_sequence.len() == 4 {
            let parsed_event = parse_x10_mouse(&self.control_sequence);
            self.reset_input_parser();
            if let Some(parsed_event) = parsed_event {
                self.pending_events.push_back(parsed_event);
            }
        }
    }

    fn process_apc_start_byte(&mut self, input_byte: u8) {
        if input_byte == b'G' {
            self.control_sequence.clear();
            self.control_sequence.push(input_byte);
            self.parser_state = ParserState::Apc;
        } else {
            self.emit_key(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT);
            self.parser_state = ParserState::Ground;
            self.process_ground_byte(input_byte);
        }
    }

    fn process_apc_byte(&mut self, input_byte: u8) {
        if matches!(input_byte, 0x18 | 0x1a) {
            self.reset_input_parser();
            return;
        }
        self.control_sequence.push(input_byte);
        if self.control_sequence.ends_with(b"\x1b\\") || input_byte == 0x9c {
            let payload_byte_count = if input_byte == 0x9c {
                self.control_sequence.len() - 1
            } else {
                self.control_sequence.len() - 2
            };
            let parsed_event = parse_kitty_reply(&self.control_sequence[..payload_byte_count]);
            self.reset_input_parser();
            if let Some(parsed_event) = parsed_event {
                self.pending_events
                    .push_back(Event::KittyGraphicsReply(parsed_event));
            }
        } else if self.control_sequence.len() >= MAX_CONTROL_STRING_BYTE_COUNT {
            let is_escape_seen = input_byte == ESCAPE_BYTE;
            self.control_sequence.clear();
            self.parser_state = ParserState::DiscardSt { is_escape_seen };
        }
    }

    fn process_osc_byte(&mut self, input_byte: u8, is_escape_seen: bool) {
        if matches!(input_byte, 0x18 | 0x1a) {
            self.reset_input_parser();
            return;
        }
        if input_byte == 0x07 || input_byte == 0x9c || (is_escape_seen && input_byte == b'\\') {
            let payload_byte_count = if input_byte == b'\\' && is_escape_seen {
                self.control_sequence.len().saturating_sub(1)
            } else {
                self.control_sequence.len()
            };
            let parsed_event = parse_osc(&self.control_sequence[..payload_byte_count]);
            self.reset_input_parser();
            if let Some(parsed_event) = parsed_event {
                self.pending_events.push_back(parsed_event);
            }
            return;
        }
        if self.control_sequence.len() >= MAX_CONTROL_STRING_BYTE_COUNT {
            self.control_sequence.clear();
            self.parser_state = ParserState::DiscardOsc {
                is_escape_seen: input_byte == ESCAPE_BYTE,
            };
            return;
        }
        self.control_sequence.push(input_byte);
        self.parser_state = ParserState::Osc {
            is_escape_seen: input_byte == ESCAPE_BYTE,
        };
    }

    fn process_discard_st_byte(&mut self, input_byte: u8, is_escape_seen: bool) {
        if matches!(input_byte, 0x18 | 0x1a)
            || input_byte == 0x9c
            || (is_escape_seen && input_byte == b'\\')
        {
            self.reset_input_parser();
        } else {
            self.parser_state = ParserState::DiscardSt {
                is_escape_seen: input_byte == ESCAPE_BYTE,
            };
        }
    }

    fn process_discard_osc_byte(&mut self, input_byte: u8, is_escape_seen: bool) {
        if matches!(input_byte, 0x18 | 0x1a)
            || input_byte == 0x07
            || input_byte == 0x9c
            || (is_escape_seen && input_byte == b'\\')
        {
            self.reset_input_parser();
        } else {
            self.parser_state = ParserState::DiscardOsc {
                is_escape_seen: input_byte == ESCAPE_BYTE,
            };
        }
    }

    fn process_paste_byte(&mut self, input_byte: u8, should_discard: bool) {
        if input_byte == PASTE_END_SEQUENCE_BYTES[self.paste_marker_match_length] {
            self.paste_marker_match_length += 1;
            if self.paste_marker_match_length == PASTE_END_SEQUENCE_BYTES.len() {
                if !should_discard {
                    let paste_bytes = std::mem::take(&mut self.paste_bytes);
                    self.pending_events.push_back(Event::Paste(
                        String::from_utf8_lossy(&paste_bytes).into_owned(),
                    ));
                }
                self.reset_input_parser();
            }
            return;
        }

        if self.paste_marker_match_length != 0 {
            if !should_discard {
                self.paste_bytes
                    .extend_from_slice(&PASTE_END_SEQUENCE_BYTES[..self.paste_marker_match_length]);
            }
            self.paste_marker_match_length = 0;
            if input_byte == PASTE_END_SEQUENCE_BYTES[0] {
                self.paste_marker_match_length = 1;
                return;
            }
        }

        if !should_discard {
            self.paste_bytes.push(input_byte);
            if self.paste_bytes.len() > MAX_PASTE_BYTE_COUNT {
                self.paste_bytes.clear();
                self.parser_state = ParserState::DiscardPaste;
            }
        }
    }

    fn process_utf8_byte(&mut self, input_byte: u8, expected_utf8_byte_count: u8) {
        if input_byte & 0xc0 != 0x80 {
            self.control_sequence.clear();
            self.parser_state = ParserState::Ground;
            self.is_alt_held = false;
            self.process_ground_byte(input_byte);
            return;
        }
        self.control_sequence.push(input_byte);
        if self.control_sequence.len() == usize::from(expected_utf8_byte_count) {
            let character = std::str::from_utf8(&self.control_sequence)
                .ok()
                .and_then(|text| text.chars().next());
            self.control_sequence.clear();
            self.parser_state = ParserState::Ground;
            if let Some(character) = character {
                self.emit_char(character);
            } else {
                self.is_alt_held = false;
            }
        }
    }

    fn emit_char(&mut self, character: char) {
        let mut modifiers = if character.is_uppercase() {
            Modifiers::SHIFT
        } else {
            Modifiers::NONE
        };
        if self.is_alt_held {
            modifiers |= Modifiers::ALT;
            self.is_alt_held = false;
        }
        self.emit_key(KeyCode::Char(character), modifiers);
    }

    fn emit_key(&mut self, key_code: KeyCode, mut modifiers: Modifiers) {
        if self.is_alt_held {
            modifiers |= Modifiers::ALT;
            self.is_alt_held = false;
        }
        self.pending_events
            .push_back(Event::Key(KeyEvent::from_key_code_and_modifiers(
                key_code, modifiers,
            )));
    }

    fn reset_input_parser(&mut self) {
        self.parser_state = ParserState::Ground;
        self.control_sequence.clear();
        self.paste_bytes.clear();
        self.paste_marker_match_length = 0;
        self.is_alt_held = false;
    }
}

fn is_csi_final(input_byte: u8) -> bool {
    (0x40..=0x7e).contains(&input_byte)
}

fn compute_utf8_byte_width(first_utf8_byte: u8) -> Option<u8> {
    match first_utf8_byte {
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn parse_osc(osc_payload: &[u8]) -> Option<Event> {
    osc_payload
        .strip_prefix(b"1337;Capabilities=")
        .map(|features| Event::TerminalFeatures(features.to_vec()))
}

fn parse_csi(control_sequence: &[u8]) -> Option<Event> {
    let (&final_byte, csi_body) = control_sequence.split_last()?;
    if csi_body == b"[" && (b'A'..=b'E').contains(&final_byte) {
        return Some(Event::Key(KeyCode::Function(final_byte - b'A' + 1).into()));
    }
    if csi_body.is_empty() {
        let parsed_event = match final_byte {
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
            b'Z' => Event::Key(KeyEvent::from_key_code_and_modifiers(
                KeyCode::BackTab,
                Modifiers::SHIFT,
            )),
            _ => return None,
        };
        return Some(parsed_event);
    }
    if csi_body.first() == Some(&b'?') && final_byte == b'c' {
        return parse_da1(&csi_body[1..]).map(Event::PrimaryDeviceAttributes);
    }
    if csi_body.first() == Some(&b'?') && final_byte == b'S' {
        return parse_graphic_attribute(&csi_body[1..]).map(Event::SixelGraphicsAttributeReply);
    }
    if csi_body[0] == b'<' && matches!(final_byte, b'M' | b'm') {
        return parse_sgr_mouse(&csi_body[1..], final_byte).map(Event::Mouse);
    }
    match final_byte {
        b't' => {
            let mut cell_size_fields = csi_body.split(|field_byte| *field_byte == b';');
            if cell_size_fields.next()? != b"6" {
                return None;
            }
            let cell_pixel_height = u16::try_from(parse_decimal(cell_size_fields.next()?)?).ok()?;
            let cell_pixel_width = u16::try_from(parse_decimal(cell_size_fields.next()?)?).ok()?;
            if cell_size_fields.next().is_some() {
                return None;
            }
            koshi_core::geometry::PixelCellSize::from_pixel_dimensions(
                cell_pixel_width,
                cell_pixel_height,
            )
            .map(Event::CellSize)
        }
        b'A' | b'B' | b'C' | b'D' | b'F' | b'H' | b'P' | b'Q' | b'R' | b'S' => {
            parse_modified_key(csi_body, final_byte).map(Event::Key)
        }
        b'M' => parse_rxvt_mouse(csi_body).map(Event::Mouse),
        b'~' => parse_tilde_key(csi_body).map(Event::Key),
        b'u' if csi_body.first() == Some(&b'?') => parse_keyboard_enhancement_flags(&csi_body[1..]),
        b'u' => parse_kitty_key(csi_body).map(Event::Key),
        _ => None,
    }
}

fn parse_da1(attribute_parameter_bytes: &[u8]) -> Option<Vec<u32>> {
    if attribute_parameter_bytes.is_empty() {
        return None;
    }
    attribute_parameter_bytes
        .split(|field_byte| *field_byte == b';')
        .map(parse_decimal)
        .collect::<Option<Vec<_>>>()
}

fn parse_graphic_attribute(attribute_parameter_bytes: &[u8]) -> Option<GraphicAttributeReply> {
    let mut attribute_fields = attribute_parameter_bytes.split(|field_byte| *field_byte == b';');
    let attribute_number = parse_decimal(attribute_fields.next()?)?;
    let attribute_status = parse_decimal(attribute_fields.next()?)?;
    let graphic_attribute_reply = match attribute_number {
        1 => parse_palette_reply(attribute_status, attribute_fields.collect()),
        2 => parse_geometry_reply(attribute_status, attribute_fields.collect()),
        _ => None,
    }?;
    Some(graphic_attribute_reply)
}

fn parse_palette_reply(
    attribute_status: u32,
    reply_parameter_values: Vec<&[u8]>,
) -> Option<GraphicAttributeReply> {
    let graphic_attribute_error = find_graphic_attribute_error(attribute_status);
    if let Some(graphic_attribute_error) = graphic_attribute_error {
        return reply_parameter_values
            .is_empty()
            .then_some(GraphicAttributeReply::Palette(Err(graphic_attribute_error)));
    }
    if attribute_status != 0 || reply_parameter_values.len() != 1 {
        return None;
    }
    let palette_entry_count = parse_decimal(reply_parameter_values[0])?;
    (palette_entry_count != 0).then_some(GraphicAttributeReply::Palette(Ok(palette_entry_count)))
}

fn parse_geometry_reply(
    attribute_status: u32,
    reply_parameter_values: Vec<&[u8]>,
) -> Option<GraphicAttributeReply> {
    let graphic_attribute_error = find_graphic_attribute_error(attribute_status);
    if let Some(graphic_attribute_error) = graphic_attribute_error {
        return reply_parameter_values
            .is_empty()
            .then_some(GraphicAttributeReply::Geometry(Err(
                graphic_attribute_error,
            )));
    }
    if attribute_status != 0 || reply_parameter_values.len() != 2 {
        return None;
    }
    let pixel_width = parse_decimal(reply_parameter_values[0])?;
    let pixel_height = parse_decimal(reply_parameter_values[1])?;
    Some(GraphicAttributeReply::Geometry(Ok((
        pixel_width,
        pixel_height,
    ))))
}

fn find_graphic_attribute_error(attribute_status: u32) -> Option<GraphicAttributeError> {
    match attribute_status {
        1 => Some(GraphicAttributeError::InvalidItem),
        2 => Some(GraphicAttributeError::InvalidAction),
        3 => Some(GraphicAttributeError::Failure),
        _ => None,
    }
}

fn parse_modified_key(csi_body: &[u8], final_byte: u8) -> Option<KeyEvent> {
    let mut key_parameter_fields = csi_body.split(|field_byte| *field_byte == b';');
    let first_key_parameter = key_parameter_fields.next()?;
    if !first_key_parameter.is_empty() && parse_decimal(first_key_parameter)? != 1 {
        return None;
    }
    let (modifiers, key_event_kind) = parse_modifier_or_default(key_parameter_fields.next())?;
    if key_parameter_fields.next().is_some() {
        return None;
    }
    let key_code = match final_byte {
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
        code: key_code,
        key_event_kind,
        modifiers,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
    })
}

fn parse_tilde_key(csi_body: &[u8]) -> Option<KeyEvent> {
    let mut key_parameter_fields = csi_body.split(|field_byte| *field_byte == b';');
    let encoded_key_number = parse_decimal(key_parameter_fields.next()?)?;
    let (modifiers, key_event_kind) = parse_modifier_or_default(key_parameter_fields.next())?;
    if key_parameter_fields.next().is_some() {
        return None;
    }
    let key_code = match encoded_key_number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::Function(u8::try_from(encoded_key_number - 10).ok()?),
        17..=21 => KeyCode::Function(u8::try_from(encoded_key_number - 11).ok()?),
        23..=26 => KeyCode::Function(u8::try_from(encoded_key_number - 12).ok()?),
        28..=29 => KeyCode::Function(u8::try_from(encoded_key_number - 15).ok()?),
        31..=34 => KeyCode::Function(u8::try_from(encoded_key_number - 17).ok()?),
        _ => return None,
    };
    Some(KeyEvent {
        code: key_code,
        key_event_kind,
        modifiers,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
    })
}

fn parse_kitty_key(csi_body: &[u8]) -> Option<KeyEvent> {
    let mut key_parameter_fields = csi_body.split(|field_byte| *field_byte == b';');
    let encoded_key_parameter = key_parameter_fields.next()?;
    let mut key_code_fields = encoded_key_parameter.split(|field_byte| *field_byte == b':');
    let codepoint = parse_decimal(key_code_fields.next()?)?;
    let shifted_key = parse_alternate_key(key_code_fields.next())?;
    let base_layout_key = parse_alternate_key(key_code_fields.next())?;
    if key_code_fields.next().is_some() {
        return None;
    }
    let (modifiers, key_event_kind) = parse_modifier_or_default(key_parameter_fields.next())?;
    let associated_text = parse_associated_text(key_parameter_fields.next());
    if key_parameter_fields.next().is_some() {
        return None;
    }
    let key_code = find_reported_key(codepoint, modifiers)?;
    Some(KeyEvent {
        code: key_code,
        key_event_kind,
        modifiers,
        shifted_key,
        base_layout_key,
        associated_text,
    })
}

/// The character one alternate-key sub-field names, or `None` when the field
/// is absent or empty.
///
/// The outer `Option` separates a refusal from an absent field: `Some(None)`
/// is an absent or empty field, and `None` is a field that names no character.
/// A base layout key with no shifted key arrives as `CSI key::base`, so an
/// empty sub-field is ordinary.
fn parse_alternate_key(alternate_key_field: Option<&[u8]>) -> Option<Option<char>> {
    match alternate_key_field {
        None => Some(None),
        Some([]) => Some(None),
        Some(alternate_key_field) => {
            let codepoint = parse_decimal(alternate_key_field)?;
            Some(Some(char::from_u32(codepoint)?))
        }
    }
}

/// The text one key event produced, from the colon-separated codepoints of
/// the third parameter. An absent or empty field produces empty text.
///
/// A malformed field produces empty text and never refuses the event, because
/// the key is named by the first parameter and stands on its own. `CSI 13;;13u`
/// and `CSI 13;;1114112u` both stay the Enter key and carry no text: a carriage
/// return is not text a key produced, and `1114112` is no character at all.
fn parse_associated_text(text_field: Option<&[u8]>) -> String {
    let Some(text_field) = text_field.filter(|field| !field.is_empty()) else {
        return String::new();
    };
    let mut associated_text = String::new();
    for text_codepoint_field in text_field.split(|field_byte| *field_byte == b':') {
        let Some(character) = parse_decimal(text_codepoint_field).and_then(char::from_u32) else {
            return String::new();
        };
        if character.is_control() {
            return String::new();
        }
        associated_text.push(character);
    }
    associated_text
}

/// The key one reported codepoint names, or `None` when the codepoint names
/// no character.
///
/// A codepoint with a Koshi key form takes it. A codepoint without one keeps
/// its number through [`KeyCode::Codepoint`], so a key the binding grammar
/// cannot name still reaches the event: Left Shift is `Unnamed(57441)`, and a
/// text-only event is `Unnamed(0)`. A codepoint that is no Unicode scalar
/// value — a surrogate, or a number above `1114111` — is refused rather than
/// given an identity it does not have.
fn find_reported_key(codepoint: u32, modifiers: Modifiers) -> Option<KeyCode> {
    if let Some(key_code) = find_functional_key(codepoint) {
        return Some(key_code);
    }
    if codepoint == TEXT_ONLY_KEY_CODEPOINT {
        return Some(KeyCode::Codepoint(codepoint));
    }
    let character = char::from_u32(codepoint)?;
    Some(match character {
        '\x1b' => KeyCode::Escape,
        '\r' => KeyCode::Enter,
        '\t' if modifiers.has_all_modifiers(Modifiers::SHIFT) => KeyCode::BackTab,
        '\t' => KeyCode::Tab,
        '\x7f' => KeyCode::Backspace,
        character => KeyCode::Char(character),
    })
}

/// The `CSI ? flags u` answer to a `CSI ? u` query, as the flag bits the
/// terminal reports active.
///
/// `ESC [ ? 7 u` reports flags 1, 2 and 4. A value above the eight bits a
/// `u8` holds is refused rather than truncated.
fn parse_keyboard_enhancement_flags(flag_field: &[u8]) -> Option<Event> {
    let enhancement_flags = u8::try_from(parse_decimal(flag_field)?).ok()?;
    Some(Event::KeyboardEnhancementFlags(enhancement_flags))
}

fn find_functional_key(codepoint: u32) -> Option<KeyCode> {
    let key_code = match codepoint {
        57_376..=57_387 => KeyCode::Function(u8::try_from(codepoint - 57_376 + 13).ok()?),
        57_388..=57_398 | 57_358..=57_363 | 57_428..=57_454 => KeyCode::Codepoint(codepoint),
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
        57_427 => KeyCode::Codepoint(codepoint),
        _ => return None,
    };
    Some(key_code)
}

fn parse_modifier_or_default(modifier_field: Option<&[u8]>) -> Option<(Modifiers, KeyEventKind)> {
    match modifier_field {
        Some(modifier_field) if !modifier_field.is_empty() => parse_modifier_field(modifier_field),
        _ => Some((Modifiers::NONE, KeyEventKind::Press)),
    }
}

fn parse_modifier_field(modifier_field: &[u8]) -> Option<(Modifiers, KeyEventKind)> {
    let mut modifier_parts = modifier_field.split(|field_byte| *field_byte == b':');
    let encoded_modifier = parse_decimal(modifier_parts.next()?)?;
    let modifier_bits = encoded_modifier.checked_sub(1)?;
    let mut modifiers = Modifiers::NONE;
    for (modifier_bit, modifier) in [
        (1, Modifiers::SHIFT),
        (2, Modifiers::ALT),
        (4, Modifiers::CONTROL),
        (8, Modifiers::SUPER),
        (16, Modifiers::HYPER),
        (32, Modifiers::META),
        (64, Modifiers::CAPS_LOCK),
        (128, Modifiers::NUM_LOCK),
    ] {
        if modifier_bits & modifier_bit != 0 {
            modifiers |= modifier;
        }
    }
    let key_event_kind = match modifier_parts.next() {
        None => KeyEventKind::Press,
        Some(event_kind_field) => match parse_decimal(event_kind_field)? {
            1 => KeyEventKind::Press,
            2 => KeyEventKind::Repeat,
            3 => KeyEventKind::Release,
            _ => return None,
        },
    };
    if modifier_parts.next().is_some() {
        return None;
    }
    Some((modifiers, key_event_kind))
}

fn parse_sgr_mouse(csi_body: &[u8], final_byte: u8) -> Option<MouseEvent> {
    let mut mouse_fields = csi_body.split(|field_byte| *field_byte == b';');
    let mouse_button_code = u8::try_from(parse_decimal(mouse_fields.next()?)?).ok()?;
    let mouse_column = u16::try_from(parse_decimal(mouse_fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    let mouse_row = u16::try_from(parse_decimal(mouse_fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    if mouse_fields.next().is_some() {
        return None;
    }
    let (mut mouse_event_kind, modifiers) = decode_mouse_code(mouse_button_code)?;
    if final_byte == b'm' {
        if let MouseEventKind::Down(button) = mouse_event_kind {
            mouse_event_kind = MouseEventKind::Up(button);
        }
    }
    Some(MouseEvent {
        mouse_event_kind,
        column: mouse_column,
        row: mouse_row,
        modifiers,
    })
}

fn parse_rxvt_mouse(csi_body: &[u8]) -> Option<MouseEvent> {
    let mut mouse_fields = csi_body.split(|field_byte| *field_byte == b';');
    let mouse_button_code = u8::try_from(parse_decimal(mouse_fields.next()?)?)
        .ok()?
        .checked_sub(32)?;
    let mouse_column = u16::try_from(parse_decimal(mouse_fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    let mouse_row = u16::try_from(parse_decimal(mouse_fields.next()?)?)
        .ok()?
        .checked_sub(1)?;
    if mouse_fields.next().is_some() {
        return None;
    }
    let (mouse_event_kind, modifiers) = decode_mouse_code(mouse_button_code)?;
    Some(MouseEvent {
        mouse_event_kind,
        column: mouse_column,
        row: mouse_row,
        modifiers,
    })
}

fn parse_x10_mouse(control_sequence: &[u8]) -> Option<Event> {
    let [b'M', mouse_button_code, encoded_column_byte, encoded_row_byte] = control_sequence else {
        return None;
    };
    let (mouse_event_kind, modifiers) = decode_mouse_code(mouse_button_code.checked_sub(32)?)?;
    Some(Event::Mouse(MouseEvent {
        mouse_event_kind,
        column: u16::from(encoded_column_byte.checked_sub(33)?),
        row: u16::from(encoded_row_byte.checked_sub(33)?),
        modifiers,
    }))
}

fn decode_mouse_code(mouse_button_code: u8) -> Option<(MouseEventKind, Modifiers)> {
    let mouse_button_number = (mouse_button_code & 0b11) | ((mouse_button_code & 0b1100_0000) >> 4);
    let is_drag = mouse_button_code & 0b0010_0000 != 0;
    let mouse_event_kind = match (mouse_button_number, is_drag) {
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
    if mouse_button_code & 4 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if mouse_button_code & 8 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if mouse_button_code & 16 != 0 {
        modifiers |= Modifiers::CONTROL;
    }
    Some((mouse_event_kind, modifiers))
}

fn parse_kitty_reply(apc_payload: &[u8]) -> Option<KittyGraphicsReply> {
    let graphics_payload = apc_payload.strip_prefix(b"G")?;
    let separator_index = graphics_payload
        .iter()
        .position(|payload_byte| *payload_byte == b';')?;
    let (control_fields, message_bytes) = graphics_payload.split_at(separator_index);
    let message_bytes = message_bytes.get(1..)?;
    if message_bytes.is_empty()
        || !message_bytes
            .iter()
            .all(|message_byte| (b' '..=b'~').contains(message_byte))
    {
        return None;
    }
    let mut image_id = None;
    for control_pair in control_fields.split(|field_byte| *field_byte == b',') {
        let equals_index = control_pair
            .iter()
            .position(|field_byte| *field_byte == b'=')?;
        let (control_key, control_parameter_bytes) = control_pair.split_at(equals_index);
        if control_key == b"i" {
            if image_id.is_some() {
                return None;
            }
            image_id = Some(parse_decimal(control_parameter_bytes.get(1..)?)?);
        }
    }
    Some(KittyGraphicsReply {
        image_id: image_id?,
        is_successful: message_bytes == b"OK",
    })
}

fn parse_decimal(decimal_bytes: &[u8]) -> Option<u32> {
    if decimal_bytes.is_empty() {
        return None;
    }
    decimal_bytes
        .iter()
        .try_fold(0_u32, |accumulated_decimal_number, decimal_byte| {
            accumulated_decimal_number
                .checked_mul(10)?
                .checked_add(u32::from(decimal_byte.checked_sub(b'0')?))
                .filter(|_| decimal_byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests;
