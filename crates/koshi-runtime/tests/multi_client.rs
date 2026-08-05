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
use koshi_core::event::{Event, InputMode, InputModeChanged, PtyResized};
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::ModFlags;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, MouseTracking};
use koshi_core::process::PtySize;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{FrameSlot, PaintedFrame};
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, WireMouseAction,
    PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_pane::pane::state::PaneKind;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

/// The terminal size the seeded session sizes its root pane against, before any
/// client attaches.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The PTY size [`VIEWPORT`] gives the seeded session's single pane: one
/// tabline row and one hint row off the terminal, then a 1-cell pane border.
const SEEDED: PtySize = PtySize { cols: 78, rows: 20 };

/// The larger of the two viewports two clients share a tab at.
const BIG: Size = Size {
    cols: 100,
    rows: 40,
};

/// Smaller than [`BIG`] on both axes.
const SMALL: Size = Size { cols: 80, rows: 30 };

/// Narrower than [`SHORT`], and taller than it.
const NARROW: Size = Size { cols: 70, rows: 40 };

/// Wider than [`NARROW`], and shorter than it.
const SHORT: Size = Size {
    cols: 100,
    rows: 24,
};

/// The display name the seeded session carries, so a reply can be checked
/// against a known value rather than a generated one.
const SESSION_NAME: &str = "workspace";

/// How long a test waits on work it cannot make happen itself — a detach the
/// serving thread has yet to notice, an event frame in flight — before failing.
const PATIENCE: Duration = Duration::from_secs(5);

/// Stops the dispatcher when the exchange thread ends, on the way out of a
/// failed assertion as well as a clean return, so a broken test reports
/// instead of leaving the dispatcher blocked on its inbox.
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
/// socket, and the fake backend.
///
/// It hands back the connections it wants left open alongside its own value:
/// a connection dropped while the dispatcher is still draining detaches its
/// client, so a test reading the registry keeps its connections here until the
/// dispatcher has stopped.
fn served<T: Send + 'static>(
    tag: &str,
    exchange: impl FnOnce(PathBuf, SessionId, Arc<FakePtyBackend>) -> (Vec<Connection>, T)
        + Send
        + 'static,
) -> (Server, Arc<FakePtyBackend>, T) {
    // A short base keeps the Unix socket path inside the OS path-length cap.
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let runtime_dir = base.join(format!("koshi-multi-client-{}-{tag}", std::process::id()));

    let session_id = SessionId::new();
    let fake = Arc::new(FakePtyBackend::new());
    let backend: Arc<dyn PtyBackend> = fake.clone();
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let mut server = Server::new(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
    );
    server
        .bootstrap_session(
            session_id,
            SESSION_NAME.to_string(),
            VIEWPORT,
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("seed the session");
    let ipc = IpcServer::start(&runtime_dir, session_id, inbox_tx.clone()).expect("start serving");

    let caller_dir = runtime_dir.clone();
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

    let (open, produced) = caller.join().expect("the exchange finished");
    drop(open);
    ipc.shutdown();
    let _ = std::fs::remove_dir_all(&runtime_dir);
    (server, fake, produced)
}

/// Connect to the socket the endpoint file advertises and walk the Hello, so
/// the returned connection is open for every other request kind.
fn open(runtime_dir: &Path, session_id: SessionId) -> Connection {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session_id))
        .expect("endpoint file readable");
    let mut connection = Connection::connect(&endpoint.socket).expect("connect");
    connection
        .send(&IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                protocol_version: PROTOCOL_VERSION,
                token: endpoint.token,
            },
        })
        .expect("send hello");
    let reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(reply.result, IpcResult::Hello);
    connection
}

/// Attach on `connection` reporting `viewport`, and return what the reply
/// carried. The connection carries only the client's event stream afterwards.
fn attach_sized(
    connection: &mut Connection,
    request_id: u64,
    viewport: Size,
) -> (ClientId, SessionId, AttachedSessionStructureSnapshot) {
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::Attach {
                viewport,
                filter: EventFilterSpec::All,
            },
        })
        .expect("send attach");
    let reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(reply.request_id, Some(request_id));
    let IpcResult::Attached {
        client_id,
        session_id,
        structure,
    } = reply.result
    else {
        panic!("expected an attach reply, got {:?}", reply.result);
    };
    (client_id, session_id, structure)
}

