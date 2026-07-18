//! The runtime state container: the single owner of every live piece of one
//! running koshi process, driven by the event loop.

use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    },
};

use koshi_config::types::KoshiConfig;
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_core::process::PtySize;
use koshi_core::registry::ActionRegistry;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_renderer::theme::Theme;
use koshi_session::session::state::Session;
use koshi_terminal::engine::TerminalEngine;

use crate::{
    placeholder::{EventBus, IpcServer, SnapshotProvider, Storage},
    runtime::{
        event::RuntimeEvent, hints::KeymapHintCatalog, reload::ConfigLayers,
        render_schedule::RenderScheduler, snapshot::resolve_theme,
    },
};

#[cfg(feature = "native")]
use crate::runtime::clipboard::ClipboardWriter;

/// Owns all mutable state for one koshi process: the sessions and their layout
/// trees, the per-pane terminal engines, the shared PTY backend, the action
/// registry, and the service handles the event loop drives. One process holds
/// exactly one.
pub struct Runtime {
    /// Every session in this process, keyed by id. Each session owns its tabs,
    /// layout trees, pane registry, and clients.
    pub(crate) sessions: HashMap<SessionId, Session>,
    /// Shared backend that spawns, resizes, writes to, and kills child PTYs.
    pty_backend: Arc<dyn PtyBackend>,
    /// Per-pane terminal engine (VTE parser + screen state), keyed by pane id.
    /// An entry is inserted when the pane's child spawns, resized whenever its
    /// PTY is, and removed when the pane closes — engines exist exactly for
    /// live panes.
    pub(crate) terminal_engines: HashMap<PaneId, TerminalEngine>,
    /// The read side of every spawned pane's PTY, keyed by pane id. Holding the
    /// handle keeps the pane's PTY sending ends alive and marks the pane live;
    /// a per-pane forwarder thread owns the handle's receivers and pushes the
    /// child's output and exit into the inbox.
    pub(crate) pty_handles: HashMap<PaneId, PtyHandle>,
    /// The last size each live pane's PTY was set to, keyed by pane id. Kept in
    /// sync by every path that resizes a PTY, so a reflow can resize (and emit
    /// [`Event::PtyResized`](koshi_core::event::Event::PtyResized)) only for panes
    /// whose size actually changed — never re-solving to a stale reference.
    pub(crate) pty_sizes: HashMap<PaneId, PtySize>,
    /// Event fan-out hub for subscribers.
    event_bus: EventBus,
    /// Source of render snapshots for attach and overflow resync.
    snapshot_provider: Arc<dyn SnapshotProvider>,
    /// Session persistence backend.
    storage: Arc<dyn Storage>,
    /// Control-socket server, present once IPC is wired.
    ipc_server: Option<IpcServer>,
    /// Every action this process can perform, seeded with the built-in `core:`
    /// table and extended by plugins as they load. The dispatcher is its only
    /// writer.
    pub(crate) action_registry: ActionRegistry,
    /// The user's stored config overrides, one layer per config file. A
    /// file's reload transaction replaces its own layer; the config loader
    /// hands over the startup layers when it lands.
    pub(crate) config_layers: ConfigLayers,
    /// The effective config: the built-in defaults with the stored user
    /// layers folded on. Recomputed by every reload transaction; consumers
    /// read current values from here — except the keymap, whose merged
    /// lookup table is [`keymap_hints`](Self::keymap_hints) (the
    /// `keybindings.modes` section here holds the folded layer data the
    /// merge consumes, not the merge result).
    pub(crate) config: KoshiConfig,
    /// Per-mode hint-bar data resolved from the merged keymap and the action
    /// registry, shared by reference with each frame's snapshot. Seeded from
    /// the built-in defaults and rebuilt whenever the keymap inputs change —
    /// a keybinding reload or a registry refresh.
    pub(crate) keymap_hints: KeymapHintCatalog,
    /// The resolved chrome theme copied onto each frame's snapshot. Resolved
    /// from the effective config's theme; a theme reload replaces it.
    pub(crate) theme: Theme,
    /// Decides when the dispatcher repaints: event handlers mark invalidation
    /// reasons on it, the event loop polls it for render timing.
    pub(crate) render_scheduler: RenderScheduler,
    /// Receiving end of the single runtime event inbox; the loop drains it.
    inbox_rx: Receiver<RuntimeEvent>,
    /// Sending end of the inbox, cloned for each pane's PTY forwarder threads so
    /// they can push [`RuntimeEvent::PtyOutput`] and [`RuntimeEvent::ChildExit`].
    pub(crate) inbox_tx: Sender<RuntimeEvent>,
    /// Restores the outer terminal when the process ends or panics.
    cleanup_guard: TerminalCleanupGuard,
    /// Set once shutdown begins, so that — once IPC/plugin command intake
    /// exists — newly-arriving commands will be rejected rather than mutate
    /// state mid-teardown. One-way; no command-dispatch path checks it yet —
    /// [`is_draining`](Self::is_draining) exposes the raw flag today.
    pub(crate) draining: bool,
    /// True when an explicit quit chord requested zero-grace process teardown.
    pub(crate) immediate_shutdown: bool,
    /// True once a `core:quit` command was applied. The event loop polls it
    /// after each event batch and exits; the flag never resets.
    pub(crate) quit_requested: bool,
    /// Bytes waiting to be written to each client's own outer terminal —
    /// escape sequences aimed at the terminal program the client runs in, not
    /// at any pane's child. An OSC 52 clipboard target queues its write here;
    /// the client's event loop drains the queue with
    /// [`take_host_writes`](Self::take_host_writes) each turn.
    host_writes: HashMap<ClientId, Vec<u8>>,
    /// Lazily opened operating system clipboard used by native clipboard
    /// targets. `None` retries the open on the next copy.
    #[cfg(feature = "native")]
    pub(crate) native_clipboard: Option<Box<dyn ClipboardWriter>>,
}

