//! Integration cover for attaching over a real control socket: what one
//! attach registers, what the reply carries and when it is written, what a
//! second attach sees, what strict decoding refuses, what a client's key
//! presses and resizes reach, what a client's mouse rounds do to its panes and
//! what the one answer each round is given carries, what a detach leaves behind
//! for the clients that stay and for the panes, and what a dropped connection
//! leaves behind.
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
    FocusPaneArgs, FocusTarget, NewPaneArgs, NewTabArgs,
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
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, WireMouseAction,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_session::client::{pane_viewport, AuthorityTier, ClientOrigin};
use koshi_test_support::fake_pty::FakePtyBackend;

/// The terminal size every attaching client in these tests reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The display name the seeded session carries, so a reply can be checked
/// against a known value rather than a generated one.
const SESSION_NAME: &str = "workspace";

/// How long a test waits on work it cannot make happen itself — a disconnect
/// the serving thread has yet to notice, an event frame in flight — before
/// failing.
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
/// socket, and the fake backend, which is how it makes a pane's program write
/// output while the session is running.
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
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let runtime_dir = base.join(format!("koshi-attach-{}-{tag}", std::process::id()));

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
    let ipc =
        IpcServer::start(&runtime_dir, session_id, inbox_tx.clone(), None).expect("start serving");

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
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                token: endpoint.token,
            },
        })
        .expect("send hello");
    let reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(
        reply.result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
    connection
}

/// Attach on `connection` reporting [`VIEWPORT`], and return what the reply
/// carried. The connection carries only the client's event stream afterwards.
fn attach(
    connection: &mut Connection,
    request_id: u64,
) -> (ClientId, SessionId, AttachedSessionStructureSnapshot) {
    attach_sized(connection, request_id, VIEWPORT)
}

/// [`attach`] reporting `viewport` instead, so a test can put two differently
/// sized clients on one tab.
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
                resume: None,
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

/// Add one tab to the running session over `connection`, and return its id.
fn new_tab(connection: &mut Connection, session_id: SessionId, request_id: u64) -> TabId {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
        },
        SystemTime::UNIX_EPOCH,
        Command::NewTab(NewTabArgs {
            cwd: None,
            client: None,
        }),
    );
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("send new-tab");
    let reply: IpcResponse = connection.recv().expect("new-tab reply");
    let IpcResult::CommandResult(CommandResult::Ok {
        command_id: _,
        emitted_events,
    }) = reply.result
    else {
        panic!("expected the new tab to apply, got {:?}", reply.result);
    };
    emitted_events
        .iter()
        .find_map(|event| match event {
            Event::TabCreated(payload) => Some(payload.tab_id),
            _ => None,
        })
        .expect("the new tab reports its id")
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
///
/// Each frame is decoded as a [`SessionEvent`], so a response frame written on
/// an attached client's connection fails the read: an [`IpcResponse`] is a
/// two-field record where a `SessionEvent` is a single-variant one.
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

#[test]
fn one_attach_registers_the_client_the_server_minted() {
    let (server, _fake, (session_id, client_id, structure)) =
        served("registers", |dir, session_id, _fake| {
            let mut viewer = open(&dir, session_id);
            let (client_id, replied_session, structure) = attach(&mut viewer, 2);
            assert_eq!(replied_session, session_id);
            (vec![viewer], (session_id, client_id, structure))
        });

    assert_eq!(structure.id, session_id);
    assert_eq!(structure.name, SESSION_NAME);
    assert_eq!(structure.tabs.len(), 1);
    assert_eq!(structure.tabs[0].index, 0);
    assert_eq!(structure.panes.len(), 1);

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 1);
    let client = session.clients.get(client_id).expect("minted client");
    assert_eq!(client.id(), client_id);
    assert_eq!(client.session_id(), session_id);
    assert_eq!(client.origin(), ClientOrigin::Local);
    assert_eq!(client.tier(), AuthorityTier::Admin);
    assert_eq!(client.colour(), 0);
    assert_eq!(client.viewport(), VIEWPORT);
    assert_eq!(client.active_tab(), structure.tabs[0].id);
    let label: Vec<&str> = client.label().split('-').collect();
    assert_eq!(label.len(), 3, "generated label, got {}", client.label());
    assert_eq!(label[0], "C");
}

