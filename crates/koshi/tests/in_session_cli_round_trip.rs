//! What a `koshi` verb typed inside a session does, from the words on the
//! command line to the number the process exits with.
//!
//! Each test walks one verb the whole way: `Cli::try_parse_from` reads the real
//! argv, [`CliCommand::to_action`](koshi::cli::CliCommand::to_action) maps it to
//! the core command, that command crosses a real control socket inside a
//! [`CommandEnvelope`], the dispatcher applies it, the attached client is told
//! what changed, and the answer becomes the exit code the binary reports.
//!
//! The session server runs on a thread of this process, over a
//! [`FakePtyBackend`] in place of the panes' real children. The backend
//! records every call, so a close and a resize have exact, observable effects
//! — a recorded [`KillPolicy`], a recorded [`PtySize`] — with no process to
//! launch. The socket it serves is real: it is bound in a fresh temporary
//! runtime directory, and every request here travels it.
//!
//! Each test serves its own temporary runtime directory, so the sessions here
//! never meet the one a developer is running. The directory sits under a short
//! base because a Unix socket path has an operating-system length cap that a
//! deep temporary path would break.
//!
//! Reading an event stream blocks forever, so each attached client gets a
//! reader thread that forwards what it reads into a queue this thread polls
//! with a deadline: a session that never reports the change fails the test
//! instead of hanging it.
//!
//! The session server is held in a guard that stops it when the test drops it,
//! so a failed assertion leaves no thread serving a socket.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;
use koshi::cli::{Cli, ResolvedTargets};
use koshi_core::command::{CliExitCode, Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::event::{
    Event, InputMode, InputModeChanged, LayoutChanged, PaneClosing, PaneCreated, PaneFocused,
    PaneRemoved, PtyResized, RejectReason,
};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::{KillPolicy, PtySize};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_link::error::CliError;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;
use tempfile::TempDir;

/// How long a poll waits for something the session server has to do before the
/// test calls it a failure.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(10);

/// The terminal size the session starts at and the attaching client reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A fresh directory to serve, under a short base so the Unix socket path
/// stays inside the operating system's path-length cap. Removed when the test
/// drops it.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

/// One session server running on its own thread, serving a real control socket
/// in its own runtime directory over a fake PTY backend. Dropping it stops
/// that thread and withdraws the socket.
struct RunningSession {
    /// The runtime directory the control socket and endpoint file live in.
    dir: TempDir,
    /// The session the server seeded and serves.
    id: SessionId,
    /// The backend that stands in for the panes' children, so a test can read
    /// what was spawned, resized and killed.
    pty: Arc<FakePtyBackend>,
    /// The runtime inbox, for the hangup that ends the serving thread.
    inbox_tx: mpsc::Sender<RuntimeEvent>,
    /// The serving thread, joined at drop. `Option` so the drop can take it
    /// out of the otherwise-borrowed struct.
    dispatcher: Option<JoinHandle<()>>,
}

impl RunningSession {
    /// Start a session server on its own thread and wait until its socket
    /// answers.
    fn start() -> RunningSession {
        let dir = test_runtime_dir();
        let id = SessionId::new();
        let pty = Arc::new(FakePtyBackend::new());
        let (inbox_tx, inbox_rx) = mpsc::channel();

        let serving_dir = dir.path().to_path_buf();
        let serving_pty = Arc::clone(&pty);
        let serving_tx = inbox_tx.clone();
        let dispatcher = std::thread::spawn(move || {
            serve_session(&serving_dir, id, serving_pty, inbox_rx, serving_tx);
        });

        let session = RunningSession {
            dir,
            id,
            pty,
            inbox_tx,
            dispatcher: Some(dispatcher),
        };
        // The endpoint file is written after the socket binds, so a readable
        // one means the socket is ready to answer.
        let deadline = Instant::now() + WAIT;
        while EndpointFile::read(&EndpointFile::path(session.dir.path(), session.id)).is_err() {
            assert!(
                Instant::now() < deadline,
                "the session server never advertised its socket"
            );
            std::thread::sleep(POLL);
        }
        session
    }

    /// The panes the backend spawned, in spawn order.
    fn panes(&self) -> Vec<PaneId> {
        self.pty.spawned_panes()
    }

    /// The session's own report of itself, read over the control socket by the
    /// library call the `koshi inspect` verbs make.
    fn overview(&self) -> koshi_core::discovery::SessionOverview {
        koshi_link::ipc_client::fetch_overview(self.dir.path(), self.id)
            .expect("the session server describes itself")
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        // The serving loop stops on a `Quit`; a loop that already stopped on
        // its own leaves a closed inbox, and the send fails harmlessly.
        let _ = self.inbox_tx.send(RuntimeEvent::Quit);
        if let Some(handle) = self.dispatcher.take() {
            let _ = handle.join();
        }
    }
}

/// Build one session's server on `pty`, seed the session, bind its control
/// socket in `runtime_dir`, and serve the runtime inbox until the session ends.
///
/// The order is the running binary's: the session is seeded before the socket
/// binds, so nothing advertises a session that does not exist yet.
fn serve_session(
    runtime_dir: &Path,
    session_id: SessionId,
    pty: Arc<FakePtyBackend>,
    inbox_rx: mpsc::Receiver<RuntimeEvent>,
    inbox_tx: mpsc::Sender<RuntimeEvent>,
) {
    let backend: Arc<dyn PtyBackend> = pty;
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let mut server = Server::new(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
    );
    server.load_startup_config(None);
    server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            VIEWPORT,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");

    let ipc_server = IpcServer::start(runtime_dir, session_id, inbox_tx, None)
        .expect("the control socket binds");
    server.attach_ipc_server(ipc_server);

    serve(&mut server);
    server.shutdown();
}

/// Serve the runtime inbox until the session ends: block until an event is due
/// (bounded by the next render deadline), apply it and any others already
/// queued, hand a fresh snapshot to any subscriber that lost a critical event,
/// push every attached client its frame when a render is due, and stop once the
/// inbox loses its last sender, a hangup arrives, a quit is applied, or no pane
/// is left running.
fn serve(server: &mut Server) {
    loop {
        let now = Instant::now();
        let event = match server.next_render_wakeup(now) {
            Some(timeout) => match server.inbox_rx().recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match server.inbox_rx().recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            },
        };
        let mut quit = false;
        if let Some(event) = event {
            quit |= server.handle_runtime_event(event).is_break();
        }
        while let Ok(event) = server.inbox_rx().try_recv() {
            quit |= server.handle_runtime_event(event).is_break();
        }
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
        if quit || server.quit_requested() || !server.has_active_panes() {
            break;
        }
    }
}

