//! Tests for the control-socket server over real sockets: serving lifecycle,
//! handshake gating, fault containment per connection, the reply path from a
//! stand-in dispatcher thread, and what an attached connection's reading half
//! carries.

use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::SystemTime;

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, ToggleLockModeArgs,
};
use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_core::geometry::Size;
use koshi_core::ids::{CommandId, PaneId, SessionId, TabId};
use koshi_core::key::{Key, KeyChord, ModFlags};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, WireMouseAction, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};

use crate::runtime::event::{AttachAccepted, EndingNotice, SessionEnding};

use super::*;

/// The terminal size every attaching client in these tests reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The secret every stand-in attach mints, so an assertion names the exact
/// token the reply carries.
const MINTED_TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it private itself.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    base.join(format!("koshi-serve-{}-{tag}", std::process::id()))
}

/// Remove a test's runtime dir once it is done with it.
fn cleanup(runtime_dir: &Path) {
    let _ = std::fs::remove_dir_all(runtime_dir);
}

/// A fresh directory to stand in for the machine-wide shared directory, under
/// a short base so the Unix socket path stays inside the OS path-length cap.
/// [`IpcServer::start`] creates it and this user's directory inside it.
fn test_shared_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    base.join(format!("koshi-shared-{}-{tag}", std::process::id()))
}

/// A stand-in for the dispatcher thread: drains the inbox, answers every
/// submitted command with `Ok` echoing its id, and every discovery request
/// with `overview`. Exits when every inbox sender is gone.
fn spawn_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::Ipc { envelope, reply } => {
                    let _ = reply.send(CommandResult::Ok {
                        command_id: envelope.id,
                        emitted_events: Vec::new(),
                    });
                }
                RuntimeEvent::IpcDiscovery { reply } => {
                    let _ = reply.send(overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// The structure a stand-in attach answers with: the session, named, with
/// nothing in it.
fn attached_structure(session_id: SessionId) -> AttachedSessionStructureSnapshot {
    AttachedSessionStructureSnapshot {
        id: session_id,
        name: "attachable".to_string(),
        tabs: Vec::new(),
        panes: Vec::new(),
    }
}

/// A stand-in dispatcher that accepts attaches: it answers every attach as
/// `client_id`, holds the queue it hands out open so the writing thread stays
/// blocked, and closes those queues on a detach the way the real dispatcher
/// does. Every other event it drains is forwarded to the returned receiver, so
/// a test reads exactly what an attached connection sent. Exits when every
/// inbox sender is gone.
fn spawn_attaching_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
) -> (JoinHandle<()>, Receiver<RuntimeEvent>) {
    let (seen_tx, seen_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut queues = Vec::new();
        let ending_notice = Arc::new(EndingNotice::default());
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::IpcAttach { reply, .. } => {
                    let (events_tx, events_rx) = mpsc::channel();
                    queues.push(events_tx);
                    let _ = reply.send(Some(AttachAccepted {
                        client_id,
                        session_id,
                        structure: attached_structure(session_id),
                        events: events_rx,
                        goodbye: Arc::default(),
                        ending_notice: Arc::clone(&ending_notice),
                        resume_token: ConnectionToken::new(MINTED_TOKEN),
                    }));
                }
                detached @ RuntimeEvent::ClientDetached { .. } => {
                    queues.clear();
                    if seen_tx.send(detached).is_err() {
                        break;
                    }
                }
                other => {
                    if seen_tx.send(other).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (handle, seen_rx)
}

/// A stand-in dispatcher that answers the first attach with `events` and
/// `ending_notice`, and drops everything else it drains. Exits when every
/// inbox sender is gone.
fn spawn_ending_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    client_id: ClientId,
    session_id: SessionId,
    events: Receiver<Delivery>,
    goodbye: Arc<GoodbyeNotice>,
    ending_notice: Arc<EndingNotice>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut queue = Some(events);
        while let Ok(event) = inbox_rx.recv() {
            if let RuntimeEvent::IpcAttach { reply, .. } = event {
                let Some(events) = queue.take() else {
                    continue;
                };
                let _ = reply.send(Some(AttachAccepted {
                    client_id,
                    session_id,
                    structure: attached_structure(session_id),
                    events,
                    goodbye: Arc::clone(&goodbye),
                    ending_notice: Arc::clone(&ending_notice),
                    resume_token: ConnectionToken::new(MINTED_TOKEN),
                }));
            }
        }
    })
}

/// Wait until no client writing thread is left on `notice`, and hand back how
/// many are. Fails the test rather than hanging if one never ends.
fn wait_for_writers_to_end(notice: &EndingNotice) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while notice.writers_running() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    notice.writers_running()
}

