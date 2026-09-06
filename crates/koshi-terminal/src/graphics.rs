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
    iterm_command_can_be_graphics, iterm_command_is_graphics, iterm_payload_started,
    parse_iterm_command, ItermTransfer,
};
use koshi_kitty::{
    parse_command, reply_display, start_animation_transfer, start_transfer, KittyAnimationChunk,
    KittyAnimationTransfer, KittyAnimationTransferOutcome, KittyChunk, KittyParser, KittyTransfer,
    KittyTransferOutcome, MAX_KITTY_CHUNK_BYTES,
};
use koshi_sixel::{
    SixelGraphic, SixelParser as ProtocolSixelParser, SixelPhase as ProtocolSixelPhase,
};
use serde::de::{self, DeserializeSeed, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) use koshi_image::checked_rgba_len;
pub use koshi_image::{
    DecodedAnimation, DecodedGraphics, DecodedImage, GraphicsError, GraphicsProtocol, ImageAction,
    ImageDimension, ImageDisplay, ImagePlacementError, ImageRecord, SixelBackground,
    MAX_GRAPHICS_CARRY_BYTES, MAX_GRAPHICS_CONTROL_BYTES, MAX_GRAPHICS_TRANSFER_BYTES,
    MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE,
};

/// The largest GNU Screen passthrough body accepted by this parser.
const MAX_SCREEN_PASSTHROUGH_BYTES: usize = 768;

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
/// `carry` rebuilds this parser's own active sequence or multipart transfer.
/// The two nested records rebuild parsers inside a split GNU Screen or tmux
/// wrapper. A one-pixel red iTerm2 transfer split after `ESC ] 1337;File` has
/// the outer wrapper in one record and the unfinished iTerm2 command in its
/// nested record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphicsTransportState {
    /// Bytes that rebuild this parser's own active sequence or transfer.
    #[serde(default)]
    pub carry: Vec<u8>,
    /// Whether [`carry`](Self::carry) contains a complete bounded rebuild.
    /// `false` means [`abandonment`](Self::abandonment) describes how to drain
    /// the open transfer after restore.
    #[serde(default = "default_true")]
    pub carryable: bool,
    /// How to drain an open sequence when `carryable` is false.
    #[serde(default)]
    pub abandonment: Option<GraphicsAbandonment>,
    /// Whether the next DCS is a GNU Screen continuation wrapper.
    #[serde(default)]
    pub screen_continuation: bool,
    /// Whether the carried bytes are inside an open GNU Screen wrapper.
    #[serde(default)]
    pub screen_wrapper_active: bool,
    /// The parser state inside the carried GNU Screen wrapper, when that
    /// wrapper ended while its enclosed stream was incomplete.
    #[serde(default)]
    pub screen_inner: Option<Box<GraphicsTransportState>>,
    /// Whether the next DCS is a tmux continuation wrapper.
    #[serde(default)]
    pub tmux_continuation: bool,
    /// Whether the carried bytes are inside an open tmux wrapper.
    #[serde(default)]
    pub tmux_wrapper_active: bool,
    /// The parser state inside the carried tmux wrapper, when that wrapper
    /// ended while its enclosed stream was incomplete.
    #[serde(default)]
    pub tmux_inner: Option<Box<GraphicsTransportState>>,
}

impl Default for GraphicsTransportState {
    fn default() -> Self {
        GraphicsTransportState {
            carry: Vec::new(),
            carryable: true,
            abandonment: None,
            screen_continuation: false,
            screen_wrapper_active: false,
            screen_inner: None,
            tmux_continuation: false,
            tmux_wrapper_active: false,
            tmux_inner: None,
        }
    }
}

impl<'de> Deserialize<'de> for GraphicsTransportState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(GraphicsTransportVisitor { depth: 0 })
    }
}

struct GraphicsTransportVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for GraphicsTransportVisitor {
    type Value = GraphicsTransportState;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a graphics transport state object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut carry = None;
        let mut carryable = None;
        let mut abandonment = None;
        let mut screen_continuation = None;
        let mut screen_wrapper_active = None;
        let mut screen_inner = None;
        let mut tmux_continuation = None;
        let mut tmux_wrapper_active = None;
        let mut tmux_inner = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "carry" => {
                    if carry.is_some() {
                        return Err(de::Error::duplicate_field("carry"));
                    }
                    carry = Some(map.next_value_seed(BoundedBytesSeed::new(
                        MAX_GRAPHICS_CARRY_BYTES,
                        "graphics carry",
                    ))?);
                }
                "carryable" => {
                    if carryable.is_some() {
                        return Err(de::Error::duplicate_field("carryable"));
                    }
                    carryable = Some(map.next_value()?);
                }
                "abandonment" => {
                    if abandonment.is_some() {
                        return Err(de::Error::duplicate_field("abandonment"));
                    }
                    abandonment = Some(map.next_value()?);
                }
                "screen_continuation" => {
                    if screen_continuation.is_some() {
                        return Err(de::Error::duplicate_field("screen_continuation"));
                    }
                    screen_continuation = Some(map.next_value()?);
                }
                "screen_wrapper_active" => {
                    if screen_wrapper_active.is_some() {
                        return Err(de::Error::duplicate_field("screen_wrapper_active"));
                    }
                    screen_wrapper_active = Some(map.next_value()?);
                }
                "screen_inner" => {
                    if screen_inner.is_some() {
                        return Err(de::Error::duplicate_field("screen_inner"));
                    }
                    screen_inner = Some(map.next_value_seed(GraphicsTransportOptionSeed {
                        depth: self.depth.saturating_add(1),
                    })?);
                }
                "tmux_continuation" => {
                    if tmux_continuation.is_some() {
                        return Err(de::Error::duplicate_field("tmux_continuation"));
                    }
                    tmux_continuation = Some(map.next_value()?);
                }
                "tmux_wrapper_active" => {
                    if tmux_wrapper_active.is_some() {
                        return Err(de::Error::duplicate_field("tmux_wrapper_active"));
                    }
                    tmux_wrapper_active = Some(map.next_value()?);
                }
                "tmux_inner" => {
                    if tmux_inner.is_some() {
                        return Err(de::Error::duplicate_field("tmux_inner"));
                    }
                    tmux_inner = Some(map.next_value_seed(GraphicsTransportOptionSeed {
                        depth: self.depth.saturating_add(1),
                    })?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }

        Ok(GraphicsTransportState {
            carry: carry.unwrap_or_default(),
            carryable: carryable.unwrap_or_else(default_true),
            abandonment: abandonment.unwrap_or_default(),
            screen_continuation: screen_continuation.unwrap_or(false),
            screen_wrapper_active: screen_wrapper_active.unwrap_or(false),
            screen_inner: screen_inner.unwrap_or_default(),
            tmux_continuation: tmux_continuation.unwrap_or(false),
            tmux_wrapper_active: tmux_wrapper_active.unwrap_or(false),
            tmux_inner: tmux_inner.unwrap_or_default(),
        })
    }
}

struct GraphicsTransportOptionSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for GraphicsTransportOptionSeed {
    type Value = Option<Box<GraphicsTransportState>>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(GraphicsTransportOptionVisitor { depth: self.depth })
    }
}

