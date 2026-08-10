//! Wire messages for the control socket.
//!
//! An exchange is one [`IpcRequest`] and the [`IpcResponse`] answering it. The
//! response repeats the request's `request_id` so a caller can match the two,
//! and names no request at all when the bytes it received could not be read.
//!
//! Every connection opens with [`IpcRequestKind::Hello`]. It settles the two
//! facts that hold for the whole connection — the protocol version both sides
//! speak, and the [`ConnectionToken`] the caller presents — so no later
//! request repeats them.
//!
//! This module is the vocabulary only: framing and sockets belong to the
//! transport layer, and the Hello checks to
//! [`handshake`](crate::handshake).

use std::fmt;

use koshi_core::command::{Command, CommandEnvelope, CommandResult};
use koshi_core::discovery::SessionOverview;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::key::KeyChord;
use koshi_core::mouse::MouseInput;
use koshi_core::redact::REDACTED;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::attach::AttachedSessionStructureSnapshot;
use crate::layout::SessionLayout;
use crate::wire::{MaybeKnown, WireName, WireVariants};

/// The highest protocol version this build speaks, and the one it uses when
/// the peer speaks it too.
///
/// Bumps when an existing wire field changes its type or its meaning, in the
/// same commit as that change: the first such change after a release sets
/// this to the released value plus one, and the value then holds until the
/// next release. A field added or removed with both sides still decoding
/// cleanly does not bump it.
///
/// Versions 3 and 4 were bumped for pure additions, under an older reading
/// that bumped on every change. The rule above resets the number to the last
/// released value — v0.1.0 is the only released build and speaks 1 — so 0.2.0
/// speaks 2.
pub const PROTOCOL_VERSION: u32 = 2;

/// The lowest protocol version this build speaks. A peer whose highest is
/// below this one is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
///
/// The floor is 2, the version 0.2.0 speaks. Version 1 is v0.1.0, which has
/// no attach and puts nothing user-visible on the socket, so no version-1
/// peer has anything to ask a session server for.
///
/// Raising this floor drops support for every build below it, so it moves
/// only on a stated decision to end that support.
pub const MIN_PROTOCOL_VERSION: u32 = 2;

/// The version two peers use, given the range each speaks: the highest both
/// have. `None` when the ranges do not overlap.
///
/// Example — a caller speaking 2 to 4 and a build speaking 2 to 2 settle on
/// 2; a caller speaking 5 to 6 and the same build settle on nothing.
#[must_use]
pub fn agreed_version(
    caller_min: u32,
    caller_max: u32,
    build_min: u32,
    build_max: u32,
) -> Option<u32> {
    let highest = caller_max.min(build_max);
    let lowest = caller_min.max(build_min);
    (lowest <= highest).then_some(highest)
}

/// The secret a connection presents to prove it belongs to the user who
/// started this Koshi.
///
/// Each running Koshi generates one and writes it to its
/// [endpoint file](crate::endpoint::EndpointFile) in the private runtime
/// directory, so being able to read the value is itself the proof.
///
/// Two ways out of this type, and only two:
///
/// - `Serialize` and [`expose`](Self::expose) write the **real secret**, for
///   the endpoint file and the socket. `serde_json::to_string(&hello)` yields
///   `{"protocol_version":2, "token":"k7Qx…"}`, secret included.
/// - `Debug` and `Display` write `***`, so a token that reaches a log line, a
///   trace, or an error dump reveals nothing.
///
/// Anything describing a request in a log takes the second form, or
/// [`IpcRequestKind::name`], which carries no payload at all.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionToken(String);

impl ConnectionToken {
    /// Wrap an already-generated secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        ConnectionToken(secret.into())
    }

    /// Generate a fresh secret: 32 bytes from the operating system's
    /// cryptographic random source, written as 64 lowercase hex characters.
    /// Every generated token has this one length.
    #[must_use]
    pub fn generate() -> Self {
        const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .expect("every supported platform provides the system random source");
        let mut secret = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            secret.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            secret.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
        }
        ConnectionToken(secret)
    }

    /// The secret itself, for writing it to the endpoint file.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl PartialEq for ConnectionToken {
    /// Two secrets of the same length are compared byte by byte to the end,
    /// never stopping at the first mismatch, so how long the answer takes does
    /// not reveal how many leading bytes a caller guessed right. `subtle`
    /// holds that property through optimization by reading each byte's verdict
    /// back through a volatile load, which the compiler may not fold away.
    ///
    /// Secrets of different lengths are refused at once: Koshi generates every
    /// token at one length, so a token's length is not a secret.
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes().ct_eq(other.0.as_bytes()).into()
    }
}

