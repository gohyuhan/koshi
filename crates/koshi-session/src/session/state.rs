//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

use koshi_core::{
    constant::MAX_TAB_FOCUS_MRU,
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
    id: TabId,
    name: String,
    index: usize,
    layout: LayoutNode,
    lifecycle: TabLifecycle,
    /// Panes this tab has focused, most-recent first, with at most one entry
    /// per pane — re-focusing moves a pane to the front instead of adding a
    /// duplicate. Capped at [`MAX_TAB_FOCUS_MRU`]; focus recovery walks
    /// it newest-first to pick the inheriting pane when the focused one
    /// disappears.
    focus_mru: Vec<PaneId>,
}

impl Tab {
    /// A freshly created tab showing a single pane. Starts in `Creating`
    /// with no focus recorded yet; `root_pane` is its only layout leaf.
    #[must_use]
    pub fn new(id: TabId, name: String, tab_index: usize, root_pane: PaneId) -> Self {
        Self {
            id,
            name,
            index: tab_index,
            layout: LayoutNode::Pane(root_pane),
            lifecycle: TabLifecycle::Creating,
            focus_mru: Vec::new(),
        }
    }

    /// This tab's stable id, matching its key in [`Session::tabs`].
    #[must_use]
    pub fn id(&self) -> TabId {
        self.id
    }

    /// The name shown for this tab in the tab bar.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// This tab's display position in the bar; kept a dense `0..n` across the
    /// session's tabs by the tab operations.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// This tab's layout tree.
    #[must_use]
    pub fn layout(&self) -> &LayoutNode {
        &self.layout
    }

    /// Set this tab's display position. Callers keep positions a dense `0..n`
    /// across the session's tabs.
    pub fn update_index(&mut self, index: usize) {
        self.index = index;
    }

    /// Replace this tab's layout tree.
    pub fn update_layout(&mut self, layout: LayoutNode) {
        self.layout = layout;
    }

    /// Records `pane` as the most-recently focused: moves it to the front,
    /// keeping one entry per pane, and drops the oldest once the cap is hit.
    pub fn record_focus_mru(&mut self, pane: PaneId) {
        self.focus_mru.retain(|&p| p != pane);
        self.focus_mru.insert(0, pane);
        if self.focus_mru.len() as u16 > MAX_TAB_FOCUS_MRU {
            self.focus_mru.pop();
        }
    }

    /// The panes this tab has focused, most-recent first.
    pub fn focus_mru(&self) -> &[PaneId] {
        &self.focus_mru
    }

    /// Remove `pane_id` from this tab's focus history.
    pub fn remove_focus_mru(&mut self, pane_id: PaneId) {
        self.focus_mru.retain(|&p| p != pane_id);
    }

    /// This tab's current lifecycle state.
    pub fn lifecycle(&self) -> &TabLifecycle {
        &self.lifecycle
    }
}

/// The configuration a session captured when it started. Carries no fields.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionConfig;

