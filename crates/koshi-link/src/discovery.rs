//! Answering the discovery queries across every running koshi.
//!
//! Each running process answers one question — describe yourself, as a
//! [`koshi_core::discovery::SessionOverview`]. This module does the rest
//! locally: probe every endpoint file in the runtime directory — and, while
//! `allow-other-users` is on, every session the shared directory advertises
//! for the other local users of this machine — drop the ones nothing listens
//! behind, and turn the answers into the rows a listing prints or the single
//! record an `inspect` prints.
//!
//! A listing row is an id chain plus the names on it: a pane row names its
//! pane, its tab, and its session, so the ids it prints can be pasted
//! straight into a `--pane`/`--tab`/`--session` flag. The full detail of one
//! entity — creation time, working directory, argv, lock state — belongs to
//! `inspect`, which renders the `koshi-core` structs themselves.

use std::path::{Path, PathBuf};

use koshi_core::discovery::{ClientDiscovery, PaneDiscovery, SessionOverview, TabDiscovery};
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::redact::redact_command_argv;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::validate::reclaim_stale_socket;
use serde::Serialize;

use crate::error::CliError;
use crate::ipc_client;

/// One `list-sessions` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    /// Stable session id.
    pub session_id: SessionId,
    /// The session's display name.
    pub session_name: String,
    /// The saved server this session runs on, by the name it was saved under
    /// or its `host:port` address. `None` for a session on this machine.
    pub server_name_or_address: Option<String>,
}

impl SessionRow {
    /// One row for `session_id`, naming `server_name_or_address`, with
    /// `session_name` filtered by
    /// [`sanitize_reported_text`].
    ///
    /// `SessionRow::from_session(id, "web\u{7f}srv", None).session_name` is
    /// `"websrv"`.
    #[must_use]
    pub fn from_session(
        session_id: SessionId,
        session_name: &str,
        server_name_or_address: Option<String>,
    ) -> Self {
        SessionRow {
            session_id,
            session_name: sanitize_reported_text(session_name),
            server_name_or_address,
        }
    }
}

/// One `list-tabs` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabRow {
    /// Stable tab id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The session holding the tab.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// One `list-panes` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRow {
    /// Stable pane id.
    pub pane_id: PaneId,
    /// The pane's title, once the child has set one.
    pub pane_name: Option<String>,
    /// The tab holding the pane.
    pub tab_id: TabId,
    /// That tab's display name.
    pub tab_name: String,
    /// The session holding the pane.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// One `list-clients` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientRow {
    /// Stable client id.
    pub client_id: ClientId,
    /// The session the client is attached to.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// What one sweep found: every session that answered, plus how many running
/// sessions could not be asked.
///
/// With one running session unasked, the paths that would answer "no running
/// session has pane X" or "there is exactly one session, so it is the default"
/// report the gap instead. A session that is gone is not unasked: it answered
/// by not being there.
#[derive(Debug, Default)]
pub struct Discovered {
    /// The sessions that answered, sorted by name and then id so two runs of
    /// the same query print the same order.
    pub sessions: Vec<SessionOverview>,
    /// How many running sessions were listening but could not answer.
    pub unasked_session_count: usize,
}

impl Discovered {
    /// One session, asked directly and answered — a complete census of the
    /// only session the query is about.
    #[must_use]
    pub fn from_overview(session_overview: SessionOverview) -> Discovered {
        Discovered {
            sessions: vec![session_overview],
            unasked_session_count: 0,
        }
    }

