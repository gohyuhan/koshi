//! The runtime state container: the single owner of every live piece of one
//! running tile process, driven by the event loop.

use std::{
    collections::HashMap,
    sync::{mpsc::Receiver, Arc},
};

use tile_core::ids::{PaneId, SessionId};
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pty::backend::state::{PtyBackend, PtyHandle};
use tile_session::session::state::Session;
use tile_terminal::state::TerminalState;

use crate::{
    placeholder::{EventBus, IpcServer, SnapshotProvider, Storage},
    runtime::event::RuntimeEvent,
};

/// Owns all mutable state for one tile process: the sessions and their layout
/// trees, the per-pane terminal engines, the shared PTY backend, and the
/// service handles the event loop drives. One process holds exactly one.
pub struct Runtime {
    /// Every session in this process, keyed by id. Each session owns its tabs,
    /// layout trees, pane registry, and clients.
    pub(crate) sessions: HashMap<SessionId, Session>,
    /// Shared backend that spawns, resizes, writes to, and kills child PTYs.
    pty_backend: Arc<dyn PtyBackend>,
    /// Per-pane terminal emulator state, keyed by pane id.
    terminal_engines: HashMap<PaneId, TerminalState>,
    /// The read side of every spawned pane's PTY, keyed by pane id. Holding the
    /// handle keeps its reader thread feeding output; the event loop polls these.
    pub(crate) pty_handles: HashMap<PaneId, PtyHandle>,
    /// Event fan-out hub for subscribers.
    event_bus: EventBus,
    /// Source of render snapshots for attach and overflow resync.
    snapshot_provider: Arc<dyn SnapshotProvider>,
    /// Session persistence backend.
    storage: Arc<dyn Storage>,
    /// Control-socket server, present once IPC is wired.
    ipc_server: Option<IpcServer>,
    /// Receiving end of the single runtime event inbox; the loop drains it.
    inbox_rx: Receiver<RuntimeEvent>,
    /// Restores the outer terminal when the process ends or panics.
    cleanup_guard: TerminalCleanupGuard,
}

impl Runtime {
    /// Build a runtime with no sessions and no terminal engines, holding the
    /// given PTY backend, service handles, event inbox, and cleanup guard.
    pub fn new(
        pty_backend: Arc<dyn PtyBackend>,
        snapshot_provider: Arc<dyn SnapshotProvider>,
        storage: Arc<dyn Storage>,
        inbox_rx: Receiver<RuntimeEvent>,
        cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
        Runtime {
            sessions: HashMap::new(),
            pty_backend,
            terminal_engines: HashMap::new(),
            pty_handles: HashMap::new(),
            event_bus: EventBus,
            snapshot_provider,
            storage,
            ipc_server: None,
            inbox_rx,
            cleanup_guard,
        }
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
    pub fn terminal_engines(&self) -> &HashMap<PaneId, TerminalState> {
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
    /// Borrow the runtime event inbox receiver.
    pub fn inbox_rx(&self) -> &Receiver<RuntimeEvent> {
        &self.inbox_rx
    }
    /// Borrow the terminal cleanup guard.
    pub fn cleanup_guard(&self) -> &TerminalCleanupGuard {
        &self.cleanup_guard
    }
}

#[cfg(test)]
mod tests;
