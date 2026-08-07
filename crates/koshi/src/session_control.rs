//! Process-level session commands served without an attached pane.

use std::path::Path;

use koshi_beta::beta_feature;
use koshi_core::command::{Command, CommandResult, DetachArgs};
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::router::{RouterRequestKind, RouterResult};

use crate::cli::{parse_prefixed_uuid, SessionRef};
use crate::discovery::{self, Discovered};
use crate::error::CliError;
use crate::ipc_client;
use crate::router_client::router_request;

/// The gated `koshi --headless` entry point: asks for a session with nothing
/// attached to it. Forwards to `request_new_session`.
#[beta_feature(otherwise = Err(CliError::Runtime {
    detail: koshi_beta::blocked_message("koshi --headless"),
}))]
pub fn request_headless_session(
    runtime_dir: &Path,
    profile: Option<&str>,
) -> Result<SessionId, CliError> {
    request_new_session(runtime_dir, profile)
}

/// Ask the router to make a new session and hand back its id. Starts a router
/// first when none is running.
///
/// The session's first shell opens in the directory this command was run in.
/// A directory that cannot be read is sent as `None`, and the session server
/// keeps the directory it inherited.
pub(crate) fn request_new_session(
    runtime_dir: &Path,
    profile: Option<&str>,
) -> Result<SessionId, CliError> {
    let kind = RouterRequestKind::CreateSession {
        profile: profile.map(str::to_string),
        cwd: std::env::current_dir().ok(),
    };
    match router_request(runtime_dir, kind)? {
        RouterResult::Created(address) => Ok(address.id),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(CliError::IpcUnavailable {
            detail: format!("the router answered a create session with {other:?}"),
        }),
    }
}

/// End the session named by `session`, or the only running session when
/// absent. An id goes straight to that session; a name is resolved against
/// every running session first.
///
/// Killing a session also shuts its control socket down, so the success reply
/// and the shutdown race: the reply almost always arrives first, but if the
/// socket closes before it does, the session has still ended and this returns
/// [`CliError::IpcUnavailable`] instead of the applied [`CommandResult`].
pub fn kill_session(session: Option<&SessionRef>) -> Result<CommandResult, CliError> {
    kill_session_in(&ipc_client::runtime_dir()?, session)
}

/// [`kill_session`] against an explicit runtime directory.
fn kill_session_in(
    runtime_dir: &Path,
    session: Option<&SessionRef>,
) -> Result<CommandResult, CliError> {
    let session_id = resolve_session(runtime_dir, session)?;
    ipc_client::submit_external_via_runtime_dir(runtime_dir, session_id, Command::Quit)
}

/// The session a `kill-session` or `detach --all` argument names: an id is
/// taken as it stands, and a name or an absent argument is resolved against
/// every running session by [`select_kill_session`].
fn resolve_session(
    runtime_dir: &Path,
    session: Option<&SessionRef>,
) -> Result<SessionId, CliError> {
    let name = match session {
        Some(SessionRef::Id(id)) => return Ok(*id),
        Some(SessionRef::Name(name)) => Some(name.as_str()),
        None => None,
    };
    select_kill_session(&discovery::fetch_all(runtime_dir), name)
}

/// Pick the named session, or apply the sole-running-session rule.
fn select_kill_session(found: &Discovered, name: Option<&str>) -> Result<SessionId, CliError> {
    let sessions = found.sessions.as_slice();
    match name {
        Some(name) => {
            let matches: Vec<SessionId> = sessions
                .iter()
                .filter(|overview| overview.session.name == name)
                .map(|overview| overview.session.id)
                .collect();
            match matches.as_slice() {
                [] => Err(found.no_such_session(name)),
                [only] if found.is_complete() => Ok(*only),
                [_] => Err(found.unanswered(&format!("cannot tell whether `{name}` is unique"))),
                several => {
                    let ids = several
                        .iter()
                        .map(SessionId::to_string)
                        .collect::<Vec<String>>()
                        .join(", ");
                    Err(CliError::CommandRejected {
                        reason: RejectReason::TargetAmbiguous,
                        help: Some(format!(
                            "several sessions are named `{name}`: {ids}; use the session id"
                        )),
                    })
                }
            }
        }
        None => {
            let mut running = sessions.iter();
            match (running.next(), running.next()) {
                (Some(_), Some(_)) => Err(CliError::CommandRejected {
                    reason: RejectReason::TargetAmbiguous,
                    help: Some(
                        "several sessions are running; name one: koshi kill-session <name>"
                            .to_string(),
                    ),
                }),
                (Some(only), None) if found.is_complete() => Ok(only.session.id),
                (None, _) if found.is_complete() => Err(CliError::NoSessions),
                _ => Err(found.unanswered(
                    "cannot tell which session to kill; name one: koshi kill-session <name>",
                )),
            }
        }
    }
}