/// A client attached over the control socket, with its event stream drained by
/// its own thread into a queue this thread polls.
struct AttachedClient {
    /// The client the session minted for this connection.
    id: ClientId,
    /// Every frame the session wrote that says something about its structure,
    /// in arrival order. A painted frame carries no structure change, so the
    /// reader passes it over.
    events: mpsc::Receiver<SessionEvent>,
}

/// The endpoint file the session server advertises: the socket address and the
/// token a Hello presents.
fn endpoint(session: &RunningSession) -> EndpointFile {
    EndpointFile::read(&EndpointFile::path(session.dir.path(), session.id))
        .expect("the session server advertises its socket")
}

/// Open a connection to the socket `endpoint` advertises, with its handshake
/// already done.
fn open(endpoint: &EndpointFile) -> Connection {
    let mut connection = Connection::connect(&endpoint.socket).expect("the socket answers");
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: endpoint.token.clone(),
            remote: false,
        },
    };
    connection.send(&hello).expect("the server reads the Hello");
    let reply: IpcResponse = connection.recv().expect("the server answers the Hello");
    match reply.result {
        IpcResult::Hello { .. } => connection,
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// Attach to `session` the way the attached client does — Hello then Attach on
/// one connection — and hand back the client the server minted plus its event
/// stream.
///
/// The connection is moved into the reader thread, which ends when the session
/// stops serving and closes it.
fn attach(session: &RunningSession) -> AttachedClient {
    let mut connection = open(&endpoint(session));
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Attach {
            viewport: VIEWPORT,
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
        },
    };
    connection
        .send(&request)
        .expect("the server reads the attach");
    let reply: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(reply.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id,
        ..
    } = reply.result
    else {
        panic!("expected an attach reply, got {:?}", reply.result);
    };
    assert_eq!(session_id, session.id);

    let (events_tx, events) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(event) = connection.recv::<SessionEvent>() {
            if matches!(event, SessionEvent::Painted { .. }) {
                continue;
            }
            if events_tx.send(event).is_err() {
                break;
            }
        }
    });

    AttachedClient {
        id: client_id,
        events,
    }
}

