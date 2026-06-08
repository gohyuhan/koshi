//! `tile-ipc` — control channel: local socket/named pipe transport, versioned IPC
//! messages, ownership checks, and CLI-to-session command forwarding.

pub mod error;
pub mod types;

pub mod transport;
