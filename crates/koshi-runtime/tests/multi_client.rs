//! Integration tests for multiple clients attached to one session: what size
//! the tab they share gives its panes, how a client viewing another tab is
//! left out of that size, what the larger client is shown when the shared size
//! is smaller than its own terminal, whose lock mode a lock command changes,
//! which pane cell a mouse press names for the client that sent it, what a
//! client moving to another session leaves behind here, and when the last
//! client moving away closes the session it left.
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

use koshi_config::layer::PartialKoshiConfig;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, DetachArgs, FocusTabArgs, LockModeArgs,
    NewTabArgs, SwitchSessionArgs, TabTarget,
};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::{Event, InputModeChanged, PtyResized};
use koshi_core::geometry::{PaneArea, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, MouseTracking};
use koshi_core::process::PtySize;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{FrameSlot, PaintedFrame};
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, WireMouseAction,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_pane::pane::state::PaneKind;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

/// The terminal size the seeded session sizes its root pane against, before any
/// client attaches.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The PTY size [`TEST_VIEWPORT_SIZE`] gives the seeded session's single pane: one
/// tabline row and one hint row off the terminal, then a 1-cell pane border.
const SEEDED_PTY_SIZE: PtySize = PtySize {
    column_count: 78,
    row_count: 20,
};

/// The larger of the two viewports two clients share a tab at.
const LARGE_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 40,
};

/// Smaller than [`LARGE_VIEWPORT_SIZE`] on both axes.
const SMALL_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 30,
};

/// Narrower than [`SHORT_VIEWPORT_SIZE`], and taller than it.
const NARROW_VIEWPORT_SIZE: Size = Size {
    column_count: 70,
    row_count: 40,
};

/// Wider than [`NARROW_VIEWPORT_SIZE`], and shorter than it.
const SHORT_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 24,
};

/// The display name the seeded session carries.
const TEST_SESSION_NAME: &str = "workspace";

/// How long a test waits on work it cannot make happen itself — a detach the
/// serving thread has yet to notice, an event frame in flight — before failing.
const TEST_WAIT_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// Sends [`RuntimeEvent::Quit`] when the exchange thread ends, on the way out
/// of a failed assertion as well as a clean return.
struct StopDispatcher(Sender<RuntimeEvent>);

impl Drop for StopDispatcher {
    fn drop(&mut self) {
        let _ = self.0.send(RuntimeEvent::Quit);
    }
}

/// Seed a headless session under a fake PTY backend, serve its control socket
/// from a directory named for `session_label`, and run `run_exchange` against that socket
/// while this thread drains the runtime inbox and pushes each attached
/// client its frames.
///
/// Returns the server and the fake backend once the exchange is done, so a
/// test can read the session state and the PTY sizes the exchange left behind,
/// plus whatever the exchange itself produced. `run_exchange` receives the runtime
/// directory and the session id, the two facts it needs to find and open the
/// socket, and the fake backend.
///
/// It hands back the connections it wants left open alongside its own value:
/// a connection dropped while the dispatcher is still draining detaches its
/// client, so a test reading the registry keeps its connections here until the
/// dispatcher has stopped.
fn serve_test_session<T: Send + 'static>(
    session_label: &str,
    run_exchange: impl FnOnce(PathBuf, SessionId, Arc<FakePtyBackend>) -> (Vec<Connection>, T)
        + Send
        + 'static,
) -> (Server, Arc<FakePtyBackend>, T) {
    // A short base keeps the Unix socket path inside the OS path-length cap.
    #[cfg(unix)]
    let socket_path_base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_path_base = std::env::temp_dir();
    let runtime_directory = socket_path_base.join(format!(
        "koshi-multi-client-{}-{session_label}",
        std::process::id()
    ));

    let session_id = SessionId::new();
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let mut server = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );
    server
        .bootstrap_session(
            session_id,
            TEST_SESSION_NAME.to_string(),
            TEST_VIEWPORT_SIZE,
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("seed the session");
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        runtime_event_sender.clone(),
        None,
    )
    .expect("start serving");

    let exchange_runtime_directory = runtime_directory.clone();
    let exchange_fake_pty_backend = fake_pty_backend.clone();
    let exchange_thread = std::thread::spawn(move || {
        let _stop = StopDispatcher(runtime_event_sender);
        run_exchange(
            exchange_runtime_directory,
            session_id,
            exchange_fake_pty_backend,
        )
    });

    // The per-session server's own loop: block until an event is due, bounded
    // by the next render deadline, apply it, hand a fresh snapshot to any
    // subscriber that lost a critical event, then push every attached client
    // its frame when a render is due.
    loop {
        let current_time = Instant::now();
        let runtime_event = match server.next_render_wakeup(current_time) {
            Some(render_wakeup_timeout) => {
                match server.inbox_rx().recv_timeout(render_wakeup_timeout) {
                    Ok(runtime_event) => Some(runtime_event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match server.inbox_rx().recv() {
                Ok(runtime_event) => Some(runtime_event),
                Err(_) => break,
            },
        };
        if let Some(runtime_event) = runtime_event {
            if server.handle_runtime_event(runtime_event).is_break() {
                break;
            }
        }
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
    }

    let (open_connections, exchange_result) =
        exchange_thread.join().expect("the exchange finished");
    drop(open_connections);
    ipc_server.shutdown();
    let _ = std::fs::remove_dir_all(&runtime_directory);
    (server, fake_pty_backend, exchange_result)
}

/// Connect to the socket the endpoint file advertises and walk the Hello, so
/// the returned connection is open for every other request kind.
fn open_session_connection(runtime_directory: &Path, session_id: SessionId) -> Connection {
    let endpoint_file = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("endpoint file readable");
    let mut ipc_connection = Connection::connect(&endpoint_file.socket_address).expect("connect");
    ipc_connection
        .send(&IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                connection_token: endpoint_file.connection_token,
                is_remote: false,
            },
        })
        .expect("send hello");
    let hello_response: IpcResponse = ipc_connection.recv().expect("hello reply");
    assert_eq!(
        hello_response.answer_result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
    ipc_connection
}

/// Attach on `ipc_connection` reporting `viewport_size` and no pane area, and return
/// what the reply carried. The IPC connection carries only the client's event
/// stream afterwards.
fn attach_test_client_with_viewport(
    ipc_connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
) -> (ClientId, SessionId, AttachedSessionStructureSnapshot) {
    let (client_id, session_id, session_structure, _) =
        attach_test_client_with_pane_area(ipc_connection, request_id, viewport_size, None);
    (client_id, session_id, session_structure)
}

/// Attach on `ipc_connection` reporting `viewport_size` and `pane_area`, and return what
/// the reply carried, including the pane area the reply echoed.
fn attach_test_client_with_pane_area(
    ipc_connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
    pane_area: Option<PaneArea>,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<PaneArea>,
) {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Attach {
                viewport: viewport_size,
                event_filter: EventFilterSpec::All,
                resume_client_id: None,
                resume_token: None,
                pane_area,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let attach_response: IpcResponse = ipc_connection.recv().expect("attach reply");
    assert_eq!(attach_response.request_id, Some(request_id));
    let IpcResult::Attached {
        client_id,
        session_id,
        session_structure,
        pane_area,
        ..
    } = attach_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            attach_response.answer_result
        );
    };
    (client_id, session_id, session_structure, pane_area)
}

