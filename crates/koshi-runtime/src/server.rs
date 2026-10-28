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
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroU32,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::Receiver,
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use koshi_config::layer::PartialKoshiConfig;
use koshi_config::types::{ClientConfig, ServerConfig};
use koshi_core::action::ActionReference;
use koshi_core::command::{CommandEnvelope, CommandResult};
use koshi_core::constant::compute_frame_interval;
use koshi_core::event::{Event, QuitCause, RejectReason};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, SubscriberId, TabId};
use koshi_core::notification::NotificationSeverity;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize};
use koshi_layout::solver::{PaneSizing, MIN_PANE_SIZE};
use koshi_observability::logging::event_log::log_event;
use koshi_observability::logging::recent_events::RecentEventRing;
use koshi_observability::repeated_warning::WarningRateLimiter;
use koshi_pty::backend::state::{CarriedPtyPane, PtyBackend, UNOBSERVED_EXIT};
use koshi_session::client::Client;
use koshi_session::session::state::{Session, SessionViewChanges};
use koshi_terminal::engine::TerminalEngine;

use crate::{
    ipc_server::IpcServer,
    resume::{
        CarriedPane, CarriedPaneState, CarriedQuit, ResumeBody, ResumeHeader,
        NEWEST_FORMAT_WITH_RESIZE_DELTAS_BEFORE_PREFERRED, RESUME_FORMAT,
        SCREEN_NOT_RESTORED_NOTICE_BYTES,
    },
    runtime::{
        bus::{EventBus, HostWrite, WriterHandoff},
        command::client_action::WaitingClientActions,
        delivery_queue::DeliveryReceiver,
        event::{RuntimeEvent, SessionEnding},
        mouse::HeldMouseButton,
        placement_preview::ClientPlacementPreview,
        pty_inbox::PaneOutputQueue,
        reload::{merge_app_layer_into_client_config, merge_app_layer_into_server_config},
        render_schedule::{FramePace, PaneRenderDeadlines},
        saved_view::SavedViewStore,
    },
};

/// One bus subscriber, the client it views as, and the pace of that client's
/// frames on its queue.
#[derive(Debug)]
pub(crate) struct FrameSubscription {
    /// The subscriber whose queue the frames go on.
    pub(crate) subscriber_id: SubscriberId,
    /// The client whose view each frame shows.
    pub(crate) client_id: ClientId,
    /// Whether the client's view changed since its last frame, and when its
    /// next frame may go.
    pub(crate) frame_pace: FramePace,
}

/// How long an announcement waits for every attached client's writing thread to
/// put the session's last frame on that client's socket.
///
/// Bounds the whole wait, not one client. A writing thread blocked inside its
/// write — a client that stopped reading its socket — never ends, so the wait
/// stops here and the session goes on without it.
const CLIENT_NOTIFICATION_TIMEOUT_DURATION: Duration = Duration::from_secs(1);

/// The [`grace_window_id`](Server::get_grace_window_id) the next [`Server`]
/// built in this process takes. Each server built takes the value and adds 1.
static NEXT_GRACE_WINDOW_ID: AtomicU64 = AtomicU64::new(0);

