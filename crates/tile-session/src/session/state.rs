//! Session state model: the aggregate root a server process owns for each
//! running session.

use std::collections::BTreeMap;

use tile_core::ids::{SessionId, TabId};
use tile_pane::registry::PaneRegistry;

use crate::client::ClientRegistry;

/// One tab: name, layout tree, lifecycle, and tab-local focus history.
/// Placeholder shell: the tab model fills it in.
#[derive(Debug)]
pub struct Tab;

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
}

impl Session {
    /// An empty session: no tabs, no panes, no clients, no plugin runtime.
    #[must_use]
    pub fn new(id: SessionId, name: String) -> Self {
        Self {
            id,
            name,
            tabs: BTreeMap::new(),
            panes: PaneRegistry,
            clients: ClientRegistry,
            config_snapshot: SessionConfig,
            plugin_runtime_ref: None,
        }
    }
}
