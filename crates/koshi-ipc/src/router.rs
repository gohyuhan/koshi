//! The control-plane protocol: how a client asks the router to create, find,
//! or list sessions, and to restart the router itself.
//!
//! The router is one process per user. It owns the list of running sessions
//! and nothing else: a caller asks it for a session's control-socket address,
//! then connects to that session directly, so no pane traffic passes through
//! the router. An exchange is one
//! [`RouterRequest`](crate::router::RouterRequest) and the
//! [`RouterResponse`](crate::router::RouterResponse) answering it, framed by
//! [`transport`](crate::transport) exactly like the session protocol.
//!
//! Every connection opens with
//! [`Hello`](crate::router::RouterRequestKind::Hello), checked by
//! [`RouterHandshake`](crate::router::RouterHandshake). It names the range of
//! versions the caller speaks, and the two sides settle on the highest they
//! both have. That range is
//! [`MIN_ROUTER_PROTOCOL_VERSION`](crate::router::MIN_ROUTER_PROTOCOL_VERSION)
//! to [`ROUTER_PROTOCOL_VERSION`](crate::router::ROUTER_PROTOCOL_VERSION),
//! which counts separately from the session protocol's
//! [`PROTOCOL_VERSION`](crate::protocol::PROTOCOL_VERSION).
//!
//! A session server is a router client too, so a command issued inside one
//! session that targets another travels the same way.

use std::path::{Path, PathBuf};
use std::time::Duration;

use koshi_core::compat::CONTROL_PROTOCOL;
use koshi_core::discovery::SessionInfo;
use koshi_core::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::handshake::{GateWords, VersionGate};
use crate::protocol::{ConnectionToken, IpcErrorPayload};
use crate::remote_tokens::{TokenEntry, TokenScope};
use crate::wire::{Answer, Envelope, MaybeKnown, WireName, WireVariants};

/// The highest control-plane protocol version this build speaks, and the one
/// it uses when the peer speaks it too.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::CONTROL_PROTOCOL`].
///
/// Version 1 is what 0.2.0 speaks. Version 2 refuses a session the router does
/// not have with [`NotFound`](crate::protocol::IpcErrorCode::NotFound), where
/// version 1 sent
/// [`MalformedRequest`](crate::protocol::IpcErrorCode::MalformedRequest).
pub const ROUTER_PROTOCOL_VERSION: u32 = CONTROL_PROTOCOL.max;

/// The lowest control-plane protocol version this build speaks. A peer whose
/// highest is below this one is refused with
/// [`UnsupportedVersion`](crate::protocol::IpcErrorCode::UnsupportedVersion).
///
/// The floor is 1, the version 0.2.0 speaks. No build before 0.2.0 has a
/// router.
///
/// Raising this floor drops support for every build below it.
pub const MIN_ROUTER_PROTOCOL_VERSION: u32 = CONTROL_PROTOCOL.min;

/// Which session a request means: the id, or the generated display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSelector {
    /// The session's stable id.
    Id(SessionId),
    /// The session's generated display name, e.g. `"quiet-lake"`.
    Name(String),
}

/// One message from a caller to the router.
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know, so a misspelled `request_id` is an error.
///
/// `K` is the request kind. A sender uses `RouterRequest`, where `K` is
/// [`RouterRequestKind`]. The router uses [`IncomingRouterRequest`], where a
/// kind this build does not have arrives as [`MaybeKnown::Unknown`].
pub type RouterRequest<K = RouterRequestKind> = Envelope<K>;

/// A control-plane request as the router reads it: the kind may name something
/// this build does not have.
pub type IncomingRouterRequest = RouterRequest<MaybeKnown<RouterRequestKind>>;

