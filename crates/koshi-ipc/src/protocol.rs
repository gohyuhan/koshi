//! Wire messages for the control socket.
//!
//! An exchange is one [`IpcRequest`](crate::protocol::IpcRequest) and the
//! [`IpcResponse`](crate::protocol::IpcResponse) answering it. The response
//! repeats the request's `request_id`. A response to bytes that could not be
//! read as a request carries no `request_id`.
//!
//! Every connection opens with
//! [`IpcRequestKind::Hello`](crate::protocol::IpcRequestKind::Hello). It
//! settles the two facts that hold for the whole connection: the protocol
//! version both sides use, and the
//! [`ConnectionToken`](crate::protocol::ConnectionToken) the caller presents.
//! No request after it repeats them.
//!
//! This module is the vocabulary only. Framing and sockets belong to the
//! transport layer. The Hello checks belong to [`handshake`](crate::handshake).

use std::fmt;

use koshi_core::command::{Command, CommandEnvelope, CommandResult};
use koshi_core::compat::SESSION_PROTOCOL;
use koshi_core::discovery::SessionOverview;
use koshi_core::geometry::{Direction, PaneArea, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::key::KeyChord;
use koshi_core::mouse::MouseInput;
use koshi_core::recent_event::RecentEvent;
use koshi_core::redact::REDACTED;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::attach::AttachedSessionStructureSnapshot;
use crate::layout::SessionLayout;
use crate::wire::{Answer, Envelope, MaybeKnown, WireName, WireVariants};

/// The highest protocol version this build speaks, and the one it uses when
/// the peer speaks it too.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::SESSION_PROTOCOL`].
pub const PROTOCOL_VERSION: u32 = SESSION_PROTOCOL.max;

/// The lowest protocol version this build speaks. A peer whose highest is
/// below this one is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
///
/// The floor is 3, the version this build speaks. Raising it drops support
/// for every build below it.
pub const MIN_PROTOCOL_VERSION: u32 = SESSION_PROTOCOL.min;

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
/// directory.
///
/// The secret leaves this type in two ways, and only two:
///
/// - `Serialize` and [`expose`](Self::expose) write the **real secret**, for
///   the endpoint file and the socket. `serde_json::to_string(&hello)` on the
///   Hello [`hello`](IpcRequestKind::hello) builds yields
///   `{"Hello":{"min_protocol_version":2,"max_protocol_version":3,
///   "token":"k7Qx…","remote":false}}`, secret included.
/// - `Debug` and `Display` write `***`. A token that reaches a log line, a
///   trace, or an error dump reveals nothing.
///
/// Anything describing a request in a log uses the second form, or
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
    ///
    /// Panics if the operating system's random source fails.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .expect("every supported platform provides the system random source");
        ConnectionToken(crate::bytes::hex(&bytes))
    }

    /// The secret itself, as plain text.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl PartialEq for ConnectionToken {
    /// Compares two secrets of the same length byte by byte through the last
    /// byte, never stopping at the first mismatch. `subtle` reads each byte's
    /// verdict back through a volatile load, which the compiler may not fold
    /// away.
    ///
    /// Two secrets of different lengths are unequal at once, with no byte
    /// compared. Every generated token has one length, 64 hex characters.
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
/// know. A misspelled `request_id` is an error.
///
/// `K` is the request kind. A sender uses `IpcRequest`, where `K` is
/// [`IpcRequestKind`]. A server uses [`IncomingRequest`], where a kind this
/// build does not have arrives as [`MaybeKnown::Unknown`].
pub type IpcRequest<K = IpcRequestKind> = Envelope<K>;

/// A request as a server reads it: the kind may name something this build does
/// not have.
pub type IncomingRequest = IpcRequest<MaybeKnown<IpcRequestKind>>;