/// What the session reports about itself over `ipc_connection`.
fn get_session_overview(ipc_connection: &mut Connection, request_id: u64) -> SessionOverview {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let discovery_response: IpcResponse = ipc_connection.recv().expect("discovery reply");
    let IpcResult::Overview(session_overview) = discovery_response.answer_result else {
        panic!(
            "expected an overview, got {:?}",
            discovery_response.answer_result
        );
    };
    session_overview
}

/// How many clients the session reports over `ipc_connection`.
fn get_attached_client_count(ipc_connection: &mut Connection, request_id: u64) -> usize {
    get_session_overview(ipc_connection, request_id)
        .clients
        .len()
}

/// Ask over `ipc_connection` until the session reports `expected_client_count` clients, numbering
/// the requests from `request_id`. Panics once [`TEST_WAIT_TIMEOUT_DURATION`] has passed.
fn wait_for_client_count(
    ipc_connection: &mut Connection,
    expected_client_count: usize,
    request_id: u64,
) {
    let deadline = Instant::now() + TEST_WAIT_TIMEOUT_DURATION;
    let mut request_id = request_id;
    loop {
        let observed_client_count = get_attached_client_count(ipc_connection, request_id);
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

/// Submit a command over `ipc_connection` and return the events it emitted.
/// Panics unless the session applied it.
fn submit_test_command(
    ipc_connection: &mut Connection,
    session_id: SessionId,
    command: Command,
    request_id: u64,
) -> Vec<Event> {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
            target_client_id: None,
        },
        SystemTime::UNIX_EPOCH,
        command,
    );
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
        })
        .expect("send command");
    let command_response: IpcResponse = ipc_connection.recv().expect("command reply");
    let IpcResult::CommandResult(CommandResult::Ok {
        command_id: _,
        emitted_events,
    }) = command_response.answer_result
    else {
        panic!(
            "expected the command to apply, got {:?}",
            command_response.answer_result
        );
    };
    emitted_events
}

/// Read `ipc_connection`'s event stream until `accepts_session_event` accepts an event, on a thread
/// this one can give up waiting on. Returns every frame read, the accepted one
/// last, and the IPC connection so it stays open. Panics once [`TEST_WAIT_TIMEOUT_DURATION`] has
/// passed with no accepted frame.
fn read_session_frames_until(
    mut ipc_connection: Connection,
    accepts_session_event: impl Fn(&SessionEvent) -> bool + Send + 'static,
) -> (Connection, Vec<SessionEvent>) {
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut session_events = Vec::new();
        loop {
            let session_event: SessionEvent = ipc_connection.recv().expect("an event frame");
            let is_target_session_event = accepts_session_event(&session_event);
            session_events.push(session_event);
            if is_target_session_event {
                break;
            }
        }
        let _ = done_tx.send((ipc_connection, session_events));
    });
    done_rx
        .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
        .expect("the awaited frame reaches the viewer")
}

