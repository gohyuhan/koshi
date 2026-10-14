//! The in-session environment for spawned panes.
//!
//! Every pane's child process receives a set of `KOSHI_*` variables naming
//! the session, client, and pane it lives in, plus the control-socket
//! address of its session. A `koshi` CLI run inside the pane reads them to
//! tell it is in a koshi session and to reach that session's socket.
//! [`add_koshi_environment`] adds that set to the spec's environment overlay
//! right before launch. On Linux and macOS, while `pane-clipboard-writes`
//! allows writes, it also puts the session's folder of stand-in clipboard
//! tools first on the pane's `PATH`.

use std::collections::BTreeMap;
use std::path::Path;

use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_ipc::endpoint::compute_socket_address;
#[cfg(unix)]
use koshi_ipc::endpoint::resolve_clipboard_tool_directory_path;

/// Add the `KOSHI_*` identity variables for one pane spawn to
/// `environment_variables`, the spawn spec's environment overlay.
///
/// Always present: `KOSHI=1` (the in-session marker), `KOSHI_SESSION_ID`,
/// and `KOSHI_PANE_ID`. Present when known: `KOSHI_CLIENT_ID` (the client
/// designated to view the pane at spawn — a pane created with no designated
/// client carries none) and `KOSHI_SOCKET` (the session's control-socket
/// address, resolved from `runtime_directory_path` through
/// [`compute_socket_address`]; a machine with no resolvable runtime directory path carries
/// none). Ids render in their prefixed `Display` form
/// (`session-<uuid>`, `client-<uuid>`, `pane-<uuid>`).
///
/// On Linux and macOS, `should_add_clipboard_tools` set to `true` and a known
/// `runtime_directory_path` also put the session's stand-in clipboard tool
/// folder ([`resolve_clipboard_tool_directory_path`]) first on the `PATH` in
/// `environment_variables`, as [`put_clipboard_tool_directory_first_on_path`]
/// states. On Windows, `should_add_clipboard_tools` changes nothing.
///
/// The values are fixed at spawn: a client that detaches afterwards leaves the
/// variable holding the spawn-time id.
pub(crate) fn add_koshi_environment(
    environment_variables: &mut BTreeMap<String, String>,
    session_id: SessionId,
    client_id: Option<ClientId>,
    pane_id: PaneId,
    runtime_directory_path: Option<&Path>,
    #[cfg_attr(not(unix), allow(unused_variables))] should_add_clipboard_tools: bool,
) {
    environment_variables.insert("KOSHI".to_string(), "1".to_string());
    environment_variables.insert("KOSHI_SESSION_ID".to_string(), session_id.to_string());
    if let Some(client_id) = client_id {
        environment_variables.insert("KOSHI_CLIENT_ID".to_string(), client_id.to_string());
    }
    environment_variables.insert("KOSHI_PANE_ID".to_string(), pane_id.to_string());
    let Some(runtime_directory_path) = runtime_directory_path else {
        return;
    };
    environment_variables.insert(
        "KOSHI_SOCKET".to_string(),
        compute_socket_address(runtime_directory_path, session_id),
    );
    #[cfg(unix)]
    if should_add_clipboard_tools {
        put_clipboard_tool_directory_first_on_path(
            environment_variables,
            &resolve_clipboard_tool_directory_path(runtime_directory_path, session_id),
        );
    }
}

/// Put `clipboard_tool_directory` first on the `PATH` in
/// `environment_variables`: in front of the `PATH` that
/// `environment_variables` holds, else in front of this process's `PATH`.
///
/// `PATH` stays as it is when that `PATH` is not set, is empty, or is not
/// UTF-8, and when `clipboard_tool_directory` is not UTF-8 or holds a `:`.
///
/// Example: `/run/koshi/session-<uuid>.clipboard-tools` and the `PATH`
/// `/usr/bin:/bin` give `/run/koshi/session-<uuid>.clipboard-tools:/usr/bin:/bin`.
#[cfg(unix)]
fn put_clipboard_tool_directory_first_on_path(
    environment_variables: &mut BTreeMap<String, String>,
    clipboard_tool_directory: &Path,
) {
    let Some(clipboard_tool_directory) = clipboard_tool_directory.to_str() else {
        return;
    };
    if clipboard_tool_directory.contains(':') {
        return;
    }
    let base_path = match environment_variables.get("PATH") {
        Some(spec_path) => spec_path.clone(),
        None => match std::env::var("PATH") {
            Ok(server_path) => server_path,
            Err(_) => return,
        },
    };
    if base_path.is_empty() {
        return;
    }
    environment_variables.insert(
        "PATH".to_string(),
        format!("{clipboard_tool_directory}:{base_path}"),
    );
}

#[cfg(test)]
mod tests;
