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
use std::sync::OnceLock;

use koshi_core::action::ActionReference;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, MoveFloatingPaneArgs, TiledPlacementTarget,
};
use koshi_core::compat::SESSION_PROTOCOL;
use koshi_core::discovery::SessionOverview;
use koshi_core::geometry::{Direction, PaneArea, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{BindingModifierFlags, KeyInput};
use koshi_core::mouse::MouseKind;
use koshi_core::notification::NotificationQueue;
use koshi_core::recent_event::RecentEvent;
use koshi_core::redact::REDACTED;
use koshi_layout::placement::PlacedTabLayout;
use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::layout::SessionLayout;
use crate::placement::PanePlacementPreviewTarget;
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
/// The floor equals [`PROTOCOL_VERSION`]: this build speaks one version.
/// Raising it drops support for every build below it.
pub const MIN_PROTOCOL_VERSION: u32 = SESSION_PROTOCOL.minimum_version;

/// The version two peers use, given the range each speaks: the highest both
/// have. `None` when the ranges do not overlap.
///
/// Example — a caller speaking 2 to 4 and a build speaking 2 to 2 settle on
/// 2; a caller speaking 5 to 6 and the same build settle on nothing.
#[must_use]
pub fn compute_agreed_protocol_version(
    caller_minimum_protocol_version: u32,
    caller_maximum_protocol_version: u32,
    build_minimum_protocol_version: u32,
    build_maximum_protocol_version: u32,
) -> Option<u32> {
    let highest_protocol_version =
        caller_maximum_protocol_version.min(build_maximum_protocol_version);
    let lowest_protocol_version =
        caller_minimum_protocol_version.max(build_minimum_protocol_version);
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
/// - `Serialize` and [`expose_secret`](Self::expose_secret) write the **real secret**, for
///   the endpoint file and the socket.
///   `serde_json::to_string(&IpcRequestKind::build_hello_request(connection_token))`
///   yields `{"Hello":{"minimum_protocol_version":5,"maximum_protocol_version":5,
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
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Whether the secret is the empty text: the token a session asks another
    /// local user for. Every generated token is 64 characters long.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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

/// Identifies one process image of a session server or a router: the program a
/// process runs from its start, or from its last `exec`. A server that
/// replaces its image answers its next Hello with another id. A server that
/// keeps its image keeps its id, also when it rotates its connection token.
/// The wire form is the bare uuid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessImageId(Uuid);

impl ProcessImageId {
    /// Generate a fresh id, a `UUIDv7`.
    #[must_use]
    pub fn generate() -> Self {
        ProcessImageId(Uuid::now_v7())
    }

    /// The id of the process image this code runs in. The first call
    /// generates it, and every call after it in the same image returns that
    /// id.
    #[must_use]
    pub fn get_own() -> Self {
        static OWN_PROCESS_IMAGE_ID: OnceLock<ProcessImageId> = OnceLock::new();
        *OWN_PROCESS_IMAGE_ID.get_or_init(ProcessImageId::generate)
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

/// The field names [`GraphicsCapabilities`] reads, named in the error for a
/// retired field name.
const GRAPHICS_CAPABILITY_FIELD_NAMES: &[&str] =
    &["supports_kitty", "supports_iterm", "supports_sixel"];

/// Native image protocols the terminal attached to one client proved it can
/// receive.
///
/// Each field defaults to `false`: a client that does not report this record
/// receives placement metadata without pixel transfers. A field this build
/// does not know is ignored. The retired names `kitty`, `iterm` and `sixel`
/// are refused as unknown fields.
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

            fn visit_str<ErrorType>(self, field_name: &str) -> Result<Self::Value, ErrorType>
            where
                ErrorType: de::Error,
            {
                Ok(match field_name {
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

            fn visit_map<MapType>(
                self,
                mut map_access: MapType,
            ) -> Result<Self::Value, MapType::Error>
            where
                MapType: MapAccess<'de>,
            {
                let mut supports_kitty = None;
                let mut supports_iterm = None;
                let mut supports_sixel = None;

                while let Some(field) = map_access.next_key::<GraphicsCapabilitiesField>()? {
                    match field {
                        GraphicsCapabilitiesField::SupportsKitty => {
                            if supports_kitty.is_some() {
                                return Err(de::Error::duplicate_field("supports_kitty"));
                            }
                            supports_kitty = Some(map_access.next_value()?);
                        }
                        GraphicsCapabilitiesField::SupportsIterm => {
                            if supports_iterm.is_some() {
                                return Err(de::Error::duplicate_field("supports_iterm"));
                            }
                            supports_iterm = Some(map_access.next_value()?);
                        }
                        GraphicsCapabilitiesField::SupportsSixel => {
                            if supports_sixel.is_some() {
                                return Err(de::Error::duplicate_field("supports_sixel"));
                            }
                            supports_sixel = Some(map_access.next_value()?);
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
                            let _: IgnoredAny = map_access.next_value()?;
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
/// answered by the next painted update, not by an [`IpcResponse`]. A rejected
/// `SubmitCommand` whose source is the client's key binding is also answered
/// by one [`SessionEvent::CommandRejected`](crate::event::SessionEvent::CommandRejected),
/// unless its target is a direction or the next or previous floating pane
/// and it found none. A client whose queue is full reads that rejection in
/// the next [`SessionEvent::Resync`](crate::event::SessionEvent::Resync)
/// instead.
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
        minimum_protocol_version: u32,
        /// The highest protocol version the caller speaks.
        maximum_protocol_version: u32,
        /// The secret read from the endpoint file.
        connection_token: ConnectionToken,
        /// Whether the connection this Hello opens carries a caller on
        /// another machine. The router sets it on the local connection it
        /// opens for a remote caller. It changes nothing about whether the
        /// Hello is accepted. The server records it as the origin of every
        /// client attached on this connection.
        is_remote: bool,
    },
    /// Join the session as a viewing client: the server mints the client,
    /// registers it for every event the session publishes, and answers with
    /// [`IpcResult::Attached`].
    ///
    /// The caller names no identity of its own. Who the client is, what it may
    /// do, and what it is called are all decided by the server.
    Attach {
        /// The caller's terminal size in cells, which the server records as
        /// the client's viewport.
        viewport_size: Size,
        /// The client record to come back as, named by a caller re-attaching
        /// after the session replaced its own process image. The server hands
        /// that record back when it still holds it, the tab that record was
        /// viewing still exists, no connection is streaming for it, and
        /// `resume_token` is the token that record's last attach minted, and
        /// mints a fresh client in every other case. `None` on a first attach.
        resume_client_id: Option<ClientId>,
        /// The token the session handed this caller at its last attach,
        /// presented to get that attach's view back: the active tab, the
        /// focused pane of each tab, the zoomed pane of each tab, the scroll
        /// offset of each pane, and the view of the floating panes. `None` on a
        /// first attach. A token the session does not hold, and a token older
        /// than 24 hours, attach with a fresh view instead of failing. A token
        /// minted for a client that is still attached, or for a record that
        /// came across a swap and still waits for its client, takes that
        /// client's view over, and the session removes that client.
        resume_token: Option<ConnectionToken>,
        /// The pane region the caller draws the tab's panes in, which the
        /// server records on the client. Absent, the server sizes the
        /// client as its viewport minus two rows.
        pane_area: Option<PaneArea>,
        /// The native image protocols the caller's terminal proved it can
        /// receive. Left off the wire when it proved none; absent, it reads as
        /// none.
        #[serde(default, skip_serializing_if = "GraphicsCapabilities::is_empty")]
        graphics_capabilities: GraphicsCapabilities,
        /// The cell dimensions measured by this terminal before the attach,
        /// or `None` when the terminal has no usable measurement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell_size: Option<koshi_core::geometry::PixelCellSize>,
        /// The client's link timeout, in milliseconds. Once the client is
        /// attached, the session writes it a
        /// [`SessionEvent::Pong`](crate::event::SessionEvent::Pong) whenever
        /// it has written it nothing for a third of this value. `None` writes
        /// no `Pong`. A value below `1000` is refused with
        /// [`IpcErrorCode::RequestFailed`]. A client on another machine sends
        /// its `remote-link-timeout-ms` and [`Ping`](Self::Ping) requests; a
        /// client on this machine sends `None`. Left off the wire when `None`;
        /// absent, it reads as `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link_timeout_ms: Option<u32>,
        /// The most frames a second the session sends this client, the
        /// client's `max-frames-per-second`. The session sends the client a
        /// frame at most once per second divided by this value, and the frame
        /// after the client's own key, paste or mouse event goes without that
        /// wait. A value outside `30..=120` is refused with
        /// [`IpcErrorCode::RequestFailed`].
        max_frames_per_second: u16,
        /// The placement target the client previews
        /// ([`SetPanePlacementPreview`](Self::SetPanePlacementPreview)),
        /// named before the client's first frame: that frame and each frame
        /// after it carry the target's preview. `None` names no target.
        placement_preview_target: Option<PanePlacementPreviewTarget>,
    },
    /// The keyboard events an attached client sends for the pane it is typing
    /// into, from one read of its terminal: its keymap bound nothing to each
    /// event, or no chord could name it.
    ///
    /// Each event carries every field the client's terminal reported, a
    /// release and a key no keybinding can name included. The session decides
    /// what reaches the pane, and writes the bytes of every event to the pane
    /// in one write, or writes none of them. Typing `é` and `-` in one read
    /// sends both events in one request, and the pane reads `C3 A9 2D` from
    /// one write.
    Keyboard {
        /// The keyboard events the client read from its terminal, oldest
        /// first.
        key_inputs: Vec<KeyInput>,
    },
    /// The attached client's terminal changed size.
    Resize {
        /// The client's new terminal size in cells.
        viewport_size: Size,
        /// The pane region the client draws the tab's panes in at the new
        /// size; `None` replaces any earlier report.
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
    SubmitCommand {
        /// The command, its id and its source.
        command_envelope: Box<CommandEnvelope>,
        /// The action the attached client's key fired to build the command,
        /// or `None` when no key fired it. The session names it in the
        /// [`SessionEvent::CommandRejected`](crate::event::SessionEvent::CommandRejected)
        /// it sends for the command, and reads it for nothing else. A message
        /// without `fired_action` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        fired_action: Option<ActionReference>,
    },
    /// Ask the session to describe itself in full. The caller narrows the
    /// answer to the query it was asked.
    Discovery,
    /// Ask the session to describe its layout: each tab's split tree, and the
    /// rectangles each viewing client solves it to.
    Layout {
        /// The one tab to describe, or every tab when absent.
        tab_id: Option<TabId>,
    },
    /// Name the placement target whose preview rides this client's painted
    /// updates, or stop the preview with `None`. Nothing answers it. Each
    /// painted update after it carries the target's preview whenever the
    /// preview differs from the one the client holds
    /// ([`painted_update`](crate::painted_update)). On a connection with no
    /// attached client, it closes the connection.
    SetPanePlacementPreview {
        /// The pane and the destination tab the client previews.
        placement_target: Option<PanePlacementPreviewTarget>,
    },
    /// Confirm one tiled pane placement that the client previews. The session
    /// solves the destination tab as the placement leaves it, for this client,
    /// and runs the placement only when that layout equals
    /// `expected_placed_tab_layout`. A frame naming `command_id` as its
    /// client's `last_applied_placement_command_id` shows the placement. A
    /// refused placement is answered with `CommandRejected` naming
    /// `command_id`. On a connection with no attached client, it closes the
    /// connection.
    ConfirmPanePlacement {
        /// The id the client chose for the placement.
        command_id: CommandId,
        /// The pane being placed.
        source_pane_id: PaneId,
        /// Where the pane goes.
        placement_target: TiledPlacementTarget,
        /// The destination tab as the client draws it after the placement.
        expected_placed_tab_layout: PlacedTabLayout,
        /// The action the client's key fired to confirm, or `None` when a
        /// mouse drop confirmed. The session names it in the
        /// `CommandRejected` it sends for a refused placement. A message
        /// without `fired_action` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        fired_action: Option<ActionReference>,
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
    /// [`SessionEvent::Restarting`](crate::event::SessionEvent::Restarting),
    /// when it leaves a dropped link to dial the session again, when its
    /// terminal goes away, and when the session moves it to another session.
    /// A link that a rejoin opens after the quit keybinding or the terminal
    /// going away ended the client gets it with `is_returning` `true`, and so
    /// does the first link of a terminal that cannot be set up. Requests
    /// arrive in the order the caller queued them.
    Leaving {
        /// Whether the client attaches again. `true` keeps the session running
        /// when that client was its last one. `false` lets
        /// `auto-close-session` end a session that the client leaves with no
        /// client. A connection that ends with no `Leaving` counts as `true`
        /// when it came from another machine. A connection on this machine
        /// counts as `true` when it ends before its first request after an
        /// `Attach` that names `resume_client_id`, and as `false` otherwise.
        is_returning: bool,
    },
    /// A heartbeat from an attached client. The session reads it and drops it,
    /// and nothing answers it. A client on another machine sends one every
    /// third of its link timeout while it waits for the answer to its
    /// `Attach`, and goes on sending one every third of its link timeout once
    /// it has joined. On a connection with no attached client, a `Ping`
    /// closes the connection, unless a request waits there for its
    /// [`Commit`](Self::Commit): then the session reads the `Ping` and drops
    /// it.
    Ping,
    /// The attached client painted after it applied the painted update
    /// numbered `update_sequence`, and that update is the last one it applied.
    /// The client sends it after each paint, and nothing answers it. The
    /// session sends that client a new painted update while the bytes it sent
    /// after the reported update stay within the client's byte window. A
    /// report of an update the session has not sent closes the connection. On
    /// a connection with no attached client, a `PaintedUpdate` closes the
    /// connection.
    PaintedUpdate {
        /// The number of the last painted update the client applied.
        update_sequence: u64,
    },
    /// One part of a clipboard copy that a stand-in clipboard tool in a pane
    /// sends before the last part, which goes as
    /// [`ClipboardCopy`](Self::ClipboardCopy).
    ///
    /// The session holds the part on this connection and answers
    /// [`IpcResult::ClipboardCopyPartTaken`] while every byte it holds stays
    /// within `max-copy-bytes`. A part that takes the held bytes past
    /// `max-copy-bytes`, or a session with `pane-clipboard-writes` off, gets
    /// [`IpcResult::ClipboardCopyRouted`] with `should_run_clipboard_tool`
    /// set to `true`, and the session drops every byte it held.
    ///
    /// The session refuses it with [`IpcErrorCode::RequestFailed`] on a
    /// connection from another machine or from another user of this machine.
    ClipboardCopyPart {
        /// The bytes of this part, at most
        /// [`MAX_CLIPBOARD_COPY_PART_BYTE_COUNT`]. Written as one base64
        /// string: the two bytes `[104, 105]` are `"aGk="`.
        #[serde(with = "crate::bytes")]
        copied_bytes: Vec<u8>,
    },
    /// The last part of a clipboard copy that a stand-in clipboard tool in
    /// `pane_id` makes, which can be empty. The copy is every part this
    /// connection sent as [`ClipboardCopyPart`](Self::ClipboardCopyPart)
    /// since its last answered copy, then these bytes.
    ///
    /// It travels inside a [`Prepare`](Self::Prepare), and the
    /// [`Commit`](Self::Commit) that runs it is answered with
    /// [`IpcResult::ClipboardCopyRouted`]. Each client on
    /// another machine that focuses `pane_id` takes the copy as an `OSC 52`
    /// host write. `should_run_clipboard_tool` is `false` when only such
    /// clients focus the pane and each one took the copy, and `true` in every
    /// other case: no client focuses the pane, a client on this machine
    /// focuses it, a client on another machine did not take the copy, such as
    /// one whose connection's writing thread has ended,
    /// `pane-clipboard-writes` is off, the session holds no such pane, or the
    /// copy passes `max-copy-bytes`. A client whose link dropped takes the
    /// copy until the session notices the drop, and its terminal does not get
    /// the copy.
    ///
    /// The session refuses it with [`IpcErrorCode::RequestFailed`] on a
    /// connection from another machine or from another user of this machine.
    ClipboardCopy {
        /// The pane the stand-in clipboard tool runs in, read from its
        /// `KOSHI_PANE_ID`.
        pane_id: PaneId,
        /// The `OSC 52` targets the tool names, such as `c` for the clipboard
        /// and `p` for the primary selection. The session keeps each of `c`,
        /// `p`, `q`, `s` and `0` to `7` once and removes every other byte.
        clipboard_targets: String,
        /// The bytes of the last part, at most
        /// [`MAX_CLIPBOARD_COPY_PART_BYTE_COUNT`]. Written as one base64
        /// string.
        #[serde(with = "crate::bytes")]
        copied_bytes: Vec<u8>,
    },
    /// Hand the session a request to hold until the caller sends
    /// [`Commit`](Self::Commit). The session runs the held request only on
    /// that `Commit`. A caller that gives up before it sends the `Commit` knows
    /// that the request never runs.
    ///
    /// The held request is a [`SubmitCommand`](Self::SubmitCommand), an
    /// [`Attach`](Self::Attach), a [`ClipboardCopy`](Self::ClipboardCopy) or a
    /// [`Restart`](Self::Restart).
    /// Every other kind is refused with [`IpcErrorCode::RequestFailed`], and
    /// nothing is held.
    ///
    /// The session answers [`IpcResult::Prepared`] once its dispatcher has
    /// answered for the request. A `ClipboardCopy` that no client on another
    /// machine would take is answered at once with
    /// [`IpcResult::ClipboardCopyRouted`] carrying
    /// `should_run_clipboard_tool` `true`, and nothing is held: copies are not
    /// allowed, the copy passes `max-copy-bytes`, or no client on another
    /// machine focuses the pane. A `ClipboardCopy` from another machine or
    /// from another user of this machine is refused as it is without
    /// `Prepare`.
    ///
    /// The session drops the held request, and every clipboard part the
    /// connection holds, when the connection ends and when the next request
    /// is anything but a `Commit`, a [`Ping`](Self::Ping) or a
    /// [`Hello`](Self::Hello). A frame that does not decode and a kind this
    /// build does not have leave it in place too.
    ///
    /// A `SubmitCommand`, an `Attach`, a `ClipboardCopy` or a `Restart` that
    /// arrives on its own, with no `Prepare`, is refused with
    /// [`IpcErrorCode::RequestFailed`] and does not run. On a connection that
    /// already carries an attached client's event stream, a `SubmitCommand`
    /// runs as it arrives.
    ///
    /// Example: `Prepare(SubmitCommand(Quit))`, answered `Prepared`, then a
    /// closed connection: the session never quits.
    Prepare(Box<IpcRequestKind>),
    /// Run the request that [`Prepare`](Self::Prepare) handed the session on
    /// this connection. The answer is the held request's own answer, under
    /// this request's `request_id`. With nothing held, the session refuses it
    /// with [`IpcErrorCode::RequestFailed`] and the connection keeps serving.
    Commit,
}

/// The most `copied_bytes` one [`IpcRequestKind::ClipboardCopy`] or
/// [`IpcRequestKind::ClipboardCopyPart`] carries: 12582774.
///
/// A [`Prepare`](IpcRequestKind::Prepare) carrying a `ClipboardCopy` with an
/// empty copy, the request id 18446744073709551615 and 12 target bytes
/// encodes to a frame of 184 bytes. A pane id always encodes to 36
/// characters. Base64 writes each 3 bytes as 4 characters, so
/// (16777216 - 184) / 4 * 3 = 12582774 bytes fit in one
/// [`MAX_FRAME_BYTE_COUNT`](crate::transport::MAX_FRAME_BYTE_COUNT) frame,
/// and 3 bytes more do not.
pub const MAX_CLIPBOARD_COPY_PART_BYTE_COUNT: usize = 12_582_774;

impl IpcRequestKind {
    /// The Hello this build opens a connection with: the versions it speaks,
    /// [`MIN_PROTOCOL_VERSION`] then [`PROTOCOL_VERSION`], the connection
    /// token the
    /// caller presents, and `is_remote` set to `false`.
    #[must_use]
    pub fn build_hello_request(connection_token: ConnectionToken) -> IpcRequestKind {
        IpcRequestKind::Hello {
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
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
            IpcRequestKind::SubmitCommand { .. } => "SubmitCommand",
            IpcRequestKind::Discovery => "Discovery",
            IpcRequestKind::Layout { .. } => "Layout",
            IpcRequestKind::SetPanePlacementPreview { .. } => "SetPanePlacementPreview",
            IpcRequestKind::ConfirmPanePlacement { .. } => "ConfirmPanePlacement",
            IpcRequestKind::RecentEvents => "RecentEvents",
            IpcRequestKind::Restart => "Restart",
            IpcRequestKind::Leaving { .. } => "Leaving",
            IpcRequestKind::Ping => "Ping",
            IpcRequestKind::PaintedUpdate { .. } => "PaintedUpdate",
            IpcRequestKind::ClipboardCopyPart { .. } => "ClipboardCopyPart",
            IpcRequestKind::ClipboardCopy { .. } => "ClipboardCopy",
            IpcRequestKind::Prepare(_) => "Prepare",
            IpcRequestKind::Commit => "Commit",
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
    /// Hand the event to the program in `pane_id` as a mouse report at the
    /// cell `pane_local_cell`. The session encodes it from that pane's live
    /// tracking level and encoding.
    Forward {
        /// The pane whose program receives the report.
        pane_id: PaneId,
        /// What the mouse did.
        mouse_kind: MouseKind,
        /// The modifier keys held during the event.
        modifier_flags: BindingModifierFlags,
        /// The cell in the pane's content the event names, `(0, 0)` at its
        /// top-left content cell.
        pane_local_cell: Point,
    },
    /// Send `arrow_count` cursor arrow keys to `pane_id` — the alternate-scroll
    /// (`?1007`) translation of a wheel tick on the alternate screen.
    AlternateScrollArrows {
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
    /// Move this client's view of a floating pane under `command_id`, the id
    /// the client chose. A move the session refuses is answered with
    /// `CommandRejected` naming `command_id`.
    MoveFloatingPane {
        /// The id the move command carries.
        command_id: CommandId,
        /// The floating pane and its new top-left cell.
        command_args: MoveFloatingPaneArgs,
    },
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
        build_version: String,
        /// The process image of the answering session server, as
        /// [`ProcessImageId::get_own`] gives it.
        process_image_id: ProcessImageId,
    },
    /// Answers [`IpcRequestKind::Attach`]: the client is registered and its
    /// event subscription is live. Every field is the server's own answer, and
    /// this frame is the last one written before the event stream starts.
    Attached {
        /// The id the server minted for this client. A second attach mints a
        /// new one, unless its `resume_client_id` named a record the server handed back.
        client_id: ClientId,
        /// The session the client joined.
        session_id: SessionId,
        /// The session's notification queue as it stands.
        notification_queue: NotificationQueue,
        /// The fresh secret this attach minted, presented on the next attach
        /// to get this attach's view back, and with `resume_client_id` to
        /// claim this client's record after the session replaced its own
        /// process image.
        resume_token: Option<ConnectionToken>,
        /// The pane region the server holds for this client, exactly as the
        /// attach reported it.
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
    /// Answers [`IpcRequestKind::ClipboardCopyPart`]: the session holds the
    /// part, and the caller sends the next part.
    ClipboardCopyPartTaken,
    /// Answers [`IpcRequestKind::ClipboardCopy`], and an
    /// [`IpcRequestKind::ClipboardCopyPart`] the session does not take: the
    /// copy is over, and the session holds none of its bytes.
    ClipboardCopyRouted {
        /// `true` tells the caller to run the real clipboard tool with every
        /// byte of the copy. `false` means every client that focuses the pane
        /// is on another machine, and each one took the copy.
        should_run_clipboard_tool: bool,
    },
    /// Answers [`IpcRequestKind::Prepare`]: the session holds the request, and
    /// runs it when [`IpcRequestKind::Commit`] arrives.
    Prepared,
}

/// Why a request was refused.
///
/// A field this build does not know is ignored, and so is a
/// [`code`](Self::code) it has no name for. Every refusal carries a
/// [`message`](Self::message) a person can read.
///
/// Example — a build with no `RateLimited` code reads
/// `{"code":"RateLimited","message":"too many attach requests"}` as
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
    /// The request was read, and this peer could not carry it out. The message
    /// names what stopped it, such as `the binary at /opt/koshi could not be
    /// read: No such file or directory`.
    RequestFailed,
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
    /// The session is replacing its process image and runs no request in
    /// this image. The request did not run. The same request, sent once the
    /// session advertises the endpoint of its new image, runs there.
    Restarting,
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
            process_image_id: ProcessImageId::get_own(),
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
        "SetPanePlacementPreview",
        "ConfirmPanePlacement",
        "RecentEvents",
        "Restart",
        "Leaving",
        "Ping",
        "PaintedUpdate",
        "ClipboardCopyPart",
        "ClipboardCopy",
        "Prepare",
        "Commit",
    ];
}

impl WireName for IpcRequestKind {
    fn get_wire_name(&self) -> &'static str {
        self.get_request_kind_name()
    }
}

impl WireVariants for IpcResult {
    /// Every answer this build has: one entry per variant of [`IpcResult`],
    /// spelled as [`WireName::get_wire_name`] spells it.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Attached",
        "CommandResult",
        "Overview",
        "Layout",
        "RecentEvents",
        "Restarting",
        "Error",
        "ClipboardCopyPartTaken",
        "ClipboardCopyRouted",
        "Prepared",
    ];
}

impl WireName for IpcResult {
    fn get_wire_name(&self) -> &'static str {
        match self {
            IpcResult::Hello { .. } => "Hello",
            IpcResult::Attached { .. } => "Attached",
            IpcResult::CommandResult(_) => "CommandResult",
            IpcResult::Overview(_) => "Overview",
            IpcResult::Layout(_) => "Layout",
            IpcResult::RecentEvents(_) => "RecentEvents",
            IpcResult::Restarting => "Restarting",
            IpcResult::Error(_) => "Error",
            IpcResult::ClipboardCopyPartTaken => "ClipboardCopyPartTaken",
            IpcResult::ClipboardCopyRouted { .. } => "ClipboardCopyRouted",
            IpcResult::Prepared => "Prepared",
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
