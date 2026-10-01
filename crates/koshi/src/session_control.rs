//! Process-level session commands served without an attached pane.

use std::path::Path;

use koshi_core::command::{Command, CommandResult, DetachArgs};
use koshi_core::ids::{ClientId, SessionId};

use crate::cli::SessionReference;
use crate::session_end::{end_session, SessionEnding};
use crate::targeting;
use koshi_core::ids::parse_prefixed_uuid;
use koshi_link::discovery::{self, Discovered};
use koshi_link::error::CliError;
use koshi_link::ipc_client;

/// End the session named by `session_reference`, or the only running session when
/// absent, through [`end_session`]. An id goes straight to that session; a name
/// is resolved against every running session first.
///
/// From the start, this process ignores `SIGHUP`, `SIGINT` and `SIGQUIT`, or
/// Ctrl+C on Windows, through
/// [`ignore_terminal_signals`](koshi_host::process_tree::ignore_terminal_signals).
pub fn kill_session(
    session_reference: Option<&SessionReference>,
) -> Result<SessionEnding, CliError> {
    koshi_host::process_tree::ignore_terminal_signals();
    kill_session_in_runtime_directory(
        &ipc_client::resolve_runtime_directory()?,
        ipc_client::resolve_shared_sessions_base_directory().as_deref(),
        session_reference,
    )
}

/// [`kill_session`] against an explicit runtime directory, with
/// `shared_sessions_base_directory` naming where other users' sessions are
/// looked up.
fn kill_session_in_runtime_directory(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_reference: Option<&SessionReference>,
) -> Result<SessionEnding, CliError> {
    let session_id = resolve_session_id_from_reference(
        runtime_directory,
        shared_sessions_base_directory,
        session_reference,
    )?;
    end_session(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
    )
}

/// Resolve the session a `kill-session` or `detach --all` argument names: an id is
/// taken as it stands, and a name or an absent argument is resolved against
/// every running session by [`resolve_discovered_session_id`].
fn resolve_session_id_from_reference(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_reference: Option<&SessionReference>,
) -> Result<SessionId, CliError> {
    let session_name = match session_reference {
        Some(SessionReference::SessionId(session_id)) => return Ok(*session_id),
        Some(SessionReference::SessionName(session_name)) => Some(session_name.as_str()),
        None => None,
    };
    resolve_discovered_session_id(
        &discovery::fetch_all_session_overviews(runtime_directory, shared_sessions_base_directory),
        session_name,
    )
}

/// Pick the named session, or apply the sole-running-session rule.
fn resolve_discovered_session_id(
    discovered_sessions: &Discovered,
    session_name: Option<&str>,
) -> Result<SessionId, CliError> {
    let session_overview = match session_name {
        Some(session_name) => targeting::find_session_by_name(discovered_sessions, session_name)?,
        None => targeting::select_only_session(
            discovered_sessions,
            "several sessions are running; name one: koshi kill-session <name>",
            "cannot tell which session to kill; name one: koshi kill-session <name>",
        )?,
    };
    Ok(session_overview.session.session_id)
}

/// Detach the client named by the `koshi detach` argument, leaving the session
/// running and its panes untouched.
///
/// The target text is a client id, a session id, or a session display name. A target
/// that names a session rather than a client leaves the choice to that
/// session, which detaches its only attached client and lists the attached ids
/// when there are several.
pub fn detach_client_or_session(detach_target_text: &str) -> Result<CommandResult, CliError> {
    detach_client_or_session_in_runtime_directory(
        &ipc_client::resolve_runtime_directory()?,
        ipc_client::resolve_shared_sessions_base_directory().as_deref(),
        detach_target_text,
    )
}

/// [`detach_client_or_session`] against an explicit runtime directory, with
/// `shared_sessions_base_directory` naming where other users' sessions are
/// looked up.
fn detach_client_or_session_in_runtime_directory(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    detach_target_text: &str,
) -> Result<CommandResult, CliError> {
    let (session_id, client_id) = resolve_detach_target(
        runtime_directory,
        shared_sessions_base_directory,
        detach_target_text,
    )?;
    ipc_client::submit_external_command_via_runtime_directory(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        Command::Detach(DetachArgs { client_id }),
    )
}