struct GraphicsTransportOptionVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for GraphicsTransportOptionVisitor {
    type Value = Option<Box<GraphicsTransportState>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("null or a nested graphics transport state object")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.depth > MAX_GRAPHICS_WRAPPER_DEPTH {
            return Err(de::Error::custom(
                "graphics wrapper nesting exceeds the supported limit",
            ));
        }
        let state = deserializer.deserialize_map(GraphicsTransportVisitor { depth: self.depth })?;
        Ok(Some(Box::new(state)))
    }
}

fn default_true() -> bool {
    true
}

/// One completed graphics operation in terminal byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GraphicsOperation {
    Failure {
        display: ImageDisplay,
        error: GraphicsError,
    },
    Image(DecodedGraphics),
    Sixel(SixelGraphic),
    Command(KittyCommand),
}

/// Terminal graphics parser that frames protocol strings and queues operations.
#[derive(Clone)]
pub(crate) struct GraphicsParser {
    state: GraphicsState,
    pending: Vec<u8>,
    carryable: bool,
    sequence_bytes: usize,
    wrapper_depth: usize,
    transfer_carry: Vec<u8>,
    transfer_carryable: bool,
    abandoned_transfer: Option<GraphicsProtocol>,
    screen_continuation: bool,
    screen_inner: Option<Box<GraphicsParser>>,
    tmux_continuation: bool,
    tmux_inner: Option<Box<GraphicsParser>>,
    kitty_transfer: Option<KittyTransfer>,
    kitty_animation_transfer: Option<KittyAnimationTransfer>,
    iterm_transfer: Option<ItermTransfer>,
}

pub(crate) struct GraphicsAdvance {
    pub(crate) events: Vec<(usize, Result<GraphicsOperation, GraphicsError>)>,
    pub(crate) terminal_inert: Vec<Range<usize>>,
}

