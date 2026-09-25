//! Integration cover for attaching over a real control socket: what one
//! attach registers, what the reply carries and when it is written, what a
//! second attach sees, what an attach naming a field this build does not know
//! still sets, what a client's key presses and resizes reach, what a client's
//! mouse rounds do to its panes and what the one answer each round is given
//! carries, what a detach leaves behind for the clients that stay and for the
//! panes, what a dropped connection leaves behind, and what a frame this build
//! cannot read costs the client that sent it.
//!
//! Each test runs the shape the per-session server process runs in: a headless
//! session seeded with no client, its inbox drained and its frames pushed on
//! the thread that owns the server, and the socket answered by the real accept
//! loop. The exchange with the socket runs on its own thread, since the caller
//! and the dispatcher must both be live for a request to be answered.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    CloseTabArgs, Command, CommandEnvelope, CommandResult, CommandSource, DetachArgs,
    FocusPaneArgs, FocusTabArgs, FocusTarget, NewPaneArgs, NewTabArgs, PanePlacementAnchor,
    PanePlacementTarget, PlacePaneArgs, PlacementRevision, TabTarget,
};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::Event;
use koshi_core::geometry::{Direction, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_core::mouse::{MouseAnswer, MouseButton, MouseInput, MouseKind, MouseTracking};
use koshi_core::process::{ExitStatus, PtySize};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    ConnectionToken, EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult,
    WireMouseAction, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::LayoutNode;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_session::client::{pane_viewport, ClientOrigin};
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_key_input_for_chord;

/// The terminal size [`attach`] reports, and the size the seeded session is
/// bootstrapped at.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The display name the seeded session carries.
const TEST_SESSION_NAME: &str = "workspace";

/// How long a test waits on work it cannot make happen itself — a disconnect
/// the serving thread has yet to notice, an event frame in flight — before
/// failing.
const TEST_WAIT_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// Sends [`RuntimeEvent::Quit`] when the exchange thread ends, on the way out
/// of a failed assertion as well as a clean return, which stops the
/// dispatcher.
struct StopDispatcher(Sender<RuntimeEvent>);

impl Drop for StopDispatcher {
    fn drop(&mut self) {
        let _ = self.0.send(RuntimeEvent::Quit);
    }
}

/// Seed a headless session under a fake PTY backend, serve its control socket
/// from a directory named for `tag`, and run `exchange` against that socket
/// while this thread drains the runtime inbox and pushes each attached
/// client its frames.
///
/// Returns the server and the fake backend once the exchange is done, so a
/// test can read the session state and the PTY sizes the exchange left behind,
/// plus whatever the exchange itself produced. `exchange` receives the runtime
/// directory and the session id, the two facts it needs to find and open the
/// socket, and the fake backend, which is how it makes a pane's program write
/// output while the session is running.
///
/// It hands back the connections it wants left open alongside its own value.
/// A connection dropped while the dispatcher is still running detaches its
/// client, so a test reading the registry keeps its connections here until the
/// dispatcher has stopped.
fn serve_test_session<T: Send + 'static>(
    tag: &str,
    exchange: impl FnOnce(PathBuf, SessionId, Arc<FakePtyBackend>) -> (Vec<Connection>, T)
        + Send
        + 'static,
) -> (Server, Arc<FakePtyBackend>, T) {
    #[cfg(unix)]
    let socket_path_base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_path_base = std::env::temp_dir();
    let runtime_directory =
        socket_path_base.join(format!("koshi-attach-{}-{tag}", std::process::id()));

    let session_id = SessionId::new();
    let fake = Arc::new(FakePtyBackend::new());
    let backend: Arc<dyn PtyBackend> = fake.clone();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let mut server = Server::from_runtime_parts(backend, inbox_rx, inbox_tx.clone());
    server
        .bootstrap_session(
            session_id,
            TEST_SESSION_NAME.to_string(),
            TEST_VIEWPORT_SIZE,
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("seed the session");
    let ipc = IpcServer::start(&runtime_directory, session_id, inbox_tx.clone(), None)
        .expect("start serving");

    let caller_dir = runtime_directory.clone();
    let caller_fake = fake.clone();
    let caller = std::thread::spawn(move || {
        let _stop = StopDispatcher(inbox_tx);
        exchange(caller_dir, session_id, caller_fake)
    });

    // The per-session server's own loop: block until an event is due, bounded
    // by the next render deadline, apply it, hand a fresh snapshot to any
    // subscriber that lost a critical event, then push every attached client
    // its frame when a render is due.
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
        if let Some(event) = event {
            if server.handle_runtime_event(event).is_break() {
                break;
            }
        }
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
    }

    let (open_connections, produced) = caller.join().expect("the exchange finished");
    drop(open_connections);
    ipc.shutdown();
    let _ = std::fs::remove_dir_all(&runtime_directory);
    (server, fake, produced)
}

/// Connect to the socket the endpoint file advertises and walk the Hello, so
/// the returned connection is open for every other request kind.
fn open_session_connection(runtime_directory: &Path, session_id: SessionId) -> Connection {
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("endpoint file readable");
    let mut connection = Connection::connect(&endpoint.socket_address).expect("connect");
    connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                connection_token: endpoint.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello");
    let ipc_response: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
    connection
}

/// Attach on `connection` reporting [`TEST_VIEWPORT_SIZE`], and return what the reply
/// carried. The connection carries only the client's event stream afterwards.
fn attach_test_client(
    connection: &mut Connection,
    request_id: u64,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<ConnectionToken>,
) {
    attach_test_client_with_viewport(connection, request_id, TEST_VIEWPORT_SIZE)
}

/// [`attach_test_client`] reporting `viewport_size` instead, so a test can put two differently
/// sized clients on one tab.
fn attach_test_client_with_viewport(
    connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<ConnectionToken>,
) {
    attach_test_client_with_resume_token(connection, request_id, viewport_size, None)
}

/// [`attach_test_client_with_viewport`] presenting `resume_token`, so a test can ask the session
/// for the view the token's client left behind.
fn attach_test_client_with_resume_token(
    connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
    resume_token: Option<ConnectionToken>,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<ConnectionToken>,
) {
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Attach {
                viewport: viewport_size,
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let ipc_response: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(ipc_response.request_id, Some(request_id));
    let IpcResult::Attached {
        client_id,
        session_id,
        session_structure,
        resume_token,
        ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    (client_id, session_id, session_structure, resume_token)
}

/// Add one tab to the running session over `connection`, and return its id.
fn build_test_tab(connection: &mut Connection, session_id: SessionId, request_id: u64) -> TabId {
    let command = Command::NewTab(NewTabArgs {
        working_directory: None,
        client_id: None,
    });
    submit_test_command(connection, session_id, command, request_id)
        .iter()
        .find_map(|event| match event {
            Event::TabCreated(payload) => Some(payload.tab_id),
            _ => None,
        })
        .expect("the new tab reports its id")
}

/// What the session reports about itself over `connection`.
fn get_session_overview(connection: &mut Connection, request_id: u64) -> SessionOverview {
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let ipc_response: IpcResponse = connection.recv().expect("discovery reply");
    let IpcResult::Overview(session_overview) = ipc_response.answer_result else {
        panic!("expected an overview, got {:?}", ipc_response.answer_result);
    };
    session_overview
}

/// How many clients the session reports over `connection`.
fn attached_client_count(connection: &mut Connection, request_id: u64) -> usize {
    get_session_overview(connection, request_id).clients.len()
}

/// Ask over `connection` until the session reports `expected_client_count` clients, numbering
/// the requests from `request_id`. Panics once [`TEST_WAIT_TIMEOUT_DURATION`] has passed.
fn wait_for_client_count(
    connection: &mut Connection,
    expected_client_count: usize,
    request_id: u64,
) {
    let deadline = Instant::now() + TEST_WAIT_TIMEOUT_DURATION;
    let mut request_id = request_id;
    loop {
        let observed_client_count = attached_client_count(connection, request_id);
        if observed_client_count == expected_client_count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the session reports {observed_client_count} attached clients, not {expected_client_count}",
        );
        request_id += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One envelope asking `session_id` for a new tab, issued by the external CLI
/// at [`SystemTime::UNIX_EPOCH`] under a fresh command id.
fn build_new_tab_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        SystemTime::UNIX_EPOCH,
        Command::NewTab(NewTabArgs {
            working_directory: None,
            client_id: None,
        }),
    )
}

/// Submit a command over `connection` and return the events it emitted.
/// Panics unless the session applied it.
fn submit_test_command(
    connection: &mut Connection,
    session_id: SessionId,
    command: Command,
    request_id: u64,
) -> Vec<Event> {
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        SystemTime::UNIX_EPOCH,
        command,
    );
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("send command");
    let ipc_response: IpcResponse = connection.recv().expect("command reply");
    let IpcResult::CommandResult(CommandResult::Ok {
        command_id: _,
        emitted_events,
    }) = ipc_response.answer_result
    else {
        panic!(
            "expected the command to apply, got {:?}",
            ipc_response.answer_result
        );
    };
    emitted_events
}