#[test]
fn nothing_in_the_request_can_raise_the_clients_authority() {
    let (server, _fake, session_id) = served("strict", |dir, session_id, _fake| {
        let mut viewer = open(&dir, session_id);

        // A well-framed attach naming one field this build does not know. The
        // field is ignored, so the attach succeeds — and every fact about the
        // client it mints comes from the server, never from these bytes.
        viewer
            .send(&serde_json::json!({
                "request_id": 2,
                "kind": {
                    "Attach": {
                        "viewport": { "cols": 80, "rows": 24 },
                        "filter": "All",
                        "tier": "admin"
                    }
                }
            }))
            .expect("send an attach carrying an extra field");

        let reply: IpcResponse = viewer.recv().expect("attach reply");
        assert_eq!(reply.request_id, Some(2));
        assert!(
            matches!(reply.result, IpcResult::Attached { .. }),
            "the attach was answered with {:?}",
            reply.result
        );
        (vec![viewer], session_id)
    });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 1, "the attach registered one client");
    let client = session
        .clients
        .list_attached()
        .next()
        .expect("the one attached client");
    assert_eq!(
        client.origin(),
        ClientOrigin::Local,
        "the origin comes from the connection, not the request"
    );
    assert_eq!(
        client.tier(),
        AuthorityTier::Admin,
        "the authority is the server's own answer for a local client"
    );
    assert_eq!(
        client.viewport(),
        Size { cols: 80, rows: 24 },
        "the viewport is the one field of the attach the server does take"
    );
}

#[test]
fn the_structure_reply_is_written_before_the_first_event_frame() {
    let (server, _fake, (session_id, first_tab, second_tab)) =
        served("reply-first", |dir, session_id, _fake| {
            let mut viewer = open(&dir, session_id);

            // Frame one on this connection decodes as a response, so no event
            // frame was written ahead of the reply.
            let (_, _, structure) = attach(&mut viewer, 2);
            assert_eq!(structure.tabs.len(), 1);

            // Everything after it is an event frame. Reading one blocks with no
            // deadline of its own, so it happens on a thread this one can give up
            // waiting on.
            let mut caller = open(&dir, session_id);
            let second_tab = new_tab(&mut caller, session_id, 3);

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
                    .recv_timeout(PATIENCE)
                    .expect("the new tab reaches the event stream"),
                second_tab,
            );
            (vec![caller], (session_id, structure.tabs[0].id, second_tab))
        });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.tabs.len(), 2);
    assert!(session.tabs.contains_key(&first_tab));
    assert!(session.tabs.contains_key(&second_tab));
}

#[test]
fn a_second_attach_mints_a_fresh_client_and_sees_the_tab_added_since_the_first() {
    let (server, _fake, (session_id, first_client, second_client)) =
        served("reattach", |dir, session_id, _fake| {
            let mut first = open(&dir, session_id);
            let (first_client, _, before) = attach(&mut first, 2);
            assert_eq!(before.tabs.len(), 1);
            let first_tab = before.tabs[0].id;

            let mut caller = open(&dir, session_id);
            let second_tab = new_tab(&mut caller, session_id, 3);

            let mut second = open(&dir, session_id);
            let (second_client, _, after) = attach(&mut second, 4);
            assert_ne!(second_client, first_client);
            assert_eq!(
                after.tabs.iter().map(|tab| tab.id).collect::<Vec<TabId>>(),
                vec![first_tab, second_tab],
                "the second attach is built from live state, not a cached copy",
            );
            (
                vec![first, second, caller],
                (session_id, first_client, second_client),
            )
        });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 2);
    let first = session.clients.get(first_client).expect("first client");
    let second = session.clients.get(second_client).expect("second client");
    assert_eq!(first.colour(), 0);
    assert_eq!(second.colour(), 1);
    assert_ne!(first.label(), second.label());
}

/// Close `tab` in the running session over `connection`, killing its panes
/// outright. Closing the last tab quits the session.
fn close_tab(connection: &mut Connection, session_id: SessionId, tab: TabId, request_id: u64) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::ExternalCli {
            session_id: Some(session_id),
        },
        SystemTime::UNIX_EPOCH,
        Command::CloseTab(CloseTabArgs {
            tab: Some(tab),
            force: true,
            tree: false,
        }),
    );
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("send close-tab");
    let reply: IpcResponse = connection.recv().expect("close-tab reply");
    let IpcResult::CommandResult(CommandResult::Ok {
        command_id: _,
        emitted_events,
    }) = reply.result
    else {
        panic!("expected the close to apply, got {:?}", reply.result);
    };
    assert!(
        emitted_events
            .iter()
            .any(|event| matches!(event, Event::Quit)),
        "closing the last tab quits the session",
    );
}

