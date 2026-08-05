//! The server half of the server/client ownership split: the single owner of
//! all authoritative session state, driven by the event loop.
//!
//! A [`Server`] owns the sessions and their layout trees, the per-pane
//! terminal engines, the shared PTY backend, the action registry, and the
//! service handles the event loop drives. The view side lives in its own
//! crate, `koshi-client`; the two halves talk only through the
//! server's doors — [`Server::submit_command`] carries a client's command in,
//! [`Server::subscribe`] carries the emitted events out — so the server never
//! reads client view state and a client never mutates session or pane data.

use std::{
    collections::HashMap,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    },
};

use koshi_config::layer::PartialKoshiConfig;
use koshi_config::types::{ClientConfig, ServerConfig};
use koshi_core::command::{CommandEnvelope, CommandResult};
use koshi_core::event::Event;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, SubscriberId};
use koshi_core::process::PtySize;
use koshi_core::registry::ActionRegistry;
use koshi_layout::solver::MIN_PANE_SIZE;
use koshi_observability::logging::event_log::log_event;
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_renderer::snapshot::Delivery;
use koshi_session::session::state::Session;
use koshi_terminal::engine::TerminalEngine;

use crate::{
    ipc_server::IpcServer,
    placeholder::{SnapshotProvider, Storage},
    runtime::{
        bus::{EventBus, EventFilter},
        event::RuntimeEvent,
        reload::{fold_client, fold_server},
        render_schedule::RenderScheduler,
    },
};

/// The authoritative half of one koshi process: owns the sessions and their
/// layout trees, the per-pane terminal engines, the shared PTY backend, the
/// action registry, and the service handles the event loop drives. One
/// process holds exactly one. The view side — viewport, rendering, the colors
/// chrome is painted in, the subscribed event feed — lives in the
/// `koshi-client` crate, which reaches session state only through
/// [`submit_command`](Self::submit_command) and [`subscribe`](Self::subscribe).
pub struct Server {
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
    /// [`Event::PtyResized`] only for panes
    /// whose size actually changed — never re-solving to a stale reference.
    pub(crate) pty_sizes: HashMap<PaneId, PtySize>,
    /// Event fan-out hub: every emitted [`Event`] is delivered to each
    /// subscriber over its own bounded queue.
    pub(crate) event_bus: EventBus,
    /// Which client each bus subscriber views as, in subscription order. Every
    /// due render puts the frame of the client named here on the subscriber's
    /// queue, a subscriber paused by a dropped critical event is resynced from
    /// that same frame, and a mouse round's answer goes to the subscriber that
    /// views the client whose viewer sent the round.
    pub(crate) subscriptions: Vec<(SubscriberId, ClientId)>,
    /// Source of render snapshots for attach.
    snapshot_provider: Arc<dyn SnapshotProvider>,
    /// Session persistence backend.
    storage: Arc<dyn Storage>,
    /// Control-socket server, present once the session's socket is serving.
    /// Shutdown takes it to stop accepting and withdraw the endpoint file.
    pub(crate) ipc_server: Option<IpcServer>,
    /// Every action this process can perform, seeded with the built-in `core:`
    /// table and extended by plugins as they load. The dispatcher is its only
    /// writer.
    pub(crate) action_registry: ActionRegistry,
    /// The user's stored `koshi.kdl` overrides. The only config file whose
    /// settings a session keeps; the theme and the keybindings are each
    /// viewer's own. Replaced whole by a `koshi.kdl` reload.
    pub(crate) app_layer: PartialKoshiConfig,
    /// The session's effective config: the built-in defaults with the stored
    /// app layer folded on, keeping the sections one session shares across
    /// every viewer. Recomputed by every `koshi.kdl` reload.
    pub(crate) config: ServerConfig,
    /// The viewer-owned sections the session itself reads, folded from the
    /// same app layer. Each viewer folds its own copy from its own files; this
    /// one backs the session-side handling of `scrollback.scroll_on_input`.
    /// Recomputed by every `koshi.kdl` reload.
    pub(crate) client_config: ClientConfig,
    /// Decides when the dispatcher repaints: event handlers mark invalidation
    /// reasons on it, the event loop polls it for render timing.
    pub(crate) render_scheduler: RenderScheduler,
    /// Receiving end of the single runtime event inbox; the loop drains it.
    inbox_rx: Receiver<RuntimeEvent>,
    /// Sending end of the inbox, cloned for each pane's PTY forwarder threads so
    /// they can push [`RuntimeEvent::PtyOutput`] and [`RuntimeEvent::ChildExit`].
    pub(crate) inbox_tx: Sender<RuntimeEvent>,
    /// Set once shutdown begins. The event loop has already exited when it is
    /// set, so no queued IPC/plugin command dispatches after it; the control
    /// socket itself is stopped in the next shutdown stage. One-way; no
    /// command-dispatch path checks it —
    /// [`is_draining`](Self::is_draining) exposes the raw flag today.
    pub(crate) draining: bool,
    /// True when an explicit quit chord requested zero-grace process teardown.
    pub(crate) immediate_shutdown: bool,
    /// True once a `core:quit` command was applied. The event loop polls it
    /// after each event batch and exits; the flag never resets.
    pub(crate) quit_requested: bool,
    /// Bytes waiting to be written to each client's own outer terminal —
    /// escape sequences aimed at the terminal program the client runs in, not
    /// at any pane's child. The copy queues its OSC 52 clipboard write here;
    /// [`push_frames`](Self::push_frames) drains a client's queue onto its
    /// subscriber, ahead of that client's next frame.
    host_writes: HashMap<ClientId, Vec<u8>>,
}