/// What a control-plane request asks for.
///
/// A field this build does not know is ignored, so a peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouterRequestKind {
    /// Opens the connection: names the control-plane protocol versions the
    /// caller speaks and presents the token read from the router's endpoint
    /// file. Sent before any other kind.
    ///
    /// The two versions are a range, lowest and highest. The router answers
    /// with the one both sides use, or refuses when the ranges do not overlap.
    ///
    /// Sending it again on an open connection is allowed and changes nothing:
    /// the versions and token are checked again and the same answer comes
    /// back, since checking them alters no state.
    Hello {
        /// The lowest control-plane protocol version the caller speaks.
        min_protocol_version: u32,
        /// The highest control-plane protocol version the caller speaks.
        max_protocol_version: u32,
        /// The secret read from the router's endpoint file.
        token: ConnectionToken,
    },
    /// Start a new session. The router picks the id and the name, spawns the
    /// session server, and answers once that server's socket is bound.
    CreateSession {
        /// The `--profile` name the new session opens, or `None` for one shell.
        profile: Option<String>,
        /// The directory the caller ran in. The session's first shell opens
        /// here; `None` leaves the session server in the directory it
        /// inherited.
        cwd: Option<PathBuf>,
        /// `Some(true)` lets the other users of this machine reach the new
        /// session, whatever that session's `koshi.kdl` says. Any other value
        /// leaves the answer to the file.
        allow_other_users: Option<bool>,
    },
    /// Look up a running session's control-socket address, so the caller can
    /// connect to that session directly.
    AttachLookup {
        /// Which session to look up.
        selector: SessionSelector,
    },
    /// List the running sessions.
    ListSessions,
    /// Restart the router: it sends its answer, then restarts into the binary
    /// at the path it started from. The session list is rebuilt from the
    /// endpoint files, so every running session stays registered.
    Restart,
    /// Hand `identity` a fresh remote access secret on `scope`. A grant that
    /// identity already holds on that same scope stops working.
    ///
    /// The router reads the clock once and stamps both the issue time and the
    /// expiry time from that one reading. A span the clock cannot represent is
    /// refused.
    GrantToken {
        /// Who the grant is handed to, in the words the operator typed.
        identity: String,
        /// How far the grant reaches.
        scope: TokenScope,
        /// How long the token works, counted from the issue time, or `None`
        /// when it never stops working.
        expires_in: Option<Duration>,
    },
    /// Stop the remote access grants `identity` holds.
    RevokeToken {
        /// Whose grants stop working.
        identity: String,
        /// The one grant that stops working, or `None` to stop every grant
        /// the identity holds.
        scope: Option<TokenScope>,
    },
    /// List the remote access grants this machine has made.
    ListTokens {
        /// The scope to list the reaching grants of, or `None` to list every
        /// grant on this machine. A session scope lists the host-wide grants
        /// beside the grants scoped to that session; a host-wide scope lists
        /// the host-wide grants alone.
        scope: Option<TokenScope>,
    },
    /// Report where this machine would serve remote clients, and whether the
    /// operator has switched remote access on.
    RemoteStatus,
    /// Switch remote access on: generate this machine's certificate when it
    /// has none, open the listener, and record the operator's answer so the
    /// listener opens on every start after this one.
    EnableRemote,
}

impl RouterRequestKind {
    /// The Hello this build opens a router connection with: the control-plane
    /// versions it speaks, lowest first, and `token` read from the router's
    /// endpoint file.
    ///
    /// Every caller builds its Hello here, so the two version fields are
    /// filled in one place.
    #[must_use]
    pub fn hello(token: ConnectionToken) -> RouterRequestKind {
        RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            token,
        }
    }

    /// The kind's name, e.g. `"CreateSession"`. Carries no payload, so it is
    /// safe on a log line even though a payload can hold the connection
    /// token.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            RouterRequestKind::Hello { .. } => "Hello",
            RouterRequestKind::CreateSession { .. } => "CreateSession",
            RouterRequestKind::AttachLookup { .. } => "AttachLookup",
            RouterRequestKind::ListSessions => "ListSessions",
            RouterRequestKind::Restart => "Restart",
            RouterRequestKind::GrantToken { .. } => "GrantToken",
            RouterRequestKind::RevokeToken { .. } => "RevokeToken",
            RouterRequestKind::ListTokens { .. } => "ListTokens",
            RouterRequestKind::RemoteStatus => "RemoteStatus",
            RouterRequestKind::EnableRemote => "EnableRemote",
        }
    }
}