impl Eq for ConnectionToken {}

impl fmt::Debug for ConnectionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ConnectionToken({REDACTED})")
    }
}

impl fmt::Display for ConnectionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// One message from a caller to a running Koshi.
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know, so a misspelled `request_id` is an error.
///
/// `K` is the request kind. A sender uses `IpcRequest`, where `K` is
/// [`IpcRequestKind`]. A server uses [`IncomingRequest`], where a kind this
/// build does not have arrives as [`MaybeKnown::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcRequest<K = IpcRequestKind> {
    /// Caller-chosen id, repeated in the response that answers this request.
    /// Unique among the requests in flight on one connection.
    pub request_id: u64,
    /// What is being asked.
    pub kind: K,
}

/// A request as a server reads it: the kind may name something this build does
/// not have.
pub type IncomingRequest = IpcRequest<MaybeKnown<IpcRequestKind>>;

/// What a request asks for.
///
/// On a connection already serving an attached client's event stream,
/// [`KeyPress`](Self::KeyPress), [`Resize`](Self::Resize),
/// [`Paste`](Self::Paste) and [`SubmitCommand`](Self::SubmitCommand) are
/// answered by the next painted frame rather than by an [`IpcResponse`].
///
/// [`Mouse`](Self::Mouse) is answered by exactly one
/// [`SessionEvent::MouseAnswer`](crate::event::SessionEvent::MouseAnswer)
/// carrying that request's `request_id`, always — including when the round
/// produced nothing to report, where the answer's list is empty. That answer
/// is what moves the viewer's drag anchor over the cells the session took.
/// A field this build does not know is ignored, so a peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequestKind {
    /// Opens the connection: names the protocol versions the caller speaks and
    /// presents the token. Sent before any other kind.
    ///
    /// The two versions are a range, lowest and highest. The server answers
    /// with the one both sides use, or refuses when the ranges do not overlap.
    ///
    /// Sending it again on an open connection is allowed and changes nothing:
    /// the versions and token are checked again and the same answer comes
    /// back, since checking them alters no state.
    Hello {
        /// The lowest protocol version the caller speaks.
        min_protocol_version: u32,
        /// The highest protocol version the caller speaks.
        max_protocol_version: u32,
        /// The secret read from the endpoint file.
        token: ConnectionToken,
    },
    /// Join the session as a viewing client: the server mints the client,
    /// registers it for the events `filter` selects, and answers with
    /// [`IpcResult::Attached`].
    ///
    /// The caller names no identity of its own. Who the client is, what it may
    /// do, and what it is called are all decided by the server.
    Attach {
        /// The caller's terminal size in cells, which the server records as
        /// the client's viewport.
        viewport: Size,
        /// Which of the session's events the client receives.
        filter: EventFilterSpec,
    },
    /// One key press the attached client's keymap did not bind, for the pane
    /// it is typing into.
    KeyPress {
        /// The chord the client read from its terminal.
        chord: KeyChord,
    },
    /// The attached client's terminal changed size.
    Resize {
        /// The client's new terminal size in cells.
        viewport: Size,
    },
    /// Text the attached client's outer terminal pasted, for the pane it is
    /// typing into. Carried whole, so no character of it can fire a
    /// keybinding.
    Paste {
        /// The pasted text, exactly as the client's terminal delivered it.
        text: String,
    },
    /// One round of mouse actions the attached client decided, in the order
    /// the session must run them. A round is what the viewer accumulated for
    /// one host mouse event, so it carries one `request_id` and receives one
    /// answer.
    Mouse(Vec<WireMouseAction>),
    /// Dispatch a command against the session.
    SubmitCommand(Box<CommandEnvelope>),
    /// Ask the session to describe itself in full. The caller narrows the
    /// answer to the query it was asked.
    Discovery,
    /// Ask the session to describe its layout: each tab's split tree, and the
    /// rectangles each viewing client solves it to.
    Layout {
        /// The one tab to describe, or every tab when absent.
        tab: Option<TabId>,
    },
}

