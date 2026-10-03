//! One session server on a thread of the test process, serving a real control
//! socket in a fresh temporary runtime directory over a [`FakePtyBackend`].
//!
//! [`RunningSession::start_session`] seeds the session, binds the socket, and
//! returns once the endpoint file is readable. Dropping the [`RunningSession`]
//! queues a `Quit`, joins the serving thread, and removes the directory.
//! [`attach_test_client`] joins the session over the socket as an attached
//! client, and a thread of its own forwards what the session writes to it.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::CommandResult;
use koshi_core::discovery::SessionOverview;
use koshi_core::event::Event;
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{IpcResponse, IpcResult};
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::pty_inbox::InboxSink;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_test_runtime_directory;
use tempfile::TempDir;

use super::session_connection::{
    build_attach_request, open_session_connection, ATTACH_VIEWPORT_SIZE,
};
use super::WAIT_DURATION;

/// How long [`RunningSession::start_session`] pauses between reads of the
/// endpoint file: 10 milliseconds.
const SESSION_ADVERTISE_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// One session server running on its own thread, serving a real control socket
/// in its own runtime directory over a fake PTY backend.
pub struct RunningSession {
    /// The runtime directory the control socket and endpoint file live in.
    pub runtime_directory: TempDir,
    /// The session the server seeded and serves, named `quiet-lake`.
    pub session_id: SessionId,
    /// The backend that stands in for the panes' children. It records every
    /// spawn, resize and kill, and takes pane output a test pushes.
    pub fake_pty_backend: Arc<FakePtyBackend>,
    /// The runtime inbox. Dropping the session sends `Quit` on it.
    inbox_sender: mpsc::Sender<RuntimeEvent>,
    /// The thread serving the session, joined at drop.
    serving_thread: Option<JoinHandle<()>>,
}

impl RunningSession {
    /// Start a session server on its own thread and return once its endpoint
    /// file is readable.
    ///
    /// # Panics
    /// When the endpoint file is still unreadable after [`WAIT_DURATION`].
    pub fn start_session() -> RunningSession {
        let runtime_directory = build_test_runtime_directory();
        let session_id = SessionId::new();
        let (inbox_sender, inbox_receiver) = mpsc::channel();
        let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
            InboxSink::from_event_sender(inbox_sender.clone()),
        )));

        let serving_runtime_directory = runtime_directory.path().to_path_buf();
        let serving_pty_backend = Arc::clone(&fake_pty_backend);
        let serving_inbox_sender = inbox_sender.clone();
        let serving_thread = std::thread::spawn(move || {
            serve_session(
                &serving_runtime_directory,
                session_id,
                serving_pty_backend,
                inbox_receiver,
                serving_inbox_sender,
            );
        });

        let running_session = RunningSession {
            runtime_directory,
            session_id,
            fake_pty_backend,
            inbox_sender,
            serving_thread: Some(serving_thread),
        };
        let advertise_deadline = Instant::now() + WAIT_DURATION;
        while EndpointFile::load_from_path(&running_session.resolve_endpoint_file_path()).is_err() {
            assert!(
                Instant::now() < advertise_deadline,
                "the session server never advertised its socket"
            );
            std::thread::sleep(SESSION_ADVERTISE_POLL_INTERVAL_DURATION);
        }
        running_session
    }

    /// The panes the backend spawned, in spawn order.
    pub fn list_pane_ids(&self) -> Vec<PaneId> {
        self.fake_pty_backend.list_spawned_pane_ids()
    }

    /// The session's own report of itself, read over the control socket by the
    /// library call the `koshi inspect` verbs make.
    ///
    /// # Panics
    /// When the session does not answer.
    pub fn fetch_session_overview(&self) -> SessionOverview {
        koshi_link::discovery::fetch_session_overview(
            self.runtime_directory.path(),
            None,
            self.session_id,
            None,
        )
        .expect("the session server describes itself")
    }

    /// The endpoint file the session server advertises: the socket address and
    /// the token a Hello presents.
    ///
    /// # Panics
    /// When the endpoint file cannot be read.
    pub fn load_session_endpoint(&self) -> EndpointFile {
        EndpointFile::load_from_path(&self.resolve_endpoint_file_path())
            .expect("the session server advertises its socket")
    }

    /// Where the session's endpoint file lives in its runtime directory.
    fn resolve_endpoint_file_path(&self) -> std::path::PathBuf {
        EndpointFile::resolve_endpoint_file_path(self.runtime_directory.path(), self.session_id)
    }
}

impl Drop for RunningSession {
    /// Queue a `Quit` and join the serving thread. A serving thread that has
    /// already stopped leaves a closed inbox, and the send fails without effect.
    fn drop(&mut self) {
        let _ = self.inbox_sender.send(RuntimeEvent::Quit);
        if let Some(serving_thread) = self.serving_thread.take() {
            let _ = serving_thread.join();
        }
    }
}