/// One message answering a [`RouterRequest`].
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know. An absent `request_id` means the request could not be read, so a
/// misspelled one is an error.
///
/// `R` is the answer. The router uses `RouterResponse`, where `R` is
/// [`RouterResult`]. A caller uses [`IncomingRouterResponse`], where a result
/// this build does not have arrives as [`MaybeKnown::Unknown`].
pub type RouterResponse<R = RouterResult> = Answer<R>;

/// A control-plane response as a caller reads it: the result may name
/// something this build does not have.
pub type IncomingRouterResponse = RouterResponse<MaybeKnown<RouterResult>>;

/// Where one running session can be reached.
///
/// A field this build does not know is ignored, so a record from a newer
/// router still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAddress {
    /// The session's stable id.
    pub id: SessionId,
    /// The session's generated display name.
    pub name: String,
    /// The session's control-socket address: a socket-file path on Unix, a
    /// bare pipe name on Windows — the string
    /// [`Connection::connect`](crate::transport::Connection::connect) takes.
    pub socket: String,
    /// The process id of the session server serving that socket.
    pub pid: u32,
}

/// The answer to a control-plane request.
///
/// A field this build does not know is ignored, so a peer that adds one still
/// decodes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouterResult {
    /// Answers [`RouterRequestKind::Hello`]: the ranges overlap and the token
    /// matched, so the connection is open.
    Hello {
        /// The version both sides use on this connection: the highest they
        /// both speak.
        protocol_version: u32,
        /// The build version of the answering router, e.g. `0.3.0`. Empty
        /// when the router predates this field.
        #[serde(default)]
        version: String,
    },
    /// Answers [`RouterRequestKind::CreateSession`]: where the new session
    /// listens.
    Created(SessionAddress),
    /// Answers [`RouterRequestKind::AttachLookup`]: where the named session
    /// listens.
    Found(SessionAddress),
    /// Answers [`RouterRequestKind::ListSessions`]: one record per running
    /// session.
    Sessions(Vec<SessionInfo>),
    /// Answers [`RouterRequestKind::Restart`]: the reply is sent, then the
    /// router restarts into the binary now on disk.
    Restarting,
    /// Answers [`RouterRequestKind::GrantToken`]: the grant is made.
    Granted {
        /// The secret the caller shows the operator once. `ConnectionToken`'s
        /// `Debug` and `Display` write it redacted.
        token: ConnectionToken,
        /// Whether a grant the identity already held on this scope stopped
        /// working.
        replaced: bool,
    },
    /// Answers [`RouterRequestKind::RevokeToken`]: the scope of every grant
    /// this call stopped, empty when the identity held none.
    Revoked(Vec<TokenScope>),
    /// Answers [`RouterRequestKind::ListTokens`]: one entry per grant,
    /// narrowed by the request's scope.
    Tokens(Vec<TokenEntry>),
    /// Answers [`RouterRequestKind::RemoteStatus`]: what this machine's
    /// remote access is set to.
    RemoteStatus {
        /// Where remote clients would be served, as `host:port`, or `None`
        /// when `koshi.kdl` names no listen address.
        address: Option<String>,
        /// Whether the operator has switched remote access on. This is the
        /// answer they gave, which outlives any one run.
        enabled: bool,
        /// Whether this router is holding the port right now. `enabled` with
        /// this `false` means the answer was given and the port could not be
        /// taken this start — something else is on the address.
        listening: bool,
        /// The fingerprint of this machine's certificate, as 64 lowercase
        /// hex characters, or `None` when no certificate has been generated.
        fingerprint: Option<String>,
        /// How many connections from another machine this router holds
        /// admitted right now, whether they have attached to a session or
        /// not. `Some(0)` is a router holding none; `None` is a router whose
        /// build reports no count at all.
        #[serde(default)]
        remote_connections: Option<usize>,
    },
    /// Answers [`RouterRequestKind::EnableRemote`]: remote access is on.
    RemoteEnabled {
        /// Where remote clients are served, as `host:port`.
        address: String,
        /// The fingerprint of this machine's certificate, as 64 lowercase
        /// hex characters. The dialling side pins it.
        fingerprint: String,
    },
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// The one JSON line a session server prints on standard output once its
/// control socket is bound and the router may hand callers its address.
///
/// A field this build does not know is ignored, so a line from a newer session
/// server still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionServerReady {
    /// The control-plane protocol version the session server speaks.
    pub protocol_version: u32,
    /// The control-socket address the session server bound: a socket-file
    /// path on Unix, a bare pipe name on Windows.
    pub socket: String,
}

