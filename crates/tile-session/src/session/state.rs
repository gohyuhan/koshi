//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use tile_core::{
    constant::MAX_TAB_FOCUS_MRU,
    geometry::Size,
    ids::{ClientId, PaneId, SessionId, TabId},
};
use tile_layout::{mode::LayoutMode, tree::LayoutNode};
use tile_pane::{pane::lifecycle::PaneLifecycle, registry::PaneRegistry};

use crate::{
    client::{Client, ClientRegistry},
    error::{InvalidTransition, SessionConsistencyError},
    session::lifecycle::{SessionLifecycle, SessionLifecycleEvent, TabLifecycle},
};

/// One tab: its name, bar position, layout tree and mode, lifecycle, and the
/// panes it focused, most-recent first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    id: TabId,
    name: String,
    index: usize,
    layout: LayoutNode,
    layout_mode: LayoutMode,
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
            layout_mode: LayoutMode::Tiled,
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

    /// How this tab's layout is arranged.
    #[must_use]
    pub fn layout_mode(&self) -> LayoutMode {
        self.layout_mode
    }

    /// Rename this tab.
    pub fn update_name(&mut self, name: String) {
        self.name = name;
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

    /// Set how this tab's layout is solved. The tree itself is untouched:
    /// entering fullscreen hides the other panes at solve time, and leaving
    /// it restores the exact prior layout.
    pub fn update_layout_mode(&mut self, mode: LayoutMode) {
        self.layout_mode = mode;
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

/// The configuration a session captured when it started. A snapshot, not a
/// live reference: a config reload builds a new snapshot for new sessions
/// instead of rewriting a running one underneath its clients. Placeholder
/// shell: the config model fills it in.
#[derive(Debug)]
pub struct SessionConfig;

/// Handle to a session's plugin runtime. Placeholder shell: the plugin
/// host fills it in.
#[derive(Debug)]
pub struct PluginRuntimeHandle;

/// One running session: the aggregate root owning the tabs, the pane
/// registry, and the attached-client registry.
///
/// Anything one client may see differently from another — focus, viewport,
/// input mode — lives on that client's entry in [`ClientRegistry`], never
/// as a session-global field: two attached clients must be able to look at
/// different tabs and panes at the same time.
#[derive(Debug)]
pub struct Session {
    /// Unique id, stable for the session's whole life.
    pub id: SessionId,
    /// Human-facing name; attach and list address sessions by it.
    pub name: String,
    /// The session's tabs, keyed by id. Display order is not the map
    /// order — it lives on each tab, so reordering tabs never moves map
    /// entries.
    pub tabs: BTreeMap<TabId, Tab>,
    /// Runtime metadata for every pane in every tab; layout trees hold
    /// only the ids.
    pub panes: PaneRegistry,
    /// The clients currently attached.
    pub clients: ClientRegistry,
    /// The configuration this session started with.
    pub config_snapshot: SessionConfig,
    /// The session's plugin runtime, once one is running.
    pub plugin_runtime_ref: Option<PluginRuntimeHandle>,

    lifecycle: SessionLifecycle,
}

impl Session {
    /// A session with no tabs, no panes, and no plugin runtime yet, holding the
    /// supplied client registry.
    #[must_use]
    pub fn new(id: SessionId, name: String, client_registry: ClientRegistry) -> Self {
        Self {
            id,
            name,
            tabs: BTreeMap::new(),
            panes: PaneRegistry::new(),
            clients: client_registry,
            config_snapshot: SessionConfig,
            plugin_runtime_ref: None,
            lifecycle: SessionLifecycle::Starting,
        }
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
    /// place), else `None`. `ClientAttached` only revives a `Detaching`
    /// (no-client) session; attaching to an already-`Running` one leaves the
    /// lifecycle unchanged.
    pub fn attach_client(&mut self, client: Client) -> Option<Client> {
        let displaced = self.clients.attach(client);
        // `ClientAttached` only revives a `Detaching` session; from `Running`
        // it is an expected no-op, so a rejected transition is not a fault here.
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
            // Already stopping/stopped sessions reject the park; that is fine —
            // a session winding down stays wound down when its last client goes.
            let _ = self.update_lifecycle(SessionLifecycleEvent::LastClientDetached);
        }
        removed
    }

    /// The viewport to size tab `tab_id` against: the per-axis minimum of the
    /// viewports of the clients viewing it (`cols` = the smallest width, `rows`
    /// = the smallest height, taken independently), or `None` when no client is.
    ///
    /// A single pane is one PTY of one cell grid, so every client viewing it
    /// shares its dimensions. The per-axis minimum is the largest grid that
    /// fits inside *every* viewer on *both* axes; larger viewers letterbox the
    /// unused margin. It is independent of which client (if any) issued the
    /// command.
    ///
    /// This fixes reconciliation to *smallest-wins* — the interim policy for the
    /// current single-client-per-command paths. A configurable multi-client
    /// reconciliation policy (smallest / largest / latest / manual), re-run when
    /// clients attach and detach, generalizes this later.
    #[must_use]
    pub fn tab_viewport(&self, tab_id: TabId) -> Option<Size> {
        self.clients
            .list_attached()
            .filter(|client| client.active_tab() == tab_id)
            .map(Client::viewport)
            .reduce(|a, b| Size {
                cols: a.cols.min(b.cols),
                rows: a.rows.min(b.rows),
            })
    }

    /// Request shutdown: move a `Running` or `Detaching` session to `Stopping`.
    /// State is retained — stopping destroys no tabs or panes — so a stopped
    /// session can be persisted and restored later.
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
    /// Run before a snapshot or render is built from the session: the tabs and
    /// their layout trees, the pane registry, each attached client's focus, and
    /// each tab's own identity can drift apart, and a corrupt state must be
    /// caught — and named — before it reaches a client. Collecting rather than
    /// failing fast means one call surfaces the whole picture, not just the
    /// first fault. See [`SessionConsistencyError`] for the individual checks.
    pub fn validate(&self) -> Result<(), Vec<SessionConsistencyError>> {
        let mut consistency_error = vec![];
        // Pane id -> the tabs whose layout holds it as a leaf. Built once here,
        // then reused to check the leaf/registry relationship in both directions.
        let mut panes_in_layout_nodes: HashMap<PaneId, Vec<TabId>> = HashMap::new();
        // Bar position -> how many tabs claim it, to catch collisions.
        let mut tab_index_counts: HashMap<usize, usize> = HashMap::new();

        for (tab_id, tab) in self.tabs.iter() {
            // A tab keyed under anything but its own id breaks every by-id lookup.
            if *tab_id != tab.id {
                consistency_error.push(SessionConsistencyError::TabKeyMismatch {
                    key: *tab_id,
                    tab_id: tab.id,
                });
            }

            // A `Closed` tab is terminal and should have left the map.
            if *tab.lifecycle() == TabLifecycle::Closed {
                consistency_error.push(SessionConsistencyError::LingeringClosedTab { tab: tab.id });
            }

            *tab_index_counts.entry(tab.index).or_insert(0) += 1;

            for pane_id in tab.layout.leaf_panes() {
                panes_in_layout_nodes
                    .entry(pane_id)
                    .or_default()
                    .push(tab.id);

                let Some(p) = self.panes.get(pane_id) else {
                    consistency_error.push(SessionConsistencyError::PaneNotInRegistry {
                        tab: tab.id,
                        pane: pane_id,
                    });
                    continue;
                };
                // A `Removed` pane should be gone from both layout and registry.
                if *p.lifecycle() == PaneLifecycle::Removed {
                    consistency_error.push(SessionConsistencyError::RemovedPaneInLayout {
                        tab: tab.id,
                        pane: pane_id,
                    });
                }
            }
        }

        // No two tabs may claim the same bar position.
        for (index, count) in &tab_index_counts {
            if *count > 1 {
                consistency_error
                    .push(SessionConsistencyError::DuplicateTabIndex { index: *index });
            }
        }

        // A pane belongs to exactly one tab at one position.
        for (pane_id, tab_ids) in &panes_in_layout_nodes {
            if tab_ids.len() > 1 {
                consistency_error.push(SessionConsistencyError::PaneInMultipleLayouts {
                    pane: *pane_id,
                    tabs: tab_ids.clone(),
                });
            }
        }

        // Every live or `Exited` record must be a leaf somewhere; a `Removed`
        // record must not linger in the registry at all.
        for pane in self.panes.list() {
            if *pane.lifecycle() == PaneLifecycle::Removed {
                consistency_error
                    .push(SessionConsistencyError::LingeringRemovedRecord { pane: pane.id() });
            } else if !panes_in_layout_nodes.contains_key(&pane.id()) {
                consistency_error.push(SessionConsistencyError::OrphanedPaneRecord {
                    pane: pane.id(),
                    lifecycle: *pane.lifecycle(),
                });
            }
        }

        for client in self.clients.list_attached() {
            // A client in this registry must belong to this session.
            if client.session_id() != self.id {
                consistency_error.push(SessionConsistencyError::ClientSessionMismatch {
                    client: client.id(),
                    found: client.session_id(),
                });
            }

            // The tab a client is currently showing must exist.
            if !self.tabs.contains_key(&client.active_tab()) {
                consistency_error.push(SessionConsistencyError::ActiveTabMissing {
                    client: client.id(),
                    tab: client.active_tab(),
                });
            }

            // Each remembered focus must point at a real pane that is a leaf of
            // the tab it was focused in.
            for (&tab_id, &focused_pane_id) in client.focused_panes() {
                if self.panes.get(focused_pane_id).is_none() {
                    consistency_error.push(SessionConsistencyError::FocusPaneNotInRegistry {
                        client: client.id(),
                        tab: tab_id,
                        pane: focused_pane_id,
                    });
                }

                match self.tabs.get(&tab_id) {
                    None => consistency_error.push(SessionConsistencyError::FocusTabMissing {
                        client: client.id(),
                        tab: tab_id,
                    }),
                    Some(tab) if !tab.layout.contains_pane(focused_pane_id) => {
                        consistency_error.push(SessionConsistencyError::FocusTargetMissing {
                            client: client.id(),
                            tab: tab_id,
                            pane: focused_pane_id,
                        });
                    }
                    Some(_) => {}
                }
            }
        }

        if consistency_error.is_empty() {
            Ok(())
        } else {
            Err(consistency_error)
        }
    }
}

#[cfg(test)]
mod tests;