#[test]
fn the_event_stream_ends_with_the_quit_frame() {
    let (server, _fake, session_id) = served("quit-ends-stream", |dir, session_id, _fake| {
        let mut viewer = open(&dir, session_id);
        let (_, _, structure) = attach(&mut viewer, 2);
        let only_tab = structure.tabs[0].id;

        let mut caller = open(&dir, session_id);
        assert_eq!(attached_client_count(&mut caller, 3), 1);
        close_tab(&mut caller, session_id, only_tab, 4);

        // The quit frame is the last one the viewer's stream carries.
        let (viewer, frames) = read_frames_until(viewer, |frame| *frame == SessionEvent::Quit);
        assert_eq!(frames.last(), Some(&SessionEvent::Quit));

        // The quit frame ends the stream: its writing thread exits and
        // detaches the client. The viewer connection is still open, so the
        // record going away can only come from that exit.
        let deadline = Instant::now() + PATIENCE;
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
    });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 0);
}

/// A pane's program exiting reaches every attached client while the session
/// keeps serving. A second pane is what keeps it serving: the session ends when
/// its last pane goes, and an ending session drops what is queued for a client.
#[test]
fn the_stream_carries_a_pane_exit_while_the_session_keeps_serving() {
    let (server, _fake, (session_id, second)) = served(
        "pane-exit-on-stream",
        |dir, session_id, fake| {
            let mut viewer = open(&dir, session_id);
            let (client_id, _, structure) = attach(&mut viewer, 2);
            let first = structure.panes[0].id;

            let mut caller = open(&dir, session_id);
            let emitted = submit(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source: Some(first),
                    tab: None,
                    direction: Direction::Right,
                    stacked: false,
                    cwd: None,
                    command: None,
                    client: Some(client_id),
                }),
                3,
            );
            let second = emitted
                .iter()
                .find_map(|event| match event {
                    Event::PaneCreated(created) => Some(created.pane_id),
                    _ => None,
                })
                .expect("the split emitted the new pane");

            // A dying program closes its terminal and then exits, and the forwarder
            // relays the exit once the output before it is drained.
            fake.close_output(second)
                .expect("the second pane's terminal closes");
            fake.trigger_child_exit(second, ExitStatus::ExitCode(0))
                .expect("the second pane's program exits");

            let (viewer, frames) = read_frames_until(
                viewer,
                move |frame| matches!(frame, SessionEvent::PaneProcessExited { pane_id, .. } if *pane_id == second),
            );
            assert_eq!(
                frames.last(),
                Some(&SessionEvent::PaneProcessExited {
                    pane_id: second,
                    exit_code: Some(0),
                }),
            );
            (vec![viewer, caller], (session_id, second))
        },
    );

    // The pane the program left is gone, and the one the client is viewing is
    // still there.
    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.panes.len(), 1);
    assert!(session.panes.get(second).is_none());
}

/// The only pane's program exiting ends the session by itself, with no close
/// asked for: the stream ends with the quit frame, and the session is left with
/// no pane and no tab.
#[test]
fn the_event_stream_ends_with_the_quit_frame_when_the_only_program_exits() {
    let (server, _fake, session_id) = served("quit-on-program-exit", |dir, session_id, fake| {
        let mut viewer = open(&dir, session_id);
        let (_, _, structure) = attach(&mut viewer, 2);
        let only_pane = structure.panes[0].id;

        // A dying program closes its terminal and then exits, and the forwarder
        // relays the exit once the output before it is drained.
        fake.close_output(only_pane)
            .expect("the only pane's terminal closes");
        fake.trigger_child_exit(only_pane, ExitStatus::ExitCode(0))
            .expect("the only pane's program exits");

        // The exit and the quit the session ends on are published in one pass,
        // and the raised ending drops whatever is still queued for a client, so
        // the quit frame is the one frame this stream is promised.
        let (viewer, frames) = read_frames_until(viewer, |frame| *frame == SessionEvent::Quit);
        assert_eq!(frames.last(), Some(&SessionEvent::Quit));
        (vec![viewer], session_id)
    });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.panes.len(), 0);
    assert_eq!(session.tabs.len(), 0);
}

#[test]
fn dropping_an_attached_connection_removes_its_client_record() {
    let (server, _fake, (session_id, client_id, tab_id, pane_id)) =
        served("disconnect", |dir, session_id, _fake| {
            let mut viewer = open(&dir, session_id);
            let (client_id, _, structure) = attach(&mut viewer, 2);
            let tab_id = structure.tabs[0].id;
            let pane_id = structure.panes[0].id;
            let mut caller = open(&dir, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 1);

            drop(viewer);

            // The record goes when the serving thread notices the connection
            // ended, so ask again until it is gone.
            let deadline = Instant::now() + PATIENCE;
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
            let after = overview(&mut caller, request_id + 1);
            assert_eq!(
                after.tabs.iter().map(|tab| tab.id).collect::<Vec<TabId>>(),
                vec![tab_id],
            );
            assert_eq!(
                after
                    .panes
                    .iter()
                    .map(|pane| pane.id)
                    .collect::<Vec<PaneId>>(),
                vec![pane_id],
            );
            (vec![caller], (session_id, client_id, tab_id, pane_id))
        });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 0);
    assert!(session.clients.get(client_id).is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        session.panes.get(pane_id).map(|pane| pane.id()),
        Some(pane_id)
    );
}