/// What the control-plane protocol's gate calls itself, and the versions it
/// speaks.
const ROUTER_WORDS: GateWords = GateWords {
    peer: "router",
    token_owner: "the router's",
    caller: "caller",
    versions: "control-plane protocol versions",
    channel: "connection",
    min_version: MIN_ROUTER_PROTOCOL_VERSION,
    max_version: ROUTER_PROTOCOL_VERSION,
};

/// One router connection's handshake gate, held by the router for the
/// connection's lifetime. Starts closed; a [`RouterRequestKind::Hello`] whose
/// version range overlaps this build's and whose token matches opens it, and
/// every other request kind is served only while it is open. The check itself
/// is the gate every koshi protocol shares.
#[derive(Debug)]
pub struct RouterHandshake(VersionGate);

impl RouterHandshake {
    /// A gate for one newly accepted router connection, closed until a Hello
    /// opens it.
    #[must_use]
    pub fn new(expected: ConnectionToken) -> RouterHandshake {
        RouterHandshake(VersionGate::new(expected, ROUTER_WORDS))
    }

    /// The control-plane protocol version this connection settled on, or
    /// `None` while no Hello has been accepted.
    ///
    /// The router puts it in [`RouterResult::Hello`], so the caller learns
    /// which version the two of them use.
    #[must_use]
    pub fn agreed(&self) -> Option<u32> {
        self.0.agreed()
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers
    /// [`HelloRequired`](crate::protocol::IpcErrorCode::HelloRequired), the
    /// same as any other kind arriving before a Hello. An open gate answers
    /// [`UnsupportedKind`](crate::protocol::IpcErrorCode::UnsupportedKind)
    /// naming it, and the connection keeps serving.
    #[must_use]
    pub fn refuse_unknown(&self, name: &str) -> IpcErrorPayload {
        self.0.refuse_unknown(name)
    }

    /// Check one incoming request kind against the connection's state.
    ///
    /// A [`Hello`](RouterRequestKind::Hello) is checked version first, then
    /// token: a caller whose version range does not overlap
    /// [`MIN_ROUTER_PROTOCOL_VERSION`]`..=`[`ROUTER_PROTOCOL_VERSION`] is
    /// refused as
    /// [`UnsupportedVersion`](crate::protocol::IpcErrorCode::UnsupportedVersion)
    /// with both ranges named, a token that does not equal the router's is
    /// refused as [`BadToken`](crate::protocol::IpcErrorCode::BadToken), and a
    /// Hello passing both checks settles the connection's version and opens the
    /// gate. Any other kind is accepted while the gate is open and refused as
    /// [`HelloRequired`](crate::protocol::IpcErrorCode::HelloRequired) while it
    /// is not.
    ///
    /// `Ok(())` means the caller serves the request — a Hello is answered
    /// with [`RouterResult::Hello`] carrying [`agreed`](Self::agreed). An
    /// `Err` carries the refusal to send back, and the gate keeps the state it
    /// had.
    pub fn check(&mut self, kind: &RouterRequestKind) -> Result<(), IpcErrorPayload> {
        match kind {
            RouterRequestKind::Hello {
                min_protocol_version,
                max_protocol_version,
                token,
            } => self
                .0
                .hello(*min_protocol_version, *max_protocol_version, token),
            other => self.0.other(other.name()),
        }
    }
}

/// The control-socket address of the router serving `runtime_dir`: the string
/// [`Connection::connect`](crate::transport::Connection::connect) takes and
/// the router's [`EndpointFile`](crate::endpoint::EndpointFile) carries. One
/// runtime directory has one address, and two runtime directories on one
/// machine have different ones.
///
/// On Unix this is `router.sock` directly inside `runtime_dir` — the location
/// [`validate_socket_addr`](crate::validate::validate_socket_addr) accepts.
/// On Windows a pipe has no filesystem path, so the name carries the
/// directory instead: `koshi-router-<hash of runtime_dir>`, inside the
/// `koshi-` namespace that same check requires.
///
/// Callers resolve `runtime_dir` through `koshi_paths::runtime_dir()`.
#[must_use]
pub fn router_socket_addr(runtime_dir: &Path) -> String {
    #[cfg(unix)]
    {
        runtime_dir.join("router.sock").display().to_string()
    }
    #[cfg(windows)]
    {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        runtime_dir.hash(&mut hasher);
        format!("koshi-router-{:016x}", hasher.finish())
    }
}

/// Where the router's endpoint file lives: `router.json` directly inside
/// `runtime_dir`. It names the router's socket and carries the token a
/// connection presents at Hello.
///
/// Callers resolve `runtime_dir` through `koshi_paths::runtime_dir()`.
#[must_use]
pub fn router_endpoint_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("router.json")
}