impl Default for GraphicsParser {
    fn default() -> Self {
        GraphicsParser {
            state: GraphicsState::default(),
            pending: Vec::new(),
            carryable: true,
            sequence_bytes: 0,
            wrapper_depth: 0,
            transfer_carry: Vec::new(),
            transfer_carryable: true,
            abandoned_transfer: None,
            screen_continuation: false,
            screen_inner: None,
            tmux_continuation: false,
            tmux_inner: None,
            kitty_transfer: None,
            kitty_animation_transfer: None,
            iterm_transfer: None,
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
    Iterm(ItermParser),
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
    fn protocol(self) -> GraphicsProtocol {
        match self {
            StringKind::Dcs => GraphicsProtocol::Sixel,
            StringKind::Apc => GraphicsProtocol::Kitty,
            StringKind::Osc => GraphicsProtocol::Iterm2,
        }
    }
}

fn string_kind(protocol: GraphicsProtocol) -> StringKind {
    match protocol {
        GraphicsProtocol::Sixel => StringKind::Dcs,
        GraphicsProtocol::Kitty => StringKind::Apc,
        GraphicsProtocol::Iterm2 => StringKind::Osc,
    }
}

#[derive(Clone)]
struct DiscardParser {
    kind: StringKind,
    error: GraphicsError,
    escaped: bool,
    report: bool,
}

impl GraphicsParser {
    /// Feed bytes and return every image or error completed by this chunk.
    pub(crate) fn advance_operations(
        &mut self,
        bytes: &[u8],
    ) -> Vec<Result<GraphicsOperation, GraphicsError>> {
        self.advance_with_offsets(bytes)
            .events
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    pub(crate) fn advance_with_offsets(&mut self, bytes: &[u8]) -> GraphicsAdvance {
        let mut events = Vec::new();
        let mut terminal_inert = Vec::new();
        let mut byte_events = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            if let Some(consumed) = self.feed_discard_data(&bytes[offset..]) {
                offset += consumed;
                continue;
            }
            if let Some(consumed) = self
                .feed_kitty_data(&bytes[offset..])
                .or_else(|| self.feed_iterm_data(&bytes[offset..]))
            {
                terminal_inert.push(offset..offset + consumed);
                offset += consumed;
                continue;
            }
            if let Some(consumed) = self.feed_tmux_data(&bytes[offset..]) {
                terminal_inert.push(offset..offset + consumed);
                offset += consumed;
                continue;
            }
            if let Some(consumed) = self.feed_screen_data(&bytes[offset..]) {
                terminal_inert.push(offset..offset + consumed);
                offset += consumed;
                continue;
            }
            let sixel_data = matches!(
                &self.state,
                GraphicsState::Sixel(parser)
                    if parser.phase == ProtocolSixelPhase::Body
                        && !parser.escaped
                        && bytes[offset].is_ascii_graphic()
            );
            byte_events.clear();
            self.feed_byte(bytes[offset], &mut byte_events);
            events.extend(byte_events.drain(..).map(|event| (offset, event)));
            if sixel_data && matches!(self.state, GraphicsState::Sixel(_)) {
                extend_last_range(&mut terminal_inert, offset);
            }
            offset += 1;
        }
        GraphicsAdvance {
            events,
            terminal_inert,
        }
    }

    /// Advance one data run in a discarded control string.
    fn feed_discard_data(&mut self, bytes: &[u8]) -> Option<usize> {
        let kind = match &self.state {
            GraphicsState::Discard(parser) if !parser.escaped => parser.kind,
            _ => return None,
        };
        let consumed = bytes
            .iter()
            .position(|byte| {
                matches!(*byte, 0x18 | 0x1a | 0x1b | 0x9c)
                    || (kind == StringKind::Osc && *byte == 0x07)
            })
            .unwrap_or(bytes.len());
        if consumed == 0 {
            return None;
        }

        self.push_pending_bytes(&bytes[..consumed]);
        self.sequence_bytes = self.sequence_bytes.saturating_add(consumed);
        Some(consumed)
    }

    /// Copy one run of Kitty payload bytes that contains no string control.
    fn feed_kitty_data(&mut self, bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Kitty(parser) = &self.state else {
            return None;
        };
        if parser.is_ignored() || parser.is_escaped() || !parser.has_header() {
            return None;
        }

        let data_room = MAX_KITTY_CHUNK_BYTES.saturating_sub(parser.payload_len());
        let sequence_room = MAX_GRAPHICS_TRANSFER_BYTES.saturating_sub(self.sequence_bytes);
        let consumed = base64_run_len(bytes, data_room.min(sequence_room));
        if consumed == 0 {
            return None;
        }

        self.push_pending_bytes(&bytes[..consumed]);
        self.sequence_bytes = self.sequence_bytes.saturating_add(consumed);
        let GraphicsState::Kitty(parser) = &mut self.state else {
            unreachable!("the Kitty parser state was checked above")
        };
        parser
            .append_payload(&bytes[..consumed])
            .expect("the bounded Kitty payload run fits");
        Some(consumed)
    }

    /// Copy one run of iTerm2 payload bytes that contains no string control.
    fn feed_iterm_data(&mut self, bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Iterm(parser) = &self.state else {
            return None;
        };
        if parser.ignored
            || parser.escaped
            || !parser.prefix_done
            || !iterm_payload_started(&parser.body)
        {
            return None;
        }

        let body_room = MAX_GRAPHICS_TRANSFER_BYTES.saturating_sub(parser.body.len());
        let sequence_room = MAX_GRAPHICS_TRANSFER_BYTES.saturating_sub(self.sequence_bytes);
        let consumed = base64_run_len(bytes, body_room.min(sequence_room));
        if consumed == 0 {
            return None;
        }

        self.push_pending_bytes(&bytes[..consumed]);
        self.sequence_bytes = self.sequence_bytes.saturating_add(consumed);
        let GraphicsState::Iterm(parser) = &mut self.state else {
            unreachable!("the iTerm2 parser state was checked above")
        };
        parser.body.extend_from_slice(&bytes[..consumed]);
        Some(consumed)
    }

    /// Copy one run of tmux wrapper bytes that contains no wrapper control.
    fn feed_tmux_data(&mut self, bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Tmux(parser) = &self.state else {
            return None;
        };
        if parser.prefix.len() < b"tmux;".len() || parser.escaped {
            return None;
        }

        let data_room = MAX_GRAPHICS_TRANSFER_BYTES.saturating_sub(parser.data.len());
        let sequence_room = MAX_GRAPHICS_TRANSFER_BYTES.saturating_sub(self.sequence_bytes);
        let limit = bytes.len().min(data_room).min(sequence_room);
        let consumed = bytes[..limit]
            .iter()
            .position(|byte| matches!(*byte, 0x18 | 0x1a | 0x1b | 0x9c))
            .unwrap_or(limit);
        if consumed == 0 {
            return None;
        }

        self.push_pending_bytes(&bytes[..consumed]);
        self.sequence_bytes = self.sequence_bytes.saturating_add(consumed);
        let GraphicsState::Tmux(parser) = &mut self.state else {
            unreachable!("the tmux parser state was checked above")
        };
        parser.data.extend_from_slice(&bytes[..consumed]);
        Some(consumed)
    }

    /// Copy one run of GNU Screen wrapper bytes that contains no wrapper control.
    fn feed_screen_data(&mut self, bytes: &[u8]) -> Option<usize> {
        let GraphicsState::Screen(parser) = &self.state else {
            return None;
        };
        if parser.escaped
            || (parser.data.as_slice() == [0x1b] && bytes.first().copied() == Some(b'\\'))
        {
            return None;
        }

        let data_room = MAX_SCREEN_PASSTHROUGH_BYTES.saturating_sub(parser.data.len());
        let sequence_room = MAX_GRAPHICS_TRANSFER_BYTES.saturating_sub(self.sequence_bytes);
        let limit = bytes.len().min(data_room).min(sequence_room);
        let consumed = bytes[..limit]
            .iter()
            .position(|byte| matches!(*byte, 0x18 | 0x1a | 0x1b | 0x9c))
            .unwrap_or(limit);
        if consumed == 0 {
            return None;
        }

        self.push_pending_bytes(&bytes[..consumed]);
        self.sequence_bytes = self.sequence_bytes.saturating_add(consumed);
        let GraphicsState::Screen(parser) = &mut self.state else {
            unreachable!("the GNU Screen parser state was checked above")
        };
        parser.data.extend_from_slice(&bytes[..consumed]);
        Some(consumed)
    }

    /// Return bytes needed to rebuild an active graphics parser after a
    /// process-image swap. An empty slice means that the active transfer
    /// exceeded the carry bound and must not be resumed from its opening.
    pub(crate) fn carry_bytes(&self) -> Option<&[u8]> {
        if matches!(self.state, GraphicsState::Ground) {
            if let Some(inner) = &self.screen_inner {
                return inner.carry_bytes();
            }
            if let Some(inner) = &self.tmux_inner {
                return inner.carry_bytes();
            }
        }
        if matches!(self.state, GraphicsState::Ground) {
            if self.has_open_transfer() && self.transfer_carryable {
                Some(&self.transfer_carry)
            } else if self.has_open_transfer() {
                Some(&[])
            } else {
                None
            }
        } else if self.carryable {
            Some(&self.pending)
        } else {
            Some(&[])
        }
    }

    pub(crate) fn transport_state(&self) -> Option<GraphicsTransportState> {
        self.has_pending_state().then(|| self.transport_snapshot())
    }

    fn transport_snapshot(&self) -> GraphicsTransportState {
        let (carry, carryable) = if matches!(self.state, GraphicsState::Ground) {
            if self.has_own_transfer() {
                (self.transfer_carry.clone(), self.transfer_carryable)
            } else {
                (Vec::new(), true)
            }
        } else {
            (self.pending.clone(), self.carryable)
        };
        GraphicsTransportState {
            carry,
            carryable,
            abandonment: self.abandonment(),
            screen_continuation: self.screen_continuation,
            screen_wrapper_active: self.screen_wrapper_active(),
            screen_inner: self
                .screen_inner
                .as_ref()
                .map(|inner| Box::new(inner.transport_snapshot())),
            tmux_continuation: self.tmux_continuation,
            tmux_wrapper_active: self.tmux_wrapper_active(),
            tmux_inner: self
                .tmux_inner
                .as_ref()
                .map(|inner| Box::new(inner.transport_snapshot())),
        }
    }

    /// Return whether a multipart transfer has bytes that still need a final
    /// protocol record.
    pub(crate) fn has_open_transfer(&self) -> bool {
        self.kitty_transfer.is_some()
            || self.kitty_animation_transfer.is_some()
            || self.iterm_transfer.is_some()
            || self
                .screen_inner
                .as_ref()
                .is_some_and(|inner| inner.has_open_transfer())
            || self
                .tmux_inner
                .as_ref()
                .is_some_and(|inner| inner.has_open_transfer())
    }

    fn has_own_transfer(&self) -> bool {
        self.kitty_transfer.is_some()
            || self.kitty_animation_transfer.is_some()
            || self.iterm_transfer.is_some()
    }

    fn state_protocol(&self) -> GraphicsProtocol {
        match &self.state {
            GraphicsState::Kitty(_) => GraphicsProtocol::Kitty,
            GraphicsState::Iterm(_) => GraphicsProtocol::Iterm2,
            GraphicsState::Discard(parser) => parser.kind.protocol(),
            _ => GraphicsProtocol::Sixel,
        }
    }

    fn abandonment(&self) -> Option<GraphicsAbandonment> {
        if let Some(protocol) = self.abandoned_transfer {
            return Some(GraphicsAbandonment::Transfer(protocol));
        }
        let carryable = if matches!(self.state, GraphicsState::Ground) && self.has_own_transfer() {
            self.transfer_carryable
        } else {
            self.carryable
        };
        if carryable {
            return None;
        }
        if self.has_own_transfer() {
            let protocol =
                if self.kitty_transfer.is_some() || self.kitty_animation_transfer.is_some() {
                    GraphicsProtocol::Kitty
                } else {
                    GraphicsProtocol::Iterm2
                };
            Some(GraphicsAbandonment::Transfer(protocol))
        } else {
            let protocol = self.state_protocol();
            if self.reports_active_sequence() {
                Some(GraphicsAbandonment::Sequence(protocol))
            } else {
                Some(GraphicsAbandonment::SilentSequence(protocol))
            }
        }
    }

    fn has_pending_state(&self) -> bool {
        !matches!(self.state, GraphicsState::Ground)
            || self.abandoned_transfer.is_some()
            || self.screen_continuation
            || self.screen_inner.is_some()
            || self.tmux_continuation
            || self.tmux_inner.is_some()
            || self.has_open_transfer()
    }

    /// Return whether the next DCS is a GNU Screen continuation wrapper.
    pub(crate) fn screen_continuation(&self) -> bool {
        self.screen_continuation
    }

    pub(crate) fn screen_wrapper_active(&self) -> bool {
        self.screen_continuation
            && matches!(
                self.state,
                GraphicsState::DcsIntro | GraphicsState::Screen(_)
            )
    }

    pub(crate) fn tmux_continuation(&self) -> bool {
        self.tmux_continuation
    }

    pub(crate) fn tmux_wrapper_active(&self) -> bool {
        self.tmux_continuation
            && matches!(self.state, GraphicsState::DcsIntro | GraphicsState::Tmux(_))
    }

    pub(crate) fn restore_carry(&mut self, bytes: &[u8], transport: GraphicsTransportState) {
        self.restore_transport(transport, Some(bytes));
    }

    fn restore_transport(
        &mut self,
        transport: GraphicsTransportState,
        top_level_bytes: Option<&[u8]>,
    ) {
        self.screen_continuation = transport.screen_continuation;
        self.tmux_continuation = transport.tmux_continuation;
        self.screen_inner = transport.screen_inner.map(|inner| {
            Box::new(GraphicsParser::from_transport(
                *inner,
                self.wrapper_depth.saturating_add(1),
            ))
        });
        self.tmux_inner = transport.tmux_inner.map(|inner| {
            Box::new(GraphicsParser::from_transport(
                *inner,
                self.wrapper_depth.saturating_add(1),
            ))
        });

        self.abandoned_transfer = match transport.abandonment {
            Some(GraphicsAbandonment::Transfer(protocol)) => Some(protocol),
            Some(GraphicsAbandonment::Sequence(_))
            | Some(GraphicsAbandonment::SilentSequence(_))
            | None => None,
        };

        if let Some(GraphicsAbandonment::Sequence(protocol)) = transport.abandonment {
            self.state = GraphicsState::Discard(DiscardParser {
                kind: string_kind(protocol),
                error: GraphicsError::TransferTooLarge { protocol },
                escaped: false,
                report: true,
            });
            self.pending.clear();
            self.carryable = false;
            self.sequence_bytes = 0;
            return;
        }
        if let Some(GraphicsAbandonment::SilentSequence(protocol)) = transport.abandonment {
            self.state = GraphicsState::Discard(DiscardParser {
                kind: string_kind(protocol),
                error: GraphicsError::TransferTooLarge { protocol },
                escaped: false,
                report: false,
            });
            self.pending.clear();
            self.carryable = false;
            self.sequence_bytes = 0;
            return;
        }

        let provided_bytes = top_level_bytes
            .filter(|bytes| !bytes.is_empty())
            .unwrap_or(&transport.carry);
        if self.screen_continuation
            && !transport.screen_wrapper_active
            && self.screen_inner.is_none()
            && !provided_bytes.is_empty()
        {
            let mut inner = GraphicsParser {
                wrapper_depth: self.wrapper_depth.saturating_add(1),
                ..GraphicsParser::default()
            };
            let _ = inner.advance_operations(provided_bytes);
            self.screen_inner = Some(Box::new(inner));
            return;
        }
        if self.tmux_continuation
            && !transport.tmux_wrapper_active
            && self.tmux_inner.is_none()
            && !provided_bytes.is_empty()
        {
            let mut inner = GraphicsParser {
                wrapper_depth: self.wrapper_depth.saturating_add(1),
                ..GraphicsParser::default()
            };
            let _ = inner.advance_operations(provided_bytes);
            self.tmux_inner = Some(Box::new(inner));
            return;
        }
        let bytes = if self.screen_inner.is_some() || self.tmux_inner.is_some() {
            transport.carry.as_slice()
        } else {
            provided_bytes
        };
        if transport.carryable {
            let _ = self.advance_operations(bytes);
        }
    }

    fn from_transport(transport: GraphicsTransportState, wrapper_depth: usize) -> Self {
        let mut parser = GraphicsParser {
            wrapper_depth,
            ..GraphicsParser::default()
        };
        parser.restore_transport(transport, None);
        parser
    }

    /// Finish a stream and report any active sequence or multipart transfer.
    pub(crate) fn finish(&mut self) -> Vec<Result<GraphicsOperation, GraphicsError>> {
        let mut events = Vec::new();
        let active_error = match &self.state {
            GraphicsState::Discard(parser) => Some(parser.error.clone()),
            _ => None,
        };
        let active_protocol = if self.reports_active_sequence() {
            Some(self.state_protocol())
        } else {
            None
        };
        if let Some(protocol) = active_protocol {
            let report = !matches!(
                &self.state,
                GraphicsState::Discard(parser) if !parser.report
            );
            self.reset();
            if report && self.abandoned_transfer != Some(protocol) {
                events.push(Err(
                    active_error.unwrap_or(GraphicsError::Truncated { protocol })
                ));
            }
        } else if !matches!(self.state, GraphicsState::Ground) {
            self.reset();
        }
        if self.kitty_transfer.take().is_some() && active_protocol != Some(GraphicsProtocol::Kitty)
        {
            events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Kitty,
            }));
        }
        if self.kitty_animation_transfer.take().is_some()
            && active_protocol != Some(GraphicsProtocol::Kitty)
        {
            events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Kitty,
            }));
        }
        if self.iterm_transfer.take().is_some() && active_protocol != Some(GraphicsProtocol::Iterm2)
        {
            events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Iterm2,
            }));
        }
        if let Some(protocol) = self.abandoned_transfer.take() {
            events.push(Err(GraphicsError::TransferTooLarge { protocol }));
        }
        if let Some(mut inner) = self.screen_inner.take() {
            events.extend(inner.finish());
        }
        if let Some(mut inner) = self.tmux_inner.take() {
            events.extend(inner.finish());
        }
        self.transfer_carry.clear();
        self.transfer_carryable = true;
        self.screen_continuation = false;
        self.tmux_continuation = false;
        self.screen_inner = None;
        self.tmux_inner = None;
        events
    }

    fn reports_active_sequence(&self) -> bool {
        match &self.state {
            GraphicsState::Ground | GraphicsState::Escape | GraphicsState::DcsIntro => false,
            GraphicsState::Kitty(parser) => {
                !parser.is_ignored() && parser.header().first().copied() == Some(b'G')
            }
            GraphicsState::Iterm(parser) => {
                !parser.ignored
                    && (parser.prefix_done || parser.prefix.as_slice() == b"1337")
                    && iterm_command_is_graphics(&parser.body)
            }
            GraphicsState::Sixel(parser) => parser.phase == ProtocolSixelPhase::Body,
            GraphicsState::Tmux(parser) => {
                parser.prefix.len() >= b"tmux;".len()
                    && self.wrapper_contains_graphics(self.tmux_inner.as_deref(), &parser.data)
            }
            GraphicsState::Screen(parser) => {
                self.wrapper_contains_graphics(self.screen_inner.as_deref(), &parser.data)
            }
            GraphicsState::Discard(parser) => parser.report,
        }
    }

    fn wrapper_contains_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        let mut parser = inner.cloned().unwrap_or_default();
        let events = parser.advance_operations(data);
        !events.is_empty() || parser.has_graphics_state()
    }

    fn has_graphics_state(&self) -> bool {
        self.reports_active_sequence()
            || self.has_open_transfer()
            || self
                .screen_inner
                .as_ref()
                .is_some_and(|inner| inner.has_graphics_state())
            || self
                .tmux_inner
                .as_ref()
                .is_some_and(|inner| inner.has_graphics_state())
    }

    fn feed_byte(&mut self, byte: u8, events: &mut Vec<Result<GraphicsOperation, GraphicsError>>) {
        if !matches!(self.state, GraphicsState::Ground) {
            self.push_pending(byte);
            if self.sequence_bytes == MAX_GRAPHICS_TRANSFER_BYTES
                && !matches!(self.state, GraphicsState::Discard(_))
            {
                let kind = match &self.state {
                    GraphicsState::Kitty(_) => StringKind::Apc,
                    GraphicsState::Iterm(_) => StringKind::Osc,
                    _ => StringKind::Dcs,
                };
                self.discard(
                    kind,
                    GraphicsError::TransferTooLarge {
                        protocol: kind.protocol(),
                    },
                    byte,
                );
                return;
            }
            self.sequence_bytes = self.sequence_bytes.saturating_add(1);
        }

        match std::mem::take(&mut self.state) {
            GraphicsState::Ground => {
                if byte == 0x18 || byte == 0x1a {
                    self.cancel_transfers();
                } else if byte == 0x1b {
                    self.begin(GraphicsState::Escape, byte);
                } else if byte == 0x90 {
                    self.begin(GraphicsState::DcsIntro, byte);
                } else if matches!(byte, 0x98 | 0x9e) {
                    self.begin_silent_string(byte);
                } else if byte == 0x9f {
                    self.begin_transfer(GraphicsState::Kitty(KittyParser::new()), byte);
                } else if byte == 0x9d {
                    self.begin_transfer(GraphicsState::Iterm(ItermParser::new()), byte);
                }
            }
            GraphicsState::Escape => self.feed_escape(byte),
            GraphicsState::DcsIntro => self.feed_dcs_intro(byte, events),
            GraphicsState::Sixel(parser) => self.feed_sixel(parser, byte, events),
            GraphicsState::Kitty(parser) => self.feed_kitty(parser, byte, events),
            GraphicsState::Iterm(parser) => self.feed_iterm(parser, byte, events),
            GraphicsState::Tmux(parser) => self.feed_tmux(parser, byte, events),
            GraphicsState::Screen(parser) => self.feed_screen(parser, byte, events),
            GraphicsState::Discard(parser) => self.feed_discard(parser, byte, events),
        }
    }

    fn feed_escape(&mut self, byte: u8) {
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
            return;
        }
        match byte {
            b'P' => self.state = GraphicsState::DcsIntro,
            b'_' => self.continue_transfer(GraphicsState::Kitty(KittyParser::new())),
            b']' => self.continue_transfer(GraphicsState::Iterm(ItermParser::new())),
            b'X' | b'^' => self.ignore_string(StringKind::Dcs, byte),
            0x1b => {
                self.pending.clear();
                self.carryable = true;
                self.pending.push(byte);
                self.state = GraphicsState::Escape;
            }
            _ => self.reset(),
        }
    }

    fn feed_dcs_intro(
        &mut self,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
            return;
        }
        if self.screen_continuation {
            if byte == 0x9c {
                self.finish_screen(Vec::new(), events);
            } else {
                self.state = GraphicsState::Screen(ScreenParser::from_first(byte));
            }
            return;
        }
        if self.tmux_continuation {
            if byte == 0x9c {
                self.finish_tmux(Vec::new(), events);
            } else {
                self.state = GraphicsState::Tmux(TmuxParser::from_first(byte));
            }
            return;
        }
        if byte == 0x9c {
            self.reset();
            return;
        }
        if self.wrapper_depth >= MAX_GRAPHICS_WRAPPER_DEPTH && matches!(byte, b't' | 0x1b) {
            self.discard(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                byte,
            );
            return;
        }
        match byte {
            b'q' => {
                let mut parser = ProtocolSixelParser::new();
                if parser.feed(b'q').is_err() {
                    self.reset();
                } else {
                    self.state = GraphicsState::Sixel(Box::new(parser));
                }
            }
            b't' => self.state = GraphicsState::Tmux(TmuxParser::new()),
            0x1b => self.state = GraphicsState::Screen(ScreenParser::new()),
            b'0'..=b'9' | b';' => {
                let mut parser = ProtocolSixelParser::new();
                if parser.feed(byte).is_err() {
                    self.reset();
                } else {
                    self.state = GraphicsState::Sixel(Box::new(parser));
                }
            }
            _ => self.ignore_string(StringKind::Dcs, byte),
        }
    }

    fn feed_sixel(
        &mut self,
        mut parser: Box<ProtocolSixelParser>,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.escaped {
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                parser.escaped = false;
                self.finish_sixel((*parser).finish(), events);
            } else {
                self.discard(
                    StringKind::Dcs,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Sixel(parser);
        } else if byte == 0x9c {
            self.finish_sixel(parser.finish(), events);
        } else if let Err(error) = parser.feed(byte) {
            if parser.phase == ProtocolSixelPhase::Header
                && byte != b'q'
                && !matches!(error, GraphicsError::TransferTooLarge { .. })
            {
                self.ignore_string(StringKind::Dcs, byte);
            } else {
                self.discard(StringKind::Dcs, error, byte);
            }
        } else {
            self.state = GraphicsState::Sixel(parser);
        }
    }

    fn finish_sixel(
        &mut self,
        result: Result<SixelGraphic, GraphicsError>,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let failed = result.is_err();
        self.reset();
        match result {
            Ok(graphic) => events.push(Ok(GraphicsOperation::Sixel(graphic))),
            Err(error) => events.push(Err(error)),
        }
        if failed {
            self.screen_continuation = false;
            self.screen_inner = None;
            self.tmux_continuation = false;
            self.tmux_inner = None;
        }
    }

    fn feed_kitty(
        &mut self,
        mut parser: KittyParser,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.is_ignored() {
            if parser.is_escaped() {
                parser.set_escaped(false);
                if byte == b'\\' {
                    self.reset();
                } else if byte == 0x18 || byte == 0x1a {
                    self.cancel_transfers();
                    self.reset();
                } else {
                    parser.set_escaped(byte == 0x1b);
                    self.state = GraphicsState::Kitty(parser);
                }
            } else if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == 0x1b {
                parser.set_escaped(true);
                self.state = GraphicsState::Kitty(parser);
            } else if byte == 0x9c {
                self.reset();
            } else {
                self.state = GraphicsState::Kitty(parser);
            }
            return;
        }
        if parser.is_escaped() {
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                parser.set_escaped(false);
                self.finish_kitty(parser, events);
            } else {
                self.discard(
                    StringKind::Apc,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.set_escaped(true);
            self.state = GraphicsState::Kitty(parser);
        } else if byte == 0x9c {
            self.finish_kitty(parser, events);
        } else if let Err(error) = parser.feed(byte) {
            self.discard(StringKind::Apc, error, byte);
        } else if parser.is_ignored() {
            self.ignore_string(StringKind::Apc, byte);
        } else {
            self.state = GraphicsState::Kitty(parser);
        }
    }

    fn feed_iterm(
        &mut self,
        mut parser: ItermParser,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.escaped {
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                parser.escaped = false;
                self.finish_iterm(parser, events);
            } else {
                self.discard(
                    StringKind::Osc,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Iterm2,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Iterm(parser);
        } else if byte == 0x07 || byte == 0x9c {
            self.finish_iterm(parser, events);
        } else if let Err(error) = parser.feed(byte) {
            self.discard(StringKind::Osc, error, byte);
        } else if parser.ignored || !iterm_command_can_be_graphics(&parser.body) {
            self.ignore_string(StringKind::Osc, byte);
        } else {
            self.state = GraphicsState::Iterm(parser);
        }
    }

    fn feed_tmux(
        &mut self,
        mut parser: TmuxParser,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.prefix.len() < b"tmux;".len() {
            if byte == 0x9c {
                self.reset();
                return;
            }
            let expected = b"tmux;"[parser.prefix.len()];
            if byte != expected {
                self.ignore_string(StringKind::Dcs, byte);
            } else {
                parser.prefix.push(byte);
                self.state = GraphicsState::Tmux(parser);
            }
            return;
        }
        if byte == 0x9c {
            if !parser.inner_terminated
                && !self.body_has_complete_graphics(self.tmux_inner.as_deref(), &parser.data)
                && self.body_has_c1_terminated_graphics(self.tmux_inner.as_deref(), &parser.data)
            {
                if parser.data.len() == MAX_GRAPHICS_TRANSFER_BYTES {
                    self.finish_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        events,
                    );
                } else {
                    parser.data.push(byte);
                    parser.inner_terminated = true;
                    self.state = GraphicsState::Tmux(parser);
                }
            } else {
                self.finish_tmux(parser.data, events);
            }
            return;
        }
        if parser.escaped {
            parser.escaped = false;
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == 0x1b {
                if parser.data.len() == MAX_GRAPHICS_TRANSFER_BYTES {
                    self.discard(
                        StringKind::Dcs,
                        GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        },
                        byte,
                    );
                } else {
                    parser.data.push(0x1b);
                    self.state = GraphicsState::Tmux(parser);
                }
            } else if byte == b'\\' {
                self.finish_tmux(parser.data, events);
            } else {
                self.discard(
                    StringKind::Dcs,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Tmux(parser);
        } else if parser.data.len() == MAX_GRAPHICS_TRANSFER_BYTES {
            self.discard(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                byte,
            );
        } else {
            parser.data.push(byte);
            self.state = GraphicsState::Tmux(parser);
        }
    }

    fn feed_screen(
        &mut self,
        mut parser: ScreenParser,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if byte == 0x9c {
            let inner_complete = if parser.inner_terminated {
                self.body_has_c1_terminated_graphics_after_boundary(
                    self.screen_inner.as_deref(),
                    &parser,
                )
            } else {
                !self.body_has_complete_graphics(self.screen_inner.as_deref(), &parser.data)
                    && self
                        .body_has_c1_terminated_graphics(self.screen_inner.as_deref(), &parser.data)
            };
            if inner_complete {
                if parser.data.len() == MAX_SCREEN_PASSTHROUGH_BYTES {
                    self.finish_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        events,
                    );
                    self.screen_continuation = false;
                } else {
                    parser.data.push(byte);
                    parser.inner_terminated = true;
                    parser.inner_data_start = parser.data.len();
                    self.state = GraphicsState::Screen(parser);
                }
            } else {
                self.finish_screen(parser.data, events);
            }
            return;
        }
        if !parser.escaped && parser.data.as_slice() == [0x1b] && byte == b'\\' {
            self.finish_screen(Vec::new(), events);
            return;
        }
        if parser.escaped {
            parser.escaped = false;
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                let inner_complete = if parser.inner_terminated {
                    self.body_has_st_terminated_graphics_after_boundary(
                        self.screen_inner.as_deref(),
                        &parser,
                    )
                } else {
                    !self.body_has_complete_graphics(self.screen_inner.as_deref(), &parser.data)
                        && self.body_has_st_terminated_graphics(
                            self.screen_inner.as_deref(),
                            &parser.data,
                        )
                };
                if inner_complete {
                    if parser.data.len() > MAX_SCREEN_PASSTHROUGH_BYTES.saturating_sub(2) {
                        self.finish_state(
                            Err(GraphicsError::TransferTooLarge {
                                protocol: GraphicsProtocol::Sixel,
                            }),
                            events,
                        );
                        self.screen_continuation = false;
                    } else {
                        parser.data.push(0x1b);
                        parser.data.push(b'\\');
                        parser.inner_terminated = true;
                        parser.inner_data_start = parser.data.len();
                        self.state = GraphicsState::Screen(parser);
                    }
                } else if parser.data.len() > MAX_SCREEN_PASSTHROUGH_BYTES {
                    self.finish_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        events,
                    );
                    self.screen_continuation = false;
                } else {
                    self.finish_screen(parser.data, events);
                }
            } else if parser.data.len() > MAX_SCREEN_PASSTHROUGH_BYTES.saturating_sub(2) {
                self.discard(
                    StringKind::Dcs,
                    GraphicsError::TransferTooLarge {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    byte,
                );
            } else {
                parser.data.push(0x1b);
                parser.data.push(byte);
                self.state = GraphicsState::Screen(parser);
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Screen(parser);
        } else if parser.data.len() == MAX_SCREEN_PASSTHROUGH_BYTES {
            self.discard(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                byte,
            );
        } else {
            parser.data.push(byte);
            self.state = GraphicsState::Screen(parser);
        }
    }

    fn finish_screen(
        &mut self,
        data: Vec<u8>,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let mut inner = self
            .screen_inner
            .take()
            .map(|inner| *inner)
            .unwrap_or_default();
        inner.wrapper_depth = self.wrapper_depth.saturating_add(1);
        inner.screen_continuation = false;
        events.extend(inner.advance_operations(&data));
        let continuation = inner.has_pending_state();
        self.reset();
        self.screen_continuation = continuation;
        if continuation {
            self.screen_inner = Some(Box::new(inner));
        }
    }

    fn finish_tmux(
        &mut self,
        data: Vec<u8>,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let mut inner = self
            .tmux_inner
            .take()
            .map(|inner| *inner)
            .unwrap_or_default();
        inner.wrapper_depth = self.wrapper_depth.saturating_add(1);
        inner.tmux_continuation = false;
        events.extend(inner.advance_operations(&data));
        let continuation = inner.has_pending_state();
        self.reset();
        self.tmux_continuation = continuation;
        if continuation {
            self.tmux_inner = Some(Box::new(inner));
        }
    }

    fn body_has_st_terminated_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        let mut candidate = data.to_vec();
        candidate.extend_from_slice(b"\x1b\\");
        matches!(self.decode_wrapper(inner, &candidate), Ok(Some(_)))
    }

    fn body_has_c1_terminated_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        let mut candidate = data.to_vec();
        candidate.push(0x9c);
        matches!(self.decode_wrapper(inner, &candidate), Ok(Some(_)))
    }

    fn body_has_st_terminated_graphics_after_boundary(
        &self,
        inner: Option<&GraphicsParser>,
        parser: &ScreenParser,
    ) -> bool {
        self.body_has_terminated_graphics_after_boundary(inner, parser, b"\x1b\\")
    }

    fn body_has_c1_terminated_graphics_after_boundary(
        &self,
        inner: Option<&GraphicsParser>,
        parser: &ScreenParser,
    ) -> bool {
        self.body_has_terminated_graphics_after_boundary(inner, parser, &[0x9c])
    }

    fn body_has_terminated_graphics_after_boundary(
        &self,
        inner: Option<&GraphicsParser>,
        parser: &ScreenParser,
        terminator: &[u8],
    ) -> bool {
        let mut replay = inner.cloned().unwrap_or_default();
        let _ = replay.advance_operations(&parser.data[..parser.inner_data_start]);
        let mut candidate = parser.data[parser.inner_data_start..].to_vec();
        candidate.extend_from_slice(terminator);
        matches!(self.decode_wrapper(Some(&replay), &candidate), Ok(Some(_)))
    }

    fn body_has_complete_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        matches!(self.decode_wrapper(inner, data), Ok(Some(_)))
    }

    fn decode_wrapper(
        &self,
        inner: Option<&GraphicsParser>,
        bytes: &[u8],
    ) -> Result<Option<GraphicsOperation>, GraphicsError> {
        let mut parser = inner.cloned().unwrap_or_default();
        let events = parser.advance_operations(bytes);
        match events.as_slice() {
            [Ok(GraphicsOperation::Failure { error, .. })] => Err(error.clone()),
            [event] => event.clone().map(Some),
            [] => Ok(None),
            _ => Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            }),
        }
    }

    fn feed_discard(
        &mut self,
        mut parser: DiscardParser,
        byte: u8,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.escaped {
            parser.escaped = false;
            if byte == b'\\' {
                if parser.report {
                    self.finish_state(Err(parser.error), events);
                } else {
                    self.finish_state(Ok(None), events);
                }
            } else if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else {
                parser.escaped = byte == 0x1b;
                self.state = GraphicsState::Discard(parser);
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Discard(parser);
        } else if byte == 0x9c || (parser.kind == StringKind::Osc && byte == 0x07) {
            if parser.report {
                self.finish_state(Err(parser.error), events);
            } else {
                self.finish_state(Ok(None), events);
            }
        } else {
            self.state = GraphicsState::Discard(parser);
        }
    }

    fn finish_kitty(
        &mut self,
        parser: KittyParser,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let mut reply = reply_display(parser.header());
        let continuation = parser.header().strip_prefix(b"G").is_some_and(|header| {
            header
                .split(|byte| *byte == b',')
                .all(|field| field.starts_with(b"m=") || field.starts_with(b"q="))
        });
        if continuation {
            if let Some(transfer) = &self.kitty_transfer {
                let quiet = reply.quiet;
                reply = transfer.display().clone();
                if parser.header().windows(2).any(|pair| pair == b"q=") {
                    reply.quiet = quiet;
                }
            } else if let Some(transfer) = &self.kitty_animation_transfer {
                let quiet = reply.quiet;
                reply = transfer.display();
                if parser.header().windows(2).any(|pair| pair == b"q=") {
                    reply.quiet = quiet;
                }
            }
        }
        let first_event = events.len();
        if parser.is_ignored() {
            self.reset();
        } else if self.kitty_animation_transfer.is_some()
            || kitty_animation_transfer_header(parser.header())
        {
            let result = parser
                .finish_animation_chunk()
                .and_then(|chunk| {
                    chunk.ok_or(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    })
                })
                .and_then(|chunk| self.accept_kitty_animation(chunk))
                .map(|command| command.map(GraphicsOperation::Command));
            let failed = result.is_err();
            self.reset();
            match result {
                Ok(Some(command)) => events.push(Ok(command)),
                Ok(None) => {}
                Err(error) => events.push(Err(error)),
            }
            if failed || !self.has_own_transfer() {
                self.kitty_transfer = None;
                self.kitty_animation_transfer = None;
                self.iterm_transfer = None;
                self.transfer_carry.clear();
                self.transfer_carryable = true;
            }
        } else if let Some(command) = parse_command(parser.header(), parser.payload()) {
            if command
                .as_ref()
                .is_ok_and(|command| matches!(command.kind(), KittyCommandKind::Delete(_)))
            {
                self.kitty_transfer = None;
                if self.abandoned_transfer == Some(GraphicsProtocol::Kitty) {
                    self.abandoned_transfer = None;
                }
                self.transfer_carry.clear();
                self.transfer_carryable = true;
            } else if self.kitty_transfer.is_some() || self.abandoned_transfer.is_some() {
                self.finish_state(
                    Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    }),
                    events,
                );
                commands::attach_error_replies(&mut events[first_event..], &reply);
                return;
            }
            self.reset();
            events.push(command.map(GraphicsOperation::Command));
        } else {
            if self.abandoned_transfer == Some(GraphicsProtocol::Kitty) {
                match parser.finish() {
                    Ok(chunk) if chunk.more() => self.reset(),
                    Ok(_) => {
                        self.abandoned_transfer = None;
                        self.finish_state(
                            Err(GraphicsError::TransferTooLarge {
                                protocol: GraphicsProtocol::Kitty,
                            }),
                            events,
                        );
                    }
                    Err(_) => self.reset(),
                }
            } else {
                let result = self.accept_kitty(parser.finish());
                self.finish_state(result, events);
            }
        }
        commands::attach_error_replies(&mut events[first_event..], &reply);
    }

    fn finish_iterm(
        &mut self,
        parser: ItermParser,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        if parser.ignored {
            self.reset();
        } else {
            match self.abandoned_transfer {
                Some(GraphicsProtocol::Iterm2) => {
                    let command = parser
                        .body
                        .split(|byte| *byte == b'=')
                        .next()
                        .unwrap_or(&parser.body);
                    match command {
                        b"FilePart" => self.reset(),
                        b"FileEnd" => {
                            self.abandoned_transfer = None;
                            self.finish_state(
                                Err(GraphicsError::TransferTooLarge {
                                    protocol: GraphicsProtocol::Iterm2,
                                }),
                                events,
                            );
                        }
                        _ => self.reset(),
                    }
                }
                _ => self.finish_iterm_command(parser, events),
            }
        }
    }

    fn finish_iterm_command(
        &mut self,
        parser: ItermParser,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let result = parse_iterm_command(&parser.body, &mut self.iterm_transfer);
        if self.iterm_transfer.is_some() {
            self.remember_transfer_sequence();
        }
        self.finish_state(result, events);
    }

    fn accept_kitty(
        &mut self,
        chunk: Result<KittyChunk, GraphicsError>,
    ) -> Result<Option<DecodedGraphics>, GraphicsError> {
        let chunk = chunk?;
        let outcome = if let Some(transfer) = self.kitty_transfer.take() {
            transfer.accept_chunk(chunk)?
        } else {
            start_transfer(chunk)?
        };
        match outcome {
            KittyTransferOutcome::Pending(transfer) => {
                self.kitty_transfer = Some(transfer);
                self.remember_transfer_sequence();
                Ok(None)
            }
            KittyTransferOutcome::Complete(image) => Ok(Some(image)),
        }
    }

    fn accept_kitty_animation(
        &mut self,
        chunk: KittyAnimationChunk,
    ) -> Result<Option<KittyCommand>, GraphicsError> {
        if self.kitty_transfer.is_some() || self.abandoned_transfer.is_some() {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        let outcome = if let Some(transfer) = self.kitty_animation_transfer.take() {
            transfer.accept_chunk(chunk)?
        } else {
            start_animation_transfer(chunk)?
        };
        match outcome {
            KittyAnimationTransferOutcome::Pending(transfer) => {
                self.kitty_animation_transfer = Some(transfer);
                self.remember_transfer_sequence();
                Ok(None)
            }
            KittyAnimationTransferOutcome::Complete(command) => Ok(Some(*command)),
        }
    }

    fn finish_state(
        &mut self,
        result: Result<Option<DecodedGraphics>, GraphicsError>,
        events: &mut Vec<Result<GraphicsOperation, GraphicsError>>,
    ) {
        let failed = result.is_err();
        self.reset();
        match result {
            Ok(Some(image)) => events.push(Ok(GraphicsOperation::Image(image))),
            Ok(None) => {}
            Err(error) => events.push(Err(error)),
        }
        if failed || !self.has_own_transfer() {
            self.kitty_transfer = None;
            self.kitty_animation_transfer = None;
            self.iterm_transfer = None;
            self.transfer_carry.clear();
            self.transfer_carryable = true;
        }
        if failed {
            self.screen_continuation = false;
            self.screen_inner = None;
            self.tmux_continuation = false;
            self.tmux_inner = None;
        }
    }

    fn discard(&mut self, kind: StringKind, error: GraphicsError, byte: u8) {
        self.discard_with_report(kind, error, byte, true);
    }

    fn discard_with_report(
        &mut self,
        kind: StringKind,
        error: GraphicsError,
        byte: u8,
        report: bool,
    ) {
        if report {
            if kind == StringKind::Apc {
                self.kitty_transfer = None;
                self.kitty_animation_transfer = None;
            } else if kind == StringKind::Osc {
                self.iterm_transfer = None;
            }
            self.transfer_carry.clear();
            self.transfer_carryable = true;
        }
        self.state = GraphicsState::Discard(DiscardParser {
            kind,
            error,
            escaped: byte == 0x1b,
            report,
        });
    }

    fn ignore_string(&mut self, kind: StringKind, byte: u8) {
        self.discard_with_report(
            kind,
            GraphicsError::InvalidCommand {
                protocol: kind.protocol(),
            },
            byte,
            false,
        );
    }

    fn begin(&mut self, state: GraphicsState, byte: u8) {
        self.pending.clear();
        self.pending.push(byte);
        self.carryable = true;
        self.sequence_bytes = 1;
        self.state = state;
    }

    fn begin_transfer(&mut self, state: GraphicsState, byte: u8) {
        self.begin(state, byte);
        self.attach_transfer_carry();
    }

    fn begin_silent_string(&mut self, byte: u8) {
        self.begin(
            GraphicsState::Discard(DiscardParser {
                kind: StringKind::Dcs,
                error: GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                },
                escaped: false,
                report: false,
            }),
            byte,
        );
    }

    fn continue_transfer(&mut self, state: GraphicsState) {
        self.attach_transfer_carry();
        self.state = state;
    }

    fn attach_transfer_carry(&mut self) {
        if !self.has_own_transfer() {
            return;
        }
        if !self.transfer_carryable {
            self.carryable = false;
            return;
        }
        if self.transfer_carry.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.transfer_carry);
        pending.extend_from_slice(&self.pending);
        if pending.len() > MAX_GRAPHICS_CARRY_BYTES {
            pending.clear();
            self.carryable = false;
        } else {
            self.pending = pending;
        }
    }

    fn push_pending(&mut self, byte: u8) {
        if !self.carryable {
            return;
        }
        if self.pending.len() == MAX_GRAPHICS_CARRY_BYTES {
            self.pending.clear();
            self.carryable = false;
        } else {
            self.pending.push(byte);
        }
    }

    fn push_pending_bytes(&mut self, bytes: &[u8]) {
        if !self.carryable {
            return;
        }
        let room = MAX_GRAPHICS_CARRY_BYTES.saturating_sub(self.pending.len());
        if bytes.len() > room {
            self.pending.clear();
            self.carryable = false;
        } else {
            self.pending.extend_from_slice(bytes);
        }
    }

    fn reset(&mut self) {
        self.state = GraphicsState::Ground;
        self.pending.clear();
        self.carryable = true;
        self.sequence_bytes = 0;
    }

    fn cancel_transfers(&mut self) {
        self.kitty_transfer = None;
        self.kitty_animation_transfer = None;
        self.iterm_transfer = None;
        self.transfer_carry.clear();
        self.transfer_carryable = true;
        self.abandoned_transfer = None;
        self.screen_continuation = false;
        self.screen_inner = None;
        self.tmux_continuation = false;
        self.tmux_inner = None;
    }

    fn remember_transfer_sequence(&mut self) {
        if !self.carryable {
            self.transfer_carry.clear();
            self.transfer_carryable = false;
            return;
        }
        if self.pending.len() > MAX_GRAPHICS_CARRY_BYTES {
            self.transfer_carry.clear();
            self.transfer_carryable = false;
            return;
        }
        self.transfer_carry.clear();
        self.transfer_carry.extend_from_slice(&self.pending);
    }
}