    /// Whether every running session answered.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unasked_session_count == 0
    }

    /// Sort the sessions by name and then id, the order
    /// [`sessions`](Self::sessions) documents.
    pub fn sort_sessions(&mut self) {
        self.sessions
            .sort_by(|left_session_overview, right_session_overview| {
                left_session_overview
                    .session
                    .session_name
                    .cmp(&right_session_overview.session.session_name)
                    .then(
                        left_session_overview
                            .session
                            .session_id
                            .cmp(&right_session_overview.session.session_id),
                    )
            });
    }

    /// The failure for a target that none of the answering sessions holds:
    /// genuinely not found when every session answered, otherwise a report
    /// that one of them could not be asked.
    pub fn build_missing_target_error(
        &self,
        target_kind: &str,
        target_identifier: &str,
    ) -> CliError {
        if self.is_complete() {
            CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!(
                    "no running session has {target_kind} {target_identifier}"
                )),
            }
        } else {
            self.build_unanswered_error(&format!(
                "{target_kind} {target_identifier} is in none of the sessions that answered"
            ))
        }
    }

    /// The failure for a `--session` no answering session matched: not
    /// running when every session answered, otherwise a report that one
    /// could not be asked.
    pub fn build_missing_session_error(&self, session_name: &str) -> CliError {
        if self.is_complete() {
            CliError::SessionNotFound {
                session_name: session_name.to_string(),
            }
        } else {
            self.build_unanswered_error(&format!(
                "`{session_name}` is not among the sessions that answered"
            ))
        }
    }

    /// The failure a listing ends with when it could not see everything, or
    /// `None` when it could.
    ///
    /// A listing prints the rows it has either way, and the exit code carries
    /// the gap: `koshi list-panes` with one session unable to answer prints the
    /// other sessions' panes and still exits 4.
    #[must_use]
    pub fn incomplete_listing(&self) -> Option<CliError> {
        if self.is_complete() {
            None
        } else {
            Some(self.build_unanswered_error("this listing is incomplete"))
        }
    }

    /// A failure that names `error_detail` and how many running sessions went
    /// unasked. The count is singular only at exactly 1.
    ///
    /// `build_unanswered_error("this listing is incomplete")` with
    /// `unasked_session_count` 1 gives
    /// `"this listing is incomplete (1 running session did not answer)"`.
    pub fn build_unanswered_error(&self, error_detail: &str) -> CliError {
        let session_noun = if self.unasked_session_count == 1 {
            "session"
        } else {
            "sessions"
        };
        CliError::IpcUnavailable {
            detail: format!(
                "{error_detail} ({} running {session_noun} did not answer)",
                self.unasked_session_count
            ),
        }
    }
}

/// Ask every session the runtime directory advertises to describe itself,
/// and, while `allow-other-users` is on, every session the shared directory
/// advertises for the other local users of this machine.
///
/// A session that is gone contributes no rows and is not counted as unasked.
/// A session of this user's that is gone also loses its endpoint file and its
/// socket file. A session that is listening but cannot finish the exchange
/// contributes no rows either, says so on stderr, and is counted.
#[must_use]
pub fn fetch_all_session_overviews(runtime_directory: &Path) -> Discovered {
    let mut discovered_sessions = Discovered::default();
    for session_id in ipc_client::list_advertised_sessions(runtime_directory) {
        record_discovery_answer(
            &mut discovered_sessions,
            session_id,
            fetch_session_overview(runtime_directory, session_id),
        );
    }
    // A session of another user's is never swept.
    for (session_id, socket_address) in ipc_client::resolve_shared_sessions_base_directory()
        .into_iter()
        .flat_map(|shared_sessions_base_directory| {
            ipc_client::list_foreign_sessions(&shared_sessions_base_directory, runtime_directory)
        })
    {
        record_discovery_answer(
            &mut discovered_sessions,
            session_id,
            ipc_client::fetch_foreign_session_overview(session_id, &socket_address),
        );
    }
    discovered_sessions.sort_sessions();
    discovered_sessions
}