/// Where the router's lock file lives: `router.lock` directly inside
/// `runtime_dir`. Holding the advisory lock on that file is what makes one
/// router the only router.
///
/// Callers resolve `runtime_dir` through `koshi_paths::runtime_dir()`.
#[must_use]
pub fn router_lock_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("router.lock")
}

impl WireVariants for RouterRequestKind {
    /// Every control-plane request kind this build has. A kind added to
    /// [`RouterRequestKind`] is added here and to
    /// [`RouterRequestKind::name`] in the same change.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "CreateSession",
        "AttachLookup",
        "ListSessions",
        "Restart",
        "GrantToken",
        "RevokeToken",
        "ListTokens",
        "RemoteStatus",
        "EnableRemote",
    ];
}

impl WireName for RouterRequestKind {
    fn wire_name(&self) -> &'static str {
        self.name()
    }
}

impl WireVariants for RouterResult {
    /// Every control-plane answer this build has. A variant added to
    /// [`RouterResult`] is added here in the same change.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Created",
        "Found",
        "Sessions",
        "Restarting",
        "Granted",
        "Revoked",
        "Tokens",
        "RemoteStatus",
        "RemoteEnabled",
        "Error",
    ];
}

impl WireName for RouterResult {
    fn wire_name(&self) -> &'static str {
        match self {
            RouterResult::Hello { .. } => "Hello",
            RouterResult::Created(_) => "Created",
            RouterResult::Found(_) => "Found",
            RouterResult::Sessions(_) => "Sessions",
            RouterResult::Restarting => "Restarting",
            RouterResult::Granted { .. } => "Granted",
            RouterResult::Revoked(_) => "Revoked",
            RouterResult::Tokens(_) => "Tokens",
            RouterResult::RemoteStatus { .. } => "RemoteStatus",
            RouterResult::RemoteEnabled { .. } => "RemoteEnabled",
            RouterResult::Error(_) => "Error",
        }
    }
}

/// The control plane, as a serve loop sees it: what the router answers on its
/// own socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPlane;

impl crate::plane::Plane for ControlPlane {
    type Kind = RouterRequestKind;
    type Result = RouterResult;
    type Gate = RouterHandshake;

    fn refusal(payload: IpcErrorPayload) -> RouterResult {
        RouterResult::Error(payload)
    }

    fn hello(agreed: u32, build: &str) -> RouterResult {
        RouterResult::Hello {
            protocol_version: agreed,
            version: build.to_string(),
        }
    }
}

impl crate::plane::Gate for RouterHandshake {
    type Kind = RouterRequestKind;

    fn agreed(&self) -> Option<u32> {
        RouterHandshake::agreed(self)
    }

    fn refuse_unknown(&self, name: &str) -> IpcErrorPayload {
        RouterHandshake::refuse_unknown(self, name)
    }

    fn check(&mut self, kind: &RouterRequestKind) -> Result<(), IpcErrorPayload> {
        RouterHandshake::check(self, kind)
    }

    fn is_hello(kind: &RouterRequestKind) -> bool {
        matches!(kind, RouterRequestKind::Hello { .. })
    }
}

#[cfg(test)]
mod tests;