/// Read `connection`'s event stream until `is_target_frame` accepts a frame, on a thread
/// this one can give up waiting on. Returns the connection, still open, and
/// every frame read, the accepted one last.
///
/// Each frame is decoded as a [`SessionEvent`], so a response frame written on
/// an attached client's connection fails the read: an [`IpcResponse`] encodes
/// as a two-field record, a `SessionEvent` as a one-field one.
fn read_session_frames_until(
    mut connection: Connection,
    is_target_frame: impl Fn(&SessionEvent) -> bool + Send + 'static,
) -> (Connection, Vec<SessionEvent>) {
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut session_events = Vec::new();
        loop {
            let session_event: SessionEvent = connection.recv().expect("an event frame");
            let is_target_event = is_target_frame(&session_event);
            session_events.push(session_event);
            if is_target_event {
                break;
            }
        }
        let _ = done_tx.send((connection, session_events));
    });
    done_rx
        .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
        .expect("the awaited frame reaches the viewer")
}

/// [`read_session_frames_until`] stopping at [`SessionEvent::Detached`].
fn read_session_frames_to_detached(connection: Connection) -> (Connection, Vec<SessionEvent>) {
    read_session_frames_until(connection, |frame| *frame == SessionEvent::Detached)
}

/// The painted frame `session_events` ends with. Panics unless the last frame read is
/// a painted one.
fn get_last_painted_frame(session_events: &[SessionEvent]) -> &PaintedFrame {
    match session_events.last() {
        Some(SessionEvent::Painted { frame }) => frame,
        other => panic!("expected the run to end with a painted frame, got {other:?}"),
    }
}

#[test]
fn one_attach_registers_the_client_the_server_minted() {
    let (server, _fake, (session_id, client_id, structure)) =
        serve_test_session("registers", |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, replied_session, structure, _) = attach_test_client(&mut viewer, 2);
            assert_eq!(replied_session, session_id);
            (vec![viewer], (session_id, client_id, structure))
        });

    assert_eq!(structure.session_id, session_id);
    assert_eq!(structure.session_name, TEST_SESSION_NAME);
    assert_eq!(structure.tabs.len(), 1);
    assert_eq!(structure.tabs[0].tab_index, 0);
    assert_eq!(structure.panes.len(), 1);

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 1);
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("minted client");
    assert_eq!(client.get_client_id(), client_id);
    assert_eq!(client.get_session_id(), session_id);
    assert_eq!(client.get_origin(), ClientOrigin::Local);
    assert_eq!(client.get_color(), 0);
    assert_eq!(client.get_viewport_size(), TEST_VIEWPORT_SIZE);
    assert_eq!(client.get_active_tab(), structure.tabs[0].tab_id);
    let label: Vec<&str> = client.get_label().split('-').collect();
    assert_eq!(
        label.len(),
        3,
        "generated label, got {}",
        client.get_label()
    );
    assert_eq!(label[0], "C");
}

#[test]
fn nothing_in_the_request_can_raise_the_clients_authority() {
    let (server, _fake, session_id) =
        serve_test_session("strict", |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);

            // A well-framed attach naming one field this build does not know. The
            // field is ignored, so the attach succeeds — and every fact about the
            // client it mints comes from the server, never from these bytes.
            viewer
                .send(&serde_json::json!({
                    "request_id": 2,
                    "request_kind": {
                        "Attach": {
                            "viewport": { "column_count": 80, "row_count": 24 },
                            "event_filter": "All",
                            "tier": "admin"
                        }
                    }
                }))
                .expect("send an attach carrying an extra field");

            let ipc_response: IpcResponse = viewer.recv().expect("attach reply");
            assert_eq!(ipc_response.request_id, Some(2));
            let IpcResult::Attached {
                session_id: joined, ..
            } = ipc_response.answer_result
            else {
                panic!(
                    "the attach was answered with {:?}",
                    ipc_response.answer_result
                );
            };
            assert_eq!(joined, session_id, "the attach joined the session it named");
            (vec![viewer], session_id)
        });

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(
        session.clients.client_count(),
        1,
        "the attach registered one client"
    );
    let client = session
        .clients
        .list_attached_clients()
        .next()
        .expect("the one attached client");
    assert_eq!(
        client.get_origin(),
        ClientOrigin::Local,
        "the origin comes from the connection, not the request"
    );
    assert_eq!(
        client.get_viewport_size(),
        Size {
            column_count: 80,
            row_count: 24
        },
        "the viewport is the one field of the attach the server does take"
    );
}

#[test]
fn the_structure_reply_is_written_before_the_first_event_frame() {
    let (server, _fake, (session_id, booted_tab_id, added_tab_id)) =
        serve_test_session("reply-first", |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);

            // Frame one on this connection decodes as a response, so no event
            // frame was written ahead of the reply.
            let (_, _, structure, _) = attach_test_client(&mut viewer, 2);
            assert_eq!(structure.tabs.len(), 1);

            // Everything after it is an event frame. Reading one blocks with no
            // deadline of its own, so it happens on a thread this one can give up
            // waiting on.
            let mut caller = open_session_connection(&runtime_directory, session_id);
            let added_tab_id = build_test_tab(&mut caller, session_id, 3);

            let (found_tx, found_rx) = mpsc::channel();
            std::thread::spawn(move || loop {
                let frame: SessionEvent = viewer.recv().expect("an event frame");
                if let SessionEvent::TabCreated { tab_id } = frame {
                    let _ = found_tx.send(tab_id);
                    return;
                }
            });
            assert_eq!(
                found_rx
                    .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                    .expect("the new tab reaches the event stream"),
                added_tab_id,
            );
            (
                vec![caller],
                (session_id, structure.tabs[0].tab_id, added_tab_id),
            )
        });

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.tabs.len(), 2);
    assert!(session.tabs.contains_key(&booted_tab_id));
    assert!(session.tabs.contains_key(&added_tab_id));
}

#[test]
fn a_second_attach_mints_a_fresh_client_and_sees_the_tab_added_since_the_first() {
    let (server, _fake, (session_id, initial_client_id, additional_client_id)) =
        serve_test_session("reattach", |runtime_directory, session_id, _fake| {
            let mut initial_connection = open_session_connection(&runtime_directory, session_id);
            let (initial_client_id, _, initial_attach_snapshot, _) =
                attach_test_client(&mut initial_connection, 2);
            assert_eq!(initial_attach_snapshot.tabs.len(), 1);
            let booted_tab_id = initial_attach_snapshot.tabs[0].tab_id;

            let mut caller = open_session_connection(&runtime_directory, session_id);
            let added_tab_id = build_test_tab(&mut caller, session_id, 3);

            let mut additional_connection = open_session_connection(&runtime_directory, session_id);
            let (additional_client_id, _, additional_attach_snapshot, _) =
                attach_test_client(&mut additional_connection, 4);
            assert_ne!(additional_client_id, initial_client_id);
            assert_eq!(
                additional_attach_snapshot
                    .tabs
                    .iter()
                    .map(|tab| tab.tab_id)
                    .collect::<Vec<TabId>>(),
                vec![booted_tab_id, added_tab_id],
                "the second attach is built from live state, not a cached copy",
            );
            (
                vec![initial_connection, additional_connection, caller],
                (session_id, initial_client_id, additional_client_id),
            )
        });

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 2);
    let initial_client_record = session
        .clients
        .get_client_by_id(initial_client_id)
        .expect("initial client");
    let additional_client_record = session
        .clients
        .get_client_by_id(additional_client_id)
        .expect("additional client");
    assert_eq!(initial_client_record.get_color(), 0);
    assert_eq!(additional_client_record.get_color(), 1);
    assert_ne!(
        initial_client_record.get_label(),
        additional_client_record.get_label()
    );
}

/// Close `tab` in the running session over `connection`, killing its panes
/// outright. Closing the last tab quits the session.
fn close_tab(connection: &mut Connection, session_id: SessionId, tab: TabId, request_id: u64) {
    let command = Command::CloseTab(CloseTabArgs {
        tab_id: Some(tab),
        should_force_close: true,
        should_kill_process_tree: false,
    });
    let emitted_events = submit_test_command(connection, session_id, command, request_id);
    assert!(
        emitted_events
            .iter()
            .any(|event| matches!(event, Event::Quit(_))),
        "closing the last tab quits the session",
    );
}

