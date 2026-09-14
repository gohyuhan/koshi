//! What a `koshi` verb typed inside a session does, from the words on the
//! command line to the number the process exits with.
//!
//! Each test walks one verb the whole way: `Cli::try_parse_from` reads the real
//! argv, [`CliCommand::build_action_command`](koshi::cli::CliCommand::build_action_command) maps it to
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
    Event, InputModeChanged, LayoutChanged, PaneClosing, PaneCreated, PaneFocused, PaneRemoved,
    PtyResized, RejectReason,
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
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_test_runtime_directory;
use tempfile::TempDir;

/// How long a poll waits for something the session server has to do before the
/// test calls it a failure.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const CLI_ROUND_TRIP_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// The terminal size the session starts at and the attaching client reports.
const ATTACH_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// One session server running on its own thread, serving a real control socket
/// in its own runtime directory over a fake PTY backend. Dropping it stops
/// that thread and withdraws the socket.
struct RunningSession {
    /// The runtime directory the control socket and endpoint file live in.
    runtime_directory: TempDir,
    /// The session the server seeded and serves.
    session_id: SessionId,
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
    fn start_session() -> RunningSession {
        let runtime_directory = build_test_runtime_directory();
        let session_id = SessionId::new();
        let pty = Arc::new(FakePtyBackend::new());
        let (inbox_tx, inbox_rx) = mpsc::channel();

        let serving_runtime_directory = runtime_directory.path().to_path_buf();
        let serving_pty = Arc::clone(&pty);
        let serving_tx = inbox_tx.clone();
        let dispatcher = std::thread::spawn(move || {
            serve_session(
                &serving_runtime_directory,
                session_id,
                serving_pty,
                inbox_rx,
                serving_tx,
            );
        });

        let running_session = RunningSession {
            runtime_directory,
            session_id,
            pty,
            inbox_tx,
            dispatcher: Some(dispatcher),
        };
        // The endpoint file is written after the socket binds, so a readable
        // one means the socket is ready to answer.
        let deadline = Instant::now() + WAIT_DURATION;
        while EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
            running_session.runtime_directory.path(),
            running_session.session_id,
        ))
        .is_err()
        {
            assert!(
                Instant::now() < deadline,
                "the session server never advertised its socket"
            );
            std::thread::sleep(CLI_ROUND_TRIP_POLL_INTERVAL_DURATION);
        }
        running_session
    }

    /// The panes the backend spawned, in spawn order.
    fn list_pane_ids(&self) -> Vec<PaneId> {
        self.pty.list_spawned_pane_ids()
    }

    /// The session's own report of itself, read over the control socket by the
    /// library call the `koshi inspect` verbs make.
    fn fetch_session_overview(&self) -> koshi_core::discovery::SessionOverview {
        koshi_link::ipc_client::fetch_session_overview(
            self.runtime_directory.path(),
            self.session_id,
        )
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
/// socket in `runtime_directory`, and serve the runtime inbox until the session ends.
///
/// The order is the running binary's: the session is seeded before the socket
/// binds, so nothing advertises a session that does not exist yet.
fn serve_session(
    runtime_directory: &Path,
    session_id: SessionId,
    pty: Arc<FakePtyBackend>,
    inbox_rx: mpsc::Receiver<RuntimeEvent>,
    inbox_tx: mpsc::Sender<RuntimeEvent>,
) {
    let backend: Arc<dyn PtyBackend> = pty;
    let mut server = Server::from_runtime_parts(backend, inbox_rx, inbox_tx.clone());
    server.load_startup_config(None);
    server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            ATTACH_VIEWPORT_SIZE,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");

    let ipc_server = IpcServer::start(runtime_directory, session_id, inbox_tx, None)
        .expect("the control socket binds");
    server.attach_ipc_server(ipc_server);

    run_session_event_loop(&mut server);
    server.shutdown();
}

/// Serve the runtime inbox until the session ends: block until an event is due
/// (bounded by the next render deadline), apply it and any others already
/// queued, hand a fresh snapshot to any subscriber that lost a critical event,
/// push every attached client its frame when a render is due, and stop once the
/// inbox loses its last sender, a hangup arrives, a quit is applied, or no pane
/// is left running.
fn run_session_event_loop(server: &mut Server) {
    loop {
        let now = Instant::now();
        let pending_event = match server.next_render_wakeup(now) {
            Some(timeout_duration) => match server.inbox_rx().recv_timeout(timeout_duration) {
                Ok(runtime_event) => Some(runtime_event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match server.inbox_rx().recv() {
                Ok(runtime_event) => Some(runtime_event),
                Err(_) => break,
            },
        };
        let mut is_quit_requested = false;
        if let Some(runtime_event) = pending_event {
            is_quit_requested |= server.handle_runtime_event(runtime_event).is_break();
        }
        while let Ok(runtime_event) = server.inbox_rx().try_recv() {
            is_quit_requested |= server.handle_runtime_event(runtime_event).is_break();
        }
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
        if is_quit_requested || server.is_quit_requested() || !server.has_active_panes() {
            break;
        }
    }
}

/// A client attached over the control socket, with its event stream drained by
/// its own thread into a queue this thread polls.
struct AttachedClient {
    /// The client the session minted for this connection.
    client_id: ClientId,
    /// Every frame the session wrote that says something about its structure,
    /// in arrival order. A painted frame carries no structure change, so the
    /// reader passes it over.
    events: mpsc::Receiver<SessionEvent>,
}

/// The endpoint file the session server advertises: the socket address and the
/// token a Hello presents.
fn get_session_endpoint(session: &RunningSession) -> EndpointFile {
    EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        session.runtime_directory.path(),
        session.session_id,
    ))
    .expect("the session server advertises its socket")
}

/// Open a connection to the socket `endpoint` advertises, with its handshake
/// already done.
fn open_session_connection(endpoint: &EndpointFile) -> Connection {
    let mut connection = Connection::connect(&endpoint.socket_address).expect("the socket answers");
    let hello = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: endpoint.connection_token.clone(),
            is_remote: false,
        },
    };
    connection.send(&hello).expect("the server reads the Hello");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the Hello");
    match ipc_response.answer_result {
        IpcResult::Hello { .. } => connection,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Attach to `session` the way the attached client does — Hello then Attach on
/// one connection — and hand back the client the server minted plus its event
/// stream.
///
/// The connection is moved into the reader thread, which ends when the session
/// stops serving and closes it.
fn attach_test_client(session: &RunningSession) -> AttachedClient {
    let mut connection = open_session_connection(&get_session_endpoint(session));
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Attach {
            viewport: ATTACH_VIEWPORT_SIZE,
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };
    connection
        .send(&request)
        .expect("the server reads the attach");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(ipc_response.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id,
        ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    assert_eq!(session_id, session.session_id);

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

    AttachedClient { client_id, events }
}

/// The next `event_count` frames the attached client is told about the session's
/// structure, in arrival order. Fails the test once [`WAIT_DURATION`] has passed
/// with fewer than `event_count` of them.
fn receive_session_events(client: &AttachedClient, event_count: usize) -> Vec<SessionEvent> {
    (0..event_count)
        .map(|_| {
            client
                .events
                .recv_timeout(WAIT_DURATION)
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
fn run_cli_invocation(
    session: &RunningSession,
    client: &AttachedClient,
    pane: PaneId,
    argv: &[&str],
) -> (CommandResult, CliExitCode) {
    let cli = Cli::try_parse_from(argv).expect("the argv parses");
    let (_, action_command) = cli
        .command
        .as_ref()
        .expect("the argv carries a subcommand")
        .build_action_command(&ResolvedTargets::default(), Direction::Right)
        .expect("the subcommand is an action verb");

    let command_result = submit_session_command(session, client, pane, action_command);
    let exit_code = match report_command_result(&command_result) {
        Ok(()) => CliExitCode::Success,
        Err(command_error) => CliExitCode::from(&command_error),
    };
    (command_result, exit_code)
}

/// The same walk as [`run_cli_invocation`] for a verb typed OUTSIDE any pane: the real
/// argv, the core command
/// [`CliCommand::build_action_command`](koshi::cli::CliCommand::build_action_command) builds, and the
/// client
/// [`CliCommand::get_source_client_id`](koshi::cli::CliCommand::get_source_client_id) reads
/// off the same parse, all the way to the session's answer and the exit code.
///
/// The command travels as [`CommandSource::ExternalCli`], the only source that
/// carries a target client.
fn run_external_cli_invocation(
    session: &RunningSession,
    argv: &[&str],
) -> (CommandResult, CliExitCode) {
    let cli = Cli::try_parse_from(argv).expect("the argv parses");
    let parsed_command = cli.command.as_ref().expect("the argv carries a subcommand");
    let (_, action_command) = parsed_command
        .build_action_command(&ResolvedTargets::default(), Direction::Right)
        .expect("the subcommand is an action verb");

    let command_result = koshi_link::ipc_client::submit_external_command_via_runtime_directory(
        session.runtime_directory.path(),
        session.session_id,
        parsed_command.get_source_client_id(),
        action_command,
    )
    .expect("the session answers the command");
    let exit_code = match report_command_result(&command_result) {
        Ok(()) => CliExitCode::Success,
        Err(command_error) => CliExitCode::from(&command_error),
    };
    (command_result, exit_code)
}

/// Submit `command` to `session` over its control socket, enveloped the way the
/// CLI running inside `pane` envelopes it, and hand back the dispatcher's
/// result.
fn submit_session_command(
    session: &RunningSession,
    client: &AttachedClient,
    pane: PaneId,
    command: Command,
) -> CommandResult {
    let session_endpoint = get_session_endpoint(session);
    let mut connection = open_session_connection(&session_endpoint);
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_in_session_cli(
            session.session_id,
            Some(client.client_id),
            pane,
            PathBuf::from(session_endpoint.socket_address),
        ),
        SystemTime::now(),
        command,
    );
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    connection
        .send(&request)
        .expect("the server reads the command");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the command");
    assert_eq!(ipc_response.request_id, Some(2));
    match ipc_response.answer_result {
        IpcResult::CommandResult(command_result) => command_result,
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
}

/// What the binary reports for a dispatched command: an applied command is a
/// success, and a rejected one is the [`CliError`] the exit-code table reads.
fn report_command_result(command_result: &CommandResult) -> Result<(), CliError> {
    match command_result {
        CommandResult::Ok { .. } => Ok(()),
        CommandResult::Rejected { reason, help, .. } => Err(CliError::CommandRejected {
            reason: *reason,
            help: help.clone(),
        }),
    }
}

/// The events an applied result carries, or the rejection that carried none.
fn list_emitted_events(command_result: &CommandResult) -> &[Event] {
    match command_result {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        unexpected_result => panic!("expected an applied command, got {unexpected_result:?}"),
    }
}

/// The tab the session's only tab is, read from its own report.
fn get_only_tab_id(session: &RunningSession) -> TabId {
    let session_overview = session.fetch_session_overview();
    assert_eq!(session_overview.tabs.len(), 1);
    session_overview.tabs[0].tab_id
}

/// The size the pane's child was last given, which is the size the layout
/// solved for that pane.
fn get_last_pane_size(session: &RunningSession, pane_id: PaneId) -> PtySize {
    *session
        .pty
        .list_pane_sizes(pane_id)
        .expect("the pane was spawned")
        .last()
        .expect("the spawn recorded the pane's first size")
}

/// The kills the backend recorded for `pane`, waited for. A closed pane's
/// child is killed on its own thread, so the record lands after the command is
/// answered.
fn wait_for_pane_kill_policies(session: &RunningSession, pane_id: PaneId) -> Vec<KillPolicy> {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        let kills = session
            .pty
            .list_pane_kill_policies(pane_id)
            .expect("the pane was spawned");
        if !kills.is_empty() {
            return kills;
        }
        assert!(
            Instant::now() < deadline,
            "the closed pane's child was never killed"
        );
        std::thread::sleep(CLI_ROUND_TRIP_POLL_INTERVAL_DURATION);
    }
}

/// The lock mode the session holds for `client`.
fn get_client_lock_mode(session: &RunningSession, client_id: ClientId) -> LockMode {
    let session_overview = session.fetch_session_overview();
    let matching_client = session_overview
        .clients
        .iter()
        .find(|client_discovery| client_discovery.client_id == client_id)
        .expect("the client is attached to the session");
    matching_client.lock_mode
}

/// One solve per client viewing `tab_id`, read over the control socket by the
/// library call `koshi debug dump-layout` makes. Fails the test when the
/// session reports any tab other than `tab_id` alone.
fn list_tab_layout_solves(
    session: &RunningSession,
    tab_id: TabId,
) -> Vec<koshi_ipc::layout::SolvedTab> {
    let layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        session.session_id,
        None,
    )
    .expect("the session describes its layout");
    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].tab_id, tab_id);
    layout.tabs[0].solved_tabs.clone()
}

/// The mode `client_id` uses for the tab, taken from `layout_solves`.
fn get_client_layout_mode(
    layout_solves: &[koshi_ipc::layout::SolvedTab],
    client_id: ClientId,
) -> koshi_layout::mode::LayoutMode {
    layout_solves
        .iter()
        .find(|layout_solve| layout_solve.client_id == client_id)
        .expect("the client views the tab")
        .layout_mode
}

/// Split `pane_id` in two and hand back the pane the split created, so a test
/// that needs a neighbor starts from a two-pane tab.
fn build_neighboring_pane(
    session: &RunningSession,
    client: &AttachedClient,
    pane_id: PaneId,
) -> PaneId {
    let (command_result, exit_code) = run_cli_invocation(
        session,
        client,
        pane_id,
        &["koshi", "new-pane", "--direction", "right"],
    );
    assert_eq!(exit_code, CliExitCode::Success);
    match list_emitted_events(&command_result) {
        [Event::PaneCreated(created_pane), ..] => created_pane.pane_id,
        unexpected_events => panic!("expected a pane to be created, got {unexpected_events:?}"),
    }
}

#[test]
fn new_pane_over_the_socket_splits_and_reports_success() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);

    let (command_result, exit_code) = run_cli_invocation(
        &session,
        &client,
        root_pane_id,
        &["koshi", "new-pane", "--direction", "right"],
    );

    // The split spawned exactly one more child, and the CLI's `--direction`
    // put it beside the pane the command was typed in.
    assert_eq!(session.list_pane_ids().len(), 2);
    let created_pane_id = session.list_pane_ids()[1];
    assert_eq!(
        list_emitted_events(&command_result),
        [
            Event::PaneCreated(PaneCreated {
                pane_id: created_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id: client.client_id,
                tab_id,
                pane_id: created_pane_id,
                previous_pane_id: Some(root_pane_id),
            }),
            // Both children are sized to the rects the split solved.
            Event::PtyResized(PtyResized {
                pane_id: created_pane_id,
                pty_size: PtySize {
                    column_count: 38,
                    row_count: 20
                },
            }),
            Event::PtyResized(PtyResized {
                pane_id: root_pane_id,
                pty_size: PtySize {
                    column_count: 38,
                    row_count: 20
                },
            }),
        ]
    );
    assert_eq!(
        receive_session_events(&client, 3),
        vec![
            SessionEvent::PaneCreated {
                pane_id: created_pane_id,
                tab_id,
            },
            SessionEvent::LayoutChanged { tab_id },
            SessionEvent::PaneFocused {
                client_id: client.client_id,
                tab_id,
                pane_id: created_pane_id,
                previous_pane_id: Some(root_pane_id),
            },
        ]
    );
    assert_eq!(exit_code, CliExitCode::Success);
}