/// The session to ask and the client it detaches, for the target text typed after
/// `koshi detach`.
///
/// A `session-<uuid>` id names a session and goes straight there, asking no
/// session to describe itself. Anything else is read against the running
/// sessions: a `client-<uuid>` id, or a bare UUID that an answering session reports
/// as an attached client, names that client and the session holding it; a bare
/// UUID no attached client carries is read as a session id instead; any other
/// target text is a session display name, resolved the way `kill-session` resolves
/// one. A resolved session with no named client is returned as `None`, so the
/// session itself picks the client.
fn resolve_detach_target(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    detach_target_text: &str,
) -> Result<(SessionId, Option<ClientId>), CliError> {
    if detach_target_text.starts_with("session-") {
        let target_uuid =
            parse_prefixed_uuid(detach_target_text, "session").map_err(|parse_error_detail| {
                CliError::InvalidArgs {
                    detail: parse_error_detail,
                }
            })?;
        return Ok((SessionId::from_uuid(target_uuid), None));
    }

    let discovered_sessions =
        discovery::fetch_all_session_overviews(runtime_directory, shared_sessions_base_directory);
    let Ok(target_uuid) = parse_prefixed_uuid(detach_target_text, "client") else {
        return Ok((
            resolve_discovered_session_id(&discovered_sessions, Some(detach_target_text))?,
            None,
        ));
    };
    let client_id = ClientId::from_uuid(target_uuid);
    match find_session_for_client(&discovered_sessions, client_id) {
        Ok(session_id) => Ok((session_id, Some(client_id))),
        // A `client-` id is a client and nothing else.
        Err(client_target_error) if detach_target_text.starts_with("client-") => {
            Err(client_target_error)
        }
        Err(client_target_error) => {
            let session_id = SessionId::from_uuid(target_uuid);
            let has_session = discovered_sessions
                .sessions
                .iter()
                .any(|session_overview| session_overview.session.session_id == session_id);
            if has_session || discovered_sessions.is_complete() {
                Ok((session_id, None))
            } else {
                Err(client_target_error)
            }
        }
    }
}

/// The session that reports `client_id` among its attached clients.
fn find_session_for_client(
    discovered_sessions: &Discovered,
    client_id: ClientId,
) -> Result<SessionId, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .find(|session_overview| {
            session_overview
                .clients
                .iter()
                .any(|attached_client| attached_client.client_id == client_id)
        })
        .map(|session_overview| session_overview.session.session_id)
        .ok_or_else(|| {
            discovered_sessions.build_missing_target_error("client", &client_id.to_string())
        })
}

/// Detach every client attached to the session named by `session_reference`, or to the
/// only running session when absent. The session keeps running and its panes
/// are untouched.
///
/// An id goes straight to that session; a name is resolved against every
/// running session first.
pub fn detach_all_session(
    session_reference: Option<&SessionReference>,
) -> Result<CommandResult, CliError> {
    detach_all_session_in_runtime_directory(
        &ipc_client::resolve_runtime_directory()?,
        ipc_client::resolve_shared_sessions_base_directory().as_deref(),
        session_reference,
    )
}

/// [`detach_all_session`] against an explicit runtime directory, with
/// `shared_sessions_base_directory` naming where other users' sessions are
/// looked up.
fn detach_all_session_in_runtime_directory(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_reference: Option<&SessionReference>,
) -> Result<CommandResult, CliError> {
    let session_id = resolve_session_id_from_reference(
        runtime_directory,
        shared_sessions_base_directory,
        session_reference,
    )?;
    ipc_client::submit_external_command_via_runtime_directory(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        Command::DetachAll,
    )
}

#[cfg(test)]
mod tests;
