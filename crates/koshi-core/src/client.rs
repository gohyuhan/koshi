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

/// The number every surface prints for a client whose `color_index` is
/// `color_index`: `color_index + 1`. `0` → `1`, `11` → `12`, `4294967295` →
/// `4294967296`.
#[must_use]
pub fn compute_client_number(color_index: u32) -> u64 {
    u64::from(color_index) + 1
}

#[cfg(test)]
mod tests;