#[test]
fn the_event_stream_ends_with_the_quit_frame() {
    let (server, _fake, session_id) = serve_test_session(
        "quit-ends-stream",
        |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (_, _, structure, _) = attach_test_client(&mut viewer, 2);
            let only_tab = structure.tabs[0].tab_id;

            let mut caller = open_session_connection(&runtime_directory, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 1);
            close_tab(&mut caller, session_id, only_tab, 4);

            // The quit frame is the last one the viewer's stream carries.
            let (viewer, frames) =
                read_session_frames_until(viewer, |frame| *frame == SessionEvent::Quit);
            assert_eq!(frames.last(), Some(&SessionEvent::Quit));

            // The quit frame ends the stream: its writing thread exits and
            // detaches the client. The viewer connection is still open, so the
            // record going away can only come from that exit.
            let deadline = Instant::now() + TEST_WAIT_TIMEOUT_DURATION;
            let mut request_id = 5;
            while attached_client_count(&mut caller, request_id) != 0 {
                assert!(
                    Instant::now() < deadline,
                    "the stream outlived its quit frame",
                );
                request_id += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            (vec![caller, viewer], session_id)
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 0);
}

/// A pane's program exiting reaches every attached client while the session
/// keeps serving. A second pane is what keeps it serving: the session ends when
/// its last pane goes, and an ending session drops what is queued for a client.
#[test]
fn the_stream_carries_a_pane_exit_while_the_session_keeps_serving() {
    let (server, _fake, (session_id, exited_pane_id)) = serve_test_session(
        "pane-exit-on-stream",
        |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, structure, _) = attach_test_client(&mut viewer, 2);
            let existing_pane_id = structure.panes[0].pane_id;

            let mut caller = open_session_connection(&runtime_directory, session_id);
            let emitted = submit_test_command(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source_pane_id: Some(existing_pane_id),
                    tab_id: None,
                    direction: Direction::Right,
                    should_stack: false,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: Some(client_id),
                }),
                3,
            );
            let new_pane_id = emitted
                .iter()
                .find_map(|event| match event {
                    Event::PaneCreated(created) => Some(created.pane_id),
                    _ => None,
                })
                .expect("the split emitted the new pane");

            // A dying program closes its terminal and then exits, and the forwarder
            // relays the exit once the output before it is drained.
            fake.close_output(new_pane_id)
                .expect("the second pane's terminal closes");
            fake.trigger_child_exit(new_pane_id, ExitStatus::ExitCode(0))
                .expect("the second pane's program exits");

            let (viewer, frames) = read_session_frames_until(viewer, move |frame| {
                matches!(
                    frame,
                    SessionEvent::PaneProcessExited { pane_id, .. }
                        if *pane_id == new_pane_id
                )
            });
            assert_eq!(
                frames.last(),
                Some(&SessionEvent::PaneProcessExited {
                    pane_id: new_pane_id,
                    exit_code: Some(0),
                    signal: None,
                }),
            );
            (vec![viewer, caller], (session_id, new_pane_id))
        },
    );

    // The pane the program left is gone, and the one the client is viewing is
    // still there.
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.panes.pane_record_count(), 1);
    assert!(session
        .panes
        .get_pane_record_by_id(exited_pane_id)
        .is_none());
}

/// The only pane's program exiting ends the session by itself, with no close
/// asked for: the stream ends with the quit frame, and the session is left with
/// no pane and no tab.
#[test]
fn the_event_stream_ends_with_the_quit_frame_when_the_only_program_exits() {
    let (server, _fake, session_id) = serve_test_session(
        "quit-on-program-exit",
        |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (_, _, structure, _) = attach_test_client(&mut viewer, 2);
            let only_pane = structure.panes[0].pane_id;

            // A dying program closes its terminal and then exits, and the forwarder
            // relays the exit once the output before it is drained.
            fake.close_output(only_pane)
                .expect("the only pane's terminal closes");
            fake.trigger_child_exit(only_pane, ExitStatus::ExitCode(0))
                .expect("the only pane's program exits");

            // The exit and the quit the session ends on are published in one pass,
            // and the raised ending drops whatever is still queued for a client, so
            // the quit frame is the one frame this stream is promised.
            let (viewer, frames) =
                read_session_frames_until(viewer, |frame| *frame == SessionEvent::Quit);
            assert_eq!(frames.last(), Some(&SessionEvent::Quit));
            (vec![viewer], session_id)
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.panes.pane_record_count(), 0);
    assert_eq!(session.tabs.len(), 0);
}

#[test]
fn dropping_an_attached_connection_removes_its_client_record() {
    let (server, _fake, (session_id, client_id, tab_id, pane_id)) =
        serve_test_session("disconnect", |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, structure, _) = attach_test_client(&mut viewer, 2);
            let tab_id = structure.tabs[0].tab_id;
            let pane_id = structure.panes[0].pane_id;
            let mut caller = open_session_connection(&runtime_directory, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 1);

            drop(viewer);

            // The record goes when the serving thread notices the connection
            // ended, so ask again until it is gone.
            let deadline = Instant::now() + TEST_WAIT_TIMEOUT_DURATION;
            let mut request_id = 4;
            while attached_client_count(&mut caller, request_id) != 0 {
                assert!(
                    Instant::now() < deadline,
                    "the client record outlived its connection",
                );
                request_id += 1;
                std::thread::sleep(Duration::from_millis(10));
            }

            // Losing the viewer costs the session nothing else: its tab and
            // its pane are both still there.
            let overview_after_disconnect = get_session_overview(&mut caller, request_id + 1);
            assert_eq!(
                overview_after_disconnect
                    .tabs
                    .iter()
                    .map(|tab| tab.tab_id)
                    .collect::<Vec<TabId>>(),
                vec![tab_id],
            );
            assert_eq!(
                overview_after_disconnect
                    .panes
                    .iter()
                    .map(|pane| pane.pane_id)
                    .collect::<Vec<PaneId>>(),
                vec![pane_id],
            );
            (vec![caller], (session_id, client_id, tab_id, pane_id))
        });

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 0);
    assert!(session.clients.get_client_by_id(client_id).is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane_id)
            .map(|pane| pane.get_pane_id()),
        Some(pane_id)
    );
}

#[test]
fn a_frame_this_build_cannot_read_costs_one_request_not_the_stream() {
    let (server, _fake, (session_id, client_id, added_tab_id)) = serve_test_session(
        "unreadable-frame",
        |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, structure, _) = attach_test_client(&mut viewer, 2);
            assert_eq!(structure.tabs.len(), 1, "the session starts with one tab");
            let mut caller = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller, 1, 3);

            // A frame this build cannot read: `SubmitCommand` is a kind it has,
            // and the command inside names a variant no build has, so the whole
            // frame fails to decode.
            let mut command_envelope_json =
                serde_json::to_value(build_new_tab_envelope(session_id))
                    .expect("the envelope encodes");
            command_envelope_json["command"] = serde_json::json!({ "CommandFromALaterKoshi": {} });
            viewer
                .send(&serde_json::json!({
                    "request_id": 20,
                    "request_kind": { "SubmitCommand": command_envelope_json },
                }))
                .expect("send a frame this build cannot read");

            // The frame after it is served: this one adds a tab, and the tab
            // reaches this viewer's own stream. Every frame read here decodes as
            // a `SessionEvent`, so nothing was written back for the frame above.
            viewer
                .send(&IpcRequest {
                    request_id: 21,
                    request_kind: IpcRequestKind::SubmitCommand(Box::new(build_new_tab_envelope(
                        session_id,
                    ))),
                })
                .expect("send the next request");
            let (viewer, session_events) = read_session_frames_until(viewer, |session_event| {
                matches!(session_event, SessionEvent::TabCreated { .. })
            });
            let added_tab_id = session_events
                .iter()
                .find_map(|session_event| match session_event {
                    SessionEvent::TabCreated { tab_id } => Some(*tab_id),
                    _ => None,
                })
                .expect("the new tab reaches the stream");

            assert_eq!(
                attached_client_count(&mut caller, 30),
                1,
                "the client that sent the unreadable frame is still attached"
            );
            (vec![viewer, caller], (session_id, client_id, added_tab_id))
        },
    );

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 1);
    assert!(session.clients.get_client_by_id(client_id).is_some());
    assert!(
        session.tabs.contains_key(&added_tab_id),
        "the request after the unreadable one applied"
    );
    assert_eq!(session.tabs.len(), 2, "the unreadable frame added no tab");
}

