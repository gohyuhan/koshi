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

use koshi_core::discovery::SessionInfo;
use koshi_core::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::protocol::{agreed_version, ConnectionToken, IpcErrorCode, IpcErrorPayload};
use crate::wire::{MaybeKnown, WireName, WireVariants};

/// The highest control-plane protocol version this build speaks, and the one
/// it uses when the peer speaks it too.
///
/// Bumps once per release cycle, in the commit that first changes a wire shape
/// after a release — not once per change. Version 1 is what 0.2.0 speaks;
/// version 2 adds [`RouterRequestKind::Restart`] and its
/// [`RouterResult::Restarting`] answer.
pub const ROUTER_PROTOCOL_VERSION: u32 = 2;

/// The lowest control-plane protocol version this build speaks. A peer whose
/// highest is below this one is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
///
/// The floor is 1, the version 0.2.0 speaks, because the router is born in
/// 0.2.0 and no earlier build has one.
///
/// Raising this floor drops support for every build below it, so it moves
/// only on a stated decision to end that support.
pub const MIN_ROUTER_PROTOCOL_VERSION: u32 = 1;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterRequest<K = RouterRequestKind> {
    /// Caller-chosen id, repeated in the response that answers this request.
    /// Unique among the requests in flight on one connection.
    pub request_id: u64,
    /// What is being asked.
    pub kind: K,
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterResponse<R = RouterResult> {
    /// The `request_id` of the request being answered, or `None` when the
    /// bytes received were too malformed to read one.
    pub request_id: Option<u64>,
    /// The answer itself.
    pub result: R,
}

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
    /// Answers [`RouterRequestKind::Hello`]: the connection is open, because
    /// the ranges overlap and the token matched.
    Hello {
        /// The version both sides use on this connection: the highest they
        /// both speak.
        protocol_version: u32,
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

/// One router connection's handshake gate, held by the router for the
/// connection's lifetime. Starts closed; a [`RouterRequestKind::Hello`] whose
/// version range overlaps this build's and whose token matches opens it, and
/// every other request kind is served only while it is open.
#[derive(Debug)]
pub struct RouterHandshake {
    /// The token the router wrote to its endpoint file; a Hello must present
    /// an equal one.
    expected: ConnectionToken,
    /// The control-plane protocol version settled for this connection, once a
    /// Hello has been accepted on it.
    agreed: Option<u32>,
}

impl RouterHandshake {
    /// A gate for one newly accepted router connection, closed until a Hello
    /// opens it.
    #[must_use]
    pub fn new(expected: ConnectionToken) -> RouterHandshake {
        RouterHandshake {
            expected,
            agreed: None,
        }
    }

    /// The control-plane protocol version this connection settled on, or
    /// `None` while no Hello has been accepted.
    ///
    /// The router puts it in [`RouterResult::Hello`], so the caller learns
    /// which version the two of them use.
    #[must_use]
    pub fn agreed(&self) -> Option<u32> {
        self.agreed
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers [`HelloRequired`](IpcErrorCode::HelloRequired),
    /// the same as any other kind arriving before a Hello. An open gate
    /// answers [`UnsupportedKind`](IpcErrorCode::UnsupportedKind) naming it,
    /// and the connection keeps serving.
    #[must_use]
    pub fn refuse_unknown(&self, name: &str) -> IpcErrorPayload {
        if self.agreed.is_none() {
            return IpcErrorPayload {
                code: IpcErrorCode::HelloRequired,
                message: format!("{name} arrived before a Hello opened the connection"),
            };
        }
        IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: format!("this router has no request kind named {name}"),
        }
    }

    /// Check one incoming request kind against the connection's state.
    ///
    /// A [`Hello`](RouterRequestKind::Hello) is checked version first, then
    /// token: a caller whose version range does not overlap
    /// [`MIN_ROUTER_PROTOCOL_VERSION`]`..=`[`ROUTER_PROTOCOL_VERSION`] is
    /// refused as [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion)
    /// with both ranges named, a token that does not equal the router's is
    /// refused as [`BadToken`](IpcErrorCode::BadToken), and a Hello passing
    /// both checks settles the connection's version and opens the gate. Any
    /// other kind is accepted while the gate is open and refused as
    /// [`HelloRequired`](IpcErrorCode::HelloRequired) while it is not.
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
            } => {
                let Some(agreed) = agreed_version(
                    *min_protocol_version,
                    *max_protocol_version,
                    MIN_ROUTER_PROTOCOL_VERSION,
                    ROUTER_PROTOCOL_VERSION,
                ) else {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::UnsupportedVersion,
                        message: format!(
                            "the caller speaks control-plane protocol versions \
                             {min_protocol_version} to {max_protocol_version}, \
                             this router speaks {MIN_ROUTER_PROTOCOL_VERSION} to \
                             {ROUTER_PROTOCOL_VERSION}"
                        ),
                    });
                };
                if *token != self.expected {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: "the token presented does not match the router's".to_string(),
                    });
                }
                self.agreed = Some(agreed);
                Ok(())
            }
            other => {
                if self.agreed.is_some() {
                    Ok(())
                } else {
                    Err(IpcErrorPayload {
                        code: IpcErrorCode::HelloRequired,
                        message: format!(
                            "{} arrived before a Hello opened the connection",
                            other.name()
                        ),
                    })
                }
            }
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
            RouterResult::Error(_) => "Error",
        }
    }
}

#[cfg(test)]
mod tests;
