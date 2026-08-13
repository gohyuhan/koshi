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
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant},
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
use koshi_pty::portable::CarriedPtyPane;
use koshi_renderer::snapshot::Delivery;
use koshi_session::session::state::Session;
use koshi_terminal::engine::TerminalEngine;

use crate::{
    ipc_server::IpcServer,
    placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage},
    resume::{CarriedPane, CarriedQuit, ResumeBody, ResumeHeader, RESUME_FORMAT},
    runtime::{
        bus::{EventBus, EventFilter},
        event::RuntimeEvent,
        reload::{fold_client, fold_server},
        render_schedule::RenderScheduler,
    },
};

/// How long an announcement waits for every attached client's writing thread to
/// put the session's last frame on that client's socket.
///
/// Bounds the whole wait, not one client. A writing thread blocked inside its
/// write — a client that stopped reading its socket — never ends, so the wait
/// stops here and the session goes on without it.
const CLIENTS_TOLD_LIMIT: Duration = Duration::from_secs(1);

/// How long the wait for the client writing threads pauses between reads of
/// how many are still running.
const CLIENTS_TOLD_POLL: Duration = Duration::from_millis(2);

/// What a restart request must be able to promise before the session accepts
/// it. `Err` carries the sentence the caller is refused with, naming what is
/// wrong.
///
/// Installed by the session server, which holds the path of the binary a swap
/// would run and the concrete PTY backend the pane records come from. It builds
/// the check out of [`binary_is_runnable`], [`panes_can_be_carried`], a wait for
/// every pane's writer to settle, and a run of the new binary to read which
/// resume formats it takes back. A process with no check installed refuses every
/// restart.
pub type RestartCheck = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Whether the binary at `exe` is one this machine could run: it can be read,
/// and on Unix it carries an execute bit.
///
/// # Errors
/// Returns the sentence naming the path and what is wrong with it.
pub fn binary_is_runnable(exe: &Path) -> Result<(), String> {
    match std::fs::metadata(exe) {
        Err(error) => Err(format!(
            "the binary at {} could not be read: {error}",
            exe.display()
        )),
        #[cfg(unix)]
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 == 0 {
                Err(format!("the binary at {} is not executable", exe.display()))
            } else {
                Ok(())
            }
        }
        #[cfg(not(unix))]
        Ok(_) => Ok(()),
    }
}

/// Whether every pane in `panes` could cross an image swap: on Unix a pane's
/// terminal must expose a descriptor, since the descriptor is what the next
/// image takes the pane back by.
///
/// # Errors
/// Returns the sentence naming the first pane whose terminal exposes no
/// descriptor.
#[cfg(unix)]
pub fn panes_can_be_carried(panes: &[CarriedPtyPane]) -> Result<(), String> {
    match panes.iter().find(|pane| pane.terminal_fd.is_none()) {
        Some(pane) => Err(format!(
            "pane {} has no terminal descriptor, so its terminal cannot cross the swap",
            pane.pane_id
        )),
        None => Ok(()),
    }
}

