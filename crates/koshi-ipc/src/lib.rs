//! `koshi-ipc` — control channel: local socket/named pipe transport, versioned IPC
//! messages, ownership checks, and CLI-to-session command forwarding.

/// Endpoint file: how a running koshi advertises its socket and token.
pub mod endpoint;
/// Error types.
pub mod error;
/// Connection handshake checks.
pub mod handshake;
pub mod protocol;
/// Transport layer.
pub mod transport;
/// Shared types.
pub mod types;
/// Socket-address trust checks and stale-socket reclaim.
pub mod validate;