/// [`read_session_frames_until`] stopping at the goodbye frame.
fn read_session_frames_to_goodbye(ipc_connection: Connection) -> (Connection, Vec<SessionEvent>) {
    read_session_frames_until(ipc_connection, |session_event| {
        *session_event == SessionEvent::Detached
    })
}

/// The painted frame `session_events` ends with. Panics unless the last frame read is
/// a painted one.
fn get_last_painted_frame(session_events: &[SessionEvent]) -> &PaintedFrame {
    match session_events.last() {
        Some(SessionEvent::Painted {
            frame: painted_frame,
        }) => painted_frame,
        other_session_event => {
            panic!("expected the run to end with a painted frame, got {other_session_event:?}")
        }
    }
}

/// The tab and the pane the [`Command::NewTab`] in `emitted_events` created. Panics
/// unless `emitted_events` holds an [`Event::PaneCreated`].
fn get_created_tab_and_pane_ids(emitted_events: &[Event]) -> (TabId, PaneId) {
    emitted_events
        .iter()
        .find_map(|event| match event {
            Event::PaneCreated(payload) => Some((payload.tab_id, payload.pane_id)),
            _ => None,
        })
        .expect("the new tab reports its tab and its root pane")
}

#[test]
fn two_clients_on_one_tab_size_the_pty_to_the_per_axis_minimum() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "per-axis-minimum",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The large client alone: the tab is its own pane region, 100 columns by
            // 38 rows, and the pane's PTY is that minus its 1-cell border.
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure) = attach_test_client_with_viewport(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.panes[0].pane_id;

            // The small client joins the same tab. It is narrower and shorter, so
            // it takes both axes and the pane's PTY shrinks on both.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE);

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(get_attached_client_count(&mut caller_connection, 3), 2);

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    small_client_connection,
                ],
                pane_id,
            )
        },
    );

    // Three resizes, in this order: the size the seeded session gave the pane,
    // the large client's own region when it attached, and the per-axis minimum of
    // the two clients once the small one joined.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
        ],
    );
}

/// A client that reports a pane area smaller than its terminal sizes the tab
/// to that area.
#[test]
fn a_client_reporting_a_pane_area_sizes_the_pty_to_that_area() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "reported-pane-area",
        |runtime_directory, session_id, _fake_pty_backend| {
            let reported_pane_area = PaneArea::Reported(Size {
                column_count: 60,
                row_count: 20,
            });
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(reported_pane_area),
            );
            let pane_id = session_structure.panes[0].pane_id;
            assert_eq!(echoed_pane_area, Some(reported_pane_area));

            (vec![client_connection], pane_id)
        },
    );

    // The seeded size, then the reported 60x20 region minus the pane's 1-cell
    // border on each side.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 58,
                row_count: 18
            }
        ],
    );
}

/// A client with no room to draw a pane contributes no size, so the tab keeps
/// the size its other viewer gives it.
#[test]
fn a_starving_client_does_not_shrink_the_tab() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "starving-second",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, _) = attach_test_client_with_pane_area(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
            );
            let pane_id = session_structure.panes[0].pane_id;

            let mut starving_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, _, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut starving_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(PaneArea::Starving),
            );
            assert_eq!(echoed_pane_area, Some(PaneArea::Starving));

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller_connection, 2, 3);

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    starving_client_connection,
                ],
                pane_id,
            )
        },
    );

    // Two resizes only: the seeded size and the large client's own region. The
    // starving client moved nothing.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            }
        ],
    );
}

/// The starving client attaches first, so the tab has no viewer that sizes it
/// while the served loop renders. Nothing panics, that client's frames carry
/// every pane suppressed, and the seeded size stands until a sized client
/// arrives.
#[test]
fn a_starving_client_attaching_first_leaves_the_seeded_size_until_a_sized_client_arrives() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "starving-first",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut starving_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut starving_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(PaneArea::Starving),
            );
            let pane_id = session_structure.panes[0].pane_id;
            assert_eq!(echoed_pane_area, Some(PaneArea::Starving));

            // The served loop renders between the two attaches, with the tab's
            // only viewer contributing no pane area.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller_connection, 1, 3);

            // The tab has no viewer that sizes it, so it solves at 0x0 and the
            // starving client's own frames carry its pane suppressed.
            let (starving_client_connection, session_events) =
                read_session_frames_until(starving_client_connection, |session_event| {
                    matches!(session_event, SessionEvent::Painted { .. })
                });
            let painted_frame = get_last_painted_frame(&session_events);
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .effective_cell_size,
                Size {
                    column_count: 0,
                    row_count: 0
                },
            );
            assert!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .is_every_pane_suppressed
            );
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots,
                vec![FrameSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        cell_size: Size {
                            column_count: 0,
                            row_count: 0
                        },
                    },
                    content_rect: None,
                    pane_kind: PaneKind::Terminal,
                    is_visible: false,
                    is_suppressed: true,
                    is_dead: false,
                }],
            );

            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_pane_area(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
            );
            wait_for_client_count(&mut caller_connection, 2, 1000);

            (
                vec![
                    caller_connection,
                    starving_client_connection,
                    large_client_connection,
                ],
                pane_id,
            )
        },
    );

    // The seeded size stood while only the starving client viewed the tab,
    // then the large client's own region took over.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            }
        ],
    );
}

