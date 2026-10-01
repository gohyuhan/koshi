//! Which koshi build is running: this program, and each koshi server process.
//!
//! These two answers differ while an update is rolling out. `koshi update`
//! installs a new binary, the router restarts into it, and each session server
//! replaces its own image one at a time; until every swap lands, the program a
//! shell runs is a newer build than the process answering it.
//!
//! This module only gathers the answers. [`crate::output`] renders them.

use std::path::Path;
use std::time::Instant;

use koshi_core::ids::SessionId;
use serde::Serialize;

use crate::cli::SessionReference;
use crate::targeting;
use koshi_link::error::CliError;
use koshi_link::{discovery, ipc_client, router_client};

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
    /// A server that could not be asked also prints
    /// `koshi: <server> did not answer: <reason>` on standard error as the
    /// probe returns.
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

/// What `koshi server-version` gathered: one row per koshi server asked, and
/// what was not asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersionReport {
    /// The router's row first, then one row per session, in session id order.
    pub server_version_rows: Vec<ServerVersionRow>,
    /// How many sessions the shared directory holds past its caps, as
    /// [`ForeignSessionListing::unlisted_session_count`](ipc_client::ForeignSessionListing::unlisted_session_count)
    /// counts them. They earn no row.
    pub unlisted_session_count: usize,
    /// How many paths the listings of sessions could not read, as
    /// [`SessionCensusPlan::unread_paths`](discovery::SessionCensusPlan::unread_paths)
    /// holds them. The sessions under each one earn no row.
    pub unread_path_count: usize,
}

/// The failure a version answer ends with when a server could not be asked, or
/// `None` when every one of them answered.
///
/// The caller prints the rows either way, then ends with this failure. Each
/// row that could not be asked counts,
/// and so does each of `unlisted_session_count` sessions that earned no row.
/// Each of `unread_path_count` paths adds its own clause.
///
/// Example: one row that could not be asked and `unread_path_count` 1 give
/// `1 koshi server did not answer; 1 path could not be read, so this answer is
/// incomplete`.
#[must_use]
pub fn build_unreachable_server_error(
    server_version_rows: &[ServerVersionRow],
    unlisted_session_count: usize,
    unread_path_count: usize,
) -> Option<CliError> {
    let unreachable_server_count = server_version_rows
        .iter()
        .filter(|server_version_row| {
            matches!(server_version_row.build, ServerBuild::Unreachable { .. })
        })
        .count()
        + unlisted_session_count;
    let mut unanswered_clauses = Vec::new();
    match unreachable_server_count {
        0 => {}
        1 => unanswered_clauses.push("1 koshi server did not answer".to_string()),
        unreachable_server_count => unanswered_clauses.push(format!(
            "{unreachable_server_count} koshi servers did not answer"
        )),
    }
    if unread_path_count > 0 {
        unanswered_clauses.push(discovery::format_unread_path_count(unread_path_count));
    }
    if unanswered_clauses.is_empty() {
        return None;
    }
    Some(CliError::IpcUnavailable {
        detail: format!(
            "{}, so this answer is incomplete",
            unanswered_clauses.join("; ")
        ),
    })
}

/// Every koshi server this user can reach and the build it named: the router
/// first, then one row per session, in session id order.
///
/// The sessions are the same set `list-sessions` asks — this user's own, as
/// [`list_own_sessions`](ipc_client::list_own_sessions) gives them, and while
/// `allow-other-users` is on, the ones other local users started, as
/// [`list_foreign_sessions`](ipc_client::list_foreign_sessions) lists them.
/// Up to [`MAX_SESSIONS_ASKED_AT_ONCE`](ipc_client::MAX_SESSIONS_ASKED_AT_ONCE)
/// sessions are asked at the same time through
/// [`ask_sessions_at_once`](ipc_client::ask_sessions_at_once), and every
/// exchange ends by one deadline,
/// [`SESSION_ANSWER_TIMEOUT_DURATION`](ipc_client::SESSION_ANSWER_TIMEOUT_DURATION)
/// after the router answered. A session restarting earns a row that could not
/// be asked, and so does each session id the shared directory advertises more
/// than once. The sessions the shared directory holds past its caps earn no
/// row: their count is in the report, and stderr says so. Neither do the
/// sessions under a path a listing could not read: the count of those paths is
/// in the report, and stderr names each one.
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
) -> Result<ServerVersionReport, CliError> {
    list_server_version_rows_in_runtime_directory(
        &ipc_client::resolve_runtime_directory()?,
        ipc_client::resolve_shared_sessions_base_directory().as_deref(),
        session_reference,
    )
}