#[test]
fn a_client_whose_queue_is_full_is_still_told_the_session_is_restarting() {
    // A client's queue is bounded and the restart is published onto it like any
    // other event, so a client whose queue is full never takes it.
    // That client would read end of stream when the image is replaced and
    // report the session dead, instead of coming back on its new socket.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter, SUBSCRIBER_QUEUE_CAPACITY};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("restart-full-queue");

    let mut bus = EventBus::new();
    let (_, events) = bus.subscribe(EventFilter::All);
    let tab = TabId::new();
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    // The announcement: publishing the restart raises the notice and puts the
    // event on a queue with no room left for it.
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Restarting);
    assert_eq!(notice.raised(), Some(SessionEnding::Restarting));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_ending_dispatcher(
        inbox_rx,
        client,
        session,
        events,
        Arc::default(),
        Arc::clone(&notice),
    );
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_dir, session, client);

    // The first frame, not a frame somewhere behind the backlog: the queue
    // still holds its full 1024 deliveries, and none of them is written.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Restarting,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the restart frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_client_whose_queue_is_full_is_still_told_the_session_ended() {
    // A client's queue is bounded and the quit is published onto it like any
    // other event, so a client whose queue is full never takes it. That client
    // would read end of stream when the session ends and report it dead,
    // instead of saying the session ended.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter, SUBSCRIBER_QUEUE_CAPACITY};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("quit-full-queue");

    let mut bus = EventBus::new();
    let (_, events) = bus.subscribe(EventFilter::All);
    let tab = TabId::new();
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    // The announcement: publishing the quit raises the notice and puts the
    // event on a queue with no room left for it.
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.raised(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_ending_dispatcher(
        inbox_rx,
        client,
        session,
        events,
        Arc::default(),
        Arc::clone(&notice),
    );
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_dir, session, client);

    // The first frame, not a frame somewhere behind the backlog: the queue
    // still holds its full 1024 deliveries, and none of them is written.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Quit,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the quit frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_client_the_server_detached_reads_its_own_goodbye_when_the_session_ends() {
    // `auto-close-session` ends the session the moment its last client
    // detaches, so the notice is raised while that client's writing thread
    // still holds frames to write. That client asked to leave, so the detach is
    // what it reads.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("detach-then-quit");

    let mut bus = EventBus::new();
    let (subscriber, events) = bus.subscribe(EventFilter::All);
    bus.publish(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    // The detach closes the queue behind the frame it already holds; the
    // session ends right after.
    bus.unsubscribe(subscriber);
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.raised(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_ending_dispatcher(
        inbox_rx,
        client,
        session,
        events,
        Arc::default(),
        Arc::clone(&notice),
    );
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_dir, session, client);

    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Detached,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the detach frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_refused_client_reads_the_refusal_as_the_one_frame_that_ends_its_stream() {
    // The session keeps running: the refusal closes this client's queue alone.
    // Its writing thread writes what was already queued, then the goodbye frame
    // the refusal named, and ends.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter};

    let client = ClientId::new();
    let session = SessionId::new();
    let tab = TabId::new();
    let runtime_dir = test_runtime_dir("refuse-while-running");

    let mut bus = EventBus::new();
    let (subscriber, events) = bus.subscribe(EventFilter::All);
    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    let goodbye: Arc<GoodbyeNotice> = Arc::default();
    goodbye.refuse_host_only();
    bus.unsubscribe(subscriber);
    let notice = Arc::clone(bus.ending_notice());
    assert_eq!(notice.raised(), None, "the session keeps running");

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_ending_dispatcher(
        inbox_rx,
        client,
        session,
        events,
        Arc::clone(&goodbye),
        Arc::clone(&notice),
    );
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_dir, session, client);

    // What was queued before the close still arrives, and the refusal is the
    // frame that ends the stream.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the queued frame arrives"),
        SessionEvent::TabCreated { tab_id: tab },
    );
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::HostOnlyRefusal,
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_refused_client_reads_the_refusal_even_when_the_session_ends_in_the_same_turn() {
    // The refused client was the session's last one and `auto-close-session` is
    // set, so the notice is raised in the same turn the detach closes the
    // queue. The writing thread drops what is queued and writes this client's
    // goodbye frame, which names the refusal.
    use koshi_core::event::{Event, TabCreated};

    use crate::runtime::bus::{EventBus, EventFilter};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("refuse-then-quit");

    let mut bus = EventBus::new();
    let (subscriber, events) = bus.subscribe(EventFilter::All);
    bus.publish(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    let goodbye: Arc<GoodbyeNotice> = Arc::default();
    goodbye.refuse_host_only();
    bus.unsubscribe(subscriber);
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.raised(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_ending_dispatcher(
        inbox_rx,
        client,
        session,
        events,
        Arc::clone(&goodbye),
        Arc::clone(&notice),
    );
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_dir, session, client);

    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::HostOnlyRefusal,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the refusal frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_client_reads_the_quit_frame_alone_when_the_events_that_ended_the_session_are_still_queued() {
    // The events that end a session and the quit itself are published in one
    // pass, so a client's queue can hold both when its writing thread takes its
    // first turn. The raised notice is what that thread writes, whatever the
    // queue still holds: the pane's exit is queued ahead of the quit here, and
    // the client reads the quit alone.
    use koshi_core::event::{Event, PaneProcessExited};

    use crate::runtime::bus::{EventBus, EventFilter};

    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("quit-behind-queue");

    let mut bus = EventBus::new();
    let (_, events) = bus.subscribe(EventFilter::All);
    let pane = PaneId::new();
    bus.publish(&Event::PaneProcessExited(PaneProcessExited {
        pane_id: pane,
        exit_code: Some(0),
    }));
    // The announcement: the queue has room, so the quit is queued behind the
    // exit and the notice is raised as well.
    let notice = Arc::clone(bus.ending_notice());
    bus.publish(&Event::Quit);
    assert_eq!(notice.raised(), Some(SessionEnding::Quit));

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_ending_dispatcher(
        inbox_rx,
        client,
        session,
        events,
        Arc::default(),
        Arc::clone(&notice),
    );
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");

    let mut connection = attach_to(&runtime_dir, session, client);

    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the client is told"),
        SessionEvent::Quit,
    );
    assert_eq!(
        wait_for_writers_to_end(&notice),
        0,
        "the writing thread must end once the client holds the quit frame"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// A served socket whose stand-in dispatcher accepts an attach as `client_id`,
/// plus the events that attached connection sends the dispatcher.
fn serve_attachable(
    tag: &str,
    client_id: ClientId,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<RuntimeEvent>,
) {
    let runtime_dir = test_runtime_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client_id, session);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_dir, dispatcher, seen)
}

/// Open a connection, say hello, attach on it, and read both replies back.
/// The connection comes back carrying `client_id`'s stream.
fn attach_to(runtime_dir: &Path, session: SessionId, client_id: ClientId) -> Connection {
    let mut connection = connect_to(runtime_dir, session);
    connection
        .send(&hello_for(runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Attach {
                viewport: VIEWPORT,
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
            },
        })
        .expect("send attach");
    let attach_reply: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attach_reply.request_id, Some(2));
    assert_eq!(
        attach_reply.result,
        IpcResult::Attached {
            client_id,
            session_id: session,
            structure: attached_structure(session),
            resume_token: Some(ConnectionToken::new(MINTED_TOKEN)),
        },
    );
    connection
}

/// A served socket in a fresh runtime dir, with a stand-in dispatcher
/// answering `overview`, plus everything a test needs to talk and clean up.
fn serve(
    tag: &str,
    overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_dir = test_runtime_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, overview);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_dir, dispatcher)
}

/// A served socket the other local users of this machine may reach, in a fresh
/// shared directory, with a stand-in dispatcher and the `allow-other-users`
/// setting reading `still_on`.
fn serve_shared(
    tag: &str,
    still_on: bool,
) -> (IpcServer, SessionId, PathBuf, PathBuf, JoinHandle<()>) {
    let runtime_dir = test_runtime_dir(tag);
    let shared_dir = test_shared_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, None);
    let server = IpcServer::start(
        &runtime_dir,
        session,
        inbox_tx,
        Some(OtherUsers {
            shared_dir: shared_dir.clone(),
            still_on: Arc::new(move || still_on),
        }),
    )
    .expect("start serving");
    (server, session, runtime_dir, shared_dir, dispatcher)
}

/// A deterministic envelope for submissions.
fn envelope() -> CommandEnvelope {
    CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Internal,
        SystemTime::UNIX_EPOCH,
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    )
}

/// The Hello that matches the endpoint file at `runtime_dir` for `session`.
fn hello_for(runtime_dir: &Path, session: SessionId) -> IpcRequest {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session))
        .expect("endpoint file readable");
    IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: endpoint.token,
            remote: false,
        },
    }
}

/// The answer an accepted Hello earns: both sides speak this build's version,
/// so they settle on it, and the answer names the build the session runs.
fn hello_accepted() -> IpcResult {
    IpcResult::Hello {
        protocol_version: PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Connect to the socket the endpoint file at `runtime_dir` advertises.
fn connect_to(runtime_dir: &Path, session: SessionId) -> Connection {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session))
        .expect("endpoint file readable");
    Connection::connect(&endpoint.socket).expect("connect")
}

/// A stand-in dispatcher answering every layout request with `layout`. The
/// returned receiver carries the tab each request named, so a test reads what
/// crossed the boundary. Exits when every inbox sender is gone.
fn spawn_layout_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    layout: Option<SessionLayout>,
) -> (JoinHandle<()>, Receiver<Option<TabId>>) {
    let (asked_tx, asked_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            if let RuntimeEvent::IpcLayout { tab, reply } = event {
                let _ = asked_tx.send(tab);
                let _ = reply.send(layout.clone());
            }
        }
    });
    (handle, asked_rx)
}

/// A served socket whose stand-in dispatcher answers layout requests with
/// `layout`, plus the tab each request named.
fn serve_layout(
    tag: &str,
    layout: Option<SessionLayout>,
) -> (
    IpcServer,
    SessionId,
    PathBuf,
    JoinHandle<()>,
    Receiver<Option<TabId>>,
) {
    let runtime_dir = test_runtime_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, asked) = spawn_layout_dispatcher(inbox_rx, layout);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_dir, dispatcher, asked)
}

