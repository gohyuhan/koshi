//! Bounded decoding for terminal image escape sequences.
//!
//! The decoder accepts Sixel, kitty graphics, and iTerm2 inline image
//! transfers. It turns each complete image into RGBA pixels and never writes
//! to a terminal grid. The terminal engine adds the cursor position at which
//! the sequence ended, then applies display records to terminal image state.

mod commands;
pub(crate) use commands::{KittyCommand, KittyCommandKind, KittyDelete};
use std::ops::Range;

#[cfg(test)]
use base64::engine::general_purpose::STANDARD;
#[cfg(test)]
use base64::Engine;
use koshi_image::BoundedBytesSeed;
use koshi_iterm::{
    can_iterm_command_be_graphics, is_iterm_graphics_command, is_iterm_payload_started,
    parse_iterm_command, ItermTransfer,
};
use koshi_kitty::{
    parse_kitty_command, parse_reply_display, start_kitty_animation_transfer, start_kitty_transfer,
    KittyAnimationChunk, KittyAnimationTransfer, KittyAnimationTransferOutcome, KittyChunk,
    KittyParser, KittyTransfer, KittyTransferOutcome, MAX_KITTY_CHUNK_BYTE_COUNT,
};
use koshi_sixel::{
    SixelGraphic, SixelParser as ProtocolSixelParser, SixelPhase as ProtocolSixelPhase,
};
use serde::de::{self, DeserializeSeed, MapAccess, Visitor};
use serde::{Deserialize, Deserializer as DeserializerTrait, Serialize};

pub(crate) use koshi_image::compute_rgba_byte_count;
pub use koshi_image::{
    DecodedAnimation, DecodedGraphics, DecodedImage, GraphicsError, GraphicsProtocol, ImageAction,
    ImageDimension, ImageDisplay, ImagePlacementError, ImageRecord, SixelBackground,
    MAX_GRAPHICS_CARRY_BYTE_COUNT, MAX_GRAPHICS_CONTROL_BYTE_COUNT,
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT, MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
};

/// The largest GNU Screen passthrough body accepted by this parser.
const MAX_SCREEN_PASSTHROUGH_BYTE_COUNT: usize = 768;

/// The deepest tmux or GNU Screen wrapper accepted around one image stream.
const MAX_GRAPHICS_WRAPPER_DEPTH: usize = 8;

/// The graphics parser state that cannot be rebuilt after an engine replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsAbandonment {
    /// Consume the remainder of one open protocol string and report its limit.
    Sequence(GraphicsProtocol),
    /// Consume the remainder of one ignored protocol string without reporting
    /// a graphics event.
    SilentSequence(GraphicsProtocol),
    /// Consume following multipart records until their final record and report the
    /// limit once that record arrives.
    Transfer(GraphicsProtocol),
}

/// Parser state carried across a terminal-engine replacement.
///
/// `carry_bytes` rebuilds this parser's own active sequence or multipart transfer.
/// The two nested records rebuild parsers inside a split GNU Screen or tmux
/// wrapper. A one-pixel red iTerm2 transfer split after `ESC ] 1337;File` has
/// the outer wrapper in one record and the unfinished iTerm2 command in its
/// nested record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphicsTransportState {
    /// Bytes that rebuild this parser's own active sequence or transfer.
    #[serde(rename = "carry", default)]
    pub carry_bytes: Vec<u8>,
    /// Whether [`carry_bytes`](Self::carry_bytes) contains a complete bounded rebuild.
    /// `false` means [`graphics_abandonment`](Self::graphics_abandonment) describes how to drain
    /// the open transfer after restore.
    #[serde(rename = "carryable", default = "default_is_true")]
    pub is_carryable: bool,
    /// How to drain an open sequence when `is_carryable` is false.
    #[serde(rename = "abandonment", default)]
    pub graphics_abandonment: Option<GraphicsAbandonment>,
    /// Whether the next DCS is a GNU Screen continuation wrapper.
    #[serde(rename = "screen_continuation", default)]
    pub is_screen_continuation: bool,
    /// Whether the carried bytes are inside an open GNU Screen wrapper.
    #[serde(rename = "screen_wrapper_active", default)]
    pub is_screen_wrapper_active: bool,
    /// The parser state inside the carried GNU Screen wrapper, when that
    /// wrapper ended while its enclosed stream was incomplete.
    #[serde(rename = "screen_inner", default)]
    pub screen_inner_transport: Option<Box<GraphicsTransportState>>,
    /// Whether the next DCS is a tmux continuation wrapper.
    #[serde(rename = "tmux_continuation", default)]
    pub is_tmux_continuation: bool,
    /// Whether the carried bytes are inside an open tmux wrapper.
    #[serde(rename = "tmux_wrapper_active", default)]
    pub is_tmux_wrapper_active: bool,
    /// The parser state inside the carried tmux wrapper, when that wrapper
    /// ended while its enclosed stream was incomplete.
    #[serde(rename = "tmux_inner", default)]
    pub tmux_inner_transport: Option<Box<GraphicsTransportState>>,
}

impl Default for GraphicsTransportState {
    fn default() -> Self {
        GraphicsTransportState {
            carry_bytes: Vec::new(),
            is_carryable: true,
            graphics_abandonment: None,
            is_screen_continuation: false,
            is_screen_wrapper_active: false,
            screen_inner_transport: None,
            is_tmux_continuation: false,
            is_tmux_wrapper_active: false,
            tmux_inner_transport: None,
        }
    }
}

impl<'de> Deserialize<'de> for GraphicsTransportState {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_map(GraphicsTransportVisitor { wrapper_depth: 0 })
    }
}

struct GraphicsTransportVisitor {
    wrapper_depth: usize,
}

impl<'de> Visitor<'de> for GraphicsTransportVisitor {
    type Value = GraphicsTransportState;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a graphics transport state object")
    }

    fn visit_map<MapReader>(
        self,
        mut transport_state_map: MapReader,
    ) -> Result<Self::Value, MapReader::Error>
    where
        MapReader: MapAccess<'de>,
    {
        let mut carry_bytes = None;
        let mut is_carryable = None;
        let mut graphics_abandonment = None;
        let mut is_screen_continuation = None;
        let mut is_screen_wrapper_active = None;
        let mut screen_inner_transport = None;
        let mut is_tmux_continuation = None;
        let mut is_tmux_wrapper_active = None;
        let mut tmux_inner_transport = None;

        while let Some(field_name) = transport_state_map.next_key::<String>()? {
            match field_name.as_str() {
                "carry" => {
                    if carry_bytes.is_some() {
                        return Err(de::Error::duplicate_field("carry"));
                    }
                    carry_bytes = Some(transport_state_map.next_value_seed(
                        BoundedBytesSeed::from_byte_limit_and_error_label(
                            MAX_GRAPHICS_CARRY_BYTE_COUNT,
                            "graphics carry",
                        ),
                    )?);
                }
                "carryable" => {
                    if is_carryable.is_some() {
                        return Err(de::Error::duplicate_field("carryable"));
                    }
                    is_carryable = Some(transport_state_map.next_value()?);
                }
                "abandonment" => {
                    if graphics_abandonment.is_some() {
                        return Err(de::Error::duplicate_field("abandonment"));
                    }
                    graphics_abandonment = Some(transport_state_map.next_value()?);
                }
                "screen_continuation" => {
                    if is_screen_continuation.is_some() {
                        return Err(de::Error::duplicate_field("screen_continuation"));
                    }
                    is_screen_continuation = Some(transport_state_map.next_value()?);
                }
                "screen_wrapper_active" => {
                    if is_screen_wrapper_active.is_some() {
                        return Err(de::Error::duplicate_field("screen_wrapper_active"));
                    }
                    is_screen_wrapper_active = Some(transport_state_map.next_value()?);
                }
                "screen_inner" => {
                    if screen_inner_transport.is_some() {
                        return Err(de::Error::duplicate_field("screen_inner"));
                    }
                    screen_inner_transport = Some(transport_state_map.next_value_seed(
                        GraphicsTransportOptionSeed {
                            wrapper_depth: self.wrapper_depth.saturating_add(1),
                        },
                    )?);
                }
                "tmux_continuation" => {
                    if is_tmux_continuation.is_some() {
                        return Err(de::Error::duplicate_field("tmux_continuation"));
                    }
                    is_tmux_continuation = Some(transport_state_map.next_value()?);
                }
                "tmux_wrapper_active" => {
                    if is_tmux_wrapper_active.is_some() {
                        return Err(de::Error::duplicate_field("tmux_wrapper_active"));
                    }
                    is_tmux_wrapper_active = Some(transport_state_map.next_value()?);
                }
                "tmux_inner" => {
                    if tmux_inner_transport.is_some() {
                        return Err(de::Error::duplicate_field("tmux_inner"));
                    }
                    tmux_inner_transport = Some(transport_state_map.next_value_seed(
                        GraphicsTransportOptionSeed {
                            wrapper_depth: self.wrapper_depth.saturating_add(1),
                        },
                    )?);
                }
                _ => {
                    let _: de::IgnoredAny = transport_state_map.next_value()?;
                }
            }
        }

        Ok(GraphicsTransportState {
            carry_bytes: carry_bytes.unwrap_or_default(),
            is_carryable: is_carryable.unwrap_or_else(default_is_true),
            graphics_abandonment: graphics_abandonment.unwrap_or_default(),
            is_screen_continuation: is_screen_continuation.unwrap_or(false),
            is_screen_wrapper_active: is_screen_wrapper_active.unwrap_or(false),
            screen_inner_transport: screen_inner_transport.unwrap_or_default(),
            is_tmux_continuation: is_tmux_continuation.unwrap_or(false),
            is_tmux_wrapper_active: is_tmux_wrapper_active.unwrap_or(false),
            tmux_inner_transport: tmux_inner_transport.unwrap_or_default(),
        })
    }
}

struct GraphicsTransportOptionSeed {
    wrapper_depth: usize,
}

impl<'de> DeserializeSeed<'de> for GraphicsTransportOptionSeed {
    type Value = Option<Box<GraphicsTransportState>>;

    fn deserialize<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        deserializer.deserialize_option(GraphicsTransportOptionVisitor {
            wrapper_depth: self.wrapper_depth,
        })
    }
}