#[test]
fn detaching_one_client_leaves_every_other_stream_running() {
    let (server, _fake, (session_id, detached_client_id, remaining_client_id, added_tab_id)) =
        serve_test_session("detach-one", |runtime_directory, session_id, _fake| {
            let mut detaching_connection = open_session_connection(&runtime_directory, session_id);
            let (detached_client_id, _, _, _) = attach_test_client(&mut detaching_connection, 2);
            let mut remaining_connection = open_session_connection(&runtime_directory, session_id);
            let (remaining_client_id, _, _, _) = attach_test_client(&mut remaining_connection, 2);

            // The caller never attaches, so nothing it does rides an event
            // stream of its own.
            let mut caller = open_session_connection(&runtime_directory, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 2);

            let emitted = submit_test_command(
                &mut caller,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(detached_client_id),
                }),
                4,
            );
            // Both clients report the same viewport, so the tab the leaver
            // held keeps its size and the detach emits nothing.
            assert_eq!(emitted, Vec::new());

            // The detached frame is the last one the departing viewer's stream carries.
            let (detached_connection, detached_frames) =
                read_session_frames_to_detached(detaching_connection);
            assert_eq!(detached_frames.last(), Some(&SessionEvent::Detached));
            wait_for_client_count(&mut caller, 1, 5);

            // The client that stayed is untouched: a tab added now still
            // reaches its stream.
            let added_tab_id = build_test_tab(&mut caller, session_id, 100);
            let (tab_event_sender, tab_event_receiver) = mpsc::channel();
            std::thread::spawn(move || loop {
                let session_event: SessionEvent =
                    remaining_connection.recv().expect("an event frame");
                if let SessionEvent::TabCreated { tab_id } = session_event {
                    let _ = tab_event_sender.send((remaining_connection, tab_id));
                    return;
                }
            });
            let (remaining_connection_after_event, created_tab_id) = tab_event_receiver
                .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                .expect("the new tab reaches the client that stayed");
            assert_eq!(created_tab_id, added_tab_id);

            (
                vec![
                    caller,
                    detached_connection,
                    remaining_connection_after_event,
                ],
                (
                    session_id,
                    detached_client_id,
                    remaining_client_id,
                    added_tab_id,
                ),
            )
        });

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 1);
    assert!(session
        .clients
        .get_client_by_id(detached_client_id)
        .is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(remaining_client_id)
            .map(|client| client.get_client_id()),
        Some(remaining_client_id)
    );
    assert!(session.tabs.contains_key(&added_tab_id));
}

#[test]
fn detach_all_takes_every_client_and_leaves_the_session_whole() {
    let (
        server,
        _fake,
        (session_id, detached_first_client_id, detached_second_client_id, tab_id, pane_id),
    ) = serve_test_session("detach-all", |runtime_directory, session_id, _fake| {
        let mut first_attached_connection = open_session_connection(&runtime_directory, session_id);
        let (detached_first_client_id, _, structure, _) =
            attach_test_client(&mut first_attached_connection, 2);
        let mut second_attached_connection =
            open_session_connection(&runtime_directory, session_id);
        let (detached_second_client_id, _, _, _) =
            attach_test_client(&mut second_attached_connection, 2);

        let mut caller = open_session_connection(&runtime_directory, session_id);
        assert_eq!(attached_client_count(&mut caller, 3), 2);

        let emitted = submit_test_command(&mut caller, session_id, Command::DetachAll, 4);
        assert_eq!(emitted, Vec::new());

        // Every attached client's stream ends with the same detached frame.
        let (detached_first_connection, first_detached_frames) =
            read_session_frames_to_detached(first_attached_connection);
        let (detached_second_connection, second_detached_frames) =
            read_session_frames_to_detached(second_attached_connection);
        assert_eq!(first_detached_frames.last(), Some(&SessionEvent::Detached));
        assert_eq!(second_detached_frames.last(), Some(&SessionEvent::Detached));
        wait_for_client_count(&mut caller, 0, 5);

        // The session with nobody watching still holds its tab and pane.
        let overview_after_detach = get_session_overview(&mut caller, 100);
        assert_eq!(overview_after_detach.session.session_id, session_id);
        assert_eq!(
            overview_after_detach
                .tabs
                .iter()
                .map(|tab| tab.tab_id)
                .collect::<Vec<TabId>>(),
            vec![structure.tabs[0].tab_id],
        );
        assert_eq!(
            overview_after_detach
                .panes
                .iter()
                .map(|pane| pane.pane_id)
                .collect::<Vec<PaneId>>(),
            vec![structure.panes[0].pane_id],
        );

        (
            vec![
                caller,
                detached_first_connection,
                detached_second_connection,
            ],
            (
                session_id,
                detached_first_client_id,
                detached_second_client_id,
                structure.tabs[0].tab_id,
                structure.panes[0].pane_id,
            ),
        )
    });

    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    assert_eq!(session.clients.client_count(), 0);
    assert!(session
        .clients
        .get_client_by_id(detached_first_client_id)
        .is_none());
    assert!(session
        .clients
        .get_client_by_id(detached_second_client_id)
        .is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane_id)
            .map(|pane| pane.get_pane_id()),
        Some(pane_id)
    );
}

#[test]
fn detaching_the_smaller_client_grows_the_tabs_pty_back() {
    // Half the columns of [`TEST_VIEWPORT_SIZE`], the size the seeded session was
    // bootstrapped at.
    const NARROW_VIEWPORT_SIZE: Size = Size {
        column_count: 40,
        row_count: 24,
    };

    let (_server, fake, pane_id) =
        serve_test_session("detach-reflow", |runtime_directory, session_id, _fake| {
            // The narrow client attaches first and holds the tab down: the
            // effective tab size is the smallest viewport of every client viewing
            // it, so the pane's PTY shrinks.
            let mut narrow = open_session_connection(&runtime_directory, session_id);
            let (narrow_client, _, structure, _) =
                attach_test_client_with_viewport(&mut narrow, 2, NARROW_VIEWPORT_SIZE);
            let pane_id = structure.panes[0].pane_id;

            let mut wide = open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport(&mut wide, 2, TEST_VIEWPORT_SIZE);

            let mut caller = open_session_connection(&runtime_directory, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 2);

            // The narrow client leaves; only the full-width viewer is left, so the
            // tab grows back and the pane's PTY reflows with it.
            submit_test_command(
                &mut caller,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(narrow_client),
                }),
                4,
            );
            let (narrow, frames) = read_session_frames_to_detached(narrow);
            assert_eq!(frames.last(), Some(&SessionEvent::Detached));
            wait_for_client_count(&mut caller, 1, 5);

            (vec![caller, narrow, wide], pane_id)
        });

    // Three resizes, in this order: the size the seeded session gave the pane,
    // down by the difference in viewport width when the narrow client joined,
    // and back to the first size when it left. The rows never changed.
    let resizes = fake.list_pane_sizes(pane_id).expect("the pane was spawned");
    let initial_pty_size = resizes[0];
    assert_eq!(
        resizes,
        vec![
            initial_pty_size,
            PtySize {
                column_count: initial_pty_size.column_count
                    - (TEST_VIEWPORT_SIZE.column_count - NARROW_VIEWPORT_SIZE.column_count),
                row_count: initial_pty_size.row_count,
            },
            initial_pty_size,
        ],
    );
}

#[test]
fn dropping_a_smaller_client_connection_grows_the_tabs_pty_back() {
    // Half the columns of [`TEST_VIEWPORT_SIZE`], the size the seeded session was
    // bootstrapped at.
    const NARROW_VIEWPORT_SIZE: Size = Size {
        column_count: 40,
        row_count: 24,
    };

    let (_server, fake, pane_id) =
        serve_test_session("drop-reflow", |runtime_directory, session_id, _fake| {
            // The narrow client attaches first and holds the tab down: the
            // effective tab size is the smallest viewport of every client viewing
            // it, so the pane's PTY shrinks.
            let mut narrow = open_session_connection(&runtime_directory, session_id);
            let (_narrow_client, _, structure, _) =
                attach_test_client_with_viewport(&mut narrow, 2, NARROW_VIEWPORT_SIZE);
            let pane_id = structure.panes[0].pane_id;

            let mut wide = open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport(&mut wide, 3, TEST_VIEWPORT_SIZE);

            let mut caller = open_session_connection(&runtime_directory, session_id);
            assert_eq!(attached_client_count(&mut caller, 4), 2);

            // Drop the narrow client's connection without sending Command::Detach.
            // The record goes when the serving thread notices the connection ended,
            // and the tab's PTY reflows immediately to the full width.
            drop(narrow);

            wait_for_client_count(&mut caller, 1, 5);

            (vec![caller, wide], pane_id)
        });

    // Three resizes, in this order: the size the seeded session gave the pane,
    // down by the difference in viewport width when the narrow client joined,
    // and back to the first size when the narrow client's connection dropped.
    let resizes = fake.list_pane_sizes(pane_id).expect("the pane was spawned");
    let initial_pty_size = resizes[0];
    assert_eq!(
        resizes,
        vec![
            initial_pty_size,
            PtySize {
                column_count: initial_pty_size.column_count
                    - (TEST_VIEWPORT_SIZE.column_count - NARROW_VIEWPORT_SIZE.column_count),
                row_count: initial_pty_size.row_count,
            },
            initial_pty_size,
        ],
        "connection drop triggers immediate PTY reconciliation"
    );
}

