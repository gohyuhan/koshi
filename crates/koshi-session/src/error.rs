//! Session domain errors.

use koshi_core::constant::MAX_FLOATING_PANES_PER_SESSION;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
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

/// Why [`Session::add_floating_member`](crate::session::state::Session::add_floating_member)
/// refused a member. The set is unchanged.
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

/// A way a session's tabs, layout trees, stored tab solves, floating panes, pane registry, and
/// client focus, zoom and floating views can disagree with one another.
/// [`Session::validate_session_consistency`](crate::session::state::Session::validate_session_consistency)
/// returns every violation it finds in one pass. Each variant names what it found: the offending
/// pane, tab or client, the bar index two tabs claim, or the number of floating members.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionConsistencyError {
    /// A layout leaf references a pane with no record in the registry.
    #[error("tab {tab_id:?} layout references pane {pane_id:?} with no registry record")]
    PaneNotInRegistry { tab_id: TabId, pane_id: PaneId },

    /// A registry record is neither a leaf in any tab's layout nor a floating
    /// pane.
    #[error("pane {pane_id:?} is absent from every layout and from the floating panes")]
    OrphanedPaneRecord { pane_id: PaneId },

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

    /// A client holds no floating view of a floating pane of its session.
    #[error("client {client_id:?} holds no floating view of floating pane {pane_id:?}")]
    FloatingViewMissing {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's shown floating panes list a pane that is not a floating
    /// pane.
    #[error("client {client_id:?} shows pane {pane_id:?} as floating, and it is not floating")]
    ShownFloatingPaneMissing {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's shown floating panes list the same pane more than once.
    /// Reported once for each repeat after the first entry.
    #[error("client {client_id:?} shows floating pane {pane_id:?} more than once")]
    DuplicateShownFloatingPane {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A client's view of a floating pane says minimized while its shown
    /// floating panes list it, or says not minimized while they do not.
    #[error("client {client_id:?} view of floating pane {pane_id:?} says is_minimized {is_minimized}, which its shown floating panes contradict")]
    FloatingPaneShownStateMismatch {
        client_id: ClientId,
        pane_id: PaneId,
        is_minimized: bool,
    },

    /// A client's floating focus is not the newest pane it shows: a focus set
    /// while it shows none, a focus on another pane, or no focus while it
    /// shows one.
    #[error("client {client_id:?} focuses floating pane {focused_pane_id:?}, and the newest floating pane it shows is {newest_shown_pane_id:?}")]
    FloatingFocusNotNewestShown {
        client_id: ClientId,
        focused_pane_id: Option<PaneId>,
        newest_shown_pane_id: Option<PaneId>,
    },

    /// A client that cannot draw floating panes shows one: it reports a
    /// `Starving` pane area or one smaller than `5x6` on either axis, or its
    /// own view of its active tab has every pane suppressed.
    #[error("client {client_id:?} cannot draw floating panes and shows floating pane {pane_id:?}")]
    UndrawableFloatingPaneShown {
        client_id: ClientId,
        pane_id: PaneId,
    },

    /// A tab's stored solve differs from a fresh solve of its tree at the tab
    /// size it is solved at, with the session's pane sizing.
    #[error("tab {tab_id:?} stores a solve that its tree does not give")]
    TabSolveStale { tab_id: TabId },

    /// A tab that a client views is solved at a tab size other than its
    /// shared tab size, `0x0` when no viewer contributes one.
    #[error("tab {tab_id:?} is solved at {solved_tab_size:?}, and its tab size is {tab_size:?}")]
    TabSolveSizeStale {
        tab_id: TabId,
        solved_tab_size: Size,
        tab_size: Size,
    },

    /// A client that views its active tab tiled focuses a pane that the tab's
    /// solve suppresses, while the solve draws another pane of the tab.
    #[error("client {client_id:?} focuses pane {pane_id:?} of tab {tab_id:?}, which the tab's solve suppresses")]
    SuppressedPaneFocused {
        client_id: ClientId,
        tab_id: TabId,
        pane_id: PaneId,
    },
}

#[cfg(test)]
mod tests;
