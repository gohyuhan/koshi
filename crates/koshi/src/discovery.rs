//! Answering the discovery queries across every running koshi.
//!
//! Each running process answers one question — describe yourself, as a
//! [`koshi_core::discovery::SessionOverview`]. This module does the rest
//! locally: probe every endpoint file in the runtime directory, drop the
//! ones nothing listens behind, and turn the answers into the rows a
//! listing prints or the single record an `inspect` prints.
//!
//! A listing row is an id chain plus the names on it: a pane row names its
//! pane, its tab, and its session, so the ids it prints can be pasted
//! straight into a `--pane`/`--tab`/`--session` flag. The full detail of one
//! entity — creation time, working directory, argv, lock state — belongs to
//! `inspect`, which renders the `koshi-core` structs themselves.

use std::path::Path;

use koshi_core::discovery::{ClientInfo, PaneInfo, SessionInfo, SessionOverview, TabInfo};
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
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
    /// The pane's title, once the child or a rename has set one.
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

/// Every running session's overview, sorted by session name and then id so
/// two runs of the same listing print the same order.
///
/// A session that is gone contributes no rows, and [`fetch_one`] has already
/// swept what it left behind. A session that is listening but cannot finish
/// the exchange also contributes no rows, and says so on stderr so its
/// absence is not silent.
#[must_use]
pub fn fetch_all(runtime_dir: &Path) -> Vec<SessionOverview> {
    let mut overviews: Vec<SessionOverview> = ipc_client::advertised_sessions(runtime_dir)
        .into_iter()
        .filter_map(|session_id| match fetch_one(runtime_dir, session_id) {
            Ok(overview) => Some(overview),
            // Gone: swept by `fetch_one`, and it simply has no rows.
            Err(CliError::SessionNotFound { .. }) => None,
            Err(error) => {
                eprintln!("koshi: session {session_id} did not answer: {error}");
                None
            }
        })
        .collect();
    overviews.sort_by(|a, b| {
        a.session
            .name
            .cmp(&b.session.name)
            .then(a.session.id.cmp(&b.session.id))
    });
    overviews
}

/// Ask the one session `session_id` to describe itself, sweeping what it
/// left behind if it is gone.
///
/// The failure keeps its kind, so a caller can tell the two apart: nothing
/// listens ([`CliError::SessionNotFound`]) versus something listens but the
/// exchange failed, such as a token that no longer matches
/// ([`CliError::IpcUnavailable`]).
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
        .map(|overview| SessionRow {
            id: overview.session.id,
            name: overview.session.name.clone(),
        })
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
                name: tab.name.clone(),
                session: overview.session.id,
                session_name: overview.session.name.clone(),
            })
        })
        .collect()
}

/// The `list-panes` answer: every pane of every listed session, in the
/// overview's own order — tab-bar order, then layout order within a tab.
///
/// A pane whose tab has left the tab list has no tab name to print and is
/// left out; the overview builds both lists from the same state in one pass,
/// so this cannot happen to a live pane.
#[must_use]
pub fn pane_rows(overviews: &[SessionOverview]) -> Vec<PaneRow> {
    overviews
        .iter()
        .flat_map(|overview| {
            overview.panes.iter().filter_map(|pane| {
                let tab = overview.tabs.iter().find(|tab| tab.id == pane.tab_id)?;
                Some(PaneRow {
                    id: pane.id,
                    name: pane.title.clone(),
                    tab: tab.id,
                    tab_name: tab.name.clone(),
                    session: overview.session.id,
                    session_name: overview.session.name.clone(),
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

/// The session `session_id` names, in full, wherever it is running.
pub fn find_session(
    overviews: &[SessionOverview],
    session_id: SessionId,
) -> Result<SessionInfo, CliError> {
    overviews
        .iter()
        .find(|overview| overview.session.id == session_id)
        .map(|overview| overview.session.clone())
        .ok_or_else(|| not_found("session", &session_id.to_string()))
}

/// The tab `tab_id` names, in full, wherever it is running.
pub fn find_tab(overviews: &[SessionOverview], tab_id: TabId) -> Result<TabInfo, CliError> {
    overviews
        .iter()
        .flat_map(|overview| overview.tabs.iter())
        .find(|tab| tab.id == tab_id)
        .cloned()
        .ok_or_else(|| not_found("tab", &tab_id.to_string()))
}

/// The pane `pane_id` names, in full, wherever it is running.
pub fn find_pane(overviews: &[SessionOverview], pane_id: PaneId) -> Result<PaneInfo, CliError> {
    overviews
        .iter()
        .flat_map(|overview| overview.panes.iter())
        .find(|pane| pane.id == pane_id)
        .cloned()
        .ok_or_else(|| not_found("pane", &pane_id.to_string()))
}

/// The client `client_id` names, in full, wherever it is attached.
pub fn find_client(
    overviews: &[SessionOverview],
    client_id: ClientId,
) -> Result<ClientInfo, CliError> {
    overviews
        .iter()
        .flat_map(|overview| overview.clients.iter())
        .find(|client| client.id == client_id)
        .cloned()
        .ok_or_else(|| not_found("client", &client_id.to_string()))
}

/// An `inspect` miss, shaped like a session's own rejection so it prints and
/// exits the same way a refused target does.
fn not_found(kind: &str, id: &str) -> CliError {
    CliError::CommandRejected {
        reason: RejectReason::TargetNotFound,
        help: Some(format!("no running session has {kind} {id}")),
    }
}

#[cfg(test)]
mod tests;