/// What the session reports about itself over `connection`.
fn overview(connection: &mut Connection, request_id: u64) -> SessionOverview {
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let reply: IpcResponse = connection.recv().expect("discovery reply");
    let IpcResult::Overview(overview) = reply.result else {
        panic!("expected an overview, got {:?}", reply.result);
    };
    overview
}

/// How many clients the session reports over `connection`.
fn attached_client_count(connection: &mut Connection, request_id: u64) -> usize {
    overview(connection, request_id).clients.len()
}

/// Ask over `connection` until the session reports `want` clients, numbering
/// the requests from `request_id`. Panics once [`PATIENCE`] has passed.
fn wait_for_client_count(connection: &mut Connection, want: usize, request_id: u64) {
    let deadline = Instant::now() + PATIENCE;
    let mut request_id = request_id;
    loop {
        let count = attached_client_count(connection, request_id);
        if count == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the session reports {count} attached clients, not {want}",
        );
        request_id += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Submit a command over `connection` and return the events it emitted.
/// Panics unless the session applied it.
fn submit(
    connection: &mut Connection,
    session_id: SessionId,
    command: Command,
    request_id: u64,
) -> Vec<Event> {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
        },
        SystemTime::UNIX_EPOCH,
        command,
    );
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("send command");
    let reply: IpcResponse = connection.recv().expect("command reply");
    let IpcResult::CommandResult(CommandResult::Ok {
        command_id: _,
        emitted_events,
    }) = reply.result
    else {
        panic!("expected the command to apply, got {:?}", reply.result);
    };
    emitted_events
}

/// Read `connection`'s event stream until `wanted` accepts a frame, on a thread
/// this one can give up waiting on. Returns every frame read, the accepted one
/// last, and the connection so it stays open.
fn read_frames_until(
    mut connection: Connection,
    wanted: impl Fn(&SessionEvent) -> bool + Send + 'static,
) -> (Connection, Vec<SessionEvent>) {
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut frames = Vec::new();
        loop {
            let frame: SessionEvent = connection.recv().expect("an event frame");
            let last = wanted(&frame);
            frames.push(frame);
            if last {
                break;
            }
        }
        let _ = done_tx.send((connection, frames));
    });
    done_rx
        .recv_timeout(PATIENCE)
        .expect("the awaited frame reaches the viewer")
}

/// [`read_frames_until`] stopping at the goodbye frame.
fn read_to_goodbye(connection: Connection) -> (Connection, Vec<SessionEvent>) {
    read_frames_until(connection, |frame| *frame == SessionEvent::Detached)
}

/// The painted frame `frames` ends with. Panics unless the last frame read is
/// a painted one.
fn last_painted(frames: &[SessionEvent]) -> &PaintedFrame {
    match frames.last() {
        Some(SessionEvent::Painted { frame }) => frame,
        other => panic!("expected the run to end with a painted frame, got {other:?}"),
    }
}

/// The tab and the pane the [`Command::NewTab`] in `emitted` created.
fn created_tab_and_pane(emitted: &[Event]) -> (TabId, PaneId) {
    emitted
        .iter()
        .find_map(|event| match event {
            Event::PaneCreated(payload) => Some((payload.tab_id, payload.pane_id)),
            _ => None,
        })
        .expect("the new tab reports its tab and its root pane")
}

#[test]
fn two_clients_on_one_tab_size_the_pty_to_the_per_axis_minimum() {
    let (_server, fake, pane_id) = served("per-axis-minimum", |dir, session_id, _fake| {
        // The big client alone: the tab is its own pane region, 100 columns by
        // 38 rows, and the pane's PTY is that minus its 1-cell border.
        let mut big = open(&dir, session_id);
        let (_, _, structure) = attach_sized(&mut big, 2, BIG);
        let pane_id = structure.panes[0].id;

        // The small client joins the same tab. It is narrower and shorter, so
        // it takes both axes and the pane's PTY shrinks on both.
        let mut small = open(&dir, session_id);
        attach_sized(&mut small, 2, SMALL);

        let mut caller = open(&dir, session_id);
        assert_eq!(attached_client_count(&mut caller, 3), 2);

        (vec![caller, big, small], pane_id)
    });

    // Three resizes, in this order: the size the seeded session gave the pane,
    // the big client's own region when it attached, and the per-axis minimum of
    // the two clients once the small one joined.
    assert_eq!(
        fake.resizes(pane_id).expect("the pane was spawned"),
        vec![
            SEEDED,
            PtySize { cols: 98, rows: 36 },
            PtySize { cols: 78, rows: 26 },
        ],
    );
}