/// An attach that reports no pane area is echoed back as none.
#[test]
fn an_attach_without_a_pane_area_echoes_none() {
    let (_server, _fake_pty_backend, echoed_pane_area) = serve_test_session(
        "echoes-none",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, _, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
            );
            (vec![client_connection], echoed_pane_area)
        },
    );

    assert_eq!(echoed_pane_area, None);
}

#[test]
fn each_axis_takes_its_minimum_from_a_different_client_and_grows_back_when_that_client_leaves() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "mixed-axis-minimum",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The narrow client alone: 70 columns by 38 rows of pane region.
            let mut narrow_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure) = attach_test_client_with_viewport(
                &mut narrow_client_connection,
                2,
                NARROW_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.panes[0].pane_id;

            // The short client joins the same tab. It is wider but shorter, so the
            // columns stay pinned by the narrow client and the rows drop to this
            // one's: each axis takes its minimum from a different client.
            let mut short_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (short_client_id, _, _) = attach_test_client_with_viewport(
                &mut short_client_connection,
                2,
                SHORT_VIEWPORT_SIZE,
            );

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(get_attached_client_count(&mut caller_connection, 3), 2);

            // The short client leaves. The narrow one is the only viewer left, so
            // the rows grow back to its own region while the columns never move.
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(short_client_id),
                }),
                4,
            );
            assert_eq!(
                emitted_events,
                vec![Event::PtyResized(PtyResized {
                    pane_id,
                    pty_size: PtySize {
                        column_count: 68,
                        row_count: 36
                    },
                })],
            );

            let (short_client_connection, session_events) =
                read_session_frames_to_goodbye(short_client_connection);
            assert_eq!(session_events.last(), Some(&SessionEvent::Detached));
            wait_for_client_count(&mut caller_connection, 1, 5);

            (
                vec![
                    caller_connection,
                    narrow_client_connection,
                    short_client_connection,
                ],
                pane_id,
            )
        },
    );

    // Four resizes, in this order: the size the seeded session gave the pane,
    // the narrow client's own region, the mixed-axis minimum once the short
    // client joined, and the narrow client's region again once it left. The
    // columns are the narrow client's throughout.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 68,
                row_count: 36
            },
            PtySize {
                column_count: 68,
                row_count: 20
            },
            PtySize {
                column_count: 68,
                row_count: 36
            },
        ],
    );
}

#[test]
fn a_client_viewing_another_tab_never_constrains_this_tabs_size() {
    let (_server, fake_pty_backend, (first_pane_id, second_pane_id)) = serve_test_session(
        "per-tab-independence",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The large client attaches to the seeded tab.
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (large_client_id, _, session_structure) = attach_test_client_with_viewport(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let first_tab_id = session_structure.tabs[0].tab_id;
            let first_pane_id = session_structure.panes[0].pane_id;

            // A second tab, created for the large client, which moves onto it.
            // Its root pane spawns at the large client's own region.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::NewTab(NewTabArgs {
                    working_directory: None,
                    client_id: Some(large_client_id),
                }),
                3,
            );
            let (second_tab_id, second_pane_id) = get_created_tab_and_pane_ids(&emitted_events);

            // Send the large client back, so it is the first tab's only viewer
            // again and the second tab has none.
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(first_tab_id),
                    client_id: Some(large_client_id),
                }),
                4,
            );

            // The small client attaches. A fresh attach lands on the
            // lowest-indexed tab, which is the first one, so the two clients
            // share it and the small one takes both axes.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (small_client_id, _, _) = attach_test_client_with_viewport(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );
            assert_eq!(get_attached_client_count(&mut caller_connection, 5), 2);

            // The small client switches to the second tab. The first tab is the
            // large client's alone again; the second tab is the small client's.
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(second_tab_id),
                    client_id: Some(small_client_id),
                }),
                6,
            );

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    small_client_connection,
                ],
                (first_pane_id, second_pane_id),
            )
        },
    );

    // The first tab's pane: seeded, the large client's own region, down to the
    // shared minimum while the small client viewed it, and back to the large
    // client's region once the small one left for the other tab. The small
    // client viewing another tab adds nothing after that.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
            PtySize {
                column_count: 98,
                row_count: 36
            },
        ],
    );

    // The second tab's pane: spawned at the large client's region, then sized to
    // the small client's alone once that client switched onto it. The large
    // client viewing the first tab never bounds it.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(second_pane_id)
            .expect("the pane was spawned"),
        vec![
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
        ],
    );
}