struct GraphicsTransportOptionVisitor {
    wrapper_depth: usize,
}

impl<'de> Visitor<'de> for GraphicsTransportOptionVisitor {
    type Value = Option<Box<GraphicsTransportState>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("null or a nested graphics transport state object")
    }

    fn visit_none<Error>(self) -> Result<Self::Value, Error>
    where
        Error: de::Error,
    {
        Ok(None)
    }

    fn visit_some<Deserializer>(
        self,
        deserializer: Deserializer,
    ) -> Result<Self::Value, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        if self.wrapper_depth > MAX_GRAPHICS_WRAPPER_DEPTH {
            return Err(de::Error::custom(
                "graphics wrapper nesting exceeds the supported limit",
            ));
        }
        let transport_state = deserializer.deserialize_map(GraphicsTransportVisitor {
            wrapper_depth: self.wrapper_depth,
        })?;
        Ok(Some(Box::new(transport_state)))
    }
}

fn default_is_true() -> bool {
    true
}

/// One completed graphics operation in terminal byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphicsOperation {
    Failure {
        image_display: ImageDisplay,
        graphics_error: GraphicsError,
    },
    Image(DecodedGraphics),
    Sixel(SixelGraphic),
    Command(KittyCommand),
}

/// Terminal graphics parser that frames protocol strings and queues operations.
#[derive(Clone)]
pub(crate) struct GraphicsParser {
    graphics_state: GraphicsState,
    pending_graphics_bytes: Vec<u8>,
    is_carryable: bool,
    graphics_sequence_byte_count: usize,
    wrapper_depth: usize,
    graphics_transfer_carry_bytes: Vec<u8>,
    is_transfer_carryable: bool,
    abandoned_transfer_protocol: Option<GraphicsProtocol>,
    is_screen_continuation: bool,
    screen_inner_parser: Option<Box<GraphicsParser>>,
    is_tmux_continuation: bool,
    tmux_inner_parser: Option<Box<GraphicsParser>>,
    kitty_transfer: Option<KittyTransfer>,
    kitty_animation_transfer: Option<KittyAnimationTransfer>,
    iterm_transfer: Option<ItermTransfer>,
    remaining_utf8_continuation_count: u8,
    pending_utf8_bytes: Vec<u8>,
}

pub(crate) struct GraphicsParseAdvance {
    pub(crate) completed_graphics_events: Vec<(usize, Result<GraphicsOperation, GraphicsError>)>,
    pub(crate) terminal_inert_ranges: Vec<Range<usize>>,
}

