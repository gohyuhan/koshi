//! What the integration tests in this directory share: building a server over
//! a fake PTY backend, seeding a headless session on it, serving its control
//! socket from the real accept loop, running the per-session server's loop on
//! the test thread, and the requests a test sends over a connection.
//!
//! A socket-level test runs the shape the per-session server process runs in:
//! a headless session seeded with no client, its inbox drained and its frames
//! pushed on the thread that owns the server, and the socket answered by the
//! real accept loop. The exchange with the socket runs on its own thread while
//! the test thread runs the dispatcher.
//!
//! Every test binary declaring `mod common;` compiles all of it, and no single
//! binary uses every helper, so an unused one is allowed here.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use koshi_config::layer::PartialKoshiConfig;
use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::Event;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, SessionId};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::pty_inbox::InboxSink;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

/// The terminal size the seeded session is bootstrapped at, before any client
/// attaches.
pub const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The display name the seeded session carries.
pub const TEST_SESSION_NAME: &str = "workspace";

/// How long a test waits on work another thread does, such as a detach the
/// serving thread has yet to notice or an event frame in flight, before it
/// fails.
pub const TEST_WAIT_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// Sends [`RuntimeEvent::Quit`] when the exchange thread ends, on the way out
/// of a failed assertion as well as a clean return. The dispatcher stops on
/// that event.
struct DispatcherStopGuard(Sender<RuntimeEvent>);

impl Drop for DispatcherStopGuard {
    fn drop(&mut self) {
        let _ = self.0.send(RuntimeEvent::Quit);
    }
}

/// A seeded headless session serving its control socket, with an exchange
/// running against that socket on its own thread.
pub struct ServedTestSession<ExchangeResult> {
    pub server: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    ipc_server: IpcServer,
    runtime_directory: PathBuf,
    exchange_thread: JoinHandle<(Vec<Connection>, ExchangeResult)>,
}

impl<ExchangeResult> ServedTestSession<ExchangeResult> {
    /// Join the exchange thread, close the connections it handed back, stop
    /// serving, and remove the runtime directory. Returns the server, the fake
    /// backend, and the exchange's own value.
    pub fn finish(self) -> (Server, Arc<FakePtyBackend>, ExchangeResult) {
        let (open_connections, exchange_result) =
            self.exchange_thread.join().expect("the exchange finished");
        drop(open_connections);
        self.ipc_server.shutdown();
        let _ = std::fs::remove_dir_all(&self.runtime_directory);
        (self.server, self.fake_pty_backend, exchange_result)
    }
}

/// A server over a fresh fake PTY backend that delivers each pane's output and
/// exit into the server's inbox, that backend, and a sender into the same
/// inbox.
pub fn build_server_with_fake_pty_backend() -> (Server, Arc<FakePtyBackend>, Sender<RuntimeEvent>) {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let server = Server::from_runtime_parts(pty_backend, runtime_event_receiver);
    (server, fake_pty_backend, runtime_event_sender)
}

/// Apply `startup_config` (`None` keeps the built-in defaults), seed a headless
/// session under a fake PTY backend, serve its control socket from a directory
/// named for `session_label`, and start `run_exchange` against that socket on
/// its own thread.
///
/// `run_exchange` receives the runtime directory and the session id, the two
/// facts it needs to find and open the socket, and the fake backend, which
/// makes a pane's program write output or exit while the session runs. It
/// hands back the connections it wants left open alongside its own value. A
/// connection dropped while the dispatcher is still running detaches its
/// client. The connections handed back close only in
/// [`ServedTestSession::finish`]. The exchange thread sends
/// [`RuntimeEvent::Quit`] when it ends.
pub fn start_test_session<ExchangeResult: Send + 'static>(
    session_label: &str,
    startup_config: Option<PartialKoshiConfig>,
    run_exchange: impl FnOnce(PathBuf, SessionId, Arc<FakePtyBackend>) -> (Vec<Connection>, ExchangeResult)
        + Send
        + 'static,
) -> ServedTestSession<ExchangeResult> {
    // The Unix base is `/tmp`, which keeps the socket path under the
    // `sun_path` length limit.
    #[cfg(unix)]
    let socket_path_base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_path_base = std::env::temp_dir();
    let runtime_directory = socket_path_base.join(format!(
        "koshi-runtime-test-{}-{session_label}",
        std::process::id()
    ));

    let session_id = SessionId::new();
    let (mut server, fake_pty_backend, runtime_event_sender) = build_server_with_fake_pty_backend();
    server.load_startup_config(startup_config);
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
        let _dispatcher_stop_guard = DispatcherStopGuard(runtime_event_sender);
        run_exchange(
            exchange_runtime_directory,
            session_id,
            exchange_fake_pty_backend,
        )
    });

    ServedTestSession {
        server,
        fake_pty_backend,
        ipc_server,
        runtime_directory,
        exchange_thread,
    }
}