/// Fold what the session `session_id` answered into `discovered_sessions`: an
/// overview becomes a row, a session that is gone adds nothing, and every
/// other failure prints on stderr and increments `unasked_session_count`.
fn record_discovery_answer(
    discovered_sessions: &mut Discovered,
    session_id: SessionId,
    session_overview_result: Result<SessionOverview, CliError>,
) {
    match session_overview_result {
        Ok(session_overview) => discovered_sessions.sessions.push(session_overview),
        Err(CliError::SessionNotFound { .. }) => {}
        Err(cli_error) => {
            eprintln!("koshi: session {session_id} did not answer: {cli_error}");
            discovered_sessions.unasked_session_count += 1;
        }
    }
}

/// Ask the one session `session_id` to describe itself, sweeping what it
/// left behind if it is gone.
///
/// Nothing listening is [`CliError::SessionNotFound`]. Something listening
/// whose exchange failed — a token that no longer matches, say — is
/// [`CliError::IpcUnavailable`].
pub fn fetch_session_overview(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    ipc_client::fetch_session_overview(runtime_directory, session_id).inspect_err(|cli_error| {
        if matches!(cli_error, CliError::SessionNotFound { .. }) {
            remove_stale_session_files(runtime_directory, session_id);
        }
    })
}

/// Remove what a session that is gone left behind: its endpoint file, and
/// the socket file it advertised. Every step is best-effort — a file already
/// removed, or one this user may not remove, leaves the listing unaffected.
fn remove_stale_session_files(runtime_directory: &Path, session_id: SessionId) {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    if let Ok(endpoint_file) = EndpointFile::load_from_path(&endpoint_path) {
        let _ = reclaim_stale_socket(&endpoint_file.socket_address);
    }
    let _ = std::fs::remove_file(&endpoint_path);
}

/// The `list-sessions` answer: one row per running session. Every row's
/// `server` is `None` — each session in `session_overviews` runs on this machine.
#[must_use]
pub fn build_session_rows(session_overviews: &[SessionOverview]) -> Vec<SessionRow> {
    session_overviews
        .iter()
        .map(|session_overview| {
            SessionRow::from_session(
                session_overview.session.session_id,
                &session_overview.session.session_name,
                None,
            )
        })
        .collect()
}

/// The `list-tabs` answer: every tab of every listed session, in tab-bar
/// order within each session.
#[must_use]
pub fn build_tab_rows(session_overviews: &[SessionOverview]) -> Vec<TabRow> {
    session_overviews
        .iter()
        .flat_map(|session_overview| {
            session_overview.tabs.iter().map(|tab_discovery| TabRow {
                tab_id: tab_discovery.tab_id,
                tab_name: sanitize_reported_text(&tab_discovery.tab_name),
                session_id: session_overview.session.session_id,
                session_name: sanitize_reported_text(&session_overview.session.session_name),
            })
        })
        .collect()
}

/// The `list-panes` answer: every pane of every listed session, in the
/// overview's own order — tab-bar order, then layout order within a tab.
///
/// A pane whose tab is not in the overview's tab list is left out.
#[must_use]
pub fn build_pane_rows(session_overviews: &[SessionOverview]) -> Vec<PaneRow> {
    session_overviews
        .iter()
        .flat_map(|session_overview| {
            session_overview.panes.iter().filter_map(|pane_discovery| {
                let tab_discovery = session_overview
                    .tabs
                    .iter()
                    .find(|tab_discovery| tab_discovery.tab_id == pane_discovery.tab_id)?;
                Some(PaneRow {
                    pane_id: pane_discovery.pane_id,
                    pane_name: pane_discovery
                        .pane_title
                        .as_deref()
                        .map(sanitize_reported_text),
                    tab_id: tab_discovery.tab_id,
                    tab_name: sanitize_reported_text(&tab_discovery.tab_name),
                    session_id: session_overview.session.session_id,
                    session_name: sanitize_reported_text(&session_overview.session.session_name),
                })
            })
        })
        .collect()
}

/// The `list-clients` answer: every client attached to every listed session.
#[must_use]
pub fn build_client_rows(session_overviews: &[SessionOverview]) -> Vec<ClientRow> {
    session_overviews
        .iter()
        .flat_map(|session_overview| {
            session_overview
                .clients
                .iter()
                .map(|client_discovery| ClientRow {
                    client_id: client_discovery.client_id,
                    session_id: session_overview.session.session_id,
                    session_name: sanitize_reported_text(&session_overview.session.session_name),
                })
        })
        .collect()
}