#[test]
fn detaching_one_client_leaves_every_other_stream_running() {
    let (server, _fake, (session_id, first_client, second_client, added_tab)) =
        served("detach-one", |dir, session_id, _fake| {
            let mut first = open(&dir, session_id);
            let (first_client, _, _) = attach(&mut first, 2);
            let mut second = open(&dir, session_id);
            let (second_client, _, _) = attach(&mut second, 2);

            // The caller never attaches, so nothing it does rides an event
            // stream of its own.
            let mut caller = open(&dir, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 2);

            let emitted = submit(
                &mut caller,
                session_id,
                Command::Detach(DetachArgs {
                    client: Some(first_client),
                }),
                4,
            );
            // Both clients report the same viewport, so the tab the leaver
            // held keeps its size and the detach emits nothing.
            assert_eq!(emitted, Vec::new());

            // The goodbye is the last frame the first viewer's stream carries.
            let (first, frames) = read_to_goodbye(first);
            assert_eq!(frames.last(), Some(&SessionEvent::Detached));
            wait_for_client_count(&mut caller, 1, 5);

            // The client that stayed is untouched: a tab added now still
            // reaches its stream.
            let added_tab = new_tab(&mut caller, session_id, 100);
            let (found_tx, found_rx) = mpsc::channel();
            std::thread::spawn(move || loop {
                let frame: SessionEvent = second.recv().expect("an event frame");
                if let SessionEvent::TabCreated { tab_id } = frame {
                    let _ = found_tx.send((second, tab_id));
                    return;
                }
            });
            let (second, seen_tab) = found_rx
                .recv_timeout(PATIENCE)
                .expect("the new tab reaches the client that stayed");
            assert_eq!(seen_tab, added_tab);

            (
                vec![caller, first, second],
                (session_id, first_client, second_client, added_tab),
            )
        });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 1);
    assert!(session.clients.get(first_client).is_none());
    assert_eq!(
        session.clients.get(second_client).map(|client| client.id()),
        Some(second_client)
    );
    assert!(session.tabs.contains_key(&added_tab));
}

#[test]
fn detach_all_takes_every_client_and_leaves_the_session_whole() {
    let (server, _fake, (session_id, first_client, second_client, tab_id, pane_id)) =
        served("detach-all", |dir, session_id, _fake| {
            let first = {
                let mut connection = open(&dir, session_id);
                let (client_id, _, structure) = attach(&mut connection, 2);
                (connection, client_id, structure)
            };
            let mut second = open(&dir, session_id);
            let (second_client, _, _) = attach(&mut second, 2);

            let mut caller = open(&dir, session_id);
            assert_eq!(attached_client_count(&mut caller, 3), 2);

            let (first, first_client, structure) = first;
            let emitted = submit(&mut caller, session_id, Command::DetachAll, 4);
            assert_eq!(emitted, Vec::new());

            // Every attached client's stream ends with the same goodbye.
            let (first, first_frames) = read_to_goodbye(first);
            let (second, second_frames) = read_to_goodbye(second);
            assert_eq!(first_frames.last(), Some(&SessionEvent::Detached));
            assert_eq!(second_frames.last(), Some(&SessionEvent::Detached));
            wait_for_client_count(&mut caller, 0, 5);

            // The session with nobody watching still holds its tab and pane.
            let after = overview(&mut caller, 100);
            assert_eq!(after.session.id, session_id);
            assert_eq!(
                after.tabs.iter().map(|tab| tab.id).collect::<Vec<TabId>>(),
                vec![structure.tabs[0].id],
            );
            assert_eq!(
                after
                    .panes
                    .iter()
                    .map(|pane| pane.id)
                    .collect::<Vec<PaneId>>(),
                vec![structure.panes[0].id],
            );

            (
                vec![caller, first, second],
                (
                    session_id,
                    first_client,
                    second_client,
                    structure.tabs[0].id,
                    structure.panes[0].id,
                ),
            )
        });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 0);
    assert!(session.clients.get(first_client).is_none());
    assert!(session.clients.get(second_client).is_none());
    assert!(session.tabs.contains_key(&tab_id));
    assert_eq!(
        session.panes.get(pane_id).map(|pane| pane.id()),
        Some(pane_id)
    );
}