/// A tiny layout to answer a layout request with, distinguishable by its name.
fn layout_named(name: &str) -> SessionLayout {
    SessionLayout {
        id: SessionId::new(),
        name: name.to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    }
}

/// A tiny overview to answer discovery with, distinguishable by its name.
fn overview_named(name: &str) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: SessionId::new(),
            name: name.to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    }
}

#[test]
fn a_submitted_command_round_trips_with_the_dispatchers_result() {
    let (server, session, runtime_dir, dispatcher) = serve("roundtrip", None);
    let mut connection = connect_to(&runtime_dir, session);
    let env = envelope();

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::SubmitCommand(Box::new(env.clone())),
        })
        .expect("send submit");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.request_id, Some(1));
    assert_eq!(hello_reply.result, hello_accepted());

    let submit_reply: IpcResponse = connection.recv().expect("submit reply");
    assert_eq!(submit_reply.request_id, Some(2));
    assert_eq!(
        submit_reply.result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: env.id,
            emitted_events: Vec::new(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// A newer koshi asking for something this build has no name for is refused by
/// name, and the caller keeps every other verb on the same connection. Killing
/// the connection instead would cost a caller its whole CLI surface for one
/// unfamiliar request.
#[test]
fn a_request_kind_this_build_lacks_is_refused_by_name_and_the_connection_keeps_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("unknown-kind", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    // A well-framed request naming a kind added by some later koshi.
    connection
        .send(&serde_json::json!({
            "request_id": 2,
            "kind": { "Floating": { "pane": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have");

    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(2));
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedKind,
            message: "this Koshi has no request kind named Floating".to_string(),
        }),
    );

    // The connection is still open and still serving: the next request is
    // answered normally.
    let env = envelope();
    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::SubmitCommand(Box::new(env.clone())),
        })
        .expect("send a command after the refusal");
    let after: IpcResponse = connection.recv().expect("command reply");
    assert_eq!(after.request_id, Some(3));
    assert_eq!(
        after.result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: env.id,
            emitted_events: Vec::new(),
        }),
        "the verb after the refusal was served normally"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// An unfamiliar kind arriving before the Hello is answered the same way any
/// other kind is: the gate is closed, so the caller learns nothing about which
/// kinds this build has.
#[test]
fn a_kind_this_build_lacks_before_hello_is_refused_as_hello_required() {
    let (server, session, runtime_dir, dispatcher) = serve("unknown-kind-early", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&serde_json::json!({
            "request_id": 9,
            "kind": { "Floating": { "pane": "00000000-0000-0000-0000-000000000001" } }
        }))
        .expect("send a kind this build does not have, before the hello");

    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(9));
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Floating arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// A caller reaching higher than this build settles on this build's highest,
/// and the connection serves from there.
#[test]
fn a_caller_speaking_a_wider_range_settles_on_this_builds_highest() {
    let (server, session, runtime_dir, dispatcher) = serve("wider-range", None);
    let mut connection = connect_to(&runtime_dir, session);
    let endpoint = EndpointFile::read(&EndpointFile::path(&runtime_dir, session))
        .expect("endpoint file readable");

    connection
        .send(&IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION + 5,
                token: endpoint.token,
                remote: false,
            },
        })
        .expect("send a hello reaching above this build");

    let reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(
        reply.result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        "the answer names the highest version both sides speak"
    );

    let env = envelope();
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::SubmitCommand(Box::new(env.clone())),
        })
        .expect("send a command");
    let after: IpcResponse = connection.recv().expect("command reply");
    assert_eq!(
        after.result,
        IpcResult::CommandResult(CommandResult::Ok {
            command_id: env.id,
            emitted_events: Vec::new(),
        }),
        "the gate opened and the connection serves at the settled version"
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// A caller whose whole range sits above this build shares no version with it,
/// so the connection is refused naming both ranges and no verb is served.
#[test]
fn a_caller_sharing_no_version_is_refused_and_serves_nothing() {
    let (server, session, runtime_dir, dispatcher) = serve("no-shared-version", None);
    let mut connection = connect_to(&runtime_dir, session);
    let endpoint = EndpointFile::read(&EndpointFile::path(&runtime_dir, session))
        .expect("endpoint file readable");
    let above = PROTOCOL_VERSION + 1;

    connection
        .send(&IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                min_protocol_version: above,
                max_protocol_version: above + 2,
                token: endpoint.token,
                remote: false,
            },
        })
        .expect("send a hello sharing no version");

    let reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(
        reply.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::UnsupportedVersion,
            message: format!(
                "the caller speaks protocol versions {above} to {}, \
                 this Koshi speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION}",
                above + 2
            ),
        }),
    );

    // The refusal left the gate closed, so the session is untouched.
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery after the refusal");
    let after: IpcResponse = connection.recv().expect("second reply");
    assert_eq!(
        after.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_request_before_hello_is_refused_and_the_connection_keeps_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("hello-first", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&IpcRequest {
            request_id: 7,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope())),
        })
        .expect("send submit before hello");
    let refusal: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(refusal.request_id, Some(7));
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "SubmitCommand arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The same connection still serves: a Hello opens it and a submit works.
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_wrong_token_is_refused_as_bad_token() {
    let (server, session, runtime_dir, dispatcher) = serve("bad-token", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                min_protocol_version: MIN_PROTOCOL_VERSION,
                max_protocol_version: PROTOCOL_VERSION,
                token: ConnectionToken::new("not-the-secret"),
                remote: false,
            },
        })
        .expect("send hello");
    let reply: IpcResponse = connection.recv().expect("reply");
    assert_eq!(
        reply.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_restart_advertises_a_fresh_token_and_refuses_the_old_one() {
    let (server, session, runtime_dir, dispatcher) = serve("restart-token", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let first = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    let (inbox_tx, inbox_rx) = mpsc::channel();
    let restarted_dispatcher = spawn_dispatcher(inbox_rx, None);
    let restarted =
        IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving again");
    let second = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    assert_ne!(
        second.token, first.token,
        "the restarted server advertises a new secret",
    );

    let mut old = connect_to(&runtime_dir, session);
    old.send(&IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: first.token,
            remote: false,
        },
    })
    .expect("send hello with the token from before the restart");
    let refusal: IpcResponse = old.recv().expect("reply");
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut fresh = connect_to(&runtime_dir, session);
    fresh
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello with the new secret");
    let accepted: IpcResponse = fresh.recv().expect("hello reply");
    assert_eq!(accepted.result, hello_accepted());

    drop(old);
    drop(fresh);
    restarted.shutdown();
    restarted_dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_detach_leaves_the_sessions_token_unchanged() {
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) = serve_attachable("detach-token", client);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let before = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    let attached = attach_to(&runtime_dir, session, client);
    drop(attached);
    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);

    let after = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    assert_eq!(
        after.token, before.token,
        "the detached client's departure leaves the session's secret alone",
    );

    let mut connection = connect_to(&runtime_dir, session);
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello with the secret from before the detach");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_malformed_frame_is_answered_and_the_connection_keeps_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("malformed", None);
    let mut connection = connect_to(&runtime_dir, session);

    // A well-framed message that is not an `IpcRequest` at all.
    connection.send(&"not a request").expect("send junk frame");
    let reply: IpcResponse = connection.recv().expect("refusal reply");
    assert_eq!(reply.request_id, None);
    assert_eq!(
        reply.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "the bytes received are not a request this build can read".to_string(),
        }),
    );

    // The stream is still aligned: the same connection opens and serves.
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn an_oversize_frame_closes_the_connection() {
    let (server, session, runtime_dir, dispatcher) = serve("oversize", None);
    let endpoint = EndpointFile::read(&EndpointFile::path(&runtime_dir, session))
        .expect("endpoint file readable");

    // A raw stream, so the length prefix can lie past the cap without a
    // payload behind it.
    let mut raw = raw_connect(&endpoint.socket);
    let oversize = (koshi_ipc::transport::MAX_FRAME_LEN + 1).to_be_bytes();
    std::io::Write::write_all(&mut raw, &oversize).expect("write oversize header");

    // The server closes: the next read finds the stream at end.
    let mut buf = [0u8; 1];
    let closed = match std::io::Read::read(&mut raw, &mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(_) => true,
    };
    assert!(
        closed,
        "the connection must be closed after an oversize frame"
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// Open the control socket as a raw byte stream, bypassing the framed
/// [`Connection`], so a test can write a corrupt frame header.
#[cfg(unix)]
fn raw_connect(addr: &str) -> std::os::unix::net::UnixStream {
    std::os::unix::net::UnixStream::connect(addr).expect("raw connect")
}

/// Open the control socket as a raw byte stream, bypassing the framed
/// [`Connection`], so a test can write a corrupt frame header. The bare pipe
/// name is served at `\\.\pipe\<name>`.
#[cfg(windows)]
fn raw_connect(addr: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{addr}"))
        .expect("raw connect")
}

#[test]
fn an_attached_connection_forwards_input_unanswered_and_detaches_on_any_other_request() {
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("attached-input", client);
    let mut connection = attach_to(&runtime_dir, session, client);
    let pressed = KeyChord::new(ModFlags::CTRL, Key::Char('t'));
    let resized = Size {
        cols: 120,
        rows: 40,
    };
    let env = envelope();

    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, pressed);

    connection
        .send(&IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::Resize { viewport: resized },
        })
        .expect("send resize");
    let RuntimeEvent::Resize { client_id, size } = seen.recv().expect("resize event") else {
        panic!("expected Resize");
    };
    assert_eq!(client_id, client);
    assert_eq!(size, resized);

    connection
        .send(&IpcRequest {
            request_id: 5,
            kind: IpcRequestKind::SubmitCommand(Box::new(env.clone())),
        })
        .expect("send submit");
    let RuntimeEvent::Ipc { envelope, reply } = seen.recv().expect("submit event") else {
        panic!("expected Ipc");
    };
    assert_eq!(envelope, env);
    assert!(
        reply
            .send(CommandResult::Ok {
                command_id: env.id,
                emitted_events: Vec::new(),
            })
            .is_err(),
        "the reply channel's receiving end is already gone",
    );

    let round = vec![WireMouseAction::Scroll {
        pane: PaneId::new(),
        up: true,
        lines: 3,
    }];
    connection
        .send(&IpcRequest {
            request_id: 6,
            kind: IpcRequestKind::Mouse(round.clone()),
        })
        .expect("send mouse round");
    let RuntimeEvent::ClientMouse {
        client_id,
        request_id,
        actions,
    } = seen.recv().expect("mouse round event")
    else {
        panic!("expected ClientMouse");
    };
    assert_eq!(client_id, client);
    assert_eq!(request_id, 6, "the round's own id crosses with it");
    assert_eq!(actions, round);

    connection
        .send(&IpcRequest {
            request_id: 7,
            kind: IpcRequestKind::Paste {
                text: String::from("hello\nworld"),
            },
        })
        .expect("send paste");
    let RuntimeEvent::HostPaste { client_id, text } = seen.recv().expect("paste event") else {
        panic!("expected HostPaste");
    };
    assert_eq!(client_id, client);
    assert_eq!(text, "hello\nworld");

    // A kind the reading half does not forward ends it, which detaches.
    connection
        .send(&IpcRequest {
            request_id: 8,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);

    // The goodbye is the first frame after the attach reply, so none of the
    // five requests above was answered with an `IpcResponse`.
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(
        matches!(
            connection.recv::<SessionEvent>(),
            Err(IpcError::Disconnected),
        ),
        "the stream ends after the goodbye",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_mouse_round_before_an_attach_closes_the_connection() {
    // A round names no client until the connection carries one, so it belongs
    // on an attached connection only.
    let (server, session, runtime_dir, dispatcher) = serve("mouse-unattached", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane: PaneId::new(),
                up: true,
                lines: 3,
            }]),
        })
        .expect("send mouse round");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back, and the connection is closed",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn discovery_answers_with_the_dispatchers_overview() {
    let (server, session, runtime_dir, dispatcher) =
        serve("discovery", Some(overview_named("workspace")));
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());
    let discovery_reply: IpcResponse = connection.recv().expect("discovery reply");
    let IpcResult::Overview(overview) = discovery_reply.result else {
        panic!("expected an overview, got {:?}", discovery_reply.result);
    };
    assert_eq!(overview.session.name, "workspace");

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn discovery_with_no_running_session_closes_the_connection() {
    let (server, session, runtime_dir, dispatcher) = serve("discovery-none", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back once no session is running",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_layout_request_answers_with_the_dispatchers_layout_and_names_the_tab_asked_for() {
    let (server, session, runtime_dir, dispatcher, asked) =
        serve_layout("layout-one-tab", Some(layout_named("workspace")));
    let wanted = TabId::new();
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Layout { tab: Some(wanted) },
        })
        .expect("send layout request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());
    let layout_reply: IpcResponse = connection.recv().expect("layout reply");
    assert_eq!(layout_reply.request_id, Some(2));
    let IpcResult::Layout(layout) = layout_reply.result else {
        panic!("expected a layout, got {:?}", layout_reply.result);
    };
    assert_eq!(layout.name, "workspace");
    assert_eq!(
        asked.recv().expect("the dispatcher was asked"),
        Some(wanted)
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_layout_request_for_every_tab_names_no_tab_to_the_dispatcher() {
    let (server, session, runtime_dir, dispatcher, asked) =
        serve_layout("layout-every-tab", Some(layout_named("workspace")));
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Layout { tab: None },
        })
        .expect("send layout request");

    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());
    let layout_reply: IpcResponse = connection.recv().expect("layout reply");
    let IpcResult::Layout(layout) = layout_reply.result else {
        panic!("expected a layout, got {:?}", layout_reply.result);
    };
    assert_eq!(layout.name, "workspace");
    assert_eq!(asked.recv().expect("the dispatcher was asked"), None);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_layout_request_with_no_running_session_closes_the_connection() {
    let (server, session, runtime_dir, dispatcher, _asked) = serve_layout("layout-none", None);
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Layout { tab: None },
        })
        .expect("send layout request");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back once no session is running",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_layout_request_on_an_attached_connection_ends_that_client_stream() {
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("layout-attached", client);
    let mut connection = attach_to(&runtime_dir, session, client);

    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::Layout { tab: None },
        })
        .expect("send layout request");

    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_gone_dispatcher_closes_the_connection_instead_of_answering() {
    let runtime_dir = test_runtime_dir("no-dispatcher");
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    drop(inbox_rx);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope())),
        })
        .expect("send submit");
    assert!(
        connection.recv::<IpcResponse>().is_err(),
        "no reply comes back once the dispatcher is gone",
    );

    drop(connection);
    server.shutdown();
    cleanup(&runtime_dir);
}

