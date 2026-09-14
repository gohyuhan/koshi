//! The in-session identity environment for spawned panes.
//!
//! Every pane's child process receives a set of `KOSHI_*` variables naming
//! the session, client, and pane it lives in, plus the control-socket
//! address of its session. A `koshi` CLI run inside the pane reads them to
//! tell it is in a koshi session and to reach that session's socket.
//! [`build_koshi_environment`] builds that set; the spawn paths merge it into the spec's
//! environment overlay right before launch.

use std::collections::BTreeMap;
use std::path::Path;

use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_ipc::endpoint::compute_socket_address;

/// Build the `KOSHI_*` identity variables for one pane spawn.
///
/// Always present: `KOSHI=1` (the in-session marker), `KOSHI_SESSION_ID`,
/// and `KOSHI_PANE_ID`. Present when known: `KOSHI_CLIENT_ID` (the client
/// designated to view the pane at spawn — a pane created with no designated
/// client carries none) and `KOSHI_SOCKET` (the session's control-socket
/// address, resolved from `runtime_directory` through
/// [`compute_socket_address`]; a machine with no resolvable runtime directory carries
/// none). Ids render in their prefixed `Display` form
/// (`session-<uuid>`, `client-<uuid>`, `pane-<uuid>`).
///
/// The values are fixed at spawn: a client that detaches afterwards leaves the
/// variable holding the spawn-time id.
pub(crate) fn build_koshi_environment(
    session_id: SessionId,
    client_id: Option<ClientId>,
    pane_id: PaneId,
    runtime_directory: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut environment_by_name = BTreeMap::new();
    environment_by_name.insert("KOSHI".to_string(), "1".to_string());
    environment_by_name.insert("KOSHI_SESSION_ID".to_string(), session_id.to_string());
    if let Some(client_id) = client_id {
        environment_by_name.insert("KOSHI_CLIENT_ID".to_string(), client_id.to_string());
    }
    environment_by_name.insert("KOSHI_PANE_ID".to_string(), pane_id.to_string());
    if let Some(runtime_directory) = runtime_directory {
        environment_by_name.insert(
            "KOSHI_SOCKET".to_string(),
            compute_socket_address(runtime_directory, session_id),
        );
    }
    environment_by_name
}

#[cfg(test)]
mod tests;
