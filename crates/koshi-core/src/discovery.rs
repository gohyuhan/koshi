//! Read-only discovery snapshots answering the list and inspect queries.
//!
//! Each `*Info` struct is the serializable answer to one discovery query
//! (`list-sessions`, `list-tabs`, `list-panes`, `list-clients`, and the
//! `inspect` forms): the runtime builds them from live state and the CLI
//! renders them as a table or JSON. Every struct carries the stable ids
//! printed by Koshi, usable directly as explicit `--session`/`--tab`/
//! `--pane`/`--client` targets.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::geometry::{Rect, Size};
use crate::ids::{ClientId, PaneId, SessionId, TabId};
use crate::lock::LockMode;

/// One session as reported by `list-sessions` and `inspect session`.
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

/// One tab as reported by `list-tabs` and `inspect tab`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    /// Stable tab id.
    pub id: TabId,
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

/// One pane as reported by `list-panes` and `inspect pane`.
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
    /// Working directory the pane started in, when known.
    pub cwd: Option<PathBuf>,
    /// The argv the pane was spawned to run — program first, then its
    /// arguments — for a command pane; `None` for a shell pane.
    pub command: Option<Vec<String>>,
    /// Where the pane sits in its life.
    pub state: PaneState,
    /// Ids of the clients whose focus is on this pane.
    pub focused_by_clients: Vec<ClientId>,
    /// The pane's solved rectangle within its tab; `None` when the layout
    /// has no room for the pane at the tab's current size.
    pub layout_rect: Option<Rect>,
}

/// One attached client as reported by `list-clients` and `inspect client`.
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
