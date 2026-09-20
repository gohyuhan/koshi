//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::collections::BTreeMap;
use std::time::SystemTime;

use koshi_core::{
    constant::MAX_TAB_FOCUS_MRU_ENTRY_COUNT,
    geometry::Size,
    ids::{ClientId, PaneId, SessionId, TabId},
};
use koshi_layout::tree::LayoutNode;
use koshi_pane::{pane::lifecycle::PaneLifecycle, registry::PaneRegistry};
use serde::{Deserialize, Serialize};

use crate::{
    client::{Client, ClientRegistry},
    error::{InvalidTransition, SessionConsistencyError},
    session::lifecycle::{SessionLifecycle, SessionLifecycleEvent, TabLifecycle},
};

/// One tab: its name, bar position, layout tree, lifecycle, and the panes it
/// focused, most-recent first.
///
/// A tab holds no layout mode. Zoom is a client property: it lives on
/// [`crate::client::Client`] as `zoom_by_tab`, and two clients on this tab can
/// hold different zoom. The tab holds the tree that every client solves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    tab_id: TabId,
    tab_name: String,
    tab_index: usize,
    layout: LayoutNode,
    lifecycle: TabLifecycle,
    /// Panes this tab has focused, most-recent first, with at most one entry
    /// per pane — re-focusing moves a pane to the front instead of adding a
    /// duplicate. Capped at [`MAX_TAB_FOCUS_MRU_ENTRY_COUNT`]; focus recovery walks
    /// it newest-first to pick the inheriting pane when the focused one
    /// disappears.
    focus_mru: Vec<PaneId>,
}

impl Tab {
    /// A freshly created tab showing a single pane. Starts in `Creating`
    /// with no focus recorded yet; `root_pane` is its only layout leaf.
    #[must_use]
    pub fn from_root_pane(
        tab_id: TabId,
        tab_name: String,
        tab_index: usize,
        root_pane: PaneId,
    ) -> Self {
        Self {
            tab_id,
            tab_name,
            tab_index,
            layout: LayoutNode::Pane(root_pane),
            lifecycle: TabLifecycle::Creating,
            focus_mru: Vec::new(),
        }
    }

    /// This tab's stable id, matching its key in [`Session::tabs`].
    #[must_use]
    pub fn get_tab_id(&self) -> TabId {
        self.tab_id
    }

    /// The name shown for this tab in the tab bar.
    #[must_use]
    pub fn get_tab_name(&self) -> &str {
        &self.tab_name
    }

    /// This tab's display position in the bar; kept a dense `0..n` across the
    /// session's tabs by the tab operations.
    #[must_use]
    pub fn get_tab_index(&self) -> usize {
        self.tab_index
    }

    /// This tab's layout tree.
    #[must_use]
    pub fn get_layout_tree(&self) -> &LayoutNode {
        &self.layout
    }

    /// Set this tab's display position. Callers keep positions a dense `0..n`
    /// across the session's tabs.
    pub fn update_tab_index(&mut self, tab_index: usize) {
        self.tab_index = tab_index;
    }

    /// Replace this tab's layout tree.
    pub fn update_layout(&mut self, layout: LayoutNode) {
        self.layout = layout;
    }

    /// Records `pane` as the most-recently focused: moves it to the front,
    /// keeping one entry per pane, then cuts the history back to
    /// [`MAX_TAB_FOCUS_MRU_ENTRY_COUNT`] entries, dropping the oldest.
    ///
    /// A history restored longer than the cap — a session file this process did
    /// not write — is cut to the cap by this one call, not by one entry.
    pub fn record_focus_mru(&mut self, pane_id: PaneId) {
        self.focus_mru
            .retain(|&focused_pane_id| focused_pane_id != pane_id);
        self.focus_mru.insert(0, pane_id);
        self.focus_mru
            .truncate(usize::from(MAX_TAB_FOCUS_MRU_ENTRY_COUNT));
    }

    /// The panes this tab has focused, most-recent first.
    pub fn list_focus_mru(&self) -> &[PaneId] {
        &self.focus_mru
    }

    /// Remove `pane_id` from this tab's focus history.
    pub fn remove_focus_mru(&mut self, pane_id: PaneId) {
        self.focus_mru
            .retain(|&focused_pane_id| focused_pane_id != pane_id);
    }

    /// This tab's current lifecycle state.
    pub fn get_lifecycle(&self) -> &TabLifecycle {
        &self.lifecycle
    }
}

