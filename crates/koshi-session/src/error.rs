//! Session domain errors. Classify into [`DomainCategory::Session`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use thiserror::Error;

use crate::session::lifecycle::{SessionLifecycle, SessionLifecycleEvent};

/// An attempt to move a session through an illegal lifecycle step.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal session lifecycle transition from {previous_lifecycle:?} on {lifecycle_event:?}")]
pub struct InvalidTransition {
    /// The state the session was in.
    pub previous_lifecycle: SessionLifecycle,
    /// The event that was rejected.
    pub lifecycle_event: SessionLifecycleEvent,
}

impl DomainError for InvalidTransition {
    fn category(&self) -> DomainCategory {
        DomainCategory::Session
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// A way a session's tabs, layout trees, pane registry, pane and tab
/// lifecycles, and client focus and zoom can disagree with one another.
/// [`Session::validate_session_consistency`](crate::session::state::Session::validate_session_consistency) returns
/// every violation it finds in one pass. Each variant names what it found: the
/// offending pane, tab or client, or the bar index two tabs claim.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionConsistencyError {
    /// A layout leaf references a pane with no record in the registry.
    #[error("tab {tab_id:?} layout references pane {pane_id:?} with no registry record")]
    PaneNotInRegistry { tab_id: TabId, pane_id: PaneId },

    /// A layout leaf points to a pane already in the `Removed` state, which
    /// should have left both the layout and the registry.
    #[error("tab {tab_id:?} layout still holds removed pane {pane_id:?}")]
    RemovedPaneInLayout { tab_id: TabId, pane_id: PaneId },

    /// A registry record in any state but `Removed` — `Spawning`, `Running`,
    /// `Exited` or `Closing` — is not a leaf in any tab's layout. `lifecycle`
    /// is the state the record holds.
    #[error("pane {pane_id:?} is {pane_lifecycle:?} but absent from every layout")]
    OrphanedPaneRecord {
        pane_id: PaneId,
        pane_lifecycle: PaneLifecycle,
    },

    /// A client focuses a pane that has no record in the registry at all.
    #[error(
        "client {client_id:?} focuses pane {pane_id:?} (tab {tab_id:?}) with no registry record"
    )]
    FocusPaneNotInRegistry {
        client_id: ClientId,
        tab_id: TabId,
        pane_id: PaneId,
    },

    /// A client remembers focus in a tab that is no longer in the session.
    /// Distinct from [`SessionConsistencyError::ActiveTabMissing`]: this is a
    /// stale `focus_by_tab` entry for a closed tab, not the tab shown now.
    #[error("client {client_id:?} remembers focus in tab {tab_id:?} that is not in the session")]
    FocusTabMissing { client_id: ClientId, tab_id: TabId },

    /// A client's remembered focus names a pane that is not a leaf in that
    /// tab's layout. The tab is in the session; the pane may or may not have a
    /// registry record.
    #[error("client {client_id:?} focuses pane {pane_id:?} absent from tab {tab_id:?} layout")]
    FocusTargetMissing {
        client_id: ClientId,
        tab_id: TabId,
        pane_id: PaneId,
    },

    /// A client is zoomed on a pane that is not a live leaf of the tab it is
    /// zoomed in — the pane has no registry record, the tab is gone, or the pane
    /// is not in that tab's layout. Removing a pane drops every zoom on it.
    #[error(
        "client {client_id:?} is zoomed on pane {pane_id:?}, not a live leaf of tab {tab_id:?}"
    )]
    ZoomTargetMissing {
        client_id: ClientId,
        tab_id: TabId,
        pane_id: PaneId,
    },

    /// A client's active tab is not one of the session's tabs. Reported only
    /// while the session still has tabs; a session emptied by its last tab
    /// closing is quitting, and its viewers' active-tab references dangle by
    /// definition until the transport disconnects them.
    #[error("client {client_id:?} active tab {tab_id:?} is not in the session")]
    ActiveTabMissing { client_id: ClientId, tab_id: TabId },

    /// A `Removed`-lifecycle record still lingers in the registry instead of
    /// having been dropped by teardown.
    #[error("removed pane {pane_id:?} still has a registry record")]
    LingeringRemovedRecord { pane_id: PaneId },

    /// The same pane is a leaf in more than one place — across two tabs, or
    /// twice within one tab's tree. A pane belongs to exactly one tab at one
    /// position, so a non-`Removed` record must map to exactly one leaf.
    #[error("pane {pane_id:?} appears as a layout leaf in tabs {tab_ids:?}")]
    PaneInMultipleLayouts {
        pane_id: PaneId,
        tab_ids: Vec<TabId>,
    },

    /// A tab is stored under a map key that is not its own id, so lookups by id
    /// reach the wrong entry or miss it entirely.
    #[error("tab stored under key {stored_tab_id:?} reports its own id as {reported_tab_id:?}")]
    TabKeyMismatch {
        stored_tab_id: TabId,
        reported_tab_id: TabId,
    },

    /// A client in this session's registry carries a different session id, so
    /// it was routed to the wrong session aggregate.
    #[error("client {client_id:?} belongs to session {found_session_id:?}, not this one")]
    ClientSessionMismatch {
        client_id: ClientId,
        found_session_id: SessionId,
    },

    /// Two tabs claim the same bar position.
    #[error("multiple tabs claim bar index {tab_index}")]
    DuplicateTabIndex { tab_index: usize },

    /// A `Closed` tab still sits in the session's tab map instead of having
    /// been dropped when it wound down.
    #[error("closed tab {tab_id:?} still sits in the session's tab map")]
    LingeringClosedTab { tab_id: TabId },
}

impl DomainError for SessionConsistencyError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Session
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

#[cfg(test)]
mod tests;