#[test]
fn the_larger_client_sees_the_tab_letterboxed_at_the_shared_size() {
    // The per-axis minimum of [`LARGE_VIEWPORT_SIZE`] and [`SMALL_VIEWPORT_SIZE`], as a pane region.
    const SHARED_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 80,
        row_count: 28,
    };

    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "letterbox",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (large_client_id, _, session_structure) = attach_test_client_with_viewport(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.panes[0].pane_id;
            let tab_id = session_structure.tabs[0].tab_id;

            // The small client joins the same tab, which invalidates the layout, so
            // the large client is sent a fresh frame at the size the two now share.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE);

            let (large_client_connection, session_events) =
                read_session_frames_until(large_client_connection, move |session_event| {
                    match session_event {
                        SessionEvent::Painted {
                            frame: painted_frame,
                        } => {
                            painted_frame
                                .session_snapshot
                                .active_tab_snapshot
                                .effective_cell_size
                                == SHARED_PANE_VIEWPORT_SIZE
                        }
                        _ => false,
                    }
                });
            let painted_frame = get_last_painted_frame(&session_events);

            // The large client's own terminal is unchanged; the tab it draws is the
            // shared size, and the margin around it is the letterbox.
            assert_eq!(painted_frame.client_snapshot.client_id, large_client_id);
            assert_eq!(
                painted_frame.client_snapshot.viewport_size,
                LARGE_VIEWPORT_SIZE
            );
            assert_eq!(painted_frame.client_snapshot.active_tab_id, tab_id);
            assert_eq!(
                painted_frame.session_snapshot.active_tab_snapshot.tab_id,
                tab_id
            );
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .effective_cell_size,
                SHARED_PANE_VIEWPORT_SIZE
            );

            // The tab holds one pane, solved at origin (0, 0) over the shared size,
            // with its content inside a 1-cell border.
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots,
                vec![FrameSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        cell_size: SHARED_PANE_VIEWPORT_SIZE,
                    },
                    content_rect: Some(Rect {
                        origin: Point { column: 1, row: 1 },
                        cell_size: Size {
                            column_count: 78,
                            row_count: 26
                        },
                    }),
                    pane_kind: PaneKind::Terminal,
                    is_visible: true,
                    is_suppressed: false,
                    is_dead: false,
                }],
            );
            assert!(
                !painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .is_every_pane_suppressed
            );

            (vec![large_client_connection, small_client_connection], ())
        },
    );
}

#[test]
fn locking_one_client_leaves_the_other_clients_lock_state_unchanged() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "per-client-lock",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (large_client_id, _, _) = attach_test_client_with_viewport(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (small_client_id, _, _) = attach_test_client_with_viewport(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );

            // Lock the large client. Lock mode belongs to one client, so the command
            // reports a single change and it names that client alone.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SetLockMode(LockModeArgs {
                    is_locked: true,
                    client_id: Some(large_client_id),
                }),
                3,
            );
            assert_eq!(
                emitted_events,
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id: large_client_id,
                    lock_mode: LockMode::Locked,
                })],
            );

            // Lock the small client. Setting the mode a client already holds emits
            // nothing, so this event is what proves the small client was still
            // unlocked while the large one was locked.
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SetLockMode(LockModeArgs {
                    is_locked: true,
                    client_id: Some(small_client_id),
                }),
                4,
            );
            assert_eq!(
                emitted_events,
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id: small_client_id,
                    lock_mode: LockMode::Locked,
                })],
            );

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    small_client_connection,
                ],
                (),
            )
        },
    );
}

/// Setting the lock mode a client already holds applies and emits nothing;
/// setting the other mode emits the change.
#[test]
fn setting_the_lock_mode_a_client_already_holds_emits_nothing() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "repeat-lock",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let (client_id, _, _) =
                attach_test_client_with_viewport(&mut client_connection, 2, LARGE_VIEWPORT_SIZE);

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let build_lock_command = |is_locked| {
                Command::SetLockMode(LockModeArgs {
                    is_locked,
                    client_id: Some(client_id),
                })
            };

            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(true),
                    3,
                ),
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id,
                    lock_mode: LockMode::Locked,
                })],
            );
            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(true),
                    4,
                ),
                Vec::<Event>::new(),
            );
            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(false),
                    5,
                ),
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id,
                    lock_mode: LockMode::Normal,
                })],
            );
            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(false),
                    6,
                ),
                Vec::<Event>::new(),
            );

            (vec![caller_connection, client_connection], ())
        },
    );
}

/// Turn normal mouse tracking with SGR encoding on in `pane_id`, the way the
/// program running there does, and read `ipc_connection`'s stream until a painted
/// frame shows the pane asking for reports — the point from which a forwarded
/// event is written to it.
fn wait_for_mouse_tracking(
    fake_pty_backend: &FakePtyBackend,
    pane_id: PaneId,
    ipc_connection: Connection,
) -> Connection {
    fake_pty_backend
        .push_output(pane_id, b"\x1b[?1000h\x1b[?1006h".to_vec())
        .expect("the pane was spawned");
    let (ipc_connection, _) =
        read_session_frames_until(ipc_connection, move |session_event| match session_event {
            SessionEvent::Painted {
                frame: painted_frame,
            } => painted_frame.pane_snapshots.iter().any(|pane_snapshot| {
                pane_snapshot.pane_id == pane_id
                    && pane_snapshot.mouse_tracking == MouseTracking::Normal
            }),
            _ => false,
        });
    ipc_connection
}