impl Default for GraphicsParser {
    fn default() -> Self {
        GraphicsParser {
            graphics_state: GraphicsState::default(),
            pending_graphics_bytes: Vec::new(),
            is_carryable: true,
            graphics_sequence_byte_count: 0,
            wrapper_depth: 0,
            graphics_transfer_carry_bytes: Vec::new(),
            is_transfer_carryable: true,
            abandoned_transfer_protocol: None,
            is_screen_continuation: false,
            screen_inner_parser: None,
            is_tmux_continuation: false,
            tmux_inner_parser: None,
            kitty_transfer: None,
            kitty_animation_transfer: None,
            iterm_transfer: None,
            remaining_utf8_continuation_count: 0,
            pending_utf8_bytes: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
enum GraphicsState {
    #[default]
    Ground,
    Escape,
    DcsIntro,
    Sixel(Box<ProtocolSixelParser>),
    Kitty(KittyParser),
    Iterm2(ItermParser),
    Tmux(TmuxParser),
    Screen(ScreenParser),
    Discard(DiscardParser),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Dcs,
    Apc,
    Osc,
}

impl StringKind {
    fn resolve_graphics_protocol(self) -> GraphicsProtocol {
        match self {
            StringKind::Dcs => GraphicsProtocol::Sixel,
            StringKind::Apc => GraphicsProtocol::Kitty,
            StringKind::Osc => GraphicsProtocol::Iterm2,
        }
    }
}

fn resolve_string_kind(protocol: GraphicsProtocol) -> StringKind {
    match protocol {
        GraphicsProtocol::Sixel => StringKind::Dcs,
        GraphicsProtocol::Kitty => StringKind::Apc,
        GraphicsProtocol::Iterm2 => StringKind::Osc,
    }
}

#[derive(Clone)]
struct DiscardParser {
    discarded_string_kind: StringKind,
    graphics_error: GraphicsError,
    is_escaped: bool,
    should_report: bool,
}

impl GraphicsParser {
    /// Feed bytes and return every image or error completed by this chunk.
    pub(crate) fn process_graphics_operations(
        &mut self,
        graphics_bytes: &[u8],
    ) -> Vec<Result<GraphicsOperation, GraphicsError>> {
        self.process_graphics_operations_with_offsets(graphics_bytes)
            .completed_graphics_events
            .into_iter()
            .map(|(_, graphics_event)| graphics_event)
            .collect()
    }

    pub(crate) fn process_graphics_operations_with_offsets(
        &mut self,
        graphics_bytes: &[u8],
    ) -> GraphicsParseAdvance {
        let mut graphics_events = Vec::new();
        let mut terminal_inert_ranges = Vec::new();
        let mut completed_graphics_events = Vec::new();
        let mut byte_offset = 0;
        while byte_offset < graphics_bytes.len() {
            if self.consume_utf8_byte(graphics_bytes[byte_offset]) {
                byte_offset += 1;
                continue;
            }
            if let Some(consumed_byte_count) =
                self.feed_discard_bytes(&graphics_bytes[byte_offset..])
            {
                byte_offset += consumed_byte_count;
                continue;
            }
            if let Some(consumed_byte_count) = self
                .feed_kitty_bytes(&graphics_bytes[byte_offset..])
                .or_else(|| self.feed_iterm_bytes(&graphics_bytes[byte_offset..]))
            {
                terminal_inert_ranges.push(byte_offset..byte_offset + consumed_byte_count);
                byte_offset += consumed_byte_count;
                continue;
            }
            if let Some(consumed_byte_count) = self.feed_tmux_bytes(&graphics_bytes[byte_offset..])
            {
                terminal_inert_ranges.push(byte_offset..byte_offset + consumed_byte_count);
                byte_offset += consumed_byte_count;
                continue;
            }
            if let Some(consumed_byte_count) =
                self.feed_screen_bytes(&graphics_bytes[byte_offset..])
            {
                terminal_inert_ranges.push(byte_offset..byte_offset + consumed_byte_count);
                byte_offset += consumed_byte_count;
                continue;
            }
            let is_sixel_payload = matches!(
                &self.graphics_state,
                GraphicsState::Sixel(parser)
                    if parser.phase == ProtocolSixelPhase::Body
                        && !parser.is_escaped
                        && graphics_bytes[byte_offset].is_ascii_graphic()
            );
            completed_graphics_events.clear();
            self.feed_graphics_byte(graphics_bytes[byte_offset], &mut completed_graphics_events);
            graphics_events.extend(
                completed_graphics_events
                    .drain(..)
                    .map(|graphics_event| (byte_offset, graphics_event)),
            );
            if is_sixel_payload && matches!(self.graphics_state, GraphicsState::Sixel(_)) {
                extend_terminal_inert_range(&mut terminal_inert_ranges, byte_offset);
            }
            byte_offset += 1;
        }
        GraphicsParseAdvance {
            completed_graphics_events: graphics_events,
            terminal_inert_ranges,
        }
    }

    fn consume_utf8_byte(&mut self, graphics_byte: u8) -> bool {
        if !matches!(self.graphics_state, GraphicsState::Ground) {
            return false;
        }
        if self.remaining_utf8_continuation_count != 0 {
            if (graphics_byte & 0xc0) == 0x80 {
                self.remaining_utf8_continuation_count -= 1;
                self.pending_utf8_bytes.push(graphics_byte);
                if self.remaining_utf8_continuation_count == 0 {
                    self.pending_utf8_bytes.clear();
                }
                return true;
            }
            self.remaining_utf8_continuation_count = 0;
            self.pending_utf8_bytes.clear();
        }
        let Some(remaining_utf8_continuation_count) = (match graphics_byte {
            0xc2..=0xdf => Some(1),
            0xe0..=0xef => Some(2),
            0xf0..=0xf4 => Some(3),
            _ => None,
        }) else {
            return false;
        };
        self.remaining_utf8_continuation_count = remaining_utf8_continuation_count;
        self.pending_utf8_bytes.clear();
        self.pending_utf8_bytes.push(graphics_byte);
        true
    }

    /// Advance one data run in a discarded control string.
    fn feed_discard_bytes(&mut self, graphics_bytes: &[u8]) -> Option<usize> {
        let discarded_string_kind = match &self.graphics_state {
            GraphicsState::Discard(parser) if !parser.is_escaped => parser.discarded_string_kind,
            _ => return None,
        };
        let consumed_byte_count = graphics_bytes
            .iter()
            .position(|discarded_byte| {
                matches!(*discarded_byte, 0x18 | 0x1a | 0x1b | 0x9c)
                    || (discarded_string_kind == StringKind::Osc && *discarded_byte == 0x07)
            })
            .unwrap_or(graphics_bytes.len());
        if consumed_byte_count == 0 {
            return None;
        }

        self.push_pending_graphics_bytes(&graphics_bytes[..consumed_byte_count]);
        self.graphics_sequence_byte_count = self
            .graphics_sequence_byte_count
            .saturating_add(consumed_byte_count);
        Some(consumed_byte_count)
    }

    /// Copy one run of Kitty payload bytes that contains no string control.
    fn feed_kitty_bytes(&mut self, graphics_bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Kitty(parser) = &self.graphics_state else {
            return None;
        };
        if parser.is_ignored() || parser.is_escaped() || !parser.has_received_control_header() {
            return None;
        }

        let kitty_payload_byte_capacity =
            MAX_KITTY_CHUNK_BYTE_COUNT.saturating_sub(parser.get_payload_byte_count());
        let sequence_byte_capacity =
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT.saturating_sub(self.graphics_sequence_byte_count);
        let consumed_byte_count = compute_base64_run_byte_count(
            graphics_bytes,
            kitty_payload_byte_capacity.min(sequence_byte_capacity),
        );
        if consumed_byte_count == 0 {
            return None;
        }

        self.push_pending_graphics_bytes(&graphics_bytes[..consumed_byte_count]);
        self.graphics_sequence_byte_count = self
            .graphics_sequence_byte_count
            .saturating_add(consumed_byte_count);
        let GraphicsState::Kitty(parser) = &mut self.graphics_state else {
            unreachable!("the Kitty parser state was checked above")
        };
        parser
            .append_payload_bytes(&graphics_bytes[..consumed_byte_count])
            .expect("the bounded Kitty payload run fits");
        Some(consumed_byte_count)
    }

    /// Copy one run of iTerm2 payload bytes that contains no string control.
    fn feed_iterm_bytes(&mut self, graphics_bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Iterm2(parser) = &self.graphics_state else {
            return None;
        };
        if parser.is_ignored
            || parser.is_escaped
            || !parser.is_prefix_complete
            || !is_iterm_payload_started(&parser.command_bytes)
        {
            return None;
        }

        let iterm_payload_byte_capacity =
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT.saturating_sub(parser.command_bytes.len());
        let sequence_byte_capacity =
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT.saturating_sub(self.graphics_sequence_byte_count);
        let consumed_byte_count = compute_base64_run_byte_count(
            graphics_bytes,
            iterm_payload_byte_capacity.min(sequence_byte_capacity),
        );
        if consumed_byte_count == 0 {
            return None;
        }

        self.push_pending_graphics_bytes(&graphics_bytes[..consumed_byte_count]);
        self.graphics_sequence_byte_count = self
            .graphics_sequence_byte_count
            .saturating_add(consumed_byte_count);
        let GraphicsState::Iterm2(parser) = &mut self.graphics_state else {
            unreachable!("the iTerm2 parser state was checked above")
        };
        parser
            .command_bytes
            .extend_from_slice(&graphics_bytes[..consumed_byte_count]);
        Some(consumed_byte_count)
    }

    /// Copy one run of tmux wrapper bytes that contains no wrapper control.
    fn feed_tmux_bytes(&mut self, graphics_bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Tmux(parser) = &self.graphics_state else {
            return None;
        };
        if parser.prefix_bytes.len() < b"tmux;".len() || parser.is_escaped {
            return None;
        }

        let payload_byte_capacity =
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT.saturating_sub(parser.payload_bytes.len());
        let sequence_byte_capacity =
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT.saturating_sub(self.graphics_sequence_byte_count);
        let max_consumed_byte_count = graphics_bytes
            .len()
            .min(payload_byte_capacity)
            .min(sequence_byte_capacity);
        let consumed_byte_count = graphics_bytes[..max_consumed_byte_count]
            .iter()
            .position(|wrapper_byte| matches!(*wrapper_byte, 0x18 | 0x1a | 0x1b | 0x9c))
            .unwrap_or(max_consumed_byte_count);
        if consumed_byte_count == 0 {
            return None;
        }

        self.push_pending_graphics_bytes(&graphics_bytes[..consumed_byte_count]);
        self.graphics_sequence_byte_count = self
            .graphics_sequence_byte_count
            .saturating_add(consumed_byte_count);
        let GraphicsState::Tmux(parser) = &mut self.graphics_state else {
            unreachable!("the tmux parser state was checked above")
        };
        parser
            .payload_bytes
            .extend_from_slice(&graphics_bytes[..consumed_byte_count]);
        Some(consumed_byte_count)
    }

    /// Copy one run of GNU Screen wrapper bytes that contains no wrapper control.
    fn feed_screen_bytes(&mut self, graphics_bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Screen(parser) = &self.graphics_state else {
            return None;
        };
        if parser.is_escaped
            || (parser.payload_bytes.as_slice() == [0x1b]
                && graphics_bytes.first().copied() == Some(b'\\'))
        {
            return None;
        }

        let screen_payload_byte_capacity =
            MAX_SCREEN_PASSTHROUGH_BYTE_COUNT.saturating_sub(parser.payload_bytes.len());
        let sequence_byte_capacity =
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT.saturating_sub(self.graphics_sequence_byte_count);
        let max_consumed_byte_count = graphics_bytes
            .len()
            .min(screen_payload_byte_capacity)
            .min(sequence_byte_capacity);
        let consumed_byte_count = graphics_bytes[..max_consumed_byte_count]
            .iter()
            .position(|wrapper_byte| matches!(*wrapper_byte, 0x18 | 0x1a | 0x1b | 0x9c))
            .unwrap_or(max_consumed_byte_count);
        if consumed_byte_count == 0 {
            return None;
        }

        self.push_pending_graphics_bytes(&graphics_bytes[..consumed_byte_count]);
        self.graphics_sequence_byte_count = self
            .graphics_sequence_byte_count
            .saturating_add(consumed_byte_count);
        let GraphicsState::Screen(parser) = &mut self.graphics_state else {
            unreachable!("the GNU Screen parser state was checked above")
        };
        parser
            .payload_bytes
            .extend_from_slice(&graphics_bytes[..consumed_byte_count]);
        Some(consumed_byte_count)
    }

    /// Return bytes needed to rebuild an active graphics parser after a
    /// process-image swap. An empty slice means that the active transfer
    /// exceeded the carry bound and must not be resumed from its opening.
    pub(crate) fn get_graphics_carry_bytes(&self) -> Option<&[u8]> {
        if matches!(self.graphics_state, GraphicsState::Ground) {
            if !self.pending_utf8_bytes.is_empty() {
                return Some(&self.pending_utf8_bytes);
            }
            if let Some(inner_parser) = &self.screen_inner_parser {
                return inner_parser.get_graphics_carry_bytes();
            }
            if let Some(inner_parser) = &self.tmux_inner_parser {
                return inner_parser.get_graphics_carry_bytes();
            }
        }
        if matches!(self.graphics_state, GraphicsState::Ground) {
            if self.has_open_transfer() && self.is_transfer_carryable {
                Some(&self.graphics_transfer_carry_bytes)
            } else if self.has_open_transfer() {
                Some(&[])
            } else {
                None
            }
        } else if self.is_carryable {
            Some(&self.pending_graphics_bytes)
        } else {
            Some(&[])
        }
    }

    pub(crate) fn get_graphics_transport_state(&self) -> Option<GraphicsTransportState> {
        self.has_pending_graphics_state()
            .then(|| self.build_graphics_transport_snapshot())
    }

    fn build_graphics_transport_snapshot(&self) -> GraphicsTransportState {
        let (carry_bytes, is_carryable) = if matches!(self.graphics_state, GraphicsState::Ground) {
            if self.has_own_graphics_transfer() {
                (
                    self.graphics_transfer_carry_bytes.clone(),
                    self.is_transfer_carryable,
                )
            } else if !self.pending_utf8_bytes.is_empty() {
                (self.pending_utf8_bytes.clone(), true)
            } else {
                (Vec::new(), true)
            }
        } else {
            (self.pending_graphics_bytes.clone(), self.is_carryable)
        };
        GraphicsTransportState {
            carry_bytes,
            is_carryable,
            graphics_abandonment: self.get_graphics_abandonment(),
            is_screen_continuation: self.is_screen_continuation,
            is_screen_wrapper_active: self.is_screen_wrapper_active(),
            screen_inner_transport: self
                .screen_inner_parser
                .as_ref()
                .map(|inner_parser| Box::new(inner_parser.build_graphics_transport_snapshot())),
            is_tmux_continuation: self.is_tmux_continuation,
            is_tmux_wrapper_active: self.is_tmux_wrapper_active(),
            tmux_inner_transport: self
                .tmux_inner_parser
                .as_ref()
                .map(|inner_parser| Box::new(inner_parser.build_graphics_transport_snapshot())),
        }
    }

    /// Return whether a multipart transfer has bytes that still need a final
    /// protocol record.
    pub(crate) fn has_open_transfer(&self) -> bool {
        self.kitty_transfer.is_some()
            || self.kitty_animation_transfer.is_some()
            || self.iterm_transfer.is_some()
            || self
                .screen_inner_parser
                .as_ref()
                .is_some_and(|inner_parser| inner_parser.has_open_transfer())
            || self
                .tmux_inner_parser
                .as_ref()
                .is_some_and(|inner_parser| inner_parser.has_open_transfer())
    }

    fn has_own_graphics_transfer(&self) -> bool {
        self.kitty_transfer.is_some()
            || self.kitty_animation_transfer.is_some()
            || self.iterm_transfer.is_some()
    }

    fn get_graphics_protocol(&self) -> GraphicsProtocol {
        match &self.graphics_state {
            GraphicsState::Kitty(_) => GraphicsProtocol::Kitty,
            GraphicsState::Iterm2(_) => GraphicsProtocol::Iterm2,
            GraphicsState::Discard(parser) => {
                parser.discarded_string_kind.resolve_graphics_protocol()
            }
            _ => GraphicsProtocol::Sixel,
        }
    }

    fn get_graphics_abandonment(&self) -> Option<GraphicsAbandonment> {
        if let Some(protocol) = self.abandoned_transfer_protocol {
            return Some(GraphicsAbandonment::Transfer(protocol));
        }
        let is_carryable = if matches!(self.graphics_state, GraphicsState::Ground)
            && self.has_own_graphics_transfer()
        {
            self.is_transfer_carryable
        } else {
            self.is_carryable
        };
        if is_carryable {
            return None;
        }
        if self.has_own_graphics_transfer() {
            let protocol =
                if self.kitty_transfer.is_some() || self.kitty_animation_transfer.is_some() {
                    GraphicsProtocol::Kitty
                } else {
                    GraphicsProtocol::Iterm2
                };
            Some(GraphicsAbandonment::Transfer(protocol))
        } else {
            let protocol = self.get_graphics_protocol();
            if self.is_active_graphics_sequence() {
                Some(GraphicsAbandonment::Sequence(protocol))
            } else {
                Some(GraphicsAbandonment::SilentSequence(protocol))
            }
        }
    }

    fn has_pending_graphics_state(&self) -> bool {
        !matches!(self.graphics_state, GraphicsState::Ground)
            || self.abandoned_transfer_protocol.is_some()
            || self.is_screen_continuation
            || self.screen_inner_parser.is_some()
            || self.is_tmux_continuation
            || self.tmux_inner_parser.is_some()
            || self.has_open_transfer()
            || !self.pending_utf8_bytes.is_empty()
    }

    /// Return whether the next DCS is a GNU Screen continuation wrapper.
    pub(crate) fn is_screen_continuation(&self) -> bool {
        self.is_screen_continuation
    }

    pub(crate) fn is_screen_wrapper_active(&self) -> bool {
        self.is_screen_continuation
            && matches!(
                self.graphics_state,
                GraphicsState::DcsIntro | GraphicsState::Screen(_)
            )
    }

    pub(crate) fn is_tmux_continuation(&self) -> bool {
        self.is_tmux_continuation
    }

    pub(crate) fn is_tmux_wrapper_active(&self) -> bool {
        self.is_tmux_continuation
            && matches!(
                self.graphics_state,
                GraphicsState::DcsIntro | GraphicsState::Tmux(_)
            )
    }

    pub(crate) fn restore_graphics_carry_state(
        &mut self,
        graphics_carry_bytes: &[u8],
        graphics_transport_state: GraphicsTransportState,
    ) {
        self.restore_graphics_transport_state(graphics_transport_state, Some(graphics_carry_bytes));
    }

    fn restore_graphics_transport_state(
        &mut self,
        graphics_transport_state: GraphicsTransportState,
        top_level_graphics_bytes: Option<&[u8]>,
    ) {
        self.is_screen_continuation = graphics_transport_state.is_screen_continuation;
        self.is_tmux_continuation = graphics_transport_state.is_tmux_continuation;
        self.screen_inner_parser =
            graphics_transport_state
                .screen_inner_transport
                .map(|inner_parser| {
                    Box::new(GraphicsParser::from_graphics_transport_state(
                        *inner_parser,
                        self.wrapper_depth.saturating_add(1),
                    ))
                });
        self.tmux_inner_parser =
            graphics_transport_state
                .tmux_inner_transport
                .map(|inner_parser| {
                    Box::new(GraphicsParser::from_graphics_transport_state(
                        *inner_parser,
                        self.wrapper_depth.saturating_add(1),
                    ))
                });

        self.abandoned_transfer_protocol = match graphics_transport_state.graphics_abandonment {
            Some(GraphicsAbandonment::Transfer(protocol)) => Some(protocol),
            Some(GraphicsAbandonment::Sequence(_) | GraphicsAbandonment::SilentSequence(_))
            | None => None,
        };

        if let Some(GraphicsAbandonment::Sequence(protocol)) =
            graphics_transport_state.graphics_abandonment
        {
            self.graphics_state = GraphicsState::Discard(DiscardParser {
                discarded_string_kind: resolve_string_kind(protocol),
                graphics_error: GraphicsError::TransferTooLarge { protocol },
                is_escaped: false,
                should_report: true,
            });
            self.pending_graphics_bytes.clear();
            self.is_carryable = false;
            self.graphics_sequence_byte_count = 0;
            return;
        }
        if let Some(GraphicsAbandonment::SilentSequence(protocol)) =
            graphics_transport_state.graphics_abandonment
        {
            self.graphics_state = GraphicsState::Discard(DiscardParser {
                discarded_string_kind: resolve_string_kind(protocol),
                graphics_error: GraphicsError::TransferTooLarge { protocol },
                is_escaped: false,
                should_report: false,
            });
            self.pending_graphics_bytes.clear();
            self.is_carryable = false;
            self.graphics_sequence_byte_count = 0;
            return;
        }

        let provided_graphics_bytes = top_level_graphics_bytes
            .filter(|graphics_bytes| !graphics_bytes.is_empty())
            .unwrap_or(&graphics_transport_state.carry_bytes);
        if self.is_screen_continuation
            && !graphics_transport_state.is_screen_wrapper_active
            && self.screen_inner_parser.is_none()
            && !provided_graphics_bytes.is_empty()
        {
            let mut inner_parser = GraphicsParser {
                wrapper_depth: self.wrapper_depth.saturating_add(1),
                ..GraphicsParser::default()
            };
            let _ = inner_parser.process_graphics_operations(provided_graphics_bytes);
            self.screen_inner_parser = Some(Box::new(inner_parser));
            return;
        }
        if self.is_tmux_continuation
            && !graphics_transport_state.is_tmux_wrapper_active
            && self.tmux_inner_parser.is_none()
            && !provided_graphics_bytes.is_empty()
        {
            let mut inner_parser = GraphicsParser {
                wrapper_depth: self.wrapper_depth.saturating_add(1),
                ..GraphicsParser::default()
            };
            let _ = inner_parser.process_graphics_operations(provided_graphics_bytes);
            self.tmux_inner_parser = Some(Box::new(inner_parser));
            return;
        }
        let restored_graphics_bytes =
            if self.screen_inner_parser.is_some() || self.tmux_inner_parser.is_some() {
                graphics_transport_state.carry_bytes.as_slice()
            } else {
                provided_graphics_bytes
            };
        if graphics_transport_state.is_carryable {
            let _ = self.process_graphics_operations(restored_graphics_bytes);
        }
    }

    fn from_graphics_transport_state(
        graphics_transport_state: GraphicsTransportState,
        wrapper_depth: usize,
    ) -> Self {
        let mut graphics_parser = GraphicsParser {
            wrapper_depth,
            ..GraphicsParser::default()
        };
        graphics_parser.restore_graphics_transport_state(graphics_transport_state, None);
        graphics_parser
    }

    /// Finish a stream and report any active sequence or multipart transfer.
    pub(crate) fn finish_graphics_stream(
        &mut self,
    ) -> Vec<Result<GraphicsOperation, GraphicsError>> {
        let mut completed_graphics_events = Vec::new();
        let active_graphics_error = match &self.graphics_state {
            GraphicsState::Discard(parser) => Some(parser.graphics_error.clone()),
            _ => None,
        };
        let active_graphics_protocol = if self.is_active_graphics_sequence() {
            Some(self.get_graphics_protocol())
        } else {
            None
        };
        if let Some(protocol) = active_graphics_protocol {
            let should_report_error = !matches!(
                &self.graphics_state,
                GraphicsState::Discard(parser) if !parser.should_report
            );
            self.reset_graphics_parser();
            if should_report_error && self.abandoned_transfer_protocol != Some(protocol) {
                completed_graphics_events.push(Err(
                    active_graphics_error.unwrap_or(GraphicsError::Truncated { protocol })
                ));
            }
        } else if !matches!(self.graphics_state, GraphicsState::Ground) {
            self.reset_graphics_parser();
        }
        if self.kitty_transfer.take().is_some()
            && active_graphics_protocol != Some(GraphicsProtocol::Kitty)
        {
            completed_graphics_events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Kitty,
            }));
        }
        if self.kitty_animation_transfer.take().is_some()
            && active_graphics_protocol != Some(GraphicsProtocol::Kitty)
        {
            completed_graphics_events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Kitty,
            }));
        }
        if self.iterm_transfer.take().is_some()
            && active_graphics_protocol != Some(GraphicsProtocol::Iterm2)
        {
            completed_graphics_events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Iterm2,
            }));
        }
        if let Some(protocol) = self.abandoned_transfer_protocol.take() {
            completed_graphics_events.push(Err(GraphicsError::TransferTooLarge { protocol }));
        }
        if let Some(mut inner_parser) = self.screen_inner_parser.take() {
            completed_graphics_events.extend(inner_parser.finish_graphics_stream());
        }
        if let Some(mut inner_parser) = self.tmux_inner_parser.take() {
            completed_graphics_events.extend(inner_parser.finish_graphics_stream());
        }
        self.graphics_transfer_carry_bytes.clear();
        self.is_transfer_carryable = true;
        self.is_screen_continuation = false;
        self.is_tmux_continuation = false;
        self.screen_inner_parser = None;
        self.tmux_inner_parser = None;
        self.remaining_utf8_continuation_count = 0;
        self.pending_utf8_bytes.clear();
        completed_graphics_events
    }

    fn is_active_graphics_sequence(&self) -> bool {
        match &self.graphics_state {
            GraphicsState::Ground | GraphicsState::Escape | GraphicsState::DcsIntro => false,
            GraphicsState::Kitty(parser) => {
                !parser.is_ignored()
                    && parser.get_control_header_bytes().first().copied() == Some(b'G')
            }
            GraphicsState::Iterm2(parser) => {
                !parser.is_ignored
                    && (parser.is_prefix_complete || parser.prefix_bytes.as_slice() == b"1337")
                    && is_iterm_graphics_command(&parser.command_bytes)
            }
            GraphicsState::Sixel(parser) => parser.phase == ProtocolSixelPhase::Body,
            GraphicsState::Tmux(parser) => {
                parser.prefix_bytes.len() >= b"tmux;".len()
                    && self.has_graphics_in_wrapper(
                        self.tmux_inner_parser.as_deref(),
                        &parser.payload_bytes,
                    )
            }
            GraphicsState::Screen(parser) => self.has_graphics_in_wrapper(
                self.screen_inner_parser.as_deref(),
                &parser.payload_bytes,
            ),
            GraphicsState::Discard(parser) => parser.should_report,
        }
    }

    fn has_graphics_in_wrapper(
        &self,
        inner_parser: Option<&GraphicsParser>,
        graphics_bytes: &[u8],
    ) -> bool {
        let mut wrapper_parser = inner_parser.cloned().unwrap_or_default();
        let graphics_operations = wrapper_parser.process_graphics_operations(graphics_bytes);
        !graphics_operations.is_empty() || wrapper_parser.has_graphics_state()
    }

    fn has_graphics_state(&self) -> bool {
        self.is_active_graphics_sequence()
            || self.has_open_transfer()
            || self
                .screen_inner_parser
                .as_ref()
                .is_some_and(|inner_parser| inner_parser.has_graphics_state())
            || self
                .tmux_inner_parser
                .as_ref()
                .is_some_and(|inner_parser| inner_parser.has_graphics_state())
    }

    fn feed_graphics_byte(
        &mut self,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if !matches!(self.graphics_state, GraphicsState::Ground) {
            self.push_pending_graphics_byte(graphics_byte);
            if self.graphics_sequence_byte_count == MAX_GRAPHICS_TRANSFER_BYTE_COUNT
                && !matches!(self.graphics_state, GraphicsState::Discard(_))
            {
                let transfer_string_kind = match &self.graphics_state {
                    GraphicsState::Kitty(_) => StringKind::Apc,
                    GraphicsState::Iterm2(_) => StringKind::Osc,
                    _ => StringKind::Dcs,
                };
                self.discard_graphics_string(
                    transfer_string_kind,
                    GraphicsError::TransferTooLarge {
                        protocol: transfer_string_kind.resolve_graphics_protocol(),
                    },
                    graphics_byte,
                );
                return;
            }
            self.graphics_sequence_byte_count = self.graphics_sequence_byte_count.saturating_add(1);
        }

        match std::mem::take(&mut self.graphics_state) {
            GraphicsState::Ground => {
                if graphics_byte == 0x18 || graphics_byte == 0x1a {
                    self.cancel_graphics_transfers();
                } else if graphics_byte == 0x1b {
                    self.begin_graphics_state(GraphicsState::Escape, graphics_byte);
                } else if graphics_byte == 0x90 {
                    self.begin_graphics_state(GraphicsState::DcsIntro, graphics_byte);
                } else if matches!(graphics_byte, 0x98 | 0x9e) {
                    self.begin_silent_string(graphics_byte);
                } else if graphics_byte == 0x9f {
                    self.begin_graphics_transfer(
                        GraphicsState::Kitty(KittyParser::new()),
                        graphics_byte,
                    );
                } else if graphics_byte == 0x9d {
                    self.begin_graphics_transfer(
                        GraphicsState::Iterm2(ItermParser::new()),
                        graphics_byte,
                    );
                }
            }
            GraphicsState::Escape => self.feed_escape(graphics_byte),
            GraphicsState::DcsIntro => self.feed_dcs_intro(graphics_byte, graphics_events),
            GraphicsState::Sixel(parser) => self.feed_sixel(parser, graphics_byte, graphics_events),
            GraphicsState::Kitty(parser) => self.feed_kitty(parser, graphics_byte, graphics_events),
            GraphicsState::Iterm2(parser) => {
                self.feed_iterm(parser, graphics_byte, graphics_events)
            }
            GraphicsState::Tmux(parser) => self.feed_tmux(parser, graphics_byte, graphics_events),
            GraphicsState::Screen(parser) => {
                self.feed_screen(parser, graphics_byte, graphics_events)
            }
            GraphicsState::Discard(parser) => {
                self.feed_discard(parser, graphics_byte, graphics_events)
            }
        }
    }

    fn feed_escape(&mut self, graphics_byte: u8) {
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
            return;
        }
        match graphics_byte {
            b'P' => self.graphics_state = GraphicsState::DcsIntro,
            b'_' => self.continue_graphics_transfer(GraphicsState::Kitty(KittyParser::new())),
            b']' => self.continue_graphics_transfer(GraphicsState::Iterm2(ItermParser::new())),
            b'X' | b'^' => self.ignore_graphics_string(StringKind::Dcs, graphics_byte),
            0x1b => {
                self.pending_graphics_bytes.clear();
                self.is_carryable = true;
                self.pending_graphics_bytes.push(graphics_byte);
                self.graphics_state = GraphicsState::Escape;
            }
            _ => self.reset_graphics_parser(),
        }
    }

    fn feed_dcs_intro(
        &mut self,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
            return;
        }
        if self.is_screen_continuation {
            if graphics_byte == 0x9c {
                self.finish_screen(Vec::new(), graphics_events);
            } else {
                self.graphics_state =
                    GraphicsState::Screen(ScreenParser::from_first_byte(graphics_byte));
            }
            return;
        }
        if self.is_tmux_continuation {
            if graphics_byte == 0x9c {
                self.finish_tmux(Vec::new(), graphics_events);
            } else {
                self.graphics_state =
                    GraphicsState::Tmux(TmuxParser::from_first_byte(graphics_byte));
            }
            return;
        }
        if graphics_byte == 0x9c {
            self.reset_graphics_parser();
            return;
        }
        if self.wrapper_depth >= MAX_GRAPHICS_WRAPPER_DEPTH && matches!(graphics_byte, b't' | 0x1b)
        {
            self.discard_graphics_string(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                graphics_byte,
            );
            return;
        }
        match graphics_byte {
            b'q' => {
                let mut parser = ProtocolSixelParser::new();
                if parser.feed_input_byte(b'q').is_err() {
                    self.reset_graphics_parser();
                } else {
                    self.graphics_state = GraphicsState::Sixel(Box::new(parser));
                }
            }
            b't' => self.graphics_state = GraphicsState::Tmux(TmuxParser::new()),
            0x1b => self.graphics_state = GraphicsState::Screen(ScreenParser::new()),
            b'0'..=b'9' | b';' => {
                let mut parser = ProtocolSixelParser::new();
                if parser.feed_input_byte(graphics_byte).is_err() {
                    self.reset_graphics_parser();
                } else {
                    self.graphics_state = GraphicsState::Sixel(Box::new(parser));
                }
            }
            _ => self.ignore_graphics_string(StringKind::Dcs, graphics_byte),
        }
    }

    fn feed_sixel(
        &mut self,
        mut parser: Box<ProtocolSixelParser>,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.is_escaped {
            if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else if graphics_byte == b'\\' {
                parser.is_escaped = false;
                self.finish_sixel((*parser).finish_payload(), graphics_events);
            } else {
                self.discard_graphics_string(
                    StringKind::Dcs,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    graphics_byte,
                );
            }
            return;
        }
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
        } else if graphics_byte == 0x1b {
            parser.is_escaped = true;
            self.graphics_state = GraphicsState::Sixel(parser);
        } else if graphics_byte == 0x9c {
            self.finish_sixel(parser.finish_payload(), graphics_events);
        } else if let Err(graphics_error) = parser.feed_input_byte(graphics_byte) {
            if parser.phase == ProtocolSixelPhase::Header
                && graphics_byte != b'q'
                && !matches!(graphics_error, GraphicsError::TransferTooLarge { .. })
            {
                self.ignore_graphics_string(StringKind::Dcs, graphics_byte);
            } else {
                self.discard_graphics_string(StringKind::Dcs, graphics_error, graphics_byte);
            }
        } else {
            self.graphics_state = GraphicsState::Sixel(parser);
        }
    }

    fn finish_sixel(
        &mut self,
        sixel_graphic_result: Result<SixelGraphic, GraphicsError>,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let is_sixel_failure = sixel_graphic_result.is_err();
        self.reset_graphics_parser();
        match sixel_graphic_result {
            Ok(sixel_graphic) => graphics_events.push(Ok(GraphicsOperation::Sixel(sixel_graphic))),
            Err(graphics_error) => graphics_events.push(Err(graphics_error)),
        }
        if is_sixel_failure {
            self.is_screen_continuation = false;
            self.screen_inner_parser = None;
            self.is_tmux_continuation = false;
            self.tmux_inner_parser = None;
        }
    }

    fn feed_kitty(
        &mut self,
        mut parser: KittyParser,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.is_ignored() {
            if parser.is_escaped() {
                parser.set_escaped(false);
                if graphics_byte == b'\\' {
                    self.reset_graphics_parser();
                } else if graphics_byte == 0x18 || graphics_byte == 0x1a {
                    self.cancel_graphics_transfers();
                    self.reset_graphics_parser();
                } else {
                    parser.set_escaped(graphics_byte == 0x1b);
                    self.graphics_state = GraphicsState::Kitty(parser);
                }
            } else if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else if graphics_byte == 0x1b {
                parser.set_escaped(true);
                self.graphics_state = GraphicsState::Kitty(parser);
            } else if graphics_byte == 0x9c {
                self.reset_graphics_parser();
            } else {
                self.graphics_state = GraphicsState::Kitty(parser);
            }
            return;
        }
        if parser.is_escaped() {
            if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else if graphics_byte == b'\\' {
                parser.set_escaped(false);
                self.finish_kitty(parser, graphics_events);
            } else {
                self.discard_graphics_string(
                    StringKind::Apc,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    },
                    graphics_byte,
                );
            }
            return;
        }
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
        } else if graphics_byte == 0x1b {
            parser.set_escaped(true);
            self.graphics_state = GraphicsState::Kitty(parser);
        } else if graphics_byte == 0x9c {
            self.finish_kitty(parser, graphics_events);
        } else if let Err(graphics_error) = parser.feed_input_byte(graphics_byte) {
            self.discard_graphics_string(StringKind::Apc, graphics_error, graphics_byte);
        } else if parser.is_ignored() {
            self.ignore_graphics_string(StringKind::Apc, graphics_byte);
        } else {
            self.graphics_state = GraphicsState::Kitty(parser);
        }
    }

    fn feed_iterm(
        &mut self,
        mut parser: ItermParser,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.is_escaped {
            if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else if graphics_byte == b'\\' {
                parser.is_escaped = false;
                self.finish_iterm(parser, graphics_events);
            } else {
                self.discard_graphics_string(
                    StringKind::Osc,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Iterm2,
                    },
                    graphics_byte,
                );
            }
            return;
        }
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
        } else if graphics_byte == 0x1b {
            parser.is_escaped = true;
            self.graphics_state = GraphicsState::Iterm2(parser);
        } else if graphics_byte == 0x07 || graphics_byte == 0x9c {
            self.finish_iterm(parser, graphics_events);
        } else if let Err(graphics_error) = parser.feed_iterm_byte(graphics_byte) {
            self.discard_graphics_string(StringKind::Osc, graphics_error, graphics_byte);
        } else if parser.is_ignored || !can_iterm_command_be_graphics(&parser.command_bytes) {
            self.ignore_graphics_string(StringKind::Osc, graphics_byte);
        } else {
            self.graphics_state = GraphicsState::Iterm2(parser);
        }
    }

    fn feed_tmux(
        &mut self,
        mut parser: TmuxParser,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.prefix_bytes.len() < b"tmux;".len() {
            if graphics_byte == 0x9c {
                self.reset_graphics_parser();
                return;
            }
            let expected_prefix_byte = b"tmux;"[parser.prefix_bytes.len()];
            if graphics_byte == expected_prefix_byte {
                parser.prefix_bytes.push(graphics_byte);
                self.graphics_state = GraphicsState::Tmux(parser);
            } else {
                self.ignore_graphics_string(StringKind::Dcs, graphics_byte);
            }
            return;
        }
        if graphics_byte == 0x9c {
            if !parser.is_inner_terminated
                && !self.body_has_complete_graphics(
                    self.tmux_inner_parser.as_deref(),
                    &parser.payload_bytes,
                )
                && self.body_has_c1_terminated_graphics(
                    self.tmux_inner_parser.as_deref(),
                    &parser.payload_bytes,
                )
            {
                if parser.payload_bytes.len() == MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
                    self.finish_graphics_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        graphics_events,
                    );
                } else {
                    parser.payload_bytes.push(graphics_byte);
                    parser.is_inner_terminated = true;
                    self.graphics_state = GraphicsState::Tmux(parser);
                }
            } else {
                self.finish_tmux(parser.payload_bytes, graphics_events);
            }
            return;
        }
        if parser.is_escaped {
            parser.is_escaped = false;
            if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else if graphics_byte == 0x1b {
                if parser.payload_bytes.len() == MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
                    self.discard_graphics_string(
                        StringKind::Dcs,
                        GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        },
                        graphics_byte,
                    );
                } else {
                    parser.payload_bytes.push(0x1b);
                    self.graphics_state = GraphicsState::Tmux(parser);
                }
            } else if graphics_byte == b'\\' {
                self.finish_tmux(parser.payload_bytes, graphics_events);
            } else {
                self.discard_graphics_string(
                    StringKind::Dcs,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    graphics_byte,
                );
            }
            return;
        }
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
        } else if graphics_byte == 0x1b {
            parser.is_escaped = true;
            self.graphics_state = GraphicsState::Tmux(parser);
        } else if parser.payload_bytes.len() == MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
            self.discard_graphics_string(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                graphics_byte,
            );
        } else {
            parser.payload_bytes.push(graphics_byte);
            self.graphics_state = GraphicsState::Tmux(parser);
        }
    }

    fn feed_screen(
        &mut self,
        mut parser: ScreenParser,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if graphics_byte == 0x9c {
            let is_inner_complete = if parser.is_inner_terminated {
                self.body_has_c1_terminated_graphics_after_boundary(
                    self.screen_inner_parser.as_deref(),
                    &parser,
                )
            } else {
                !self.body_has_complete_graphics(
                    self.screen_inner_parser.as_deref(),
                    &parser.payload_bytes,
                ) && self.body_has_c1_terminated_graphics(
                    self.screen_inner_parser.as_deref(),
                    &parser.payload_bytes,
                )
            };
            if is_inner_complete {
                if parser.payload_bytes.len() == MAX_SCREEN_PASSTHROUGH_BYTE_COUNT {
                    self.finish_graphics_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        graphics_events,
                    );
                    self.is_screen_continuation = false;
                } else {
                    parser.payload_bytes.push(graphics_byte);
                    parser.is_inner_terminated = true;
                    parser.inner_data_start_index = parser.payload_bytes.len();
                    self.graphics_state = GraphicsState::Screen(parser);
                }
            } else {
                self.finish_screen(parser.payload_bytes, graphics_events);
            }
            return;
        }
        if !parser.is_escaped && parser.payload_bytes.as_slice() == [0x1b] && graphics_byte == b'\\'
        {
            self.finish_screen(Vec::new(), graphics_events);
            return;
        }
        if parser.is_escaped {
            parser.is_escaped = false;
            if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else if graphics_byte == b'\\' {
                let is_inner_complete = if parser.is_inner_terminated {
                    self.body_has_st_terminated_graphics_after_boundary(
                        self.screen_inner_parser.as_deref(),
                        &parser,
                    )
                } else {
                    !self.body_has_complete_graphics(
                        self.screen_inner_parser.as_deref(),
                        &parser.payload_bytes,
                    ) && self.body_has_st_terminated_graphics(
                        self.screen_inner_parser.as_deref(),
                        &parser.payload_bytes,
                    )
                };
                if is_inner_complete {
                    if parser.payload_bytes.len()
                        > MAX_SCREEN_PASSTHROUGH_BYTE_COUNT.saturating_sub(2)
                    {
                        self.finish_graphics_state(
                            Err(GraphicsError::TransferTooLarge {
                                protocol: GraphicsProtocol::Sixel,
                            }),
                            graphics_events,
                        );
                        self.is_screen_continuation = false;
                    } else {
                        parser.payload_bytes.push(0x1b);
                        parser.payload_bytes.push(b'\\');
                        parser.is_inner_terminated = true;
                        parser.inner_data_start_index = parser.payload_bytes.len();
                        self.graphics_state = GraphicsState::Screen(parser);
                    }
                } else if parser.payload_bytes.len() > MAX_SCREEN_PASSTHROUGH_BYTE_COUNT {
                    self.finish_graphics_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        graphics_events,
                    );
                    self.is_screen_continuation = false;
                } else {
                    self.finish_screen(parser.payload_bytes, graphics_events);
                }
            } else if parser.payload_bytes.len()
                > MAX_SCREEN_PASSTHROUGH_BYTE_COUNT.saturating_sub(2)
            {
                self.discard_graphics_string(
                    StringKind::Dcs,
                    GraphicsError::TransferTooLarge {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    graphics_byte,
                );
            } else {
                parser.payload_bytes.push(0x1b);
                parser.payload_bytes.push(graphics_byte);
                self.graphics_state = GraphicsState::Screen(parser);
            }
            return;
        }
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
        } else if graphics_byte == 0x1b {
            parser.is_escaped = true;
            self.graphics_state = GraphicsState::Screen(parser);
        } else if parser.payload_bytes.len() == MAX_SCREEN_PASSTHROUGH_BYTE_COUNT {
            self.discard_graphics_string(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                graphics_byte,
            );
        } else {
            parser.payload_bytes.push(graphics_byte);
            self.graphics_state = GraphicsState::Screen(parser);
        }
    }

    fn finish_screen(
        &mut self,
        screen_payload_bytes: Vec<u8>,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let mut inner_parser = self
            .screen_inner_parser
            .take()
            .map(|inner_parser| *inner_parser)
            .unwrap_or_default();
        inner_parser.wrapper_depth = self.wrapper_depth.saturating_add(1);
        inner_parser.is_screen_continuation = false;
        graphics_events.extend(inner_parser.process_graphics_operations(&screen_payload_bytes));
        let has_continuation = inner_parser.has_pending_graphics_state();
        self.reset_graphics_parser();
        self.is_screen_continuation = has_continuation;
        if has_continuation {
            self.screen_inner_parser = Some(Box::new(inner_parser));
        }
    }

    fn finish_tmux(
        &mut self,
        tmux_payload_bytes: Vec<u8>,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let mut inner_parser = self
            .tmux_inner_parser
            .take()
            .map(|inner_parser| *inner_parser)
            .unwrap_or_default();
        inner_parser.wrapper_depth = self.wrapper_depth.saturating_add(1);
        inner_parser.is_tmux_continuation = false;
        graphics_events.extend(inner_parser.process_graphics_operations(&tmux_payload_bytes));
        let has_continuation = inner_parser.has_pending_graphics_state();
        self.reset_graphics_parser();
        self.is_tmux_continuation = has_continuation;
        if has_continuation {
            self.tmux_inner_parser = Some(Box::new(inner_parser));
        }
    }

    fn body_has_st_terminated_graphics(
        &self,
        inner_parser: Option<&GraphicsParser>,
        graphics_body_bytes: &[u8],
    ) -> bool {
        let mut candidate_graphics_bytes = graphics_body_bytes.to_vec();
        candidate_graphics_bytes.extend_from_slice(b"\x1b\\");
        matches!(
            self.decode_wrapper(inner_parser, &candidate_graphics_bytes),
            Ok(Some(_))
        )
    }

    fn body_has_c1_terminated_graphics(
        &self,
        inner_parser: Option<&GraphicsParser>,
        graphics_body_bytes: &[u8],
    ) -> bool {
        let mut candidate_graphics_bytes = graphics_body_bytes.to_vec();
        candidate_graphics_bytes.push(0x9c);
        matches!(
            self.decode_wrapper(inner_parser, &candidate_graphics_bytes),
            Ok(Some(_))
        )
    }

    fn body_has_st_terminated_graphics_after_boundary(
        &self,
        inner_parser: Option<&GraphicsParser>,
        screen_parser: &ScreenParser,
    ) -> bool {
        self.body_has_terminated_graphics_after_boundary(inner_parser, screen_parser, b"\x1b\\")
    }

    fn body_has_c1_terminated_graphics_after_boundary(
        &self,
        inner_parser: Option<&GraphicsParser>,
        screen_parser: &ScreenParser,
    ) -> bool {
        self.body_has_terminated_graphics_after_boundary(inner_parser, screen_parser, &[0x9c])
    }

    fn body_has_terminated_graphics_after_boundary(
        &self,
        inner_parser: Option<&GraphicsParser>,
        screen_parser: &ScreenParser,
        terminator: &[u8],
    ) -> bool {
        let mut replay_parser = inner_parser.cloned().unwrap_or_default();
        let _ = replay_parser.process_graphics_operations(
            &screen_parser.payload_bytes[..screen_parser.inner_data_start_index],
        );
        let mut candidate_graphics_bytes =
            screen_parser.payload_bytes[screen_parser.inner_data_start_index..].to_vec();
        candidate_graphics_bytes.extend_from_slice(terminator);
        matches!(
            self.decode_wrapper(Some(&replay_parser), &candidate_graphics_bytes),
            Ok(Some(_))
        )
    }

    fn body_has_complete_graphics(
        &self,
        inner_parser: Option<&GraphicsParser>,
        graphics_body_bytes: &[u8],
    ) -> bool {
        matches!(
            self.decode_wrapper(inner_parser, graphics_body_bytes),
            Ok(Some(_))
        )
    }

    fn decode_wrapper(
        &self,
        inner_parser: Option<&GraphicsParser>,
        wrapper_bytes: &[u8],
    ) -> Result<Option<GraphicsOperation>, GraphicsError> {
        let mut wrapper_parser = inner_parser.cloned().unwrap_or_default();
        let graphics_operations = wrapper_parser.process_graphics_operations(wrapper_bytes);
        match graphics_operations.as_slice() {
            [Ok(GraphicsOperation::Failure { graphics_error, .. })] => Err(graphics_error.clone()),
            [graphics_event] => graphics_event.clone().map(Some),
            [] => Ok(None),
            _ => Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            }),
        }
    }

    fn feed_discard(
        &mut self,
        mut parser: DiscardParser,
        graphics_byte: u8,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.is_escaped {
            parser.is_escaped = false;
            if graphics_byte == b'\\' {
                if parser.should_report {
                    self.finish_graphics_state(Err(parser.graphics_error), graphics_events);
                } else {
                    self.finish_graphics_state(Ok(None), graphics_events);
                }
            } else if graphics_byte == 0x18 || graphics_byte == 0x1a {
                self.cancel_graphics_transfers();
                self.reset_graphics_parser();
            } else {
                parser.is_escaped = graphics_byte == 0x1b;
                self.graphics_state = GraphicsState::Discard(parser);
            }
            return;
        }
        if graphics_byte == 0x18 || graphics_byte == 0x1a {
            self.cancel_graphics_transfers();
            self.reset_graphics_parser();
        } else if graphics_byte == 0x1b {
            parser.is_escaped = true;
            self.graphics_state = GraphicsState::Discard(parser);
        } else if graphics_byte == 0x9c
            || (parser.discarded_string_kind == StringKind::Osc && graphics_byte == 0x07)
        {
            if parser.should_report {
                self.finish_graphics_state(Err(parser.graphics_error), graphics_events);
            } else {
                self.finish_graphics_state(Ok(None), graphics_events);
            }
        } else {
            self.graphics_state = GraphicsState::Discard(parser);
        }
    }

    fn finish_kitty(
        &mut self,
        kitty_parser: KittyParser,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let mut image_display_reply = parse_reply_display(kitty_parser.get_control_header_bytes());
        let is_continuation_chunk = kitty_parser
            .get_control_header_bytes()
            .strip_prefix(b"G")
            .is_some_and(|control_header_bytes| {
                control_header_bytes
                    .split(|control_header_byte| *control_header_byte == b',')
                    .all(|control_field_bytes| {
                        control_field_bytes.starts_with(b"m=")
                            || control_field_bytes.starts_with(b"q=")
                    })
            });
        if is_continuation_chunk {
            if let Some(transfer) = &self.kitty_transfer {
                let response_suppression_level = image_display_reply.response_suppression_level;
                image_display_reply = transfer.get_image_display().clone();
                if kitty_parser
                    .get_control_header_bytes()
                    .windows(2)
                    .any(|control_pair_bytes| control_pair_bytes == b"q=")
                {
                    image_display_reply.response_suppression_level = response_suppression_level;
                }
            } else if let Some(transfer) = &self.kitty_animation_transfer {
                let response_suppression_level = image_display_reply.response_suppression_level;
                image_display_reply = transfer.get_image_display();
                if kitty_parser
                    .get_control_header_bytes()
                    .windows(2)
                    .any(|control_pair_bytes| control_pair_bytes == b"q=")
                {
                    image_display_reply.response_suppression_level = response_suppression_level;
                }
            }
        }
        let first_graphics_event_index = graphics_events.len();
        let kitty_command_result = (!kitty_parser.is_ignored())
            .then(|| {
                parse_kitty_command(
                    kitty_parser.get_control_header_bytes(),
                    kitty_parser.get_payload_bytes(),
                )
            })
            .flatten();
        if kitty_parser.is_ignored() {
            self.reset_graphics_parser();
        } else if kitty_command_result
            .as_ref()
            .is_some_and(|kitty_command_result| {
                kitty_command_result.as_ref().is_ok_and(|kitty_command| {
                    matches!(
                        kitty_command.get_command_kind(),
                        KittyCommandKind::Delete(_) | KittyCommandKind::AnimationDelete
                    )
                })
            })
        {
            self.kitty_transfer = None;
            self.kitty_animation_transfer = None;
            if self.abandoned_transfer_protocol == Some(GraphicsProtocol::Kitty) {
                self.abandoned_transfer_protocol = None;
            }
            self.graphics_transfer_carry_bytes.clear();
            self.is_transfer_carryable = true;
            self.reset_graphics_parser();
            graphics_events.push(
                kitty_command_result
                    .expect("the parsed command is present")
                    .map(GraphicsOperation::Command),
            );
        } else if self.kitty_animation_transfer.is_some()
            || has_kitty_animation_transfer_header(kitty_parser.get_control_header_bytes())
        {
            let animation_command_result = kitty_parser
                .finish_kitty_animation_chunk()
                .and_then(|animation_chunk| {
                    animation_chunk.ok_or(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    })
                })
                .and_then(|animation_chunk| self.accept_kitty_animation_chunk(animation_chunk))
                .map(|kitty_command_result| kitty_command_result.map(GraphicsOperation::Command));
            let is_animation_command_failed = animation_command_result.is_err();
            self.reset_graphics_parser();
            match animation_command_result {
                Ok(Some(kitty_command)) => graphics_events.push(Ok(kitty_command)),
                Ok(None) => {}
                Err(graphics_error) => graphics_events.push(Err(graphics_error)),
            }
            if is_animation_command_failed || !self.has_own_graphics_transfer() {
                self.kitty_transfer = None;
                self.kitty_animation_transfer = None;
                self.iterm_transfer = None;
                self.graphics_transfer_carry_bytes.clear();
                self.is_transfer_carryable = true;
            }
        } else if let Some(kitty_command_result) = kitty_command_result {
            if self.kitty_transfer.is_some() || self.abandoned_transfer_protocol.is_some() {
                self.finish_graphics_state(
                    Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    }),
                    graphics_events,
                );
                commands::attach_error_replies(
                    &mut graphics_events[first_graphics_event_index..],
                    &image_display_reply,
                );
                return;
            }
            self.reset_graphics_parser();
            graphics_events.push(kitty_command_result.map(GraphicsOperation::Command));
        } else if self.abandoned_transfer_protocol == Some(GraphicsProtocol::Kitty) {
            match kitty_parser.finish_kitty_chunk() {
                Ok(kitty_chunk) if kitty_chunk.has_more_chunks() => self.reset_graphics_parser(),
                Ok(_) => {
                    self.abandoned_transfer_protocol = None;
                    self.finish_graphics_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Kitty,
                        }),
                        graphics_events,
                    );
                }
                Err(_) => self.reset_graphics_parser(),
            }
        } else {
            let decoded_graphics_result =
                self.accept_kitty_chunk(kitty_parser.finish_kitty_chunk());
            self.finish_graphics_state(decoded_graphics_result, graphics_events);
        }
        commands::attach_error_replies(
            &mut graphics_events[first_graphics_event_index..],
            &image_display_reply,
        );
    }

    fn finish_iterm(
        &mut self,
        iterm_parser: ItermParser,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if iterm_parser.is_ignored {
            self.reset_graphics_parser();
        } else {
            match self.abandoned_transfer_protocol {
                Some(GraphicsProtocol::Iterm2) => {
                    let iterm_command_name = iterm_parser
                        .command_bytes
                        .split(|command_byte| *command_byte == b'=')
                        .next()
                        .unwrap_or(&iterm_parser.command_bytes);
                    match iterm_command_name {
                        b"FilePart" => self.reset_graphics_parser(),
                        b"FileEnd" => {
                            self.abandoned_transfer_protocol = None;
                            self.finish_graphics_state(
                                Err(GraphicsError::TransferTooLarge {
                                    protocol: GraphicsProtocol::Iterm2,
                                }),
                                graphics_events,
                            );
                        }
                        _ => self.reset_graphics_parser(),
                    }
                }
                _ => self.finish_iterm_command(iterm_parser, graphics_events),
            }
        }
    }

    fn finish_iterm_command(
        &mut self,
        iterm_parser: ItermParser,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let parsed_iterm_command =
            parse_iterm_command(&iterm_parser.command_bytes, &mut self.iterm_transfer);
        if self.iterm_transfer.is_some() {
            self.remember_graphics_transfer_sequence();
        }
        self.finish_graphics_state(parsed_iterm_command, graphics_events);
    }

    fn accept_kitty_chunk(
        &mut self,
        kitty_chunk_result: Result<KittyChunk, GraphicsError>,
    ) -> Result<Option<DecodedGraphics>, GraphicsError> {
        let kitty_chunk = kitty_chunk_result?;
        let kitty_transfer_outcome = if let Some(transfer) = self.kitty_transfer.take() {
            transfer.accept_continuation_chunk(kitty_chunk)?
        } else {
            start_kitty_transfer(kitty_chunk)?
        };
        match kitty_transfer_outcome {
            KittyTransferOutcome::Pending(transfer) => {
                self.kitty_transfer = Some(transfer);
                self.remember_graphics_transfer_sequence();
                Ok(None)
            }
            KittyTransferOutcome::Complete(decoded_graphics) => Ok(Some(decoded_graphics)),
        }
    }

    fn accept_kitty_animation_chunk(
        &mut self,
        animation_chunk: KittyAnimationChunk,
    ) -> Result<Option<KittyCommand>, GraphicsError> {
        if self.kitty_transfer.is_some() || self.abandoned_transfer_protocol.is_some() {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        let animation_transfer_outcome =
            if let Some(transfer) = self.kitty_animation_transfer.take() {
                transfer.accept_continuation_chunk(animation_chunk)?
            } else {
                start_kitty_animation_transfer(animation_chunk)?
            };
        match animation_transfer_outcome {
            KittyAnimationTransferOutcome::Pending(transfer) => {
                self.kitty_animation_transfer = Some(transfer);
                self.remember_graphics_transfer_sequence();
                Ok(None)
            }
            KittyAnimationTransferOutcome::Complete(kitty_command) => Ok(Some(*kitty_command)),
        }
    }

    fn finish_graphics_state(
        &mut self,
        decoded_graphics_result: Result<Option<DecodedGraphics>, GraphicsError>,
        graphics_events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let is_graphics_failure = decoded_graphics_result.is_err();
        self.reset_graphics_parser();
        match decoded_graphics_result {
            Ok(Some(decoded_graphics)) => {
                graphics_events.push(Ok(GraphicsOperation::Image(decoded_graphics)))
            }
            Ok(None) => {}
            Err(graphics_error) => graphics_events.push(Err(graphics_error)),
        }
        if is_graphics_failure || !self.has_own_graphics_transfer() {
            self.kitty_transfer = None;
            self.kitty_animation_transfer = None;
            self.iterm_transfer = None;
            self.graphics_transfer_carry_bytes.clear();
            self.is_transfer_carryable = true;
        }
        if is_graphics_failure {
            self.is_screen_continuation = false;
            self.screen_inner_parser = None;
            self.is_tmux_continuation = false;
            self.tmux_inner_parser = None;
        }
    }

    fn discard_graphics_string(
        &mut self,
        string_kind: StringKind,
        graphics_error: GraphicsError,
        terminating_byte: u8,
    ) {
        self.discard_graphics_string_with_report(
            string_kind,
            graphics_error,
            terminating_byte,
            true,
        );
    }

    fn discard_graphics_string_with_report(
        &mut self,
        string_kind: StringKind,
        graphics_error: GraphicsError,
        terminating_byte: u8,
        should_report_error: bool,
    ) {
        if should_report_error {
            if string_kind == StringKind::Apc {
                self.kitty_transfer = None;
                self.kitty_animation_transfer = None;
            } else if string_kind == StringKind::Osc {
                self.iterm_transfer = None;
            }
            self.graphics_transfer_carry_bytes.clear();
            self.is_transfer_carryable = true;
        }
        self.graphics_state = GraphicsState::Discard(DiscardParser {
            discarded_string_kind: string_kind,
            graphics_error,
            is_escaped: terminating_byte == 0x1b,
            should_report: should_report_error,
        });
    }

    fn ignore_graphics_string(&mut self, string_kind: StringKind, terminating_byte: u8) {
        self.discard_graphics_string_with_report(
            string_kind,
            GraphicsError::InvalidCommand {
                protocol: string_kind.resolve_graphics_protocol(),
            },
            terminating_byte,
            false,
        );
    }

    fn begin_graphics_state(&mut self, graphics_state: GraphicsState, graphics_byte: u8) {
        self.pending_graphics_bytes.clear();
        self.pending_graphics_bytes.push(graphics_byte);
        self.is_carryable = true;
        self.graphics_sequence_byte_count = 1;
        self.graphics_state = graphics_state;
    }

    fn begin_graphics_transfer(&mut self, graphics_state: GraphicsState, graphics_byte: u8) {
        self.begin_graphics_state(graphics_state, graphics_byte);
        self.attach_transfer_carry_bytes();
    }

    fn begin_silent_string(&mut self, graphics_byte: u8) {
        self.begin_graphics_state(
            GraphicsState::Discard(DiscardParser {
                discarded_string_kind: StringKind::Dcs,
                graphics_error: GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                },
                is_escaped: false,
                should_report: false,
            }),
            graphics_byte,
        );
    }

    fn continue_graphics_transfer(&mut self, graphics_state: GraphicsState) {
        self.attach_transfer_carry_bytes();
        self.graphics_state = graphics_state;
    }

    fn attach_transfer_carry_bytes(&mut self) {
        if !self.has_own_graphics_transfer() {
            return;
        }
        if !self.is_transfer_carryable {
            self.is_carryable = false;
            return;
        }
        if self.graphics_transfer_carry_bytes.is_empty() {
            return;
        }
        let mut carried_transfer_bytes = std::mem::take(&mut self.graphics_transfer_carry_bytes);
        carried_transfer_bytes.extend_from_slice(&self.pending_graphics_bytes);
        if carried_transfer_bytes.len() > MAX_GRAPHICS_CARRY_BYTE_COUNT {
            carried_transfer_bytes.clear();
            self.is_carryable = false;
        } else {
            self.pending_graphics_bytes = carried_transfer_bytes;
        }
    }

    fn push_pending_graphics_byte(&mut self, graphics_byte: u8) {
        if !self.is_carryable {
            return;
        }
        if self.pending_graphics_bytes.len() == MAX_GRAPHICS_CARRY_BYTE_COUNT {
            self.pending_graphics_bytes.clear();
            self.is_carryable = false;
        } else {
            self.pending_graphics_bytes.push(graphics_byte);
        }
    }

    fn push_pending_graphics_bytes(&mut self, pending_graphics_bytes: &[u8]) {
        if !self.is_carryable {
            return;
        }
        let available_byte_count =
            MAX_GRAPHICS_CARRY_BYTE_COUNT.saturating_sub(self.pending_graphics_bytes.len());
        if pending_graphics_bytes.len() > available_byte_count {
            self.pending_graphics_bytes.clear();
            self.is_carryable = false;
        } else {
            self.pending_graphics_bytes
                .extend_from_slice(pending_graphics_bytes);
        }
    }

    fn reset_graphics_parser(&mut self) {
        self.graphics_state = GraphicsState::Ground;
        self.pending_graphics_bytes.clear();
        self.is_carryable = true;
        self.graphics_sequence_byte_count = 0;
        self.remaining_utf8_continuation_count = 0;
        self.pending_utf8_bytes.clear();
    }

    fn cancel_graphics_transfers(&mut self) {
        self.kitty_transfer = None;
        self.kitty_animation_transfer = None;
        self.iterm_transfer = None;
        self.graphics_transfer_carry_bytes.clear();
        self.is_transfer_carryable = true;
        self.abandoned_transfer_protocol = None;
        self.is_screen_continuation = false;
        self.screen_inner_parser = None;
        self.is_tmux_continuation = false;
        self.tmux_inner_parser = None;
    }

    fn remember_graphics_transfer_sequence(&mut self) {
        if !self.is_carryable {
            self.graphics_transfer_carry_bytes.clear();
            self.is_transfer_carryable = false;
            return;
        }
        if self.pending_graphics_bytes.len() > MAX_GRAPHICS_CARRY_BYTE_COUNT {
            self.graphics_transfer_carry_bytes.clear();
            self.is_transfer_carryable = false;
            return;
        }
        self.graphics_transfer_carry_bytes.clear();
        self.graphics_transfer_carry_bytes
            .extend_from_slice(&self.pending_graphics_bytes);
    }
}

