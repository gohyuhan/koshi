//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use koshi_core::{
    constant::{MAX_FLOATING_PANES_PER_SESSION, MAX_TAB_FOCUS_MRU_ENTRY_COUNT},
    geometry::{FloatingPaneSize, PixelCellSize, Size},
    ids::{ClientId, PaneId, SessionId, TabId},
};
use koshi_layout::tree::LayoutNode;
use koshi_pane::{pane::lifecycle::PaneLifecycle, registry::PaneRegistry};
use serde::{Deserialize, Serialize};

use crate::{
    client::{Client, ClientRegistry},
    error::{FloatingSetError, InvalidTransition, SessionConsistencyError},
    session::lifecycle::{SessionLifecycle, SessionLifecycleEvent},
};

/// One tab: its name, bar position, layout tree, and the panes it focused,
/// most-recent first.
///
/// A tab holds no layout mode. Zoom is a client property: it lives on
/// [`crate::client::Client`] as `zoomed_pane_id_by_tab_id`, and two clients on this tab can
/// hold different zoom. The tab holds the tree that every client solves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    tab_id: TabId,
    tab_name: String,
    tab_index: usize,
    layout: LayoutNode,
    /// Panes this tab has focused, most-recent first, with at most one entry
    /// per pane — re-focusing moves a pane to the front instead of adding a
    /// duplicate. Capped at [`MAX_TAB_FOCUS_MRU_ENTRY_COUNT`]; focus recovery walks
    /// it newest-first to pick the inheriting pane when the focused one
    /// disappears.
    focus_mru: Vec<PaneId>,
}

impl Tab {
    /// A freshly created tab showing a single pane, with no focus recorded
    /// yet; `root_pane_id` is its only layout leaf.
    #[must_use]
    pub fn from_root_pane(
        tab_id: TabId,
        tab_name: String,
        tab_index: usize,
        root_pane_id: PaneId,
    ) -> Self {
        Self {
            tab_id,
            tab_name,
            tab_index,
            layout: LayoutNode::Pane(root_pane_id),
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

    /// Records `pane_id` as the most-recently focused: moves it to the front,
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
}

/// One floating pane: the pane, the size it asks for, and the size it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingMember {
    /// The pane: a record in [`Session::panes`] that no tab's layout holds as
    /// a leaf.
    pub pane_id: PaneId,
    /// The size the pane asks for, per axis.
    pub desired_size: FloatingPaneSize,
    /// The size in cells that the pane holds.
    pub solved_size: Size,
}

/// The floating panes of one session, in creation order: a new member is
/// appended, and a removal keeps the order of the rest.
///
/// [`FloatingSet::add_member`] holds each pane once and at most
/// [`MAX_FLOATING_PANES_PER_SESSION`] members.
/// [`Session::remove_floating_member`] removes a member and every client's
/// view of it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingSet {
    members: Vec<FloatingMember>,
}

impl FloatingSet {
    /// Every member, in creation order.
    #[must_use]
    pub fn list_members(&self) -> &[FloatingMember] {
        &self.members
    }

    /// Append `floating_member` as the newest member.
    ///
    /// # Errors
    ///
    /// [`FloatingSetError::DuplicatePane`] when the set already holds
    /// `floating_member.pane_id`, else [`FloatingSetError::TooManyPanes`] when
    /// the set holds [`MAX_FLOATING_PANES_PER_SESSION`] members or more. The set
    /// does not change on an error.
    pub fn add_member(&mut self, floating_member: FloatingMember) -> Result<(), FloatingSetError> {
        if self
            .members
            .iter()
            .any(|member| member.pane_id == floating_member.pane_id)
        {
            return Err(FloatingSetError::DuplicatePane {
                pane_id: floating_member.pane_id,
            });
        }
        if self.members.len() >= MAX_FLOATING_PANES_PER_SESSION {
            return Err(FloatingSetError::TooManyPanes);
        }
        self.members.push(floating_member);
        Ok(())
    }
}