/// Filter every string `session_overview` took from the session that answered through
/// [`sanitize_reported_text`]: the session name, each tab name, and each pane's
/// title, working directory and argv. Ids, times, sizes and counts are left as
/// they are.
///
/// Callers run this the moment an overview comes off a socket, so every reader
/// of it — a listing row, an `inspect` record, a `--session <name>` lookup —
/// sees the same filtered text.
///
/// A pane whose argv is `["sh", "-c", "\u{1b}[2J"]` reads back as
/// `["sh", "-c", "[2J"]`.
pub fn filter_session_overview_text(session_overview: &mut SessionOverview) {
    session_overview.session.session_name =
        sanitize_reported_text(&session_overview.session.session_name);
    for tab_discovery in &mut session_overview.tabs {
        tab_discovery.tab_name = sanitize_reported_text(&tab_discovery.tab_name);
    }
    for pane_discovery in &mut session_overview.panes {
        pane_discovery.pane_title = pane_discovery
            .pane_title
            .as_deref()
            .map(sanitize_reported_text);
        pane_discovery.working_directory =
            pane_discovery
                .working_directory
                .as_ref()
                .map(|working_directory| {
                    PathBuf::from(sanitize_reported_text(&working_directory.to_string_lossy()))
                });
        if let Some(command_argv) = &mut pane_discovery.command_argv {
            for command_argument in command_argv.iter_mut() {
                *command_argument = sanitize_reported_text(command_argument);
            }
        }
    }
}

/// Hide the arguments of every pane's command across `session_overviews`, leaving
/// each program name visible.
pub fn redact_pane_commands(session_overviews: &mut [SessionOverview]) {
    for session_overview in session_overviews.iter_mut() {
        for pane_discovery in session_overview.panes.iter_mut() {
            pane_discovery.command_argv = pane_discovery
                .command_argv
                .as_deref()
                .map(redact_command_argv);
        }
    }
}

/// The tab `tab_id` names, in full, wherever it is running.
///
/// No answering session holding it gives [`Discovered::build_missing_target_error`]'s failure for
/// `"tab"`.
pub fn find_tab(discovered_sessions: &Discovered, tab_id: TabId) -> Result<TabDiscovery, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .flat_map(|session_overview| session_overview.tabs.iter())
        .find(|tab_discovery| tab_discovery.tab_id == tab_id)
        .cloned()
        .ok_or_else(|| discovered_sessions.build_missing_target_error("tab", &tab_id.to_string()))
}

/// The pane `pane_id` names, in full, wherever it is running.
///
/// No answering session holding it gives [`Discovered::build_missing_target_error`]'s failure for
/// `"pane"`.
pub fn find_pane(
    discovered_sessions: &Discovered,
    pane_id: PaneId,
) -> Result<PaneDiscovery, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .flat_map(|session_overview| session_overview.panes.iter())
        .find(|pane_discovery| pane_discovery.pane_id == pane_id)
        .cloned()
        .ok_or_else(|| discovered_sessions.build_missing_target_error("pane", &pane_id.to_string()))
}

/// The client `client_id` names, in full, wherever it is attached.
///
/// No answering session holding it gives [`Discovered::build_missing_target_error`]'s failure for
/// `"client"`.
pub fn find_client(
    discovered_sessions: &Discovered,
    client_id: ClientId,
) -> Result<ClientDiscovery, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .flat_map(|session_overview| session_overview.clients.iter())
        .find(|client_discovery| client_discovery.client_id == client_id)
        .cloned()
        .ok_or_else(|| {
            discovered_sessions.build_missing_target_error("client", &client_id.to_string())
        })
}

#[cfg(test)]
mod tests;
