//! The in-session identity environment for spawned panes.
//!
//! Every pane's child process receives a set of `KOSHI_*` variables naming
//! the session, client, and pane it lives in, plus the control-socket
//! address of its session — so a `koshi` CLI run inside the pane can tell it
//! is in a koshi session and reach the right socket. [`koshi_env`] builds
//! that set; the spawn paths merge it into the spec's environment overlay
//! right before launch.

use std::collections::BTreeMap;
use std::path::Path;

use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_ipc::endpoint::socket_addr;

/// Build the `KOSHI_*` identity variables for one pane spawn.
///
/// Always present: `KOSHI=1` (the in-session marker), `KOSHI_SESSION_ID`,
/// and `KOSHI_PANE_ID`. Present when known: `KOSHI_CLIENT_ID` (the client
/// designated to view the pane at spawn — a pane created with no designated
/// client carries none) and `KOSHI_SOCKET` (the session's control-socket
/// address, resolved from `runtime_dir` through
/// [`socket_addr`]; a machine with no resolvable runtime directory carries
/// none). Ids render in their prefixed `Display` form
/// (`session-<uuid>`, `client-<uuid>`, `pane-<uuid>`).
///
/// The values are fixed at spawn: a client that detaches later leaves the
/// variable holding the spawn-time id, and the runtime re-validates every id
/// against live state when a CLI presents them.
pub(crate) fn koshi_env(
    session_id: SessionId,
    client_id: Option<ClientId>,
    pane_id: PaneId,
    runtime_dir: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("KOSHI".to_string(), "1".to_string());
    env.insert("KOSHI_SESSION_ID".to_string(), session_id.to_string());
    if let Some(client_id) = client_id {
        env.insert("KOSHI_CLIENT_ID".to_string(), client_id.to_string());
    }
    env.insert("KOSHI_PANE_ID".to_string(), pane_id.to_string());
    if let Some(runtime_dir) = runtime_dir {
        env.insert(
            "KOSHI_SOCKET".to_string(),
            socket_addr(runtime_dir, session_id),
        );
    }
    env
}

#[cfg(test)]
mod tests;