/// One running session: the aggregate root owning the tabs, the floating
/// panes, the pane registry, and the attached-client registry.
///
/// Anything one client may see differently from another — focus, viewport,
/// input mode — lives on that client's entry in [`ClientRegistry`], never as a
/// session-global field. Two attached clients can look at different tabs and
/// panes at the same time. `should_start_locked` is the mode the session hands the
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
    /// The session's floating panes, in creation order. Each attached client
    /// holds its own view of each one.
    pub floating_set: FloatingSet,
    /// Runtime metadata for every pane in every tab and every floating pane.
    /// Layout trees and the floating set name each pane by its id.
    pub panes: PaneRegistry,
    /// The clients currently attached.
    pub clients: ClientRegistry,
    /// True while the next client to attach must start in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked). A profile
    /// carrying the `lock` marker sets it; [`Session::take_start_lock`] reads
    /// it and clears it: exactly one attach is locked. A session seeded
    /// without that marker holds `false` and locks nobody.
    pub should_start_locked: bool,

    /// A restart seeded a new shell after the carried session could not be
    /// restored. The statusline shows the notice until input reaches a pane.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_recovery_notice_visible: bool,

    /// Generation of committed layout, membership, and shared sizing inputs.
    placement_revision: u64,

    lifecycle: SessionLifecycle,
}

