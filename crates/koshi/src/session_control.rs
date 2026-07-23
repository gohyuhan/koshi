//! Process-level session commands served without an attached pane.

use std::path::Path;

use koshi_core::command::{Command, CommandResult};
use koshi_core::event::RejectReason;
use koshi_core::ids::SessionId;

use crate::discovery::{self, Discovered};
use crate::error::CliError;
use crate::ipc_client;

/// End the session named by `name`, or the only running session when absent.
///
/// Killing a session also shuts its control socket down, so the success reply
/// and the shutdown race: the reply almost always arrives first, but if the
/// socket closes before it does, the session has still ended and this returns
/// [`CliError::IpcUnavailable`] instead of the applied [`CommandResult`].
pub fn kill_session(name: Option<&str>) -> Result<CommandResult, CliError> {
    kill_session_in(&ipc_client::runtime_dir()?, name)
}

/// [`kill_session`] against an explicit runtime directory.
fn kill_session_in(runtime_dir: &Path, name: Option<&str>) -> Result<CommandResult, CliError> {
    let found = discovery::fetch_all(runtime_dir);
    let session_id = select_kill_session(&found, name)?;
    ipc_client::submit_external_via_runtime_dir(runtime_dir, session_id, Command::Quit)
}

/// Pick the named session, or apply the sole-running-session rule.
fn select_kill_session(found: &Discovered, name: Option<&str>) -> Result<SessionId, CliError> {
    let sessions = found.sessions.as_slice();
    match name {
        Some(name) => {
            let mut matches = sessions
                .iter()
                .filter(|overview| overview.session.name == name);
            match (matches.next(), matches.next()) {
                (Some(_), Some(_)) => Err(CliError::CommandRejected {
                    reason: RejectReason::TargetAmbiguous,
                    help: Some(format!(
                        "several sessions are named `{name}`; stop one from its own terminal"
                    )),
                }),
                (Some(only), None) if found.is_complete() => Ok(only.session.id),
                (Some(_), None) => {
                    Err(found.unanswered(&format!("cannot tell whether `{name}` is unique")))
                }
                (None, _) => Err(found.no_such_session(name)),
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