#[test]
fn an_attached_client_types_into_its_pane_and_resizes_the_tab_it_views() {
    // Smaller than [`TEST_VIEWPORT_SIZE`] on both axes, so this one client's report is
    // the smallest of every client viewing the tab and the tab follows it.
    const RESIZED_VIEWPORT_SIZE: Size = Size {
        column_count: 60,
        row_count: 20,
    };

    // `<C-a>` reaches the pane as the ASCII SOH byte.
    const TYPED_KEY_CHORD: KeyChord = KeyChord::from_parts(ModFlags::CTRL, Key::Char('a'));
    const TYPED_KEY_BYTES: &[u8] = &[0x01];

    let (_server, fake, pane_id) = serve_test_session(
        "types-and-resizes",
        |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, replied_session, structure, _) = attach_test_client(&mut viewer, 2);
            assert_eq!(replied_session, session_id);
            let tab_id = structure.tabs[0].tab_id;
            let pane_id = structure.panes[0].pane_id;

            // The attach records no focused pane, so the pane this client types
            // into is named over a second connection, which never attaches.
            let mut caller = open_session_connection(&runtime_directory, session_id);
            submit_test_command(
                &mut caller,
                session_id,
                Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(pane_id),
                    client_id: Some(client_id),
                }),
                3,
            );

            // The first frame the session composes for this client, drawn at the
            // size the attach reported.
            let (mut viewer, frames) = read_session_frames_until(viewer, |frame| {
                matches!(frame, SessionEvent::Painted { .. })
            });
            let painted = get_last_painted_frame(&frames);
            assert_eq!(painted.client_snapshot.client_id, client_id);
            assert_eq!(painted.session_snapshot.session_id, session_id);
            assert_eq!(painted.client_snapshot.viewport_size, TEST_VIEWPORT_SIZE);
            assert_eq!(painted.client_snapshot.active_tab_id, tab_id);
            assert_eq!(painted.session_snapshot.active_tab_snapshot.tab_id, tab_id);
            assert_eq!(painted.client_snapshot.focused_pane_id, Some(pane_id));

            // Both requests travel up this one connection, so the dispatcher reads
            // them in this order: the press has reached the pane by the time the
            // resized frame is composed.
            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Keyboard {
                        key_input: build_key_input_for_chord(TYPED_KEY_CHORD),
                    },
                })
                .expect("send key press");
            viewer
                .send(&IpcRequest {
                    request_id: 5,
                    request_kind: IpcRequestKind::Resize {
                        viewport: RESIZED_VIEWPORT_SIZE,
                        pane_area: None,
                        cell_size: None,
                    },
                })
                .expect("send resize");

            let (viewer, frames) = read_session_frames_until(
                viewer,
                |frame| matches!(frame, SessionEvent::Painted { frame } if frame.client_snapshot.viewport_size == RESIZED_VIEWPORT_SIZE),
            );
            // Nothing between the two frames drew at a third size.
            for earlier in &frames[..frames.len() - 1] {
                if let SessionEvent::Painted { frame } = earlier {
                    assert_eq!(frame.client_snapshot.viewport_size, TEST_VIEWPORT_SIZE);
                }
            }
            let painted = get_last_painted_frame(&frames);
            assert_eq!(painted.client_snapshot.client_id, client_id);
            assert_eq!(painted.client_snapshot.viewport_size, RESIZED_VIEWPORT_SIZE);
            assert_eq!(
                painted
                    .session_snapshot
                    .active_tab_snapshot
                    .effective_cell_size,
                pane_viewport(RESIZED_VIEWPORT_SIZE),
            );

            // The stream still ends with the detached frame once the client is detached.
            submit_test_command(
                &mut caller,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(client_id),
                }),
                6,
            );
            let (viewer, frames) = read_session_frames_to_detached(viewer);
            assert_eq!(frames.last(), Some(&SessionEvent::Detached));
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| **frame == SessionEvent::Detached)
                    .count(),
                1,
            );

            (vec![caller, viewer], pane_id)
        },
    );

    // The press is the only thing written to the pane, and it arrived encoded.
    assert_eq!(
        fake.list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![TYPED_KEY_BYTES.to_vec()],
    );
}

#[test]
fn an_accepted_pane_swap_delivers_its_frame_without_a_resize() {
    let (_session_server, _fake_pty_backend, ()) = serve_test_session(
        "pane-swap-frame",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) = attach_test_client(&mut viewer, 2);
            let target_pane_id = attached_session_structure.panes[0].pane_id;
            let mut observer = open_session_connection(&runtime_directory, session_id);
            let (observer_client_id, _, _, _) = attach_test_client(&mut observer, 2);
            let mut caller = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source_pane_id: Some(target_pane_id),
                    tab_id: None,
                    direction: Direction::Right,
                    should_stack: false,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: Some(client_id),
                }),
                3,
            );
            let source_pane_id = emitted_events
                .iter()
                .find_map(|event| match event {
                    Event::PaneCreated(created_pane) => Some(created_pane.pane_id),
                    _ => None,
                })
                .expect("the split creates the source pane");

            let (mut viewer, initial_frames) =
                read_session_frames_until(viewer, move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.client_snapshot.client_id == client_id
                                && frame.session_snapshot.active_tab_snapshot.pane_slots.len() == 2
                    )
                });
            let (observer, observer_initial_frames) =
                read_session_frames_until(observer, move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.client_snapshot.client_id == observer_client_id
                                && frame.session_snapshot.active_tab_snapshot.pane_slots.len() == 2
                    )
                });
            let initial_frame = get_last_painted_frame(&initial_frames);
            let active_tab_id = initial_frame.client_snapshot.active_tab_id;
            let source_rect_before = initial_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == source_pane_id)
                .expect("the source pane has a slot before the swap")
                .outer_rect;
            let target_rect_before = initial_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == target_pane_id)
                .expect("the target pane has a slot before the swap")
                .outer_rect;
            let session_revision_before = initial_frame.session_snapshot.session_revision;
            let client_revision_before = initial_frame.client_snapshot.client_revision;
            let target_pane_resize_history = fake_pty_backend
                .list_pane_sizes(target_pane_id)
                .expect("the target PTY was spawned");
            let source_pane_resize_history = fake_pty_backend
                .list_pane_sizes(source_pane_id)
                .expect("the source PTY was spawned");
            let placement_command_id = CommandId::new();

            viewer
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SubmitCommand(Box::new(
                        CommandEnvelope::from_parts(
                            placement_command_id,
                            CommandSource::from_key_binding(client_id),
                            SystemTime::UNIX_EPOCH,
                            Command::PlacePane(PlacePaneArgs {
                                source_pane_id,
                                placement_target: PanePlacementTarget::Swap { target_pane_id },
                                expected_placement_revision: Some(PlacementRevision {
                                    session_revision: session_revision_before,
                                    client_revision: client_revision_before,
                                }),
                            }),
                        ),
                    )),
                })
                .expect("send the checked pane swap");

            let (viewer, committed_frames) = read_session_frames_until(
                viewer,
                move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.session_snapshot.session_revision == session_revision_before + 1
                    )
                },
            );
            let (observer, observer_committed_frames) = read_session_frames_until(
                observer,
                move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.client_snapshot.client_id == observer_client_id
                                && frame.session_snapshot.session_revision == session_revision_before + 1
                    )
                },
            );
            let committed_frame = get_last_painted_frame(&committed_frames);
            let expected_commit_event = SessionEvent::PanePlacementCommitted {
                command_id: placement_command_id,
                source_pane_id,
                source_tab_id: active_tab_id,
                destination_tab_id: active_tab_id,
                placement_target: PanePlacementTarget::Swap { target_pane_id },
            };
            for committed_client_events in [&committed_frames, &observer_committed_frames] {
                let commit_event_index = committed_client_events
                    .iter()
                    .position(|session_event| session_event == &expected_commit_event)
                    .expect("the server broadcasts the exact placement commit to each viewer");
                let committed_painted_frame_index = committed_client_events
                    .iter()
                    .position(|session_event| {
                        matches!(
                            session_event,
                            SessionEvent::Painted { frame }
                                if frame.session_snapshot.session_revision == session_revision_before + 1
                        )
                    })
                    .expect("each viewer receives the committed frame");
                assert!(
                    commit_event_index < committed_painted_frame_index,
                    "the placement identity arrives before its authoritative frame"
                );
            }
            assert_eq!(
                observer_initial_frames
                    .last()
                    .map(SessionEvent::get_event_name),
                Some("Painted"),
                "the observer starts from the same committed layout"
            );
            let source_rect_after = committed_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == source_pane_id)
                .expect("the source pane remains in the committed frame")
                .outer_rect;
            let target_rect_after = committed_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == target_pane_id)
                .expect("the target pane remains in the committed frame")
                .outer_rect;
            assert_eq!(
                committed_frame.client_snapshot.viewport_size,
                TEST_VIEWPORT_SIZE
            );
            assert_eq!(
                committed_frame.session_snapshot.session_revision,
                session_revision_before + 1
            );
            assert_eq!(source_rect_after, target_rect_before);
            assert_eq!(target_rect_after, source_rect_before);

            assert_eq!(
                fake_pty_backend
                    .list_pane_sizes(target_pane_id)
                    .expect("the target PTY remains available"),
                target_pane_resize_history,
                "the swap does not resize the target PTY"
            );
            assert_eq!(
                fake_pty_backend
                    .list_pane_sizes(source_pane_id)
                    .expect("the source PTY remains available"),
                source_pane_resize_history,
                "the swap does not resize the source PTY"
            );

            (vec![caller, viewer, observer], ())
        },
    );
}