#[test]
fn close_pane_over_the_socket_kills_the_child_and_removes_the_pane() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);
    let created_pane_id = build_neighboring_pane(&session, &client, root_pane_id);
    // The split's own frames are the previous test's subject.
    let _ = receive_session_events(&client, 3);

    let (command_result, exit_code) =
        run_cli_invocation(&session, &client, created_pane_id, &["koshi", "close-pane"]);

    // The close is one transaction: the pane the command was typed in leaves
    // the layout, the client's focus falls back to the pane that stayed, and
    // that pane's child is resized to the space it took back.
    assert_eq!(
        list_emitted_events(&command_result),
        [
            Event::PaneClosing(PaneClosing {
                pane_id: created_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: created_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id: client.client_id,
                tab_id,
                pane_id: root_pane_id,
                previous_pane_id: Some(created_pane_id),
            }),
            Event::PtyResized(PtyResized {
                pane_id: root_pane_id,
                pty_size: PtySize {
                    column_count: 78,
                    row_count: 20
                },
            }),
        ]
    );
    assert_eq!(
        receive_session_events(&client, 4),
        vec![
            SessionEvent::PaneClosing {
                pane_id: created_pane_id,
            },
            SessionEvent::PaneRemoved {
                pane_id: created_pane_id,
                tab_id,
            },
            SessionEvent::LayoutChanged { tab_id },
            SessionEvent::PaneFocused {
                client_id: client.client_id,
                tab_id,
                pane_id: root_pane_id,
                previous_pane_id: Some(created_pane_id),
            },
        ]
    );
    assert_eq!(exit_code, CliExitCode::Success);

    // No `--force`, so the pane's own close policy picks the kill: a graceful
    // one carrying the standard window.
    assert_eq!(
        wait_for_pane_kill_policies(&session, created_pane_id),
        vec![KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
    // The pane that stayed takes the whole tab back.
    assert_eq!(
        get_last_pane_size(&session, root_pane_id),
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
    let session_overview = session.fetch_session_overview();
    assert_eq!(
        session_overview
            .panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<Vec<_>>(),
        vec![root_pane_id]
    );
}

#[test]
fn resize_pane_over_the_socket_moves_the_border_and_resizes_the_child() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);
    let created_pane_id = build_neighboring_pane(&session, &client, root_pane_id);
    let _ = receive_session_events(&client, 3);
    // The split left both children at 38 columns by 20 rows.
    assert_eq!(
        get_last_pane_size(&session, created_pane_id),
        PtySize {
            column_count: 38,
            row_count: 20
        }
    );

    let (command_result, exit_code) = run_cli_invocation(
        &session,
        &client,
        created_pane_id,
        &["koshi", "resize-pane", "--direction", "left", "--size", "5"],
    );

    // Moving the pane's left border outward by 5 cells widens it to 43 and
    // narrows the neighbor on that side to 33.
    assert_eq!(
        list_emitted_events(&command_result),
        [
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PtyResized(PtyResized {
                pane_id: root_pane_id,
                pty_size: PtySize {
                    column_count: 33,
                    row_count: 20
                },
            }),
            Event::PtyResized(PtyResized {
                pane_id: created_pane_id,
                pty_size: PtySize {
                    column_count: 43,
                    row_count: 20
                },
            }),
        ]
    );
    assert_eq!(
        receive_session_events(&client, 1),
        vec![SessionEvent::LayoutChanged { tab_id }]
    );
    assert_eq!(exit_code, CliExitCode::Success);

    assert_eq!(
        get_last_pane_size(&session, created_pane_id),
        PtySize {
            column_count: 43,
            row_count: 20
        }
    );
    assert_eq!(
        get_last_pane_size(&session, root_pane_id),
        PtySize {
            column_count: 33,
            row_count: 20
        }
    );
}