#[test]
fn the_endpoint_file_lives_while_serving_and_both_files_go_at_shutdown() {
    let (server, session, runtime_dir, dispatcher) = serve("lifecycle", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    assert!(
        endpoint_path.exists(),
        "endpoint file present while serving"
    );
    assert_eq!(endpoint.pid, std::process::id());
    #[cfg(unix)]
    assert!(
        Path::new(&endpoint.socket).exists(),
        "socket file present while serving",
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after shutdown");
    #[cfg(unix)]
    assert!(
        !Path::new(&endpoint.socket).exists(),
        "socket file gone after shutdown",
    );
    assert!(
        matches!(
            Connection::connect(&endpoint.socket),
            Err(IpcError::NoListener { .. }),
        ),
        "nothing listens after shutdown",
    );
    cleanup(&runtime_dir);
}

#[test]
fn dropping_the_server_without_shutdown_still_removes_both_files() {
    let (server, session, runtime_dir, dispatcher) = serve("drop-cleans", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    drop(server);
    dispatcher.join().expect("dispatcher exits");

    assert!(!endpoint_path.exists(), "endpoint file gone after drop");
    assert!(
        matches!(
            Connection::connect(&endpoint.socket),
            Err(IpcError::NoListener { .. }),
        ),
        "nothing listens after drop",
    );
    cleanup(&runtime_dir);
}

#[cfg(unix)]
#[test]
fn shutdown_returns_and_removes_the_endpoint_even_when_the_wake_cannot_connect() {
    let (server, session, runtime_dir, dispatcher) = serve("wake-fails", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    // Unlink the socket file out from under the listener: the wake connect
    // inside shutdown now fails, so shutdown must skip the join instead of
    // waiting forever on the still-blocked accept loop.
    std::fs::remove_file(&endpoint.socket).expect("unlink the live socket");

    server.shutdown();

    assert!(
        !endpoint_path.exists(),
        "endpoint file gone even though the accept loop could not be woken",
    );
    drop(dispatcher);
    cleanup(&runtime_dir);
}

#[cfg(unix)]
#[test]
fn a_leftover_socket_file_is_reclaimed_at_start() {
    let runtime_dir = test_runtime_dir("reclaim");
    koshi_paths::ensure_private_dir(&runtime_dir).expect("create runtime dir");
    let session = SessionId::new();
    let addr = socket_addr(&runtime_dir, session);
    std::fs::write(&addr, b"").expect("plant a leftover file at the socket path");

    let (inbox_tx, _inbox_rx) = mpsc::channel();
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None)
        .expect("start reclaims the leftover and serves");

    server.shutdown();
    cleanup(&runtime_dir);
}

#[test]
fn a_second_start_on_the_same_session_is_refused_while_serving() {
    let (server, session, runtime_dir, dispatcher) = serve("busy", None);

    let (inbox_tx, _inbox_rx) = mpsc::channel();
    assert!(
        matches!(
            IpcServer::start(&runtime_dir, session, inbox_tx, None),
            Err(IpcError::SocketBusy { .. }),
        ),
        "the live listener must refuse a second bind",
    );

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_session_only_its_own_user_may_reach_binds_inside_the_runtime_directory() {
    let (server, session, runtime_dir, dispatcher) = serve("own-user-socket", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let endpoint = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    assert_eq!(endpoint.socket, socket_addr(&runtime_dir, session));
    assert_eq!(endpoint_path.parent(), Some(runtime_dir.as_path()));

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_session_other_local_users_may_reach_keeps_its_endpoint_file_private() {
    let (server, session, runtime_dir, shared_dir, dispatcher) =
        serve_shared("shared-endpoint", true);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);

    assert_eq!(endpoint_path.parent(), Some(runtime_dir.as_path()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(&endpoint_path)
            .expect("stat endpoint file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
    cleanup(&shared_dir);
}

#[cfg(unix)]
#[test]
fn the_socket_of_a_session_other_local_users_may_reach_is_open_to_every_local_user() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (server, session, runtime_dir, shared_dir, dispatcher) = serve_shared("shared-mode", true);
    let endpoint = EndpointFile::read(&EndpointFile::path(&runtime_dir, session))
        .expect("endpoint file readable");

    // The runtime directory was created by this start, so its owner is the
    // user whose directory under the shared one holds the socket.
    let uid = std::fs::metadata(&runtime_dir)
        .expect("stat runtime dir")
        .uid();
    assert_eq!(
        PathBuf::from(&endpoint.socket),
        shared_dir
            .join(uid.to_string())
            .join(format!("{session}.sock")),
    );
    let mode = std::fs::metadata(&endpoint.socket)
        .expect("stat socket file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o666);

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
    cleanup(&shared_dir);
}

#[cfg(windows)]
#[test]
fn the_marker_naming_a_shared_session_lives_while_serving_and_goes_at_shutdown() {
    let (server, session, runtime_dir, shared_dir, dispatcher) =
        serve_shared("shared-marker", true);
    let marker = advert_path(&shared_dir, session);

    assert!(marker.exists(), "marker present while serving");

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");

    assert!(!marker.exists(), "marker gone after shutdown");
    cleanup(&runtime_dir);
    cleanup(&shared_dir);
}

#[test]
fn the_user_who_started_the_session_attaches_over_the_shared_socket_with_the_token() {
    let runtime_dir = test_runtime_dir("shared-attach");
    let shared_dir = test_shared_dir("shared-attach");
    let session = SessionId::new();
    let client = ClientId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, _seen) = spawn_attaching_dispatcher(inbox_rx, client, session);
    let server = IpcServer::start(
        &runtime_dir,
        session,
        inbox_tx,
        Some(OtherUsers {
            shared_dir: shared_dir.clone(),
            still_on: Arc::new(|| true),
        }),
    )
    .expect("start serving");

    let connection = attach_to(&runtime_dir, session, client);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
    cleanup(&shared_dir);
}

// --- Serving one connection from another local user ---

/// A control-socket address unique to this test: a file path under the short
/// temporary base on Unix, a pipe name on Windows.
fn test_addr(tag: &str) -> String {
    let unique = format!("koshi-peer-{}-{tag}", std::process::id());
    #[cfg(unix)]
    {
        PathBuf::from("/tmp")
            .join(unique)
            .with_extension("sock")
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        unique
    }
}

/// Serve one connection from another local user of this machine, with
/// `still_on` standing in for the `allow-other-users` setting the serving loop
/// reads. Hands back the caller's end, the serving thread and the address, so
/// a test can flip the setting under a live connection.
///
/// The user who started a session is served whatever the setting says, so
/// there is no such connection to cut and only this peer carries the live
/// read.
fn serve_other_user(
    tag: &str,
    still_on: &Arc<AtomicBool>,
    inbox_tx: Sender<RuntimeEvent>,
) -> (Connection, JoinHandle<()>, String) {
    let addr = test_addr(tag);
    remove_socket_file(&addr);
    let listener = Listener::bind(&addr).expect("bind");
    let setting = Arc::clone(still_on);
    let serving = std::thread::spawn(move || {
        let connection = listener.accept().expect("accept");
        let intake = Arc::new(Intake::default());
        let served = intake.accept(&connection).expect("the intake takes it");
        serve_connection(
            connection,
            ConnectionToken::generate(),
            &inbox_tx,
            Peer::Local {
                same_user: false,
                other_users_allowed: true,
            },
            Some(Arc::new(move || setting.load(Ordering::SeqCst))),
            &served,
        );
    });
    let caller = Connection::connect(&addr).expect("connect");
    (caller, serving, addr)
}

/// The Hello another local user sends: this build's range and no token, which
/// is all a user who cannot read the endpoint file has to present.
fn other_user_hello() -> IpcRequest {
    IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: ConnectionToken::new(""),
            remote: false,
        },
    }
}

#[test]
fn another_local_user_keeps_being_served_while_the_setting_stays_on() {
    let still_on = Arc::new(AtomicBool::new(true));
    let overview = overview_named("shared-session");
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, Some(overview.clone()));
    let (mut caller, serving, addr) = serve_other_user("stays-on", &still_on, inbox_tx);

    caller.send(&other_user_hello()).expect("send hello");
    let reply: IpcResponse = caller.recv().expect("hello reply");
    assert_eq!(reply.result, hello_accepted());

    for request_id in [2, 3] {
        caller
            .send(&IpcRequest {
                request_id,
                kind: IpcRequestKind::Discovery,
            })
            .expect("send discovery");
        let reply: IpcResponse = caller.recv().expect("discovery reply");
        assert_eq!(
            reply,
            IpcResponse {
                request_id: Some(request_id),
                result: IpcResult::Overview(overview.clone()),
            }
        );
    }

    drop(caller);
    serving.join().expect("serving thread");
    dispatcher.join().expect("dispatcher exits");
    remove_socket_file(&addr);
}

#[test]
fn another_local_users_connection_is_cut_when_the_setting_goes_off() {
    let still_on = Arc::new(AtomicBool::new(true));
    let overview = overview_named("shared-session");
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_dispatcher(inbox_rx, Some(overview.clone()));
    let (mut caller, serving, addr) = serve_other_user("goes-off", &still_on, inbox_tx);

    caller.send(&other_user_hello()).expect("send hello");
    let reply: IpcResponse = caller.recv().expect("hello reply");
    assert_eq!(reply.result, hello_accepted());
    caller
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let reply: IpcResponse = caller.recv().expect("discovery reply");
    assert_eq!(
        reply,
        IpcResponse {
            request_id: Some(2),
            result: IpcResult::Overview(overview),
        }
    );

    // The serving loop is blocked reading, so the setting turns off between
    // one request and the next.
    still_on.store(false, Ordering::SeqCst);
    caller
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");

    assert!(
        matches!(caller.recv::<IpcResponse>(), Err(IpcError::Disconnected)),
        "the request is answered with a closed connection, not an overview",
    );

    drop(caller);
    serving.join().expect("serving thread");
    dispatcher.join().expect("dispatcher exits");
    remove_socket_file(&addr);
}

#[test]
fn an_attached_client_of_another_local_user_is_detached_when_the_setting_goes_off() {
    let client = ClientId::new();
    let session = SessionId::new();
    let still_on = Arc::new(AtomicBool::new(true));
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client, session);
    let (mut caller, serving, addr) = serve_other_user("attached-off", &still_on, inbox_tx);

    caller.send(&other_user_hello()).expect("send hello");
    let reply: IpcResponse = caller.recv().expect("hello reply");
    assert_eq!(reply.result, hello_accepted());
    caller
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Attach {
                viewport: VIEWPORT,
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
            },
        })
        .expect("send attach");
    let reply: IpcResponse = caller.recv().expect("attach reply");
    assert_eq!(
        reply.result,
        IpcResult::Attached {
            client_id: client,
            session_id: session,
            structure: attached_structure(session),
            resume_token: Some(ConnectionToken::new(MINTED_TOKEN)),
        }
    );

    let pressed = KeyChord::new(ModFlags::NONE, Key::Char('k'));
    caller
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, pressed);

    still_on.store(false, Ordering::SeqCst);
    caller
        .send(&IpcRequest {
            request_id: 4,
            kind: IpcRequestKind::KeyPress { chord: pressed },
        })
        .expect("send key press");

    // The typing that arrived after the setting went off never reached the
    // session; the client left instead.
    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
    assert_eq!(
        caller.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );

    drop(caller);
    serving.join().expect("serving thread");
    dispatcher.join().expect("dispatcher exits");
    remove_socket_file(&addr);
}

