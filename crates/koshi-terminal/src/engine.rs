//! The per-pane terminal engine: a VTE parser — the state machine that decodes
//! raw terminal escape-sequence bytes into actions — paired with the
//! [`TerminalState`] it drives.
//!
//! One [`TerminalEngine`] backs one pane. PTY output arrives in read-sized
//! chunks that can split an escape sequence or a multi-byte UTF-8 code point
//! at any byte; the parser is the state machine that carries such a partial
//! decode from one chunk to the next, so it lives exactly as long as the
//! pane's screen state. Bundling the pair keeps a pane's decoder and screen
//! model one unit under one map entry in the runtime. Each
//! [`advance`](TerminalEngine::advance) call also hands back the reply bytes
//! the chunk's device queries produced, for the caller to write into the PTY.
//!
//! The parser cannot be written to disk, so the engine also keeps the bytes
//! that put another parser where this one stands — see
//! [`undecoded`](TerminalEngine::undecoded). A process-image swap carries those
//! bytes and feeds them to the next image's parser, so a sequence the swap cut
//! in half still completes there. A sequence whose held bytes pass 64 KiB is
//! not carried, so one pane's output cannot grow the engine's memory without a
//! bound.

use koshi_core::process::PtySize;

use crate::scrollback::ScrollbackLimit;
use crate::state::TerminalState;

/// The byte every escape sequence starts with: `ESC`, `0x1b`.
const ESCAPE: u8 = 0x1b;

/// `CAN`, `0x18`: abandons the sequence in progress from any parser state.
const CANCEL: u8 = 0x18;

/// `SUB`, `0x1a`: abandons the sequence in progress from any parser state and
/// prints an error glyph.
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
const MAX_UNDECODED: usize = 64 * 1024;

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
    /// The bytes that put another parser where `parser` stands, as
    /// [`undecoded`](TerminalEngine::undecoded) describes them.
    undecoded: Vec<u8>,
    /// A second parser fed the same bytes as `parser`, driving no screen. It
    /// reports where each sequence ends, so one chunk costs one pass over that
    /// chunk however long the sequence it continues.
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
}

impl TerminalEngine {
    /// An engine for a fresh pane of `size`: an idle parser and a blank
    /// [`TerminalState`].
    pub fn new(size: PtySize) -> Self {
        Self::with_scrollback(size, ScrollbackLimit::default())
    }

    /// Like [`new`](Self::new), but with an explicit scrollback limit so a
    /// caller can honor the user's configured `scrollback` caps.
    pub fn with_scrollback(size: PtySize, limit: ScrollbackLimit) -> Self {
        TerminalEngine {
            parser: vte::Parser::<OSC_CAPACITY>::new_with_size(),
            state: TerminalState::with_scrollback(size, limit),
            undecoded: Vec::new(),
            tail_parser: vte::Parser::<OSC_CAPACITY>::new_with_size(),
            on_boundary: true,
            in_string_body: false,
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
    /// [`undecoded`](Self::undecoded) is set to the bytes that put another
    /// parser where this one now stands.
    #[must_use = "undelivered replies hang the querying app"]
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.state, bytes);
        self.hold_undecoded(bytes);
        self.state.take_replies()
    }

    /// An engine wrapped around an existing `state`, with a parser fed
    /// `undecoded` — the bytes that put a parser where the previous engine's
    /// parser stood, from [`undecoded`](Self::undecoded).
    ///
    /// The replay reaches no screen: every action those bytes dispatch is
    /// dropped, since the previous engine already applied it to `state`. What
    /// the replay leaves behind is the parser's own position, so the rest of a
    /// sequence that was cut in half completes here instead of printing as
    /// text. Pass an empty slice for a state that was not carried out of a
    /// running engine.
    pub fn from_state(state: TerminalState, undecoded: &[u8]) -> Self {
        let mut engine = TerminalEngine {
            parser: vte::Parser::<OSC_CAPACITY>::new_with_size(),
            state,
            undecoded: Vec::new(),
            tail_parser: vte::Parser::<OSC_CAPACITY>::new_with_size(),
            on_boundary: true,
            in_string_body: false,
        };
        engine.parser.advance(&mut NoScreen, undecoded);
        engine.hold_undecoded(undecoded);
        engine
    }