impl IpcRequestKind {
    /// The Hello this build opens a connection with: the versions it speaks,
    /// lowest first, and the `token` the caller presents.
    ///
    /// Every caller builds its Hello here, so the two version fields are
    /// filled in one place.
    #[must_use]
    pub fn hello(token: ConnectionToken) -> IpcRequestKind {
        IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token,
        }
    }

    /// The kind's name, e.g. `"SubmitCommand"`. Carries no payload, so it is
    /// safe on a log line even though a payload can hold the connection token
    /// or text the user typed.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            IpcRequestKind::Hello { .. } => "Hello",
            IpcRequestKind::Attach { .. } => "Attach",
            IpcRequestKind::KeyPress { .. } => "KeyPress",
            IpcRequestKind::Resize { .. } => "Resize",
            IpcRequestKind::Paste { .. } => "Paste",
            IpcRequestKind::Mouse(_) => "Mouse",
            IpcRequestKind::SubmitCommand(_) => "SubmitCommand",
            IpcRequestKind::Discovery => "Discovery",
            IpcRequestKind::Layout { .. } => "Layout",
        }
    }
}

/// One thing an attached client asks the session to do for a mouse event.
///
/// The wire spelling of the viewer's own `MouseAction`, variant for variant.
/// Every variant names its target explicitly, so the session hit-tests
/// nothing.
///
/// [`Command`](Self::Command) is here rather than on
/// [`IpcRequestKind::SubmitCommand`] so a command the mouse issued arrives
/// inside its round, in its place among the other actions.
/// A field this build does not know is ignored, so a peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireMouseAction {
    /// Move this client's scrollback view of `pane` by `lines`, up into
    /// history or back down toward live output.
    Scroll {
        /// The pane whose view moves.
        pane: PaneId,
        /// Up into history, or down toward live output.
        up: bool,
        /// Lines to move.
        lines: usize,
    },
    /// Hand the event to the program in `pane` as a mouse report. The session
    /// encodes it from that pane's live tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane: PaneId,
        /// The event, with the cell it landed on and the modifiers held.
        mouse: MouseInput,
    },
    /// Send `count` cursor arrow keys to `pane` — the alternate-scroll
    /// (`?1007`) translation of a wheel tick on the alternate screen.
    AltScrollArrows {
        /// The pane whose program receives the arrows.
        pane: PaneId,
        /// Up-arrows, or down-arrows.
        up: bool,
        /// How many.
        count: usize,
    },
    /// Move `pane`'s `side` border `count` cells, one cell per step, in the
    /// direction `step` names.
    Resize {
        /// The pane whose border moves.
        pane: PaneId,
        /// Which of the pane's borders was grabbed.
        side: Direction,
        /// `1` grows the pane, `-1` shrinks it.
        step: i16,
        /// How many single-cell steps the pointer travelled.
        count: u16,
    },
    /// Run the command through the session's command door, attributed to this
    /// client's mouse.
    Command(Box<Command>),
}

/// Which of the session's events an attaching client asks for.
///
/// This is the wire spelling only. The server maps it to the filter its event
/// hub works in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventFilterSpec {
    /// Every event the session publishes.
    All,
}

/// One message answering an [`IpcRequest`].
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know. An absent `request_id` means the request could not be read, so a
/// misspelled one is an error.
///
/// `R` is the answer. A server uses `IpcResponse`, where `R` is
/// [`IpcResult`]. A caller uses [`IncomingResponse`], where a result this
/// build does not have arrives as [`MaybeKnown::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpcResponse<R = IpcResult> {
    /// The `request_id` of the request being answered, or `None` when the
    /// bytes received were too malformed to read one — a caller that sent
    /// request 7 and reads `None` knows the answer belongs to no request of
    /// its own.
    pub request_id: Option<u64>,
    /// The answer itself.
    pub result: R,
}

