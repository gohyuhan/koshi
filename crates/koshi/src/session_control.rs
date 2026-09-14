//! Process-level session commands served without an attached pane.

use std::path::Path;

use koshi_core::command::{Command, CommandResult, DetachArgs};
use koshi_core::ids::{ClientId, SessionId};

use crate::cli::SessionReference;
use crate::targeting;
use koshi_core::ids::parse_prefixed_uuid;
use koshi_link::discovery::{self, Discovered};
use koshi_link::error::CliError;
use koshi_link::ipc_client;
use koshi_link::router_client::request_new_session;

/// The `koshi --headless` entry point: asks for a session with nothing
/// attached to it. Forwards to `request_new_session`.
///
/// `allow_other_users` is the `--allow-other-users` flag typed beside
/// `--headless`.
pub fn request_headless_session(
    runtime_directory: &Path,
    profile_name: Option<&str>,
    should_allow_other_users: Option<bool>,
) -> Result<SessionId, CliError> {
    request_new_session(runtime_directory, profile_name, should_allow_other_users)
}

/// End the session named by `session_reference`, or the only running session when
/// absent. An id goes straight to that session; a name is resolved against
/// every running session first.
///
/// Killing a session also shuts its control socket down, so the success reply
/// and the shutdown race: the reply almost always arrives first, but if the
/// socket closes before it does, the session has still ended and this returns
/// [`CliError::IpcUnavailable`] instead of the applied [`CommandResult`].
pub fn kill_session(
    session_reference: Option<&SessionReference>,
) -> Result<CommandResult, CliError> {
    kill_session_in_runtime_directory(&ipc_client::resolve_runtime_directory()?, session_reference)
}

/// [`kill_session`] against an explicit runtime directory.
fn kill_session_in_runtime_directory(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<CommandResult, CliError> {
    let session_id = resolve_session_target(runtime_directory, session_reference)?;
    ipc_client::submit_external_command_via_runtime_directory(
        runtime_directory,
        session_id,
        None,
        Command::Quit,
    )
}

/// Resolve the session a `kill-session` or `detach --all` argument names: an id is
/// taken as it stands, and a name or an absent argument is resolved against
/// every running session by [`select_session_to_kill`].
fn resolve_session_target(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<SessionId, CliError> {
    let session_name = match session_reference {
        Some(SessionReference::SessionId(session_id)) => return Ok(*session_id),
        Some(SessionReference::SessionName(session_name)) => Some(session_name.as_str()),
        None => None,
    };
    select_session_to_kill(
        &discovery::fetch_all_session_overviews(runtime_directory),
        session_name,
    )
}

/// Pick the named session, or apply the sole-running-session rule.
fn select_session_to_kill(
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
pub fn detach_client_or_session(target_text: &str) -> Result<CommandResult, CliError> {
    detach_client_or_session_in_runtime_directory(
        &ipc_client::resolve_runtime_directory()?,
        target_text,
    )
}

/// [`detach_client_or_session`] against an explicit runtime directory.
fn detach_client_or_session_in_runtime_directory(
    runtime_directory: &Path,
    target_text: &str,
) -> Result<CommandResult, CliError> {
    let (session_id, client_id) = select_detach_target(runtime_directory, target_text)?;
    ipc_client::submit_external_command_via_runtime_directory(
        runtime_directory,
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
fn select_detach_target(
    runtime_directory: &Path,
    target_text: &str,
) -> Result<(SessionId, Option<ClientId>), CliError> {
    if target_text.starts_with("session-") {
        let parsed_uuid = parse_prefixed_uuid(target_text, "session").map_err(|parse_detail| {
            CliError::InvalidArgs {
                detail: parse_detail,
            }
        })?;
        return Ok((SessionId::from_uuid(parsed_uuid), None));
    }

    let discovered_sessions = discovery::fetch_all_session_overviews(runtime_directory);
    let Ok(parsed_uuid) = parse_prefixed_uuid(target_text, "client") else {
        return Ok((
            select_session_to_kill(&discovered_sessions, Some(target_text))?,
            None,
        ));
    };
    let client_id = ClientId::from_uuid(parsed_uuid);
    match find_session_for_client(&discovered_sessions, client_id) {
        Ok(session_id) => Ok((session_id, Some(client_id))),
        // A `client-` id is a client and nothing else.
        Err(selection_error) if target_text.starts_with("client-") => Err(selection_error),
        Err(selection_error) => {
            let session_id = SessionId::from_uuid(parsed_uuid);
            let has_session_record = discovered_sessions
                .sessions
                .iter()
                .any(|session_overview| session_overview.session.session_id == session_id);
            if has_session_record || discovered_sessions.is_complete() {
                Ok((session_id, None))
            } else {
                Err(selection_error)
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
        session_reference,
    )
}

/// [`detach_all_session`] against an explicit runtime directory.
fn detach_all_session_in_runtime_directory(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<CommandResult, CliError> {
    let session_id = resolve_session_target(runtime_directory, session_reference)?;
    ipc_client::submit_external_command_via_runtime_directory(
        runtime_directory,
        session_id,
        None,
        Command::DetachAll,
    )
}

#[cfg(test)]
mod tests;
