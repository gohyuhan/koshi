//! Session domain errors.

use koshi_core::constant::MAX_FLOATING_PANES_PER_SESSION;
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

/// Why [`FloatingSet::add_member`](crate::session::state::FloatingSet::add_member) refused a
/// member. The set is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FloatingSetError {
    /// The member's pane is already a floating pane of the session. `PaneId`
    /// displays as `pane-<uuid>`, so the message reads `pane-<uuid> is already
    /// a floating pane`.
    #[error("{pane_id} is already a floating pane")]
    DuplicatePane { pane_id: PaneId },

    /// The session already holds [`MAX_FLOATING_PANES_PER_SESSION`] floating
    /// panes.
    #[error(
        "a session holds at most {} floating panes",
        MAX_FLOATING_PANES_PER_SESSION
    )]
    TooManyPanes,
}

/// A way a session's tabs, layout trees, floating panes, pane registry, pane and tab lifecycles,
/// and client focus, zoom and floating views can disagree with one another.
/// [`Session::validate_session_consistency`](crate::session::state::Session::validate_session_consistency)
/// returns every violation it finds in one pass. Each variant names what it found: the offending
/// pane, tab or client, the bar index two tabs claim, or the number of floating members.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionConsistencyError {
    /// A layout leaf references a pane with no record in the registry.
    #[error("tab {tab_id:?} layout references pane {pane_id:?} with no registry record")]
    PaneNotInRegistry { tab_id: TabId, pane_id: PaneId },

    /// A layout leaf names a pane whose registry record is in the `Removed`
    /// state.
    #[error("tab {tab_id:?} layout still holds removed pane {pane_id:?}")]
    RemovedPaneInLayout { tab_id: TabId, pane_id: PaneId },

    /// A registry record in any state but `Removed` — `Spawning`, `Running`,
    /// `Exited` or `Closing` — is neither a leaf in any tab's layout nor a
    /// floating pane. `pane_lifecycle` is the state the record holds.
    #[error("pane {pane_id:?} is {pane_lifecycle:?} but absent from every layout and from the floating panes")]
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
    /// stale `focused_pane_id_by_tab_id` entry for a closed tab, not the tab shown now.
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

    /// A client's active tab is not one of the session's tabs. Not reported
    /// while the session holds no tabs: after its last tab closes, every
    /// client's active tab names that closed tab until the transport
    /// disconnects the client.
    #[error("client {client_id:?} active tab {tab_id:?} is not in the session")]
    ActiveTabMissing { client_id: ClientId, tab_id: TabId },

    /// The registry still holds a record in the `Removed` state.
    #[error("removed pane {pane_id:?} still has a registry record")]
    LingeringRemovedRecord { pane_id: PaneId },

    /// The same pane is a leaf in more than one place — across two tabs, or
    /// twice within one tab's tree. `tab_ids` names the tab of each leaf, one
    /// entry per leaf.
    #[error("pane {pane_id:?} appears as a layout leaf in tabs {tab_ids:?}")]
    PaneInMultipleLayouts {
        pane_id: PaneId,
        tab_ids: Vec<TabId>,
    },

    /// A tab is stored under the map key `stored_tab_id`, and its own id is
    /// `reported_tab_id`.
    #[error("tab stored under key {stored_tab_id:?} reports its own id as {reported_tab_id:?}")]
    TabKeyMismatch {
        stored_tab_id: TabId,
        reported_tab_id: TabId,
    },

    /// A client in this session's registry carries another session's id,
    /// `found_session_id`.
    #[error("client {client_id:?} belongs to session {found_session_id:?}, not this one")]
    ClientSessionMismatch {
        client_id: ClientId,
        found_session_id: SessionId,
    },

    /// Two tabs claim the same bar position.
    #[error("multiple tabs claim bar index {tab_index}")]
    DuplicateTabIndex { tab_index: usize },

    /// The floating panes number more than [`MAX_FLOATING_PANES_PER_SESSION`].
    /// `member_count` counts every entry, a repeated pane included.
    #[error(
        "floating panes list {member_count} panes, more than {}",
        MAX_FLOATING_PANES_PER_SESSION
    )]
    TooManyFloatingPanes { member_count: usize },

    /// A floating pane has no record in the registry.
    #[error("floating pane {pane_id:?} has no registry record")]
    FloatingPaneNotInRegistry { pane_id: PaneId },

    /// A floating member names a pane whose registry record is in the
    /// `Removed` state.
    #[error("floating panes still hold removed pane {pane_id:?}")]
    RemovedPaneInFloatingSet { pane_id: PaneId },

    /// The floating panes list the same pane more than once.
    #[error("floating panes list pane {pane_id:?} more than once")]
    DuplicateFloatingPane { pane_id: PaneId },

    /// A floating pane is also a leaf in the layout of each tab in `tab_ids`.
    #[error("floating pane {pane_id:?} is also a layout leaf in tabs {tab_ids:?}")]
    FloatingPaneInLayout {
        pane_id: PaneId,
        tab_ids: Vec<TabId>,
    },

    /// A client stores a floating view of a pane that is not a floating pane.
    #[error(
        "client {client_id:?} stores a floating view of pane {pane_id:?}, which is not floating"
    )]
    FloatingViewTargetMissing {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's floating focus order lists a pane that is not a floating
    /// pane.
    #[error(
        "client {client_id:?} floating focus order lists pane {pane_id:?}, which is not floating"
    )]
    FloatingFocusOrderTargetMissing {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's floating focus order lists the same pane more than once.
    /// Reported once for each repeat after the first entry.
    #[error("client {client_id:?} floating focus order lists pane {pane_id:?} more than once")]
    DuplicateFloatingFocusOrderEntry {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's floating focus names a pane that is not a floating pane.
    #[error("client {client_id:?} focuses pane {pane_id:?} as floating, and it is not floating")]
    FocusedFloatingPaneMissing {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's floating focus names a pane that is not the last entry of
    /// that client's floating focus order.
    #[error("client {client_id:?} focuses floating pane {pane_id:?}, which is not last in its floating focus order")]
    FocusedFloatingPaneNotOnTop {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's floating focus names a pane that client minimized.
    #[error("client {client_id:?} focuses floating pane {pane_id:?}, which it minimized")]
    FocusedFloatingPaneMinimized {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client minimized a floating pane that its floating focus order does
    /// not list.
    #[error("client {client_id:?} minimized floating pane {pane_id:?}, which its floating focus order does not list")]
    MinimizedFloatingPaneNotInFocusOrder {
        client_id: ClientId,
        pane_id: PaneId,
    },
}

#[cfg(test)]
mod tests;
