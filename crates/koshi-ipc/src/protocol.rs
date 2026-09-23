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
use koshi_core::key::KeyInput;
use koshi_core::mouse::MouseInput;
use koshi_core::recent_event::RecentEvent;
use koshi_core::redact::REDACTED;
use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use subtle::ConstantTimeEq;

use crate::attach::AttachedSessionStructureSnapshot;
use crate::layout::SessionLayout;
use crate::wire::{Answer, Envelope, MaybeKnown, WireName, WireVariants};

/// The highest protocol version this build speaks, and the one it uses when
/// the peer speaks it too.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::SESSION_PROTOCOL`].
pub const PROTOCOL_VERSION: u32 = SESSION_PROTOCOL.maximum_version;

/// The lowest protocol version this build speaks. A peer whose highest is
/// below this one is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
///
/// The floor is 4, the version this build speaks. Raising it drops support
/// for every build below it.
pub const MIN_PROTOCOL_VERSION: u32 = SESSION_PROTOCOL.minimum_version;

/// The version two peers use, given the range each speaks: the highest both
/// have. `None` when the ranges do not overlap.
///
/// Example — a caller speaking 2 to 4 and a build speaking 2 to 2 settle on
/// 2; a caller speaking 5 to 6 and the same build settle on nothing.
#[must_use]
pub fn compute_agreed_protocol_version(
    caller_min_protocol_version: u32,
    caller_max_protocol_version: u32,
    build_min_protocol_version: u32,
    build_max_protocol_version: u32,
) -> Option<u32> {
    let highest_protocol_version = caller_max_protocol_version.min(build_max_protocol_version);
    let lowest_protocol_version = caller_min_protocol_version.max(build_min_protocol_version);
    (lowest_protocol_version <= highest_protocol_version).then_some(highest_protocol_version)
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
///   Hello [`hello`](IpcRequestKind::build_hello_request) builds yields
///   `{"Hello":{"min_protocol_version":4,"max_protocol_version":4,
///   "connection_token":"k7Qx…","is_remote":false}}`, secret included.
/// - `Debug` and `Display` write `***`. A token that reaches a log line, a
///   trace, or an error dump reveals nothing.
///
/// Anything describing a request in a log uses the second form, or
/// [`IpcRequestKind::get_request_kind_name`], which carries no payload at all.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionToken(String);

impl ConnectionToken {
    /// Wrap an already-generated secret.
    #[must_use]
    pub fn from_secret(secret: impl Into<String>) -> Self {
        ConnectionToken(secret.into())
    }

    /// Generate a fresh secret: 32 bytes from the operating system's
    /// cryptographic random source, written as 64 lowercase hex characters.
    /// Every generated token has this one length.
    ///
    /// Panics if the operating system's random source fails.
    #[must_use]
    pub fn generate() -> Self {
        let mut random_token_bytes = [0u8; 32];
        getrandom::fill(&mut random_token_bytes)
            .expect("every supported platform provides the system random source");
        ConnectionToken(crate::bytes::format_hex(&random_token_bytes))
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
    fn eq(&self, other_token: &Self) -> bool {
        self.0.as_bytes().ct_eq(other_token.0.as_bytes()).into()
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
/// `RequestKind` is the request kind. A sender uses `IpcRequest`, where
/// `RequestKind` is
/// [`IpcRequestKind`]. A server uses [`IncomingRequest`], where a kind this
/// build does not have arrives as [`MaybeKnown::Unknown`].
pub type IpcRequest<RequestKind = IpcRequestKind> = Envelope<RequestKind>;

/// A request as a server reads it: the kind may name something this build does
/// not have.
pub type IncomingRequest = IpcRequest<MaybeKnown<IpcRequestKind>>;

/// Native image protocols the terminal attached to one client proved it can
/// receive.
///
/// Each field defaults to `false`, so a client that does not report this
/// record receives placement metadata without pixel transfers. New protocol
/// fields can be added without changing the attach shape.
const GRAPHICS_CAPABILITY_FIELD_NAMES: &[&str] =
    &["supports_kitty", "supports_iterm", "supports_sixel"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GraphicsCapabilities {
    /// The terminal answered the Kitty graphics protocol query with `OK`.
    pub supports_kitty: bool,
    /// The terminal advertised the iTerm2 inline-image protocol.
    pub supports_iterm: bool,
    /// The terminal advertised the DEC Sixel protocol.
    pub supports_sixel: bool,
}

enum GraphicsCapabilitiesField {
    SupportsKitty,
    SupportsIterm,
    SupportsSixel,
    RetiredKitty,
    RetiredIterm,
    RetiredSixel,
    Unknown,
}

impl<'de> Deserialize<'de> for GraphicsCapabilitiesField {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        struct GraphicsCapabilitiesFieldVisitor;

        impl<'de> Visitor<'de> for GraphicsCapabilitiesFieldVisitor {
            type Value = GraphicsCapabilitiesField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a graphics capability field name")
            }

            fn visit_str<ErrorType>(self, value: &str) -> Result<Self::Value, ErrorType>
            where
                ErrorType: de::Error,
            {
                Ok(match value {
                    "supports_kitty" => GraphicsCapabilitiesField::SupportsKitty,
                    "supports_iterm" => GraphicsCapabilitiesField::SupportsIterm,
                    "supports_sixel" => GraphicsCapabilitiesField::SupportsSixel,
                    "kitty" => GraphicsCapabilitiesField::RetiredKitty,
                    "iterm" => GraphicsCapabilitiesField::RetiredIterm,
                    "sixel" => GraphicsCapabilitiesField::RetiredSixel,
                    _ => GraphicsCapabilitiesField::Unknown,
                })
            }
        }

        deserializer.deserialize_identifier(GraphicsCapabilitiesFieldVisitor)
    }
}

impl<'de> Deserialize<'de> for GraphicsCapabilities {
    fn deserialize<DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        struct GraphicsCapabilitiesVisitor;

        impl<'de> Visitor<'de> for GraphicsCapabilitiesVisitor {
            type Value = GraphicsCapabilities;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a graphics capability object")
            }

            fn visit_map<MapType>(self, mut map: MapType) -> Result<Self::Value, MapType::Error>
            where
                MapType: MapAccess<'de>,
            {
                let mut supports_kitty = None;
                let mut supports_iterm = None;
                let mut supports_sixel = None;

                while let Some(field) = map.next_key::<GraphicsCapabilitiesField>()? {
                    match field {
                        GraphicsCapabilitiesField::SupportsKitty => {
                            if supports_kitty.is_some() {
                                return Err(de::Error::duplicate_field("supports_kitty"));
                            }
                            supports_kitty = Some(map.next_value()?);
                        }
                        GraphicsCapabilitiesField::SupportsIterm => {
                            if supports_iterm.is_some() {
                                return Err(de::Error::duplicate_field("supports_iterm"));
                            }
                            supports_iterm = Some(map.next_value()?);
                        }
                        GraphicsCapabilitiesField::SupportsSixel => {
                            if supports_sixel.is_some() {
                                return Err(de::Error::duplicate_field("supports_sixel"));
                            }
                            supports_sixel = Some(map.next_value()?);
                        }
                        GraphicsCapabilitiesField::RetiredKitty => {
                            return Err(de::Error::unknown_field(
                                "kitty",
                                GRAPHICS_CAPABILITY_FIELD_NAMES,
                            ));
                        }
                        GraphicsCapabilitiesField::RetiredIterm => {
                            return Err(de::Error::unknown_field(
                                "iterm",
                                GRAPHICS_CAPABILITY_FIELD_NAMES,
                            ));
                        }
                        GraphicsCapabilitiesField::RetiredSixel => {
                            return Err(de::Error::unknown_field(
                                "sixel",
                                GRAPHICS_CAPABILITY_FIELD_NAMES,
                            ));
                        }
                        GraphicsCapabilitiesField::Unknown => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(GraphicsCapabilities {
                    supports_kitty: supports_kitty.unwrap_or(false),
                    supports_iterm: supports_iterm.unwrap_or(false),
                    supports_sixel: supports_sixel.unwrap_or(false),
                })
            }
        }

        deserializer.deserialize_map(GraphicsCapabilitiesVisitor)
    }
}

impl GraphicsCapabilities {
    /// Return whether at least one native image protocol was proved.
    #[must_use]
    pub const fn has_native_image_protocol(self) -> bool {
        self.supports_kitty || self.supports_iterm || self.supports_sixel
    }

    /// Return whether the terminal proved no native image protocol.
    fn is_empty(&self) -> bool {
        !self.has_native_image_protocol()
    }
}

/// What a request asks for.
///
/// On a connection already serving an attached client's event stream,
/// [`Keyboard`](Self::Keyboard), [`Resize`](Self::Resize),
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
        connection_token: ConnectionToken,
        /// Whether the connection this Hello opens carries a caller on
        /// another machine. The router sets it on the local connection it
        /// opens for a remote caller. Absent means `false`. It changes nothing
        /// about whether the Hello is accepted. The server records it as the
        /// origin of every client attached on this connection.
        #[serde(default)]
        is_remote: bool,
    },
    /// Join the session as a viewing client: the server mints the client,
    /// registers it for the events `event_filter` selects, and answers with
    /// [`IpcResult::Attached`].
    ///
    /// The caller names no identity of its own. Who the client is, what it may
    /// do, and what it is called are all decided by the server.
    Attach {
        /// The caller's terminal size in cells, which the server records as
        /// the client's viewport.
        viewport: Size,
        /// Which of the session's events the client receives.
        event_filter: EventFilterSpec,
        /// The client record to come back as, named by a caller re-attaching
        /// after the session replaced its own process image. The server hands
        /// that record back when it still holds it, the tab that record was
        /// viewing still exists, and no connection is streaming for it, and
        /// mints a fresh client in every other case. Absent on a first attach,
        /// and from a caller that predates this field.
        #[serde(default)]
        resume_client_id: Option<ClientId>,
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
        #[serde(default, skip_serializing_if = "GraphicsCapabilities::is_empty")]
        graphics_capabilities: GraphicsCapabilities,
        /// The cell dimensions measured by this terminal before the attach,
        /// or `None` when the terminal has no usable measurement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
    },
    /// One keyboard event an attached client sends for the pane it is typing
    /// into: its keymap bound nothing to the event, or no chord could name it.
    ///
    /// Carries every field the client's terminal reported, a release and a key
    /// no keybinding can name included. The session decides what reaches the
    /// pane.
    Keyboard {
        /// The keyboard event the client read from its terminal.
        key_input: KeyInput,
    },
    /// The attached client's terminal changed size.
    Resize {
        /// The client's new terminal size in cells.
        viewport: Size,
        /// The pane region the client draws the tab's panes in at the new
        /// size; `None` replaces any earlier report.
        #[serde(default)]
        pane_area: Option<PaneArea>,
        /// The cell dimensions measured for this resized viewport, or `None`
        /// to clear the client's previous measurement until a reply arrives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
    },
    /// The attached client measured the pixel dimensions of one terminal cell.
    CellSize {
        /// The nonzero pixel dimensions of one cell.
        cell_size: koshi_core::geometry::PixelCellSize,
    },
    /// Text the attached client's outer terminal pasted, for the pane it is
    /// typing into. Carried whole: no character of it fires a keybinding.
    Paste {
        /// The pasted text, exactly as the client's terminal delivered it.
        pasted_text: String,
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
        tab_id: Option<TabId>,
    },
    /// Ask for a bounded read-only preview of moving `pane_id` toward
    /// `destination_tab_id`. The answer arrives on the attached event stream.
    ReadPanePlacement {
        /// The pane whose current visible content is previewed.
        pane_id: PaneId,
        /// The tab whose current layout is previewed as the destination.
        destination_tab_id: TabId,
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
    /// [`MIN_PROTOCOL_VERSION`] then [`PROTOCOL_VERSION`], the connection
    /// token the
    /// caller presents, and `is_remote` set to `false`.
    #[must_use]
    pub fn build_hello_request(connection_token: ConnectionToken) -> IpcRequestKind {
        IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token,
            is_remote: false,
        }
    }

    /// The kind's name, e.g. `"SubmitCommand"`. Carries no payload: not the
    /// connection token, not the text the user typed. Safe on a log line.
    #[must_use]
    pub fn get_request_kind_name(&self) -> &'static str {
        match self {
            IpcRequestKind::Hello { .. } => "Hello",
            IpcRequestKind::Attach { .. } => "Attach",
            IpcRequestKind::Keyboard { .. } => "Keyboard",
            IpcRequestKind::Resize { .. } => "Resize",
            IpcRequestKind::CellSize { .. } => "CellSize",
            IpcRequestKind::Paste { .. } => "Paste",
            IpcRequestKind::Mouse(_) => "Mouse",
            IpcRequestKind::SubmitCommand(_) => "SubmitCommand",
            IpcRequestKind::Discovery => "Discovery",
            IpcRequestKind::Layout { .. } => "Layout",
            IpcRequestKind::ReadPanePlacement { .. } => "ReadPanePlacement",
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
    /// Move this client's scrollback view of `pane_id` by `scroll_line_count`,
    /// up into history or back down toward live output.
    Scroll {
        /// The pane whose view moves.
        pane_id: PaneId,
        /// Up into history, or down toward live output.
        is_scrolling_up: bool,
        /// Lines to move.
        scroll_line_count: usize,
    },
    /// Hand the event to the program in `pane_id` as a mouse report. The session
    /// encodes it from that pane's live tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane_id: PaneId,
        /// The event, with the cell it landed on and the modifiers held.
        mouse_input: MouseInput,
    },
    /// Send `arrow_count` cursor arrow keys to `pane_id` — the alternate-scroll
    /// (`?1007`) translation of a wheel tick on the alternate screen.
    AltScrollArrows {
        /// The pane whose program receives the arrows.
        pane_id: PaneId,
        /// Up-arrows, or down-arrows.
        is_scrolling_up: bool,
        /// How many.
        arrow_count: usize,
    },
    /// Move `pane_id`'s `border_side` border `requested_cell_count` cells, one
    /// cell per step, in the direction `resize_step` names.
    Resize {
        /// The pane whose border moves.
        pane_id: PaneId,
        /// Which of the pane's borders was grabbed.
        border_side: Direction,
        /// `1` grows the pane, `-1` shrinks it.
        resize_step: i16,
        /// How many single-cell steps the pointer travelled.
        requested_cell_count: u16,
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
/// `Response` is the answer. A server uses `IpcResponse`, where `Response` is
/// [`IpcResult`]. A caller uses [`IncomingResponse`], where a result this
/// build does not have arrives as [`MaybeKnown::Unknown`].
pub type IpcResponse<Response = IpcResult> = Answer<Response>;

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
        build_version: String,
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
        session_structure: AttachedSessionStructureSnapshot,
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
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    pub code: IpcErrorCode,
    /// A human-facing sentence naming what was wrong.
    pub message: String,
}

/// The kinds of refusal a request can meet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The requested read-only snapshot exceeds a bounded resource limit.
    ResourceLimit,
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
    type RequestKind = IpcRequestKind;
    type Response = IpcResult;
    type Gate = crate::handshake::Handshake;

    fn build_refusal_response(error_payload: IpcErrorPayload) -> IpcResult {
        IpcResult::Error(error_payload)
    }

    fn build_hello_response(agreed_protocol_version: u32, build_version: &str) -> IpcResult {
        IpcResult::Hello {
            protocol_version: agreed_protocol_version,
            build_version: build_version.to_string(),
        }
    }
}

impl WireVariants for IpcRequestKind {
    /// Every request kind this build has: one entry per variant of
    /// [`IpcRequestKind`], spelled as [`IpcRequestKind::get_request_kind_name`] spells it.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Attach",
        "Keyboard",
        "Resize",
        "CellSize",
        "Paste",
        "Mouse",
        "SubmitCommand",
        "Discovery",
        "Layout",
        "ReadPanePlacement",
        "RecentEvents",
        "Restart",
        "Leaving",
    ];
}

impl WireName for IpcRequestKind {
    fn wire_name(&self) -> &'static str {
        self.get_request_kind_name()
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