#[test]
fn lock_over_the_socket_puts_the_client_in_locked_input_mode() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Normal
    );

    let (command_result, exit_code) =
        run_cli_invocation(&session, &client, root_pane_id, &["koshi", "lock"]);

    assert_eq!(
        list_emitted_events(&command_result),
        [Event::InputModeChanged(InputModeChanged {
            client_id: client.client_id,
            lock_mode: LockMode::Locked,
        })]
    );
    assert_eq!(exit_code, CliExitCode::Success);
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Locked
    );
}

#[test]
fn unlock_over_the_socket_returns_the_client_to_normal_input_mode() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let (_, locked_exit_code) =
        run_cli_invocation(&session, &client, root_pane_id, &["koshi", "lock"]);
    assert_eq!(locked_exit_code, CliExitCode::Success);
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Locked
    );

    let (command_result, exit_code) =
        run_cli_invocation(&session, &client, root_pane_id, &["koshi", "unlock"]);

    assert_eq!(
        list_emitted_events(&command_result),
        [Event::InputModeChanged(InputModeChanged {
            client_id: client.client_id,
            lock_mode: LockMode::Normal,
        })]
    );
    assert_eq!(exit_code, CliExitCode::Success);
    assert_eq!(
        get_client_lock_mode(&session, client.client_id),
        LockMode::Normal
    );
}

