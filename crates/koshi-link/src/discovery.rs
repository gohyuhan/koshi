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

use std::path::Path;

use koshi_core::discovery::{ClientInfo, PaneInfo, SessionOverview, TabInfo};
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::redact::redact_argv;
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
    pub id: SessionId,
    /// The session's display name.
    pub name: String,
    /// The saved server this session runs on, by the name it was saved under
    /// or its `host:port` address. `None` for a session on this machine.
    pub server: Option<String>,
}

impl SessionRow {
    /// One row for `id`, naming `server`, with `name` filtered by
    /// [`sanitize_reported_text`].
    ///
    /// `SessionRow::new(id, "web\u{7f}srv", None).name` is `"websrv"`.
    #[must_use]
    pub fn new(id: SessionId, name: &str, server: Option<String>) -> Self {
        SessionRow {
            id,
            name: sanitize_reported_text(name),
            server,
        }
    }
}

/// One `list-tabs` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabRow {
    /// Stable tab id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The session holding the tab.
    pub session: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// One `list-panes` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRow {
    /// Stable pane id.
    pub id: PaneId,
    /// The pane's title, once the child has set one.
    pub name: Option<String>,
    /// The tab holding the pane.
    pub tab: TabId,
    /// That tab's display name.
    pub tab_name: String,
    /// The session holding the pane.
    pub session: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// One `list-clients` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientRow {
    /// Stable client id.
    pub id: ClientId,
    /// The session the client is attached to.
    pub session: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// What one sweep found: every session that answered, plus how many running
/// sessions could not be asked.
///
/// With one running session unasked, the paths that would answer "no running
/// session has pane X" or "there is exactly one session, so it is the default"
/// report the unasked session instead. A session that is gone is not unasked:
/// it answered by not being there.
#[derive(Debug, Default)]
pub struct Discovered {
    /// The sessions that answered, sorted by name and then id so two runs of
    /// the same query print the same order.
    pub sessions: Vec<SessionOverview>,
    /// How many running sessions were listening but could not answer.
    pub unasked: usize,
}

impl Discovered {
    /// One session, asked directly and answered — a complete census of the
    /// only session the query is about.
    #[must_use]
    pub fn of(overview: SessionOverview) -> Discovered {
        Discovered {
            sessions: vec![overview],
            unasked: 0,
        }
    }

