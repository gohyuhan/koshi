//! Process-level session commands served without an attached pane.

use std::path::Path;

use koshi_core::command::{Command, CommandResult};
use koshi_core::event::RejectReason;
use koshi_core::ids::SessionId;

use crate::cli::SessionRef;
use crate::discovery::{self, Discovered};
use crate::error::CliError;
use crate::ipc_client;

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
    let session_id = match session {
        Some(SessionRef::Id(id)) => *id,
        Some(SessionRef::Name(name)) => {
            select_kill_session(&discovery::fetch_all(runtime_dir), Some(name.as_str()))?
        }
        None => select_kill_session(&discovery::fetch_all(runtime_dir), None)?,
    };
    ipc_client::submit_external_via_runtime_dir(runtime_dir, session_id, Command::Quit)
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

#[cfg(test)]
mod tests;