#[test]
fn each_axis_takes_its_minimum_from_a_different_client_and_grows_back_when_that_client_leaves() {
    let (_server, fake, pane_id) = served("mixed-axis-minimum", |dir, session_id, _fake| {
        // The narrow client alone: 70 columns by 38 rows of pane region.
        let mut narrow = open(&dir, session_id);
        let (_, _, structure) = attach_sized(&mut narrow, 2, NARROW);
        let pane_id = structure.panes[0].id;

        // The short client joins the same tab. It is wider but shorter, so the
        // columns stay pinned by the narrow client and the rows drop to this
        // one's: each axis takes its minimum from a different client.
        let mut short = open(&dir, session_id);
        let (short_client, _, _) = attach_sized(&mut short, 2, SHORT);

        let mut caller = open(&dir, session_id);
        assert_eq!(attached_client_count(&mut caller, 3), 2);

        // The short client leaves. The narrow one is the only viewer left, so
        // the rows grow back to its own region while the columns never move.
        let emitted = submit(
            &mut caller,
            session_id,
            Command::Detach(DetachArgs {
                client: Some(short_client),
            }),
            4,
        );
        assert_eq!(
            emitted,
            vec![Event::PtyResized(PtyResized {
                pane_id,
                size: PtySize { cols: 68, rows: 36 },
            })],
        );

        let (short, frames) = read_to_goodbye(short);
        assert_eq!(frames.last(), Some(&SessionEvent::Detached));
        wait_for_client_count(&mut caller, 1, 5);

        (vec![caller, narrow, short], pane_id)
    });

    // Four resizes, in this order: the size the seeded session gave the pane,
    // the narrow client's own region, the mixed-axis minimum once the short
    // client joined, and the narrow client's region again once it left. The
    // columns are the narrow client's throughout.
    assert_eq!(
        fake.resizes(pane_id).expect("the pane was spawned"),
        vec![
            SEEDED,
            PtySize { cols: 68, rows: 36 },
            PtySize { cols: 68, rows: 20 },
            PtySize { cols: 68, rows: 36 },
        ],
    );
}

#[test]
fn a_client_viewing_another_tab_never_constrains_this_tabs_size() {
    let (_server, fake, (first_pane, second_pane)) =
        served("per-tab-independence", |dir, session_id, _fake| {
            // The big client attaches to the seeded tab.
            let mut big = open(&dir, session_id);
            let (big_client, _, structure) = attach_sized(&mut big, 2, BIG);
            let first_tab = structure.tabs[0].id;
            let first_pane = structure.panes[0].id;

            // A second tab, created for the big client, which moves onto it.
            // Its root pane spawns at the big client's own region.
            let mut caller = open(&dir, session_id);
            let emitted = submit(
                &mut caller,
                session_id,
                Command::NewTab(NewTabArgs {
                    cwd: None,
                    client: Some(big_client),
                }),
                3,
            );
            let (second_tab, second_pane) = created_tab_and_pane(&emitted);

            // Send the big client back, so it is the first tab's only viewer
            // again and the second tab has none.
            submit(
                &mut caller,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Id(first_tab),
                    client: Some(big_client),
                }),
                4,
            );

            // The small client attaches. A fresh attach lands on the
            // lowest-indexed tab, which is the first one, so the two clients
            // share it and the small one takes both axes.
            let mut small = open(&dir, session_id);
            let (small_client, _, _) = attach_sized(&mut small, 2, SMALL);
            assert_eq!(attached_client_count(&mut caller, 5), 2);

            // The small client switches to the second tab. The first tab is the
            // big client's alone again; the second tab is the small client's.
            submit(
                &mut caller,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Id(second_tab),
                    client: Some(small_client),
                }),
                6,
            );

            (vec![caller, big, small], (first_pane, second_pane))
        });

    // The first tab's pane: seeded, the big client's own region, down to the
    // shared minimum while the small client viewed it, and back to the big
    // client's region once the small one left for the other tab. The small
    // client viewing another tab adds nothing after that.
    assert_eq!(
        fake.resizes(first_pane).expect("the pane was spawned"),
        vec![
            SEEDED,
            PtySize { cols: 98, rows: 36 },
            PtySize { cols: 78, rows: 26 },
            PtySize { cols: 98, rows: 36 },
        ],
    );

    // The second tab's pane: spawned at the big client's region, then sized to
    // the small client's alone once that client switched onto it. The big
    // client viewing the first tab never bounds it.
    assert_eq!(
        fake.resizes(second_pane).expect("the pane was spawned"),
        vec![
            PtySize { cols: 98, rows: 36 },
            PtySize { cols: 78, rows: 26 },
        ],
    );
}