/// One running session: the aggregate root owning the tabs, the pane
/// registry, and the attached-client registry.
///
/// Anything one client may see differently from another — focus, viewport,
/// input mode — lives on that client's entry in [`ClientRegistry`], never as a
/// session-global field. Two attached clients can look at different tabs and
/// panes at the same time. `start_locked` is the mode the session hands the
/// first client to attach, not a mode the session is in.
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    /// Unique id, stable for the session's whole life.
    pub session_id: SessionId,
    /// Human-facing name; attach and list address sessions by it.
    pub session_name: String,
    /// When the session was created. Supplied by the caller at the creation
    /// boundary, never read from the clock here.
    pub created_at: SystemTime,
    /// The session's tabs, keyed by id. Display order is not the map order: it
    /// lives on each tab as its display index, and reordering tabs moves no map
    /// entry.
    pub tabs: BTreeMap<TabId, Tab>,
    /// Runtime metadata for every pane in every tab; layout trees hold
    /// only the ids.
    pub panes: PaneRegistry,
    /// The clients currently attached.
    pub clients: ClientRegistry,
    /// True while the next client to attach must start in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked). A profile
    /// carrying the `lock` marker sets it; [`Session::take_start_lock`] reads
    /// it and clears it, so exactly one attach is locked. A session seeded
    /// without that marker holds `false` and locks nobody. Absent from a
    /// stored session, it reads back `false`.
    #[serde(default)]
    pub start_locked: bool,

    lifecycle: SessionLifecycle,
}

impl Session {
    /// A session with no tabs and no panes, holding the supplied client
    /// registry. Starts in `Starting` with `start_locked`
    /// `false`. `created_at` is supplied by the caller at the creation
    /// boundary, never read from the clock here.
    #[must_use]
    pub fn from_identity_and_client_registry(
        session_id: SessionId,
        session_name: String,
        created_at: SystemTime,
        client_registry: ClientRegistry,
    ) -> Self {
        Self {
            session_id,
            session_name,
            created_at,
            tabs: BTreeMap::new(),
            panes: PaneRegistry::new(),
            clients: client_registry,
            start_locked: false,
            lifecycle: SessionLifecycle::Starting,
        }
    }

    /// Whether this attach must start in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked), clearing the
    /// flag as it reads it.
    ///
    /// Reads [`start_locked`](Self::start_locked) and clears it in one step,
    /// so it returns `true` at most once per session.
    pub fn take_start_lock(&mut self) -> bool {
        std::mem::take(&mut self.start_locked)
    }

    /// The session's current lifecycle state.
    pub fn get_lifecycle(&self) -> &SessionLifecycle {
        &self.lifecycle
    }

    /// Apply a lifecycle `event`, advancing the session's state, or return
    /// [`InvalidTransition`] if the move is illegal from the current state.
    /// Crate-internal — callers drive the lifecycle through the typed wrappers
    /// ([`Session::attach_client`], [`Session::detach_client`],
    /// [`Session::request_session_stop`], [`Session::complete_session_stop`]) or the tab
    /// operations, so the firing conditions stay in one place. Each caller
    /// decides whether a rejected event is an expected no-op to ignore (a
    /// re-attach to an already-`Running` session) or a fault to abort on (a tab
    /// created under a wound-down session).
    pub(crate) fn update_lifecycle(
        &mut self,
        lifecycle_event: SessionLifecycleEvent,
    ) -> Result<(), InvalidTransition> {
        self.lifecycle = self.lifecycle.transition(lifecycle_event)?;
        Ok(())
    }

    /// Attach `client` and mark the session live, returning the record it
    /// displaced when that id was already attached (a re-attach replaces in
    /// place), else `None`. `ClientAttached` moves a `Detaching` (no-client)
    /// session to `Running`; from `Starting`, `Running`, `Stopping` or
    /// `Stopped` it is rejected and the lifecycle stays as it was. The client
    /// is registered either way.
    pub fn attach_client(&mut self, client: Client) -> Option<Client> {
        let displaced = self.clients.attach_client(client);
        let _ = self.update_lifecycle(SessionLifecycleEvent::ClientAttached);
        displaced
    }

    /// Detach the client `client_id`, returning the removed record (`None` if it
    /// was not attached). When it was the *last* attached client the session
    /// drops to `Detaching` — its tabs and panes stay alive; detaching one of
    /// several clients leaves the session `Running`.
    pub fn detach_client(&mut self, client_id: ClientId) -> Option<Client> {
        let removed = self.clients.detach_client(client_id);
        if !self.clients.has_clients() {
            // Only a `Running` session moves to `Detaching`. `Starting`,
            // `Detaching`, `Stopping` and `Stopped` reject the event and keep
            // the state they had.
            let _ = self.update_lifecycle(SessionLifecycleEvent::LastClientDetached);
        }
        removed
    }

