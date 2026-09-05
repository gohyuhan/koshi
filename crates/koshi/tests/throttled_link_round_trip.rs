//! What a session does for an attached client whose link cannot carry the
//! events as fast as the session produces them.
//!
//! One session server runs on a thread of this process over a
//! [`FakePtyBackend`], the way `in_session_cli_round_trip` runs it, and two
//! clients attach to it:
//!
//! 1. The throttled client. Its frames do not travel the control socket
//!    directly. A loopback listener in this process accepts its connection,
//!    opens its own connection to the control socket, and relays raw bytes
//!    between the two with one
//!    [`pump_throttled`](koshi_test_support::throttle::pump_throttled) per
//!    direction. The direction carrying the session's events is held to a small
//!    number of bytes per slice, so the session outruns it.
//! 2. The control client, attached straight to the control socket with no
//!    throttle, whose reader thread drains everything the session writes.
//!
//! The session then gets a burst it cannot deliver: pane output, which is a
//! lossy event class the bus may drop, and thousands of tab focus changes,
//! which are the critical class it may not. The throttled client's queue fills,
//! its lossy events are dropped, the first critical event that does not fit
//! marks it desynced, and the serve loop's own render pass resyncs it with a
//! [`SessionEvent::Resync`] naming how many events went missing. Nothing in
//! this file reaches into the bus to make that happen. A focus change
//! submitted after that resync must arrive on the throttled connection
//! itself.
//!
//! Every wait here is bounded. A queue read uses the time left until its
//! [`Instant`] deadline, so a session that never resyncs fails this test instead
//! of hanging it.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusTabArgs, NewTabArgs, TabTarget,
};
use koshi_core::event::{Event, TabFocused};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_ipc::transport::{frame_halves, Connection, Deadlined, FrameReader, FrameWriter};
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::test_runtime_dir;
use koshi_test_support::throttle::pump_throttled;
use tempfile::TempDir;

/// How long a poll waits for something the session server has to do before the
/// test calls it a failure.
const WAIT: Duration = Duration::from_secs(60);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(10);

/// The terminal size the session starts at and every attaching client reports.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The span one slice of a relay pump covers.
const SLICE: Duration = Duration::from_millis(10);

/// Bytes per slice on the direction carrying the throttled client's own frames
/// to the session. Wide enough that its handshake and its commands are not
/// slowed at all.
const TO_SESSION_BYTES: usize = 64 * 1024;

/// Bytes per slice on the direction carrying the session's events to the
/// throttled client: about 100 kilobytes a second, far under what the burst
/// below produces.
const FROM_SESSION_BYTES: usize = 1024;

/// How many pane-output chunks the burst pushes. Each one becomes a lossy
/// `PaneOutputUpdated` event.
const LOSSY_CHUNKS: usize = 400;

/// One chunk of pane output: a row of one repeated character, then a new line.
/// A row of equal cells travels as one run, so the picture the session composes
/// for this pane stays small and the backlog the throttled link must carry is
/// the events, not the pictures.
const OUTPUT_CHUNK: &[u8] =
    b"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\r\n";

/// How many tab focus changes the burst submits. Each one puts one critical
/// event on every subscriber's queue, which is several times the 1024 entries
/// one queue holds plus everything the throttled link carries away while the
/// burst runs.
const CRITICAL_COMMANDS: usize = 6000;

/// One session server running on its own thread, serving a real control socket
/// in its own runtime directory over a fake PTY backend. Dropping it stops that
/// thread and withdraws the socket.
struct RunningSession {
    /// The runtime directory the control socket and endpoint file live in.
    dir: TempDir,
    /// The session the server seeded and serves.
    id: SessionId,
    /// The backend that stands in for the panes' children, so a test can drive
    /// pane output.
    pty: Arc<FakePtyBackend>,
    /// The runtime inbox, for the hangup that ends the serving thread.
    inbox_tx: mpsc::Sender<RuntimeEvent>,
    /// The serving thread, joined at drop. `Option` so the drop can take it out
    /// of the otherwise-borrowed struct.
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
        // The endpoint file is written after the socket binds, so a readable one
        // means the socket is ready to answer.
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

    /// The session's own report of itself, read over the control socket by the
    /// library call the `koshi inspect` verbs make.
    fn overview(&self) -> koshi_core::discovery::SessionOverview {
        koshi_link::ipc_client::fetch_overview(self.dir.path(), self.id)
            .expect("the session server describes itself")
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        // The serving loop stops on a `Quit`; a loop that already stopped on its
        // own leaves a closed inbox, and the send fails harmlessly.
        let _ = self.inbox_tx.send(RuntimeEvent::Quit);
        if let Some(handle) = self.dispatcher.take() {
            let _ = handle.join();
        }
    }
}

