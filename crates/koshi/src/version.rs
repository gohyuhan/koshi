//! Which koshi build is running: this program, and each koshi server process.
//!
//! These two answers differ while an update is rolling out. `koshi update`
//! installs a new binary, the router restarts into it, and each session server
//! replaces its own image one at a time; until every swap lands, the program a
//! shell runs is a newer build than the process answering it.
//!
//! This module only gathers the answers. [`crate::output`] renders them.

use std::path::Path;

use koshi_core::ids::SessionId;
use serde::Serialize;

use crate::cli::SessionRef;
use crate::targeting;
use koshi_link::error::CliError;
use koshi_link::{ipc_client, router_client};

/// The build of the koshi program that ran this command, as `koshi version`
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientVersion {
    /// The build this program was compiled at.
    pub version: String,
}

impl ClientVersion {
    /// The build this program was compiled at.
    #[must_use]
    pub fn of_this_build() -> ClientVersion {
        ClientVersion {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Which koshi server one [`ServerVersionRow`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerKind {
    /// The one router this machine runs.
    Router,
    /// One session's own server.
    Session,
}

/// What asking one koshi server for its build produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerBuild {
    /// The server answered and named this build.
    Running {
        /// The build it named, e.g. `0.2.0`.
        version: String,
    },
    /// The server answered and is too old to name its build.
    Unnamed,
    /// Nothing is listening there.
    NotRunning,
    /// The server could not be asked.
    Unreachable {
        /// What went wrong, as the caller would have been told.
        detail: String,
    },
}

/// One `server-version` row: a koshi server, and what asking it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerVersionRow {
    /// Which server this row is about.
    pub kind: ServerKind,
    /// The session this row is about; absent on the router's row.
    pub session: Option<SessionId>,
    /// What asking it produced.
    #[serde(flatten)]
    pub build: ServerBuild,
}

impl ServerVersionRow {
    /// A row from what a version probe answered: `Ok(None)` is nothing
    /// running, an empty string is a server too old to name its build, and an
    /// error is a server that could not be asked.
    ///
    /// A server that could not be asked also says so on standard error as the
    /// probe returns, so the reason is visible beside a table that has no room
    /// for it.
    fn from_probe(
        kind: ServerKind,
        session: Option<SessionId>,
        probed: Result<Option<String>, CliError>,
    ) -> ServerVersionRow {
        let build = match probed {
            Ok(None) => ServerBuild::NotRunning,
            Ok(Some(named)) if named.is_empty() => ServerBuild::Unnamed,
            Ok(Some(version)) => ServerBuild::Running { version },
            Err(error) => {
                let named = match session {
                    Some(session_id) => format!("session {session_id}"),
                    None => "the router".to_string(),
                };
                eprintln!("koshi: {named} did not answer: {error}");
                ServerBuild::Unreachable {
                    detail: error.to_string(),
                }
            }
        };
        ServerVersionRow {
            kind,
            session,
            build,
        }
    }
}

/// The failure a version answer ends with when a server could not be asked, or
/// `None` when every one of them answered.
///
/// The rows print either way — a partial answer beats none — so this is what
/// stops a caller reading only standard output and the exit code from taking
/// those rows for the whole picture.
#[must_use]
pub fn unreachable_servers(rows: &[ServerVersionRow]) -> Option<CliError> {
    let unasked = rows
        .iter()
        .filter(|row| matches!(row.build, ServerBuild::Unreachable { .. }))
        .count();
    if unasked == 0 {
        return None;
    }
    let servers = if unasked == 1 {
        "1 koshi server did not answer".to_string()
    } else {
        format!("{unasked} koshi servers did not answer")
    };
    Some(CliError::IpcUnavailable {
        detail: format!("{servers}, so this answer is incomplete"),
    })
}

/// Every koshi server this user can reach and the build it named: the router
/// first, then one row per session, in session id order.
///
/// The sessions are the same set `list-sessions` shows — this user's own, and
/// while `allow-other-users` is on, the ones other local users started.
///
/// `session` narrows the answer to that one session and leaves out the
/// router. An id is asked directly; a name is looked up against the running
/// sessions and must match exactly one.
///
/// A server that could not be asked earns a row saying so rather than sinking
/// the whole answer; [`unreachable_servers`] turns those rows into the failure
/// the caller ends with.
pub fn server_version_rows(
    session: Option<&SessionRef>,
) -> Result<Vec<ServerVersionRow>, CliError> {
    server_version_rows_in(&ipc_client::runtime_dir()?, session)
}

/// [`server_version_rows`] against an explicit runtime directory.
fn server_version_rows_in(
    runtime_dir: &Path,
    session: Option<&SessionRef>,
) -> Result<Vec<ServerVersionRow>, CliError> {
    if let Some(session) = session {
        let session_id = resolve_session(runtime_dir, session)?;
        return Ok(vec![session_row(runtime_dir, session_id)]);
    }

    let mut rows = vec![ServerVersionRow::from_probe(
        ServerKind::Router,
        None,
        router_client::running_router_version(runtime_dir),
    )];
    // The two sources never overlap: `foreign_sessions` drops every id
    // `advertised_sessions` reports, so no session earns two rows.
    let mut sessions = ipc_client::advertised_sessions(runtime_dir);
    sessions.extend(
        ipc_client::shared_base()
            .into_iter()
            .flat_map(|base| ipc_client::foreign_sessions(&base, runtime_dir))
            .map(|(session_id, _)| session_id),
    );
    sessions.sort();
    for session_id in sessions {
        rows.push(session_row(runtime_dir, session_id));
    }
    Ok(rows)
}

/// Ask one session's server for its build.
fn session_row(runtime_dir: &Path, session_id: SessionId) -> ServerVersionRow {
    ServerVersionRow::from_probe(
        ServerKind::Session,
        Some(session_id),
        ipc_client::running_session_version(runtime_dir, session_id),
    )
}

/// The session a `--session` value names: an id is taken as it stands, and a
/// name is looked up over a census of the running sessions.
fn resolve_session(runtime_dir: &Path, session: &SessionRef) -> Result<SessionId, CliError> {
    match session {
        SessionRef::Id(id) => Ok(*id),
        SessionRef::Name(name) => targeting::scope_sessions(runtime_dir, Some(session))?
            .sessions
            .first()
            .map(|overview| overview.session.id)
            .ok_or_else(|| CliError::SessionNotFound {
                session: name.clone(),
            }),
    }
}

#[cfg(test)]
mod tests;