#[test]
fn detaching_the_smaller_client_grows_the_tabs_pty_back() {
    // Half the columns of [`VIEWPORT`], which the session's pane spawned at.
    const NARROW: Size = Size { cols: 40, rows: 24 };

    let (_server, fake, pane_id) = served("detach-reflow", |dir, session_id, _fake| {
        // The narrow client attaches first and holds the tab down: the
        // effective tab size is the smallest viewport of every client viewing
        // it, so the pane's PTY shrinks.
        let mut narrow = open(&dir, session_id);
        let (narrow_client, _, structure) = attach_sized(&mut narrow, 2, NARROW);
        let pane_id = structure.panes[0].id;

        let mut wide = open(&dir, session_id);
        attach_sized(&mut wide, 2, VIEWPORT);

        let mut caller = open(&dir, session_id);
        assert_eq!(attached_client_count(&mut caller, 3), 2);

        // The narrow client leaves; only the full-width viewer is left, so the
        // tab grows back and the pane's PTY reflows with it.
        submit(
            &mut caller,
            session_id,
            Command::Detach(DetachArgs {
                client: Some(narrow_client),
            }),
            4,
        );
        let (narrow, frames) = read_to_goodbye(narrow);
        assert_eq!(frames.last(), Some(&SessionEvent::Detached));
        wait_for_client_count(&mut caller, 1, 5);

        (vec![caller, narrow, wide], pane_id)
    });

    // Three resizes, in this order: the size the seeded session gave the pane,
    // down by the difference in viewport width when the narrow client joined,
    // and back to the first size when it left. The rows never changed.
    let resizes = fake.resizes(pane_id).expect("the pane was spawned");
    let full = resizes[0];
    assert_eq!(
        resizes,
        vec![
            full,
            PtySize {
                cols: full.cols - (VIEWPORT.cols - NARROW.cols),
                rows: full.rows,
            },
            full,
        ],
    );
}