/// Send one mouse round holding a single left press on `pane_id` at the client
/// cell `screen_point`, then read `ipc_connection`'s stream until that round
/// is answered.
fn send_mouse_press(
    mut ipc_connection: Connection,
    pane_id: PaneId,
    screen_point: Point,
    request_id: u64,
) -> Connection {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                pane_id,
                mouse_input: MouseInput {
                    mouse_kind: MouseKind::Press(MouseButton::Left),
                    position: screen_point,
                    modifier_flags: ModFlags::NONE,
                },
            }]),
        })
        .expect("send mouse round");
    let (ipc_connection, _) =
        read_session_frames_until(ipc_connection, move |session_event| match session_event {
            SessionEvent::MouseAnswer {
                request_id: answered_request_id,
                mouse_answers: _,
            } => *answered_request_id == request_id,
            _ => false,
        });
    ipc_connection
}

#[test]
fn a_mouse_click_is_answered_against_the_clicking_clients_own_view() {
    // The one cell both clients press, each in its own terminal. The tab they
    // share is 80 by 28: the small client's 80x30 terminal holds it at (0, 1),
    // putting the pane's content at (1, 2), and the large client's 100x40
    // terminal centers it at (10, 6), putting the pane's content at (11, 7).
    // So this cell is the pane's column 11, row 6 for the small client, and the
    // pane's column 1, row 1 for the large one.
    const SHARED_PANE_CELL_POSITION: Point = Point { column: 11, row: 7 };

    // A cell in the large client's own 100x40 terminal, past the right and bottom
    // edges of the pane's content there (columns 11 to 88, rows 7 to 32): the
    // letterbox margin around the shared tab. It is pulled to the nearest
    // content cell, the pane's column 78, row 26.
    const LETTERBOX_MARGIN_CELL_POSITION: Point = Point {
        column: 90,
        row: 35,
    };

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "per-client-mouse",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure) = attach_test_client_with_viewport(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.panes[0].pane_id;

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE);

            // The program in the pane asks for mouse reports. Until it has, a
            // forwarded event is written nowhere.
            let small_client_connection =
                wait_for_mouse_tracking(&fake_pty_backend, pane_id, small_client_connection);

            let small_client_connection = send_mouse_press(
                small_client_connection,
                pane_id,
                SHARED_PANE_CELL_POSITION,
                3,
            );
            let large_client_connection = send_mouse_press(
                large_client_connection,
                pane_id,
                SHARED_PANE_CELL_POSITION,
                3,
            );
            let large_client_connection = send_mouse_press(
                large_client_connection,
                pane_id,
                LETTERBOX_MARGIN_CELL_POSITION,
                4,
            );

            (
                vec![large_client_connection, small_client_connection],
                pane_id,
            )
        },
    );

    // Three reports, in the order the rounds ran. The same terminal cell names
    // a different pane cell for each client, because each round is placed in
    // the view of the client that sent it.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![
            b"\x1b[<0;11;6M".to_vec(),
            b"\x1b[<0;1;1M".to_vec(),
            b"\x1b[<0;78;26M".to_vec(),
        ],
    );
}

/// A press forwarded to a pane whose program has not asked for mouse reports
/// writes nothing to that pane's PTY. The round is still answered.
#[test]
fn a_mouse_press_before_the_pane_asks_for_reports_writes_nothing() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "mouse-before-tracking",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure) =
                attach_test_client_with_viewport(&mut client_connection, 2, LARGE_VIEWPORT_SIZE);
            let pane_id = session_structure.panes[0].pane_id;

            // The tab's only viewer, so its content starts at column 1, row 2 of
            // its own terminal: one tabline row, then the pane's 1-cell border.
            let client_connection =
                send_mouse_press(client_connection, pane_id, Point { column: 1, row: 2 }, 3);

            (vec![client_connection], pane_id)
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        Vec::<Vec<u8>>::new(),
    );
}

