//! Read-only discovery snapshots answering the list and inspect queries.
//!
//! Each `*Info` struct describes one entity from live runtime state. An
//! `inspect` query renders one of them in full; a `list-*` query keeps the
//! ids and names off them and prints one row per entity. Every struct
//! carries the stable ids printed by Koshi, usable directly as explicit
//! `--session`/`--tab`/`--pane`/`--client` targets.
//!
//! [`SessionOverview`] gathers all four into one picture of a session, so a
//! caller asking across process boundaries makes one request and filters the
//! answer for the query it was actually given.
//!
//! Paths serialize as their lossy UTF-8 string, so a non-UTF-8 working
//! directory never fails a render.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize, Serializer};

use crate::geometry::Size;
use crate::ids::{ClientId, PaneId, SessionId, TabId};
use crate::lock::LockMode;

/// One session, as `inspect session` reports it and `list-sessions` rows
/// are drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Stable session id.
    pub id: SessionId,
    /// The session's generated display name.
    pub name: String,
    /// When the session was created.
    pub created_at: SystemTime,
    /// Ids of the clients currently attached.
    pub attached_clients: Vec<ClientId>,
    /// Number of panes across all of the session's tabs.
    pub pane_count: usize,
}

/// One tab, as `inspect tab` reports it and `list-tabs` rows are drawn
/// from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    /// Stable tab id.
    pub id: TabId,
    /// The session holding the tab.
    pub session_id: SessionId,
    /// The tab's generated display name.
    pub name: String,
    /// The tab's position in the tab bar, zero-based.
    pub index: usize,
    /// The tab's most-recently-focused pane, once one has been focused.
    pub active_pane: Option<PaneId>,
    /// Number of panes in the tab.
    pub pane_count: usize,
}

/// Where a pane sits in its life, as reported by discovery queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    /// The pane is being created; its child has not started yet.
    Spawning,
    /// The pane's child process is running.
    Running,
    /// The pane's child exited. `code` is `None` when it was signal-killed
    /// or its status was unavailable.
    Exited {
        /// The child's exit code, when one was observed.
        code: Option<i32>,
    },
    /// The pane is shutting down.
    Closing,
}

/// One pane, as `inspect pane` reports it and `list-panes` rows are drawn
/// from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Stable pane id.
    pub id: PaneId,
    /// The tab holding the pane.
    pub tab_id: TabId,
    /// The session holding the pane.
    pub session_id: SessionId,
    /// The pane's display title, once the child or a rename has set one.
    pub title: Option<String>,
    /// Working directory the pane started in, when known. Serializes as
    /// the path's lossy UTF-8 string.
    #[serde(serialize_with = "serialize_path_lossy")]
    pub cwd: Option<PathBuf>,
    /// The argv the pane was spawned to run — program first, then its
    /// arguments — for a command pane; `None` for a shell pane.
    pub command: Option<Vec<String>>,
    /// Where the pane sits in its life.
    pub state: PaneState,
    /// Ids of the clients whose focus is on this pane.
    pub focused_by_clients: Vec<ClientId>,
}

/// One attached client, as `inspect client` reports it and `list-clients`
/// rows are drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Stable client id.
    pub id: ClientId,
    /// The session the client is attached to.
    pub session_id: SessionId,
    /// When the client attached.
    pub attached_at: SystemTime,
    /// The client's terminal viewport size.
    pub viewport_size: Size,
    /// The tab the client is viewing.
    pub active_tab: TabId,
    /// The client's focused pane in the tab it is viewing, once it has
    /// focused one.
    pub focused_pane: Option<PaneId>,
    /// The client's modal input state.
    pub lock_state: LockMode,
}

/// One session described in full: itself, its tabs, every pane across those
/// tabs, and the clients attached to it.
///
/// This is what a session answers a discovery request with. The caller keeps
/// the rows its query asked for and drops the rest, and merges the overviews
/// of several sessions when a query spans more than one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOverview {
    /// The session itself.
    pub session: SessionInfo,
    /// The session's tabs, in tab-bar order.
    pub tabs: Vec<TabInfo>,
    /// Every pane in the session, across all of its tabs.
    pub panes: Vec<PaneInfo>,
    /// The clients currently attached to the session.
    pub clients: Vec<ClientInfo>,
}

/// Serialize an optional path as its lossy UTF-8 string, so a path with
/// non-UTF-8 bytes still serializes (invalid sequences become U+FFFD).
fn serialize_path_lossy<S>(path: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match path {
        Some(path) => serializer.serialize_some(&path.to_string_lossy()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests;