#[test]
fn the_directory_other_local_users_reach_holds_only_the_socket() {
    let (server, session, runtime_dir, shared_dir, dispatcher) = serve_shared("shared-only", true);
    #[cfg(unix)]
    let user_dir = {
        use std::os::unix::fs::MetadataExt;

        let uid = std::fs::metadata(&runtime_dir)
            .expect("stat runtime dir")
            .uid();
        shared_dir.join(uid.to_string())
    };
    // Pipe names share one machine-wide namespace, so Windows advertises in
    // the shared directory itself.
    #[cfg(windows)]
    let user_dir = shared_dir.clone();

    let mut entries: Vec<String> = std::fs::read_dir(&user_dir)
        .expect("read the shared directory")
        .map(|entry| {
            entry
                .expect("read an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    entries.sort();

    // The endpoint file carrying the token is not among them: it stayed in the
    // private runtime directory the socket left.
    #[cfg(unix)]
    assert_eq!(entries, vec![format!("{session}.sock")]);
    #[cfg(windows)]
    assert_eq!(entries, vec![session.to_string()]);

    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
    cleanup(&shared_dir);
}

#[test]
fn admit_gates_the_starting_user_always_and_the_other_users_by_the_setting() {
    assert_eq!(
        admit(true, false),
        Peer::Local {
            same_user: true,
            other_users_allowed: false,
        },
    );
    assert_eq!(
        admit(true, true),
        Peer::Local {
            same_user: true,
            other_users_allowed: true,
        },
    );
    assert_eq!(
        admit(false, false),
        Peer::Local {
            same_user: false,
            other_users_allowed: false,
        },
    );
    assert_eq!(
        admit(false, true),
        Peer::Local {
            same_user: false,
            other_users_allowed: true,
        },
    );
}

/// A stand-in dispatcher that answers every restart request with `verdict` and
/// every discovery request with `overview`, so a test reads what a refusal or
/// an acceptance looks like on the socket and whether the session keeps
/// serving after it. Exits when every inbox sender is gone.
fn spawn_restart_dispatcher(
    inbox_rx: Receiver<RuntimeEvent>,
    verdict: Result<(), String>,
    overview: Option<SessionOverview>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(event) = inbox_rx.recv() {
            match event {
                RuntimeEvent::IpcRestart { reply } => {
                    let _ = reply.send(verdict.clone());
                }
                RuntimeEvent::IpcDiscovery { reply } => {
                    let _ = reply.send(overview.clone());
                }
                _ => {}
            }
        }
    })
}

/// A served socket whose stand-in dispatcher answers restart requests with
/// `verdict` and discovery requests with `overview`.
fn serve_restartable(
    tag: &str,
    verdict: Result<(), String>,
    overview: Option<SessionOverview>,
) -> (IpcServer, SessionId, PathBuf, JoinHandle<()>) {
    let runtime_dir = test_runtime_dir(tag);
    let session = SessionId::new();
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let dispatcher = spawn_restart_dispatcher(inbox_rx, verdict, overview);
    let server = IpcServer::start(&runtime_dir, session, inbox_tx, None).expect("start serving");
    (server, session, runtime_dir, dispatcher)
}

/// Say hello, send a restart, and hand back what the restart was answered.
fn restart_over(runtime_dir: &Path, session: SessionId) -> (Connection, IpcResult) {
    let mut connection = connect_to(runtime_dir, session);
    connection
        .send(&hello_for(runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Restart,
        })
        .expect("send restart");
    let reply: IpcResponse = connection.recv().expect("restart reply");
    assert_eq!(reply.request_id, Some(2));
    (connection, reply.result)
}

/// Ask for the session's description on an open connection and hand back the
/// answer, so a test can show the session still serves after a refusal.
fn discovery_over(connection: &mut Connection, request_id: u64) -> IpcResult {
    connection
        .send(&IpcRequest {
            request_id,
            kind: IpcRequestKind::Discovery,
        })
        .expect("send discovery");
    let reply: IpcResponse = connection.recv().expect("discovery reply");
    assert_eq!(reply.request_id, Some(request_id));
    reply.result
}

#[test]
fn an_accepted_restart_is_answered_restarting() {
    let (server, session, runtime_dir, dispatcher) =
        serve_restartable("restart-accepted", Ok(()), None);

    let (connection, result) = restart_over(&runtime_dir, session);

    assert_eq!(result, IpcResult::Restarting);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// The Hello gate covers the restart like every other kind, so a caller that
/// never opened the connection cannot make the session replace its own image.
#[test]
fn a_restart_before_hello_is_refused_as_hello_required_and_the_connection_keeps_serving() {
    let (server, session, runtime_dir, dispatcher) =
        serve_restartable("restart-early", Ok(()), Some(overview_named("still-here")));
    let mut connection = connect_to(&runtime_dir, session);

    connection
        .send(&IpcRequest {
            request_id: 9,
            kind: IpcRequestKind::Restart,
        })
        .expect("send restart before the hello");
    let refusal: IpcResponse = connection.recv().expect("refusal reply");

    assert_eq!(refusal.request_id, Some(9));
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Restart arrived before a Hello opened the connection".to_string(),
        }),
    );

    // The gate is still closed, so the same connection still answers.
    assert_eq!(
        discovery_over(&mut connection, 10),
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::HelloRequired,
            message: "Discovery arrived before a Hello opened the connection".to_string(),
        }),
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

