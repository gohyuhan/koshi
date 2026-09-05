//! The per-pane terminal engine: a VTE parser — the state machine that decodes
//! raw terminal escape-sequence bytes into actions — paired with the
//! [`TerminalState`] it drives.
//!
//! One [`TerminalEngine`] backs one pane. PTY output arrives in read-sized
//! chunks that can split an escape sequence or a multi-byte UTF-8 code point
//! at any byte; the parser carries such a partial decode from one chunk to
//! the next. Each [`advance`](TerminalEngine::advance) call also hands back
//! the reply bytes the chunk's device queries produced, for the caller to
//! write into the PTY.
//!
//! The engine also keeps the bytes that put another parser where this one
//! stands — see [`undecoded`](TerminalEngine::undecoded) and
//! [`graphics_undecoded`](TerminalEngine::graphics_undecoded). A process-image
//! swap carries those bytes to the next image's parsers, and a sequence the
//! swap cut in half completes there. Graphics wrapper nesting and a transfer
//! that cannot be rebuilt within 64 KiB use the complete transport state.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Range;

use koshi_core::process::PtySize;

pub use crate::graphics::GraphicsTransportState;
use crate::graphics::{GraphicsError, GraphicsParser, ImageRecord, MAX_IMAGE_BYTES};
use crate::scrollback::ScrollbackLimit;
use crate::state::{ShellIntegrationFact, TerminalState};

/// The byte every escape sequence starts with: `ESC`, `0x1b`.
const ESCAPE: u8 = 0x1b;

/// `CAN`, `0x18`: abandons the sequence in progress from any parser state.
const CANCEL: u8 = 0x18;

/// `SUB`, `0x1a`: abandons the sequence in progress from any parser state.
const SUBSTITUTE: u8 = 0x1a;

/// The second byte of `ESC X`, which opens a start of string.
const START_OF_STRING: u8 = 0x58;

/// The second byte of `ESC ^`, which opens a privacy message.
const PRIVACY_MESSAGE: u8 = 0x5e;

/// The second byte of `ESC _`, which opens an application program command.
const APPLICATION_COMMAND: u8 = 0x5f;

/// The bytes one of those three openings takes: `ESC` and the byte after it.
const STRING_OPENING: usize = 2;

/// The most bytes of a UTF-8 code point that can be missing at the end of a
/// chunk: a four-byte code point whose last byte has not arrived.
const CODE_POINT_TAIL: usize = 3;

/// The most bytes [`undecoded`](TerminalEngine::undecoded) holds, 64 KiB. The
/// engine stops holding a sequence that passes this size and reports nothing
/// until that sequence ends.
pub(crate) const MAX_UNDECODED: usize = 64 * 1024;

/// The largest number of image events held before the caller drains them.
pub const MAX_GRAPHICS_EVENTS: usize = 64;

/// The largest batch returned by [`TerminalEngine::take_graphics`], including
/// one queue-full report.
pub const MAX_GRAPHICS_EVENT_BATCH: usize = MAX_GRAPHICS_EVENTS + 1;

/// One ordered image event produced by the terminal decoder.
pub type GraphicsEvent = Result<ImageRecord, GraphicsError>;

#[derive(Clone, Copy)]
enum C1StringKind {
    Dcs,
    Osc,
    Dropped,
}

#[derive(Clone, Copy, Default)]
enum C1InputState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    String(C1StringKind),
    StringEscape(C1StringKind),
}

#[derive(Default)]
struct C1InputNormalizer {
    state: C1InputState,
    utf8_continuations: u8,
}

struct NormalizedInput<'a> {
    bytes: Cow<'a, [u8]>,
    raw_to_normalized: Option<Vec<usize>>,
}

struct NormalizedBuffer {
    bytes: Vec<u8>,
    raw_to_normalized: Vec<usize>,
}

impl<'a> NormalizedInput<'a> {
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn end_offset(&self, raw_end: usize) -> usize {
        self.raw_to_normalized
            .as_ref()
            .map_or(raw_end, |offsets| offsets[raw_end])
    }
}

fn without_terminal_inert<'a>(bytes: &'a [u8], ranges: &[Range<usize>]) -> Cow<'a, [u8]> {
    if ranges.is_empty() {
        return Cow::Borrowed(bytes);
    }

    let removed = ranges.iter().map(|range| range.len()).sum::<usize>();
    let mut compacted = Vec::with_capacity(bytes.len() - removed);
    let mut at = 0;
    for range in ranges {
        debug_assert!(at <= range.start && range.start <= range.end && range.end <= bytes.len());
        compacted.extend_from_slice(&bytes[at..range.start]);
        at = range.end;
    }
    compacted.extend_from_slice(&bytes[at..]);
    Cow::Owned(compacted)
}