/// Run `run_exchange` against a session from [`start_test_session`] while this
/// thread drains the runtime inbox and pushes each attached client its frames.
///
/// Returns the server and the fake backend once the exchange is done, so a
/// test can read the session state and the PTY sizes the exchange left behind,
/// plus whatever the exchange itself produced.
pub fn serve_test_session<ExchangeResult: Send + 'static>(
    session_label: &str,
    run_exchange: impl FnOnce(PathBuf, SessionId, Arc<FakePtyBackend>) -> (Vec<Connection>, ExchangeResult)
        + Send
        + 'static,
) -> (Server, Arc<FakePtyBackend>, ExchangeResult) {
    let mut served_test_session = start_test_session(session_label, None, run_exchange);
    let server = &mut served_test_session.server;

    // The per-session server's own loop: block until an event is due, bounded
    // by the next render deadline, apply it, hand a fresh snapshot to any
    // subscriber that lost a critical event, then push every attached client
    // its frame when a render is due.
    loop {
        let current_time = Instant::now();
        let runtime_event = match server.compute_next_render_wakeup(current_time) {
            Some(render_wakeup_timeout) => {
                match server
                    .get_inbox_receiver()
                    .recv_timeout(render_wakeup_timeout)
                {
                    Ok(runtime_event) => Some(runtime_event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match server.get_inbox_receiver().recv() {
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

    served_test_session.finish()
}

/// Connect to the socket the endpoint file advertises and complete the Hello.
/// The returned connection accepts every other request kind.
pub fn open_session_connection(runtime_directory: &Path, session_id: SessionId) -> Connection {
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
                minimum_protocol_version: MIN_PROTOCOL_VERSION,
                maximum_protocol_version: PROTOCOL_VERSION,
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

/// What the session reports about itself over `ipc_connection`.
pub fn get_session_overview(ipc_connection: &mut Connection, request_id: u64) -> SessionOverview {
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
pub fn count_attached_clients(ipc_connection: &mut Connection, request_id: u64) -> usize {
    get_session_overview(ipc_connection, request_id)
        .clients
        .len()
}

/// Ask over `ipc_connection` until the session reports `expected_client_count`
/// clients, numbering the requests from `request_id`. Returns the next unused
/// request id. Panics once [`TEST_WAIT_TIMEOUT_DURATION`] has passed.
pub fn wait_for_client_count(
    ipc_connection: &mut Connection,
    expected_client_count: usize,
    request_id: u64,
) -> u64 {
    let deadline = Instant::now() + TEST_WAIT_TIMEOUT_DURATION;
    let mut request_id = request_id;
    loop {
        let observed_client_count = count_attached_clients(ipc_connection, request_id);
        request_id += 1;
        if observed_client_count == expected_client_count {
            return request_id;
        }
        assert!(
            Instant::now() < deadline,
            "the session reports {observed_client_count} attached clients, not {expected_client_count}",
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Submit a command over `ipc_connection`, as an external `koshi` invocation
/// naming `session_id` and no client, and return the events it emitted.
/// Panics unless the session applied it.
pub fn submit_test_command(
    ipc_connection: &mut Connection,
    session_id: SessionId,
    command: Command,
    request_id: u64,
) -> Vec<Event> {
    submit_test_command_for_client(ipc_connection, session_id, None, command, request_id)
}

/// Submit a command over `ipc_connection`, as an external `koshi` invocation
/// naming `session_id` and targeting `target_client_id`, and return the events
/// it emitted. Panics unless the session applied it.
pub fn submit_test_command_for_client(
    ipc_connection: &mut Connection,
    session_id: SessionId,
    target_client_id: Option<ClientId>,
    command: Command,
    request_id: u64,
) -> Vec<Event> {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), target_client_id),
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

/// Read `ipc_connection`'s event stream until `accepts_session_event` accepts
/// an event, on a thread this one can give up waiting on. Returns the
/// connection, still open, and every frame read, the accepted one last.
/// Panics once [`TEST_WAIT_TIMEOUT_DURATION`] has passed with no accepted
/// frame.
///
/// Each frame is decoded as a [`SessionEvent`], so a response frame written on
/// an attached client's connection fails the read: an [`IpcResponse`] encodes
/// as a two-field record, a `SessionEvent` as a one-field one.
pub fn read_session_frames_until(
    mut ipc_connection: Connection,
    accepts_session_event: impl Fn(&SessionEvent) -> bool + Send + 'static,
) -> (Connection, Vec<SessionEvent>) {
    let (read_result_sender, read_result_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut session_events = Vec::new();
        loop {
            let session_event: SessionEvent = ipc_connection.recv().expect("an event frame");
            let is_accepted_session_event = accepts_session_event(&session_event);
            session_events.push(session_event);
            if is_accepted_session_event {
                break;
            }
        }
        let _ = read_result_sender.send((ipc_connection, session_events));
    });
    read_result_receiver
        .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
        .expect("the awaited frame reaches the viewer")
}

/// [`read_session_frames_until`] stopping at [`SessionEvent::Detached`].
pub fn read_session_frames_to_detached(
    ipc_connection: Connection,
) -> (Connection, Vec<SessionEvent>) {
    read_session_frames_until(ipc_connection, |session_event| {
        *session_event == SessionEvent::Detached
    })
}

/// The painted frame `session_events` ends with. Panics unless the last frame
/// read is a painted one.
pub fn get_last_painted_frame(session_events: &[SessionEvent]) -> &PaintedFrame {
    match session_events.last() {
        Some(SessionEvent::Painted {
            frame: painted_frame,
        }) => painted_frame,
        other_session_event => {
            panic!("expected the run to end with a painted frame, got {other_session_event:?}")
        }
    }
}
