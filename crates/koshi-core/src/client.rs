//! What a session records about a client that is not its view state.

use serde::{Deserialize, Serialize};

/// Where a client connected from. The server sets it at accept, never from
/// anything the client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ClientOrigin {
    /// Connected over this machine's own unix socket or named pipe.
    #[default]
    Local,
    /// Connected from another machine.
    Remote,
}