/// [`list_server_version_rows`] against an explicit runtime directory, with
/// `shared_sessions_base_directory` naming where other users' sessions are
/// listed from.
fn list_server_version_rows_in_runtime_directory(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_reference: Option<&SessionReference>,
) -> Result<ServerVersionReport, CliError> {
    if let Some(session_reference) = session_reference {
        let session_id = resolve_session_id(
            runtime_directory,
            shared_sessions_base_directory,
            session_reference,
        )?;
        return Ok(ServerVersionReport {
            server_version_rows: vec![build_session_version_row(
                runtime_directory,
                shared_sessions_base_directory,
                session_id,
            )],
            unlisted_session_count: 0,
            unread_path_count: 0,
        });
    }

    let mut server_version_rows = vec![ServerVersionRow::from_version_probe(
        ServerKind::Router,
        None,
        router_client::find_running_router_version(runtime_directory),
    )];
    let answer_deadline = Instant::now() + ipc_client::SESSION_ANSWER_TIMEOUT_DURATION;
    let census_plan =
        discovery::plan_session_census(runtime_directory, shared_sessions_base_directory);
    discovery::print_census_gap_notes(&census_plan);
    for duplicated_session in &census_plan.duplicated_sessions {
        server_version_rows.push(ServerVersionRow::from_version_probe(
            ServerKind::Session,
            Some(duplicated_session.session_id),
            Err(duplicated_session.build_refusal_error()),
        ));
    }
    let session_asks = census_plan.session_asks;
    let version_answers = ipc_client::ask_sessions_at_once(
        &session_asks,
        answer_deadline,
        |(session_id, foreign_socket_address)| match foreign_socket_address {
            None => ipc_client::find_running_session_version(
                runtime_directory,
                None,
                *session_id,
                Some(answer_deadline),
            ),
            Some(foreign_socket_address) => ipc_client::find_foreign_session_version(
                *session_id,
                foreign_socket_address,
                answer_deadline,
            ),
        },
    );
    for ((session_id, _), version_answer) in session_asks.iter().zip(version_answers) {
        server_version_rows.push(ServerVersionRow::from_version_probe(
            ServerKind::Session,
            Some(*session_id),
            version_answer,
        ));
    }
    server_version_rows[1..].sort_by_key(|server_version_row| server_version_row.session_id);
    Ok(ServerVersionReport {
        server_version_rows,
        unlisted_session_count: census_plan.unlisted_session_count,
        unread_path_count: census_plan.unread_paths.len(),
    })
}

/// Ask one session's server for its build. The session has
/// [`SESSION_ANSWER_TIMEOUT_DURATION`](ipc_client::SESSION_ANSWER_TIMEOUT_DURATION)
/// to answer.
fn build_session_version_row(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
) -> ServerVersionRow {
    ServerVersionRow::from_version_probe(
        ServerKind::Session,
        Some(session_id),
        ipc_client::find_running_session_version(
            runtime_directory,
            shared_sessions_base_directory,
            session_id,
            Some(Instant::now() + ipc_client::SESSION_ANSWER_TIMEOUT_DURATION),
        ),
    )
}

/// The session a `--session` value names: an id is taken as it stands, and a
/// name is looked up over a census of the running sessions.
fn resolve_session_id(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_reference: &SessionReference,
) -> Result<SessionId, CliError> {
    match session_reference {
        SessionReference::SessionId(session_id) => Ok(*session_id),
        SessionReference::SessionName(session_name) => targeting::resolve_session_scope(
            runtime_directory,
            shared_sessions_base_directory,
            Some(session_reference),
        )?
        .sessions
        .first()
        .map(|overview| overview.session.session_id)
        .ok_or_else(|| CliError::SessionNotFound {
            session_name: session_name.clone(),
        }),
    }
}

#[cfg(test)]
mod tests;