/// A response as a caller reads it: the result may name something this build
/// does not have.
pub type IncomingResponse = IpcResponse<MaybeKnown<IpcResult>>;

/// The answer to a request.
///
/// A field this build does not know is ignored, so a peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResult {
    /// Answers [`IpcRequestKind::Hello`]: the connection is open, the ranges
    /// overlap, and the caller met the token rule for where it came from.
    Hello {
        /// The version both sides use on this connection: the highest they
        /// both speak.
        protocol_version: u32,
    },
    /// Answers [`IpcRequestKind::Attach`]: the client is registered and its
    /// event subscription is live. Every field is the server's own answer, and
    /// this frame is the last one written before the event stream starts.
    Attached {
        /// The id the server minted for this client. A second attach mints a
        /// new one.
        client_id: ClientId,
        /// The session the client joined.
        session_id: SessionId,
        /// What the session contains right now, built for this reply.
        structure: AttachedSessionStructureSnapshot,
    },
    /// What dispatching the submitted command produced.
    CommandResult(CommandResult),
    /// The session's full description.
    Overview(SessionOverview),
    /// The session's layout: each tab's split tree and its solved rectangles.
    Layout(SessionLayout),
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// Why a request was refused.
///
/// A field this build does not know is ignored, and so is a
/// [`code`](Self::code) it has no name for. A refusal is the one message a
/// newer koshi is most likely to have added to, and it always carries a
/// [`message`](Self::message) a person can read.
///
/// Example — a build with no `rate_limited` code reads
/// `{"code":"rate_limited","message":"too many attach requests"}` as
/// [`IpcErrorCode::Unknown`] and still shows the sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcErrorPayload {
    /// The refusal, as a value a caller can branch on. A code this build has
    /// no name for reads as [`IpcErrorCode::Unknown`].
    #[serde(default, deserialize_with = "crate::wire::or_default")]
    pub code: IpcErrorCode,
    /// A human-facing sentence naming what was wrong.
    pub message: String,
}

/// The kinds of refusal a request can meet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    /// The token presented does not match the session's.
    BadToken,
    /// The caller and this build share no protocol version. The message names
    /// both ranges.
    UnsupportedVersion,
    /// The caller named a request kind this build does not have. The message
    /// names it. The connection stays open.
    UnsupportedKind,
    /// The bytes received are not a request this build can read.
    MalformedRequest,
    /// A request arrived before [`IpcRequestKind::Hello`] opened the
    /// connection.
    HelloRequired,
    /// The caller is another user of this machine, and this Koshi serves only
    /// the user who started it. The message names the `koshi.kdl` setting that
    /// lets other users in.
    OtherUsersOff,
    /// A refusal this build has no name for, from a newer koshi. The
    /// [`message`](IpcErrorPayload::message) beside it still reads.
    #[default]
    Unknown,
}

impl WireVariants for IpcRequestKind {
    /// Every request kind this build has. A kind added to
    /// [`IpcRequestKind`] is added here and to
    /// [`IpcRequestKind::name`] in the same change.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Attach",
        "KeyPress",
        "Resize",
        "Paste",
        "Mouse",
        "SubmitCommand",
        "Discovery",
        "Layout",
    ];
}

impl WireName for IpcRequestKind {
    fn wire_name(&self) -> &'static str {
        self.name()
    }
}

impl WireVariants for IpcResult {
    /// Every answer this build has. A variant added to [`IpcResult`] is added
    /// here in the same change.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Attached",
        "CommandResult",
        "Overview",
        "Layout",
        "Error",
    ];
}

impl WireName for IpcResult {
    fn wire_name(&self) -> &'static str {
        match self {
            IpcResult::Hello { .. } => "Hello",
            IpcResult::Attached { .. } => "Attached",
            IpcResult::CommandResult(_) => "CommandResult",
            IpcResult::Overview(_) => "Overview",
            IpcResult::Layout(_) => "Layout",
            IpcResult::Error(_) => "Error",
        }
    }
}

#[cfg(test)]
mod tests;