    /// Whether every running session answered.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unasked == 0
    }

    /// Sort the sessions by name and then id, the order
    /// [`sessions`](Self::sessions) documents.
    pub fn sort_sessions(&mut self) {
        self.sessions.sort_by(|a, b| {
            a.session
                .name
                .cmp(&b.session.name)
                .then(a.session.id.cmp(&b.session.id))
        });
    }

    /// The failure for a target that none of the answering sessions holds:
    /// genuinely not found when every session answered, otherwise a report
    /// that one of them could not be asked.
    pub fn missing(&self, kind: &str, id: &str) -> CliError {
        if self.is_complete() {
            CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!("no running session has {kind} {id}")),
            }
        } else {
            self.unanswered(&format!(
                "{kind} {id} is in none of the sessions that answered"
            ))
        }
    }

    /// The failure for a `--session` no answering session matched: not
    /// running when every session answered, otherwise a report that one
    /// could not be asked.
    pub fn no_such_session(&self, session: &str) -> CliError {
        if self.is_complete() {
            CliError::SessionNotFound {
                session: session.to_string(),
            }
        } else {
            self.unanswered(&format!(
                "`{session}` is not among the sessions that answered"
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
            Some(self.unanswered("this listing is incomplete"))
        }
    }

    /// A failure that names `detail` and how many running sessions went
    /// unasked.
    pub fn unanswered(&self, detail: &str) -> CliError {
        let sessions = if self.unasked == 1 {
            "1 running session did not answer".to_string()
        } else {
            format!("{} running sessions did not answer", self.unasked)
        };
        CliError::IpcUnavailable {
            detail: format!("{detail} ({sessions})"),
        }
    }
}

/// Ask every session the runtime directory advertises to describe itself,
/// and, while `allow-other-users` is on, every session the shared directory
/// advertises for the other local users of this machine.
///
/// A session that is gone contributes no rows and is not counted as unasked
/// — [`fetch_one`] has already swept what it left behind. A session that is
/// listening but cannot finish the exchange contributes no rows either, says
/// so on stderr, and is counted.
#[must_use]
pub fn fetch_all(runtime_dir: &Path) -> Discovered {
    let mut found = Discovered::default();
    for session_id in ipc_client::advertised_sessions(runtime_dir) {
        add_answer(&mut found, session_id, fetch_one(runtime_dir, session_id));
    }
    // A session of another user's is never swept: what it left behind belongs
    // to that user.
    for (session_id, socket) in ipc_client::shared_base()
        .into_iter()
        .flat_map(|base| ipc_client::foreign_sessions(&base, runtime_dir))
    {
        add_answer(
            &mut found,
            session_id,
            ipc_client::fetch_foreign_overview(session_id, &socket),
        );
    }
    found.sort_sessions();
    found
}

/// Fold what the session `session_id` answered into `found`: an overview
/// becomes a row, a session that is gone adds nothing, and every other failure
/// prints on stderr and counts as unasked.
fn add_answer(
    found: &mut Discovered,
    session_id: SessionId,
    answered: Result<SessionOverview, CliError>,
) {
    match answered {
        Ok(overview) => found.sessions.push(overview),
        Err(CliError::SessionNotFound { .. }) => {}
        Err(error) => {
            eprintln!("koshi: session {session_id} did not answer: {error}");
            found.unasked += 1;
        }
    }
}

/// Ask the one session `session_id` to describe itself, sweeping what it
/// left behind if it is gone.
///
/// Nothing listening is [`CliError::SessionNotFound`]. Something listening
/// whose exchange failed — a token that no longer matches, say — is
/// [`CliError::IpcUnavailable`].
pub fn fetch_one(runtime_dir: &Path, session_id: SessionId) -> Result<SessionOverview, CliError> {
    ipc_client::fetch_overview(runtime_dir, session_id).inspect_err(|error| {
        if matches!(error, CliError::SessionNotFound { .. }) {
            sweep(runtime_dir, session_id);
        }
    })
}

/// Remove what a session that is gone left behind: its endpoint file, and
/// the socket file it advertised. Every step is best-effort — a file already
/// removed, or one this user may not remove, leaves the listing unaffected.
fn sweep(runtime_dir: &Path, session_id: SessionId) {
    let path = EndpointFile::path(runtime_dir, session_id);
    if let Ok(endpoint) = EndpointFile::read(&path) {
        let _ = reclaim_stale_socket(&endpoint.socket);
    }
    let _ = std::fs::remove_file(&path);
}

/// The `list-sessions` answer: one row per running session.
#[must_use]
pub fn session_rows(overviews: &[SessionOverview]) -> Vec<SessionRow> {
    overviews
        .iter()
        .map(|overview| SessionRow::new(overview.session.id, &overview.session.name, None))
        .collect()
}

/// The `list-tabs` answer: every tab of every listed session, in tab-bar
/// order within each session.
#[must_use]
pub fn tab_rows(overviews: &[SessionOverview]) -> Vec<TabRow> {
    overviews
        .iter()
        .flat_map(|overview| {
            overview.tabs.iter().map(|tab| TabRow {
                id: tab.id,
                name: sanitize_reported_text(&tab.name),
                session: overview.session.id,
                session_name: sanitize_reported_text(&overview.session.name),
            })
        })
        .collect()
}

/// The `list-panes` answer: every pane of every listed session, in the
/// overview's own order — tab-bar order, then layout order within a tab.
///
/// A pane whose tab is not in the overview's tab list has no tab name to print
/// and is left out.
#[must_use]
pub fn pane_rows(overviews: &[SessionOverview]) -> Vec<PaneRow> {
    overviews
        .iter()
        .flat_map(|overview| {
            overview.panes.iter().filter_map(|pane| {
                let tab = overview.tabs.iter().find(|tab| tab.id == pane.tab_id)?;
                Some(PaneRow {
                    id: pane.id,
                    name: pane.title.as_deref().map(sanitize_reported_text),
                    tab: tab.id,
                    tab_name: sanitize_reported_text(&tab.name),
                    session: overview.session.id,
                    session_name: sanitize_reported_text(&overview.session.name),
                })
            })
        })
        .collect()
}

/// The `list-clients` answer: every client attached to every listed session.
#[must_use]
pub fn client_rows(overviews: &[SessionOverview]) -> Vec<ClientRow> {
    overviews
        .iter()
        .flat_map(|overview| {
            overview.clients.iter().map(|client| ClientRow {
                id: client.id,
                session: overview.session.id,
                session_name: overview.session.name.clone(),
            })
        })
        .collect()
}

/// Hide the arguments of every pane's command across `overviews`, leaving
/// each program name visible.
pub fn redact_pane_commands(overviews: &mut [SessionOverview]) {
    for overview in overviews.iter_mut() {
        for pane in overview.panes.iter_mut() {
            pane.command = pane.command.as_deref().map(redact_argv);
        }
    }
}

/// The tab `tab_id` names, in full, wherever it is running.
pub fn find_tab(found: &Discovered, tab_id: TabId) -> Result<TabInfo, CliError> {
    found
        .sessions
        .iter()
        .flat_map(|overview| overview.tabs.iter())
        .find(|tab| tab.id == tab_id)
        .cloned()
        .ok_or_else(|| found.missing("tab", &tab_id.to_string()))
}

/// The pane `pane_id` names, in full, wherever it is running.
pub fn find_pane(found: &Discovered, pane_id: PaneId) -> Result<PaneInfo, CliError> {
    found
        .sessions
        .iter()
        .flat_map(|overview| overview.panes.iter())
        .find(|pane| pane.id == pane_id)
        .cloned()
        .ok_or_else(|| found.missing("pane", &pane_id.to_string()))
}

/// The client `client_id` names, in full, wherever it is attached.
pub fn find_client(found: &Discovered, client_id: ClientId) -> Result<ClientInfo, CliError> {
    found
        .sessions
        .iter()
        .flat_map(|overview| overview.clients.iter())
        .find(|client| client.id == client_id)
        .cloned()
        .ok_or_else(|| found.missing("client", &client_id.to_string()))
}

#[cfg(test)]
mod tests;