/// What a restart request must be able to promise before the session accepts
/// it. `Err` carries the [`RestartRefusal`] the caller is refused with.
///
/// Built by the session server, which holds the path of the binary a swap
/// would run and the concrete PTY backend the pane records come from, and
/// handed to [`IpcServer::start`](crate::ipc_server::IpcServer::start) inside
/// an [`ImageReplacement`](crate::ipc_server::ImageReplacement). It is made of
/// [`is_binary_runnable`], [`can_carry_panes`], a wait for every pane's writer
/// to settle, and a run of the new binary to read which resume formats it
/// takes back. It runs on the thread that asks for the restart, never on the
/// dispatcher.
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
    /// Every pane whose child the backend drives. The backend's sink queues
    /// each one's child output and exit on that pane's [`PaneOutputQueue`].
    pub(crate) live_pane_ids: HashSet<PaneId>,
    /// The output queue of each live pane whose queue woke the dispatcher, by
    /// pane. Removing a pane closes and drops its queue.
    pub(crate) pane_output_queue_by_pane_id: HashMap<PaneId, Arc<PaneOutputQueue>>,
    /// The turn rotation: the panes whose turns come, in order. Each pane
    /// appears at most once. A live pane here holds output or an exit in its
    /// queue. A removed pane stays here until its turn, which
    /// [`Server::serve_output_turn`] passes over.
    pub(crate) ready_output_pane_ids: VecDeque<PaneId>,
    /// The threads killing the children of panes that left the session, one
    /// per pane, each running [`PtyBackend::kill_pane`] under that pane's kill
    /// policy. [`Server::wait_for_pane_kills`] joins them.
    pub(crate) pane_kill_threads: Vec<JoinHandle<()>>,
    /// The last size each live pane's PTY was set to, keyed by pane id. Every
    /// path that resizes a PTY writes the new size here. A reflow resizes, and
    /// emits [`Event::PtyResized`], only for the panes whose solved size
    /// differs from this record.
    pub(crate) pty_size_by_pane_id: HashMap<PaneId, PtySize>,
    /// Fan-out hub: [`EventBus::publish`] delivers each pane placement
    /// commit, quit and restart to every subscriber, over its own bounded
    /// queue.
    pub(crate) event_bus: EventBus,
    /// The newest records of the events this server published. The threads
    /// that serve the session's control socket hold the same ring and answer
    /// `koshi debug events` from it.
    pub(crate) recent_event_ring: Arc<RecentEventRing>,
    /// Which client each bus subscriber views as, and the pace of its frames,
    /// in subscription order. Each frame that is due puts the frame of the
    /// client named here on the subscriber's queue, a subscriber paused by a
    /// dropped event is resynced from that same frame, and a mouse round's
    /// answer goes to the subscriber that views the client whose viewer sent
    /// the round.
    pub(crate) subscriptions: Vec<FrameSubscription>,
    /// The pane placement preview each client names, by client id
    /// ([`set_pane_placement_preview`](Self::set_pane_placement_preview)).
    /// [`mark_changed_views`](Self::mark_changed_views) builds a preview again
    /// when a state it reads changed, and drops the preview of a client
    /// attached to no session. Nothing in it is carried across an image swap.
    pub(crate) placement_preview_by_client_id: HashMap<ClientId, ClientPlacementPreview>,
    /// Every mouse button whose press the session wrote to a pane's program
    /// and whose release it has not written, in the order the presses were
    /// written. [`forward_mouse_to_pane`](Self::forward_mouse_to_pane) adds and
    /// ends entries. Removing a client's record releases that client's entries,
    /// and [`carry_out`](Self::carry_out) releases every entry. Removing a pane
    /// ends its entries with no release written. Nothing in it is carried
    /// across an image swap.
    pub(crate) held_mouse_buttons: Vec<HeldMouseButton>,
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
    /// Each pane's next image animation frame, updated for one pane each time
    /// that pane's engine reads output, is inserted, or is advanced at its
    /// deadline.
    pub(crate) pane_animation_deadlines: PaneRenderDeadlines,
    /// Each pane's synchronized-output end, updated as
    /// `pane_animation_deadlines` is.
    pub(crate) pane_synchronized_output_deadlines: PaneRenderDeadlines,
    /// Receiving end of the single runtime event inbox; the loop drains it.
    inbox_receiver: Receiver<RuntimeEvent>,
    /// True when a quit asked for zero-grace process teardown, in this process
    /// or carried across an image swap.
    pub(crate) should_shutdown_immediately: bool,
    /// True once a quit was requested —
    /// [`Command::KillSession`](koshi_core::command::Command::KillSession), or
    /// the last client leaving under `auto-close-session` — in this process
    /// or carried across an image swap. The event loop polls it before it waits for an
    /// event and after each event batch, and exits once
    /// [`is_awaiting_client`](Self::is_awaiting_client) is false; the flag never
    /// resets.
    pub(crate) is_quit_requested: bool,
    /// True once the dispatcher took a
    /// [`RuntimeEvent::IpcRestart`],
    /// which a restart request sends once its restart check passed. The event
    /// loop polls it after each event batch and exits into the swap.
    /// [`cancel_restart`](Self::cancel_restart) puts it back to false when the
    /// swap is abandoned and the session keeps serving in this process.
    pub(crate) is_restart_requested: bool,
    /// The clients whose records came across an image swap and have not
    /// attached again yet. Filled by [`resume`](Self::resume) from the carried
    /// sessions, emptied one id at a time as those clients attach again or an
    /// attach presenting a record's token takes that record over, and read by
    /// [`handle_drop_unclaimed_clients`](Self::handle_drop_unclaimed_clients)
    /// when the grace window closes.
    pub(crate) client_ids_awaiting_reconnect: HashSet<ClientId>,
    /// The viewer action requests that wait for their client's answer
    /// ([`request_client_action`](Self::request_client_action)). Nothing in it
    /// is carried across an image swap: the swap answers every one first.
    pub(crate) waiting_client_actions: WaitingClientActions,
    /// The id [`get_grace_window_id`](Self::get_grace_window_id) returns.
    pub(crate) grace_window_id: u64,
    /// The view each dropped client left behind
    /// ([`SavedView`](crate::runtime::saved_view::SavedView)), keyed by the
    /// sha256 of the token that client's attach minted. Every attach mints one
    /// token and files its hash here; presenting that token on the next attach
    /// hands the view back once. An image swap carries the store to the next
    /// image ([`carry_out`](Self::carry_out), [`resume`](Self::resume)); nothing
    /// in it is sent over a socket.
    pub(crate) saved_view_store: SavedViewStore,
    /// When a pane's answer to a device query last failed to be written and
    /// was logged: one such failure is logged per
    /// [`LOG_WINDOW_DURATION`](koshi_observability::repeated_warning::LOG_WINDOW_DURATION).
    pub(crate) device_reply_failure_warning: WarningRateLimiter,
}