#[test]
fn dropping_a_smaller_client_connection_grows_the_tabs_pty_back() {
    // Half the columns of [`VIEWPORT`], which the session's pane spawned at.
    const NARROW: Size = Size { cols: 40, rows: 24 };

    let (_server, fake, pane_id) = served("drop-reflow", |dir, session_id, _fake| {
        // The narrow client attaches first and holds the tab down: the
        // effective tab size is the smallest viewport of every client viewing
        // it, so the pane's PTY shrinks.
        let mut narrow = open(&dir, session_id);
        let (_narrow_client, _, structure) = attach_sized(&mut narrow, 2, NARROW);
        let pane_id = structure.panes[0].id;

        let mut wide = open(&dir, session_id);
        attach_sized(&mut wide, 3, VIEWPORT);

        let mut caller = open(&dir, session_id);
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
    let resizes = fake.resizes(pane_id).expect("the pane was spawned");
    let full = resizes[0];
    assert_eq!(
        resizes,
        vec![
            full,
            PtySize {
                cols: full.cols - (VIEWPORT.cols - NARROW.cols),
                rows: full.rows,
            },
            full,
        ],
        "connection drop triggers immediate PTY reconciliation"
    );
}

#[test]
fn an_attached_client_types_into_its_pane_and_resizes_the_tab_it_views() {
    // Smaller than [`VIEWPORT`] on both axes, so this one client's report is
    // the smallest of every client viewing the tab and the tab follows it.
    const RESIZED: Size = Size { cols: 60, rows: 20 };

    // `<C-a>` reaches the pane as the ASCII SOH byte.
    const TYPED: KeyChord = KeyChord::new(ModFlags::CTRL, Key::Char('a'));
    const TYPED_BYTES: &[u8] = &[0x01];

    let (_server, fake, pane_id) = served("types-and-resizes", |dir, session_id, _fake| {
        let mut viewer = open(&dir, session_id);
        let (client_id, replied_session, structure) = attach(&mut viewer, 2);
        assert_eq!(replied_session, session_id);
        let tab_id = structure.tabs[0].id;
        let pane_id = structure.panes[0].id;

        // The attach records no focused pane, so the pane this client types
        // into is named over a second connection, which never attaches.
        let mut caller = open(&dir, session_id);
        submit(
            &mut caller,
            session_id,
            Command::FocusPane(FocusPaneArgs {
                target: FocusTarget::Pane(pane_id),
                client: Some(client_id),
            }),
            3,
        );

        // The first frame the session composes for this client, drawn at the
        // size the attach reported.
        let (mut viewer, frames) = read_frames_until(viewer, |frame| {
            matches!(frame, SessionEvent::Painted { .. })
        });
        let painted = last_painted(&frames);
        assert_eq!(painted.client.id, client_id);
        assert_eq!(painted.session.id, session_id);
        assert_eq!(painted.client.viewport, VIEWPORT);
        assert_eq!(painted.client.active_tab, tab_id);
        assert_eq!(painted.session.active_tab.id, tab_id);
        assert_eq!(painted.client.focused_pane, Some(pane_id));

        // Both requests travel up this one connection, so the dispatcher reads
        // them in this order: the press has reached the pane by the time the
        // resized frame is composed.
        viewer
            .send(&IpcRequest {
                request_id: 4,
                kind: IpcRequestKind::KeyPress { chord: TYPED },
            })
            .expect("send key press");
        viewer
            .send(&IpcRequest {
                request_id: 5,
                kind: IpcRequestKind::Resize { viewport: RESIZED },
            })
            .expect("send resize");

        let (viewer, frames) = read_frames_until(
            viewer,
            |frame| matches!(frame, SessionEvent::Painted { frame } if frame.client.viewport == RESIZED),
        );
        // Nothing between the two frames drew at a third size.
        for earlier in &frames[..frames.len() - 1] {
            if let SessionEvent::Painted { frame } = earlier {
                assert_eq!(frame.client.viewport, VIEWPORT);
            }
        }
        let painted = last_painted(&frames);
        assert_eq!(painted.client.id, client_id);
        assert_eq!(painted.client.viewport, RESIZED);
        assert_eq!(
            painted.session.active_tab.effective_size,
            pane_viewport(RESIZED),
        );

        // The stream still ends with the goodbye once the client is detached.
        submit(
            &mut caller,
            session_id,
            Command::Detach(DetachArgs {
                client: Some(client_id),
            }),
            6,
        );
        let (viewer, frames) = read_to_goodbye(viewer);
        assert_eq!(frames.last(), Some(&SessionEvent::Detached));
        assert_eq!(
            frames
                .iter()
                .filter(|frame| **frame == SessionEvent::Detached)
                .count(),
            1,
        );

        (vec![caller, viewer], pane_id)
    });

    // The press is the only thing written to the pane, and it arrived encoded.
    assert_eq!(
        fake.writes(pane_id).expect("the pane was spawned"),
        vec![TYPED_BYTES.to_vec()],
    );
}

/// [`read_frames_until`] stopping at the answer to mouse round `request_id`.
fn read_to_mouse_answer(
    connection: Connection,
    request_id: u64,
) -> (Connection, Vec<SessionEvent>) {
    read_frames_until(connection, move |frame| match frame {
        SessionEvent::MouseAnswer {
            request_id: answered,
            answers: _,
        } => *answered == request_id,
        _ => false,
    })
}

/// Every mouse-round answer in `frames`, in the order they arrived: the round
/// each one answers, and what that answer carried.
fn mouse_answers(frames: &[SessionEvent]) -> Vec<(u64, Vec<MouseAnswer>)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            SessionEvent::MouseAnswer {
                request_id,
                answers,
            } => Some((*request_id, answers.clone())),
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
    let (connection, _) = read_frames_until(connection, move |frame| match frame {
        SessionEvent::Painted { frame } => frame
            .panes
            .iter()
            .any(|painted| painted.id == pane && painted.mouse_tracking == MouseTracking::Normal),
        _ => false,
    });
    connection
}

/// Print enough lines in `pane` to push at least `retained` of them into its
/// scrollback, and read `connection`'s stream until a painted frame holds them.
///
/// Returns the line that frame shows on the pane's top row, which is the line a
/// scroll up from here counts back from.
fn fill_scrollback(
    fake: &FakePtyBackend,
    pane: PaneId,
    retained: usize,
    connection: Connection,
) -> (Connection, u64) {
    let lines = retained + usize::from(VIEWPORT.rows);
    fake.push_output(pane, b"x\r\n".repeat(lines))
        .expect("the pane was spawned");
    let (connection, frames) = read_frames_until(connection, move |frame| match frame {
        SessionEvent::Painted { frame } => frame
            .panes
            .iter()
            .any(|painted| painted.id == pane && painted.scrollback.retained_lines >= retained),
        _ => false,
    });
    let top_row = last_painted(&frames)
        .panes
        .iter()
        .find(|painted| painted.id == pane)
        .expect("the pane this client views")
        .view_top_row;
    (connection, top_row)
}

/// One left press with nothing held, at the client cell `at`.
fn press(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

/// A cell above and left of any pane's content, so it clamps into the pane's
/// top-left content cell whatever the chrome around it measures.
const PRESSED: Point = Point { x: 0, y: 0 };

/// The report the program in the pane reads for [`PRESSED`]: a left press at
/// the pane's own column 1, row 1, in the SGR form the pane asked for.
const REPORT: &[u8] = b"\x1b[<0;1;1M";

#[test]
fn an_attached_client_forwards_a_mouse_press_into_its_pane() {
    let (_server, fake, pane_id) = served("mouse-forward", |dir, session_id, fake| {
        let mut viewer = open(&dir, session_id);
        let (_, replied_session, structure) = attach(&mut viewer, 2);
        assert_eq!(replied_session, session_id);
        let pane_id = structure.panes[0].id;

        // The program in the pane asks for mouse reports. Until it has, a
        // forwarded event is written nowhere.
        let mut viewer = wait_for_mouse_tracking(&fake, pane_id, viewer);
        viewer
            .send(&IpcRequest {
                request_id: 4,
                kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                    pane: pane_id,
                    mouse: press(PRESSED),
                }]),
            })
            .expect("send mouse round");

        // The answer says the round ran, so the write it carried has happened.
        let (viewer, _) = read_to_mouse_answer(viewer, 4);

        (vec![viewer], pane_id)
    });

    // The report is the only thing written to the pane, and it arrived encoded.
    assert_eq!(
        fake.writes(pane_id).expect("the pane was spawned"),
        vec![REPORT.to_vec()],
    );
}