/// A binary this machine could not run, written into `dir`: on Unix a file
/// with its execute permission dropped, elsewhere a path with nothing at it.
/// Hands back the path and the sentence the check refuses it with.
fn unrunnable_binary(dir: &Path) -> (PathBuf, String) {
    std::fs::create_dir_all(dir).expect("the directory is created");
    let exe = dir.join("koshi");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::write(&exe, b"").expect("the stand-in binary is written");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644))
            .expect("the execute permission is dropped");
        let message = format!("the binary at {} is not executable", exe.display());
        (exe, message)
    }
    #[cfg(not(unix))]
    {
        let error = std::fs::metadata(&exe).expect_err("nothing is at that path");
        let message = format!("the binary at {} could not be read: {error}", exe.display());
        (exe, message)
    }
}

/// The reply is the session's only chance to refuse: after it the swap runs. A
/// binary this machine could not run must never reach it, and the refusal must
/// leave the session serving.
#[test]
fn a_restart_naming_a_binary_that_cannot_run_is_refused_and_the_session_keeps_serving() {
    let binary_dir = test_runtime_dir("restart-bad-binary-dir");
    let (exe, message) = unrunnable_binary(&binary_dir);
    let overview = overview_named("still-here");
    let (server, session, runtime_dir, dispatcher) = serve_restartable(
        "restart-bad-binary",
        crate::server::binary_is_runnable(&exe),
        Some(overview.clone()),
    );

    let (mut connection, result) = restart_over(&runtime_dir, session);

    assert_eq!(
        result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message,
        }),
    );
    // Nothing was torn down, so the session answers the next request.
    assert_eq!(
        discovery_over(&mut connection, 3),
        IpcResult::Overview(overview)
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
    cleanup(&binary_dir);
}