impl Server {
    /// Build a server with no sessions, no terminal engines, no subscribers and
    /// no timed renders, holding the given PTY backend, event inbox and
    /// recent-events ring.
    /// Both effective configs start at the built-in defaults, over an
    /// empty app layer that [`load_startup_config`](Self::load_startup_config)
    /// and every `koshi.kdl` reload replace.
    pub fn from_runtime_parts(
        pty_backend: Arc<dyn PtyBackend>,
        inbox_receiver: Receiver<RuntimeEvent>,
        recent_event_ring: Arc<RecentEventRing>,
    ) -> Self {
        let app_layer = PartialKoshiConfig::default();
        let config = merge_app_layer_into_server_config(&app_layer);
        let client_config = merge_app_layer_into_client_config(&app_layer);
        Server {
            session_by_id: HashMap::new(),
            pty_backend,
            terminal_engine_by_pane_id: HashMap::new(),
            live_pane_ids: HashSet::new(),
            pane_output_queue_by_pane_id: HashMap::new(),
            ready_output_pane_ids: VecDeque::new(),
            pane_kill_threads: Vec::new(),
            pty_size_by_pane_id: HashMap::new(),
            event_bus: EventBus::new(),
            recent_event_ring,
            subscriptions: Vec::new(),
            placement_preview_by_client_id: HashMap::new(),
            held_mouse_buttons: Vec::new(),
            ipc_server: None,
            pane_animation_deadlines: PaneRenderDeadlines::default(),
            pane_synchronized_output_deadlines: PaneRenderDeadlines::default(),
            inbox_receiver,
            should_shutdown_immediately: false,
            is_quit_requested: false,
            is_restart_requested: false,
            client_ids_awaiting_reconnect: HashSet::new(),
            waiting_client_actions: WaitingClientActions::default(),
            grace_window_id: NEXT_GRACE_WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            saved_view_store: SavedViewStore::default(),
            device_reply_failure_warning: WarningRateLimiter::new(),
            app_layer,
            config,
            client_config,
        }
    }