#[test]
fn the_answer_to_a_round_that_reports_nothing_still_reaches_the_viewer() {
    // The answer is what releases the viewer's gate: without this frame the
    // viewer's mouse uplink never sends again, since it holds every later round
    // back until the round in flight is answered.
    let (_server, _fake, ()) = served("mouse-answer", |dir, session_id, fake| {
        let mut viewer = open(&dir, session_id);
        let (_, _, structure) = attach(&mut viewer, 2);
        let pane_id = structure.panes[0].id;

        let mut viewer = wait_for_mouse_tracking(&fake, pane_id, viewer);
        viewer
            .send(&IpcRequest {
                request_id: 4,
                kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                    pane: pane_id,
                    mouse: press(PRESSED),
                }]),
            })
            .expect("send mouse round");

        // A forward has nothing to report, and the round is answered anyway.
        let (viewer, frames) = read_to_mouse_answer(viewer, 4);
        assert_eq!(mouse_answers(&frames), vec![(4, Vec::new())]);

        (vec![viewer], ())
    });
}

#[test]
fn a_scroll_round_answers_with_the_pane_and_the_line_its_view_landed_on() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED: usize = 40;
    const LINES: usize = 5;

    let (_server, _fake, ()) = served("mouse-scroll", |dir, session_id, fake| {
        let mut viewer = open(&dir, session_id);
        let (_, _, structure) = attach(&mut viewer, 2);
        let pane_id = structure.panes[0].id;

        let (mut viewer, top_row) = fill_scrollback(&fake, pane_id, RETAINED, viewer);
        viewer
            .send(&IpcRequest {
                request_id: 4,
                kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                    pane: pane_id,
                    up: true,
                    lines: LINES,
                }]),
            })
            .expect("send mouse round");

        // The view landed five lines above the line it was showing.
        let (viewer, frames) = read_to_mouse_answer(viewer, 4);
        assert_eq!(
            mouse_answers(&frames),
            vec![(
                4,
                vec![MouseAnswer::Scrolled {
                    pane: pane_id,
                    top: Some(top_row - LINES as u64),
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
    const ASKED: u16 = 200;
    const APPLIED: u16 = 36;

    let (_server, _fake, ()) = served("mouse-border", |dir, session_id, _fake| {
        let mut viewer = open(&dir, session_id);
        let (client_id, _, structure) = attach(&mut viewer, 2);
        let pane_id = structure.panes[0].id;

        // The neighbour whose room the border move eats into, split off the
        // client's own pane over a second connection, which never attaches.
        let mut caller = open(&dir, session_id);
        submit(
            &mut caller,
            session_id,
            Command::NewPane(NewPaneArgs {
                source: Some(pane_id),
                tab: None,
                direction: Direction::Right,
                stacked: false,
                cwd: None,
                command: None,
                client: Some(client_id),
            }),
            3,
        );

        viewer
            .send(&IpcRequest {
                request_id: 4,
                kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                    pane: pane_id,
                    side: Direction::Right,
                    step: 1,
                    count: ASKED,
                }]),
            })
            .expect("send mouse round");

        let (viewer, frames) = read_to_mouse_answer(viewer, 4);
        assert_eq!(
            mouse_answers(&frames),
            vec![(
                4,
                vec![MouseAnswer::Resized {
                    pane: pane_id,
                    side: Direction::Right,
                    step: 1,
                    applied: APPLIED,
                }]
            )],
        );

        (vec![caller, viewer], ())
    });
}

#[test]
fn one_round_runs_every_action_it_holds_and_is_answered_once() {
    // Lines of history to print, and how far up the round scrolls.
    const RETAINED: usize = 40;
    const LINES: usize = 5;

    let (server, fake, (session_id, client_id, tab_id, pane_id)) =
        served("mouse-round", |dir, session_id, fake| {
            let mut viewer = open(&dir, session_id);
            let (client_id, _, structure) = attach(&mut viewer, 2);
            let tab_id = structure.tabs[0].id;
            let pane_id = structure.panes[0].id;

            // Splitting the pane this client attached on moves its focus to the
            // new pane, so the pane the round asks for is not the one already
            // focused. The split is made over a second connection, which never
            // attaches.
            let mut caller = open(&dir, session_id);
            submit(
                &mut caller,
                session_id,
                Command::NewPane(NewPaneArgs {
                    source: Some(pane_id),
                    tab: None,
                    direction: Direction::Right,
                    stacked: false,
                    cwd: None,
                    command: None,
                    client: Some(client_id),
                }),
                3,
            );

            let viewer = wait_for_mouse_tracking(&fake, pane_id, viewer);
            let (mut viewer, top_row) = fill_scrollback(&fake, pane_id, RETAINED, viewer);

            viewer
                .send(&IpcRequest {
                    request_id: 4,
                    kind: IpcRequestKind::Mouse(vec![
                        WireMouseAction::Command(Box::new(Command::FocusPane(FocusPaneArgs {
                            target: FocusTarget::Pane(pane_id),
                            client: Some(client_id),
                        }))),
                        WireMouseAction::Scroll {
                            pane: pane_id,
                            up: true,
                            lines: LINES,
                        },
                        WireMouseAction::Forward {
                            pane: pane_id,
                            mouse: press(PRESSED),
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
                    kind: IpcRequestKind::Mouse(Vec::new()),
                })
                .expect("send empty mouse round");
            let (viewer, rest) = read_to_mouse_answer(viewer, 5);
            frames.extend(rest);

            // One answer for the round, holding the scroll alone: the focus
            // command and the forward each report nothing.
            assert_eq!(
                mouse_answers(&frames),
                vec![
                    (
                        4,
                        vec![MouseAnswer::Scrolled {
                            pane: pane_id,
                            top: Some(top_row - LINES as u64),
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

    // The command the round carried went through the session's command door:
    // the focus the split had moved away is back on the round's pane.
    let session = server.sessions().get(&session_id).expect("session running");
    let client = session.clients.get(client_id).expect("the viewing client");
    assert_eq!(client.focused_pane(tab_id), Some(pane_id));

    // And the forward the round carried reached the pane, encoded, once.
    assert_eq!(
        fake.writes(pane_id).expect("the pane was spawned"),
        vec![REPORT.to_vec()],
    );
}

#[test]
fn two_rounds_sent_back_to_back_are_answered_in_the_order_they_were_sent() {
    // Lines of history to print, then how far up each round scrolls.
    const RETAINED: usize = 40;
    const FIRST: usize = 2;
    const SECOND: usize = 3;

    let (_server, _fake, ()) = served("mouse-order", |dir, session_id, fake| {
        let mut viewer = open(&dir, session_id);
        let (_, _, structure) = attach(&mut viewer, 2);
        let pane_id = structure.panes[0].id;

        let (mut viewer, top_row) = fill_scrollback(&fake, pane_id, RETAINED, viewer);
        for (request_id, lines) in [(4, FIRST), (5, SECOND)] {
            viewer
                .send(&IpcRequest {
                    request_id,
                    kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                        pane: pane_id,
                        up: true,
                        lines,
                    }]),
                })
                .expect("send mouse round");
        }

        // Both rounds moved the same view, so the lines they answer with name
        // the order they ran in: two lines up, then three more.
        let (viewer, frames) = read_to_mouse_answer(viewer, 5);
        assert_eq!(
            mouse_answers(&frames),
            vec![
                (
                    4,
                    vec![MouseAnswer::Scrolled {
                        pane: pane_id,
                        top: Some(top_row - FIRST as u64),
                    }],
                ),
                (
                    5,
                    vec![MouseAnswer::Scrolled {
                        pane: pane_id,
                        top: Some(top_row - (FIRST + SECOND) as u64),
                    }],
                ),
            ],
        );

        (vec![viewer], ())
    });
}
