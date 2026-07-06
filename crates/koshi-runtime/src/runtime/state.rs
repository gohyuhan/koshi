//! The runtime state container: the single owner of every live piece of one
//! running koshi process, driven by the event loop.

use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    },
};

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::PtySize;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_session::session::state::Session;
use koshi_terminal::engine::TerminalEngine;

use crate::{
    placeholder::{EventBus, IpcServer, SnapshotProvider, Storage},
    runtime::{event::RuntimeEvent, render_schedule::RenderScheduler},
};

/// Owns all mutable state for one koshi process: the sessions and their layout
/// trees, the per-pane terminal engines, the shared PTY backend, and the
/// service handles the event loop drives. One process holds exactly one.
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
}

impl Runtime {
    /// Build a runtime with no sessions, no terminal engines, and a fresh
    /// render scheduler, holding the given PTY backend, service handles,
    /// event inbox, and cleanup guard.
    pub fn new(
        pty_backend: Arc<dyn PtyBackend>,
        snapshot_provider: Arc<dyn SnapshotProvider>,
        storage: Arc<dyn Storage>,
        inbox_rx: Receiver<RuntimeEvent>,
        inbox_tx: Sender<RuntimeEvent>,
        cleanup_guard: TerminalCleanupGuard,
    ) -> Self {
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
            render_scheduler: RenderScheduler::new(),
            inbox_rx,
            inbox_tx,
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