fn without_terminal_inert_offset(raw_end: usize, ranges: &[Range<usize>]) -> usize {
    let removed = ranges
        .iter()
        .take_while(|range| range.start < raw_end)
        .map(|range| range.end.min(raw_end) - range.start)
        .sum::<usize>();
    raw_end - removed
}

impl C1InputNormalizer {
    fn normalize<'a>(&mut self, bytes: &'a [u8]) -> NormalizedInput<'a> {
        let mut normalized: Option<NormalizedBuffer> = None;
        let mut index = 0;

        while index < bytes.len() {
            let plain = self.plain_run(&bytes[index..]);
            if plain != 0 {
                if let Some(buffer) = normalized.as_mut() {
                    let output_start = buffer.bytes.len();
                    buffer.bytes.extend_from_slice(&bytes[index..index + plain]);
                    buffer
                        .raw_to_normalized
                        .extend(output_start + 1..=output_start + plain);
                }
                index += plain;
                continue;
            }

            let byte = bytes[index];
            let replacement = self.replacement(byte);
            if let Some(buffer) = normalized.as_mut() {
                if let Some(replacement) = replacement {
                    buffer.bytes.extend_from_slice(replacement);
                } else {
                    buffer.bytes.push(byte);
                }
                buffer.raw_to_normalized.push(buffer.bytes.len());
            } else if let Some(replacement) = replacement {
                let capacity = bytes.len().saturating_add(1);
                let mut buffer = NormalizedBuffer {
                    bytes: Vec::with_capacity(capacity),
                    raw_to_normalized: Vec::with_capacity(capacity),
                };
                buffer.bytes.extend_from_slice(&bytes[..index]);
                buffer.raw_to_normalized.extend(0..=index);
                buffer.bytes.extend_from_slice(replacement);
                buffer.raw_to_normalized.push(buffer.bytes.len());
                normalized = Some(buffer);
            }
            index += 1;
        }

        match normalized {
            Some(NormalizedBuffer {
                bytes,
                raw_to_normalized,
            }) => NormalizedInput {
                bytes: Cow::Owned(bytes),
                raw_to_normalized: Some(raw_to_normalized),
            },
            None => NormalizedInput {
                bytes: Cow::Borrowed(bytes),
                raw_to_normalized: None,
            },
        }
    }

    /// Return the leading bytes that cannot change the normalizer state.
    fn plain_run(&self, bytes: &[u8]) -> usize {
        if self.utf8_continuations != 0 {
            return 0;
        }
        let changes_state = |byte: u8| match self.state {
            C1InputState::Ground => byte == ESCAPE || byte >= 0x80,
            C1InputState::String(kind) => {
                matches!(byte, CANCEL | SUBSTITUTE | ESCAPE)
                    || byte >= 0x80
                    || (byte == 0x07 && matches!(kind, C1StringKind::Osc))
            }
            C1InputState::Escape
            | C1InputState::EscapeIntermediate
            | C1InputState::Csi
            | C1InputState::StringEscape(_) => true,
        };
        bytes
            .iter()
            .position(|byte| changes_state(*byte))
            .unwrap_or(bytes.len())
    }

    fn replacement(&mut self, byte: u8) -> Option<&'static [u8]> {
        if matches!(self.state, C1InputState::Ground | C1InputState::String(_)) {
            if self.utf8_continuations != 0 {
                if (byte & 0xc0) == 0x80 {
                    self.utf8_continuations -= 1;
                    return None;
                }
                self.utf8_continuations = 0;
            }
            self.utf8_continuations = match byte {
                0xc2..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf4 => 3,
                _ => 0,
            };
            if self.utf8_continuations != 0 {
                return None;
            }
        } else {
            self.utf8_continuations = 0;
        }

        match byte {
            0x90 if matches!(self.state, C1InputState::Ground) => {
                self.state = C1InputState::String(C1StringKind::Dcs);
                Some(b"\x1bP")
            }
            0x98 | 0x9e if matches!(self.state, C1InputState::Ground) => {
                self.state = C1InputState::String(C1StringKind::Dropped);
                Some(if byte == 0x98 { b"\x1bX" } else { b"\x1b^" })
            }
            0x9d if matches!(self.state, C1InputState::Ground) => {
                self.state = C1InputState::String(C1StringKind::Osc);
                Some(b"\x1b]")
            }
            0x9f if matches!(self.state, C1InputState::Ground) => {
                self.state = C1InputState::String(C1StringKind::Dropped);
                Some(b"\x1b_")
            }
            0x9c if matches!(self.state, C1InputState::String(_)) => {
                self.state = C1InputState::Ground;
                Some(b"\x1b\\")
            }
            _ => {
                self.advance_state(byte);
                None
            }
        }
    }

    fn advance_state(&mut self, byte: u8) {
        self.state = match self.state {
            C1InputState::Ground => match byte {
                ESCAPE => C1InputState::Escape,
                _ => C1InputState::Ground,
            },
            C1InputState::Escape => Self::advance_escape(byte),
            C1InputState::EscapeIntermediate => match byte {
                CANCEL | SUBSTITUTE => C1InputState::Ground,
                ESCAPE => C1InputState::Escape,
                0x20..=0x2f => C1InputState::EscapeIntermediate,
                0x30..=0x7e => C1InputState::Ground,
                _ => C1InputState::EscapeIntermediate,
            },
            C1InputState::Csi => match byte {
                CANCEL | SUBSTITUTE => C1InputState::Ground,
                ESCAPE => C1InputState::Escape,
                0x40..=0x7e => C1InputState::Ground,
                _ => C1InputState::Csi,
            },
            C1InputState::String(kind) => match byte {
                CANCEL | SUBSTITUTE => C1InputState::Ground,
                ESCAPE => C1InputState::StringEscape(kind),
                0x07 if matches!(kind, C1StringKind::Osc) => C1InputState::Ground,
                _ => C1InputState::String(kind),
            },
            C1InputState::StringEscape(kind) => match byte {
                CANCEL | SUBSTITUTE => C1InputState::Ground,
                ESCAPE => C1InputState::StringEscape(kind),
                0x20..=0x2f => C1InputState::EscapeIntermediate,
                0x50 => C1InputState::String(C1StringKind::Dcs),
                0x58 | 0x5e | 0x5f => C1InputState::String(C1StringKind::Dropped),
                0x5b => C1InputState::Csi,
                0x5d => C1InputState::String(C1StringKind::Osc),
                0x30..=0x7e => C1InputState::Ground,
                _ => C1InputState::StringEscape(kind),
            },
        };
    }

    fn advance_escape(byte: u8) -> C1InputState {
        match byte {
            CANCEL | SUBSTITUTE => C1InputState::Ground,
            ESCAPE => C1InputState::Escape,
            0x20..=0x2f => C1InputState::EscapeIntermediate,
            0x50 => C1InputState::String(C1StringKind::Dcs),
            0x58 | 0x5e | 0x5f => C1InputState::String(C1StringKind::Dropped),
            0x5b => C1InputState::Csi,
            0x5d => C1InputState::String(C1StringKind::Osc),
            0x30..=0x7e => C1InputState::Ground,
            _ => C1InputState::Escape,
        }
    }
}