#[test]
fn a_pane_swap_confirmed_before_another_viewers_new_pane_is_rejected_to_its_viewer() {
    let (_session_server, _fake_pty_backend, ()) = serve_test_session(
        "pane-swap-stale",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) = attach_test_client(&mut viewer, 2);
            let target_pane_id = attached_session_structure.panes[0].pane_id;
            let mut other_viewer = open_session_connection(&runtime_directory, session_id);
            let (other_client_id, _, _, _) = attach_test_client(&mut other_viewer, 2);
            let mut caller = open_session_connection(&runtime_directory, session_id);
            let build_new_pane_command = |client_id: ClientId| {
                Command::NewPane(NewPaneArgs {
                    source_pane_id: Some(target_pane_id),
                    tab_id: None,
                    direction: Direction::Right,
                    should_stack: false,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: Some(client_id),
                })
            };
            let emitted_events = submit_test_command(
                &mut caller,
                session_id,
                build_new_pane_command(client_id),
                3,
            );
            let source_pane_id = emitted_events
                .iter()
                .find_map(|event| match event {
                    Event::PaneCreated(created_pane) => Some(created_pane.pane_id),
                    _ => None,
                })
                .expect("the split creates the source pane");
            let (viewer, initial_frames) =
                read_session_frames_until(viewer, move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.client_snapshot.client_id == client_id
                                && frame.session_snapshot.active_tab_snapshot.pane_slots.len() == 2
                    )
                });
            let initial_frame = get_last_painted_frame(&initial_frames);
            let session_revision_before = initial_frame.session_snapshot.session_revision;
            let client_revision_before = initial_frame.client_snapshot.client_revision;

            submit_test_command(
                &mut caller,
                session_id,
                build_new_pane_command(other_client_id),
                4,
            );
            let (mut viewer, _) = read_session_frames_until(viewer, move |session_event| {
                matches!(
                    session_event,
                    SessionEvent::Painted { frame }
                        if frame.session_snapshot.session_revision > session_revision_before
                )
            });
            let placement_command_id = CommandId::new();
            viewer
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SubmitCommand(Box::new(
                        CommandEnvelope::from_parts(
                            placement_command_id,
                            CommandSource::from_key_binding(client_id),
                            SystemTime::UNIX_EPOCH,
                            Command::PlacePane(PlacePaneArgs {
                                source_pane_id,
                                placement_target: PanePlacementTarget::Swap { target_pane_id },
                                expected_placement_revision: Some(PlacementRevision {
                                    session_revision: session_revision_before,
                                    client_revision: client_revision_before,
                                }),
                            }),
                        ),
                    )),
                })
                .expect("send the pane swap built on the earlier layout");

            let expected_rejection = SessionEvent::PlacementCommandRejected {
                command_id: placement_command_id,
            };
            let (viewer, answer_events) = read_session_frames_until(viewer, move |session_event| {
                *session_event == expected_rejection
            });
            assert!(
                !answer_events.iter().any(|session_event| matches!(
                    session_event,
                    SessionEvent::PanePlacementCommitted { command_id, .. }
                        if *command_id == placement_command_id
                )),
                "the session commits nothing for the stale swap"
            );

            (vec![caller, viewer, other_viewer], ())
        },
    );
}

#[test]
fn an_accepted_pane_insertion_delivers_its_frame_without_a_resize() {
    let (_session_server, _fake_pty_backend, ()) = serve_test_session(
        "place-frame",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, attached_session_structure, _) = attach_test_client(&mut viewer, 2);
            let target_pane_id = attached_session_structure.panes[0].pane_id;
            let mut caller = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source_pane_id: Some(target_pane_id),
                    tab_id: None,
                    direction: Direction::Right,
                    should_stack: false,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: Some(client_id),
                }),
                3,
            );
            let source_pane_id = emitted_events
                .iter()
                .find_map(|event| match event {
                    Event::PaneCreated(created_pane) => Some(created_pane.pane_id),
                    _ => None,
                })
                .expect("the split creates the source pane");

            let (mut viewer, initial_frames) =
                read_session_frames_until(viewer, move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.client_snapshot.client_id == client_id
                                && frame.session_snapshot.active_tab_snapshot.pane_slots.len() == 2
                    )
                });
            let initial_frame = get_last_painted_frame(&initial_frames);
            let active_tab_id = initial_frame.session_snapshot.active_tab_snapshot.tab_id;
            let target_rect_before = initial_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == target_pane_id)
                .expect("the target pane has a slot before insertion")
                .outer_rect;
            let session_revision_before = initial_frame.session_snapshot.session_revision;
            let client_revision_before = initial_frame.client_snapshot.client_revision;

            viewer
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SubmitCommand(Box::new(
                        CommandEnvelope::from_parts(
                            CommandId::new(),
                            CommandSource::from_key_binding(client_id),
                            SystemTime::UNIX_EPOCH,
                            Command::PlacePane(PlacePaneArgs {
                                source_pane_id,
                                placement_target: PanePlacementTarget::Split {
                                    destination_tab_id: active_tab_id,
                                    anchor: PanePlacementAnchor::Pane(target_pane_id),
                                    direction: Direction::Down,
                                },
                                expected_placement_revision: Some(PlacementRevision {
                                    session_revision: session_revision_before,
                                    client_revision: client_revision_before,
                                }),
                            }),
                        ),
                    )),
                })
                .expect("send the checked pane insertion");

            let (viewer, committed_frames) = read_session_frames_until(
                viewer,
                move |session_event| {
                    matches!(
                        session_event,
                        SessionEvent::Painted { frame }
                            if frame.session_snapshot.session_revision == session_revision_before + 1
                    )
                },
            );
            let committed_frame = get_last_painted_frame(&committed_frames);
            let source_rect_after = committed_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == source_pane_id)
                .expect("the source pane remains in the committed frame")
                .outer_rect;
            let target_rect_after = committed_frame
                .session_snapshot
                .active_tab_snapshot
                .pane_slots
                .iter()
                .find(|pane_slot| pane_slot.pane_id == target_pane_id)
                .expect("the target pane remains in the committed frame")
                .outer_rect;
            assert_eq!(
                committed_frame.client_snapshot.viewport_size,
                TEST_VIEWPORT_SIZE
            );
            assert_eq!(
                committed_frame.session_snapshot.session_revision,
                session_revision_before + 1
            );
            let target_row_count_after = target_rect_before.cell_size.row_count / 2;
            let source_row_count_after =
                target_rect_before.cell_size.row_count - target_row_count_after;
            assert_eq!(target_rect_after.origin, target_rect_before.origin);
            assert_eq!(
                target_rect_after.cell_size,
                Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: target_row_count_after,
                }
            );
            assert_eq!(
                source_rect_after.origin,
                Point {
                    column: target_rect_before.origin.column,
                    row: target_rect_before.origin.row + target_row_count_after,
                }
            );
            assert_eq!(
                source_rect_after.cell_size,
                Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: source_row_count_after,
                }
            );

            (vec![caller, viewer], ())
        },
    );
}

/// [`read_session_frames_until`] stopping at the answer to mouse round `request_id`.
fn read_to_mouse_answer(
    connection: Connection,
    request_id: u64,
) -> (Connection, Vec<SessionEvent>) {
    read_session_frames_until(connection, move |frame| match frame {
        SessionEvent::MouseAnswer {
            request_id: answered,
            mouse_answers: _,
        } => *answered == request_id,
        _ => false,
    })
}