impl Session {
    /// A session with no tabs and no panes, holding the supplied client
    /// registry. Starts in `Starting` with `should_start_locked`
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
            floating_set: FloatingSet::default(),
            panes: PaneRegistry::new(),
            clients: client_registry,
            should_start_locked: false,
            is_recovery_notice_visible: false,
            placement_revision: 0,
            lifecycle: SessionLifecycle::Starting,
        }
    }

    /// Whether this attach must start in
    /// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked), clearing the
    /// flag as it reads it.
    ///
    /// Reads [`should_start_locked`](Self::should_start_locked) and clears it in one step:
    /// it returns `true` at most once per session.
    pub fn take_start_lock(&mut self) -> bool {
        std::mem::take(&mut self.should_start_locked)
    }

    /// The session's current lifecycle state.
    pub fn get_lifecycle(&self) -> &SessionLifecycle {
        &self.lifecycle
    }

    /// The generation of this session's committed placement inputs.
    #[must_use]
    pub fn get_placement_revision(&self) -> u64 {
        self.placement_revision
    }

    /// Whether the session revision can advance without wrapping.
    #[must_use]
    pub fn can_advance_placement_revision(&self) -> bool {
        self.placement_revision < u64::MAX
    }

    /// Advance the session revision once. Returns `false` at the maximum value.
    pub fn advance_placement_revision(&mut self) -> bool {
        let Some(next_revision) = self.placement_revision.checked_add(1) else {
            return false;
        };
        self.placement_revision = next_revision;
        true
    }

    /// Apply a `lifecycle_event`, advancing the session's state, or return
    /// [`InvalidTransition`] if the move is illegal from the current state.
    /// Crate-internal: callers drive the lifecycle through the typed wrappers
    /// ([`Session::attach_client`], [`Session::detach_client`],
    /// [`Session::request_session_stop`], [`Session::complete_session_stop`]) or the tab
    /// operations. Each caller
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
        let displaced_client = self.clients.attach_client(client);
        let _ = self.update_lifecycle(SessionLifecycleEvent::ClientAttached);
        displaced_client
    }

    /// Detach the client `client_id`, returning the removed record (`None` if it
    /// was not attached). When it was the *last* attached client the session
    /// drops to `Detaching` — its tabs and panes stay alive; detaching one of
    /// several clients leaves the session `Running`.
    pub fn detach_client(&mut self, client_id: ClientId) -> Option<Client> {
        let detached_client = self.clients.detach_client(client_id);
        if !self.clients.has_clients() {
            // Only a `Running` session moves to `Detaching`. `Starting`,
            // `Detaching`, `Stopping` and `Stopped` reject the event and keep
            // the state they had.
            let _ = self.update_lifecycle(SessionLifecycleEvent::LastClientDetached);
        }
        detached_client
    }

    /// The pane region to size tab `tab_id` against: each viewing client's own
    /// pane area, reduced to the per-axis minimum (`column_count` and `row_count`
    /// independently), which is the largest grid that fits inside *every*
    /// viewer on *both* axes.
    ///
    /// Every attached client whose [`Client::get_active_tab_id`] is `tab_id`
    /// contributes its [`Client::get_pane_area`]; a viewer that reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving)
    /// contributes nothing. Returns `None` when no viewer of `tab_id`
    /// contributes a size. The result does not depend on which client (if any)
    /// issued the command, nor on the order the viewers attached.
    #[must_use]
    pub fn get_tab_size(&self, tab_id: TabId) -> Option<Size> {
        self.clients
            .list_attached_clients()
            .filter(|client| client.get_active_tab_id() == tab_id)
            .filter_map(Client::get_pane_area)
            .reduce(Size::compute_minimum_axes)
    }

    /// The cell size of the earliest-attached client viewing `tab_id` that
    /// reported one, ties broken by the lower client id. `None` when no client
    /// viewing `tab_id` reported a cell size.
    #[must_use]
    pub fn get_tab_cell_size(&self, tab_id: TabId) -> Option<PixelCellSize> {
        self.clients
            .list_attached_clients()
            .filter(|client| {
                client.get_active_tab_id() == tab_id && client.get_cell_size().is_some()
            })
            .min_by_key(|client| (client.get_attached_at(), client.get_client_id()))
            .and_then(Client::get_cell_size)
    }

    /// Remove `pane_id` from the floating set, keeping the order of the other
    /// members, and from every attached client's floating view: its stored
    /// view, its floating focus order entry, and a floating focus on it.
    /// Returns the removed member, or `None`, changing nothing, when `pane_id`
    /// is not floating.
    pub fn remove_floating_member(&mut self, pane_id: PaneId) -> Option<FloatingMember> {
        let member_index = self
            .floating_set
            .members
            .iter()
            .position(|floating_member| floating_member.pane_id == pane_id)?;
        let floating_member = self.floating_set.members.remove(member_index);
        for client in self.clients.list_attached_clients_mut() {
            client.remove_floating_pane_view(pane_id);
        }
        Some(floating_member)
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
    /// The checks run in this order:
    ///
    /// 1. Each tab's map key, then each of its layout leaves against the pane
    ///    registry, tab by tab.
    /// 2. Each bar index that two tabs claim.
    /// 3. Each pane that two layouts hold.
    /// 4. The number of floating members against
    ///    [`MAX_FLOATING_PANES_PER_SESSION`], then each floating member against
    ///    the pane registry, the other members and the layouts.
    /// 5. Each registry record against the layouts and the floating members.
    /// 6. Each attached client's session id, active tab, focus, zoom, floating
    ///    views, floating focus order and floating focus.
    ///
    /// See [`SessionConsistencyError`] for the individual checks. Each check
    /// walks its subjects by id, by bar index, or in floating focus order: one
    /// session reports the same list on every call.
    pub fn validate_session_consistency(&self) -> Result<(), Vec<SessionConsistencyError>> {
        let mut consistency_violations = vec![];
        // Pane id -> the tabs whose layout holds it as a leaf, in pane id
        // order. Built once here, then read to check the leaf/registry
        // relationship in both directions.
        let mut tab_ids_by_pane_id: BTreeMap<PaneId, Vec<TabId>> = BTreeMap::new();
        // Bar position -> how many tabs claim it.
        let mut tab_count_by_index: BTreeMap<usize, usize> = BTreeMap::new();

        for (tab_id, tab) in self.tabs.iter() {
            // Every tab is keyed under its own id.
            if *tab_id != tab.tab_id {
                consistency_violations.push(SessionConsistencyError::TabKeyMismatch {
                    stored_tab_id: *tab_id,
                    reported_tab_id: tab.tab_id,
                });
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
                // A `Removed` pane still in a layout is a violation.
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

        // A pane is a layout leaf at most once across all tabs.
        for (pane_id, tab_ids) in &tab_ids_by_pane_id {
            if tab_ids.len() > 1 {
                consistency_violations.push(SessionConsistencyError::PaneInMultipleLayouts {
                    pane_id: *pane_id,
                    tab_ids: tab_ids.clone(),
                });
            }
        }

        // The floating set holds at most `MAX_FLOATING_PANES_PER_SESSION`
        // entries.
        let floating_member_count = self.floating_set.list_members().len();
        if floating_member_count > MAX_FLOATING_PANES_PER_SESSION {
            consistency_violations.push(SessionConsistencyError::TooManyFloatingPanes {
                member_count: floating_member_count,
            });
        }

        // Pane id -> how many times the floating set lists it.
        let mut floating_member_count_by_pane_id: BTreeMap<PaneId, usize> = BTreeMap::new();
        for floating_member in self.floating_set.list_members() {
            *floating_member_count_by_pane_id
                .entry(floating_member.pane_id)
                .or_insert(0) += 1;
        }

        // A floating member is a registry pane that is not `Removed`, listed
        // once, and a leaf of no layout.
        for (&pane_id, &member_count) in &floating_member_count_by_pane_id {
            match self.panes.get_pane_record_by_id(pane_id) {
                None => consistency_violations
                    .push(SessionConsistencyError::FloatingPaneNotInRegistry { pane_id }),
                Some(pane_record) if *pane_record.get_lifecycle() == PaneLifecycle::Removed => {
                    consistency_violations
                        .push(SessionConsistencyError::RemovedPaneInFloatingSet { pane_id });
                }
                Some(_) => {}
            }
            if member_count > 1 {
                consistency_violations
                    .push(SessionConsistencyError::DuplicateFloatingPane { pane_id });
            }
            if let Some(tab_ids) = tab_ids_by_pane_id.get(&pane_id) {
                consistency_violations.push(SessionConsistencyError::FloatingPaneInLayout {
                    pane_id,
                    tab_ids: tab_ids.clone(),
                });
            }
        }

        // Every live or `Exited` record must be a leaf somewhere or a floating
        // member; a `Removed` record must not linger in the registry at all.
        for pane_record in self.panes.list_pane_records() {
            if *pane_record.get_lifecycle() == PaneLifecycle::Removed {
                consistency_violations.push(SessionConsistencyError::LingeringRemovedRecord {
                    pane_id: pane_record.get_pane_id(),
                });
            } else if !tab_ids_by_pane_id.contains_key(&pane_record.get_pane_id())
                && !floating_member_count_by_pane_id.contains_key(&pane_record.get_pane_id())
            {
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
            // tab closing leaves every client's `active_tab_id` naming that closed
            // tab until the transport disconnects them.
            if !self.tabs.is_empty() && !self.tabs.contains_key(&client.get_active_tab_id()) {
                consistency_violations.push(SessionConsistencyError::ActiveTabMissing {
                    client_id: client.get_client_id(),
                    tab_id: client.get_active_tab_id(),
                });
            }

            // Each remembered focus must point at a real pane that is a leaf of
            // the tab it was focused in.
            for (&tab_id, &focused_pane_id) in client
                .list_focused_pane_ids()
                .iter()
                .collect::<BTreeMap<_, _>>()
            {
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
                    Some(tab) if !tab.layout.has_pane(focused_pane_id) => {
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
            for (&tab_id, &zoomed_pane_id) in client
                .list_zoomed_pane_ids()
                .iter()
                .collect::<BTreeMap<_, _>>()
            {
                let is_live_leaf = self.panes.get_pane_record_by_id(zoomed_pane_id).is_some()
                    && self
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|tab| tab.layout.has_pane(zoomed_pane_id));
                if !is_live_leaf {
                    consistency_violations.push(SessionConsistencyError::ZoomTargetMissing {
                        client_id: client.get_client_id(),
                        tab_id,
                        pane_id: zoomed_pane_id,
                    });
                }
            }

            // Each stored floating view must name a floating member.
            for &viewed_pane_id in client
                .list_floating_pane_views()
                .keys()
                .collect::<BTreeSet<_>>()
            {
                if !floating_member_count_by_pane_id.contains_key(&viewed_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::FloatingViewTargetMissing {
                            client_id: client.get_client_id(),
                            pane_id: viewed_pane_id,
                        },
                    );
                }
            }

            // Each floating focus order entry must name a floating member, once.
            let mut listed_pane_ids: BTreeSet<PaneId> = BTreeSet::new();
            for &ordered_pane_id in client.list_floating_pane_focus_order() {
                if !floating_member_count_by_pane_id.contains_key(&ordered_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::FloatingFocusOrderTargetMissing {
                            client_id: client.get_client_id(),
                            pane_id: ordered_pane_id,
                        },
                    );
                }
                if !listed_pane_ids.insert(ordered_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::DuplicateFloatingFocusOrderEntry {
                            client_id: client.get_client_id(),
                            pane_id: ordered_pane_id,
                        },
                    );
                }
            }

            // The floating focus must name a floating member that is last in
            // the floating focus order and that this client has not minimized.
            if let Some(focused_floating_pane_id) = client.get_focused_floating_pane_id() {
                if !floating_member_count_by_pane_id.contains_key(&focused_floating_pane_id) {
                    consistency_violations.push(
                        SessionConsistencyError::FocusedFloatingPaneMissing {
                            client_id: client.get_client_id(),
                            pane_id: focused_floating_pane_id,
                        },
                    );
                }
                if client.list_floating_pane_focus_order().last() != Some(&focused_floating_pane_id)
                {
                    consistency_violations.push(
                        SessionConsistencyError::FocusedFloatingPaneNotOnTop {
                            client_id: client.get_client_id(),
                            pane_id: focused_floating_pane_id,
                        },
                    );
                }
                if client
                    .get_floating_pane_view(focused_floating_pane_id)
                    .is_minimized
                {
                    consistency_violations.push(
                        SessionConsistencyError::FocusedFloatingPaneMinimized {
                            client_id: client.get_client_id(),
                            pane_id: focused_floating_pane_id,
                        },
                    );
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