fn is_base64_byte(candidate_byte: u8) -> bool {
    matches!(
        candidate_byte,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'='
    )
}

fn has_kitty_animation_transfer_header(control_header_bytes: &[u8]) -> bool {
    let Some(control_body_bytes) = control_header_bytes.strip_prefix(b"G") else {
        return false;
    };
    let action_code = control_body_bytes
        .split(|control_body_byte| *control_body_byte == b',')
        .find_map(|control_field_bytes| control_field_bytes.strip_prefix(b"a="));
    action_code == Some(b"f")
        && control_body_bytes
            .split(|control_body_byte| *control_body_byte == b',')
            .any(|control_field_bytes| control_field_bytes == b"m=1")
}

fn compute_base64_run_byte_count(payload_bytes: &[u8], maximum_payload_byte_count: usize) -> usize {
    let base64_bytes = &payload_bytes[..payload_bytes.len().min(maximum_payload_byte_count)];
    base64_bytes
        .iter()
        .position(|base64_byte| !is_base64_byte(*base64_byte))
        .unwrap_or(base64_bytes.len())
}

fn extend_terminal_inert_range(terminal_inert_ranges: &mut Vec<Range<usize>>, byte_offset: usize) {
    if let Some(range) = terminal_inert_ranges
        .last_mut()
        .filter(|range| range.end == byte_offset)
    {
        range.end += 1;
    } else {
        terminal_inert_ranges.push(byte_offset..byte_offset + 1);
    }
}