#[test]
fn new_tab_with_a_client_flag_switches_that_client_onto_the_new_tab() {
    let session = RunningSession::start_session();
    let issuer_client = attach_test_client(&session);
    let named_client = attach_test_client(&session);
    assert_ne!(issuer_client.client_id, named_client.client_id);
    let root_pane_id = session.list_pane_ids()[0];
    let first_tab_id = get_only_tab_id(&session);
    let named_client_id_text = named_client.client_id.to_string();

    let (_, exit_code) = run_cli_invocation(
        &session,
        &issuer_client,
        root_pane_id,
        &["koshi", "new-tab", "--client", &named_client_id_text],
    );

    assert_eq!(exit_code, CliExitCode::Success);
    let session_overview = session.fetch_session_overview();
    assert_eq!(session_overview.tabs.len(), 2);
    assert_eq!(session_overview.tabs[0].tab_id, first_tab_id);
    let new_tab_id = session_overview.tabs[1].tab_id;

    let layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        session.session_id,
        None,
    )
    .expect("the session describes its layout");
    let get_active_tab_id = |client_id: ClientId| {
        layout
            .clients
            .iter()
            .find(|client_focus| client_focus.client_id == client_id)
            .expect("the client is attached to the session")
            .active_tab_id
    };
    assert_eq!(get_active_tab_id(named_client.client_id), new_tab_id);
    // The client that typed the command stays where it was.
    assert_eq!(get_active_tab_id(issuer_client.client_id), first_tab_id);
}