/// Build one session's server on `pty`, seed the session, bind its control
/// socket in `runtime_dir`, and serve the runtime inbox until the session ends.
fn serve_session(
    runtime_dir: &Path,
    session_id: SessionId,
    pty: Arc<FakePtyBackend>,
    inbox_rx: mpsc::Receiver<RuntimeEvent>,
    inbox_tx: mpsc::Sender<RuntimeEvent>,
) {
    let backend: Arc<dyn PtyBackend> = pty;
    let mut server = Server::new(backend, inbox_rx, inbox_tx.clone());
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

/// The endpoint file the session server advertises: the socket address and the
/// token a Hello presents.
fn endpoint(session: &RunningSession) -> EndpointFile {
    EndpointFile::read(&EndpointFile::path(session.dir.path(), session.id))
        .expect("the session server advertises its socket")
}

/// One loopback stream half, as a framed connection reads or writes it.
///
/// [`Deadlined::set_deadline`] is ignored: this stream's own read timeout, set
/// where it is opened, is what bounds a read on it.
struct LoopbackHalf(TcpStream);

impl Read for LoopbackHalf {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for LoopbackHalf {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Deadlined for LoopbackHalf {
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// Open a connection to the socket `endpoint` advertises, with its handshake
/// already done.
fn open(endpoint: &EndpointFile) -> Connection {
    let mut connection = Connection::connect(&endpoint.socket).expect("the socket answers");
    let hello = hello_request(endpoint);
    connection.send(&hello).expect("the server reads the Hello");
    let reply: IpcResponse = connection.recv().expect("the server answers the Hello");
    match reply.result {
        IpcResult::Hello { .. } => connection,
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// The opening request every connection here sends: the versions this build
/// speaks and the secret the endpoint file carries.
fn hello_request(endpoint: &EndpointFile) -> IpcRequest {
    IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: endpoint.token.clone(),
            remote: false,
        },
    }
}

/// The request that joins a session as an attached client.
fn attach_request() -> IpcRequest {
    IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Attach {
            viewport: VIEWPORT,
            filter: EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
            graphics: koshi_ipc::protocol::GraphicsCapabilities::default(),
        },
    }
}

/// A client attached over the control socket, with its event stream drained by
/// its own thread into a queue this thread polls.
struct AttachedClient {
    /// The client the session minted for this connection.
    id: ClientId,
    /// The session's structure as the attach reply reported it.
    structure: AttachedSessionStructureSnapshot,
    /// Every frame the session wrote that says something other than the picture
    /// to draw, in arrival order.
    events: mpsc::Receiver<SessionEvent>,
}

/// Attach to `session` straight over its control socket — Hello then Attach on
/// one connection — and hand back the client the server minted, the structure
/// it was given, and its event stream.
fn attach(session: &RunningSession) -> AttachedClient {
    let mut connection = open(&endpoint(session));
    connection
        .send(&attach_request())
        .expect("the server reads the attach");
    let reply: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(reply.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id,
        structure,
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
        structure,
        events,
    }
}

/// A relay listening on loopback: it accepts one connection, opens its own
/// connection to `endpoint`, and copies raw bytes between the two at a bounded
/// rate in each direction.
///
/// The direction carrying the session's events moves
/// [`FROM_SESSION_BYTES`] per [`SLICE`]; the direction carrying the client's own
/// frames moves [`TO_SESSION_BYTES`]. Both pumps stop at `deadline`, so no
/// thread here outlives the test.
fn start_relay(endpoint: EndpointFile, deadline: Instant) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the relay's listener");
    let address = listener
        .local_addr()
        .expect("read the relay's bound address")
        .to_string();
    std::thread::spawn(move || {
        let (client_side, _) = listener.accept().expect("the relay accepts one client");
        // The pump reading this stream re-checks the deadline after each
        // timeout, so a client that stops sending cannot hold that thread.
        client_side
            .set_read_timeout(Some(POLL))
            .expect("set the relay's read timeout");
        let inbound = client_side
            .try_clone()
            .expect("duplicate the relay's client stream");
        let session_side = Connection::connect(&endpoint.socket).expect("the socket answers");
        let (from_session, to_session) = session_side.split_raw();
        pump_throttled(inbound, to_session, TO_SESSION_BYTES, SLICE, deadline);
        pump_throttled(
            from_session,
            client_side,
            FROM_SESSION_BYTES,
            SLICE,
            deadline,
        );
    });
    address
}

/// A client attached through the relay: the halves of its framed connection,
/// plus what the attach reply said.
struct ThrottledClient {
    /// The client the session minted for this connection.
    id: ClientId,
    /// The half the session's frames arrive on. Nothing reads it until the test
    /// hands it to [`drain_into_queue`], which is what holds this client behind
    /// the session.
    incoming: FrameReader,
    /// The half this client's own frames go out on, kept open for as long as the
    /// connection must live.
    outgoing: FrameWriter,
}

/// Connect to the relay at `address`, do the Hello and the Attach over it, and
/// hand back the attached client with its stream unread.
fn attach_through_relay(session: &RunningSession, address: &str) -> ThrottledClient {
    let endpoint = endpoint(session);
    let stream = TcpStream::connect(address).expect("the relay answers");
    let reading = stream
        .try_clone()
        .expect("duplicate the client's relay stream");
    let (mut incoming, mut outgoing) = frame_halves(
        Box::new(LoopbackHalf(reading)),
        Box::new(LoopbackHalf(stream)),
    );

    outgoing
        .send(&hello_request(&endpoint))
        .expect("the relay carries the Hello");
    let reply: IpcResponse = incoming.recv().expect("the server answers the Hello");
    let IpcResult::Hello { .. } = reply.result else {
        panic!("the Hello was answered with {:?}", reply.result);
    };

    outgoing
        .send(&attach_request())
        .expect("the relay carries the attach");
    let reply: IpcResponse = incoming.recv().expect("the server answers the attach");
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

    ThrottledClient {
        id: client_id,
        incoming,
        outgoing,
    }
}

/// Start reading `incoming` on its own thread, forwarding every frame that is
/// not the picture to draw into a queue this thread polls with a deadline.
fn drain_into_queue(mut incoming: FrameReader) -> mpsc::Receiver<SessionEvent> {
    let (events_tx, events) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(event) = incoming.recv::<SessionEvent>() {
            if matches!(event, SessionEvent::Painted { .. }) {
                continue;
            }
            if events_tx.send(event).is_err() {
                break;
            }
        }
    });
    events
}