    /// Take the engine apart and hand back its screen model, dropping the
    /// parser. Read [`undecoded`](Self::undecoded) first to carry the parser's
    /// position.
    pub fn into_state(self) -> TerminalState {
        self.state
    }

    /// The bytes that put another parser where this one stands: one escape
    /// sequence that has no final byte yet, the opening of a string whose body
    /// is still arriving, or the first bytes of a UTF-8 code point. Empty when
    /// the parser sits on a sequence boundary.
    ///
    /// A caller carries these across a process-image swap and hands them to
    /// [`from_state`](Self::from_state).
    ///
    /// Example: the chunk ends with `ESC ] 7 ; file://host/Users/yuhan/Proj` →
    /// those bytes, and the pane's reported directory is still whatever the
    /// last finished report set.
    ///
    /// Four string kinds keep only their opening bytes. The parser hands each
    /// body byte straight on or drops it, holding none of it:
    ///
    /// - a device control string — `ESC P q # 0 ; 2 ; 0` → `ESC P q`, so a
    ///   sixel image of any size adds nothing here;
    /// - a start of string, `ESC X` — `ESC X hello` → `ESC X`;
    /// - a privacy message, `ESC ^` — `ESC ^ hello` → `ESC ^`;
    /// - an application program command, `ESC _` — `ESC _ G a=T,f=100;<image>`
    ///   → `ESC _`, so a kitty graphics image of any size adds nothing here.
    ///
    /// An operating system command keeps its whole body; the parser reads the
    /// body and dispatches it at the terminator. Once any sequence
    /// passes 64 KiB the engine stops holding it and reports empty until it
    /// ends: the swap then leaves the next parser on a sequence boundary, and
    /// the rest of the body prints as text.
    pub fn undecoded(&self) -> &[u8] {
        &self.undecoded
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

    /// Move `tail_parser` over `chunk` and update
    /// [`undecoded`](Self::undecoded) from where it stops.
    ///
    /// A parser leaves a sequence boundary only at [`ESCAPE`], so the last
    /// `ESCAPE` in `chunk` opens the last sequence there and everything before
    /// it is decoded: the scan drops what it holds and restarts on a fresh
    /// parser at that byte. A chunk with no `ESCAPE` carries on from where the
    /// previous chunk stopped, so a sequence spread over many chunks is read
    /// once, not once per chunk.
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
        let mut probe = ActionProbe::default();
        while at < chunk.len() {
            probe.boundary = false;
            probe.hooked = false;
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
                    // The parser reports no action for these three openings and
                    // drops every body byte, so the opening alone puts another
                    // parser here.
                    self.undecoded.truncate(STRING_OPENING);
                    self.in_string_body = true;
                } else if self.undecoded.len() > MAX_UNDECODED {
                    // Release the buffer instead of clearing it, so the pane
                    // keeps no room for the longest sequence it ever saw.
                    self.undecoded = Vec::new();
                    self.in_string_body = true;
                } else {
                    self.in_string_body = probe.hooked;
                }
            }
            at = stop;
        }
        if self.on_boundary {
            // The parser holds no sequence, so only the first bytes of a UTF-8
            // code point can be left over.
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

/// A [`vte::Perform`] that records the two things a scan needs: whether an
/// action put the parser back on a sequence boundary, and whether the parser
/// opened a device control string. It touches no screen, so a caller can run a
/// second parser over bytes a real engine has already decoded.
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
    /// boundary and at the start of a device control string body, so the scan
    /// learns which bytes of the chunk are still undecoded.
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