/// Whether every pane in `panes` could cross an image swap. Always yes here:
/// every pane's pseudoconsole stays in the supervisor process, which outlives
/// the swap.
///
/// # Errors
/// Never returns an error.
#[cfg(windows)]
pub fn panes_can_be_carried(_panes: &[CarriedPtyPane]) -> Result<(), String> {
    Ok(())
}

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
    /// Set once shutdown begins, and never cleared. The event loop has already
    /// exited when it is set, so no queued IPC or plugin command dispatches
    /// after it; the control socket is stopped in the next shutdown stage. No
    /// command-dispatch path reads it; [`is_draining`](Self::is_draining) is
    /// its only reader.
    pub(crate) draining: bool,
    /// True when a quit asked for zero-grace process teardown, in this process
    /// or carried across an image swap.
    pub(crate) immediate_shutdown: bool,
    /// True once a `core:quit` command was applied, in this process or carried
    /// across an image swap. The event loop polls it before it waits for an
    /// event and after each event batch, and exits once
    /// [`awaits_a_client`](Self::awaits_a_client) is false; the flag never
    /// resets.
    pub(crate) quit_requested: bool,
    /// True once a restart request passed [`restart_check`](Self::restart_check)
    /// and was accepted. The event loop polls it after each event batch and
    /// exits into the swap. [`cancel_restart`](Self::cancel_restart) puts it
    /// back to false when the swap is abandoned and the session keeps serving in
    /// this process.
    pub(crate) restart_requested: bool,
    /// What a restart must promise before it is accepted, installed by the
    /// session server. `None` refuses every restart.
    restart_check: Option<RestartCheck>,
    /// The clients whose records came across an image swap and have not
    /// attached again yet. Filled by [`resume`](Self::resume) from the carried
    /// sessions, emptied one id at a time as those clients attach again, and
    /// read by
    /// [`handle_drop_unclaimed_clients`](Self::handle_drop_unclaimed_clients)
    /// when the grace window closes.
    pub(crate) awaiting_reconnect: HashSet<ClientId>,
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
            restart_requested: false,
            restart_check: None,
            awaiting_reconnect: HashSet::new(),
            host_writes: HashMap::new(),
            app_layer,
            config,
            client_config,
        }
    }

    /// Rebuild a server from the state a previous process image carried out,
    /// over panes that are already running.
    ///
    /// The event bus, the action registry, the render scheduler, the built-in
    /// config defaults, the subscribers and the control socket are all built
    /// fresh here, on the stock [`NullSnapshotProvider`] and [`NullStorage`].
    /// What comes from the swap is what [`ResumeBody`] carries, over `handles`
    /// and `sizes`, plus the records of the clients that were told to attach
    /// again.
    /// [`load_startup_config`](Self::load_startup_config) still runs afterwards,
    /// so the session comes back on the `koshi.kdl` that is on disk at that
    /// moment.
    ///
    /// Two callers reach this, and they differ in where `handles` comes from.
    /// The new image after a successful swap passes the handles its backend
    /// built by taking each pane back from its descriptor and process id. The
    /// old image after a swap that failed to start passes
    /// [`PtyHandle::detached`] handles: it never let its panes go, so the same
    /// backend still holds them.
    ///
    /// No connection survives the swap, so every client the carried sessions
    /// hold starts out awaiting its own re-attach. Each one that attaches again
    /// naming its id leaves that set, and whoever is left is detached when the
    /// grace window closes.
    pub fn resume(
        pty_backend: Arc<dyn PtyBackend>,
        inbox_rx: Receiver<RuntimeEvent>,
        inbox_tx: Sender<RuntimeEvent>,
        body: ResumeBody,
        handles: HashMap<PaneId, PtyHandle>,
        sizes: HashMap<PaneId, PtySize>,
    ) -> Self {
        let mut server = Server::new(
            pty_backend,
            Arc::new(NullSnapshotProvider),
            Arc::new(NullStorage),
            inbox_rx,
            inbox_tx,
        );
        server.awaiting_reconnect = body
            .sessions
            .values()
            .flat_map(|session| session.clients.list_attached())
            .map(|client| client.id())
            .collect();
        server.sessions = body.sessions;
        // A quit applied before the swap comes back with its kind. The serve
        // loop leaves it alone while any carried client is still expected, so
        // the clients that were told to come back are the ones it ends for.
        if let Some(quit) = body.quit {
            server.quit_requested = true;
            server.immediate_shutdown = quit == CarriedQuit::Immediate;
        }
        let mut undecoded = body.undecoded;
        server.terminal_engines = body
            .engines
            .into_iter()
            .map(|(pane_id, state)| {
                let held = undecoded.remove(&pane_id).unwrap_or_default();
                (pane_id, TerminalEngine::from_state(state, &held))
            })
            .collect();
        server.pty_handles = handles;
        server.pty_sizes = sizes;
        server
    }

    /// Drain this server into the two halves of its resume file: the header
    /// naming `session_id`, `session_name` and every pane in `panes`, and the
    /// body [`ResumeBody`] names.
    ///
    /// The state moves out of the server, which is left with no sessions and no
    /// terminal engines, so no pane's grid or scrollback is copied on the way.
    ///
    /// The parser itself is dropped, so each engine hands over the bytes that
    /// reproduce its position.
    ///
    /// `panes` is what the concrete PTY backend reports as live. A pane's size
    /// in the header is this server's own record of it; a pane this server has
    /// no size for takes the size the backend reports. On Unix each record also
    /// names the terminal its descriptor is the master of. A pane whose child
    /// the backend already reaped carries that child's exit status.
    pub fn carry_out(
        &mut self,
        session_id: SessionId,
        session_name: String,
        panes: &[CarriedPtyPane],
    ) -> (ResumeHeader, ResumeBody) {
        let carried = panes
            .iter()
            .map(|pane| {
                let size = self
                    .pty_sizes
                    .get(&pane.pane_id)
                    .copied()
                    .unwrap_or(pane.size);
                CarriedPane {
                    pane_id: pane.pane_id,
                    pid: pane.pid,
                    rows: size.rows,
                    cols: size.cols,
                    #[cfg(unix)]
                    terminal_fd: pane.terminal_fd,
                    #[cfg(windows)]
                    terminal_fd: None,
                    #[cfg(unix)]
                    terminal_name: pane
                        .terminal_fd
                        .and_then(koshi_pty::portable::terminal_master_name),
                    #[cfg(windows)]
                    terminal_name: None,
                    exit: pane.exit,
                }
            })
            .collect();
        let header = ResumeHeader {
            format: RESUME_FORMAT,
            session_id,
            session_name,
            panes: carried,
        };
        let mut undecoded = HashMap::new();
        let engines = std::mem::take(&mut self.terminal_engines)
            .into_iter()
            .map(|(pane_id, engine)| {
                if !engine.undecoded().is_empty() {
                    undecoded.insert(pane_id, engine.undecoded().to_vec());
                }
                (pane_id, engine.into_state())
            })
            .collect();
        let body = ResumeBody {
            sessions: std::mem::take(&mut self.sessions),
            engines,
            undecoded,
            quit: self.quit_requested.then_some(if self.immediate_shutdown {
                CarriedQuit::Immediate
            } else {
                CarriedQuit::Graceful
            }),
        };
        (header, body)
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
    /// render, after [`resync_lagged`](Self::resync_lagged), and once more by a
    /// caller that has stopped rendering and still holds bytes for a client's
    /// own terminal.
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
        // The list is taken out for the walk and put back after it: building a
        // frame and queueing it both need `self`. Nothing in the loop touches
        // `subscriptions` itself.
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
    /// `true` when at least one subscriber holds the move. `false` when the
    /// client has no subscriber, or every queue was full: a full queue drops
    /// the move and desyncs that subscriber, and the move is never replayed.
    pub(crate) fn send_switch(&mut self, client_id: ClientId, session_id: SessionId) -> bool {
        let viewers: Vec<SubscriberId> = self
            .subscriptions
            .iter()
            .filter(|&&(_, viewed)| viewed == client_id)
            .map(|&(id, _)| id)
            .collect();
        let mut moved = false;
        for id in viewers {
            moved |= self.event_bus.try_send_switch(id, session_id);
        }
        moved
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

    /// Whether a `core:quit` command was applied, in this process or carried
    /// across an image swap. The event loop exits once this is true and
    /// [`awaits_a_client`](Self::awaits_a_client) is false.
    #[must_use]
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Whether a restart request was accepted; the event loop exits into the
    /// image swap when this turns true.
    #[must_use]
    pub fn restart_requested(&self) -> bool {
        self.restart_requested
    }

    /// Install what a restart request must promise before it is accepted.
    /// Called by the session server before the event loop starts, and again on
    /// the server it keeps after a swap that did not start, so that server
    /// answers the next restart too.
    pub fn set_restart_check(&mut self, check: RestartCheck) {
        self.restart_check = Some(check);
    }

    /// Take the accepted restart back, so the event loop stops asking for the
    /// swap. Called when the swap was abandoned before anything irreversible
    /// happened and the session keeps serving in this process.
    pub fn cancel_restart(&mut self) {
        self.restart_requested = false;
    }

    /// Whether any client's record came across an image swap and has not been
    /// claimed again.
    ///
    /// A session that still expects a client does not end: the client was told
    /// to come back, so it is given its window to do so and read what ended the
    /// session. The window is what empties this — see
    /// `handle_drop_unclaimed_clients` — so the wait is always bounded.
    #[must_use]
    pub fn awaits_a_client(&self) -> bool {
        !self.awaiting_reconnect.is_empty()
    }

    /// Tell every attached client that this session is replacing its own
    /// process image, so each one waits for it and attaches again, and return
    /// once every one of them holds that frame.
    ///
    /// [`Event::Restarting`] is the stream's last frame for the clients that
    /// receive it, the same way [`Event::Quit`] is. Publishing it raises the
    /// shared [`EndingNotice`](crate::runtime::event::EndingNotice), which is
    /// what reaches a client whose bounded queue is full, since the event
    /// itself does not fit that queue.
    ///
    /// The wait then holds until every client's writing thread has written the
    /// frame and ended, or until one second passes, so the caller can replace
    /// the process image knowing nothing is left half-told.
    pub fn announce_restarting(&mut self) {
        self.publish_events(&[Event::Restarting]);
        self.wait_for_clients_told();
    }

    /// Tell every attached client that this session ended, so each one says so
    /// rather than reporting a session that died, and return once every one of
    /// them holds that frame.
    ///
    /// [`Event::Quit`] is the stream's last frame for the clients that receive
    /// it. Publishing it raises the shared
    /// [`EndingNotice`](crate::runtime::event::EndingNotice), which is what
    /// reaches a client whose bounded queue is full.
    ///
    /// A notice that is already raised keeps the frame it was raised with, so
    /// this call publishes nothing while one is up. Two things raise it: a
    /// session that published its quit itself, which closing the last tab does,
    /// so the quit frame goes out exactly once; and
    /// [`announce_restarting`](Self::announce_restarting), which leaves
    /// [`SessionEnding::Restarting`](crate::runtime::event::SessionEnding) as
    /// the frame every attached client has already read and left this stream on,
    /// so a quit that follows it reaches no client.
    ///
    /// The wait then holds until every client's writing thread has written the
    /// frame and ended, or until one second passes, so the caller can tear the
    /// process down knowing nothing is left half-told.
    pub fn announce_quit(&mut self) {
        if self.event_bus.ending_notice().raised().is_none() {
            self.publish_events(&[Event::Quit]);
        }
        self.wait_for_clients_told();
    }

    /// Wait until every attached client's writing thread has written the
    /// session's last frame and ended.
    ///
    /// A client that stopped reading its socket leaves its thread blocked
    /// inside its write, so the wait gives up after
    /// [`CLIENTS_TOLD_LIMIT`] and says so.
    fn wait_for_clients_told(&self) {
        let deadline = Instant::now() + CLIENTS_TOLD_LIMIT;
        while self.event_bus.ending_notice().writers_running() > 0 {
            if Instant::now() >= deadline {
                tracing::warn!(
                    clients = self.event_bus.ending_notice().writers_running(),
                    "a client did not take the last frame within the wait"
                );
                return;
            }
            std::thread::sleep(CLIENTS_TOLD_POLL);
        }
    }

    /// Borrow what this session and its clients' writing threads share about
    /// the session's last frame.
    #[cfg(test)]
    pub(crate) fn ending_notice(&self) -> &Arc<crate::runtime::event::EndingNotice> {
        self.event_bus.ending_notice()
    }

    /// Hand the runtime inbox's receiving end over, consuming the server.
    ///
    /// The image swap calls this: the panes keep delivering into the same inbox
    /// across the swap, so the server [`resume`](Self::resume) builds reads the
    /// same receiver the drained one held.
    #[must_use]
    pub fn into_inbox_rx(self) -> Receiver<RuntimeEvent> {
        self.inbox_rx
    }

    /// Serve one restart request: run the installed check, and accept the
    /// restart only when it passes.
    ///
    /// An accepted restart sets [`restart_requested`](Self::restart_requested)
    /// and changes nothing else, so the swap runs after the reply is written. A
    /// refused one changes nothing at all and the session keeps serving.
    ///
    /// # Errors
    /// Returns the sentence the caller is refused with: whatever the installed
    /// check named, or that this process cannot replace its own image when no
    /// check is installed.
    pub(crate) fn handle_ipc_restart(&mut self) -> Result<(), String> {
        let Some(check) = self.restart_check.clone() else {
            return Err(
                "this koshi cannot replace its own image, so it cannot restart".to_string(),
            );
        };
        check()?;
        self.restart_requested = true;
        Ok(())
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
    /// Whether shutdown has begun. It records that teardown started; it gates
    /// no command.
    pub fn is_draining(&self) -> bool {
        self.draining
    }
}

#[cfg(test)]
mod tests;