/// The next `count` frames the attached client is told about the session's
/// structure, in arrival order. Fails the test once [`WAIT`] has passed with
/// fewer than `count` of them.
fn next_events(client: &AttachedClient, count: usize) -> Vec<SessionEvent> {
    (0..count)
        .map(|_| {
            client
                .events
                .recv_timeout(WAIT)
                .expect("the session reports the change")
        })
        .collect()
}

/// Run one `koshi` invocation typed inside `pane` by `client`, and hand back
/// what the session answered and the exit code the binary reports for that
/// answer.
///
/// The steps are the binary's, in its order: parse the argv, map the
/// subcommand to its core command, submit the command over the session's
/// control socket, then turn the result into the process exit status.
fn run_cli(
    session: &RunningSession,
    client: &AttachedClient,
    pane: PaneId,
    argv: &[&str],
) -> (CommandResult, CliExitCode) {
    let cli = Cli::try_parse_from(argv).expect("the argv parses");
    let (_, command) = cli
        .command
        .as_ref()
        .expect("the argv carries a subcommand")
        .to_action(&ResolvedTargets::default(), Direction::Right)
        .expect("the subcommand is an action verb");

    let result = submit(session, client, pane, command);
    let code = match report(&result) {
        Ok(()) => CliExitCode::Success,
        Err(error) => CliExitCode::from(&error),
    };
    (result, code)
}

/// Submit `command` to `session` over its control socket, enveloped the way the
/// CLI running inside `pane` envelopes it, and hand back the dispatcher's
/// result.
fn submit(
    session: &RunningSession,
    client: &AttachedClient,
    pane: PaneId,
    command: Command,
) -> CommandResult {
    let endpoint = endpoint(session);
    let mut connection = open(&endpoint);
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::in_session_cli(
            session.id,
            Some(client.id),
            pane,
            PathBuf::from(endpoint.socket),
        ),
        SystemTime::now(),
        command,
    );
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    connection
        .send(&request)
        .expect("the server reads the command");
    let reply: IpcResponse = connection.recv().expect("the server answers the command");
    assert_eq!(reply.request_id, Some(2));
    match reply.result {
        IpcResult::CommandResult(result) => result,
        other => panic!("the command was answered with {other:?}"),
    }
}

/// What the binary reports for a dispatched command: an applied command is a
/// success, and a rejected one is the [`CliError`] the exit-code table reads.
fn report(result: &CommandResult) -> Result<(), CliError> {
    match result {
        CommandResult::Ok { .. } => Ok(()),
        CommandResult::Rejected { reason, help, .. } => Err(CliError::CommandRejected {
            reason: *reason,
            help: help.clone(),
        }),
    }
}

/// The events an applied result carries, or the rejection that carried none.
fn applied_events(result: &CommandResult) -> &[Event] {
    match result {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        other => panic!("expected an applied command, got {other:?}"),
    }
}

/// The tab the session's only tab is, read from its own report.
fn only_tab(session: &RunningSession) -> TabId {
    let overview = session.overview();
    assert_eq!(overview.tabs.len(), 1);
    overview.tabs[0].id
}

/// The size the pane's child was last given, which is the size the layout
/// solved for that pane.
fn last_size(session: &RunningSession, pane: PaneId) -> PtySize {
    *session
        .pty
        .resizes(pane)
        .expect("the pane was spawned")
        .last()
        .expect("the spawn recorded the pane's first size")
}

/// The kills the backend recorded for `pane`, waited for. A closed pane's
/// child is killed on its own thread, so the record lands after the command is
/// answered.
fn kills_of(session: &RunningSession, pane: PaneId) -> Vec<KillPolicy> {
    let deadline = Instant::now() + WAIT;
    loop {
        let kills = session.pty.kills(pane).expect("the pane was spawned");
        if !kills.is_empty() {
            return kills;
        }
        assert!(
            Instant::now() < deadline,
            "the closed pane's child was never killed"
        );
        std::thread::sleep(POLL);
    }
}