/// Build one session's server on `fake_pty_backend`, seed the session named
/// `quiet-lake`, bind its control socket in `runtime_directory`, and serve the
/// runtime inbox until the session ends.
///
/// The session is seeded before the socket binds.
fn serve_session(
    runtime_directory: &Path,
    session_id: SessionId,
    fake_pty_backend: Arc<FakePtyBackend>,
    inbox_receiver: mpsc::Receiver<RuntimeEvent>,
    inbox_sender: mpsc::Sender<RuntimeEvent>,
) {
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend;
    let mut session_server = Server::from_runtime_parts(pty_backend, inbox_receiver);
    session_server.load_startup_config(None);
    session_server
        .bootstrap_session(
            session_id,
            "quiet-lake".to_string(),
            ATTACH_VIEWPORT_SIZE,
            SystemTime::now(),
            None,
        )
        .expect("the session is seeded");

    let ipc_server = IpcServer::start(runtime_directory, session_id, inbox_sender, None, None)
        .expect("the control socket binds");
    session_server.attach_ipc_server(ipc_server);

    run_session_event_loop(&mut session_server);
    session_server.shutdown();
}

/// Serve the runtime inbox until the session ends: block until an event is due
/// (bounded by the next render deadline), apply it and any others already
/// queued, hand a fresh snapshot to any subscriber that lost a critical event,
/// push every attached client its frame when a render is due, and stop once the
/// inbox loses its last sender, a hangup arrives, a quit is applied, or no pane
/// is left running.
fn run_session_event_loop(session_server: &mut Server) {
    loop {
        let current_time = Instant::now();
        let pending_runtime_event = match session_server.compute_next_render_wakeup(current_time) {
            Some(timeout_duration) => {
                match session_server
                    .get_inbox_receiver()
                    .recv_timeout(timeout_duration)
                {
                    Ok(runtime_event) => Some(runtime_event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match session_server.get_inbox_receiver().recv() {
                Ok(runtime_event) => Some(runtime_event),
                Err(_) => break,
            },
        };
        let mut is_quit_requested = false;
        if let Some(runtime_event) = pending_runtime_event {
            is_quit_requested |= session_server
                .handle_runtime_event(runtime_event)
                .is_break();
        }
        while let Ok(runtime_event) = session_server.get_inbox_receiver().try_recv() {
            is_quit_requested |= session_server
                .handle_runtime_event(runtime_event)
                .is_break();
        }
        session_server.resync_lagged();
        if session_server.poll_render(Instant::now()) {
            session_server.push_frames();
        }
        if is_quit_requested
            || session_server.is_quit_requested()
            || !session_server.has_active_panes()
        {
            break;
        }
    }
}

/// A client attached over the control socket, with its event stream drained by
/// its own thread into a queue the test reads.
pub struct AttachedClient {
    /// The client the session minted for this connection.
    pub client_id: ClientId,
    /// The session's structure as the attach reply reported it.
    pub session_structure: AttachedSessionStructureSnapshot,
    /// Every frame the session wrote to this client but
    /// [`SessionEvent::Painted`], in arrival order.
    pub session_events: mpsc::Receiver<SessionEvent>,
}

/// Attach to `session` straight over its control socket — Hello then Attach on
/// one connection — and hand back the client the server minted, the structure
/// it was given, and its event stream.
///
/// The connection moves into the thread [`forward_session_events`] starts,
/// which ends when the session closes it or the [`AttachedClient`] is dropped.
///
/// # Panics
/// When the attach reply is not `Attached` for request `2` and this session.
pub fn attach_test_client(session: &RunningSession) -> AttachedClient {
    let mut connection = open_session_connection(&session.load_session_endpoint());
    connection
        .send(&build_attach_request())
        .expect("the server reads the attach");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(ipc_response.request_id, Some(2));
    let IpcResult::Attached {
        client_id,
        session_id,
        session_structure,
        ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    assert_eq!(session_id, session.session_id);

    AttachedClient {
        client_id,
        session_structure,
        session_events: forward_session_events(move || connection.recv::<SessionEvent>()),
    }
}

/// Call `receive_session_event` on a thread of its own until it fails, and
/// forward every event but [`SessionEvent::Painted`] into the queue this
/// returns. The thread also ends once the queue's receiver is dropped.
pub fn forward_session_events(
    mut receive_session_event: impl FnMut() -> Result<SessionEvent, IpcError> + Send + 'static,
) -> mpsc::Receiver<SessionEvent> {
    let (session_event_sender, session_events) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(session_event) = receive_session_event() {
            if let SessionEvent::Painted { .. } = session_event {
                continue;
            }
            if session_event_sender.send(session_event).is_err() {
                break;
            }
        }
    });
    session_events
}

/// The events an applied result carries.
///
/// # Panics
/// When `command_result` is a rejection.
pub fn list_emitted_events(command_result: &CommandResult) -> &[Event] {
    match command_result {
        CommandResult::Ok { emitted_events, .. } => emitted_events,
        unexpected_result => panic!("expected an applied command, got {unexpected_result:?}"),
    }
}
