//! `tile-ipc` — control channel: local socket/named pipe transport, versioned IPC
//! messages, ownership checks, and CLI-to-session command forwarding.

/// Error types.
pub mod error;
/// Transport layer.
pub mod transport;
/// Shared types.
pub mod types;