#[test]
fn the_larger_client_sees_the_tab_letterboxed_at_the_shared_size() {
    // The per-axis minimum of [`BIG`] and [`SMALL`], as a pane region.
    const SHARED: Size = Size { cols: 80, rows: 28 };

    let (_server, _fake, ()) = served("letterbox", |dir, session_id, _fake| {
        let mut big = open(&dir, session_id);
        let (big_client, _, structure) = attach_sized(&mut big, 2, BIG);
        let pane_id = structure.panes[0].id;
        let tab_id = structure.tabs[0].id;

        // The small client joins the same tab, which invalidates the layout, so
        // the big client is sent a fresh frame at the size the two now share.
        let mut small = open(&dir, session_id);
        attach_sized(&mut small, 2, SMALL);

        let (big, frames) = read_frames_until(big, move |frame| match frame {
            SessionEvent::Painted { frame } => frame.session.active_tab.effective_size == SHARED,
            _ => false,
        });
        let painted = last_painted(&frames);

        // The big client's own terminal is unchanged; the tab it draws is the
        // shared size, and the margin around it is the letterbox.
        assert_eq!(painted.client.id, big_client);
        assert_eq!(painted.client.viewport, BIG);
        assert_eq!(painted.client.active_tab, tab_id);
        assert_eq!(painted.session.active_tab.id, tab_id);
        assert_eq!(painted.session.active_tab.effective_size, SHARED);

        // The tab holds one pane, solved at origin (0, 0) over the shared size,
        // with its content inside a 1-cell border.
        assert_eq!(
            painted.session.active_tab.slots,
            vec![FrameSlot {
                pane_id,
                rect: Rect {
                    origin: Point { x: 0, y: 0 },
                    size: SHARED,
                },
                inner_rect: Some(Rect {
                    origin: Point { x: 1, y: 1 },
                    size: Size { cols: 78, rows: 26 },
                }),
                kind: PaneKind::Terminal,
                visible: true,
                suppressed: false,
                dead: false,
            }],
        );
        assert!(!painted.session.active_tab.all_suppressed);

        (vec![big, small], ())
    });
}

#[test]
fn locking_one_client_leaves_the_other_clients_lock_state_unchanged() {
    let (_server, _fake, ()) = served("per-client-lock", |dir, session_id, _fake| {
        let mut big = open(&dir, session_id);
        let (big_client, _, _) = attach_sized(&mut big, 2, BIG);

        let mut small = open(&dir, session_id);
        let (small_client, _, _) = attach_sized(&mut small, 2, SMALL);

        // Lock the big client. Lock mode belongs to one client, so the command
        // reports a single change and it names that client alone.
        let mut caller = open(&dir, session_id);
        let emitted = submit(
            &mut caller,
            session_id,
            Command::SetLockMode(LockModeArgs {
                locked: true,
                client: Some(big_client),
            }),
            3,
        );
        assert_eq!(
            emitted,
            vec![Event::InputModeChanged(InputModeChanged {
                client_id: big_client,
                mode: InputMode::Locked,
            })],
        );

        // Lock the small client. Setting the mode a client already holds emits
        // nothing, so this event is what proves the small client was still
        // unlocked while the big one was locked.
        let emitted = submit(
            &mut caller,
            session_id,
            Command::SetLockMode(LockModeArgs {
                locked: true,
                client: Some(small_client),
            }),
            4,
        );
        assert_eq!(
            emitted,
            vec![Event::InputModeChanged(InputModeChanged {
                client_id: small_client,
                mode: InputMode::Locked,
            })],
        );

        (vec![caller, big, small], ())
    });
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
    let (connection, _) = read_frames_until(connection, move |frame| match frame {
        SessionEvent::Painted { frame } => frame
            .panes
            .iter()
            .any(|painted| painted.id == pane && painted.mouse_tracking == MouseTracking::Normal),
        _ => false,
    });
    connection
}

