//! The control-plane protocol: how a client asks the router to create, find,
//! list, or kill sessions.
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
//! [`RouterHandshake`](crate::router::RouterHandshake). The version it names
//! is [`ROUTER_PROTOCOL_VERSION`](crate::router::ROUTER_PROTOCOL_VERSION),
//! which counts separately from the session protocol's
//! [`PROTOCOL_VERSION`](crate::protocol::PROTOCOL_VERSION).
//!
//! A session server is a router client too, so a command issued inside one
//! session that targets another travels the same way.

use std::path::{Path, PathBuf};

use koshi_core::discovery::SessionInfo;
use koshi_core::ids::SessionId;
use serde::{Deserialize, Serialize};

use crate::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};

/// The control-plane protocol version this build speaks. A connection whose
/// [`RouterRequestKind::Hello`] names a different version is refused with
/// [`IpcErrorCode::UnsupportedVersion`].
///
/// Bumps once per release cycle, in the commit that first changes a wire shape
/// after a release — not once per change. This protocol was born in 0.2.0 and
/// has never shipped, so it stays 1 until 0.2.0 is out.
pub const ROUTER_PROTOCOL_VERSION: u32 = 1;

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
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterRequest {
    /// Caller-chosen id, repeated in the response that answers this request.
    /// Unique among the requests in flight on one connection.
    pub request_id: u64,
    /// What is being asked.
    pub kind: RouterRequestKind,
}

/// What a control-plane request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RouterRequestKind {
    /// Opens the connection: names the control-plane protocol version the
    /// caller speaks and presents the token read from the router's endpoint
    /// file. Sent before any other kind.
    ///
    /// Sending it again on an open connection is allowed and changes nothing:
    /// the version and token are checked again and the same answer comes
    /// back, since checking them alters no state.
    Hello {
        /// The control-plane protocol version the caller speaks.
        protocol_version: u32,
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
    },
    /// Look up a running session's control-socket address, so the caller can
    /// connect to that session directly.
    AttachLookup {
        /// Which session to look up.
        selector: SessionSelector,
    },
    /// List the running sessions.
    ListSessions,
}

impl RouterRequestKind {
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
        }
    }
}

/// One message answering a [`RouterRequest`].
///
/// Decoding rejects any field it does not know. An absent `request_id` means
/// the request could not be read, so a misspelled one is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterResponse {
    /// The `request_id` of the request being answered, or `None` when the
    /// bytes received were too malformed to read one.
    pub request_id: Option<u64>,
    /// The answer itself.
    pub result: RouterResult,
}

/// Where one running session can be reached.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouterResult {
    /// Answers [`RouterRequestKind::Hello`]: the connection is open, because
    /// the versions agree and the token matched.
    Hello,
    /// Answers [`RouterRequestKind::CreateSession`]: where the new session
    /// listens.
    Created(SessionAddress),
    /// Answers [`RouterRequestKind::AttachLookup`]: where the named session
    /// listens.
    Found(SessionAddress),
    /// Answers [`RouterRequestKind::ListSessions`]: one record per running
    /// session.
    Sessions(Vec<SessionInfo>),
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// The one JSON line a session server prints on standard output once its
/// control socket is bound and the router may hand callers its address.
///
/// Decoding rejects any field it does not know, so a misspelled name is an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionServerReady {
    /// The control-plane protocol version the session server speaks.
    pub protocol_version: u32,
    /// The control-socket address the session server bound: a socket-file
    /// path on Unix, a bare pipe name on Windows.
    pub socket: String,
}

/// One router connection's handshake gate, held by the router for the
/// connection's lifetime. Starts closed; a [`RouterRequestKind::Hello`]
/// carrying the right control-plane protocol version and token opens it, and
/// every other request kind is served only while it is open.
#[derive(Debug)]
pub struct RouterHandshake {
    /// The token the router wrote to its endpoint file; a Hello must present
    /// an equal one.
    expected: ConnectionToken,
    /// True once a Hello has been accepted on this connection.
    open: bool,
}

impl RouterHandshake {
    /// A gate for one newly accepted router connection, closed until a Hello
    /// opens it.
    #[must_use]
    pub fn new(expected: ConnectionToken) -> RouterHandshake {
        RouterHandshake {
            expected,
            open: false,
        }
    }

    /// Check one incoming request kind against the connection's state.
    ///
    /// A [`Hello`](RouterRequestKind::Hello) is checked version first, then
    /// token: a version other than [`ROUTER_PROTOCOL_VERSION`] is refused as
    /// [`UnsupportedVersion`](IpcErrorCode::UnsupportedVersion) with both
    /// versions named, a token that does not equal the router's is refused as
    /// [`BadToken`](IpcErrorCode::BadToken), and a Hello passing both checks
    /// opens the gate. Any other kind is accepted while the gate is open and
    /// refused as [`HelloRequired`](IpcErrorCode::HelloRequired) while it is
    /// not.
    ///
    /// `Ok(())` means the caller serves the request — a Hello is answered
    /// with [`RouterResult::Hello`]. An `Err` carries the refusal to send
    /// back, and the gate keeps the state it had.
    pub fn check(&mut self, kind: &RouterRequestKind) -> Result<(), IpcErrorPayload> {
        match kind {
            RouterRequestKind::Hello {
                protocol_version,
                token,
            } => {
                if *protocol_version != ROUTER_PROTOCOL_VERSION {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::UnsupportedVersion,
                        message: format!(
                            "the caller speaks control-plane protocol version \
                             {protocol_version}, this router speaks \
                             {ROUTER_PROTOCOL_VERSION}"
                        ),
                    });
                }
                if *token != self.expected {
                    return Err(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: "the token presented does not match the router's".to_string(),
                    });
                }
                self.open = true;
                Ok(())
            }
            other => {
                if self.open {
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

#[cfg(test)]
mod tests;
