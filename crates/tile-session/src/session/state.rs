//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tile_core::{
    constant::MAX_TAB_FOCUS_MRU,
    ids::{ClientId, PaneId, SessionId, TabId},
};
use tile_layout::{mode::LayoutMode, tree::LayoutNode};
use tile_pane::registry::PaneRegistry;

use crate::{
    client::{Client, ClientRegistry},
    session::lifecycle::{SessionLifecycle, SessionLifecycleEvent, TabLifecycle},
};

/// One tab: its name, bar position, layout tree and mode, lifecycle, and the
/// panes it focused, most-recent first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub name: String,
    pub index: usize,
    pub layout: LayoutNode,
    pub layout_mode: LayoutMode,
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

    /// Records `pane` as the most-recently focused: moves it to the front,
    /// keeping one entry per pane, and drops the oldest once the cap is hit.
    pub fn record_focus_mru(&mut self, pane: PaneId) {
        self.focus_mru.retain(|&p| p != pane);
        self.focus_mru.insert(0, pane);
        if self.focus_mru.len() as u16 > MAX_TAB_FOCUS_MRU {
            self.focus_mru.pop();
        }
    }

    pub fn focus_mru(&self) -> &[PaneId] {
        &self.focus_mru
    }

    pub fn remove_focus_mru(&mut self, pane_id: PaneId) {
        self.focus_mru.retain(|&p| p != pane_id);
    }

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

    /// Apply a lifecycle `event`, advancing the session's state; an illegal
    /// transition from the current state is ignored. Crate-internal — callers
    /// drive the lifecycle through the typed wrappers ([`Session::attach_client`],
    /// [`Session::detach_client`], [`Session::request_stop`],
    /// [`Session::complete_stop`]) or the tab operations, so the firing
    /// conditions stay in one place.
    pub(crate) fn update_lifecycle(&mut self, event: SessionLifecycleEvent) {
        if let Ok(next_lifecycle) = self.lifecycle.transition(event) {
            self.lifecycle = next_lifecycle;
        }
    }

    /// Attach `client` and mark the session live. `ClientAttached` only revives
    /// a `Detaching` (no-client) session; attaching to an already-`Running` one
    /// leaves the lifecycle unchanged.
    pub fn attach_client(&mut self, client: Client) {
        self.clients.attach(client);
        self.update_lifecycle(SessionLifecycleEvent::ClientAttached);
    }

    /// Detach the client `client_id`. When it was the *last* attached client the
    /// session drops to `Detaching` — its tabs and panes stay alive; detaching
    /// one of several clients leaves the session `Running`.
    pub fn detach_client(&mut self, client_id: ClientId) {
        self.clients.detach(client_id);
        if self.clients.is_empty() {
            self.update_lifecycle(SessionLifecycleEvent::LastClientDetached);
        }
    }

    /// Request shutdown: move a `Running` or `Detaching` session to `Stopping`.
    /// State is retained — stopping destroys no tabs or panes — so a stopped
    /// session can be persisted and restored later.
    pub fn request_stop(&mut self) {
        self.update_lifecycle(SessionLifecycleEvent::StopRequested);
    }

    /// Finish shutdown once teardown is done, moving `Stopping` to the terminal
    /// `Stopped`.
    pub fn complete_stop(&mut self) {
        self.update_lifecycle(SessionLifecycleEvent::StopCompleted);
    }
}
