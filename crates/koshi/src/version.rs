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

use crate::cli::SessionReference;
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
    pub fn build_client_version() -> ClientVersion {
        ClientVersion {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Which koshi server one [`ServerVersionRow`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServerKind {
    /// The one router this machine runs.
    Router,
    /// One session's own server.
    Session,
}

/// What asking one koshi server for its build produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state")]
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
    pub server_kind: ServerKind,
    /// The session this row is about; absent on the router's row.
    pub session_id: Option<SessionId>,
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
    fn from_version_probe(
        server_kind: ServerKind,
        session_id: Option<SessionId>,
        version_probe_result: Result<Option<String>, CliError>,
    ) -> ServerVersionRow {
        let build = match version_probe_result {
            Ok(None) => ServerBuild::NotRunning,
            Ok(Some(server_version)) if server_version.is_empty() => ServerBuild::Unnamed,
            Ok(Some(version)) => ServerBuild::Running { version },
            Err(version_probe_error) => {
                let server_label = match session_id {
                    Some(session_id) => format!("session {session_id}"),
                    None => "the router".to_string(),
                };
                eprintln!("koshi: {server_label} did not answer: {version_probe_error}");
                ServerBuild::Unreachable {
                    detail: version_probe_error.to_string(),
                }
            }
        };
        ServerVersionRow {
            server_kind,
            session_id,
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
pub fn build_unreachable_server_error(
    server_version_rows: &[ServerVersionRow],
) -> Option<CliError> {
    let unreachable_server_count = server_version_rows
        .iter()
        .filter(|server_version_row| {
            matches!(server_version_row.build, ServerBuild::Unreachable { .. })
        })
        .count();
    if unreachable_server_count == 0 {
        return None;
    }
    let unreachable_server_summary = if unreachable_server_count == 1 {
        "1 koshi server did not answer".to_string()
    } else {
        format!("{unreachable_server_count} koshi servers did not answer")
    };
    Some(CliError::IpcUnavailable {
        detail: format!("{unreachable_server_summary}, so this answer is incomplete"),
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
/// the whole answer; [`build_unreachable_server_error`] turns those rows into the failure
/// the caller ends with.
pub fn list_server_version_rows(
    session_reference: Option<&SessionReference>,
) -> Result<Vec<ServerVersionRow>, CliError> {
    list_server_version_rows_in_runtime_directory(
        &ipc_client::resolve_runtime_directory()?,
        session_reference,
    )
}

/// [`list_server_version_rows`] against an explicit runtime directory.
fn list_server_version_rows_in_runtime_directory(
    runtime_directory: &Path,
    session_reference: Option<&SessionReference>,
) -> Result<Vec<ServerVersionRow>, CliError> {
    if let Some(session_reference) = session_reference {
        let session_id = resolve_session_id(runtime_directory, session_reference)?;
        return Ok(vec![build_session_version_row(
            runtime_directory,
            session_id,
        )]);
    }

    let mut server_version_rows = vec![ServerVersionRow::from_version_probe(
        ServerKind::Router,
        None,
        router_client::get_running_router_version(runtime_directory),
    )];
    // The two sources never overlap: `foreign_sessions` drops every id
    // `advertised_sessions` reports, so no session earns two rows.
    let mut session_ids = ipc_client::list_advertised_sessions(runtime_directory);
    session_ids.extend(
        ipc_client::resolve_shared_sessions_base_directory()
            .into_iter()
            .flat_map(|shared_base_directory| {
                ipc_client::list_foreign_sessions(&shared_base_directory, runtime_directory)
            })
            .map(|(session_id, _)| session_id),
    );
    session_ids.sort();
    for session_id in session_ids {
        server_version_rows.push(build_session_version_row(runtime_directory, session_id));
    }
    Ok(server_version_rows)
}

/// Ask one session's server for its build.
fn build_session_version_row(runtime_directory: &Path, session_id: SessionId) -> ServerVersionRow {
    ServerVersionRow::from_version_probe(
        ServerKind::Session,
        Some(session_id),
        ipc_client::get_running_session_version(runtime_directory, session_id),
    )
}

/// The session a `--session` value names: an id is taken as it stands, and a
/// name is looked up over a census of the running sessions.
fn resolve_session_id(
    runtime_directory: &Path,
    session_reference: &SessionReference,
) -> Result<SessionId, CliError> {
    match session_reference {
        SessionReference::SessionId(session_id) => Ok(*session_id),
        SessionReference::SessionName(session_name) => {
            targeting::resolve_session_scope(runtime_directory, Some(session_reference))?
                .sessions
                .first()
                .map(|overview| overview.session.session_id)
                .ok_or_else(|| CliError::SessionNotFound {
                    session_name: session_name.clone(),
                })
        }
    }
}

#[cfg(test)]
mod tests;