impl Server {
    /// Build a server with no sessions, no terminal engines, no subscribers, a
    /// fresh render scheduler, and an action registry holding the built-in
    /// actions, holding the given PTY backend, service handles, and event
    /// inbox. Both effective configs start at the built-in defaults, over an
    /// empty app layer that [`load_startup_config`](Self::load_startup_config)
    /// and every `koshi.kdl` reload replace.
    pub fn new(
        pty_backend: Arc<dyn PtyBackend>,
        snapshot_provider: Arc<dyn SnapshotProvider>,
        storage: Arc<dyn Storage>,
        inbox_rx: Receiver<RuntimeEvent>,
        inbox_tx: Sender<RuntimeEvent>,
    ) -> Self {
        let app_layer = PartialKoshiConfig::default();
        let config = fold_server(&app_layer);
        let client_config = fold_client(&app_layer);
        Server {
            sessions: HashMap::new(),
            pty_backend,
            terminal_engines: HashMap::new(),
            pty_handles: HashMap::new(),
            pty_sizes: HashMap::new(),
            event_bus: EventBus::new(),
            subscriptions: Vec::new(),
            snapshot_provider,
            storage,
            ipc_server: None,
            action_registry: ActionRegistry::new(),
            render_scheduler: RenderScheduler::new(),
            inbox_rx,
            inbox_tx,
            draining: false,
            immediate_shutdown: false,
            quit_requested: false,
            host_writes: HashMap::new(),
            app_layer,
            config,
            client_config,
        }
    }

    /// The client→server door: dispatch one command envelope against live
    /// state and hand back its result. The only way a client-side caller
    /// requests a session/pane mutation.
    pub fn submit_command(&mut self, envelope: CommandEnvelope) -> CommandResult {
        self.dispatch(envelope)
    }

    /// Mark the chrome stale so the next poll repaints it. The viewer calls
    /// this after a key changed something only it can see — an opened or
    /// closed sequence, a mode switch — since the hint bar and mode tag are
    /// drawn from the viewer's own state.
    pub fn invalidate_status(&mut self) {
        self.render_scheduler
            .invalidate(crate::runtime::render_schedule::InvalidationReason::StatusChanged);
    }

    /// The server→client door: register a subscriber for the events `filter`
    /// selects and hand back the receiving end of its own bounded queue.
    /// Dropping the receiver ends the subscription.
    ///
    /// `client_id` is the client the subscriber views as: the one whose frame
    /// [`push_frames`](Self::push_frames) queues each due render, and the one
    /// [`resync_lagged`](Self::resync_lagged) builds when a critical event does
    /// not fit the queue.
    pub fn subscribe(&mut self, client_id: ClientId, filter: EventFilter) -> Receiver<Delivery> {
        let (id, rx) = self.event_bus.subscribe(filter);
        self.subscriptions.push((id, client_id));
        rx
    }

    /// Drop every subscription registered as viewing `client_id`, closing the
    /// sending end of each one's queue. Called when the client detaches: the
    /// frames those subscribers are resynced from are built from the client's
    /// own view state, which is gone with the record.
    ///
    /// A client with no subscription of its own leaves the bus untouched.
    /// Bytes still queued for the client's own terminal are dropped with it:
    /// the terminal that was to be written to is gone.
    pub(crate) fn unsubscribe_client(&mut self, client_id: ClientId) {
        self.host_writes.remove(&client_id);
        let bus = &mut self.event_bus;
        self.subscriptions.retain(|&(id, viewed)| {
            if viewed == client_id {
                bus.unsubscribe(id);
                return false;
            }
            true
        });
    }

    /// Put a fresh frame on the queue of every subscriber paused by a dropped
    /// critical event, returning it to live delivery. Called once per pass of
    /// the event loop, before the frame the loop paints.
    ///
    /// The frame is this subscriber's viewing client's, built by
    /// [`build_snapshot`](Self::build_snapshot). A subscriber that views no
    /// client, or whose client is no longer attached, can never be resynced, so
    /// its subscription is dropped.
    pub fn resync_lagged(&mut self) {
        if !self.event_bus.has_desynced() {
            return;
        }
        for id in self.event_bus.desynced() {
            let viewed = self
                .subscriptions
                .iter()
                .find(|&&(subscriber, _)| subscriber == id)
                .map(|&(_, client_id)| client_id);
            let Some(client_id) = viewed else {
                tracing::warn!(
                    subscriber = %id,
                    "paused subscriber views no client; unsubscribing"
                );
                self.event_bus.unsubscribe(id);
                continue;
            };
            let Some(snapshot) = self.build_snapshot(client_id) else {
                tracing::warn!(
                    subscriber = %id,
                    client = %client_id,
                    "paused subscriber's client is gone; unsubscribing"
                );
                self.event_bus.unsubscribe(id);
                continue;
            };
            // A full queue leaves the subscriber paused; the next pass builds it
            // a newer frame and tries again.
            self.event_bus.try_resync(id, Box::new(snapshot));
        }
        let bus = &self.event_bus;
        self.subscriptions.retain(|&(id, _)| bus.contains(id));
    }