/// The lock mode the session holds for `client`.
fn lock_state(session: &RunningSession, client: ClientId) -> LockMode {
    let overview = session.overview();
    let found = overview
        .clients
        .iter()
        .find(|info| info.id == client)
        .expect("the client is attached to the session");
    found.lock_state
}

/// Split `pane` in two and hand back the pane the split created, so a test
/// that needs a neighbor starts from a two-pane tab.
fn split(session: &RunningSession, client: &AttachedClient, pane: PaneId) -> PaneId {
    let (result, code) = run_cli(
        session,
        client,
        pane,
        &["koshi", "new-pane", "--direction", "right"],
    );
    assert_eq!(code, CliExitCode::Success);
    match applied_events(&result) {
        [Event::PaneCreated(created), ..] => created.pane_id,
        other => panic!("expected a pane to be created, got {other:?}"),
    }
}

#[test]
fn new_pane_over_the_socket_splits_and_reports_success() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    let tab = only_tab(&session);

    let (result, code) = run_cli(
        &session,
        &client,
        root,
        &["koshi", "new-pane", "--direction", "right"],
    );

    // The split spawned exactly one more child, and the CLI's `--direction`
    // put it beside the pane the command was typed in.
    assert_eq!(session.panes().len(), 2);
    let created = session.panes()[1];
    assert_eq!(
        applied_events(&result),
        [
            Event::PaneCreated(PaneCreated {
                pane_id: created,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PaneFocused(PaneFocused {
                client_id: client.id,
                tab_id: tab,
                pane_id: created,
                prior_pane: Some(root),
            }),
            // Both children are sized to the rects the split solved.
            Event::PtyResized(PtyResized {
                pane_id: created,
                size: PtySize { cols: 38, rows: 20 },
            }),
            Event::PtyResized(PtyResized {
                pane_id: root,
                size: PtySize { cols: 38, rows: 20 },
            }),
        ]
    );
    assert_eq!(
        next_events(&client, 3),
        vec![
            SessionEvent::PaneCreated {
                pane_id: created,
                tab_id: tab,
            },
            SessionEvent::LayoutChanged { tab_id: tab },
            SessionEvent::PaneFocused {
                client_id: client.id,
                tab_id: tab,
                pane_id: created,
                prior_pane: Some(root),
            },
        ]
    );
    assert_eq!(code, CliExitCode::Success);
}

#[test]
fn close_pane_over_the_socket_kills_the_child_and_removes_the_pane() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    let tab = only_tab(&session);
    let created = split(&session, &client, root);
    // The split's own frames are the previous test's subject.
    let _ = next_events(&client, 3);

    let (result, code) = run_cli(&session, &client, created, &["koshi", "close-pane"]);

    // The close is one transaction: the pane the command was typed in leaves
    // the layout, the client's focus falls back to the pane that stayed, and
    // that pane's child is resized to the space it took back.
    assert_eq!(
        applied_events(&result),
        [
            Event::PaneClosing(PaneClosing { pane_id: created }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: created,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PaneFocused(PaneFocused {
                client_id: client.id,
                tab_id: tab,
                pane_id: root,
                prior_pane: Some(created),
            }),
            Event::PtyResized(PtyResized {
                pane_id: root,
                size: PtySize { cols: 78, rows: 20 },
            }),
        ]
    );
    assert_eq!(
        next_events(&client, 4),
        vec![
            SessionEvent::PaneClosing { pane_id: created },
            SessionEvent::PaneRemoved {
                pane_id: created,
                tab_id: tab,
            },
            SessionEvent::LayoutChanged { tab_id: tab },
            SessionEvent::PaneFocused {
                client_id: client.id,
                tab_id: tab,
                pane_id: root,
                prior_pane: Some(created),
            },
        ]
    );
    assert_eq!(code, CliExitCode::Success);

    // No `--force`, so the pane's own close policy picks the kill: a graceful
    // one carrying the standard window.
    assert_eq!(
        kills_of(&session, created),
        vec![KillPolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
    // The pane that stayed takes the whole tab back.
    assert_eq!(last_size(&session, root), PtySize { cols: 78, rows: 20 });
    let overview = session.overview();
    assert_eq!(
        overview
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<Vec<_>>(),
        vec![root]
    );
}

#[test]
fn resize_pane_over_the_socket_moves_the_border_and_resizes_the_child() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    let tab = only_tab(&session);
    let created = split(&session, &client, root);
    let _ = next_events(&client, 3);
    // The split left both children at 38 columns by 20 rows.
    assert_eq!(last_size(&session, created), PtySize { cols: 38, rows: 20 });

    let (result, code) = run_cli(
        &session,
        &client,
        created,
        &["koshi", "resize-pane", "--direction", "left", "--size", "5"],
    );

    // Moving the pane's left border outward by 5 cells widens it to 43 and
    // narrows the neighbor on that side to 33.
    assert_eq!(
        applied_events(&result),
        [
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PtyResized(PtyResized {
                pane_id: root,
                size: PtySize { cols: 33, rows: 20 },
            }),
            Event::PtyResized(PtyResized {
                pane_id: created,
                size: PtySize { cols: 43, rows: 20 },
            }),
        ]
    );
    assert_eq!(
        next_events(&client, 1),
        vec![SessionEvent::LayoutChanged { tab_id: tab }]
    );
    assert_eq!(code, CliExitCode::Success);

    assert_eq!(last_size(&session, created), PtySize { cols: 43, rows: 20 });
    assert_eq!(last_size(&session, root), PtySize { cols: 33, rows: 20 });
}

#[test]
fn lock_over_the_socket_puts_the_client_in_locked_input_mode() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    assert_eq!(lock_state(&session, client.id), LockMode::Normal);

    let (result, code) = run_cli(&session, &client, root, &["koshi", "lock"]);

    assert_eq!(
        applied_events(&result),
        [Event::InputModeChanged(InputModeChanged {
            client_id: client.id,
            mode: InputMode::Locked,
        })]
    );
    assert_eq!(code, CliExitCode::Success);
    assert_eq!(lock_state(&session, client.id), LockMode::Locked);
}

#[test]
fn unlock_over_the_socket_returns_the_client_to_normal_input_mode() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    let (_, locked_code) = run_cli(&session, &client, root, &["koshi", "lock"]);
    assert_eq!(locked_code, CliExitCode::Success);
    assert_eq!(lock_state(&session, client.id), LockMode::Locked);

    let (result, code) = run_cli(&session, &client, root, &["koshi", "unlock"]);

    assert_eq!(
        applied_events(&result),
        [Event::InputModeChanged(InputModeChanged {
            client_id: client.id,
            mode: InputMode::Normal,
        })]
    );
    assert_eq!(code, CliExitCode::Success);
    assert_eq!(lock_state(&session, client.id), LockMode::Normal);
}