#[derive(Clone)]
struct ItermParser {
    prefix_bytes: Vec<u8>,
    command_bytes: Vec<u8>,
    is_prefix_complete: bool,
    is_ignored: bool,
    is_escaped: bool,
}

impl ItermParser {
    fn new() -> Self {
        ItermParser {
            prefix_bytes: Vec::new(),
            command_bytes: Vec::new(),
            is_prefix_complete: false,
            is_ignored: false,
            is_escaped: false,
        }
    }

    fn feed_iterm_byte(&mut self, iterm_input_byte: u8) -> Result<(), GraphicsError> {
        if self.is_ignored {
            return Ok(());
        }
        if !self.is_prefix_complete {
            if iterm_input_byte == b';' {
                if self.prefix_bytes.as_slice() == b"1337" {
                    self.is_prefix_complete = true;
                } else {
                    self.is_ignored = true;
                }
                return Ok(());
            }
            if self.prefix_bytes.len() == 4
                || !iterm_input_byte.is_ascii_digit()
                || iterm_input_byte != b"1337"[self.prefix_bytes.len()]
            {
                self.is_ignored = true;
                return Ok(());
            }
            self.prefix_bytes.push(iterm_input_byte);
            return Ok(());
        }
        push_bounded_bytes(
            &mut self.command_bytes,
            iterm_input_byte,
            MAX_GRAPHICS_TRANSFER_BYTE_COUNT,
            GraphicsProtocol::Iterm2,
        )
    }
}