/// The most bytes one OSC sequence accumulates. The parser drops every byte
/// past this and dispatches what it holds when the sequence ends.
pub(crate) const OSC_CAPACITY: usize = 8 * 1024;

/// One pane's emulation engine: the byte decoder and the screen model it
/// feeds.
pub struct TerminalEngine {
    /// The VTE state machine. Holds any partial escape sequence or split
    /// UTF-8 code point between [`advance`](TerminalEngine::advance) calls.
    parser: vte::Parser<OSC_CAPACITY>,
    /// The screen model the parser's decoded actions mutate.
    state: TerminalState,
    /// The canonical bytes that put another parser where `parser` stands, as
    /// [`undecoded`](TerminalEngine::undecoded) describes them. Eight-bit
    /// string controls use their seven-bit `ESC` forms so the VTE parser can
    /// replay them.
    undecoded: Vec<u8>,
    /// The raw bytes that put the graphics parser where it stands.
    graphics_undecoded: Vec<u8>,
    /// A second parser fed the same bytes as `parser`, driving no screen. It
    /// reports where each sequence ends. One chunk costs one pass over that
    /// chunk, however long the sequence it continues.
    tail_parser: vte::Parser<OSC_CAPACITY>,
    /// Set while `tail_parser` sits on a sequence boundary, where `undecoded`
    /// holds at most the first bytes of a UTF-8 code point.
    on_boundary: bool,
    /// Set while `tail_parser` sits in the body of a string whose bytes
    /// `undecoded` does not hold: a device control string, a start of string, a
    /// privacy message, an application program command, or any sequence that
    /// passed [`MAX_UNDECODED`]. `undecoded` holds the opening bytes of the
    /// first four kinds and nothing of the fifth.
    in_string_body: bool,
    /// The raw terminal-image parser that observes the same bytes as the VTE
    /// parser without changing terminal state.
    graphics_parser: GraphicsParser,
    /// Complete image records and recoverable image errors waiting for the
    /// terminal caller.
    graphics_events: VecDeque<GraphicsEvent>,
    /// Converts 8-bit string controls to the 7-bit forms supported by `vte`.
    terminal_input: C1InputNormalizer,
    /// RGBA bytes held by successful image events.
    graphics_event_bytes: usize,
    /// Number of events dropped after the bounded graphics queue filled.
    graphics_events_dropped: usize,
    /// Set when the next DCS is a GNU Screen continuation wrapper.
    graphics_screen_continuation: bool,
    /// Set when the carried bytes belong to an open GNU Screen wrapper.
    graphics_screen_wrapper_active: bool,
    /// Set when the next DCS is a tmux continuation wrapper.
    graphics_tmux_continuation: bool,
    /// Set when the carried bytes belong to an open tmux wrapper.
    graphics_tmux_wrapper_active: bool,
}