/// Send one mouse round holding a single left press on `pane` at the client
/// cell `at`, then read `connection`'s stream until that round is answered —
/// the point from which the write it carried has happened.
fn press_pane(mut connection: Connection, pane: PaneId, at: Point, request_id: u64) -> Connection {
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                pane,
                mouse: MouseInput {
                    kind: MouseKind::Press(MouseButton::Left),
                    at,
                    mods: ModFlags::NONE,
                },
            }]),
        })
        .expect("send mouse round");
    let (connection, _) = read_frames_until(connection, move |frame| match frame {
        SessionEvent::MouseAnswer {
            request_id: answered,
            answers: _,
        } => *answered == request_id,
        _ => false,
    });
    connection
}

#[test]
fn a_mouse_click_is_answered_against_the_clicking_clients_own_view() {
    // The one cell both clients press, each in its own terminal. The tab they
    // share is 80 by 28: the small client's 80x30 terminal holds it at (0, 1),
    // putting the pane's content at (1, 2), and the big client's 100x40
    // terminal centers it at (10, 6), putting the pane's content at (11, 7).
    // So this cell is the pane's column 11, row 6 for the small client, and the
    // pane's column 1, row 1 for the big one.
    const SHARED_CELL: Point = Point { x: 11, y: 7 };

    // A cell in the big client's own 100x40 terminal, past the right and bottom
    // edges of the pane's content there (columns 11 to 88, rows 7 to 32): the
    // letterbox margin around the shared tab. It is pulled to the nearest
    // content cell, the pane's column 78, row 26.
    const MARGIN_CELL: Point = Point { x: 90, y: 35 };

    let (_server, fake, pane_id) = served("per-client-mouse", |dir, session_id, fake| {
        let mut big = open(&dir, session_id);
        let (_, _, structure) = attach_sized(&mut big, 2, BIG);
        let pane_id = structure.panes[0].id;

        let mut small = open(&dir, session_id);
        attach_sized(&mut small, 2, SMALL);

        // The program in the pane asks for mouse reports. Until it has, a
        // forwarded event is written nowhere.
        let small = wait_for_mouse_tracking(&fake, pane_id, small);

        let small = press_pane(small, pane_id, SHARED_CELL, 3);
        let big = press_pane(big, pane_id, SHARED_CELL, 3);
        let big = press_pane(big, pane_id, MARGIN_CELL, 4);

        (vec![big, small], pane_id)
    });

    // Three reports, in the order the rounds ran. The same terminal cell names
    // a different pane cell for each client, because each round is placed in
    // the view of the client that sent it.
    assert_eq!(
        fake.writes(pane_id).expect("the pane was spawned"),
        vec![
            b"\x1b[<0;11;6M".to_vec(),
            b"\x1b[<0;1;1M".to_vec(),
            b"\x1b[<0;78;26M".to_vec(),
        ],
    );
}