/// A pane whose terminal exposes no descriptor cannot cross the swap, so the
/// restart is refused and the pane is named. Windows keeps every pane's
/// pseudoconsole in the supervisor process, so no pane holds a restart back
/// there.
#[cfg(unix)]
#[test]
fn a_restart_with_a_pane_that_has_no_terminal_descriptor_is_refused_naming_that_pane() {
    let stranded = PaneId::new();
    let panes = [koshi_pty::portable::CarriedPtyPane {
        pane_id: stranded,
        terminal_fd: None,
        pid: 51234,
        size: koshi_core::process::PtySize { cols: 80, rows: 24 },
        exit: None,
    }];
    let overview = overview_named("still-here");
    let (server, session, runtime_dir, dispatcher) = serve_restartable(
        "restart-no-fd",
        crate::server::panes_can_be_carried(&panes),
        Some(overview.clone()),
    );

    let (mut connection, result) = restart_over(&runtime_dir, session);

    assert_eq!(
        result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "pane {stranded} has no terminal descriptor, \
                 so its terminal cannot cross the swap"
            ),
        }),
    );
    assert_eq!(
        discovery_over(&mut connection, 3),
        IpcResult::Overview(overview)
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_restart_on_an_attached_connection_detaches_that_client_and_restarts_nothing() {
    // A `Restart` reaches the dispatcher only over a connection that is serving
    // requests. An attached connection is carrying one client's events instead,
    // so the request ends that stream and no restart request is made.
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("attached-restart", client);
    let mut connection = attach_to(&runtime_dir, session, client);

    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::Restart,
        })
        .expect("send restart");

    let RuntimeEvent::ClientDetached { client_id, .. } = seen.recv().expect("detach event") else {
        panic!("expected ClientDetached");
    };
    assert_eq!(client_id, client);
    assert_eq!(
        connection.recv::<SessionEvent>().expect("goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(
        matches!(
            connection.recv::<SessionEvent>(),
            Err(IpcError::Disconnected),
        ),
        "the stream ends after the goodbye, with no answer to the restart",
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

// --- leaving ---

/// Wait until `server` counts no attached client's connection, and hand back
/// how many it counts. Fails the test rather than hanging if one never ends.
fn wait_for_clients_to_leave(server: &IpcServer) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.attached_connections() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    server.attached_connections()
}

/// Wait until `server` counts `want` attached clients' connections, and hand
/// back how many it counts. The count rises on the serving thread after the
/// attach reply is written, so a caller that just read that reply polls here.
fn wait_for_attached(server: &IpcServer, want: usize) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.attached_connections() != want && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    server.attached_connections()
}