#[test]
fn a_client_flag_zooms_that_client_and_leaves_the_other_tiled() {
    let session = RunningSession::start_session();
    let issuer_client = attach_test_client(&session);
    let named_client = attach_test_client(&session);
    assert_ne!(issuer_client.client_id, named_client.client_id);
    let root_pane_id = session.list_pane_ids()[0];
    let tab_id = get_only_tab_id(&session);

    let named_client_id_text = named_client.client_id.to_string();
    let (command_result, exit_code) = run_external_cli_invocation(
        &session,
        &[
            "koshi",
            "toggle-pane-fullscreen",
            "--client",
            &named_client_id_text,
        ],
    );

    let CommandResult::Ok { .. } = &command_result else {
        panic!("the command was answered with {command_result:?}");
    };
    assert_eq!(exit_code, CliExitCode::Success);
    // The zoom lands on the named client's own view. The client that sent the
    // command keeps its tiled one.
    let layout_solves = list_tab_layout_solves(&session, tab_id);
    assert_eq!(layout_solves.len(), 2);
    assert_eq!(
        get_client_layout_mode(&layout_solves, named_client.client_id),
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(
        get_client_layout_mode(&layout_solves, issuer_client.client_id),
        koshi_layout::mode::LayoutMode::Tiled
    );
}

#[test]
fn a_fullscreen_command_naming_no_client_is_refused_and_zooms_nothing() {
    let session = RunningSession::start_session();
    let issuer_client = attach_test_client(&session);
    let named_client = attach_test_client(&session);
    assert_ne!(issuer_client.client_id, named_client.client_id);
    let tab_id = get_only_tab_id(&session);

    let (command_result, exit_code) =
        run_external_cli_invocation(&session, &["koshi", "toggle-pane-fullscreen"]);

    match &command_result {
        CommandResult::Rejected { reason, .. } => {
            assert_eq!(*reason, RejectReason::TargetAmbiguous);
        }
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
    assert_eq!(exit_code, CliExitCode::RuntimeAction);
    // A refused toggle changes nothing: both clients still solve the tab tiled.
    let layout_solves = list_tab_layout_solves(&session, tab_id);
    assert_eq!(layout_solves.len(), 2);
    assert_eq!(
        get_client_layout_mode(&layout_solves, named_client.client_id),
        koshi_layout::mode::LayoutMode::Tiled
    );
    assert_eq!(
        get_client_layout_mode(&layout_solves, issuer_client.client_id),
        koshi_layout::mode::LayoutMode::Tiled
    );
}

// --- The debug dumps ---

#[test]
fn dump_layout_over_the_socket_describes_a_tab_no_client_is_viewing() {
    // The session is seeded headless, so its tab has a tree and nothing to
    // solve it against.
    let session = RunningSession::start_session();
    let root_pane_id = session.list_pane_ids()[0];

    let layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        session.session_id,
        None,
    )
    .expect("the session describes its layout");

    assert_eq!(layout.session_id, session.session_id);
    assert_eq!(layout.session_name, "quiet-lake");
    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].tab_index, 0);
    assert_eq!(
        layout.tabs[0].layout_tree,
        koshi_layout::tree::LayoutNode::Pane(root_pane_id)
    );
    assert_eq!(layout.tabs[0].solved_tabs, Vec::new());
    assert_eq!(layout.clients, Vec::new());

    let rendered_output = koshi::output::render_layouts(&[layout], koshi::cli::OutputFormat::Table);
    assert!(
        rendered_output.contains("    no client views this tab\n"),
        "{rendered_output}"
    );
}