/// Handle to a session's plugin runtime. Carries no fields.
#[derive(Debug)]
pub struct PluginRuntimeHandle;

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
    pub id: SessionId,
    /// Human-facing name; attach and list address sessions by it.
    pub name: String,
    /// When the session was created. Supplied by the caller at the creation
    /// boundary, never read from the clock here.
    pub created_at: SystemTime,
    /// The session's tabs, keyed by id. Display order is not the map order: it
    /// lives on each tab as [`Tab::index`], and reordering tabs moves no map
    /// entry.
    pub tabs: BTreeMap<TabId, Tab>,
    /// Runtime metadata for every pane in every tab; layout trees hold
    /// only the ids.
    pub panes: PaneRegistry,
    /// The clients currently attached.
    pub clients: ClientRegistry,
    /// The configuration this session started with.
    pub config_snapshot: SessionConfig,
    /// The session's plugin runtime, once one is running. A serialized session
    /// leaves it out, and a deserialized one reads it back as `None`.
    #[serde(skip)]
    pub plugin_runtime_ref: Option<PluginRuntimeHandle>,
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
    /// A session with no tabs, no panes, and no plugin runtime yet, holding the
    /// supplied client registry. Starts in `Starting` with `start_locked`
    /// `false`. `created_at` is supplied by the caller at the creation
    /// boundary, never read from the clock here.
    #[must_use]
    pub fn new(
        id: SessionId,
        name: String,
        created_at: SystemTime,
        client_registry: ClientRegistry,
    ) -> Self {
        Self {
            id,
            name,
            created_at,
            tabs: BTreeMap::new(),
            panes: PaneRegistry::new(),
            clients: client_registry,
            config_snapshot: SessionConfig,
            plugin_runtime_ref: None,
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
    pub fn lifecycle(&self) -> &SessionLifecycle {
        &self.lifecycle
    }

    /// Apply a lifecycle `event`, advancing the session's state, or return
    /// [`InvalidTransition`] if the move is illegal from the current state.
    /// Crate-internal — callers drive the lifecycle through the typed wrappers
    /// ([`Session::attach_client`], [`Session::detach_client`],
    /// [`Session::request_stop`], [`Session::complete_stop`]) or the tab
    /// operations, so the firing conditions stay in one place. Each caller
    /// decides whether a rejected event is an expected no-op to ignore (a
    /// re-attach to an already-`Running` session) or a fault to abort on (a tab
    /// created under a wound-down session).
    pub(crate) fn update_lifecycle(
        &mut self,
        event: SessionLifecycleEvent,
    ) -> Result<(), InvalidTransition> {
        self.lifecycle = self.lifecycle.transition(event)?;
        Ok(())
    }

    /// Attach `client` and mark the session live, returning the record it
    /// displaced when that id was already attached (a re-attach replaces in
    /// place), else `None`. `ClientAttached` moves a `Detaching` (no-client)
    /// session to `Running`; from `Starting`, `Running`, `Stopping` or
    /// `Stopped` it is rejected and the lifecycle stays as it was. The client
    /// is registered either way.
    pub fn attach_client(&mut self, client: Client) -> Option<Client> {
        let displaced = self.clients.attach(client);
        let _ = self.update_lifecycle(SessionLifecycleEvent::ClientAttached);
        displaced
    }

    /// Detach the client `client_id`, returning the removed record (`None` if it
    /// was not attached). When it was the *last* attached client the session
    /// drops to `Detaching` — its tabs and panes stay alive; detaching one of
    /// several clients leaves the session `Running`.
    pub fn detach_client(&mut self, client_id: ClientId) -> Option<Client> {
        let removed = self.clients.detach(client_id);
        if self.clients.is_empty() {
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
    /// Every attached client whose [`Client::active_tab`] is `tab_id`
    /// contributes its [`Client::pane_area`]; a viewer that reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving)
    /// contributes nothing. Returns `None` when no viewer of `tab_id`
    /// contributes a size. The result does not depend on which client (if any)
    /// issued the command, nor on the order the viewers attached.
    #[must_use]
    pub fn tab_viewport(&self, tab_id: TabId) -> Option<Size> {
        self.clients
            .list_attached()
            .filter(|client| client.active_tab() == tab_id)
            .filter_map(Client::pane_area)
            .reduce(Size::min_axes)
    }

    /// Request shutdown: move a `Starting`, `Running` or `Detaching` session to
    /// `Stopping`. State is retained: stopping destroys no tabs, panes or
    /// clients.
    pub fn request_stop(&mut self) {
        // Idempotent: requesting a stop on an already-`Stopping`/`Stopped`
        // session is rejected and changes nothing.
        let _ = self.update_lifecycle(SessionLifecycleEvent::StopRequested);
    }

    /// Finish shutdown once teardown is done, moving `Stopping` to the terminal
    /// `Stopped`.
    pub fn complete_stop(&mut self) {
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
    /// returned violations arrive in no defined order.
    pub fn validate(&self) -> Result<(), Vec<SessionConsistencyError>> {
        let mut violations = vec![];
        // Pane id -> the tabs whose layout holds it as a leaf. Built once here,
        // then reused to check the leaf/registry relationship in both directions.
        let mut panes_in_layout_nodes: HashMap<PaneId, Vec<TabId>> = HashMap::new();
        // Bar position -> how many tabs claim it, to catch collisions.
        let mut tab_index_counts: HashMap<usize, usize> = HashMap::new();

        for (tab_id, tab) in self.tabs.iter() {
            // Every tab is keyed under its own id.
            if *tab_id != tab.id {
                violations.push(SessionConsistencyError::TabKeyMismatch {
                    key: *tab_id,
                    tab_id: tab.id,
                });
            }

            // A `Closed` tab is terminal and should have left the map.
            if *tab.lifecycle() == TabLifecycle::Closed {
                violations.push(SessionConsistencyError::LingeringClosedTab { tab: tab.id });
            }

            *tab_index_counts.entry(tab.index).or_insert(0) += 1;

            for pane_id in tab.layout.leaf_panes() {
                panes_in_layout_nodes
                    .entry(pane_id)
                    .or_default()
                    .push(tab.id);

                let Some(record) = self.panes.get(pane_id) else {
                    violations.push(SessionConsistencyError::PaneNotInRegistry {
                        tab: tab.id,
                        pane: pane_id,
                    });
                    continue;
                };
                // A `Removed` pane should be gone from both layout and registry.
                if *record.lifecycle() == PaneLifecycle::Removed {
                    violations.push(SessionConsistencyError::RemovedPaneInLayout {
                        tab: tab.id,
                        pane: pane_id,
                    });
                }
            }
        }

        // No two tabs may claim the same bar position.
        for (index, count) in &tab_index_counts {
            if *count > 1 {
                violations.push(SessionConsistencyError::DuplicateTabIndex { index: *index });
            }
        }

        // A pane belongs to exactly one tab at one position.
        for (pane_id, tab_ids) in &panes_in_layout_nodes {
            if tab_ids.len() > 1 {
                violations.push(SessionConsistencyError::PaneInMultipleLayouts {
                    pane: *pane_id,
                    tabs: tab_ids.clone(),
                });
            }
        }

        // Every live or `Exited` record must be a leaf somewhere; a `Removed`
        // record must not linger in the registry at all.
        for pane in self.panes.list() {
            if *pane.lifecycle() == PaneLifecycle::Removed {
                violations
                    .push(SessionConsistencyError::LingeringRemovedRecord { pane: pane.id() });
            } else if !panes_in_layout_nodes.contains_key(&pane.id()) {
                violations.push(SessionConsistencyError::OrphanedPaneRecord {
                    pane: pane.id(),
                    lifecycle: *pane.lifecycle(),
                });
            }
        }

        for client in self.clients.list_attached() {
            // A client in this registry must belong to this session.
            if client.session_id() != self.id {
                violations.push(SessionConsistencyError::ClientSessionMismatch {
                    client: client.id(),
                    found: client.session_id(),
                });
            }

            // The tab a client is currently showing must exist. Checked only
            // while the session still holds tabs: a session emptied by its last
            // tab closing leaves every client's `active_tab` naming that closed
            // tab until the transport disconnects them.
            if !self.tabs.is_empty() && !self.tabs.contains_key(&client.active_tab()) {
                violations.push(SessionConsistencyError::ActiveTabMissing {
                    client: client.id(),
                    tab: client.active_tab(),
                });
            }

            // Each remembered focus must point at a real pane that is a leaf of
            // the tab it was focused in.
            for (&tab_id, &focused_pane_id) in client.focused_panes() {
                if self.panes.get(focused_pane_id).is_none() {
                    violations.push(SessionConsistencyError::FocusPaneNotInRegistry {
                        client: client.id(),
                        tab: tab_id,
                        pane: focused_pane_id,
                    });
                }

                match self.tabs.get(&tab_id) {
                    None => violations.push(SessionConsistencyError::FocusTabMissing {
                        client: client.id(),
                        tab: tab_id,
                    }),
                    Some(tab) if !tab.layout.contains_pane(focused_pane_id) => {
                        violations.push(SessionConsistencyError::FocusTargetMissing {
                            client: client.id(),
                            tab: tab_id,
                            pane: focused_pane_id,
                        });
                    }
                    Some(_) => {}
                }
            }

            // The pane a client is zoomed on must have a registry record and be
            // a leaf of the tab it is zoomed in. Removing a pane drops every
            // zoom on it.
            for (&tab_id, &zoomed_pane_id) in client.zoomed_panes() {
                let live_leaf = self.panes.get(zoomed_pane_id).is_some()
                    && self
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|tab| tab.layout.contains_pane(zoomed_pane_id));
                if !live_leaf {
                    violations.push(SessionConsistencyError::ZoomTargetMissing {
                        client: client.id(),
                        tab: tab_id,
                        pane: zoomed_pane_id,
                    });
                }
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

#[cfg(test)]
mod tests;
