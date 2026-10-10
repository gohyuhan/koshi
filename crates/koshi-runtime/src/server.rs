//! The server half of the server/client ownership split: the single owner of
//! all authoritative session state, driven by the event loop.
//!
//! A [`Server`] owns the sessions and their layout trees, the per-pane
//! terminal engines, the shared PTY backend, and the event inbox the event
//! loop drains. The view side lives in its own
//! crate, `koshi-client`; the two halves talk only through the
//! server's doors — [`Server::submit_command`] carries a client's command in,
//! [`Server::subscribe`] carries the emitted events out — so the server never
//! reads client view state and a client never mutates session or pane data.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{mpsc::Receiver, Arc},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use koshi_config::layer::PartialKoshiConfig;
use koshi_config::types::{ClientConfig, ServerConfig};
use koshi_core::command::{CommandEnvelope, CommandResult};
use koshi_core::event::{Event, QuitCause};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, SubscriberId, TabId};
use koshi_core::process::{ExitStatus, KillPolicy, PtySize};
use koshi_layout::solver::{PaneSizing, MIN_PANE_SIZE};
use koshi_observability::logging::event_log::log_event;
use koshi_observability::logging::recent_events;
use koshi_pty::backend::state::{CarriedPtyPane, PtyBackend, UNOBSERVED_EXIT};
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::Client;
use koshi_session::session::state::Session;
use koshi_terminal::engine::TerminalEngine;

use crate::{
    ipc_server::IpcServer,
    resume::{
        CarriedPane, CarriedPaneState, CarriedQuit, ResumeBody, ResumeHeader, RESUME_FORMAT,
        SCREEN_NOT_RESTORED_NOTICE_BYTES,
    },
    runtime::{
        bus::EventBus,
        event::RuntimeEvent,
        reload::{merge_app_layer_into_client_config, merge_app_layer_into_server_config},
        render_schedule::RenderScheduler,
        saved_view::SavedViewStore,
    },
};

/// How long an announcement waits for every attached client's writing thread to
/// put the session's last frame on that client's socket.
///
/// Bounds the whole wait, not one client. A writing thread blocked inside its
/// write — a client that stopped reading its socket — never ends, so the wait
/// stops here and the session goes on without it.
const CLIENT_NOTIFICATION_TIMEOUT_DURATION: Duration = Duration::from_secs(1);

/// How long the wait for the client writing threads pauses between reads of
/// how many are still running.
const CLIENT_NOTIFICATION_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(2);

/// What a restart request must be able to promise before the session accepts
/// it. `Err` carries the [`RestartRefusal`] the caller is refused with.
///
/// Installed by the session server, which holds the path of the binary a swap
/// would run and the concrete PTY backend the pane records come from. It builds
/// the check out of [`is_binary_runnable`], [`can_carry_panes`], a wait for
/// every pane's writer to settle, and a run of the new binary to read which
/// resume formats it takes back. A process with no check installed refuses every
/// restart.
pub type RestartCheck = Arc<dyn Fn() -> Result<(), RestartRefusal> + Send + Sync>;

/// Why a session refused to restart into its program file.
///
/// Its `Display` is the sentence the caller is refused with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartRefusal {
    /// The program file cannot become the next image as it is: it cannot be
    /// read or run, it gave no readable `resume-support` answer, or it does
    /// not read the resume file this build writes.
    UnfitProgramFile {
        /// The sentence naming what is wrong, such as `the binary at
        /// /usr/local/bin/koshi is not executable`.
        refusal_reason: String,
    },
    /// A pane cannot cross the swap now: its terminal exposes no descriptor,
    /// or a program in it keeps the pane's writer from settling.
    PaneNotReady {
        /// The sentence naming the pane and what holds it.
        refusal_reason: String,
    },
    /// This process has no restart check installed: it cannot replace its own
    /// image.
    ImageReplacementUnsupported,
}

impl RestartRefusal {
    /// Whether this refusal can end while the program file stays the same:
    /// `true` for [`PaneNotReady`](Self::PaneNotReady), which ends once the
    /// pane closes or its writer settles, and `false` for every other variant.
    #[must_use]
    pub fn can_end_without_file_change(&self) -> bool {
        matches!(self, RestartRefusal::PaneNotReady { .. })
    }
}

impl std::fmt::Display for RestartRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestartRefusal::UnfitProgramFile { refusal_reason }
            | RestartRefusal::PaneNotReady { refusal_reason } => {
                formatter.write_str(refusal_reason)
            }
            RestartRefusal::ImageReplacementUnsupported => {
                formatter.write_str("this koshi cannot replace its own image, so it cannot restart")
            }
        }
    }
}