#[test]
fn dump_layout_over_the_socket_shows_the_attached_clients_solved_rectangles() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];

    let layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        session.session_id,
        None,
    )
    .expect("the session describes its layout");

    assert_eq!(layout.tabs.len(), 1);
    let solved_tabs = &layout.tabs[0].solved_tabs;
    assert_eq!(solved_tabs.len(), 1);
    assert_eq!(solved_tabs[0].client_id, client.client_id);
    // The tab solves against the terminal minus its two chrome rows: an 80x24
    // client results in an 80x22 viewport.
    assert_eq!(
        solved_tabs[0].viewport_size,
        Size {
            column_count: 80,
            row_count: 22
        }
    );
    assert_eq!(
        solved_tabs[0].layout_mode,
        koshi_layout::mode::LayoutMode::Tiled
    );
    assert_eq!(
        solved_tabs[0].pane_rects,
        vec![koshi_ipc::layout::SolvedPane {
            pane_id: root_pane_id,
            outer_rect: koshi_core::geometry::Rect::from_origin_and_size(
                koshi_core::geometry::Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 22
                },
            ),
        }],
    );
    assert_eq!(solved_tabs[0].suppressed_pane_ids, Vec::new());
    assert!(!solved_tabs[0].is_every_pane_suppressed);
    assert_eq!(solved_tabs[0].stack_headers, Vec::new());
    assert_eq!(
        layout.clients,
        vec![koshi_ipc::layout::ClientFocus {
            client_id: client.client_id,
            active_tab_id: layout.tabs[0].tab_id,
            focused_pane_id: Some(root_pane_id),
        }],
    );
}