/// What a request asks for.
///
/// On a connection already serving an attached client's event stream,
/// [`KeyPress`](Self::KeyPress), [`Resize`](Self::Resize),
/// [`Paste`](Self::Paste) and [`SubmitCommand`](Self::SubmitCommand) are
/// answered by the next painted frame, not by an [`IpcResponse`].
///
/// [`Mouse`](Self::Mouse) is answered by exactly one
/// [`SessionEvent::MouseAnswer`](crate::event::SessionEvent::MouseAnswer)
/// carrying that request's `request_id`, always — including when the round
/// produced nothing to report, where the answer's list is empty. The viewer
/// moves its drag anchor over the cells the session took from that answer.
///
/// A field this build does not know is ignored. A peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequestKind {
    /// Opens the connection: names the protocol versions the caller speaks and
    /// presents the token. Sent before any other kind.
    ///
    /// The two versions are a range, lowest and highest. The server answers
    /// with the highest version both sides speak, or refuses when the ranges
    /// do not overlap.
    ///
    /// A second Hello on an open connection is checked and answered the same
    /// way. It settles the connection's version again from its own range, and
    /// a `remote` of `true` stays set from then on.
    Hello {
        /// The lowest protocol version the caller speaks.
        min_protocol_version: u32,
        /// The highest protocol version the caller speaks.
        max_protocol_version: u32,
        /// The secret read from the endpoint file.
        token: ConnectionToken,
        /// Whether the connection this Hello opens carries a caller on
        /// another machine. The router sets it on the local connection it
        /// opens for a remote caller. Absent means `false`. It changes nothing
        /// about whether the Hello is accepted. The server records it as the
        /// origin of every client attached on this connection.
        #[serde(default)]
        remote: bool,
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
        /// The client record to come back as, named by a caller re-attaching
        /// after the session replaced its own process image. The server hands
        /// that record back when it still holds it, the tab that record was
        /// viewing still exists, and no connection is streaming for it, and
        /// mints a fresh client in every other case. Absent on a first attach,
        /// and from a caller that predates this field.
        #[serde(default)]
        resume: Option<ClientId>,
        /// The token the session handed this caller at its last attach,
        /// presented to get that attach's view back: the active tab, the
        /// focused pane of each tab, the zoomed pane of each tab, and the
        /// scroll offset of each pane. Absent on a first attach, and from a
        /// caller that predates this field. A token the session does not
        /// hold, and a token older than 120 seconds, attach with a fresh view
        /// instead of failing.
        #[serde(default)]
        resume_token: Option<ConnectionToken>,
        /// The pane region the caller draws the tab's panes in, which the
        /// server records on the client. Absent, the server sizes the
        /// client as its viewport minus two rows.
        #[serde(default)]
        pane_area: Option<PaneArea>,
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
        /// The pane region the client draws the tab's panes in at the new
        /// size; `None` replaces any earlier report.
        #[serde(default)]
        pane_area: Option<PaneArea>,
    },
    /// Text the attached client's outer terminal pasted, for the pane it is
    /// typing into. Carried whole: no character of it fires a keybinding.
    Paste {
        /// The pasted text, exactly as the client's terminal delivered it.
        text: String,
    },
    /// One round of mouse actions the attached client decided, in the order
    /// the session must run them. A round is what the viewer accumulated for
    /// one host mouse event. It carries one `request_id` and receives one
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
    /// Ask the session for the events it published most recently, newest last.
    /// The answer holds each event's name and the ids it named, and no payload
    /// content of any kind.
    RecentEvents,
    /// Restart the session server: it sends its answer, then replaces its own
    /// process image with the binary at the path it started from. Every pane,
    /// its child process, its terminal and its scrollback stay as they are.
    /// Each attached client attaches again and finds the session it left.
    Restart,
    /// The caller sends nothing more on this connection. The session serves
    /// every request that arrived before it, then closes the connection. No
    /// answer comes back.
    ///
    /// An attached client sends it when it reads
    /// [`SessionEvent::Restarting`](crate::event::SessionEvent::Restarting).
    /// Requests arrive in the order the caller queued them.
    Leaving,
}

impl IpcRequestKind {
    /// The Hello this build opens a connection with: the versions it speaks,
    /// [`MIN_PROTOCOL_VERSION`] then [`PROTOCOL_VERSION`], the `token` the
    /// caller presents, and `remote` set to `false`.
    #[must_use]
    pub fn hello(token: ConnectionToken) -> IpcRequestKind {
        IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token,
            remote: false,
        }
    }

    /// The kind's name, e.g. `"SubmitCommand"`. Carries no payload: not the
    /// connection token, not the text the user typed. Safe on a log line.
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
            IpcRequestKind::RecentEvents => "RecentEvents",
            IpcRequestKind::Restart => "Restart",
            IpcRequestKind::Leaving => "Leaving",
        }
    }
}