#[test]
fn switching_session_detaches_the_client_here_and_lets_it_join_the_other_session() {
    // The per-axis minimum of [`BIG`] and [`SMALL`], as a pane region: the size
    // the tab holds while both clients view it.
    const SHARED: Size = Size { cols: 80, rows: 28 };

    // The pane region [`BIG`] gives the tab on its own, once the other client
    // is gone.
    const ALONE: Size = Size {
        cols: 100,
        rows: 38,
    };

    // One process serves one session, so the session moved to is a second
    // server of its own, seeded and served on its own thread. The mover reads
    // the session's id off the first channel; the second tells the joining side
    // that the client has left the session it was in.
    let (target_tx, target_rx) = mpsc::channel();
    let (left_tx, left_rx) = mpsc::channel();

    let target = std::thread::spawn(move || {
        let (server, fake, (session_id, client_id, pane_id)) =
            served("switch-target", move |dir, session_id, _fake| {
                target_tx.send(session_id).expect("the mover reads the id");
                left_rx
                    .recv_timeout(PATIENCE)
                    .expect("the client leaves the session it was in");

                // What the moved client does next, and all the router does for
                // it: open this session's socket and attach there.
                let mut joiner = open(&dir, session_id);
                let (client_id, replied_session, structure) = attach_sized(&mut joiner, 2, SMALL);
                assert_eq!(replied_session, session_id);
                (vec![joiner], (session_id, client_id, structure.panes[0].id))
            });

        // The reply named a client this session minted for the attach.
        let session = server.sessions().get(&session_id).expect("session running");
        assert_eq!(session.clients.len(), 1);
        assert_eq!(
            session.clients.get(client_id).expect("minted client").id(),
            client_id,
        );

        // Two resizes: the size the seeded session gave the pane, and the
        // joining client's own region once it attached.
        assert_eq!(
            fake.resizes(pane_id).expect("the pane was spawned"),
            vec![SEEDED, PtySize { cols: 78, rows: 26 }],
        );
    });

    let (_server, fake, pane_id) = served("switch-source", move |dir, session_id, _fake| {
        let target_session = target_rx
            .recv_timeout(PATIENCE)
            .expect("the other session is serving");

        let mut big = open(&dir, session_id);
        let (_, _, structure) = attach_sized(&mut big, 2, BIG);
        let pane_id = structure.panes[0].id;

        let mut small = open(&dir, session_id);
        let (small_client, _, _) = attach_sized(&mut small, 2, SMALL);

        // Read the big client's stream past the shared size, so the frame read
        // after the move is one the move caused.
        let (big, _) = read_frames_until(big, |frame| match frame {
            SessionEvent::Painted { frame } => frame.session.active_tab.effective_size == SHARED,
            _ => false,
        });

        // The move itself. It puts the other session on the moved client's own
        // queue and changes nothing here, so it emits nothing.
        let mut caller = open(&dir, session_id);
        let emitted = submit(
            &mut caller,
            session_id,
            Command::SwitchSession(SwitchSessionArgs {
                client: Some(small_client),
                session: target_session,
            }),
            3,
        );
        assert_eq!(emitted, Vec::<Event>::new());

        // The moved client is told where to go on its event stream.
        let (small, frames) = read_frames_until(small, |frame| {
            matches!(frame, SessionEvent::SwitchTo { .. })
        });
        assert_eq!(
            frames.last(),
            Some(&SessionEvent::SwitchTo {
                session_id: target_session,
            }),
        );

        // The client leaves by closing its connection, which is what a real one
        // does once it has read where to go.
        drop(small);
        left_tx.send(()).expect("the other session is waiting");

        // The tab grows back to the client that stayed, over one pane inside a
        // 1-cell border: this session keeps serving that client.
        let (big, frames) = read_frames_until(big, |frame| match frame {
            SessionEvent::Painted { frame } => frame.session.active_tab.effective_size == ALONE,
            _ => false,
        });
        assert_eq!(
            last_painted(&frames).session.active_tab.slots,
            vec![FrameSlot {
                pane_id,
                rect: Rect {
                    origin: Point { x: 0, y: 0 },
                    size: ALONE,
                },
                inner_rect: Some(Rect {
                    origin: Point { x: 1, y: 1 },
                    size: Size { cols: 98, rows: 36 },
                }),
                kind: PaneKind::Terminal,
                visible: true,
                suppressed: false,
                dead: false,
            }],
        );

        (vec![caller, big], pane_id)
    });

    // Four resizes, in this order: the size the seeded session gave the pane,
    // the staying client's own region, the shared minimum once the other client
    // joined the tab, and the staying client's region again once that client
    // moved away.
    assert_eq!(
        fake.resizes(pane_id).expect("the pane was spawned"),
        vec![
            SEEDED,
            PtySize { cols: 98, rows: 36 },
            PtySize { cols: 78, rows: 26 },
            PtySize { cols: 98, rows: 36 },
        ],
    );

    target.join().expect("the other session finished");
}

/// How long the loop in [`served_until_quit`] blocks on its inbox before it
/// reads the quit request again.
const QUIT_POLL: Duration = Duration::from_millis(50);