/// Every mouse-round answer in `frames`, in the order they arrived: the round
/// each one answers, and what that answer carried.
fn list_mouse_answers(frames: &[SessionEvent]) -> Vec<(u64, Vec<MouseAnswer>)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            SessionEvent::MouseAnswer {
                request_id,
                mouse_answers,
            } => Some((*request_id, mouse_answers.clone())),
            _ => None,
        })
        .collect()
}

/// Turn normal mouse tracking with SGR encoding on in `pane`, the way the
/// program running there does, and read `connection`'s stream until a painted
/// frame shows the pane asking for reports — the point from which a forwarded
/// event is written to it.
fn wait_for_mouse_tracking(
    fake: &FakePtyBackend,
    pane: PaneId,
    connection: Connection,
) -> Connection {
    fake.push_output(pane, b"\x1b[?1000h\x1b[?1006h".to_vec())
        .expect("the pane was spawned");
    let (connection, _) = read_session_frames_until(connection, move |frame| match frame {
        SessionEvent::Painted { frame } => frame.pane_snapshots.iter().any(|pane_snapshot| {
            pane_snapshot.pane_id == pane && pane_snapshot.mouse_tracking == MouseTracking::Normal
        }),
        _ => false,
    });
    connection
}

/// Print enough lines in `pane` to push at least `retained` of them into its
/// scrollback, and read `connection`'s stream until a painted frame holds them.
///
/// Returns the line that frame shows on the pane's top row, which is the line a
/// scroll up from here counts back from.
fn fill_pane_scrollback(
    fake: &FakePtyBackend,
    pane: PaneId,
    retained: usize,
    connection: Connection,
) -> (Connection, u64) {
    let lines = retained + usize::from(TEST_VIEWPORT_SIZE.row_count);
    fake.push_output(pane, b"x\r\n".repeat(lines))
        .expect("the pane was spawned");
    let (connection, frames) = read_session_frames_until(connection, move |frame| match frame {
        SessionEvent::Painted { frame } => frame.pane_snapshots.iter().any(|pane_snapshot| {
            pane_snapshot.pane_id == pane
                && pane_snapshot.scrollback_meta.retained_line_count >= retained
        }),
        _ => false,
    });
    let top_row = get_last_painted_frame(&frames)
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane)
        .expect("the pane this client views")
        .view_top_row_index;
    (connection, top_row)
}

/// One left press with nothing held, at the client cell `at`.
fn build_left_mouse_press(position: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position,
        modifier_flags: ModFlags::NONE,
    }
}

/// A cell above and left of any pane's content, so it clamps into the pane's
/// top-left content cell whatever the chrome around it measures.
const MOUSE_PRESS_POSITION: Point = Point { column: 0, row: 0 };

/// The report the program in the pane reads for [`MOUSE_PRESS_POSITION`]: a left press at
/// the pane's own column 1, row 1, in the SGR form the pane asked for.
const MOUSE_REPORT_BYTES: &[u8] = b"\x1b[<0;1;1M";

#[test]
fn an_attached_client_forwards_a_mouse_press_into_its_pane() {
    let (_server, fake, pane_id) =
        serve_test_session("mouse-forward", |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (_, replied_session, structure, _) = attach_test_client(&mut viewer, 2);
            assert_eq!(replied_session, session_id);
            let pane_id = structure.panes[0].pane_id;

            // The program in the pane asks for mouse reports. Until it has, a
            // forwarded event is written nowhere.
            let mut viewer = wait_for_mouse_tracking(&fake, pane_id, viewer);
            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                        pane_id,
                        mouse_input: build_left_mouse_press(MOUSE_PRESS_POSITION),
                    }]),
                })
                .expect("send mouse round");

            // The answer says the round ran, so the write it carried has happened.
            let (viewer, _) = read_to_mouse_answer(viewer, 4);

            (vec![viewer], pane_id)
        });

    // The report is the only thing written to the pane, and it arrived encoded.
    assert_eq!(
        fake.list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![MOUSE_REPORT_BYTES.to_vec()],
    );
}

#[test]
fn the_answer_to_a_round_that_reports_nothing_still_reaches_the_viewer() {
    // The viewer holds every following round back until the round in flight is
    // answered, so without this frame it sends no mouse round again.
    let (_server, _fake, ()) =
        serve_test_session("mouse-answer", |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (_, _, structure, _) = attach_test_client(&mut viewer, 2);
            let pane_id = structure.panes[0].pane_id;

            let mut viewer = wait_for_mouse_tracking(&fake, pane_id, viewer);
            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                        pane_id,
                        mouse_input: build_left_mouse_press(MOUSE_PRESS_POSITION),
                    }]),
                })
                .expect("send mouse round");

            // A forward has nothing to report, and the round is answered anyway.
            let (viewer, frames) = read_to_mouse_answer(viewer, 4);
            assert_eq!(list_mouse_answers(&frames), vec![(4, Vec::new())]);

            (vec![viewer], ())
        });
}

#[test]
fn a_scroll_round_answers_with_the_pane_and_the_line_its_view_landed_on() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const SCROLL_LINE_COUNT: usize = 5;

    let (_server, _fake, ()) =
        serve_test_session("mouse-scroll", |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (_, _, structure, _) = attach_test_client(&mut viewer, 2);
            let pane_id = structure.panes[0].pane_id;

            let (mut viewer, top_row) =
                fill_pane_scrollback(&fake, pane_id, RETAINED_SCROLLBACK_LINE_COUNT, viewer);
            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                        pane_id,
                        is_scrolling_up: true,
                        scroll_line_count: SCROLL_LINE_COUNT,
                    }]),
                })
                .expect("send mouse round");

            // The view landed five lines above the line it was showing.
            let (viewer, frames) = read_to_mouse_answer(viewer, 4);
            assert_eq!(
                list_mouse_answers(&frames),
                vec![(
                    4,
                    vec![MouseAnswer::Scrolled {
                        pane_id,
                        top_row_number: Some(top_row - SCROLL_LINE_COUNT as u64),
                    }],
                )],
            );

            (vec![viewer], ())
        });
}

#[test]
fn a_border_move_round_answers_with_the_cells_the_wall_left_it() {
    // Far more cells than the neighbour pane can give: the tab is 80 columns
    // wide, the split leaves the neighbour 40 of them, and a pane's box holds a
    // 2-column content minimum inside a 1-cell border, so it stops at 4 columns
    // and the border takes the 36 cells above that.
    const REQUESTED_CELL_COUNT: u16 = 200;
    const APPLIED_CELL_COUNT: u16 = 36;

    let (_server, _fake, ()) =
        serve_test_session("mouse-border", |runtime_directory, session_id, _fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, structure, _) = attach_test_client(&mut viewer, 2);
            let pane_id = structure.panes[0].pane_id;

            // The neighbour whose room the border move eats into, split off the
            // client's own pane over a second connection, which never attaches.
            let mut caller = open_session_connection(&runtime_directory, session_id);
            submit_test_command(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source_pane_id: Some(pane_id),
                    tab_id: None,
                    direction: Direction::Right,
                    should_stack: false,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: Some(client_id),
                }),
                3,
            );

            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                        pane_id,
                        border_side: Direction::Right,
                        resize_step: 1,
                        requested_cell_count: REQUESTED_CELL_COUNT,
                    }]),
                })
                .expect("send mouse round");

            let (viewer, frames) = read_to_mouse_answer(viewer, 4);
            assert_eq!(
                list_mouse_answers(&frames),
                vec![(
                    4,
                    vec![MouseAnswer::Resized {
                        pane_id,
                        border_side: Direction::Right,
                        resize_step: 1,
                        applied_cell_count: APPLIED_CELL_COUNT,
                    }]
                )],
            );

            (vec![caller, viewer], ())
        });
}