/// Whether the binary at `executable_path` is one this machine could run: its metadata can
/// be read, and on Unix it carries an execute bit.
///
/// # Errors
/// Returns the sentence naming the path and what is wrong with it.
pub fn is_binary_runnable(executable_path: &Path) -> Result<(), String> {
    match std::fs::metadata(executable_path) {
        Err(io_error) => Err(format!(
            "the binary at {} could not be read: {io_error}",
            executable_path.display()
        )),
        #[cfg(unix)]
        Ok(executable_metadata) => {
            use std::os::unix::fs::PermissionsExt as _;
            if executable_metadata.permissions().mode() & 0o111 == 0 {
                Err(format!(
                    "the binary at {} is not executable",
                    executable_path.display()
                ))
            } else {
                Ok(())
            }
        }
        #[cfg(not(unix))]
        Ok(_) => Ok(()),
    }
}

/// Whether every pane in `pane_records` could cross an image swap: on Unix a pane's
/// terminal must expose a descriptor. The next image takes the pane back by
/// that descriptor.
///
/// # Errors
/// Returns the sentence naming the first pane whose terminal exposes no
/// descriptor.
#[cfg(unix)]
pub fn can_carry_panes(pane_records: &[CarriedPtyPane]) -> Result<(), String> {
    match pane_records
        .iter()
        .find(|pane_record| pane_record.terminal_fd.is_none())
    {
        Some(pane_record) => Err(format!(
            "pane {} has no terminal descriptor, so its terminal cannot cross the swap",
            pane_record.pane_id
        )),
        None => Ok(()),
    }
}

/// Whether every pane in `pane_records` could cross an image swap. Always yes here:
/// every pane's pseudoconsole stays in the supervisor process, which outlives
/// the swap.
///
/// # Errors
/// Never returns an error.
#[cfg(windows)]
pub fn can_carry_panes(_pane_records: &[CarriedPtyPane]) -> Result<(), String> {
    Ok(())
}

/// The authoritative half of one koshi process: owns the sessions and their
/// layout trees, the per-pane terminal engines, the shared PTY backend, and the
/// event inbox the event loop drains. One
/// process holds exactly one. The view side — viewport, rendering, the colors
/// chrome is painted in, the subscribed event feed — lives in the
/// `koshi-client` crate, which reaches session state only through
/// [`submit_command`](Self::submit_command) and [`subscribe`](Self::subscribe).
pub struct Server {
    /// Every session in this process, keyed by id. Each session owns its tabs,
    /// layout trees, pane registry, and clients.
    pub(crate) session_by_id: HashMap<SessionId, Session>,
    /// Shared backend that spawns, resizes, writes to, and kills child PTYs.
    pty_backend: Arc<dyn PtyBackend>,
    /// Per-pane terminal engine (VTE parser + screen state), keyed by pane id.
    /// An entry is inserted when the pane's child spawns, resized whenever its
    /// PTY is, and removed when the pane closes — engines exist exactly for
    /// live panes.
    pub(crate) terminal_engine_by_pane_id: HashMap<PaneId, TerminalEngine>,
    /// Every pane whose child the backend drives. The backend's sink pushes
    /// each one's child output and exit into the inbox.
    pub(crate) live_pane_ids: HashSet<PaneId>,
    /// The threads killing the children of panes that left the session, one
    /// per pane, each running [`PtyBackend::kill_pane`] under that pane's kill
    /// policy. [`Server::wait_for_pane_kills`] joins them.
    pub(crate) pane_kill_threads: Vec<JoinHandle<()>>,
    /// The last size each live pane's PTY was set to, keyed by pane id. Every
    /// path that resizes a PTY writes the new size here. A reflow resizes, and
    /// emits [`Event::PtyResized`], only for the panes whose solved size
    /// differs from this record.
    pub(crate) pty_size_by_pane_id: HashMap<PaneId, PtySize>,
    /// Event fan-out hub: every emitted [`Event`] is delivered to each
    /// subscriber over its own bounded queue.
    pub(crate) event_bus: EventBus,
    /// Which client each bus subscriber views as, in subscription order. Every
    /// due render puts the frame of the client named here on the subscriber's
    /// queue, a subscriber paused by a dropped event is resynced from
    /// that same frame, and a mouse round's answer goes to the subscriber that
    /// views the client whose viewer sent the round.
    pub(crate) subscriptions: Vec<(SubscriberId, ClientId)>,
    /// Control-socket server, present once the session's socket is serving.
    /// Shutdown takes it to stop accepting and withdraw the endpoint file.
    pub(crate) ipc_server: Option<IpcServer>,
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
    /// one backs the session-side handling of `scrollback.should_scroll_to_input`.
    /// Recomputed by every `koshi.kdl` reload.
    pub(crate) client_config: ClientConfig,
    /// Decides when the dispatcher repaints: event handlers mark invalidation
    /// reasons on it, the event loop polls it for render timing.
    pub(crate) render_scheduler: RenderScheduler,
    /// Monotonic time at which retained image animations were last advanced.
    pub(crate) animation_clock: Instant,
    /// Receiving end of the single runtime event inbox; the loop drains it.
    inbox_receiver: Receiver<RuntimeEvent>,
    /// True when a quit asked for zero-grace process teardown, in this process
    /// or carried across an image swap.
    pub(crate) should_shutdown_immediately: bool,
    /// True once a `core:quit` command was applied, in this process or carried
    /// across an image swap. The event loop polls it before it waits for an
    /// event and after each event batch, and exits once
    /// [`is_awaiting_client`](Self::is_awaiting_client) is false; the flag never
    /// resets.
    pub(crate) is_quit_requested: bool,
    /// True once a restart request passed [`restart_check`](Self::restart_check)
    /// and was accepted. The event loop polls it after each event batch and
    /// exits into the swap. [`cancel_restart`](Self::cancel_restart) puts it
    /// back to false when the swap is abandoned and the session keeps serving in
    /// this process.
    pub(crate) is_restart_requested: bool,
    /// What a restart must promise before it is accepted, installed by the
    /// session server. `None` refuses every restart.
    restart_check: Option<RestartCheck>,
    /// The clients whose records came across an image swap and have not
    /// attached again yet. Filled by [`resume`](Self::resume) from the carried
    /// sessions, emptied one id at a time as those clients attach again, and
    /// read by
    /// [`handle_drop_unclaimed_clients`](Self::handle_drop_unclaimed_clients)
    /// when the grace window closes.
    pub(crate) client_ids_awaiting_reconnect: HashSet<ClientId>,
    /// The view each dropped client left behind — the tab it was on, the pane
    /// it had focused in each tab, the pane it had zoomed in each tab, and how
    /// far it had scrolled up each pane — keyed by the sha256 of the token that
    /// client's attach minted. Every attach mints one token and files its hash
    /// here; presenting that token on the next attach hands the view back once.
    /// The store lives only in memory: nothing in it is written to disk, sent
    /// over a socket, or carried across an image swap.
    pub(crate) saved_view_store: SavedViewStore,
    /// Bytes waiting to be written to each client's own outer terminal —
    /// escape sequences aimed at the terminal program the client runs in, not
    /// at any pane's child. The copy queues its OSC 52 clipboard write here;
    /// [`push_frames`](Self::push_frames) drains a client's queue onto its
    /// subscriber, ahead of that client's next frame.
    host_write_bytes_by_client_id: HashMap<ClientId, Vec<u8>>,
}