    /// The pane region to size tab `tab_id` against: each viewing client's own
    /// pane area, reduced to the per-axis minimum (`cols` and `rows`
    /// independently), which is the largest grid that fits inside *every*
    /// viewer on *both* axes.
    ///
    /// Every attached client whose [`Client::get_active_tab`] is `tab_id`
    /// contributes its [`Client::get_pane_area`]; a viewer that reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving)
    /// contributes nothing. Returns `None` when no viewer of `tab_id`
    /// contributes a size. The result does not depend on which client (if any)
    /// issued the command, nor on the order the viewers attached.
    #[must_use]
    pub fn get_tab_viewport(&self, tab_id: TabId) -> Option<Size> {
        self.clients
            .list_attached_clients()
            .filter(|client| client.get_active_tab() == tab_id)
            .filter_map(Client::get_pane_area)
            .reduce(Size::compute_minimum_axes)
    }

    /// Return the oldest measured viewer's cell dimensions for this tab.
    #[must_use]
    pub fn get_tab_cell_size(&self, tab_id: TabId) -> Option<koshi_core::geometry::PixelCellSize> {
        self.clients
            .list_attached_clients()
            .filter(|client| client.get_active_tab() == tab_id && client.get_cell_size().is_some())
            .min_by_key(|client| (client.get_attached_at(), client.get_client_id()))
            .and_then(Client::get_cell_size)
    }

    /// Request shutdown: move a `Starting`, `Running` or `Detaching` session to
    /// `Stopping`. State is retained: stopping destroys no tabs, panes or
    /// clients.
    pub fn request_session_stop(&mut self) {
        // Idempotent: requesting a stop on an already-`Stopping`/`Stopped`
        // session is rejected and changes nothing.
        let _ = self.update_lifecycle(SessionLifecycleEvent::StopRequested);
    }

    /// Finish shutdown once teardown is done, moving `Stopping` to the terminal
    /// `Stopped`.
    pub fn complete_session_stop(&mut self) {
        // Only a `Stopping` session completes; any other state rejects it.
        let _ = self.update_lifecycle(SessionLifecycleEvent::StopCompleted);
    }

