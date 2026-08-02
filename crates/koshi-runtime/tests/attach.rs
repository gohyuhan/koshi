//! Integration cover for attaching over a real control socket: what one
//! attach registers, what the reply carries and when it is written, what a
//! second attach sees, what strict decoding refuses, what a detach leaves
//! behind for the clients that stay and for the panes, and what a dropped
//! connection leaves behind.
//!
//! Each test runs the shape the per-session server process runs in: a headless
//! session seeded with no client, its inbox drained on the thread that owns
//! the server, and the socket answered by the real accept loop. The exchange
//! with the socket runs on its own thread, since the caller and the dispatcher
//! must both be live for a request to be answered.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    CloseTabArgs, Command, CommandEnvelope, CommandResult, CommandSource, DetachArgs, NewTabArgs,
};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::Event;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::process::PtySize;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_session::client::{AuthorityTier, ClientOrigin};
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
/// while this thread drains the runtime inbox.
///
/// Returns the server and the fake backend once the exchange is done, so a
/// test can read the session state and the PTY sizes the exchange left behind,
/// plus whatever the exchange itself produced. `exchange` receives the runtime
/// directory and the session id, the two facts it needs to find and open the
/// socket.
///
/// It hands back the connections it wants left open alongside its own value:
/// a connection dropped while the dispatcher is still draining detaches its
/// client, so a test reading the registry keeps its connections here until the
/// dispatcher has stopped.
fn served<T: Send + 'static>(
    tag: &str,
    exchange: impl FnOnce(PathBuf, SessionId) -> (Vec<Connection>, T) + Send + 'static,
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
    let ipc = IpcServer::start(&runtime_dir, session_id, inbox_tx.clone()).expect("start serving");

    let caller_dir = runtime_dir.clone();
    let caller = std::thread::spawn(move || {
        let _stop = StopDispatcher(inbox_tx);
        exchange(caller_dir, session_id)
    });

    loop {
        let Ok(event) = server.inbox_rx().recv() else {
            break;
        };
        if server.handle_runtime_event(event).is_break() {
            break;
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

/// Read `connection`'s event stream until the goodbye frame, on a thread this
/// one can give up waiting on. Returns every frame read, the goodbye last, and
/// the connection so it stays open.
fn read_to_goodbye(mut connection: Connection) -> (Connection, Vec<SessionEvent>) {
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut frames = Vec::new();
        loop {
            let frame: SessionEvent = connection.recv().expect("an event frame");
            let goodbye = frame == SessionEvent::Detached;
            frames.push(frame);
            if goodbye {
                break;
            }
        }
        let _ = done_tx.send((connection, frames));
    });
    done_rx
        .recv_timeout(PATIENCE)
        .expect("the goodbye frame reaches the viewer")
}

#[test]
fn one_attach_registers_the_client_the_server_minted() {
    let (server, _fake, (session_id, client_id, structure)) =
        served("registers", |dir, session_id| {
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
    let (server, _fake, session_id) = served("strict", |dir, session_id| {
        let mut viewer = open(&dir, session_id);

        // A well-framed request naming one field this build does not know.
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

        let reply: IpcResponse = viewer.recv().expect("refusal reply");
        assert_eq!(reply.request_id, None);
        assert_eq!(
            reply.result,
            IpcResult::Error(IpcErrorPayload {
                code: IpcErrorCode::MalformedRequest,
                message: "the bytes received are not a request this build can read".to_string(),
            }),
        );

        // The stream is still aligned, and the refused request registered
        // nothing.
        assert_eq!(attached_client_count(&mut viewer, 3), 0);
        (vec![viewer], session_id)
    });

    let session = server.sessions().get(&session_id).expect("session running");
    assert_eq!(session.clients.len(), 0);
}

#[test]
fn the_structure_reply_is_written_before_the_first_event_frame() {
    let (server, _fake, (session_id, first_tab, second_tab)) =
        served("reply-first", |dir, session_id| {
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
        served("reattach", |dir, session_id| {
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
    let (server, _fake, session_id) = served("quit-ends-stream", |dir, session_id| {
        let mut viewer = open(&dir, session_id);
        let (_, _, structure) = attach(&mut viewer, 2);
        let only_tab = structure.tabs[0].id;

        let mut caller = open(&dir, session_id);
        assert_eq!(attached_client_count(&mut caller, 3), 1);
        close_tab(&mut caller, session_id, only_tab, 4);

        // Reading a frame blocks with no deadline of its own, so the walk to
        // the quit frame runs on a thread this one can give up waiting on.
        // The connection comes back so it stays open below.
        let (quit_tx, quit_rx) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let frame: SessionEvent = viewer.recv().expect("an event frame");
                if matches!(frame, SessionEvent::Quit) {
                    break;
                }
            }
            let _ = quit_tx.send(viewer);
        });
        let viewer = quit_rx
            .recv_timeout(PATIENCE)
            .expect("the quit frame reaches the event stream");

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

#[test]
fn dropping_an_attached_connection_removes_its_client_record() {
    let (server, _fake, (session_id, client_id, tab_id, pane_id)) =
        served("disconnect", |dir, session_id| {
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
        served("detach-one", |dir, session_id| {
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

            // The goodbye is the only frame the first viewer was ever sent,
            // and the last one its stream carries.
            let (first, frames) = read_to_goodbye(first);
            assert_eq!(frames, vec![SessionEvent::Detached]);
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
        served("detach-all", |dir, session_id| {
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

            // Every attached client is sent the same goodbye.
            let (first, first_frames) = read_to_goodbye(first);
            let (second, second_frames) = read_to_goodbye(second);
            assert_eq!(first_frames, vec![SessionEvent::Detached]);
            assert_eq!(second_frames, vec![SessionEvent::Detached]);
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

    let (_server, fake, pane_id) = served("detach-reflow", |dir, session_id| {
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
        assert_eq!(frames, vec![SessionEvent::Detached]);
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