impl TerminalEngine {
    /// An engine for a fresh pane of `size`: an idle parser and a blank
    /// [`TerminalState`].
    pub fn new(size: PtySize) -> Self {
        Self::with_scrollback(size, ScrollbackLimit::default())
    }

    /// Like [`new`](Self::new), with `limit` as the scrollback limit.
    pub fn with_scrollback(size: PtySize, limit: ScrollbackLimit) -> Self {
        Self::with_idle_parsers(TerminalState::with_scrollback(size, limit))
    }

    /// An engine around `state` with both parsers idle and nothing held.
    fn with_idle_parsers(state: TerminalState) -> Self {
        TerminalEngine {
            parser: vte::Parser::<OSC_CAPACITY>::new_with_size(),
            state,
            undecoded: Vec::new(),
            graphics_undecoded: Vec::new(),
            tail_parser: vte::Parser::<OSC_CAPACITY>::new_with_size(),
            on_boundary: true,
            in_string_body: false,
            graphics_parser: GraphicsParser::default(),
            graphics_events: VecDeque::new(),
            terminal_input: C1InputNormalizer::default(),
            graphics_event_bytes: 0,
            graphics_events_dropped: 0,
            graphics_screen_continuation: false,
            graphics_screen_wrapper_active: false,
            graphics_tmux_continuation: false,
            graphics_tmux_wrapper_active: false,
        }
    }