impl Runtime {
    /// Build a runtime with no sessions, no terminal engines, a fresh render
    /// scheduler, and an action registry holding the built-in actions, holding
    /// the given PTY backend, service handles, event inbox, cleanup guard,
    /// and the default split direction for new panes. The direction seeds the
    /// initial app config layer; a `koshi.kdl` reload replaces it.
    pub fn new(
        pty_backend: Arc<dyn PtyBackend>,
        snapshot_provider: Arc<dyn SnapshotProvider>,
        storage: Arc<dyn Storage>,
        inbox_rx: Receiver<RuntimeEvent>,
        inbox_tx: Sender<RuntimeEvent>,
        cleanup_guard: TerminalCleanupGuard,
        default_new_pane_direction: Direction,
    ) -> Self {
        let action_registry = ActionRegistry::new();
        let config_layers =
            ConfigLayers::with_default_new_pane_direction(default_new_pane_direction);
        let config = config_layers.effective();
        Runtime {
            sessions: HashMap::new(),
            pty_backend,
            terminal_engines: HashMap::new(),
            pty_handles: HashMap::new(),
            pty_sizes: HashMap::new(),
            event_bus: EventBus,
            snapshot_provider,
            storage,
            ipc_server: None,
            keymap_hints: KeymapHintCatalog::from_registry(&action_registry),
            theme: resolve_theme(&config.theme),
            action_registry,
            render_scheduler: RenderScheduler::new(),
            inbox_rx,
            inbox_tx,
            cleanup_guard,
            draining: false,
            immediate_shutdown: false,
            quit_requested: false,
            host_writes: HashMap::new(),
            #[cfg(feature = "native")]
            native_clipboard: None,
            config_layers,
            config,
        }
    }

    /// Take every byte queued for `client_id`'s outer terminal, or `None` when
    /// nothing is queued. The caller writes them to the terminal verbatim.
    pub fn take_host_writes(&mut self, client_id: ClientId) -> Option<Vec<u8>> {
        self.host_writes.remove(&client_id)
    }

    /// Queue `bytes` for `client_id`'s outer terminal, behind anything already
    /// queued.
    pub(crate) fn queue_host_write(&mut self, client_id: ClientId, bytes: &[u8]) {
        self.host_writes
            .entry(client_id)
            .or_default()
            .extend_from_slice(bytes);
    }

    /// Whether a `core:quit` command was applied; the event loop exits when
    /// this turns true.
    #[must_use]
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Borrow the session map.
    pub fn sessions(&self) -> &HashMap<SessionId, Session> {
        &self.sessions
    }
    /// Borrow the shared PTY backend.
    pub fn pty_backend(&self) -> &Arc<dyn PtyBackend> {
        &self.pty_backend
    }
    /// Borrow the per-pane terminal engine map.
    pub fn terminal_engines(&self) -> &HashMap<PaneId, TerminalEngine> {
        &self.terminal_engines
    }
    /// Borrow the event bus.
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }
    /// Borrow the snapshot provider.
    pub fn snapshot_provider(&self) -> &Arc<dyn SnapshotProvider> {
        &self.snapshot_provider
    }
    /// Borrow the storage backend.
    pub fn storage(&self) -> &Arc<dyn Storage> {
        &self.storage
    }
    /// Borrow the IPC server, if one is wired.
    pub fn ipc_server(&self) -> Option<&IpcServer> {
        self.ipc_server.as_ref()
    }
    /// Borrow the action registry.
    pub fn action_registry(&self) -> &ActionRegistry {
        &self.action_registry
    }
    /// Borrow the runtime event inbox receiver.
    pub fn inbox_rx(&self) -> &Receiver<RuntimeEvent> {
        &self.inbox_rx
    }
    /// Borrow the terminal cleanup guard.
    pub fn cleanup_guard(&self) -> &TerminalCleanupGuard {
        &self.cleanup_guard
    }
    /// Whether shutdown has begun. Once command intake exists it will gate new
    /// commands; today it only records that teardown started.
    pub fn is_draining(&self) -> bool {
        self.draining
    }
}

#[cfg(test)]
mod tests;