#[derive(Clone)]
struct TmuxParser {
    prefix_bytes: Vec<u8>,
    payload_bytes: Vec<u8>,
    is_escaped: bool,
    is_inner_terminated: bool,
}

impl TmuxParser {
    fn new() -> Self {
        Self::from_first_byte(b't')
    }

    fn from_first_byte(first_byte: u8) -> Self {
        TmuxParser {
            prefix_bytes: vec![first_byte],
            payload_bytes: Vec::new(),
            is_escaped: false,
            is_inner_terminated: false,
        }
    }
}

#[derive(Clone)]
struct ScreenParser {
    payload_bytes: Vec<u8>,
    is_escaped: bool,
    is_inner_terminated: bool,
    inner_data_start_index: usize,
}

impl ScreenParser {
    fn new() -> Self {
        Self::from_first_byte(0x1b)
    }

    fn from_first_byte(first_byte: u8) -> Self {
        ScreenParser {
            payload_bytes: vec![first_byte],
            is_escaped: false,
            is_inner_terminated: false,
            inner_data_start_index: 0,
        }
    }
}

fn push_bounded_bytes(
    bounded_bytes: &mut Vec<u8>,
    graphics_input_byte: u8,
    maximum_byte_count: usize,
    protocol: GraphicsProtocol,
) -> Result<(), GraphicsError> {
    if bounded_bytes.len() == maximum_byte_count {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    bounded_bytes.push(graphics_input_byte);
    Ok(())
}

#[cfg(test)]
mod tests;