    /// Check every cross-store invariant and return *all* violations in one
    /// pass, or `Ok(())` when the session is internally consistent.
    ///
    /// Checks each tab's map key, lifecycle and bar index; every layout leaf
    /// against the pane registry and every registry record against the layout
    /// trees; and each attached client's session id, active tab, focus and
    /// zoom. See [`SessionConsistencyError`] for the individual checks. The
    /// returned violations arrive in a fixed order: the checks run in the order
    /// listed above, and each one walks its own subjects by id or by bar index,
    /// so one session always reports the same list.
    pub fn validate_session_consistency(&self) -> Result<(), Vec<SessionConsistencyError>> {
        let mut consistency_violations = vec![];
        // Pane id -> the tabs whose layout holds it as a leaf. Built once here,
        // then reused to check the leaf/registry relationship in both
        // directions. Sorted, so two violations from one walk always come out
        // in the same order.
        let mut tab_ids_by_pane_id: BTreeMap<PaneId, Vec<TabId>> = BTreeMap::new();
        // Bar position -> how many tabs claim it, to catch collisions.
        let mut tab_count_by_index: BTreeMap<usize, usize> = BTreeMap::new();

        for (tab_id, tab) in self.tabs.iter() {
            // Every tab is keyed under its own id.
            if *tab_id != tab.tab_id {
                consistency_violations.push(SessionConsistencyError::TabKeyMismatch {
                    stored_tab_id: *tab_id,
                    reported_tab_id: tab.tab_id,
                });
            }

            // A `Closed` tab is terminal and should have left the map.
            if *tab.get_lifecycle() == TabLifecycle::Closed {
                consistency_violations
                    .push(SessionConsistencyError::LingeringClosedTab { tab_id: tab.tab_id });
            }

            *tab_count_by_index.entry(tab.tab_index).or_insert(0) += 1;

            for pane_id in tab.layout.list_leaf_pane_ids() {
                tab_ids_by_pane_id
                    .entry(pane_id)
                    .or_default()
                    .push(tab.tab_id);

                let Some(pane_record) = self.panes.get_pane_record_by_id(pane_id) else {
                    consistency_violations.push(SessionConsistencyError::PaneNotInRegistry {
                        tab_id: tab.tab_id,
                        pane_id,
                    });
                    continue;
                };
                // A `Removed` pane should be gone from both layout and registry.
                if *pane_record.get_lifecycle() == PaneLifecycle::Removed {
                    consistency_violations.push(SessionConsistencyError::RemovedPaneInLayout {
                        tab_id: tab.tab_id,
                        pane_id,
                    });
                }
            }
        }

        // No two tabs may claim the same bar position.
        for (tab_index, tab_count) in &tab_count_by_index {
            if *tab_count > 1 {
                consistency_violations.push(SessionConsistencyError::DuplicateTabIndex {
                    tab_index: *tab_index,
                });
            }
        }

        // A pane belongs to exactly one tab at one position.
        for (pane_id, tab_ids) in &tab_ids_by_pane_id {
            if tab_ids.len() > 1 {
                consistency_violations.push(SessionConsistencyError::PaneInMultipleLayouts {
                    pane_id: *pane_id,
                    tab_ids: tab_ids.clone(),
                });
            }
        }

        // Every live or `Exited` record must be a leaf somewhere; a `Removed`
        // record must not linger in the registry at all.
        for pane_record in self.panes.list_pane_records() {
            if *pane_record.get_lifecycle() == PaneLifecycle::Removed {
                consistency_violations.push(SessionConsistencyError::LingeringRemovedRecord {
                    pane_id: pane_record.get_pane_id(),
                });
            } else if !tab_ids_by_pane_id.contains_key(&pane_record.get_pane_id()) {
                consistency_violations.push(SessionConsistencyError::OrphanedPaneRecord {
                    pane_id: pane_record.get_pane_id(),
                    pane_lifecycle: *pane_record.get_lifecycle(),
                });
            }
        }

        for client in self.clients.list_attached_clients() {
            // A client in this registry must belong to this session.
            if client.get_session_id() != self.session_id {
                consistency_violations.push(SessionConsistencyError::ClientSessionMismatch {
                    client_id: client.get_client_id(),
                    found_session_id: client.get_session_id(),
                });
            }

            // The tab a client is currently showing must exist. Checked only
            // while the session still holds tabs: a session emptied by its last
            // tab closing leaves every client's `active_tab` naming that closed
            // tab until the transport disconnects them.
            if !self.tabs.is_empty() && !self.tabs.contains_key(&client.get_active_tab()) {
                consistency_violations.push(SessionConsistencyError::ActiveTabMissing {
                    client_id: client.get_client_id(),
                    tab_id: client.get_active_tab(),
                });
            }

            // Each remembered focus must point at a real pane that is a leaf of
            // the tab it was focused in.
            for (&tab_id, &focused_pane_id) in client.list_focused_panes() {
                if self.panes.get_pane_record_by_id(focused_pane_id).is_none() {
                    consistency_violations.push(SessionConsistencyError::FocusPaneNotInRegistry {
                        client_id: client.get_client_id(),
                        tab_id,
                        pane_id: focused_pane_id,
                    });
                }

                match self.tabs.get(&tab_id) {
                    None => consistency_violations.push(SessionConsistencyError::FocusTabMissing {
                        client_id: client.get_client_id(),
                        tab_id,
                    }),
                    Some(tab) if !tab.layout.contains_pane(focused_pane_id) => {
                        consistency_violations.push(SessionConsistencyError::FocusTargetMissing {
                            client_id: client.get_client_id(),
                            tab_id,
                            pane_id: focused_pane_id,
                        });
                    }
                    Some(_) => {}
                }
            }

            // The pane a client is zoomed on must have a registry record and be
            // a leaf of the tab it is zoomed in. Removing a pane drops every
            // zoom on it.
            for (&tab_id, &zoomed_pane_id) in client.list_zoomed_panes() {
                let is_live_leaf = self.panes.get_pane_record_by_id(zoomed_pane_id).is_some()
                    && self
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|tab| tab.layout.contains_pane(zoomed_pane_id));
                if !is_live_leaf {
                    consistency_violations.push(SessionConsistencyError::ZoomTargetMissing {
                        client_id: client.get_client_id(),
                        tab_id,
                        pane_id: zoomed_pane_id,
                    });
                }
            }
        }

        if consistency_violations.is_empty() {
            Ok(())
        } else {
            Err(consistency_violations)
        }
    }
}

#[cfg(test)]
mod tests;