#[test]
fn every_key_a_client_sent_reaches_the_dispatcher_before_that_client_leaves() {
    // A client that reads the restart frame sends `Leaving` and writes nothing
    // after it. Requests arrive in the order the client queued them, so reading
    // that one is what says the session holds every key that client typed. The
    // image swap waits for exactly this before it carries the session out.
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) = serve_attachable("leaving", client);
    let mut connection = attach_to(&runtime_dir, session, client);
    assert_eq!(
        wait_for_attached(&server, 1),
        1,
        "the attached client's connection is counted while it is read"
    );

    let typed = [
        KeyChord::new(ModFlags::CTRL, Key::Char('a')),
        KeyChord::new(ModFlags::CTRL, Key::Char('b')),
        KeyChord::new(ModFlags::CTRL, Key::Char('c')),
    ];
    for (round, chord) in typed.iter().enumerate() {
        connection
            .send(&IpcRequest {
                request_id: 3 + round as u64,
                kind: IpcRequestKind::KeyPress { chord: *chord },
            })
            .expect("send the key press");
    }
    connection
        .send(&IpcRequest {
            request_id: 6,
            kind: IpcRequestKind::Leaving,
        })
        .expect("send leaving");

    for chord in typed {
        let RuntimeEvent::ClientKeyPress {
            client_id,
            chord: pressed,
        } = seen
            .recv_timeout(Duration::from_secs(5))
            .expect("key press event")
        else {
            panic!("expected ClientKeyPress");
        };
        assert_eq!(client_id, client);
        assert_eq!(pressed, chord);
    }
    // The reading half ends on the request that follows those keys, so the
    // client's record is released here and nowhere earlier.
    assert!(
        matches!(
            seen.recv_timeout(Duration::from_secs(5)).expect("detach event"),
            RuntimeEvent::ClientDetached { client_id, .. } if client_id == client,
        ),
        "leaving detaches the client that left",
    );
    assert_eq!(
        wait_for_clients_to_leave(&server),
        0,
        "the connection is no longer counted once its client has left"
    );

    // The session closes the connection it was serving: the stream carries this
    // client's goodbye and then ends.
    assert_eq!(
        connection
            .recv::<SessionEvent>()
            .expect("the goodbye frame"),
        SessionEvent::Detached,
    );
    assert!(matches!(
        connection.recv::<SessionEvent>(),
        Err(IpcError::Disconnected),
    ));

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_control_connection_that_leaves_is_closed_with_no_answer() {
    // Nothing on a control connection is left half-answered when it leaves:
    // every request it sent was answered as it was served, and the request that
    // ends it carries no answer of its own.
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("leaving-control", client);

    let mut connection = connect_to(&runtime_dir, session);
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let hello_reply: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(hello_reply.result, hello_accepted());

    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Leaving,
        })
        .expect("send leaving");
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "a connection that leaves is closed with no answer",
    );
    // A control connection carries no client, so nothing about it reaches the
    // session.
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );
    assert_eq!(server.attached_connections(), 0);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

// --- rotating the token ---

#[test]
fn a_rotated_token_is_advertised_and_the_one_before_it_is_refused() {
    let (server, session, runtime_dir, dispatcher) = serve("rotate-token", None);
    let endpoint_path = EndpointFile::path(&runtime_dir, session);
    let first = EndpointFile::read(&endpoint_path).expect("endpoint file readable");

    server
        .rotate_token()
        .expect("the fresh token is advertised");

    let second = EndpointFile::read(&endpoint_path).expect("endpoint file readable");
    assert_ne!(
        second.token, first.token,
        "the rotation advertises a new secret",
    );
    assert_eq!(
        second.socket, first.socket,
        "the address the session is serving on does not change",
    );

    let mut old = connect_to(&runtime_dir, session);
    old.send(&IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: first.token,
            remote: false,
        },
    })
    .expect("send hello with the token from before the rotation");
    let refusal: IpcResponse = old.recv().expect("reply");
    assert_eq!(
        refusal.result,
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::BadToken,
            message: "the token presented does not match this Koshi's".to_string(),
        }),
    );

    let mut fresh = connect_to(&runtime_dir, session);
    fresh
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello with the rotated secret");
    let accepted: IpcResponse = fresh.recv().expect("hello reply");
    assert_eq!(accepted.result, hello_accepted());

    drop(old);
    drop(fresh);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn rotating_the_token_takes_connections_again_after_the_intake_closed() {
    // The image swap closes the intake and then finds it cannot go through
    // with the swap. The session keeps this socket, so it has to serve on it
    // again.
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("rotate-reopen", client);

    server.close_intake();
    server
        .rotate_token()
        .expect("the fresh token is advertised");

    let mut connection = connect_to(&runtime_dir, session);
    connection
        .send(&hello_for(&runtime_dir, session))
        .expect("send hello");
    let accepted: IpcResponse = connection.recv().expect("hello reply");
    assert_eq!(accepted.result, hello_accepted());

    // Served, not merely accepted: what this connection sends reaches the
    // dispatcher again.
    let chord = KeyChord::new(ModFlags::CTRL, Key::Char('r'));
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Attach {
                viewport: VIEWPORT,
                filter: EventFilterSpec::All,
                resume: None,
                resume_token: None,
            },
        })
        .expect("send attach");
    let attached: IpcResponse = connection.recv().expect("attach reply");
    assert_eq!(attached.request_id, Some(2));
    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::KeyPress { chord },
        })
        .expect("send the key press");
    let RuntimeEvent::ClientKeyPress {
        client_id,
        chord: pressed,
    } = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("key press")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(pressed, chord);

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

// --- closing the intake ---

#[test]
fn a_request_a_client_sends_after_the_intake_closes_never_reaches_the_dispatcher() {
    // The image swap closes the intake and then makes one pass over the runtime
    // inbox. A key press that reached the dispatcher after that pass would be
    // neither applied nor carried across, so the user's keystroke would vanish.
    let client = ClientId::new();
    let session = SessionId::new();
    let runtime_dir = test_runtime_dir("intake-closed");
    // The test keeps an inbox sender of its own, so it can end this client's
    // writing thread once the intake refuses the detach that would.
    let (inbox_tx, inbox_rx) = mpsc::channel();
    let (dispatcher, seen) = spawn_attaching_dispatcher(inbox_rx, client, session);
    let server =
        IpcServer::start(&runtime_dir, session, inbox_tx.clone(), None).expect("start serving");
    let mut connection = attach_to(&runtime_dir, session, client);
    let taken = KeyChord::new(ModFlags::CTRL, Key::Char('a'));
    let refused = KeyChord::new(ModFlags::CTRL, Key::Char('b'));

    // Before the close: the connection carries this client's keys, so the press
    // reaches the dispatcher.
    connection
        .send(&IpcRequest {
            request_id: 3,
            kind: IpcRequestKind::KeyPress { chord: taken },
        })
        .expect("send the key press the session takes");
    let RuntimeEvent::ClientKeyPress { client_id, chord } = seen.recv().expect("key press event")
    else {
        panic!("expected ClientKeyPress");
    };
    assert_eq!(client_id, client);
    assert_eq!(chord, taken);

    server.close_intake();

    // After the close: the client is refused. Its send fails outright, or the
    // press is read and never handed over. Either way nothing more reaches the
    // dispatcher — including the detach the connection's own ending would
    // otherwise queue, which is what keeps this client's record carried across.
    let _ = connection.send(&IpcRequest {
        request_id: 4,
        kind: IpcRequestKind::KeyPress { chord: refused },
    });
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    // Closing this client's queue is what ends its writing thread, and the
    // dispatcher ends once every inbox sender is gone.
    inbox_tx
        .send(RuntimeEvent::ClientDetached {
            client_id: client,
            detached_at: SystemTime::now(),
            streamed: true,
        })
        .expect("the detach is queued");
    drop(inbox_tx);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}

#[test]
fn a_connection_accepted_after_the_intake_closes_is_not_served() {
    // A caller that connects while the swap is carrying the state out must not
    // be answered: the session it would reach is about to be replaced.
    let client = ClientId::new();
    let (server, session, runtime_dir, dispatcher, seen) =
        serve_attachable("intake-closed-accept", client);

    server.close_intake();

    let mut connection = connect_to(&runtime_dir, session);
    // A send may fail as the accept loop drops the connection; the read that
    // follows reports end of stream either way.
    let _ = connection.send(&hello_for(&runtime_dir, session));
    assert!(
        matches!(
            connection.recv::<IpcResponse>(),
            Err(IpcError::Disconnected),
        ),
        "a connection accepted after the intake closed is closed unanswered",
    );
    assert_eq!(
        seen.recv_timeout(Duration::from_secs(2)).unwrap_err(),
        mpsc::RecvTimeoutError::Timeout,
    );

    drop(connection);
    server.shutdown();
    dispatcher.join().expect("dispatcher exits");
    cleanup(&runtime_dir);
}