/// How long [`served_until_quit`] runs before it stops waiting for the quit
/// request. The exchange is over in milliseconds, so a session that never asks
/// to close waits this out.
const QUIT_PATIENCE: Duration = Duration::from_secs(2);

/// [`served`] with `auto-close-session` set to `auto_close`, run under the loop
/// the per-session server binary runs: the quit request is read after every
/// event, and the inbox's own quit hangup is applied like any other event. The
/// serving thread queues a dropped connection's detach, and the exchange thread
/// queues the hangup, so the loop keeps reading until the quit request is set
/// or [`QUIT_PATIENCE`] has passed.
///
/// Returns the server once the quit request is set, or once [`QUIT_PATIENCE`]
/// has passed, so a test can read whether the session asked to close.
fn served_until_quit(
    tag: &str,
    auto_close: bool,
    exchange: impl FnOnce(PathBuf, SessionId) -> Vec<Connection> + Send + 'static,
) -> Server {
    // A short base keeps the Unix socket path inside the OS path-length cap.
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let runtime_dir = base.join(format!("koshi-multi-client-{}-{tag}", std::process::id()));

    let session_id = SessionId::new();
    let backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let mut server = Server::new(
        backend,
        snapshot_provider,
        storage,
        inbox_rx,
        inbox_tx.clone(),
    );
    server.load_startup_config(Some(PartialKoshiConfig {
        auto_close_session: Some(auto_close),
        ..PartialKoshiConfig::default()
    }));
    server
        .bootstrap_session(
            session_id,
            SESSION_NAME.to_string(),
            VIEWPORT,
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("seed the session");
    let ipc = IpcServer::start(&runtime_dir, session_id, inbox_tx.clone()).expect("start serving");

    let caller_dir = runtime_dir.clone();
    let caller = std::thread::spawn(move || {
        let _stop = StopDispatcher(inbox_tx);
        exchange(caller_dir, session_id)
    });

    let deadline = Instant::now() + QUIT_PATIENCE;
    while !server.quit_requested() && Instant::now() < deadline {
        if let Ok(event) = server.inbox_rx().recv_timeout(QUIT_POLL) {
            let _ = server.handle_runtime_event(event);
        }
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
    }

    let open = caller.join().expect("the exchange finished");
    drop(open);
    ipc.shutdown();
    let _ = std::fs::remove_dir_all(&runtime_dir);
    server
}

/// Attach one client at [`SMALL`], move it to another session, and close its
/// connection: the whole of what a moved client does to the session it leaves.
/// Returns the caller connection, which is attached to nothing.
fn move_the_only_client_away(dir: PathBuf, session_id: SessionId) -> Vec<Connection> {
    // The session moved to is never read here: the id is put on the moved
    // client's queue, and that client reaches the other session itself.
    let elsewhere = SessionId::new();

    let mut viewer = open(&dir, session_id);
    let (client_id, _, _) = attach_sized(&mut viewer, 2, SMALL);

    let mut caller = open(&dir, session_id);
    let emitted = submit(
        &mut caller,
        session_id,
        Command::SwitchSession(SwitchSessionArgs {
            client: Some(client_id),
            session: elsewhere,
        }),
        3,
    );
    assert_eq!(emitted, Vec::<Event>::new());

    let (viewer, frames) = read_frames_until(viewer, |frame| {
        matches!(frame, SessionEvent::SwitchTo { .. })
    });
    assert_eq!(
        frames.last(),
        Some(&SessionEvent::SwitchTo {
            session_id: elsewhere,
        }),
    );

    drop(viewer);
    vec![caller]
}

#[test]
fn switching_the_last_client_away_closes_the_session_only_with_auto_close_on() {
    // The moved client was the only one attached, so its leaving empties the
    // session and `auto-close-session` asks the process to quit.
    let closing = served_until_quit("switch-auto-close-on", true, move_the_only_client_away);
    assert!(
        closing.quit_requested(),
        "the emptied session was left running",
    );

    // The same move with the setting off: the session keeps running with no
    // client attached.
    let staying = served_until_quit("switch-auto-close-off", false, move_the_only_client_away);
    assert!(
        !staying.quit_requested(),
        "the emptied session asked to close",
    );
}