#[test]
fn one_round_runs_every_action_it_holds_and_is_answered_once() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const SCROLL_LINE_COUNT: usize = 5;

    let (server, fake, (session_id, client_id, tab_id, pane_id)) =
        serve_test_session("mouse-round", |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, structure, _) = attach_test_client(&mut viewer, 2);
            let tab_id = structure.tabs[0].tab_id;
            let pane_id = structure.panes[0].pane_id;

            // Splitting the pane this client attached on moves its focus to the
            // new pane, so the pane the round asks for is not the one already
            // focused. The split is made over a second connection, which never
            // attaches.
            let mut caller = open_session_connection(&runtime_directory, session_id);
            submit_test_command(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source_pane_id: Some(pane_id),
                    tab_id: None,
                    direction: Direction::Right,
                    should_stack: false,
                    working_directory: None,
                    spawn_spec: None,
                    client_id: Some(client_id),
                }),
                3,
            );

            let viewer = wait_for_mouse_tracking(&fake, pane_id, viewer);
            let (mut viewer, top_row) =
                fill_pane_scrollback(&fake, pane_id, RETAINED_SCROLLBACK_LINE_COUNT, viewer);

            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::Mouse(vec![
                        WireMouseAction::Command(Box::new(Command::FocusPane(FocusPaneArgs {
                            focus_target: FocusTarget::Pane(pane_id),
                            client_id: Some(client_id),
                        }))),
                        WireMouseAction::Scroll {
                            pane_id,
                            is_scrolling_up: true,
                            scroll_line_count: SCROLL_LINE_COUNT,
                        },
                        WireMouseAction::Forward {
                            pane_id,
                            mouse_input: build_left_mouse_press(MOUSE_PRESS_POSITION),
                        },
                    ]),
                })
                .expect("send mouse round");
            let (mut viewer, mut frames) = read_to_mouse_answer(viewer, 4);

            // A second round, sent once the first was answered. Its own answer
            // is the frame after which no further answer to the first round can
            // still be in flight.
            viewer
                .send(&IpcRequest {
                    request_id: 5,
                    request_kind: IpcRequestKind::Mouse(Vec::new()),
                })
                .expect("send empty mouse round");
            let (viewer, rest) = read_to_mouse_answer(viewer, 5);
            frames.extend(rest);

            // One answer for the round, holding the scroll alone: the focus
            // command and the forward each report nothing.
            assert_eq!(
                list_mouse_answers(&frames),
                vec![
                    (
                        4,
                        vec![MouseAnswer::Scrolled {
                            pane_id,
                            top_row_number: Some(top_row - SCROLL_LINE_COUNT as u64),
                        }],
                    ),
                    (5, Vec::new()),
                ],
            );

            (
                vec![caller, viewer],
                (session_id, client_id, tab_id, pane_id),
            )
        });

    // The session applied the command the round carried: the focus the split
    // had moved away is back on the round's pane.
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the viewing client");
    assert_eq!(client.get_focused_pane(tab_id), Some(pane_id));

    // And the forward the round carried reached the pane, encoded, once.
    assert_eq!(
        fake.list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![MOUSE_REPORT_BYTES.to_vec()],
    );
}

#[test]
fn two_rounds_sent_back_to_back_are_answered_in_the_order_they_were_sent() {
    // Lines of history to print, then how far up each round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const FIRST_SCROLL_LINE_COUNT: usize = 2;
    const SECOND_SCROLL_LINE_COUNT: usize = 3;

    let (_server, _fake, ()) =
        serve_test_session("mouse-order", |runtime_directory, session_id, fake| {
            let mut viewer = open_session_connection(&runtime_directory, session_id);
            let (_, _, structure, _) = attach_test_client(&mut viewer, 2);
            let pane_id = structure.panes[0].pane_id;

            let (mut viewer, top_row) =
                fill_pane_scrollback(&fake, pane_id, RETAINED_SCROLLBACK_LINE_COUNT, viewer);
            for (request_id, lines) in [(4, FIRST_SCROLL_LINE_COUNT), (5, SECOND_SCROLL_LINE_COUNT)]
            {
                viewer
                    .send(&IpcRequest {
                        request_id,
                        request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                            pane_id,
                            is_scrolling_up: true,
                            scroll_line_count: lines,
                        }]),
                    })
                    .expect("send mouse round");
            }

            // Both rounds moved the same view, so the lines they answer with name
            // the order they ran in: two lines up, then three more.
            let (viewer, frames) = read_to_mouse_answer(viewer, 5);
            assert_eq!(
                list_mouse_answers(&frames),
                vec![
                    (
                        4,
                        vec![MouseAnswer::Scrolled {
                            pane_id,
                            top_row_number: Some(top_row - FIRST_SCROLL_LINE_COUNT as u64),
                        }],
                    ),
                    (
                        5,
                        vec![MouseAnswer::Scrolled {
                            pane_id,
                            top_row_number: Some(
                                top_row
                                    - (FIRST_SCROLL_LINE_COUNT + SECOND_SCROLL_LINE_COUNT) as u64
                            ),
                        }],
                    ),
                ],
            );

            (vec![viewer], ())
        });
}

/// Submit a command over `connection` — an external `koshi` invocation naming
/// `session_id` and targeting `client_id` — and return the events it emitted.
/// Panics unless the session applied it.
fn submit_for_client(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    command: Command,
    request_id: u64,
) -> Vec<Event> {
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        SystemTime::UNIX_EPOCH,
        command,
    );
    connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("send command");
    let ipc_response: IpcResponse = connection.recv().expect("command reply");
    let IpcResult::CommandResult(CommandResult::Ok {
        command_id: _,
        emitted_events,
    }) = ipc_response.answer_result
    else {
        panic!(
            "expected the command to apply, got {:?}",
            ipc_response.answer_result
        );
    };
    emitted_events
}

#[test]
fn attaching_again_with_the_token_brings_back_the_tab_focus_zoom_and_scroll() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED_SCROLLBACK_LINE_COUNT: usize = 40;
    const SCROLL_LINE_COUNT: usize = 5;

    let (
        server,
        _fake,
        (
            session_id,
            detached_client_id,
            resumed_client_id,
            booted_tab_id,
            added_tab_id,
            root_pane_id,
        ),
    ) = serve_test_session("resume-view", |runtime_directory, session_id, fake| {
        let mut viewer = open_session_connection(&runtime_directory, session_id);
        let (detached_client_id, _, structure, resume_token) = attach_test_client(&mut viewer, 2);
        let booted_tab_id = structure.tabs[0].tab_id;
        let root_pane_id = structure.panes[0].pane_id;
        let resume_token = resume_token.expect("the attach minted a token");

        // Every command rides a connection that never attaches, so the
        // viewer's own stream carries frames alone.
        let mut caller = open_session_connection(&runtime_directory, session_id);

        // Scroll the pane five lines up, zoom it, then switch tabs.
        let (mut viewer, _top_row) =
            fill_pane_scrollback(&fake, root_pane_id, RETAINED_SCROLLBACK_LINE_COUNT, viewer);
        viewer
            .send(&IpcRequest {
                request_id: 4,
                request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                    pane_id: root_pane_id,
                    is_scrolling_up: true,
                    scroll_line_count: SCROLL_LINE_COUNT,
                }]),
            })
            .expect("send mouse round");
        let (viewer, _frames) = read_to_mouse_answer(viewer, 4);
        submit_for_client(
            &mut caller,
            session_id,
            detached_client_id,
            Command::TogglePaneFullscreen,
            5,
        );
        // Adding a tab moves the client that asked for it onto that tab.
        let added_tab_id = build_test_tab(&mut caller, session_id, 6);
        submit_for_client(
            &mut caller,
            session_id,
            detached_client_id,
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Id(added_tab_id),
                client_id: Some(detached_client_id),
            }),
            7,
        );

        // The link breaks. The session files the view under the token this
        // attach minted.
        drop(viewer);
        wait_for_client_count(&mut caller, 0, 8);

        let mut resumed_connection = open_session_connection(&runtime_directory, session_id);
        let (resumed_client_id, resumed_session_id, structure, _) =
            attach_test_client_with_resume_token(
                &mut resumed_connection,
                2,
                TEST_VIEWPORT_SIZE,
                Some(resume_token),
            );
        assert_eq!(resumed_session_id, session_id);
        assert_eq!(
            structure
                .tabs
                .iter()
                .map(|tab| tab.tab_id)
                .collect::<Vec<_>>(),
            vec![booted_tab_id, added_tab_id],
        );
        let LayoutNode::Pane(additional_pane_id) = structure.tabs[1].layout else {
            panic!("the added tab holds one pane, got {:?}", structure.tabs[1]);
        };
        let mut expected_pane_ids = vec![root_pane_id, additional_pane_id];
        expected_pane_ids.sort();
        assert_eq!(
            structure
                .panes
                .iter()
                .map(|pane| pane.pane_id)
                .collect::<Vec<_>>(),
            expected_pane_ids,
        );

        (
            vec![caller, resumed_connection],
            (
                session_id,
                detached_client_id,
                resumed_client_id,
                booted_tab_id,
                added_tab_id,
                root_pane_id,
            ),
        )
    });

    assert_ne!(
        resumed_client_id, detached_client_id,
        "the token attaches as a fresh client"
    );
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session running");
    let resumed_client = session
        .clients
        .get_client_by_id(resumed_client_id)
        .expect("the client the token attached");
    assert_eq!(resumed_client.get_active_tab(), added_tab_id);
    assert_eq!(
        resumed_client.get_focused_pane(booted_tab_id),
        Some(root_pane_id)
    );
    assert_eq!(
        resumed_client.get_layout_mode(booted_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(
        resumed_client.get_scroll_offset(root_pane_id),
        SCROLL_LINE_COUNT
    );
}