    /// Rebuild a server from the state a previous process image carried out,
    /// over panes that are already running.
    ///
    /// The event bus and the subscribers are built fresh here, and no
    /// control socket is attached: the caller attaches one with
    /// [`attach_ipc_server`](Self::attach_ipc_server). Events are recorded
    /// into `recent_event_ring`, after the records it already holds. What
    /// comes from the swap is what [`ResumeBody`] carries, over the panes
    /// `pty_size_by_pane_id` names, plus the records of the clients that were
    /// told to attach again.
    ///
    /// `startup_app_config` is applied through [`load_startup_config`](Self::load_startup_config)
    /// before any pane is matched, so the session comes back on the `koshi.kdl` the caller read:
    /// a blank screen takes its scrollback limits from it, and every tab of every carried session
    /// is solved with its pane sizes ([`Session::update_pane_sizing`]). `None` keeps the built-in
    /// defaults.
    ///
    /// `source_resume_format` is the format the resume file was written in. A body of format
    /// [`NEWEST_FORMAT_WITH_RESIZE_DELTAS_BEFORE_PREFERRED`] or older has every carried session's
    /// resize deltas rewritten before the first solve, with those pane sizes and the terminal
    /// sizes in `pty_size_by_pane_id`
    /// ([`Session::convert_resize_deltas_to_apply_after_preferred`]). Each tab whose terminal
    /// sizes give back the rect it was last drawn at solves to those sizes there, whether or not
    /// a client views it. In every other tab, the resize delta of each child with a `preferred`
    /// target moves onto that child's trailing-most flexible sibling.
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
    ///    blank screen showing [`SCREEN_NOT_RESTORED_NOTICE_BYTES`]. Its child keeps running, and
    ///    that session posts the `Warning` notification
    ///    `pane …a1b2: its screen could not be restored after the restart; its program is still
    ///    running`.
    /// 3. A driven pane that no session holds has its child ended with [`KillPolicy::Tree`] and
    ///    is not recorded.
    /// 4. A pane a session holds that the backend does not drive is closed with
    ///    its carried exit status, or [`UNOBSERVED_EXIT`] when none was received.
    ///
    /// No connection survives the swap, so every client the carried sessions
    /// hold starts out awaiting its own re-attach. Each one that attaches again
    /// naming its id and presenting its resume token leaves that set, and
    /// whoever is left is detached when the grace window closes. The body's
    /// saved-view store becomes this server's: the views filed before the swap
    /// and the hash minted for each carried client. A carried client with no
    /// hash in it, as every client of a body converted from format 4 or older,
    /// is not handed back by its id and waits for the grace window.
    ///
    /// After the panes are matched, every viewed tab of every carried session resizes each live
    /// PTY to the size its solve gives, and so does every floating pane: a pane whose size did
    /// not change is not resized.
    // Carries the parts a server is built from (`pty_backend`, `inbox_receiver`,
    // `recent_event_ring`), the config it starts on (`startup_app_config`), and
    // what the swap carried (`body`, `source_resume_format`,
    // `pty_size_by_pane_id`, `exit_status_by_pane_id`).
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        pty_backend: Arc<dyn PtyBackend>,
        inbox_receiver: Receiver<RuntimeEvent>,
        recent_event_ring: Arc<RecentEventRing>,
        startup_app_config: Option<PartialKoshiConfig>,
        body: ResumeBody,
        source_resume_format: u32,
        pty_size_by_pane_id: HashMap<PaneId, PtySize>,
        exit_status_by_pane_id: HashMap<PaneId, ExitStatus>,
    ) -> Self {
        let mut server = Server::from_runtime_parts(pty_backend, inbox_receiver, recent_event_ring);
        server.load_startup_config(startup_app_config);
        server.client_ids_awaiting_reconnect = body
            .session_by_id
            .values()
            .flat_map(|session| session.clients.list_attached_clients())
            .map(|client| client.get_client_id())
            .collect();
        server.saved_view_store = body.saved_view_store;
        server.session_by_id = body.session_by_id;
        if source_resume_format <= NEWEST_FORMAT_WITH_RESIZE_DELTAS_BEFORE_PREFERRED {
            let pane_sizing = server.get_pane_sizing();
            for session in server.session_by_id.values_mut() {
                session.convert_resize_deltas_to_apply_after_preferred(
                    &pty_size_by_pane_id,
                    pane_sizing,
                );
            }
        }
        let mut resume_events = server.update_every_session_pane_sizing();
        // A quit applied before the swap comes back with its kind. The serve
        // loop leaves it alone while any carried client is still expected, so
        // the clients that were told to come back are the ones it ends for.
        if let Some(carried_quit) = body.carried_quit {
            server.is_quit_requested = true;
            server.should_shutdown_immediately = carried_quit == CarriedQuit::Immediate;
        }
        let session_id_by_pane_id: HashMap<PaneId, SessionId> = server
            .session_by_id
            .values()
            .flat_map(|session| {
                session
                    .panes
                    .list_pane_records()
                    .map(|pane_record| (pane_record.get_pane_id(), session.session_id))
            })
            .collect();
        let mut carried_pane_state_by_pane_id = body.carried_pane_state_by_pane_id;
        let restored_at = Instant::now();
        let restored_wall_time = SystemTime::now();
        for (pane_id, pty_size) in pty_size_by_pane_id {
            let Some(&session_id) = session_id_by_pane_id.get(&pane_id) else {
                tracing::warn!(%pane_id, "a carried pane belongs to no session; its child is ended");
                let _ = server.pty_backend.kill_pane(pane_id, KillPolicy::Tree);
                continue;
            };
            let Some(carried_pane_state) = carried_pane_state_by_pane_id.remove(&pane_id) else {
                tracing::warn!(%pane_id, "a carried pane has no screen to restore; it comes back blank");
                server.park_pane_pty(pane_id, pty_size);
                server.handle_pty_output(pane_id, SCREEN_NOT_RESTORED_NOTICE_BYTES);
                server.post_session_notification(
                    session_id,
                    NotificationSeverity::Warning,
                    &format!(
                        "{}: its screen could not be restored after the restart; its program is still running",
                        pane_id.format_short_label()
                    ),
                );
                continue;
            };
            server.live_pane_ids.insert(pane_id);
            server.pty_size_by_pane_id.insert(pane_id, pty_size);
            server.insert_terminal_engine(
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
        resume_events.extend(
            server.close_undriven_panes(session_id_by_pane_id.into_keys(), exit_status_by_pane_id),
        );
        let pty_backend = Arc::clone(&server.pty_backend);
        let tab_ids_by_session_id: Vec<(SessionId, Vec<TabId>)> = server
            .session_by_id
            .iter()
            .map(|(&session_id, session)| (session_id, session.tabs.keys().copied().collect()))
            .collect();
        for (session_id, tab_ids) in tab_ids_by_session_id {
            for tab_id in tab_ids {
                server.reflow_tab_if_viewed(
                    pty_backend.as_ref(),
                    session_id,
                    tab_id,
                    &mut resume_events,
                );
            }
            server.reflow_floating_panes(pty_backend.as_ref(), session_id, &mut resume_events);
        }
        server.publish_events(&resume_events);
        server
    }

    /// Close carried panes the backend does not drive, using a status received
    /// during the supervisor link when one exists.
    fn close_undriven_panes(
        &mut self,
        session_pane_ids: impl IntoIterator<Item = PaneId>,
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
    /// Before anything moves, a release is written for every mouse button a
    /// client holds, at that button's last cell
    /// (`release_every_mouse_button`). `held_mouse_buttons` is left empty and
    /// is not carried.
    ///
    /// The header's `session_id` and `session_name` are read from the same
    /// session the body carries, so the two halves name one session.
    ///
    /// The state moves out of the server, which is left with no sessions, no
    /// terminal engines and an empty saved-view store. No pane's grid or
    /// scrollback is copied on the way.
    ///
    /// The body's `left_running_child_process_ids` is empty: the PTY backend
    /// holds those, and the caller sets them.
    ///
    /// The parser itself is dropped, so each engine hands over the bytes that
    /// reproduce its position.
    ///
    /// `panes` is what the concrete PTY backend reports as live. A pane's size
    /// in the header is this server's own record of it; a pane this server has
    /// no size for takes the size the backend reports. On Unix each record also
    /// names the terminal its descriptor is the master of. A pane carries the
    /// exit status the backend reports for it
    /// ([`CarriedPtyPane::exit_status`]).
    pub fn carry_out(&mut self, panes: &[CarriedPtyPane]) -> Option<(ResumeHeader, ResumeBody)> {
        let session = self.get_sole_session()?;
        let session_id = session.session_id;
        let session_name = session.session_name.clone();
        self.release_every_mouse_button();
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
            saved_view_store: std::mem::take(&mut self.saved_view_store),
            left_running_child_process_ids: Vec::new(),
        };
        Some((header, resume_body))
    }

    /// The client→server door: dispatch one command envelope against live
    /// state and hand back its result. The only way a client-side caller
    /// requests a session/pane mutation.
    pub fn submit_command(&mut self, envelope: CommandEnvelope) -> CommandResult {
        self.dispatch(envelope)
    }

    /// The server→client door: register a subscriber and hand back the
    /// receiving end of its own bounded queue, with what waits beside that
    /// queue for the thread that writes the client's connection: the
    /// host-write bytes for `client_id`'s own terminal, and the owed-frame mark
    /// ([`WriterHandoff`]). Dropping the receiver ends the subscription.
    ///
    /// `client_id` is the client the subscriber views as: the one whose frame
    /// [`push_frames`](Self::push_frames) queues each frame that is due, and
    /// the one [`resync_lagged`](Self::resync_lagged) builds when an event
    /// does not fit the queue. The subscriber gets a frame at most once per
    /// [`compute_frame_interval`] of `max_frames_per_second`, and its first
    /// frame is due at once.
    ///
    /// # Panics
    /// When `max_frames_per_second` is `0`.
    pub fn subscribe(
        &mut self,
        client_id: ClientId,
        max_frames_per_second: u16,
    ) -> (DeliveryReceiver, Arc<WriterHandoff>) {
        let (subscriber_id, delivery_receiver, writer_handoff) = self.event_bus.subscribe();
        self.subscriptions.push(FrameSubscription {
            subscriber_id,
            client_id,
            frame_pace: FramePace::with_frame_interval(compute_frame_interval(
                max_frames_per_second,
            )),
        });
        (delivery_receiver, writer_handoff)
    }

    /// Drop every subscription registered as viewing `client_id`, closing the
    /// sending end of each one's queue. Called when the client detaches: the
    /// frames those subscribers are resynced from are built from the client's
    /// own view state, which is gone with the record.
    ///
    /// A client with no subscription of its own leaves the bus untouched.
    /// Bytes still waiting for the client's own terminal stay with the thread
    /// that writes the client's connection, and nothing adds to them.
    pub(crate) fn unsubscribe_client(&mut self, client_id: ClientId) {
        let bus = &mut self.event_bus;
        self.subscriptions.retain(|frame_subscription| {
            if frame_subscription.client_id == client_id {
                bus.unsubscribe(frame_subscription.subscriber_id);
                return false;
            }
            true
        });
    }

    /// Put a fresh frame on the queue of every subscriber paused by a dropped
    /// event, returning it to live delivery. Called once per pass of the
    /// event loop, before the frames the loop pushes
    /// ([`push_due_frames`](Self::push_due_frames)).
    ///
    /// The frame is this subscriber's viewing client's, built by
    /// [`build_snapshot`](Self::build_snapshot), and goes with that client's
    /// session's notification queue, regardless of the subscriber's frame
    /// pace. A queued frame counts as the subscriber's
    /// frame at `current_time` (`FramePace::record_frame_sent`). A subscriber
    /// that views no client, or whose client is no longer attached, has its
    /// subscription dropped.
    ///
    /// A paused subscriber owed a frame is skipped, and no frame is built for
    /// it: a frame or resync snapshot found its queue full, and its writing
    /// thread has not taken the owed-frame mark since
    /// ([`WriterHandoff::take_owed_frame_mark`]). The
    /// [`RuntimeEvent::QueueRoomFreed`] that thread sends after it takes the
    /// mark starts the pass that resyncs the subscriber.
    pub fn resync_lagged(&mut self, current_time: Instant) {
        if !self.event_bus.has_desynced_subscribers() {
            return;
        }
        for desynced_subscriber_id in self.event_bus.list_desynced_subscriber_ids() {
            if self.event_bus.is_frame_owed(desynced_subscriber_id) {
                continue;
            }
            let Some(subscription_index) =
                self.subscriptions.iter().position(|frame_subscription| {
                    frame_subscription.subscriber_id == desynced_subscriber_id
                })
            else {
                tracing::warn!(
                    subscriber_id = %desynced_subscriber_id,
                    "paused subscriber views no client; unsubscribing"
                );
                self.event_bus.unsubscribe(desynced_subscriber_id);
                continue;
            };
            let client_id = self.subscriptions[subscription_index].client_id;
            let (Some(snapshot), Some(notification_queue)) = (
                self.build_snapshot(client_id),
                self.get_session_for_client(client_id)
                    .map(|session| Box::new(session.notification_queue.clone())),
            ) else {
                tracing::warn!(
                    subscriber_id = %desynced_subscriber_id,
                    client_id = %client_id,
                    "paused subscriber's client is gone; unsubscribing"
                );
                self.event_bus.unsubscribe(desynced_subscriber_id);
                continue;
            };
            if self.event_bus.try_resync(
                desynced_subscriber_id,
                Box::new(snapshot),
                notification_queue,
            ) {
                self.subscriptions[subscription_index]
                    .frame_pace
                    .record_frame_sent(current_time);
            }
        }
        self.remove_ended_subscriptions();
    }

    /// Put the current frame of each client whose frame is due at
    /// `current_time` on its subscriber's queue. Called once per pass, after
    /// [`resync_lagged`](Self::resync_lagged).
    ///
    /// A frame is due when the client's view changed since its last frame and
    /// its pace lets a frame go (`FramePace::compute_frame_wait` is
    /// `Some(Duration::ZERO)`). The frame is the subscriber's viewing
    /// client's, built by [`build_snapshot`](Self::build_snapshot). A
    /// subscriber that is paused, or owed a frame as
    /// [`resync_lagged`](Self::resync_lagged) states, is skipped, and no frame
    /// is built for it.
    ///
    /// A queued frame counts as the subscriber's frame at `current_time`
    /// (`FramePace::record_frame_sent`). A frame that does not fit the queue
    /// is dropped, marks the subscriber owed a frame, and leaves its view
    /// change in place: the pass that [`RuntimeEvent::QueueRoomFreed`] starts
    /// sends a newer one. A client the session no longer shows a frame for
    /// has its view change cleared. A subscriber the send removed — its
    /// receiver is gone — takes its `subscriptions` entry with it.
    pub fn push_frames(&mut self, current_time: Instant) {
        for subscription_index in 0..self.subscriptions.len() {
            let frame_subscription = &self.subscriptions[subscription_index];
            let subscriber_id = frame_subscription.subscriber_id;
            let client_id = frame_subscription.client_id;
            let is_frame_due = frame_subscription
                .frame_pace
                .compute_frame_wait(current_time)
                == Some(Duration::ZERO);
            if !is_frame_due || !self.event_bus.can_take_frame(subscriber_id) {
                continue;
            }
            let Some(snapshot) = self.build_snapshot(client_id) else {
                self.subscriptions[subscription_index]
                    .frame_pace
                    .clear_view_change();
                continue;
            };
            if self
                .event_bus
                .try_send_frame(subscriber_id, Box::new(snapshot))
            {
                self.subscriptions[subscription_index]
                    .frame_pace
                    .record_frame_sent(current_time);
            }
        }
        self.remove_ended_subscriptions();
    }

    /// Read each session's view changes ([`Session::take_view_changes`]),
    /// build again each placement preview that reads a changed state
    /// ([`update_changed_placement_previews`](Self::update_changed_placement_previews)),
    /// then mark the view changed ([`FramePace::mark_view_changed`]) of every
    /// subscriber whose client's frame shows a changed state
    /// ([`is_client_frame_changed`](Self::is_client_frame_changed)). A pass in
    /// which no session changed reads no client and no subscriber.
    pub(crate) fn mark_changed_views(&mut self) {
        let view_changes_by_session_id: HashMap<SessionId, SessionViewChanges> = self
            .session_by_id
            .iter_mut()
            .map(|(&session_id, session)| (session_id, session.take_view_changes()))
            .filter(|(_, view_changes)| !view_changes.is_empty())
            .collect();
        if view_changes_by_session_id.is_empty() {
            return;
        }
        self.update_changed_placement_previews(&view_changes_by_session_id);
        for subscription_index in 0..self.subscriptions.len() {
            let client_id = self.subscriptions[subscription_index].client_id;
            if self.is_client_frame_changed(client_id, &view_changes_by_session_id) {
                self.subscriptions[subscription_index]
                    .frame_pace
                    .mark_view_changed();
            }
        }
    }

    /// Whether `view_changes_by_session_id` lists a state that a frame of
    /// `client_id` shows: the tab bar, the floating panes or the pane sizing of
    /// the client's session, the client's own view, or the layout of its
    /// active tab. `false` for a client attached to no session.
    fn is_client_frame_changed(
        &self,
        client_id: ClientId,
        view_changes_by_session_id: &HashMap<SessionId, SessionViewChanges>,
    ) -> bool {
        let Some(session) = self.get_session_for_client(client_id) else {
            return false;
        };
        let Some(view_changes) = view_changes_by_session_id.get(&session.session_id) else {
            return false;
        };
        view_changes.is_tab_bar_changed
            || view_changes.is_floating_set_changed
            || view_changes.is_pane_sizing_changed
            || view_changes.changed_client_ids.contains(&client_id)
            || session
                .clients
                .get_client_by_id(client_id)
                .is_some_and(|client| {
                    view_changes
                        .layout_changed_tab_ids
                        .contains(&client.get_active_tab_id())
                })
    }

    /// Mark the view changed of every subscriber that views `client_id`.
    pub(crate) fn mark_client_view_changed(&mut self, client_id: ClientId) {
        for frame_subscription in &mut self.subscriptions {
            if frame_subscription.client_id == client_id {
                frame_subscription.frame_pace.mark_view_changed();
            }
        }
    }

    /// Mark the view changed of every subscriber whose client shows `pane_id`
    /// ([`is_pane_shown_to_client`](Self::is_pane_shown_to_client)). A pane in
    /// no session marks nothing.
    pub(crate) fn mark_views_showing_pane(&mut self, pane_id: PaneId) {
        for subscription_index in 0..self.subscriptions.len() {
            let client_id = self.subscriptions[subscription_index].client_id;
            if self.is_pane_shown_to_client(client_id, pane_id) {
                self.subscriptions[subscription_index]
                    .frame_pace
                    .mark_view_changed();
            }
        }
    }

    /// Mark the view changed of every subscriber whose client is attached to
    /// the session that holds the floating pane `pane_id`, whether the client
    /// shows the pane or minimized it. A pane that floats in no session marks
    /// nothing.
    pub(crate) fn mark_views_listing_floating_pane(&mut self, pane_id: PaneId) {
        let Some(session) = self
            .session_by_id
            .values()
            .find(|session| session.floating_set.has_pane(pane_id))
        else {
            return;
        };
        for frame_subscription in &mut self.subscriptions {
            if session
                .clients
                .get_client_by_id(frame_subscription.client_id)
                .is_some()
            {
                frame_subscription.frame_pace.mark_view_changed();
            }
        }
    }

    /// Let the next frame of every subscriber that views `client_id` go
    /// without waiting for its pace ([`FramePace::allow_next_frame_now`]).
    /// Called for the client's own key, paste and mouse events.
    pub(crate) fn allow_client_frame_now(&mut self, client_id: ClientId) {
        for frame_subscription in &mut self.subscriptions {
            if frame_subscription.client_id == client_id {
                frame_subscription.frame_pace.allow_next_frame_now();
            }
        }
    }

    /// Whether a frame of `client_id` can show `pane_id`: the pane is a pane
    /// of the client's placement preview
    /// ([`ClientPlacementPreview::list_preview_pane_ids`]), a leaf of the
    /// client's active tab, or a floating pane of the client's session that
    /// the client shows ([`Client::is_floating_pane_shown`]). `false` when the
    /// client or the pane is in no session, or they are in different sessions.
    pub(crate) fn is_pane_shown_to_client(&self, client_id: ClientId, pane_id: PaneId) -> bool {
        let is_preview_pane = self
            .placement_preview_by_client_id
            .get(&client_id)
            .is_some_and(|client_placement_preview| {
                client_placement_preview
                    .list_preview_pane_ids()
                    .contains(&pane_id)
            });
        if is_preview_pane {
            return true;
        }
        let Some(session) = self.get_session_for_client(client_id) else {
            return false;
        };
        let Some(client) = session.clients.get_client_by_id(client_id) else {
            return false;
        };
        if session.floating_set.has_pane(pane_id) {
            return client.is_floating_pane_shown(pane_id);
        }
        session
            .find_tab_by_pane_id(pane_id)
            .is_some_and(|tab| tab.get_tab_id() == client.get_active_tab_id())
    }

    /// Drop every `subscriptions` entry whose subscriber the bus no longer
    /// holds.
    fn remove_ended_subscriptions(&mut self) {
        let bus = &self.event_bus;
        self.subscriptions
            .retain(|frame_subscription| bus.has_subscriber(frame_subscription.subscriber_id));
    }

    /// Put the session `client_id` moves to on the queue of every subscriber
    /// that views that client, after the bytes waiting for the client's own
    /// terminal. `owner_user_id` is the user id whose folder of the shared
    /// directory advertises `session_id`, or `None` for a lookup by id alone.
    /// The move is queued as
    /// [`EventBus::try_send_switch`](crate::runtime::bus::EventBus::try_send_switch)
    /// states.
    ///
    /// `true` when at least one subscriber holds the move. `false` when the
    /// client has no subscriber, or every queue was full: a full queue drops
    /// the move and desyncs that subscriber, and the move is never replayed.
    pub(crate) fn send_switch(
        &mut self,
        client_id: ClientId,
        session_id: SessionId,
        owner_user_id: Option<u32>,
    ) -> bool {
        let mut has_sent_switch = false;
        for frame_subscription in &self.subscriptions {
            if frame_subscription.client_id == client_id {
                has_sent_switch |= self.event_bus.try_send_switch(
                    frame_subscription.subscriber_id,
                    session_id,
                    owner_user_id,
                );
            }
        }
        has_sent_switch
    }

    /// The subscriber that views `client_id`, or `None` when no subscriber
    /// does. The first in subscription order when several do.
    pub(crate) fn find_subscriber_id_by_client_id(
        &self,
        client_id: ClientId,
    ) -> Option<SubscriberId> {
        self.subscriptions
            .iter()
            .find(|frame_subscription| frame_subscription.client_id == client_id)
            .map(|frame_subscription| frame_subscription.subscriber_id)
    }

    /// Deliver the session's rejection of command `command_id` to
    /// `client_id`, the client that sent it: `reject_reason`, the hint
    /// `help`, and `fired_action`, the action the client's key fired for the
    /// command or `None`
    /// ([`EventBus::send_command_rejection`](crate::runtime::bus::EventBus::send_command_rejection)).
    /// A `client_id` with no subscriber takes nothing.
    pub(crate) fn send_command_rejection(
        &mut self,
        client_id: ClientId,
        command_id: CommandId,
        fired_action: Option<ActionReference>,
        reject_reason: RejectReason,
        help: Option<String>,
    ) {
        let Some(subscriber_id) = self.find_subscriber_id_by_client_id(client_id) else {
            return;
        };
        self.event_bus.send_command_rejection(
            subscriber_id,
            command_id,
            fired_action,
            reject_reason,
            help,
        );
    }

    /// Post `text` from koshi itself at `severity` to session `session_id`'s
    /// notification queue, at the current wall-clock time
    /// ([`NotificationQueue::post_core_entry`](koshi_core::notification::NotificationQueue::post_core_entry)),
    /// and deliver the post to every attached client as one
    /// `NotificationPosted`.
    /// A client that attaches after the post receives the queue in its attach
    /// answer.
    /// Does nothing when this process holds no session `session_id`.
    ///
    /// Example: posting `pane …a1b2 killed by signal 9` as an `Error` gives
    /// each attached client one `NotificationPosted` carrying that entry.
    pub fn post_session_notification(
        &mut self,
        session_id: SessionId,
        severity: NotificationSeverity,
        text: &str,
    ) {
        let Some(session) = self.session_by_id.get_mut(&session_id) else {
            return;
        };
        let notification_queue = &mut session.notification_queue;
        let posted_entry = notification_queue
            .post_core_entry(severity, text, NonZeroU32::MIN, SystemTime::now())
            .clone();
        let revision = notification_queue.get_revision();
        let removed_subscriber_ids = self
            .event_bus
            .publish_notification_post(revision, &posted_entry);
        self.remove_subscriptions(&removed_subscriber_ids);
    }

    /// Log each of `events`, add it to the recent-events ring, then publish it
    /// on the bus: [`EventBus::publish`](crate::runtime::bus::EventBus::publish)
    /// names the events that reach a subscriber. The shared tail of every
    /// handler that emits events outside a command transaction (attach,
    /// detach, resize, child exit); a command's events pass through the same
    /// three steps when its transaction is sealed.
    ///
    /// A subscriber the delivery removes — its receiver is gone — takes its
    /// `subscriptions` entry with it.
    pub(crate) fn publish_events(&mut self, events: &[Event]) {
        for event in events {
            log_event(event);
            self.recent_event_ring.record_event(event);
            let removed_subscriber_ids = self.event_bus.publish(event);
            self.remove_subscriptions(&removed_subscriber_ids);
        }
    }

    /// Drop the `subscriptions` entry of each of `removed_subscriber_ids`, the
    /// subscribers a delivery removed from the bus. An empty slice changes
    /// nothing and reads no entry.
    fn remove_subscriptions(&mut self, removed_subscriber_ids: &[SubscriberId]) {
        if removed_subscriber_ids.is_empty() {
            return;
        }
        self.subscriptions.retain(|frame_subscription| {
            !removed_subscriber_ids.contains(&frame_subscription.subscriber_id)
        });
    }

    /// Queue `host_write` for `client_id`'s outer terminal on the first
    /// subscriber that views `client_id`, as
    /// [`EventBus::queue_host_write`](crate::runtime::bus::EventBus::queue_host_write)
    /// states.
    ///
    /// Returns `true` once `host_write` waits for that subscriber's
    /// connection. Returns `false`, and queues nothing, when no subscriber
    /// views `client_id`, which a client that is not connected matches.
    /// Returns `false` when the subscriber's waiting host writes refuse
    /// `host_write`, or the wake finds the subscriber's receiver gone, as
    /// [`EventBus::queue_host_write`](crate::runtime::bus::EventBus::queue_host_write)
    /// states.
    pub(crate) fn queue_host_write(&mut self, client_id: ClientId, host_write: &HostWrite) -> bool {
        match self.find_subscriber_id_by_client_id(client_id) {
            Some(subscriber_id) => self.event_bus.queue_host_write(subscriber_id, host_write),
            None => false,
        }
    }

    /// Whether a quit was requested —
    /// [`Command::KillSession`](koshi_core::command::Command::KillSession), or
    /// the last client leaving under `auto-close-session` — in this process
    /// or carried across an image swap. The event loop exits once this is true and
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

    /// The id a [`RuntimeEvent::DropUnclaimedClients`] carries to close this
    /// server's grace window. Every server built in one process has another
    /// id, also one rebuilt from the same inbox, and the dispatcher drops an
    /// event that carries another server's id.
    #[must_use]
    pub fn get_grace_window_id(&self) -> u64 {
        self.grace_window_id
    }

    /// Tell every attached client that this session is replacing its own
    /// process image, so each one waits for it and attaches again, and return
    /// once every one of them holds that frame. A viewer action request that
    /// waits for an answer keeps waiting: its client reads the request before
    /// this frame.
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

    /// Tell every attached client that this session ended, and return once
    /// every one of them holds that frame. Each client then says the session
    /// ended. Every viewer action request that waits for an answer is
    /// answered first
    /// ([`answer_client_actions_at_session_ending`](Self::answer_client_actions_at_session_ending)).
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
        self.answer_client_actions_at_session_ending(SessionEnding::Quit);
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
        let running_writer_count = self
            .event_bus
            .get_ending_notice()
            .wait_until_writers_end(deadline);
        if running_writer_count > 0 {
            tracing::warn!(
                clients = running_writer_count,
                "a client did not take the last frame within the wait"
            );
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

    /// The id of every session this server holds, in session-id order.
    pub(crate) fn list_session_ids(&self) -> Vec<SessionId> {
        let mut session_ids: Vec<SessionId> = self.session_by_id.keys().copied().collect();
        session_ids.sort_unstable();
        session_ids
    }

    /// Solve every tab of every session again with
    /// [`get_pane_sizing`](Self::get_pane_sizing)
    /// ([`Session::update_pane_sizing`]), session by session in session-id
    /// order. Returns the events of those solves, in the same order.
    pub(crate) fn update_every_session_pane_sizing(&mut self) -> Vec<Event> {
        let pane_sizing = self.get_pane_sizing();
        let mut emitted_events = Vec::new();
        for session_id in self.list_session_ids() {
            if let Some(session) = self.session_by_id.get_mut(&session_id) {
                emitted_events.extend(session.update_pane_sizing(pane_sizing));
            }
        }
        emitted_events
    }

    /// Borrow the shared PTY backend.
    pub fn get_pty_backend(&self) -> &Arc<dyn PtyBackend> {
        &self.pty_backend
    }
    /// Borrow the ring this server records each event it publishes into.
    pub fn get_recent_event_ring(&self) -> &Arc<RecentEventRing> {
        &self.recent_event_ring
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