/// One thing an attached client asks the session to do for a mouse event.
///
/// The wire spelling of the viewer's own `MouseAction`, variant for variant.
/// Every variant names its target. The session hit-tests nothing.
///
/// [`Command`](Self::Command) carries a command the mouse issued inside its
/// round, in its place among the other actions.
///
/// A field this build does not know is ignored. A peer that adds one still
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
/// know. An absent `request_id` means the request could not be read. A
/// misspelled one is an error.
///
/// `R` is the answer. A server uses `IpcResponse`, where `R` is
/// [`IpcResult`]. A caller uses [`IncomingResponse`], where a result this
/// build does not have arrives as [`MaybeKnown::Unknown`].
pub type IpcResponse<R = IpcResult> = Answer<R>;

/// A response as a caller reads it: the result may name something this build
/// does not have.
pub type IncomingResponse = IpcResponse<MaybeKnown<IpcResult>>;

/// The answer to a request.
///
/// A field this build does not know is ignored. A peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResult {
    /// Answers [`IpcRequestKind::Hello`]: the connection is open, the ranges
    /// overlap, and the caller met the token rule for where it came from.
    Hello {
        /// The version both sides use on this connection: the highest they
        /// both speak.
        protocol_version: u32,
        /// The build version of the answering session server, e.g. `0.3.0`.
        /// Empty when the session server predates this field.
        #[serde(default)]
        version: String,
    },
    /// Answers [`IpcRequestKind::Attach`]: the client is registered and its
    /// event subscription is live. Every field is the server's own answer, and
    /// this frame is the last one written before the event stream starts.
    Attached {
        /// The id the server minted for this client. A second attach mints a
        /// new one, unless its `resume` named a record the server handed back.
        client_id: ClientId,
        /// The session the client joined.
        session_id: SessionId,
        /// What the session contains right now, built for this reply.
        structure: AttachedSessionStructureSnapshot,
        /// The fresh secret this attach minted, presented on the next attach
        /// to get this attach's view back. `None` from a session server that
        /// predates this field.
        #[serde(default)]
        resume_token: Option<ConnectionToken>,
        /// The pane region the server holds for this client, exactly as the
        /// attach reported it.
        #[serde(default)]
        pane_area: Option<PaneArea>,
    },
    /// What dispatching the submitted command produced.
    CommandResult(CommandResult),
    /// The session's full description.
    Overview(SessionOverview),
    /// The session's layout: each tab's split tree and its solved rectangles.
    Layout(SessionLayout),
    /// The events the session published most recently, oldest first, each
    /// reduced to its name and the ids it named.
    RecentEvents(Vec<RecentEvent>),
    /// Answers [`IpcRequestKind::Restart`]: the reply is sent, then the
    /// session server replaces its image with the binary now on disk.
    Restarting,
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// Why a request was refused.
///
/// A field this build does not know is ignored, and so is a
/// [`code`](Self::code) it has no name for. Every refusal carries a
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
    /// The caller named a target this build does not have. The message names
    /// it.
    NotFound,
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

/// The session protocol, as a serve loop sees it: what a session server
/// answers on its own session's control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPlane;

impl crate::plane::Plane for SessionPlane {
    type Kind = IpcRequestKind;
    type Result = IpcResult;
    type Gate = crate::handshake::Handshake;

    fn refusal(payload: IpcErrorPayload) -> IpcResult {
        IpcResult::Error(payload)
    }

    fn hello(agreed: u32, build: &str) -> IpcResult {
        IpcResult::Hello {
            protocol_version: agreed,
            version: build.to_string(),
        }
    }
}

impl WireVariants for IpcRequestKind {
    /// Every request kind this build has: one entry per variant of
    /// [`IpcRequestKind`], spelled as [`IpcRequestKind::name`] spells it.
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
        "RecentEvents",
        "Restart",
        "Leaving",
    ];
}

impl WireName for IpcRequestKind {
    fn wire_name(&self) -> &'static str {
        self.name()
    }
}

impl WireVariants for IpcResult {
    /// Every answer this build has: one entry per variant of [`IpcResult`],
    /// spelled as [`WireName::wire_name`] spells it.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Attached",
        "CommandResult",
        "Overview",
        "Layout",
        "RecentEvents",
        "Restarting",
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
            IpcResult::RecentEvents(_) => "RecentEvents",
            IpcResult::Restarting => "Restarting",
            IpcResult::Error(_) => "Error",
        }
    }
}

#[cfg(test)]
mod tests;