#[test]
fn switching_session_detaches_the_client_here_and_lets_it_join_the_other_session() {
    // The per-axis minimum of [`LARGE_VIEWPORT_SIZE`] and [`SMALL_VIEWPORT_SIZE`], as a pane region: the size
    // the tab holds while both clients view it.
    const SHARED_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 80,
        row_count: 28,
    };

    // The pane region [`LARGE_VIEWPORT_SIZE`] gives the tab on its own, once the other client
    // is gone.
    const SINGLE_CLIENT_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 100,
        row_count: 38,
    };

    // One process serves one session, so the session moved to is a second
    // server of its own, seeded and served on its own thread. The mover reads
    // the session's id off the first channel; the second tells the joining side
    // that the client has left the session it was in.
    let (target_session_id_sender, target_session_id_receiver) = mpsc::channel();
    let (source_client_left_sender, source_client_left_receiver) = mpsc::channel();

    let target_session_thread = std::thread::spawn(move || {
        let (
            target_server,
            target_fake_backend,
            (target_session_id, target_client_id, target_pane_id),
        ) = serve_test_session(
            "switch-target",
            move |runtime_directory, session_id, _fake_pty_backend| {
                target_session_id_sender
                    .send(session_id)
                    .expect("the mover reads the id");
                source_client_left_receiver
                    .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                    .expect("the client leaves the session it was in");

                // What the moved client does next, and all the router does for
                // it: open this session's socket and attach there.
                let mut joining_connection =
                    open_session_connection(&runtime_directory, session_id);
                let (joined_client_id, joined_session_id, session_structure) =
                    attach_test_client_with_viewport(
                        &mut joining_connection,
                        2,
                        SMALL_VIEWPORT_SIZE,
                    );
                assert_eq!(joined_session_id, session_id);
                (
                    vec![joining_connection],
                    (
                        session_id,
                        joined_client_id,
                        session_structure.panes[0].pane_id,
                    ),
                )
            },
        );

        // The reply named a client this session minted for the attach.
        let session = target_server
            .list_sessions()
            .get(&target_session_id)
            .expect("session running");
        assert_eq!(session.clients.client_count(), 1);
        assert_eq!(
            session
                .clients
                .get_client_by_id(target_client_id)
                .expect("minted client")
                .get_client_id(),
            target_client_id,
        );

        // Two resizes: the size the seeded session gave the pane, and the
        // joining client's own region once it attached.
        assert_eq!(
            target_fake_backend
                .list_pane_sizes(target_pane_id)
                .expect("the pane was spawned"),
            vec![
                SEEDED_PTY_SIZE,
                PtySize {
                    column_count: 78,
                    row_count: 26
                }
            ],
        );
    });

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "switch-source",
        move |runtime_directory, session_id, _fake_pty_backend| {
            let destination_session_id = target_session_id_receiver
                .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                .expect("the other session is serving");

            let mut wide_connection = open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure) =
                attach_test_client_with_viewport(&mut wide_connection, 2, LARGE_VIEWPORT_SIZE);
            let source_pane_id = session_structure.panes[0].pane_id;

            let mut narrow_connection = open_session_connection(&runtime_directory, session_id);
            let (narrow_client_id, _, _) =
                attach_test_client_with_viewport(&mut narrow_connection, 2, SMALL_VIEWPORT_SIZE);

            // Read the large client's stream past the shared size, so the frame read
            // after the move is one the move caused.
            let (wide_connection, _) =
                read_session_frames_until(wide_connection, |session_event| match session_event {
                    SessionEvent::Painted {
                        frame: painted_frame,
                    } => {
                        painted_frame
                            .session_snapshot
                            .active_tab_snapshot
                            .effective_cell_size
                            == SHARED_PANE_VIEWPORT_SIZE
                    }
                    _ => false,
                });

            // The move itself. It puts the other session on the moved client's own
            // queue and changes nothing here, so it emits nothing.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SwitchSession(SwitchSessionArgs {
                    client_id: Some(narrow_client_id),
                    session_id: destination_session_id,
                }),
                3,
            );
            assert_eq!(emitted_events, Vec::<Event>::new());

            // The moved client is told where to go on its event stream.
            let (narrow_connection, switch_session_events) =
                read_session_frames_until(narrow_connection, |session_event| {
                    matches!(session_event, SessionEvent::SwitchTo { .. })
                });
            assert_eq!(
                switch_session_events.last(),
                Some(&SessionEvent::SwitchTo {
                    session_id: destination_session_id,
                }),
            );

            // The client leaves by closing its connection, which is what a real one
            // does once it has read where to go.
            drop(narrow_connection);
            source_client_left_sender
                .send(())
                .expect("the other session is waiting");

            // The tab grows back to the client that stayed, over one pane inside a
            // 1-cell border: this session keeps serving that client.
            let (wide_connection, wide_session_events) =
                read_session_frames_until(wide_connection, |session_event| match session_event {
                    SessionEvent::Painted {
                        frame: painted_frame,
                    } => {
                        painted_frame
                            .session_snapshot
                            .active_tab_snapshot
                            .effective_cell_size
                            == SINGLE_CLIENT_PANE_VIEWPORT_SIZE
                    }
                    _ => false,
                });
            assert_eq!(
                get_last_painted_frame(&wide_session_events)
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots,
                vec![FrameSlot {
                    pane_id: source_pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        cell_size: SINGLE_CLIENT_PANE_VIEWPORT_SIZE,
                    },
                    content_rect: Some(Rect {
                        origin: Point { column: 1, row: 1 },
                        cell_size: Size {
                            column_count: 98,
                            row_count: 36
                        },
                    }),
                    pane_kind: PaneKind::Terminal,
                    is_visible: true,
                    is_suppressed: false,
                    is_dead: false,
                }],
            );

            (vec![caller_connection, wide_connection], source_pane_id)
        },
    );

    // Four resizes, in this order: the size the seeded session gave the pane,
    // the staying client's own region, the shared minimum once the other client
    // joined the tab, and the staying client's region again once that client
    // moved away.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
            PtySize {
                column_count: 98,
                row_count: 36
            },
        ],
    );

    target_session_thread
        .join()
        .expect("the other session finished");
}