    /// Feed one chunk of PTY output through the parser into the state, and
    /// return the reply bytes any device queries in the chunk produced —
    /// answers to DA (Device Attributes), DSR (Device Status Report), and
    /// DECRQM (Request Mode) queries the app sent; empty when the chunk held
    /// no query. The caller writes the replies back into the pane's PTY.
    ///
    /// Chunks may split an escape sequence or a UTF-8 code point at any byte;
    /// the parser resumes the partial decode on the next call, and
    /// [`undecoded`](Self::undecoded) is set to the canonical bytes that put
    /// another parser where this one now stands. This method drains shell-integration
    /// facts without returning them; use
    /// [`Self::advance_with_shell_integration`] when the caller handles those facts.
    #[must_use = "undelivered replies hang the querying app"]
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        let (replies, _) = self.advance_with_shell_integration(bytes);
        replies
    }

    /// Feed one chunk through the parser and return device replies plus the
    /// shell-integration facts that the chunk produced. A `C` marker when the
    /// shell is not already running a command returns
    /// [`ShellIntegrationFact::CommandStarted`], and a matched `D` marker
    /// returns [`ShellIntegrationFact::CommandFinished`] with its exit code.
    /// The facts contain no command text. `ESC ] 133 ; C` followed by
    /// `ESC ] 133 ; D ; 137` returns both facts in that order.
    #[must_use = "undelivered replies or shell facts are lost"]
    pub fn advance_with_shell_integration(
        &mut self,
        bytes: &[u8],
    ) -> (Vec<u8>, Vec<ShellIntegrationFact>) {
        let graphics = self.graphics_parser.advance_with_offsets(bytes);
        let terminal_input = without_terminal_inert(bytes, &graphics.terminal_inert);
        let normalized = self.terminal_input.normalize(&terminal_input);
        let terminal_bytes = normalized.bytes();
        let mut parser_at = 0;
        for (offset, result) in graphics.events {
            let raw_end = without_terminal_inert_offset(offset + 1, &graphics.terminal_inert);
            let end = normalized.end_offset(raw_end);
            if end > parser_at {
                self.parser
                    .advance(&mut self.state, &terminal_bytes[parser_at..end]);
                parser_at = end;
            }
            let anchor = self.state.active_cursor_position();
            self.queue_graphics(result, anchor);
        }
        if parser_at < terminal_bytes.len() {
            self.parser
                .advance(&mut self.state, &terminal_bytes[parser_at..]);
        }
        self.hold_undecoded(terminal_bytes);
        self.sync_graphics_undecoded();
        (
            self.state.take_replies(),
            self.state.take_shell_integration_facts(),
        )
    }

    /// An engine wrapped around an existing `state`, with a parser fed
    /// `undecoded` — the bytes that put a parser where the previous engine's
    /// parser stood, from [`undecoded`](Self::undecoded).
    ///
    /// The replay reaches no screen: every action those bytes dispatch is
    /// dropped, and `state` stays as passed. The replay leaves the parser at
    /// the previous parser's position, and the rest of a sequence that was cut
    /// in half completes here. Pass an empty slice for a state that was not
    /// carried out of a running engine.
    pub fn from_state(state: TerminalState, undecoded: &[u8]) -> Self {
        Self::from_state_with_graphics(state, undecoded, &[])
    }

    /// An engine around `state` with the VTE and graphics parser positions
    /// carried from another engine.
    pub fn from_state_with_graphics(
        state: TerminalState,
        undecoded: &[u8],
        graphics_undecoded: &[u8],
    ) -> Self {
        Self::from_state_with_graphics_and_events(state, undecoded, graphics_undecoded, &[])
    }

    /// An engine around `state` with parser positions and queued graphics
    /// events carried from another engine.
    pub fn from_state_with_graphics_and_events(
        state: TerminalState,
        undecoded: &[u8],
        graphics_undecoded: &[u8],
        graphics_events: &[GraphicsEvent],
    ) -> Self {
        Self::from_state_with_graphics_and_events_and_screen(
            state,
            undecoded,
            graphics_undecoded,
            graphics_events,
            false,
            false,
        )
    }

    /// An engine around state with parser positions, queued graphics events,
    /// and GNU Screen continuation state carried from another engine. This is
    /// the compatibility form for the two legacy wrapper flags; use
    /// [`from_state_with_graphics_and_events_and_wrappers`](Self::from_state_with_graphics_and_events_and_wrappers)
    /// when nested parser state or bounded-transfer abandonment is present.
    pub fn from_state_with_graphics_and_events_and_screen(
        state: TerminalState,
        undecoded: &[u8],
        graphics_undecoded: &[u8],
        graphics_events: &[GraphicsEvent],
        graphics_screen_continuation: bool,
        graphics_screen_wrapper_active: bool,
    ) -> Self {
        Self::from_state_with_graphics_and_events_and_wrappers(
            state,
            undecoded,
            graphics_undecoded,
            graphics_events,
            GraphicsTransportState {
                screen_continuation: graphics_screen_continuation,
                screen_wrapper_active: graphics_screen_wrapper_active,
                ..GraphicsTransportState::default()
            },
        )
    }

    /// An engine around state with parser positions, queued graphics events,
    /// and the complete graphics transport state carried from another engine.
    /// A split Screen wrapper such as `ESC P ESC ] 1337;File=... ESC \` is
    /// restored from its nested parser record before the next PTY bytes arrive.
    pub fn from_state_with_graphics_and_events_and_wrappers(
        state: TerminalState,
        undecoded: &[u8],
        graphics_undecoded: &[u8],
        graphics_events: &[GraphicsEvent],
        graphics_transport: GraphicsTransportState,
    ) -> Self {
        let mut engine = Self::with_idle_parsers(state);
        engine
            .graphics_parser
            .restore_carry(graphics_undecoded, graphics_transport);
        let normalized = engine.terminal_input.normalize(undecoded);
        engine.parser.advance(&mut NoScreen, normalized.bytes());
        engine.hold_undecoded(normalized.bytes());
        engine.sync_graphics_undecoded();
        for event in graphics_events {
            if let Err(GraphicsError::QueueFull { dropped }) = event {
                engine.graphics_events_dropped =
                    engine.graphics_events_dropped.saturating_add(*dropped);
            } else {
                engine.queue_graphics_event(event.clone());
            }
        }
        engine
    }

    /// Take the engine apart and hand back its screen model, dropping the
    /// parser. Read [`undecoded`](Self::undecoded),
    /// [`graphics_undecoded`](Self::graphics_undecoded), and
    /// [`take_graphics`](Self::take_graphics) first to carry parser positions
    /// and queued image events.
    pub fn into_state(self) -> TerminalState {
        self.state
    }

    /// Drain complete image records and recoverable image errors in the order
    /// their protocol terminators reached the terminal parser. When the queue
    /// dropped records, one `QueueFull` report follows the held events.
    ///
    /// A display record is applied to image state before it is made available
    /// to the caller; a malformed or unplaceable record returns a typed error.
    pub fn take_graphics(&mut self) -> Vec<GraphicsEvent> {
        let mut events: Vec<GraphicsEvent> = self.graphics_events.drain(..).collect();
        self.graphics_event_bytes = 0;
        if self.graphics_events_dropped != 0 {
            events.push(Err(GraphicsError::QueueFull {
                dropped: self.graphics_events_dropped,
            }));
            self.graphics_events_dropped = 0;
        }
        events
    }

    /// Finish the graphics stream, report any incomplete transfer, and drain
    /// all image events already queued by the engine.
    ///
    /// A stream ending after `ESC _ Gf=32,s=1,v=1;` returns one typed
    /// `Truncated` error and leaves the terminal cells unchanged.
    pub fn finish(&mut self) -> Vec<GraphicsEvent> {
        let anchor = self.state.active_cursor_position();
        for result in self.graphics_parser.finish() {
            self.queue_graphics(result, anchor);
        }
        self.graphics_undecoded.clear();
        self.graphics_screen_continuation = false;
        self.graphics_screen_wrapper_active = false;
        self.graphics_tmux_continuation = false;
        self.graphics_tmux_wrapper_active = false;
        self.take_graphics()
    }

    /// The canonical bytes that put another parser where this one stands: one escape
    /// sequence that has no final byte yet, the opening of a string whose body
    /// is still arriving, or the first bytes of a UTF-8 code point. Empty when
    /// the parser sits on a sequence boundary, with one exception: a control
    /// sequence the parser ignores (`ESC [ 3 ? m`) dispatches nothing, and
    /// the scan holds it, and every C0 or C1 control byte after it, until the
    /// next escape byte, printed character, `CAN`, or `SUB`. Replaying it
    /// leaves the parser on a sequence boundary.
    ///
    /// A caller carries these across a process-image swap and hands them to
    /// [`from_state`](Self::from_state). Eight-bit string controls are stored
    /// in their seven-bit `ESC` forms.
    ///
    /// Example: the chunk ends with `ESC ] 7 ; file://host/Users/yuhan/Proj` →
    /// those bytes, and the pane's reported directory is still whatever the
    /// last finished report set.
    ///
    /// Four string kinds keep only their opening bytes. The parser hands each
    /// body byte straight on or drops it, holding none of it:
    ///
    /// - a device control string — `ESC P q # 0 ; 2 ; 0` → `ESC P q`; a sixel
    ///   image of any size adds nothing here;
    /// - a start of string, `ESC X` — `ESC X hello` → `ESC X`;
    /// - a privacy message, `ESC ^` — `ESC ^ hello` → `ESC ^`;
    /// - an application program command, `ESC _` — `ESC _ G a=T,f=100;<image>`
    ///   → `ESC _`; a kitty graphics image of any size adds nothing here.
    ///
    /// An operating system command keeps its whole body; the parser reads the
    /// body and dispatches it at the terminator. Once any sequence passes
    /// 64 KiB the engine stops holding it and reports empty until it ends. A
    /// swap in that window leaves the next parser on a sequence boundary, and
    /// the rest of the body prints as text.
    pub fn undecoded(&self) -> &[u8] {
        &self.undecoded
    }

    /// The raw bytes that put the graphics parser where it stands.
    ///
    /// A caller carrying a simple process-image swap passes these bytes to
    /// [`from_state_with_graphics`](Self::from_state_with_graphics). A swap
    /// that cuts a tmux or GNU Screen wrapper, or abandons a large transfer,
    /// uses [`graphics_transport_state`](Self::graphics_transport_state).
    /// Ordinary VTE parser bytes are returned by [`undecoded`](Self::undecoded).
    pub fn graphics_undecoded(&self) -> &[u8] {
        &self.graphics_undecoded
    }

    /// The complete graphics-parser state needed by a process-image swap.
    ///
    /// Example: a split Screen wrapper returns a state with `screen_inner`;
    /// `graphics_undecoded` contains the raw bytes without wrapper state.
    pub fn graphics_transport_state(&self) -> Option<GraphicsTransportState> {
        self.graphics_parser.transport_state()
    }

    /// Whether the next DCS belongs to an unfinished GNU Screen wrapper.
    pub fn graphics_screen_continuation(&self) -> bool {
        self.graphics_screen_continuation
    }

    /// Whether a carried GNU Screen wrapper is still open.
    pub fn graphics_screen_wrapper_active(&self) -> bool {
        self.graphics_screen_wrapper_active
    }

    /// Whether the next DCS belongs to an unfinished tmux continuation.
    pub fn graphics_tmux_continuation(&self) -> bool {
        self.graphics_tmux_continuation
    }

    /// Whether a carried tmux wrapper is still open.
    pub fn graphics_tmux_wrapper_active(&self) -> bool {
        self.graphics_tmux_wrapper_active
    }

    /// The screen model, for reads (rendering, cursor and mode queries).
    pub fn state(&self) -> &TerminalState {
        &self.state
    }

    /// Resize the screen model to `size` (see [`TerminalState::resize`]).
    ///
    /// The parser keeps any partial decode: a sequence split across the
    /// resize still completes.
    pub fn resize(&mut self, size: PtySize) {
        self.state.resize(size);
    }

    fn queue_graphics(
        &mut self,
        result: Result<crate::graphics::DecodedGraphics, GraphicsError>,
        anchor: (u16, u16),
    ) {
        match result {
            Ok(decoded) => {
                let record = ImageRecord {
                    protocol: decoded.protocol,
                    image: decoded.image,
                    action: decoded.action,
                    display: decoded.display,
                    anchor,
                };
                let bytes = record.image.rgba.len();
                let protocol = record.protocol;
                let event = self
                    .state
                    .apply_image_record(&record)
                    .map(|()| record)
                    .map_err(|reason| GraphicsError::PlacementRejected { protocol, reason });
                let queued_bytes = if event.is_ok() { bytes } else { 0 };
                self.queue_graphics_event_with_bytes(event, queued_bytes);
            }
            Err(error) => self.queue_graphics_event(Err(error)),
        }
    }

    fn queue_graphics_event(&mut self, event: GraphicsEvent) {
        let bytes = match &event {
            Ok(record) => record.image.rgba.len(),
            Err(_) => 0,
        };
        self.queue_graphics_event_with_bytes(event, bytes);
    }

    fn queue_graphics_event_with_bytes(&mut self, event: GraphicsEvent, bytes: usize) {
        if self.graphics_events.len() == MAX_GRAPHICS_EVENTS
            || self
                .graphics_event_bytes
                .checked_add(bytes)
                .is_none_or(|total| total > MAX_IMAGE_BYTES)
        {
            self.graphics_events_dropped = self.graphics_events_dropped.saturating_add(1);
            return;
        }
        self.graphics_event_bytes += bytes;
        self.graphics_events.push_back(event);
    }

    fn sync_graphics_undecoded(&mut self) {
        self.graphics_undecoded.clear();
        if let Some(carry) = self.graphics_parser.carry_bytes() {
            self.graphics_undecoded.extend_from_slice(carry);
        }
        self.graphics_screen_continuation = self.graphics_parser.screen_continuation();
        self.graphics_screen_wrapper_active = self.graphics_parser.screen_wrapper_active();
        self.graphics_tmux_continuation = self.graphics_parser.tmux_continuation();
        self.graphics_tmux_wrapper_active = self.graphics_parser.tmux_wrapper_active();
    }

    /// Move `tail_parser` over `chunk` and update
    /// [`undecoded`](Self::undecoded) from where it stops.
    ///
    /// A parser leaves a sequence boundary only at [`ESCAPE`]. The scan drops
    /// what it holds and restarts on a fresh parser at the last `ESCAPE` in
    /// `chunk`; the last sequence opens there and everything before it is
    /// decoded. A chunk with no `ESCAPE` carries on from where the previous
    /// chunk stopped. A sequence spread over many chunks is read once.
    ///
    /// The scan holds at most [`MAX_UNDECODED`] bytes of one sequence. Past
    /// that it releases the buffer and holds nothing more until the sequence
    /// ends.
    fn hold_undecoded(&mut self, chunk: &[u8]) {
        let mut at = 0;
        if let Some(start) = chunk.iter().rposition(|byte| *byte == ESCAPE) {
            self.tail_parser = vte::Parser::<OSC_CAPACITY>::new_with_size();
            self.undecoded.clear();
            self.on_boundary = false;
            self.in_string_body = false;
            at = start;
        }
        // Each round runs to the action that ends a sequence or opens the body
        // of a device control string, or to the end of the chunk.
        while at < chunk.len() {
            let mut probe = ActionProbe::default();
            let read = self
                .tail_parser
                .advance_until_terminated(&mut probe, &chunk[at..]);
            let stop = at + read;
            if probe.boundary {
                self.undecoded.clear();
                self.on_boundary = true;
                self.in_string_body = false;
            } else if !self.on_boundary && !self.in_string_body {
                self.undecoded.extend_from_slice(&chunk[at..stop]);
                if opens_dropped_string(&self.undecoded) {
                    // The opening alone puts another parser inside the body.
                    self.undecoded.truncate(STRING_OPENING);
                    self.in_string_body = true;
                } else if self.undecoded.len() > MAX_UNDECODED {
                    // A fresh `Vec` frees the buffer's capacity.
                    self.undecoded = Vec::new();
                    self.in_string_body = true;
                } else {
                    self.in_string_body = probe.hooked;
                }
            }
            at = stop;
        }
        if self.on_boundary {
            // Only the first bytes of a UTF-8 code point can be left over.
            let tail = chunk.len().saturating_sub(CODE_POINT_TAIL);
            self.undecoded.extend_from_slice(&chunk[tail..]);
            let kept = split_code_point(&self.undecoded).len();
            let decoded = self.undecoded.len() - kept;
            self.undecoded.drain(..decoded);
        }
    }
}