    /// Put each client's current frame on its queue. Called once per due
    /// render, after [`resync_lagged`](Self::resync_lagged).
    ///
    /// The frame is the subscriber's viewing client's, built by
    /// [`build_snapshot`](Self::build_snapshot).
    ///
    /// Bytes queued for a client's own terminal go on its subscriber's queue
    /// first, so a clipboard write lands ahead of the frame drawn after it.
    ///
    /// A frame that does not fit the queue is dropped and the next due render
    /// offers a newer one. A subscriber the send removed — its receiver is gone
    /// — takes its `subscriptions` entry with it.
    pub fn push_frames(&mut self) {
        // Taken and put back rather than cloned: building a frame and queueing
        // it both need `self`, and this runs on every due frame for every
        // client, so the clone was an allocation per render tick. Nothing in
        // the loop touches `subscriptions` itself.
        let subscriptions = std::mem::take(&mut self.subscriptions);
        for &(id, client_id) in &subscriptions {
            if let Some(bytes) = self.host_writes.remove(&client_id) {
                self.event_bus.try_send_host_write(id, bytes);
            }
            let Some(snapshot) = self.build_snapshot(client_id) else {
                continue;
            };
            self.event_bus.try_send_frame(id, Box::new(snapshot));
        }
        self.subscriptions = subscriptions;
        let bus = &self.event_bus;
        self.subscriptions.retain(|&(id, _)| bus.contains(id));
    }

    /// Put the session `client_id` moves to on the queue of every subscriber
    /// that views that client.
    ///
    /// A subscriber whose queue is full drops the switch and is resynced from
    /// a fresh frame, so that client stays in this session and the user presses
    /// the key again.
    pub(crate) fn send_switch(&mut self, client_id: ClientId, session_id: SessionId) {
        let viewers: Vec<SubscriberId> = self
            .subscriptions
            .iter()
            .filter(|&&(_, viewed)| viewed == client_id)
            .map(|&(id, _)| id)
            .collect();
        for id in viewers {
            self.event_bus.try_send_switch(id, session_id);
        }
    }

    /// Log each of `events`, then deliver it to every subscriber. The shared
    /// tail of every handler that emits events outside a command transaction
    /// (attach, detach, resize, child exit); a command's events pass through
    /// the same pair when its transaction is sealed.
    ///
    /// A subscriber the delivery removes — its receiver is gone — takes its
    /// `subscriptions` entry with it.
    pub(crate) fn publish_events(&mut self, events: &[Event]) {
        for event in events {
            log_event(event);
            let removed = self.event_bus.publish(event);
            if !removed.is_empty() {
                self.subscriptions.retain(|(id, _)| !removed.contains(id));
            }
        }
    }

    /// Take every byte queued for `client_id`'s outer terminal, or `None` when
    /// nothing is queued. Test-only: delivery runs through
    /// [`push_frames`](Self::push_frames).
    #[cfg(test)]
    pub(crate) fn take_host_writes(&mut self, client_id: ClientId) -> Option<Vec<u8>> {
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

    /// The effective per-pane minimum content size the layout solver enforces:
    /// the configured `pane.min-cols`/`min-rows`, each raised to the hard
    /// [`MIN_PANE_SIZE`] floor so a smaller (or zero) configured value can never
    /// drive a pane below the size a PTY can run at.
    pub(crate) fn effective_pane_min(&self) -> Size {
        Size {
            cols: self.config.pane.min_cols.max(MIN_PANE_SIZE.cols),
            rows: self.config.pane.min_rows.max(MIN_PANE_SIZE.rows),
        }
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
    #[cfg(test)]
    pub(crate) fn event_bus(&self) -> &EventBus {
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
    /// Wire the serving control-socket server in, so shutdown stops it and
    /// withdraws its endpoint file with the rest of teardown.
    pub fn attach_ipc_server(&mut self, ipc_server: IpcServer) {
        self.ipc_server = Some(ipc_server);
    }
    /// Borrow the action registry.
    pub fn action_registry(&self) -> &ActionRegistry {
        &self.action_registry
    }
    /// Borrow the runtime event inbox receiver.
    pub fn inbox_rx(&self) -> &Receiver<RuntimeEvent> {
        &self.inbox_rx
    }
    /// Whether shutdown has begun. Once command intake exists it will gate new
    /// commands; today it only records that teardown started.
    pub fn is_draining(&self) -> bool {
        self.draining
    }
}

#[cfg(test)]
mod tests;