/// How long the loop in [`run_server_until_quit`] blocks on its inbox before it
/// reads the quit request again.
const QUIT_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How long [`run_server_until_quit`] runs before it stops waiting for the quit
/// request. The exchange is over in milliseconds, so a session that never asks
/// to close waits this out.
const QUIT_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// [`serve_test_session`] with `auto-close-session` set to
/// `should_auto_close_session`, run under a loop
/// modelled on the per-session server binary's, minus the wait for clients
/// carried across an image swap, which no server here has: the quit request is
/// read after every event, and the inbox's own quit hangup is applied like any
/// other event. The serving thread queues a dropped connection's detach, and
/// the exchange thread queues the hangup, so the loop keeps reading until the
/// quit request is set or [`QUIT_TIMEOUT_DURATION`] has passed.
///
/// Returns the server once the quit request is set, or once [`QUIT_TIMEOUT_DURATION`]
/// has passed, so a test can read whether the session asked to close.
fn run_server_until_quit(
    session_label: &str,
    should_auto_close_session: bool,
    run_exchange: impl FnOnce(PathBuf, SessionId) -> Vec<Connection> + Send + 'static,
) -> Server {
    // A short base keeps the Unix socket path inside the OS path-length cap.
    #[cfg(unix)]
    let socket_path_base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_path_base = std::env::temp_dir();
    let runtime_directory = socket_path_base.join(format!(
        "koshi-multi-client-{}-{session_label}",
        std::process::id()
    ));

    let session_id = SessionId::new();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let mut server = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );
    server.load_startup_config(Some(PartialKoshiConfig {
        should_auto_close_session: Some(should_auto_close_session),
        ..PartialKoshiConfig::default()
    }));
    server
        .bootstrap_session(
            session_id,
            TEST_SESSION_NAME.to_string(),
            TEST_VIEWPORT_SIZE,
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("seed the session");
    let ipc_server = IpcServer::start(
        &runtime_directory,
        session_id,
        runtime_event_sender.clone(),
        None,
    )
    .expect("start serving");

    let exchange_runtime_directory = runtime_directory.clone();
    let exchange_thread = std::thread::spawn(move || {
        let _stop = StopDispatcher(runtime_event_sender);
        run_exchange(exchange_runtime_directory, session_id)
    });

    let deadline = Instant::now() + QUIT_TIMEOUT_DURATION;
    while !server.is_quit_requested() && Instant::now() < deadline {
        if let Ok(runtime_event) = server.inbox_rx().recv_timeout(QUIT_POLL_INTERVAL_DURATION) {
            let _ = server.handle_runtime_event(runtime_event);
        }
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
    }

    let open_connections = exchange_thread.join().expect("the exchange finished");
    drop(open_connections);
    ipc_server.shutdown();
    let _ = std::fs::remove_dir_all(&runtime_directory);
    server
}

/// Attach one client at [`SMALL_VIEWPORT_SIZE`], move it to another session, and close its
/// connection: the whole of what a moved client does to the session it leaves.
/// Returns the caller connection, which is attached to nothing.
fn move_the_only_client_away(runtime_directory: PathBuf, session_id: SessionId) -> Vec<Connection> {
    // The session moved to is never read here: the id is put on the moved
    // client's queue, and that client reaches the other session itself.
    let destination_session_id = SessionId::new();

    let mut client_connection = open_session_connection(&runtime_directory, session_id);
    let (client_id, _, _) =
        attach_test_client_with_viewport(&mut client_connection, 2, SMALL_VIEWPORT_SIZE);

    let mut caller_connection = open_session_connection(&runtime_directory, session_id);
    let emitted_events = submit_test_command(
        &mut caller_connection,
        session_id,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: Some(client_id),
            session_id: destination_session_id,
        }),
        3,
    );
    assert_eq!(emitted_events, Vec::<Event>::new());

    let (client_connection, session_events) =
        read_session_frames_until(client_connection, |session_event| {
            matches!(session_event, SessionEvent::SwitchTo { .. })
        });
    assert_eq!(
        session_events.last(),
        Some(&SessionEvent::SwitchTo {
            session_id: destination_session_id,
        }),
    );

    drop(client_connection);
    vec![caller_connection]
}

#[test]
fn switching_the_last_client_away_closes_the_session_only_with_auto_close_on() {
    // The moved client was the only one attached, so its leaving empties the
    // session and `auto-close-session` asks the process to quit.
    let auto_close_server =
        run_server_until_quit("switch-auto-close-on", true, move_the_only_client_away);
    assert!(
        auto_close_server.is_quit_requested(),
        "the emptied session was left running",
    );

    // The same move with the setting off: the session keeps running with no
    // client attached.
    let keep_open_server =
        run_server_until_quit("switch-auto-close-off", false, move_the_only_client_away);
    assert!(
        !keep_open_server.is_quit_requested(),
        "the emptied session asked to close",
    );
}