/// Detach the client the `koshi detach` argument names, leaving the session
/// running and its panes untouched.
///
/// The value is a client id, a session id, or a session display name. A value
/// that names a session rather than a client leaves the choice to that
/// session, which detaches its only attached client and lists the attached ids
/// when there are several.
pub fn detach_client_or_session(raw: &str) -> Result<CommandResult, CliError> {
    detach_client_or_session_in(&ipc_client::runtime_dir()?, raw)
}

/// [`detach_client_or_session`] against an explicit runtime directory.
fn detach_client_or_session_in(runtime_dir: &Path, raw: &str) -> Result<CommandResult, CliError> {
    let (session_id, client) = select_detach_target(runtime_dir, raw)?;
    ipc_client::submit_external_via_runtime_dir(
        runtime_dir,
        session_id,
        Command::Detach(DetachArgs { client }),
    )
}

/// The session to ask and the client it detaches, for the value typed after
/// `koshi detach`.
///
/// A `session-<uuid>` id names a session and goes straight there, asking no
/// session to describe itself. Anything else is read against the running
/// sessions: a `client-<uuid>` id, or a bare UUID an answering session reports
/// as an attached client, names that client and the session holding it; a bare
/// UUID no attached client carries is read as a session id instead; any other
/// value is a session display name, resolved the way `kill-session` resolves
/// one. A resolved session with no named client is returned as `None`, so the
/// session itself picks the client.
fn select_detach_target(
    runtime_dir: &Path,
    raw: &str,
) -> Result<(SessionId, Option<ClientId>), CliError> {
    if raw.starts_with("session-") {
        let uuid = parse_prefixed_uuid(raw, "session")
            .map_err(|detail| CliError::InvalidArgs { detail })?;
        return Ok((SessionId::from_uuid(uuid), None));
    }

    let found = discovery::fetch_all(runtime_dir);
    let Ok(uuid) = parse_prefixed_uuid(raw, "client") else {
        return Ok((select_kill_session(&found, Some(raw))?, None));
    };
    let client_id = ClientId::from_uuid(uuid);
    match session_holding(&found, client_id) {
        Ok(session_id) => Ok((session_id, Some(client_id))),
        // A `client-` id is a client and nothing else.
        Err(error) if raw.starts_with("client-") => Err(error),
        Err(error) => {
            let session_id = SessionId::from_uuid(uuid);
            let answered = found
                .sessions
                .iter()
                .any(|overview| overview.session.id == session_id);
            if answered || found.is_complete() {
                Ok((session_id, None))
            } else {
                Err(error)
            }
        }
    }
}

/// The session that reports `client_id` among its attached clients.
fn session_holding(found: &Discovered, client_id: ClientId) -> Result<SessionId, CliError> {
    found
        .sessions
        .iter()
        .find(|overview| overview.clients.iter().any(|client| client.id == client_id))
        .map(|overview| overview.session.id)
        .ok_or_else(|| found.missing("client", &client_id.to_string()))
}

/// Detach every client attached to the session named by `session`, or to the
/// only running session when absent. The session keeps running and its panes
/// are untouched.
///
/// An id goes straight to that session; a name is resolved against every
/// running session first.
pub fn detach_all_session(session: Option<&SessionRef>) -> Result<CommandResult, CliError> {
    detach_all_session_in(&ipc_client::runtime_dir()?, session)
}

/// [`detach_all_session`] against an explicit runtime directory.
fn detach_all_session_in(
    runtime_dir: &Path,
    session: Option<&SessionRef>,
) -> Result<CommandResult, CliError> {
    let session_id = resolve_session(runtime_dir, session)?;
    ipc_client::submit_external_via_runtime_dir(runtime_dir, session_id, Command::DetachAll)
}

#[cfg(test)]
mod tests;
