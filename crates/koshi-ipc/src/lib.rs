//! `koshi-ipc` — control channel: local socket/named pipe transport, versioned IPC
//! messages, ownership checks, and CLI-to-session command forwarding.

/// The session structure a client receives when it attaches.
pub mod attach;
/// Endpoint file: how a running koshi advertises its socket and token.
pub mod endpoint;
/// Error types.
pub mod error;
/// The events an attached client receives after the attach reply.
pub mod event;
/// One painted frame: the pane content a client draws.
pub mod frame;
/// Connection handshake checks.
pub mod handshake;
pub mod protocol;
/// The control-plane protocol: the messages that create, find, list and kill
/// sessions, the handshake that opens a router connection, and the fixed
/// names the router serves under.
pub mod router;
/// Transport layer.
pub mod transport;
/// Shared types.
pub mod types;
/// Socket-address trust checks and stale-socket reclaim.
pub mod validate;