impl Server {
    /// Build a server with no sessions, no terminal engines, no subscribers and
    /// a fresh render scheduler, holding the given PTY backend and event inbox.
    /// Both effective configs start at the built-in defaults, over an
    /// empty app layer that [`load_startup_config`](Self::load_startup_config)
    /// and every `koshi.kdl` reload replace.
    pub fn from_runtime_parts(
        pty_backend: Arc<dyn PtyBackend>,
        inbox_receiver: Receiver<RuntimeEvent>,
    ) -> Self {
        let app_layer = PartialKoshiConfig::default();
        let config = merge_app_layer_into_server_config(&app_layer);
        let client_config = merge_app_layer_into_client_config(&app_layer);
        Server {
            session_by_id: HashMap::new(),
            pty_backend,
            terminal_engine_by_pane_id: HashMap::new(),
            live_pane_ids: HashSet::new(),
            pane_kill_threads: Vec::new(),
            pty_size_by_pane_id: HashMap::new(),
            event_bus: EventBus::new(),
            subscriptions: Vec::new(),
            ipc_server: None,
            render_scheduler: RenderScheduler::new(),
            animation_clock: Instant::now(),
            inbox_receiver,
            should_shutdown_immediately: false,
            is_quit_requested: false,
            is_restart_requested: false,
            restart_check: None,
            client_ids_awaiting_reconnect: HashSet::new(),
            saved_view_store: SavedViewStore::default(),
            host_write_bytes_by_client_id: HashMap::new(),
            app_layer,
            config,
            client_config,
        }
    }

    /// Show the session's recovery notice on each viewer's statusline until
    /// input reaches one of its panes.
    ///
    /// # Panics
    /// Panics when `session_id` does not name a session this server holds.
    pub fn show_session_recovery_notice(&mut self, session_id: SessionId) {
        self.session_by_id
            .get_mut(&session_id)
            .expect("the recovered session was seeded")
            .is_recovery_notice_visible = true;
        self.render_scheduler.invalidate();
    }