/// A [`vte::Perform`] that drops every action. Replaying carried bytes through
/// it moves a parser without touching a screen.
struct NoScreen;

impl vte::Perform for NoScreen {}

/// A [`vte::Perform`] that records two things: whether an action put the
/// parser back on a sequence boundary, and whether the parser opened a device
/// control string. It touches no screen.
///
/// Every action listed here leaves the parser on a sequence boundary, as long
/// as the bytes scanned hold no [`ESCAPE`] past their first byte: `ESCAPE`
/// alone ends an operating system command or a device control string into the
/// next sequence instead of into the ground state.
#[derive(Default)]
struct ActionProbe {
    /// Set when an action put the parser back on a sequence boundary.
    boundary: bool,
    /// Set when the parser opened a device control string.
    hooked: bool,
}

impl vte::Perform for ActionProbe {
    fn print(&mut self, _c: char) {
        self.boundary = true;
    }

    fn execute(&mut self, byte: u8) {
        if byte == CANCEL || byte == SUBSTITUTE {
            self.boundary = true;
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.hooked = true;
    }

    fn unhook(&mut self) {
        self.boundary = true;
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.boundary = true;
    }

    fn csi_dispatch(
        &mut self,
        _params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
        self.boundary = true;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.boundary = true;
    }

    /// Stops [`vte::Parser::advance_until_terminated`] at each sequence
    /// boundary and at the start of a device control string body.
    fn terminated(&self) -> bool {
        self.boundary || self.hooked
    }
}

/// True when `bytes` opens a start of string, a privacy message or an
/// application program command: `ESC X`, `ESC ^` or `ESC _`. The parser reads
/// the body of each one and keeps none of it.
fn opens_dropped_string(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        [
            ESCAPE,
            START_OF_STRING | PRIVACY_MESSAGE | APPLICATION_COMMAND,
            ..
        ]
    )
}

/// The bytes at the end of `bytes` that begin a UTF-8 code point without
/// completing it, or empty when the last code point is whole.
fn split_code_point(bytes: &[u8]) -> &[u8] {
    let mut tail = &bytes[bytes.len().saturating_sub(CODE_POINT_TAIL)..];
    loop {
        let Err(error) = std::str::from_utf8(tail) else {
            return &[];
        };
        let Some(invalid) = error.error_len() else {
            return &tail[error.valid_up_to()..];
        };
        tail = &tail[error.valid_up_to() + invalid..];
    }
}

#[cfg(test)]
mod tests;