#[test]
fn dump_layout_over_the_socket_narrowed_to_one_tab_describes_that_tab_alone() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let (_, exit_code) = run_cli_invocation(&session, &client, root_pane_id, &["koshi", "new-tab"]);
    assert_eq!(exit_code, CliExitCode::Success);
    let wanted_tab_id = session.fetch_session_overview().tabs[1].tab_id;

    let layout = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        session.session_id,
        Some(wanted_tab_id),
    )
    .expect("the session describes its layout");

    assert_eq!(layout.tabs.len(), 1);
    assert_eq!(layout.tabs[0].tab_id, wanted_tab_id);
    assert_eq!(layout.tabs[0].tab_index, 1);
}

#[test]
fn dump_layout_over_the_socket_narrowed_to_an_unknown_tab_reports_the_tab_missing() {
    // The session answers; it simply holds no such tab. Naming a tab and
    // getting an empty answer is a missing target, not a successful dump.
    let session = RunningSession::start_session();
    let unknown_tab_id = TabId::new();

    let layout_error = koshi_link::ipc_client::fetch_layout(
        session.runtime_directory.path(),
        session.session_id,
        Some(unknown_tab_id),
    )
    .expect_err("the session holds no such tab");

    assert_eq!(
        layout_error.to_string(),
        CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some(format!("no running session has tab {unknown_tab_id}")),
        }
        .to_string(),
    );
}