    /// Rebuild a server from the state a previous process image carried out,
    /// over panes that are already running.
    ///
    /// The event bus, the render scheduler and the subscribers are built fresh here, and no
    /// control socket is attached: the caller attaches one with
    /// [`attach_ipc_server`](Self::attach_ipc_server). What comes from the swap is what
    /// [`ResumeBody`] carries, over the panes `pty_size_by_pane_id` names, plus the records of the
    /// clients that were told to attach again.
    ///
    /// `startup_app_config` is applied through [`load_startup_config`](Self::load_startup_config)
    /// before any pane is matched, so the session comes back on the `koshi.kdl` the caller read:
    /// a blank screen takes its scrollback limits from it, and a closed pane's tab reflows under
    /// its pane sizes. `None` keeps the built-in defaults.
    ///
    /// `pty_size_by_pane_id` names every pane whose child the backend drives, with the size its
    /// terminal holds. Two callers reach this: the new image after a successful swap passes the
    /// panes it took back, and the old image after a swap that failed to start passes every pane
    /// it still holds.
    /// `exit_status_by_pane_id` carries exits received while the Windows helper
    /// was answering its pane list. A pane absent from the backend that has no
    /// such status closes with [`UNOBSERVED_EXIT`].
    ///
    /// Each pane is matched against the carried sessions:
    ///
    /// 1. A driven pane that a session holds and the body carries a state for comes back with
    ///    that screen.
    /// 2. A driven pane that a session holds and the body carries no state for comes back with a
    ///    blank screen showing [`SCREEN_NOT_RESTORED_NOTICE_BYTES`]. Its child keeps running.
    /// 3. A driven pane that no session holds has its child ended with [`KillPolicy::Tree`] and
    ///    is not recorded.
    /// 4. A pane a session holds that the backend does not drive is closed with
    ///    its carried exit status, or [`UNOBSERVED_EXIT`] when none was received.
    ///
    /// No connection survives the swap, so every client the carried sessions
    /// hold starts out awaiting its own re-attach. Each one that attaches again
    /// naming its id leaves that set, and whoever is left is detached when the
    /// grace window closes.
    pub fn resume(
        pty_backend: Arc<dyn PtyBackend>,
        inbox_receiver: Receiver<RuntimeEvent>,
        startup_app_config: Option<PartialKoshiConfig>,
        body: ResumeBody,
        pty_size_by_pane_id: HashMap<PaneId, PtySize>,
        exit_status_by_pane_id: HashMap<PaneId, ExitStatus>,
    ) -> Self {
        let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver);
        server.load_startup_config(startup_app_config);
        server.client_ids_awaiting_reconnect = body
            .session_by_id
            .values()
            .flat_map(|session| session.clients.list_attached_clients())
            .map(|client| client.get_client_id())
            .collect();
        server.session_by_id = body.session_by_id;
        // A quit applied before the swap comes back with its kind. The serve
        // loop leaves it alone while any carried client is still expected, so
        // the clients that were told to come back are the ones it ends for.
        if let Some(carried_quit) = body.carried_quit {
            server.is_quit_requested = true;
            server.should_shutdown_immediately = carried_quit == CarriedQuit::Immediate;
        }
        let session_pane_ids: HashSet<PaneId> = server
            .session_by_id
            .values()
            .flat_map(|session| session.panes.list_pane_records())
            .map(|pane_record| pane_record.get_pane_id())
            .collect();
        let mut carried_pane_state_by_pane_id = body.carried_pane_state_by_pane_id;
        let restored_at = Instant::now();
        let restored_wall_time = SystemTime::now();
        for (pane_id, pty_size) in pty_size_by_pane_id {
            if !session_pane_ids.contains(&pane_id) {
                tracing::warn!(%pane_id, "a carried pane belongs to no session; its child is ended");
                let _ = server.pty_backend.kill_pane(pane_id, KillPolicy::Tree);
                continue;
            }
            let Some(carried_pane_state) = carried_pane_state_by_pane_id.remove(&pane_id) else {
                tracing::warn!(%pane_id, "a carried pane has no screen to restore; it comes back blank");
                server.park_pane_pty(pane_id, pty_size);
                server.handle_pty_output(pane_id, SCREEN_NOT_RESTORED_NOTICE_BYTES);
                continue;
            };
            server.live_pane_ids.insert(pane_id);
            server.pty_size_by_pane_id.insert(pane_id, pty_size);
            server.terminal_engine_by_pane_id.insert(
                pane_id,
                TerminalEngine::from_carried_state(
                    carried_pane_state.terminal_state,
                    &carried_pane_state.undecoded_bytes,
                    &carried_pane_state.graphics_events,
                    carried_pane_state.graphics_transport.unwrap_or_default(),
                    carried_pane_state.synchronized_output,
                    restored_at,
                    restored_wall_time,
                ),
            );
        }
        let exit_events = server.close_undriven_panes(session_pane_ids, exit_status_by_pane_id);
        server.publish_events(&exit_events);
        server
    }

    /// Close carried panes the backend does not drive, using a status received
    /// during the supervisor link when one exists.
    fn close_undriven_panes(
        &mut self,
        session_pane_ids: HashSet<PaneId>,
        mut exit_status_by_pane_id: HashMap<PaneId, ExitStatus>,
    ) -> Vec<Event> {
        let mut exit_events = Vec::new();
        for pane_id in session_pane_ids {
            if self.live_pane_ids.contains(&pane_id) {
                continue;
            }
            tracing::warn!(%pane_id, "a carried pane has no running child; it closes");
            let exit_status = exit_status_by_pane_id
                .remove(&pane_id)
                .unwrap_or(UNOBSERVED_EXIT);
            exit_events.extend(self.handle_child_exit(pane_id, exit_status));
        }
        exit_events
    }

    /// Drain this server into the two halves of its resume file: the header
    /// naming the session this server holds and every pane in `panes`, and the
    /// body [`ResumeBody`] names.
    ///
    /// `None` when this server holds no session, leaving it untouched: there is
    /// no session to name and nothing to carry.
    ///
    /// The header's `session_id` and `session_name` are read from the same
    /// session the body carries, so the two halves name one session.
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
    pub fn carry_out(&mut self, panes: &[CarriedPtyPane]) -> Option<(ResumeHeader, ResumeBody)> {
        let session = self.get_sole_session()?;
        let session_id = session.session_id;
        let session_name = session.session_name.clone();
        let carried_panes = panes
            .iter()
            .map(|pane| {
                let pty_size = self
                    .pty_size_by_pane_id
                    .get(&pane.pane_id)
                    .copied()
                    .unwrap_or(pane.pty_size);
                CarriedPane {
                    pane_id: pane.pane_id,
                    process_id: pane.process_id,
                    row_count: pty_size.row_count,
                    column_count: pty_size.column_count,
                    #[cfg(unix)]
                    terminal_fd: pane.terminal_fd,
                    #[cfg(windows)]
                    terminal_fd: None,
                    #[cfg(unix)]
                    terminal_name: pane
                        .terminal_fd
                        .and_then(koshi_pty::portable::find_terminal_master_name),
                    #[cfg(windows)]
                    terminal_name: None,
                    exit_status: pane.exit_status,
                }
            })
            .collect();
        let header = ResumeHeader {
            resume_format: RESUME_FORMAT,
            session_id,
            session_name,
            carried_panes,
        };
        let carried_at = Instant::now();
        let carried_pane_state_by_pane_id = std::mem::take(&mut self.terminal_engine_by_pane_id)
            .into_iter()
            .map(|(pane_id, mut engine)| {
                let carried_pane_state = CarriedPaneState {
                    undecoded_bytes: engine.get_undecoded_terminal_bytes().to_vec(),
                    graphics_events: engine.take_graphics_events(),
                    graphics_transport: engine.get_graphics_transport_state(),
                    synchronized_output: engine.get_synchronized_output_transport(carried_at),
                    terminal_state: engine.into_terminal_state(),
                };
                (pane_id, carried_pane_state)
            })
            .collect();
        let resume_body = ResumeBody {
            session_by_id: std::mem::take(&mut self.session_by_id),
            carried_pane_state_by_pane_id,
            carried_quit: self
                .is_quit_requested
                .then_some(if self.should_shutdown_immediately {
                    CarriedQuit::Immediate
                } else {
                    CarriedQuit::Graceful
                }),
        };
        Some((header, resume_body))
    }

    /// The client→server door: dispatch one command envelope against live
    /// state and hand back its result. The only way a client-side caller
    /// requests a session/pane mutation.
    pub fn submit_command(&mut self, envelope: CommandEnvelope) -> CommandResult {
        self.dispatch(envelope)
    }

    /// The server→client door: register a subscriber for the events `filter`
    /// selects and hand back the receiving end of its own bounded queue.
    /// Dropping the receiver ends the subscription.
    ///
    /// `client_id` is the client the subscriber views as: the one whose frame
    /// [`push_frames`](Self::push_frames) queues each due render, and the one
    /// [`resync_lagged`](Self::resync_lagged) builds when an event does
    /// not fit the queue.
    pub fn subscribe(&mut self, client_id: ClientId) -> Receiver<Delivery> {
        let (subscriber_id, receiver) = self.event_bus.subscribe();
        self.subscriptions.push((subscriber_id, client_id));
        receiver
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
        self.host_write_bytes_by_client_id.remove(&client_id);
        let bus = &mut self.event_bus;
        self.subscriptions
            .retain(|&(subscriber_id, viewed_client_id)| {
                if viewed_client_id == client_id {
                    bus.unsubscribe(subscriber_id);
                    return false;
                }
                true
            });
    }

    /// Put a fresh frame on the queue of every subscriber paused by a dropped
    /// event, returning it to live delivery. Called once per pass of
    /// the event loop, before the frame the loop paints.
    ///
    /// The frame is this subscriber's viewing client's, built by
    /// [`build_snapshot`](Self::build_snapshot). A subscriber that views no
    /// client, or whose client is no longer attached, can never be resynced, so
    /// its subscription is dropped.
    pub fn resync_lagged(&mut self) {
        if !self.event_bus.has_desynced_subscribers() {
            return;
        }
        for desynced_subscriber_id in self.event_bus.list_desynced_subscriber_ids() {
            let Some(client_id) = self
                .subscriptions
                .iter()
                .find(|&&(subscriber_id, _)| subscriber_id == desynced_subscriber_id)
                .map(|&(_, client_id)| client_id)
            else {
                tracing::warn!(
                    subscriber_id = %desynced_subscriber_id,
                    "paused subscriber views no client; unsubscribing"
                );
                self.event_bus.unsubscribe(desynced_subscriber_id);
                continue;
            };
            let Some(snapshot) = self.build_snapshot(client_id) else {
                tracing::warn!(
                    subscriber_id = %desynced_subscriber_id,
                    client_id = %client_id,
                    "paused subscriber's client is gone; unsubscribing"
                );
                self.event_bus.unsubscribe(desynced_subscriber_id);
                continue;
            };
            // A full queue leaves the subscriber paused; the next pass builds it
            // a newer frame and tries again.
            self.event_bus
                .try_resync(desynced_subscriber_id, Box::new(snapshot));
        }
        let bus = &self.event_bus;
        self.subscriptions
            .retain(|&(subscriber_id, _)| bus.has_subscriber(subscriber_id));
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
        // The list is taken out for the walk and put back after it. Nothing in
        // the loop touches `subscriptions` itself.
        let subscriptions = std::mem::take(&mut self.subscriptions);
        for &(subscriber_id, client_id) in &subscriptions {
            if let Some(host_output_bytes) = self.host_write_bytes_by_client_id.remove(&client_id) {
                self.event_bus
                    .try_send_host_write(subscriber_id, host_output_bytes);
            }
            let Some(snapshot) = self.build_snapshot(client_id) else {
                continue;
            };
            self.event_bus
                .try_send_frame(subscriber_id, Box::new(snapshot));
        }
        self.subscriptions = subscriptions;
        let bus = &self.event_bus;
        self.subscriptions
            .retain(|&(subscriber_id, _)| bus.has_subscriber(subscriber_id));
    }

    /// Put the session `client_id` moves to on the queue of every subscriber
    /// that views that client.
    ///
    /// `true` when at least one subscriber holds the move. `false` when the
    /// client has no subscriber, or every queue was full: a full queue drops
    /// the move and desyncs that subscriber, and the move is never replayed.
    pub(crate) fn send_switch(&mut self, client_id: ClientId, session_id: SessionId) -> bool {
        let subscriber_ids: Vec<SubscriberId> = self
            .subscriptions
            .iter()
            .filter(|&&(_, viewed)| viewed == client_id)
            .map(|&(subscriber_id, _)| subscriber_id)
            .collect();
        let mut has_sent_switch = false;
        for subscriber_id in subscriber_ids {
            has_sent_switch |= self.event_bus.try_send_switch(subscriber_id, session_id);
        }
        has_sent_switch
    }

    /// Build and queue one read-only placement preview for `client_id`.
    pub(crate) fn send_placement_snapshot(
        &mut self,
        client_id: ClientId,
        request_id: u64,
        source_pane_id: PaneId,
        destination_tab_id: TabId,
    ) -> bool {
        let Some(subscriber_id) = self
            .subscriptions
            .iter()
            .find(|&&(_, viewed_client_id)| viewed_client_id == client_id)
            .map(|&(subscriber_id, _)| subscriber_id)
        else {
            return false;
        };
        match self.build_placement_snapshot(client_id, source_pane_id, destination_tab_id) {
            Ok(placement_snapshot) => self.event_bus.try_send_placement_snapshot(
                subscriber_id,
                request_id,
                Box::new(placement_snapshot),
            ),
            Err(placement_error) => self.event_bus.try_send_placement_refusal(
                subscriber_id,
                request_id,
                placement_error,
            ),
        }
    }

    /// Put one rejected placement command on the queue of the client that sent
    /// it.
    pub(crate) fn send_placement_command_rejection(
        &mut self,
        client_id: ClientId,
        command_id: CommandId,
    ) -> bool {
        let Some(subscriber_id) = self
            .subscriptions
            .iter()
            .find(|&&(_, viewed_client_id)| viewed_client_id == client_id)
            .map(|&(subscriber_id, _)| subscriber_id)
        else {
            return false;
        };
        self.event_bus
            .try_send_placement_command_rejection(subscriber_id, command_id)
    }

    /// Log each of `events`, add it to the recent-events ring, then deliver it
    /// to every subscriber. The shared tail of every handler that emits events
    /// outside a command transaction (attach, detach, resize, child exit); a
    /// command's events pass through the same three steps when its transaction
    /// is sealed.
    ///
    /// A subscriber the delivery removes — its receiver is gone — takes its
    /// `subscriptions` entry with it.
    pub(crate) fn publish_events(&mut self, events: &[Event]) {
        for event in events {
            log_event(event);
            recent_events::record_event(event);
            let removed_subscriber_ids = self.event_bus.publish(event);
            if !removed_subscriber_ids.is_empty() {
                self.subscriptions
                    .retain(|(subscriber_id, _)| !removed_subscriber_ids.contains(subscriber_id));
            }
        }
    }

    /// Queue `host_input_bytes` for `client_id`'s outer terminal, behind anything already
    /// queued.
    pub(crate) fn queue_host_write(&mut self, client_id: ClientId, host_input_bytes: &[u8]) {
        self.host_write_bytes_by_client_id
            .entry(client_id)
            .or_default()
            .extend_from_slice(host_input_bytes);
    }

    /// Whether a `core:quit` command was applied, in this process or carried
    /// across an image swap. The event loop exits once this is true and
    /// [`is_awaiting_client`](Self::is_awaiting_client) is false.
    #[must_use]
    pub fn is_quit_requested(&self) -> bool {
        self.is_quit_requested
    }

    /// Whether a restart request was accepted; the event loop exits into the
    /// image swap when this turns true.
    #[must_use]
    pub fn is_restart_requested(&self) -> bool {
        self.is_restart_requested
    }

    /// Install what a restart request must promise before it is accepted.
    /// Called by the session server before the event loop starts, and again on
    /// the server it keeps after a swap that did not start, which then answers
    /// the next restart too.
    pub fn set_restart_check(&mut self, check: RestartCheck) {
        self.restart_check = Some(check);
    }

    /// Take the accepted restart back, so the event loop stops asking for the
    /// swap. Called when the swap was abandoned before anything irreversible
    /// happened and the session keeps serving in this process.
    pub fn cancel_restart(&mut self) {
        self.is_restart_requested = false;
    }

    /// Whether any client's record came across an image swap and has not been
    /// claimed again.
    ///
    /// A requested quit does not end the session while this is true.
    /// `handle_drop_unclaimed_clients` empties the set when the grace window
    /// closes.
    #[must_use]
    pub fn is_awaiting_client(&self) -> bool {
        !self.client_ids_awaiting_reconnect.is_empty()
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
        if self
            .event_bus
            .get_ending_notice()
            .get_session_ending()
            .is_none()
        {
            self.publish_events(&[Event::Quit(QuitCause::Requested)]);
        }
        self.wait_for_clients_told();
    }

    /// Wait until every attached client's writing thread has written the
    /// session's last frame and ended.
    ///
    /// A client that stopped reading its socket leaves its thread blocked
    /// inside its write, so the wait gives up after
    /// [`CLIENT_NOTIFICATION_TIMEOUT_DURATION`] and says so.
    fn wait_for_clients_told(&self) {
        let deadline = Instant::now() + CLIENT_NOTIFICATION_TIMEOUT_DURATION;
        while self.event_bus.get_ending_notice().count_running_writers() > 0 {
            if Instant::now() >= deadline {
                tracing::warn!(
                    clients = self.event_bus.get_ending_notice().count_running_writers(),
                    "a client did not take the last frame within the wait"
                );
                return;
            }
            std::thread::sleep(CLIENT_NOTIFICATION_POLL_INTERVAL_DURATION);
        }
    }

    /// Hand the runtime inbox's receiving end over, consuming the server.
    ///
    /// The image swap calls this: the panes keep delivering into the same inbox
    /// across the swap, so the server [`resume`](Self::resume) builds reads the
    /// same receiver the drained one held.
    #[must_use]
    pub fn into_inbox_receiver(self) -> Receiver<RuntimeEvent> {
        self.inbox_receiver
    }

    /// Serve one restart request: run the installed check, and accept the
    /// restart only when it passes.
    ///
    /// An accepted restart sets
    /// [`is_restart_requested`](Self::is_restart_requested) and changes nothing
    /// else, so the swap runs after the reply is written. A
    /// refused one changes nothing at all and the session keeps serving.
    ///
    /// # Errors
    /// Returns the refusal the installed check gave, or
    /// [`RestartRefusal::ImageReplacementUnsupported`] when no check is
    /// installed.
    pub(crate) fn handle_ipc_restart(&mut self) -> Result<(), RestartRefusal> {
        let Some(check) = self.restart_check.clone() else {
            return Err(RestartRefusal::ImageReplacementUnsupported);
        };
        check()?;
        self.is_restart_requested = true;
        Ok(())
    }

    /// Borrow the session map.
    pub fn list_sessions(&self) -> &HashMap<SessionId, Session> {
        &self.session_by_id
    }

    /// The session that owns `client_id`, or `None` if no attached client has
    /// that id. Command dispatch's `resolve_acting_session` resolves the same
    /// key-binding/mouse client to its session.
    pub(crate) fn get_session_for_client(&self, client_id: ClientId) -> Option<&Session> {
        self.list_sessions()
            .values()
            .find(|session| session.clients.get_client_by_id(client_id).is_some())
    }

    /// Mutable twin of [`get_session_for_client`](Self::get_session_for_client): the same
    /// client→session lookup, for callers that edit the client's view state (e.g.
    /// the scroll handlers).
    pub(crate) fn get_session_for_client_mut(
        &mut self,
        client_id: ClientId,
    ) -> Option<&mut Session> {
        self.session_by_id
            .values_mut()
            .find(|session| session.clients.get_client_by_id(client_id).is_some())
    }

    /// The session that owns `pane_id`, or `None` if no session's registry holds
    /// that pane. The single pane→session lookup, shared by pane-target
    /// resolution and child-exit routing.
    pub(crate) fn get_session_for_pane(&self, pane_id: PaneId) -> Option<&Session> {
        self.list_sessions()
            .values()
            .find(|session| session.panes.get_pane_record_by_id(pane_id).is_some())
    }

    /// Mutable twin of [`get_session_for_pane`](Self::get_session_for_pane), for callers
    /// that edit the owning session's state — the scroll re-anchor and the
    /// selection handlers.
    pub(crate) fn get_session_for_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Session> {
        self.session_by_id
            .values_mut()
            .find(|session| session.panes.get_pane_record_by_id(pane_id).is_some())
    }

    /// Mutable access to the client attached under `client_id` in any session, or
    /// `None` if no attached client has that id. Resolves the owning session via
    /// [`get_session_for_client_mut`](Self::get_session_for_client_mut), the shared
    /// client→session lookup.
    pub(crate) fn get_client_mut(&mut self, client_id: ClientId) -> Option<&mut Client> {
        self.get_session_for_client_mut(client_id)?
            .clients
            .get_client_mut_by_id(client_id)
    }

    /// Borrow the one session this process serves, or `None` while it holds
    /// none — before genesis, and between the last session ending and the
    /// process exiting. Genesis seeds exactly one session and no command
    /// creates another in this process.
    pub(crate) fn get_sole_session(&self) -> Option<&Session> {
        self.session_by_id.values().next()
    }

    /// The per-pane sizing every layout solve in this server uses: the
    /// configured pane minimum floored at [`MIN_PANE_SIZE`], and the
    /// configured gap between split children.
    pub(crate) fn get_pane_sizing(&self) -> PaneSizing {
        PaneSizing {
            minimum_size: Size {
                column_count: self
                    .config
                    .pane
                    .minimum_column_count
                    .max(MIN_PANE_SIZE.column_count),
                row_count: self
                    .config
                    .pane
                    .minimum_row_count
                    .max(MIN_PANE_SIZE.row_count),
            },
            gap_cell_count: self.config.pane.gap_cell_count,
        }
    }

    /// Borrow the shared PTY backend.
    pub fn get_pty_backend(&self) -> &Arc<dyn PtyBackend> {
        &self.pty_backend
    }
    /// Borrow the per-pane terminal engine map.
    pub fn list_terminal_engines(&self) -> &HashMap<PaneId, TerminalEngine> {
        &self.terminal_engine_by_pane_id
    }
    /// Borrow the IPC server, if one is wired.
    pub fn get_ipc_server(&self) -> Option<&IpcServer> {
        self.ipc_server.as_ref()
    }
    /// Wire the serving control-socket server in, so shutdown stops it and
    /// withdraws its endpoint file with the rest of teardown.
    pub fn attach_ipc_server(&mut self, ipc_server: IpcServer) {
        self.ipc_server = Some(ipc_server);
    }
    /// Borrow the runtime event inbox receiver.
    pub fn get_inbox_receiver(&self) -> &Receiver<RuntimeEvent> {
        &self.inbox_receiver
    }
}

#[cfg(test)]
mod tests;
