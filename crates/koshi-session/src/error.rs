//! Session domain errors. Classify into [`DomainCategory::Session`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use thiserror::Error;

use crate::session::lifecycle::{SessionLifecycle, SessionLifecycleEvent};

/// An attempt to move a session through an illegal lifecycle step.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal session lifecycle transition from {from:?} on {event:?}")]
pub struct InvalidTransition {
    /// The state the session was in.
    pub from: SessionLifecycle,
    /// The event that was rejected.
    pub event: SessionLifecycleEvent,
}

impl DomainError for InvalidTransition {
    fn category(&self) -> DomainCategory {
        DomainCategory::Session
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// A way a session's layout, pane registry, client focus, and pane lifecycles
/// can disagree with one another. [`Session::validate`](crate::session::state::Session::validate)
/// returns every violation it finds in one pass, so a single check surfaces the
/// whole picture rather than only the first fault. Each variant names the
/// offending pane, tab, or client, so a caught state points straight at what to
/// fix before a snapshot or render is built from it.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionConsistencyError {
    /// A layout leaf references a pane with no record in the registry.
    #[error("tab {tab:?} layout references pane {pane:?} with no registry record")]
    PaneNotInRegistry { tab: TabId, pane: PaneId },

    /// A layout leaf points to a pane already in the `Removed` state, which
    /// should have left both the layout and the registry.
    #[error("tab {tab:?} layout still holds removed pane {pane:?}")]
    RemovedPaneInLayout { tab: TabId, pane: PaneId },

    /// A live or `Exited` record is not a leaf in any tab's layout — an orphan
    /// the layout forgot or never placed.
    #[error("pane {pane:?} is {lifecycle:?} but absent from every layout")]
    OrphanedPaneRecord {
        pane: PaneId,
        lifecycle: PaneLifecycle,
    },

    /// A client focuses a pane that has no record in the registry at all.
    #[error("client {client:?} focuses pane {pane:?} (tab {tab:?}) with no registry record")]
    FocusPaneNotInRegistry {
        client: ClientId,
        tab: TabId,
        pane: PaneId,
    },

    /// A client remembers focus in a tab that is no longer in the session.
    /// Distinct from [`SessionConsistencyError::ActiveTabMissing`]: this is a
    /// stale `focus_by_tab` entry for a closed tab, not the tab shown now.
    #[error("client {client:?} remembers focus in tab {tab:?} that is not in the session")]
    FocusTabMissing { client: ClientId, tab: TabId },

    /// A client focuses a pane that exists, in a tab that exists, but the pane
    /// is not a leaf in that tab's layout.
    #[error("client {client:?} focuses pane {pane:?} absent from tab {tab:?} layout")]
    FocusTargetMissing {
        client: ClientId,
        tab: TabId,
        pane: PaneId,
    },

    /// A client's active tab is not one of the session's tabs. Reported only
    /// while the session still has tabs; a session emptied by its last tab
    /// closing is quitting, and its viewers' active-tab references dangle by
    /// definition until the transport disconnects them.
    #[error("client {client:?} active tab {tab:?} is not in the session")]
    ActiveTabMissing { client: ClientId, tab: TabId },

    /// A `Removed`-lifecycle record still lingers in the registry instead of
    /// having been dropped by teardown.
    #[error("removed pane {pane:?} still has a registry record")]
    LingeringRemovedRecord { pane: PaneId },

    /// The same pane is a leaf in more than one place — across two tabs, or
    /// twice within one tab's tree. A pane belongs to exactly one tab at one
    /// position, so a non-`Removed` record must map to exactly one leaf.
    #[error("pane {pane:?} appears as a layout leaf in tabs {tabs:?}")]
    PaneInMultipleLayouts { pane: PaneId, tabs: Vec<TabId> },

    /// A tab is stored under a map key that is not its own id, so lookups by id
    /// reach the wrong entry or miss it entirely.
    #[error("tab stored under key {key:?} reports its own id as {tab_id:?}")]
    TabKeyMismatch { key: TabId, tab_id: TabId },

    /// A client in this session's registry carries a different session id, so
    /// it was routed to the wrong session aggregate.
    #[error("client {client:?} belongs to session {found:?}, not this one")]
    ClientSessionMismatch { client: ClientId, found: SessionId },

    /// Two tabs claim the same bar position.
    #[error("multiple tabs claim bar index {index}")]
    DuplicateTabIndex { index: usize },

    /// A `Closed` tab still sits in the session's tab map instead of having
    /// been dropped when it wound down.
    #[error("closed tab {tab:?} still sits in the session's tab map")]
    LingeringClosedTab { tab: TabId },
}

impl DomainError for SessionConsistencyError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Session
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