fn is_base64_byte(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=')
}

fn kitty_animation_transfer_header(header: &[u8]) -> bool {
    let Some(body) = header.strip_prefix(b"G") else {
        return false;
    };
    let action = body
        .split(|byte| *byte == b',')
        .find_map(|field| field.strip_prefix(b"a="));
    action == Some(b"f")
        && body
            .split(|byte| *byte == b',')
            .any(|field| field == b"m=1")
}

fn base64_run_len(bytes: &[u8], limit: usize) -> usize {
    let bytes = &bytes[..bytes.len().min(limit)];
    bytes
        .iter()
        .position(|byte| !is_base64_byte(*byte))
        .unwrap_or(bytes.len())
}

fn extend_last_range(ranges: &mut Vec<Range<usize>>, offset: usize) {
    if let Some(range) = ranges.last_mut().filter(|range| range.end == offset) {
        range.end += 1;
    } else {
        ranges.push(offset..offset + 1);
    }
}

#[derive(Clone)]
struct ItermParser {
    prefix: Vec<u8>,
    body: Vec<u8>,
    prefix_done: bool,
    ignored: bool,
    escaped: bool,
}

impl ItermParser {
    fn new() -> Self {
        ItermParser {
            prefix: Vec::new(),
            body: Vec::new(),
            prefix_done: false,
            ignored: false,
            escaped: false,
        }
    }