#[test]
fn dump_layout_against_a_session_that_is_not_running_reports_it_as_not_running() {
    let runtime_directory = build_test_runtime_directory();
    let session = SessionId::new();

    let session_error =
        koshi_link::ipc_client::fetch_layout(runtime_directory.path(), session, None)
            .expect_err("nothing advertises that session");

    assert!(
        matches!(&session_error, CliError::SessionNotFound { session_name } if *session_name == session.to_string()),
        "expected SessionNotFound, got {session_error:?}",
    );
}

#[test]
fn dump_state_over_the_socket_hides_a_pane_commands_arguments() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];
    let (_, exit_code) = run_cli_invocation(
        &session,
        &client,
        root_pane_id,
        &["koshi", "run", "--", "mysql", "-pHUNTER2"],
    );
    assert_eq!(exit_code, CliExitCode::Success);

    let mut redacted_session_overviews = vec![session.fetch_session_overview()];
    koshi_link::discovery::redact_pane_commands(&mut redacted_session_overviews);

    let command_pane = redacted_session_overviews[0]
        .panes
        .iter()
        .find(|pane_discovery| pane_discovery.pane_id != root_pane_id)
        .expect("the command pane is listed");
    assert_eq!(
        command_pane.command_argv,
        Some(vec!["mysql".to_string(), "***".to_string()]),
    );

    let rendered_output = koshi::output::render_dump_state(
        &redacted_session_overviews,
        koshi::cli::OutputFormat::Table,
    );
    assert!(rendered_output.contains("mysql ***"), "{rendered_output}");
    assert!(
        !rendered_output.contains("HUNTER2"),
        "the password never reaches the dump: {rendered_output}"
    );
}

#[test]
fn a_resize_with_no_neighbor_is_refused_and_reports_the_action_exit_code() {
    let session = RunningSession::start_session();
    let client = attach_test_client(&session);
    let root_pane_id = session.list_pane_ids()[0];

    // The session holds one pane, so neither border of it can move: nothing
    // sits beside it to take the cells from.
    let (command_result, exit_code) = run_cli_invocation(
        &session,
        &client,
        root_pane_id,
        &["koshi", "resize-pane", "--direction", "left", "--size", "5"],
    );

    let CommandResult::Rejected { reason, .. } = &command_result else {
        panic!("expected a rejection, got {command_result:?}");
    };
    assert_eq!(*reason, RejectReason::InvalidState);
    assert_eq!(exit_code, CliExitCode::RuntimeAction);
    assert_eq!(session.list_pane_ids().len(), 1);
    assert_eq!(
        get_last_pane_size(&session, root_pane_id),
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
}