// --- The debug dumps ---

#[test]
fn dump_layout_over_the_socket_describes_a_tab_no_client_is_viewing() {
    // The session is seeded headless, so its tab has a tree and nothing to
    // solve it against.
    let session = RunningSession::start();
    let root = session.panes()[0];

    let layout = koshi_link::ipc_client::fetch_layout(session.dir.path(), session.id, None)
        .expect("the session describes its layout");

    assert_eq!(layout.id, session.id);
    assert_eq!(layout.name, "quiet-lake");
    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].index, 0);
    assert_eq!(
        layout.tabs[0].tree,
        koshi_layout::tree::LayoutNode::Pane(root)
    );
    assert_eq!(layout.tabs[0].solved, Vec::new());
    assert_eq!(layout.clients, Vec::new());

    let rendered = koshi::output::render_layouts(&[layout], koshi::cli::FormatArg::Table);
    assert!(
        rendered.contains("    no client views this tab\n"),
        "{rendered}"
    );
}

#[test]
fn dump_layout_over_the_socket_shows_the_attached_clients_solved_rectangles() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];

    let layout = koshi_link::ipc_client::fetch_layout(session.dir.path(), session.id, None)
        .expect("the session describes its layout");

    assert_eq!(layout.tabs.len(), 1);
    let solved = &layout.tabs[0].solved;
    assert_eq!(solved.len(), 1);
    assert_eq!(solved[0].client, client.id);
    // The tab solves against the terminal minus its two chrome rows: an 80x24
    // client results in an 80x22 viewport.
    assert_eq!(solved[0].viewport, Size { cols: 80, rows: 22 });
    assert_eq!(solved[0].mode, koshi_layout::mode::LayoutMode::Tiled);
    assert_eq!(
        solved[0].panes,
        vec![koshi_ipc::layout::SolvedPane {
            id: root,
            rect: koshi_core::geometry::Rect::new(
                koshi_core::geometry::Point { x: 0, y: 0 },
                Size { cols: 80, rows: 22 },
            ),
        }],
    );
    assert_eq!(solved[0].suppressed, Vec::new());
    assert!(!solved[0].all_suppressed);
    assert_eq!(solved[0].stack_headers, Vec::new());
    assert_eq!(
        layout.clients,
        vec![koshi_ipc::layout::ClientFocus {
            id: client.id,
            active_tab: layout.tabs[0].id,
            focused_pane: Some(root),
        }],
    );
}

