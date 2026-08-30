//! What a session records about a client that is not its view state.

use serde::{Deserialize, Serialize};

/// Where a client connected from. The server sets it at accept, never from
/// anything the client sends.
///
/// The type has no `Default`: a value that is absent is not
/// [`Local`](ClientOrigin::Local). Carry an absent origin as
/// `Option<ClientOrigin>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientOrigin {
    /// Connected over this machine's own unix socket or named pipe.
    Local,
    /// Connected from another machine.
    Remote,
}