    fn feed(&mut self, byte: u8) -> Result<(), GraphicsError> {
        if self.ignored {
            return Ok(());
        }
        if !self.prefix_done {
            if byte == b';' {
                if self.prefix.as_slice() != b"1337" {
                    self.ignored = true;
                } else {
                    self.prefix_done = true;
                }
                return Ok(());
            }
            if self.prefix.len() == 4
                || !byte.is_ascii_digit()
                || byte != b"1337"[self.prefix.len()]
            {
                self.ignored = true;
                return Ok(());
            }
            self.prefix.push(byte);
            return Ok(());
        }
        push_bounded(
            &mut self.body,
            byte,
            MAX_GRAPHICS_TRANSFER_BYTES,
            GraphicsProtocol::Iterm2,
        )
    }
}

#[derive(Clone)]
struct TmuxParser {
    prefix: Vec<u8>,
    data: Vec<u8>,
    escaped: bool,
    inner_terminated: bool,
}

impl TmuxParser {
    fn new() -> Self {
        Self::from_first(b't')
    }

    fn from_first(byte: u8) -> Self {
        TmuxParser {
            prefix: vec![byte],
            data: Vec::new(),
            escaped: false,
            inner_terminated: false,
        }
    }
}

#[derive(Clone)]
struct ScreenParser {
    data: Vec<u8>,
    escaped: bool,
    inner_terminated: bool,
    inner_data_start: usize,
}

impl ScreenParser {
    fn new() -> Self {
        Self::from_first(0x1b)
    }

    fn from_first(byte: u8) -> Self {
        ScreenParser {
            data: vec![byte],
            escaped: false,
            inner_terminated: false,
            inner_data_start: 0,
        }
    }
}

fn push_bounded(
    target: &mut Vec<u8>,
    byte: u8,
    limit: usize,
    protocol: GraphicsProtocol,
) -> Result<(), GraphicsError> {
    if target.len() == limit {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    target.push(byte);
    Ok(())
}

#[cfg(test)]
mod tests;