#[test]
fn dump_layout_over_the_socket_narrowed_to_one_tab_describes_that_tab_alone() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    let (_, code) = run_cli(&session, &client, root, &["koshi", "new-tab"]);
    assert_eq!(code, CliExitCode::Success);
    let wanted = session.overview().tabs[1].id;

    let layout = koshi_link::ipc_client::fetch_layout(session.dir.path(), session.id, Some(wanted))
        .expect("the session describes its layout");

    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].id, wanted);
    assert_eq!(layout.tabs[0].index, 1);
}

#[test]
fn dump_layout_over_the_socket_narrowed_to_an_unknown_tab_reports_the_tab_missing() {
    // The session answers; it simply holds no such tab. Naming a tab and
    // getting an empty answer is a missing target, not a successful dump.
    let session = RunningSession::start();
    let unknown = TabId::new();

    let error = koshi_link::ipc_client::fetch_layout(session.dir.path(), session.id, Some(unknown))
        .expect_err("the session holds no such tab");

    assert_eq!(
        error.to_string(),
        CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some(format!("no running session has tab {unknown}")),
        }
        .to_string(),
    );
}

#[test]
fn dump_layout_against_a_session_that_is_not_running_reports_it_as_not_running() {
    let runtime_dir = test_runtime_dir();
    let session = SessionId::new();

    let error = koshi_link::ipc_client::fetch_layout(runtime_dir.path(), session, None)
        .expect_err("nothing advertises that session");

    assert!(
        matches!(&error, CliError::SessionNotFound { session: named } if *named == session.to_string()),
        "expected SessionNotFound, got {error:?}",
    );
}

#[test]
fn dump_state_over_the_socket_hides_a_pane_commands_arguments() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];
    let (_, code) = run_cli(
        &session,
        &client,
        root,
        &["koshi", "run", "--", "mysql", "-pHUNTER2"],
    );
    assert_eq!(code, CliExitCode::Success);

    let mut found = vec![session.overview()];
    koshi_link::discovery::redact_pane_commands(&mut found);

    let command_pane = found[0]
        .panes
        .iter()
        .find(|pane| pane.id != root)
        .expect("the command pane is listed");
    assert_eq!(
        command_pane.command,
        Some(vec!["mysql".to_string(), "***".to_string()]),
    );

    let rendered = koshi::output::render_dump_state(&found, koshi::cli::FormatArg::Table);
    assert!(rendered.contains("mysql ***"), "{rendered}");
    assert!(
        !rendered.contains("HUNTER2"),
        "the password never reaches the dump: {rendered}"
    );
}

#[test]
fn a_resize_with_no_neighbor_is_refused_and_reports_the_action_exit_code() {
    let session = RunningSession::start();
    let client = attach(&session);
    let root = session.panes()[0];

    // The session holds one pane, so neither border of it can move: nothing
    // sits beside it to take the cells from.
    let (result, code) = run_cli(
        &session,
        &client,
        root,
        &["koshi", "resize-pane", "--direction", "left", "--size", "5"],
    );

    let CommandResult::Rejected { reason, .. } = &result else {
        panic!("expected a rejection, got {result:?}");
    };
    assert_eq!(*reason, RejectReason::InvalidState);
    assert_eq!(code, CliExitCode::RuntimeAction);
    assert_eq!(session.panes().len(), 1);
    assert_eq!(last_size(&session, root), PtySize { cols: 78, rows: 20 });
}