/// Submit `command` over `connection`, enveloped the way the CLI running inside
/// `pane` envelopes it, and hand back the dispatcher's result.
///
/// The connection is reused across calls: the control socket serves requests on
/// one connection until its peer hangs up, so a burst of commands costs one
/// connection, not one each.
fn submit_on(
    connection: &mut Connection,
    session: &RunningSession,
    client: ClientId,
    pane: PaneId,
    socket: &str,
    command: Command,
) -> CommandResult {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::in_session_cli(session.id, Some(client), pane, PathBuf::from(socket)),
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

/// The events an applied result carries, or the rejection that carried none.
fn applied_events(result: &CommandResult) -> &[Event] {
    match result {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        other => panic!("expected an applied command, got {other:?}"),
    }
}

/// Attach once more over the control socket and hand back only the structure
/// the reply carried, with the connection closed again.
fn reattached_structure(session: &RunningSession) -> AttachedSessionStructureSnapshot {
    let mut connection = open(&endpoint(session));
    connection
        .send(&attach_request())
        .expect("the server reads the attach");
    let reply: IpcResponse = connection.recv().expect("the server answers the attach");
    let IpcResult::Attached { structure, .. } = reply.result else {
        panic!("expected an attach reply, got {:?}", reply.result);
    };
    structure
}

/// Wait until the session reports exactly `tabs` tabs, so the burst's last
/// command has been applied before anything is compared.
fn wait_for_tab_count(session: &RunningSession, tabs: usize) {
    let deadline = Instant::now() + WAIT;
    loop {
        let overview = session.overview();
        if overview.tabs.len() == tabs {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the session settled at {} tabs, not {tabs}",
            overview.tabs.len()
        );
        std::thread::sleep(POLL);
    }
}

/// `client`'s id as the session reports its attached clients, and `None` when
/// the session no longer holds that client.
fn attached_id(session: &RunningSession, client: ClientId) -> Option<ClientId> {
    session
        .overview()
        .clients
        .iter()
        .find(|info| info.id == client)
        .map(|info| info.id)
}

#[test]
fn a_burst_the_throttled_link_cannot_carry_desyncs_that_client_and_resyncs_it() {
    let session = RunningSession::start();
    let socket = endpoint(&session).socket;
    let root = session.pty.spawned_panes()[0];

    // Every pump stops at this instant, so no relay thread outlives the test
    // even if an assertion below ends it early.
    let relay_deadline = Instant::now() + WAIT * 3;
    let relay = start_relay(endpoint(&session), relay_deadline);
    let throttled = attach_through_relay(&session, &relay);
    let control = attach(&session);

    // A second tab, so each focus change below actually moves focus and emits
    // the critical event that fills the throttled client's queue.
    let first_tab = session.overview().tabs[0].id;
    let mut commands = open(&endpoint(&session));
    let created = submit_on(
        &mut commands,
        &session,
        control.id,
        root,
        &socket,
        Command::NewTab(NewTabArgs::default()),
    );
    let second_tab = match applied_events(&created) {
        [Event::TabCreated(payload), ..] => payload.tab_id,
        other => panic!("expected a tab to be created, got {other:?}"),
    };
    assert_eq!(session.overview().tabs.len(), 2);

    // The lossy half of the burst: pane output the bus may drop.
    for _ in 0..LOSSY_CHUNKS {
        session
            .pty
            .push_output(root, OUTPUT_CHUNK.to_vec())
            .expect("the fake backend takes the pane's output");
    }

    // The critical half: tab focus changes, which the bus may not drop
    // silently. Nothing reads the throttled client's stream while this runs, so
    // its queue fills and then overflows.
    // The new tab is the one the control client is on, so the first `Next` wraps
    // back to the first tab and every turn after it swaps the two.
    for turn in 0..CRITICAL_COMMANDS {
        let result = submit_on(
            &mut commands,
            &session,
            control.id,
            root,
            &socket,
            Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
                client: Some(control.id),
            }),
        );
        let (onto, off) = if turn % 2 == 0 {
            (first_tab, second_tab)
        } else {
            (second_tab, first_tab)
        };
        assert_eq!(
            applied_events(&result),
            [Event::TabFocused(TabFocused {
                client_id: control.id,
                tab_id: onto,
                prior_tab: off,
            })]
        );
    }
    wait_for_tab_count(&session, 2);

    // Only now does the throttled client start reading. Its queue holds the
    // backlog, and the resync the serve loop owes it lands after that backlog.
    let throttled_events = drain_into_queue(throttled.incoming);
    let deadline = Instant::now() + WAIT;
    let mut events_read = 0;
    let dropped_count = loop {
        assert!(
            Instant::now() < deadline,
            "the throttled client was never resynced after reading {events_read} events"
        );
        let event = throttled_events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the session keeps writing to the throttled client");
        events_read += 1;
        assert!(
            Instant::now() < deadline,
            "the throttled client was never resynced after reading {events_read} events"
        );
        if let SessionEvent::Resync { dropped_count } = event {
            break dropped_count;
        }
    };
    assert!(
        dropped_count >= 1,
        "the resync reported {dropped_count} missed events, not at least one"
    );

    // The resync arrived on the connection the client attached on, and the
    // session still holds that client: the throttled link was never dropped and
    // remade.
    assert_eq!(attached_id(&session, throttled.id), Some(throttled.id));

    // A focus change submitted after the resync must reach the throttled
    // client through the relay, as the exact event the dispatcher emitted.
    // Frames still in flight from the burst — and any further resync among
    // them — are read past.
    let result = submit_on(
        &mut commands,
        &session,
        control.id,
        root,
        &socket,
        Command::FocusTab(FocusTabArgs {
            target: TabTarget::Next,
            client: Some(control.id),
        }),
    );
    let probe = match applied_events(&result) {
        [Event::TabFocused(payload)] => *payload,
        other => panic!("expected one focus change, got {other:?}"),
    };
    let probe_deadline = Instant::now() + WAIT;
    loop {
        assert!(
            Instant::now() < probe_deadline,
            "the focus change submitted after the resync never reached the throttled client"
        );
        let event = throttled_events
            .recv_timeout(probe_deadline.saturating_duration_since(Instant::now()))
            .expect("the session keeps writing to the throttled client");
        assert!(
            Instant::now() < probe_deadline,
            "the focus change submitted after the resync never reached the throttled client"
        );
        if event
            == (SessionEvent::TabFocused {
                client_id: probe.client_id,
                tab_id: probe.tab_id,
                prior_tab: probe.prior_tab,
            })
        {
            break;
        }
    }

    // The session the throttled client caught up to is the session an
    // unthrottled client sees. Both reads are fresh attaches over the control
    // socket on the settled session, so every id in them is the same id.
    let settled = reattached_structure(&session);
    let settled_again = reattached_structure(&session);
    assert_eq!(settled, settled_again);
    assert_eq!(settled.id, session.id);
    assert_eq!(
        settled
            .tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<TabId>>(),
        vec![first_tab, second_tab]
    );

    // The control client's own stream never stopped: it read the tab creation
    // the burst opened with.
    let first = control
        .events
        .recv_timeout(WAIT)
        .expect("the control client is told the session changed");
    assert_eq!(first, SessionEvent::TabCreated { tab_id: second_tab });
    assert_eq!(control.structure.tabs.len(), 1);

    // Closes the throttled client's writing half. It is open through every
    // assertion above.
    drop(throttled.outgoing);
}
