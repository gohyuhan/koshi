//! Cross-process tests for a session server that replaces its own process
//! image: a real `koshi serve-session` runs with real panes and real child
//! processes, a real client is attached over its control socket, and the test
//! asks it to restart into the binary it was started from.
//!
//! What only a test like this reaches: the readers being held still, a pane's
//! terminal crossing the swap, the swap itself, and the session that comes back
//! afterwards. None of those exist inside one process.
//!
//! Every test serves its own temporary runtime directory and its own home
//! directory, so the session servers here never meet the one a developer is
//! running and never read a developer's `koshi.kdl`. Both sit under a short
//! base because a Unix socket path has an operating-system length cap that a
//! deep temporary path would break.
//!
//! Reading a frame blocks forever, so every event stream is read on a thread of
//! its own: a session that stops answering fails the test on a deadline instead
//! of hanging the suite. That deadline is also what proves a live child does not
//! hold the swap up.
//!
//! Nothing here is gated to one operating system. Where the evidence itself is
//! platform-specific — the process id that `execvp` keeps on Unix, the handover
//! to a new process on Windows — the test branches inside the assertion.
//!
//! Every process a test starts is held in a guard that ends it when the test
//! drops it, so a failed assertion leaves nothing running.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs, FocusTarget,
    NewPaneArgs, WriteToPaneArgs,
};
use koshi_core::discovery::{PaneLifecycle, SessionOverview};
use koshi_core::event::Event;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_ipc::endpoint::{resolve_resume_file_path, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, WireMouseAction,
};
use koshi_ipc::router::{
    resolve_router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress,
};
use koshi_ipc::transport::{Connection, FrameWriter};
use koshi_layout::mode::LayoutMode;
use tempfile::TempDir;

mod common;

use common::{copy_koshi_binary, start_koshi_process, terminate_process};

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure. It is also the ceiling on a swap: a session that has
/// not come back by then has wedged, which is the failure this suite exists to
/// catch.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const RESTART_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How long a test reads on for frames that must not arrive, before it calls
/// their absence settled.
const SETTLE_DURATION: Duration = Duration::from_millis(750);

/// How long a test waits after reading the restart frame before it sends the
/// input a user typed into the window the swap leaves open. The session waits a
/// second for its clients to leave, so this sits inside that second and well
/// past the moment the session stops reading them on its own.
const TYPED_AFTER_RESTART_FRAME_DURATION: Duration = Duration::from_millis(250);

/// How long a test waits for the session server to detach a client record
/// nobody claimed. The session server holds such a record for thirty seconds;
/// the rest is room for the swap and for the poll that watches.
const RECONNECT_WAIT_DURATION: Duration = Duration::from_secs(75);

/// The display name the session server is started under, standing in for the
/// one the router generates.
const SESSION_SERVER_NAME: &str = "workspace";

/// The terminal size the attaching client reports in the tests that need a pane
/// short enough for output to scroll off the top of it.
const SHORT_ATTACH_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The terminal size the attaching client reports in the tests that need every
/// line a pane printed to stay on screen.
const TALL_ATTACH_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 40,
};

/// A fresh directory, under a short base so the Unix socket path stays inside
/// the operating system's path-length cap. Removed when the test drops it.
fn build_short_test_directory() -> TempDir {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(base)
        .expect("a temporary directory")
}

/// A session server the test started. Dropping it ends that server.
struct RunningSession {
    /// The process the test started.
    child_process: Child,
    /// The pipe the ready line was read from, held open for as long as the
    /// guard lives: on Unix the image replacing this one inherits that pipe and
    /// writes its own ready line into it.
    _ready_output_reader: BufReader<ChildStdout>,
}

impl RunningSession {
    /// True once the process the test started has ended. On Windows a restart
    /// hands over to a new process and this one ends; on Unix it keeps running,
    /// under the same process id, as the new image.
    fn has_session_server_exited(&mut self) -> bool {
        self.child_process
            .try_wait()
            .expect("the session server's state can be read")
            .is_some()
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// A process the test did not start itself, held by its process id. Dropping it
/// ends that process.
///
/// A restart is guarded with one of these: on Windows the session runs in a
/// process the test never spawned, and on Unix it is the process the test
/// already holds, which a second ending does nothing to.
struct RunningProcess {
    process_id: u32,
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        terminate_process(self.process_id);
    }
}

/// A router the test started. Dropping it ends that router.
struct RunningRouter {
    child_process: Child,
}

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// The `koshi` binary at `binary_path`, set to keep its files under `test_home_directory` rather than
/// in the developer's own directories, and stripped of the pane identity so it
/// never reads the session a developer runs the test from.
///
/// Every variable the platform path resolvers read is pointed at
/// `test_home_directory`, on every platform, so the config file this process
/// reads is the one the test directory holds — none, which leaves every setting
/// at its built-in default. `KOSHI_RUNTIME_DIR` names
/// `<test_home_directory>/run`, which every caller here then
/// overrides with its own `--runtime-dir` argument.
fn build_koshi_command(binary_path: &Path, test_home_directory: &Path) -> std::process::Command {
    let mut process_command = std::process::Command::new(binary_path);
    process_command
        .env("HOME", test_home_directory)
        .env("USERPROFILE", test_home_directory)
        .env("KOSHI_RUNTIME_DIR", test_home_directory.join("run"))
        .env("XDG_CONFIG_HOME", test_home_directory.join("config"))
        .env("XDG_DATA_HOME", test_home_directory.join("data"))
        .env("XDG_STATE_HOME", test_home_directory.join("state"))
        .env("APPDATA", test_home_directory.join("roaming"))
        .env("LOCALAPPDATA", test_home_directory.join("local"))
        // The five variables the runtime injects at pane spawn; `KOSHI` is the
        // marker a nested koshi reads, and a test run from inside a koshi pane
        // would hand every one of them to this child.
        .env_remove("KOSHI")
        .env_remove("KOSHI_SESSION_ID")
        .env_remove("KOSHI_CLIENT_ID")
        .env_remove("KOSHI_PANE_ID")
        .env_remove("KOSHI_SOCKET")
        .stdin(Stdio::null())
        // The session server's own log reaches the test run's output, so a
        // failure here is read beside the reason the server gave for it.
        .stderr(Stdio::inherit());
    // The shell a seeded pane launches, so every platform opens the same one
    // whatever the developer's login shell is. Windows reads `COMSPEC`.
    #[cfg(unix)]
    process_command.env("SHELL", "/bin/sh");
    process_command
}

/// Start the binary at `binary_path` as one session's server, under the identity the
/// router would have handed it, and wait for the ready line it prints once its
/// control_connection socket is bound.
fn start_session_server(
    binary_path: &Path,
    test_home_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> RunningSession {
    let mut child_process = start_koshi_process(
        build_koshi_command(binary_path, test_home_directory)
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_SERVER_NAME)
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .stdout(Stdio::piped()),
    );
    let mut ready_output_reader = BufReader::new(
        child_process
            .stdout
            .take()
            .expect("the ready line is a pipe"),
    );
    let mut ready_line = String::new();
    ready_output_reader
        .read_line(&mut ready_line)
        .expect("the session server prints where it listens");
    assert!(
        ready_line.contains("\"socket_address\""),
        "the ready line named no socket: {ready_line}"
    );
    RunningSession {
        child_process,
        _ready_output_reader: ready_output_reader,
    }
}

/// Start the `koshi` binary as the router serving `runtime_directory`.
fn start_router_process(test_home_directory: &Path, runtime_directory: &Path) -> RunningRouter {
    let child_process = start_koshi_process(
        build_koshi_command(Path::new(env!("CARGO_BIN_EXE_koshi")), test_home_directory)
            .arg("serve-router")
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .stdout(Stdio::null()),
    );
    RunningRouter { child_process }
}

/// Open a connection to `session_id`'s control_connection socket with its handshake
/// already done, retrying until the session answers, and hand back the endpoint
/// file the socket was advertised in.
fn open_session_connection(
    runtime_directory: &Path,
    session_id: SessionId,
) -> (Connection, EndpointFile) {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(opened_connection) = try_open_session_connection(runtime_directory, session_id)
        {
            return opened_connection;
        }
        assert!(
            Instant::now() < deadline,
            "no session server answered for {session_id}"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a connection: read the endpoint file, connect, and
/// send the Hello that opens the connection.
///
/// `None` means the session server has yet to bind its socket and advertise the
/// token the Hello presents; the next attempt reads the file again.
fn try_open_session_connection(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Option<(Connection, EndpointFile)> {
    let session_endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .ok()?;
    let mut connection = Connection::connect(&session_endpoint.socket_address).ok()?;
    let hello_request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(
            session_endpoint.connection_token.clone(),
        ),
    };
    connection.send(&hello_request).ok()?;
    let ipc_response: IpcResponse = connection.recv().ok()?;
    match ipc_response.answer_result {
        IpcResult::Hello { .. } => Some((connection, session_endpoint)),
        IpcResult::Error(_) => None,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Ask the session for `request_kind` on a connection that carries no client's event
/// stream, and hand back its answer.
fn send_session_request(
    connection: &mut Connection,
    request_id: u64,
    request_kind: IpcRequestKind,
) -> IpcResult {
    let ipc_request = IpcRequest {
        request_id,
        request_kind,
    };
    connection
        .send(&ipc_request)
        .expect("the session reads the request");
    let ipc_response: IpcResponse = connection.recv().expect("the session answers the request");
    assert_eq!(ipc_response.request_id, Some(request_id));
    ipc_response.answer_result
}

/// Submit `command` on a control_connection connection to `session_id`, targeting
/// `client_id`, and hand back the events it emitted. A rejected command fails
/// the test.
fn submit_session_command(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    command: Command,
) -> Vec<Event> {
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), Some(client_id)),
        SystemTime::now(),
        command,
    );
    match send_session_request(
        connection,
        7,
        IpcRequestKind::SubmitCommand(Box::new(envelope)),
    ) {
        IpcResult::CommandResult(CommandResult::Ok { emitted_events, .. }) => emitted_events,
        unexpected_result => panic!("the command was answered with {unexpected_result:?}"),
    }
}

/// Split a new pane off the client's focused one, running `spawn_spec`, and hand
/// back the pane the session created. `None` launches the platform shell.
fn build_pane(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    spawn_spec: Option<SpawnSpec>,
) -> PaneId {
    let emitted_events = submit_session_command(
        connection,
        session_id,
        client_id,
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            working_directory: None,
            spawn_spec,
            client_id: Some(client_id),
        }),
    );
    emitted_events
        .iter()
        .find_map(|session_event| match session_event {
            Event::PaneCreated(created_pane) => Some(created_pane.pane_id),
            _ => None,
        })
        .expect("the new pane is announced")
}

/// Type `terminal_line` into `pane_id`, ending it with the carriage return a terminal
/// sends for the Enter key.
fn send_terminal_line(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    pane_id: PaneId,
    terminal_line: &str,
) {
    let mut input_bytes = terminal_line.as_bytes().to_vec();
    input_bytes.push(b'\r');
    submit_session_command(
        connection,
        session_id,
        client_id,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            input_bytes,
        }),
    );
}

/// The session's own description of itself, read over its control socket.
fn fetch_session_overview(runtime_directory: &Path, session_id: SessionId) -> SessionOverview {
    koshi_link::ipc_client::fetch_session_overview(runtime_directory, session_id)
        .expect("the session server describes itself")
}

/// The panes the session holds, in the order it reports them, with the state
/// each one is in.
fn list_pane_lifecycles(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Vec<(PaneId, PaneLifecycle)> {
    let mut pane_lifecycles: Vec<(PaneId, PaneLifecycle)> =
        fetch_session_overview(runtime_directory, session_id)
            .panes
            .into_iter()
            .map(|pane_record| (pane_record.pane_id, pane_record.lifecycle))
            .collect();
    pane_lifecycles.sort_by_key(|(pane_id, _)| *pane_id);
    pane_lifecycles
}

/// The one pane a freshly seeded session holds, before a test opens any of its
/// own.
fn get_seeded_pane_id(runtime_directory: &Path, session_id: SessionId) -> PaneId {
    let recorded_pane_lifecycles = list_pane_lifecycles(runtime_directory, session_id);
    assert_eq!(
        recorded_pane_lifecycles.len(),
        1,
        "a seeded session holds one pane: {recorded_pane_lifecycles:?}"
    );
    recorded_pane_lifecycles[0].0
}

/// Wait for the endpoint file `session_id` to advertise a socket other than
/// the one `endpoint_before_restart` names.
///
/// A session server mints a fresh connection token every time it binds, so a
/// token other than `endpoint_before_restart`'s belongs to the socket the
/// session came back on. This is the same fact an attached client watches for.
fn wait_for_restarted_session_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
    endpoint_before_restart: &EndpointFile,
) -> EndpointFile {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(loaded_endpoint) = EndpointFile::load_from_path(
            &EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id),
        ) {
            if loaded_endpoint.connection_token.expose()
                != endpoint_before_restart.connection_token.expose()
            {
                return loaded_endpoint;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the session advertised no new socket after its restart"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// The process ids whose parent is `parent`, in ascending order, as the
/// operating system reports them.
///
/// Unix only: there a pane's child is the session server's own child, and
/// `execvp` keeps it. On Windows a pane's child belongs to the process holding
/// the panes instead.
#[cfg(unix)]
fn list_child_process_ids(parent_process_id: u32) -> Vec<u32> {
    let process_list_output = std::process::Command::new("ps")
        .arg("-A")
        .arg("-o")
        .arg("pid=,ppid=")
        .output()
        .expect("the process list can be read");
    let process_list_text = String::from_utf8_lossy(&process_list_output.stdout);
    let mut child_process_ids: Vec<u32> = process_list_text
        .lines()
        .filter_map(|process_list_line| {
            let mut process_fields = process_list_line.split_whitespace();
            let process_id: u32 = process_fields.next()?.parse().ok()?;
            let reported_parent_process_id: u32 = process_fields.next()?.parse().ok()?;
            (reported_parent_process_id == parent_process_id).then_some(process_id)
        })
        .collect();
    child_process_ids.sort_unstable();
    child_process_ids
}

/// One attached client's event stream, read on a thread of its own so a test
/// never blocks forever on a frame that does not come, plus the writing half
/// that carries this client's own requests up.
struct AttachedClientStream {
    /// The id the session minted or handed back for this client.
    client_id: ClientId,
    /// Every frame the reading thread has read, in arrival order. The read that
    /// ends the connection arrives here too.
    session_events: Receiver<Result<SessionEvent, IpcError>>,
    /// This client's own uplink.
    request_writer: FrameWriter,
}

impl AttachedClientStream {
    /// Join `session_id` as a viewing client at `viewport`, the way the
    /// attached client joins it: Hello, then Attach on the same connection.
    ///
    /// `resume` names the client record to come back as after the session
    /// replaced its own image, and is `None` on a first attach.
    fn attach_test_client(
        runtime_directory: &Path,
        session_id: SessionId,
        viewport: Size,
        resume: Option<ClientId>,
    ) -> (AttachedClientStream, EndpointFile) {
        let (mut connection, endpoint_file) =
            open_session_connection(runtime_directory, session_id);
        let attach_request = IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport,
                event_filter: EventFilterSpec::All,
                resume_client_id: resume,
                resume_token: None,
                pane_area: None,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        };
        connection
            .send(&attach_request)
            .expect("the session reads the attach");
        let ipc_response: IpcResponse = connection.recv().expect("the session answers the attach");
        assert_eq!(ipc_response.request_id, Some(2));
        let IpcResult::Attached {
            client_id,
            session_id: joined,
            ..
        } = ipc_response.answer_result
        else {
            panic!(
                "expected an attach reply, got {:?}",
                ipc_response.answer_result
            );
        };
        assert_eq!(joined, session_id);

        let (mut session_event_reader, request_writer) = connection.split();
        let (session_events_tx, session_events) = mpsc::channel();
        std::thread::spawn(move || loop {
            let session_event_result = session_event_reader.recv::<SessionEvent>();
            let is_stream_ended = session_event_result.is_err();
            if session_events_tx.send(session_event_result).is_err() || is_stream_ended {
                return;
            }
        });
        (
            AttachedClientStream {
                client_id,
                session_events,
                request_writer,
            },
            endpoint_file,
        )
    }

    /// The next frame on the stream, or the read that ended it. Fails the test
    /// once [`WAIT_DURATION`] has passed with nothing arriving.
    fn receive_next_event(&self) -> Result<SessionEvent, IpcError> {
        match self.session_events.recv_timeout(WAIT_DURATION) {
            Ok(session_event) => session_event,
            Err(RecvTimeoutError::Timeout) => {
                panic!("no frame arrived within {WAIT_DURATION:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("the event stream closed with no frame"),
        }
    }

    /// The first event `event_predicate` accepts. Fails the test once [`WAIT_DURATION`] has
    /// passed with none, naming every event kind that did arrive.
    fn receive_event_when(&self, event_predicate: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = Instant::now() + WAIT_DURATION;
        let mut received_event_names: Vec<&'static str> = Vec::new();
        loop {
            let remaining_duration = deadline.saturating_duration_since(Instant::now());
            match self.session_events.recv_timeout(remaining_duration) {
                Ok(Ok(session_event)) => {
                    if event_predicate(&session_event) {
                        return session_event;
                    }
                    received_event_names.push(session_event.get_event_name());
                }
                Ok(Err(stream_error)) => {
                    panic!("the event stream ended before the event: {stream_error}")
                }
                Err(RecvTimeoutError::Timeout) => panic!(
                    "the event the test was waiting for never arrived within {WAIT_DURATION:?}; \
                     the stream carried {received_event_names:?}"
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the event stream closed before the event")
                }
            }
        }
    }

    /// The next painted frame, skipping every frame that carries no picture.
    /// Fails the test on a stream that ends first.
    fn receive_next_painted_frame(&self) -> PaintedFrame {
        loop {
            match self.receive_next_event() {
                Ok(SessionEvent::Painted {
                    frame: painted_frame,
                }) => return *painted_frame,
                Ok(_) => {}
                Err(stream_error) => {
                    panic!("the event stream ended before a frame: {stream_error}")
                }
            }
        }
    }

    /// The first painted frame `frame_predicate` accepts. Fails the test once [`WAIT_DURATION`]
    /// has passed with none, naming what the last painted frame held.
    fn receive_painted_frame_when(
        &self,
        frame_predicate: impl Fn(&PaintedFrame) -> bool,
    ) -> PaintedFrame {
        let deadline = Instant::now() + WAIT_DURATION;
        let mut last_painted_frame: Option<PaintedFrame> = None;
        loop {
            let remaining_duration = deadline.saturating_duration_since(Instant::now());
            match self.session_events.recv_timeout(remaining_duration) {
                Ok(Ok(SessionEvent::Painted {
                    frame: painted_frame,
                })) => {
                    if frame_predicate(&painted_frame) {
                        return *painted_frame;
                    }
                    last_painted_frame = Some(*painted_frame);
                }
                Ok(Ok(_)) => {}
                Ok(Err(stream_error)) => {
                    panic!("the event stream ended before a frame: {stream_error}")
                }
                Err(RecvTimeoutError::Timeout) => panic!(
                    "no frame showed what the test was waiting for within {WAIT_DURATION:?}; \
                     the last painted frame held {}",
                    last_painted_frame.as_ref().map_or(
                        "no painted frame at all".to_string(),
                        describe_painted_frame
                    )
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the event stream closed with no frame")
                }
            }
        }
    }

    /// Read to the frame that says the session is replacing its own image, and
    /// hand back everything read before it.
    fn receive_events_until_restarting(&self) -> Vec<SessionEvent> {
        let mut received_events = Vec::new();
        loop {
            match self.receive_next_event() {
                Ok(SessionEvent::Restarting) => return received_events,
                Ok(session_event) => received_events.push(session_event),
                Err(stream_error) => {
                    panic!("the event stream ended before the restart: {stream_error}")
                }
            }
        }
    }

    /// The last painted frame to arrive in the next `time_window`, and `None` when
    /// no frame is painted in it.
    fn receive_last_painted_frame(&self, time_window: Duration) -> Option<PaintedFrame> {
        self.drain_session_events(time_window)
            .into_iter()
            .filter_map(|session_event| match session_event {
                SessionEvent::Painted {
                    frame: painted_frame,
                } => Some(*painted_frame),
                _ => None,
            })
            .next_back()
    }

    /// Everything that arrives in the next `time_window`. A stream that ends inside
    /// it contributes the frames it delivered first.
    fn drain_session_events(&self, time_window: Duration) -> Vec<SessionEvent> {
        let deadline = Instant::now() + time_window;
        let mut received_events = Vec::new();
        while let Some(remaining_duration) = deadline.checked_duration_since(Instant::now()) {
            match self.session_events.recv_timeout(remaining_duration) {
                Ok(Ok(session_event)) => received_events.push(session_event),
                Ok(Err(_)) | Err(_) => return received_events,
            }
        }
        received_events
    }

    /// Send one request up this client's own connection. The streaming half
    /// writes no response, so the answer is whatever reaches the event stream.
    fn send_session_request(&mut self, request_id: u64, request_kind: IpcRequestKind) {
        let ipc_request = IpcRequest {
            request_id,
            request_kind,
        };
        self.request_writer
            .send(&ipc_request)
            .expect("the session reads the request");
    }

    /// Send one request up this client's own connection, passing over a
    /// connection the session has already closed. This is how a client behaves
    /// while the session is replacing its image: it keeps sending until the
    /// socket under it goes.
    fn send_request_while_connection_open(
        &mut self,
        request_id: u64,
        request_kind: IpcRequestKind,
    ) {
        let ipc_request = IpcRequest {
            request_id,
            request_kind,
        };
        let _ = self.request_writer.send(&ipc_request);
    }

    /// Move this client's view of `pane_id` up into scrollback by `scroll_line_count`, and
    /// hand back the first frame painted after the session answered the round.
    ///
    /// The session answers exactly one round per request, so waiting for that
    /// answer is what makes the frame after it the scrolled one.
    fn scroll_pane_up(&mut self, pane_id: PaneId, scroll_line_count: usize) -> PaintedFrame {
        self.send_session_request(
            11,
            IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count,
            }]),
        );
        loop {
            match self.receive_next_event() {
                Ok(SessionEvent::MouseAnswer { request_id, .. }) => {
                    assert_eq!(request_id, 11);
                    return self.receive_next_painted_frame();
                }
                Ok(_) => {}
                Err(stream_error) => {
                    panic!("the event stream ended before the scroll answer: {stream_error}")
                }
            }
        }
    }
}

/// The rows `pane_id` shows in `painted_frame`, each with its trailing blanks cut off and
/// every blank row at the bottom dropped.
///
/// A blank row between two rows of text is kept, so a gap in a pane's output is
/// visible in what this returns.
///
/// Empty when the frame carries no content for `pane_id`, and when the pane shows
/// no cells. A frame painted before the pane existed carries no content for it.
fn get_pane_rows(painted_frame: &PaintedFrame, pane_id: PaneId) -> Vec<String> {
    let Some(pane_snapshot) = painted_frame
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
    else {
        return Vec::new();
    };
    let Some(terminal_window) = pane_snapshot.terminal_window.as_ref() else {
        return Vec::new();
    };
    let mut row_texts: Vec<String> = terminal_window
        .row_snapshots
        .iter()
        .map(|row_snapshot| {
            row_snapshot
                .expand_cells()
                .iter()
                .filter(|cell_snapshot| cell_snapshot.cell_width > 0)
                .map(|cell_snapshot| cell_snapshot.character)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    while row_texts.last().is_some_and(String::is_empty) {
        row_texts.pop();
    }
    row_texts
}

/// Every pane in `painted_frame`, each with the rows it shows, for a failure message.
///
/// Before → after: a frame holding one pane that printed one line reads
/// `pane-… : ["printed-1"]`.
fn describe_painted_frame(painted_frame: &PaintedFrame) -> String {
    if painted_frame.pane_snapshots.is_empty() {
        return "no panes".to_string();
    }
    painted_frame
        .pane_snapshots
        .iter()
        .map(|pane_snapshot| {
            format!(
                "{} : {:?}",
                pane_snapshot.pane_id,
                get_pane_rows(painted_frame, pane_snapshot.pane_id)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many scrollback lines `pane_id` holds in `painted_frame`.
fn get_retained_line_count(painted_frame: &PaintedFrame, pane_id: PaneId) -> usize {
    painted_frame
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
        .unwrap_or_else(|| panic!("the frame carries no content for pane {pane_id}"))
        .scrollback_meta
        .retained_line_count
}

/// A child that prints `count` lines reading `<prefix>-1` to `<prefix>-count`
/// as fast as it can, and then stays alive with nothing more to say.
fn build_burst_spawn_spec(line_prefix: &str, line_count: u32) -> SpawnSpec {
    #[cfg(unix)]
    let script = format!(
        "line_number=1; while [ $line_number -le {line_count} ]; do printf '{line_prefix}-%d\\n' $line_number; line_number=$((line_number+1)); done; \
         sleep 300"
    );
    // The loop is parenthesised, which ends its body at the closing bracket.
    // `&` alone does not: it reads as one more command inside the body, and the
    // wait below then runs on the first pass instead of after the last one.
    #[cfg(windows)]
    let script = format!(
        "(for /L %i in (1,1,{line_count}) do @echo {line_prefix}-%i) & ping -n 301 127.0.0.1 >nul"
    );
    build_shell_spawn_spec(&script)
}

/// A child that prints `count` lines reading `<prefix>-1` to `<prefix>-count`
/// with a pause between them, and then stays alive.
///
/// The pause is what lets a test send the restart while the child is still
/// printing, so the output really does cross the swap.
fn build_paced_spawn_spec(line_prefix: &str, line_count: u32) -> SpawnSpec {
    #[cfg(unix)]
    let script = format!(
        "line_number=1; while [ $line_number -le {line_count} ]; do printf '{line_prefix}-%d\\n' $line_number; line_number=$((line_number+1)); \
         sleep 0.25; done; sleep 300"
    );
    #[cfg(windows)]
    let script = format!(
        "(for /L %i in (1,1,{line_count}) do @(echo {line_prefix}-%i & ping -n 2 127.0.0.1 >nul)) & \
         ping -n 301 127.0.0.1 >nul"
    );
    build_shell_spawn_spec(&script)
}

/// A child that prints nothing and never exits: the case whose reader has
/// nothing to read and whose process cannot be waited on.
fn build_idle_spawn_spec() -> SpawnSpec {
    #[cfg(unix)]
    let script = "sleep 300".to_string();
    #[cfg(windows)]
    let script = "ping -n 301 127.0.0.1 >nul".to_string();
    build_shell_spawn_spec(&script)
}

/// A child that waits for one key and then exits reporting success.
fn build_key_then_exit_spawn_spec() -> SpawnSpec {
    #[cfg(unix)]
    let script = "read line; exit 0".to_string();
    // `set /p` reads a line, the way `read` does. `pause` is not the same
    // thing: it takes a key event, which is not what a client writing bytes to
    // a pane produces.
    #[cfg(windows)]
    let script = "set /p x= & exit 0".to_string();
    build_shell_spawn_spec(&script)
}

/// Run `script` through the platform's own command interpreter.
fn build_shell_spawn_spec(shell_script: &str) -> SpawnSpec {
    #[cfg(unix)]
    let (program, shell_command_flag) = (PathBuf::from("/bin/sh"), "-c");
    #[cfg(windows)]
    let (program, shell_command_flag) = (PathBuf::from("cmd.exe"), "/C");
    SpawnSpec {
        shell_kind: ShellKind::from_program(&program),
        program,
        arguments: vec![shell_command_flag.to_string(), shell_script.to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
    }
}

#[test]
fn a_restart_keeps_every_pane_its_child_its_screen_and_its_scrollback() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let mut session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    // A short terminal, so the thirty lines each pane prints do not all fit and
    // the ones above the top land in scrollback.
    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            SHORT_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let left_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_burst_spawn_spec("left", 30)),
    );
    let right_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_burst_spawn_spec("right", 30)),
    );

    let initial_painted_frame =
        attached_client_stream.receive_painted_frame_when(|painted_frame| {
            get_pane_rows(painted_frame, left_pane_id)
                .last()
                .is_some_and(|row| row == "left-30")
                && get_pane_rows(painted_frame, right_pane_id)
                    .last()
                    .is_some_and(|row| row == "right-30")
        });
    // The last line each child printed ends with a newline, and that newline
    // moves the view one row on. The frame above can be the one painted between
    // the line and its newline, so this takes the last frame painted once the
    // children have gone quiet, and compares that against the swap.
    let settled_frame = attached_client_stream
        .receive_last_painted_frame(SETTLE_DURATION)
        .unwrap_or(initial_painted_frame);
    let left_settled_rows = get_pane_rows(&settled_frame, left_pane_id);
    let right_settled_rows = get_pane_rows(&settled_frame, right_pane_id);
    let left_retained_lines = get_retained_line_count(&settled_frame, left_pane_id);
    let right_retained_lines = get_retained_line_count(&settled_frame, right_pane_id);

    #[cfg(unix)]
    let children_before_restart = list_child_process_ids(endpoint_before_restart.process_id);

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    attached_client_stream.receive_events_until_restarting();
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    let (mut attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        SHORT_ATTACH_VIEWPORT_SIZE,
        None,
    );

    // The session that came back holds every pane it held, each with its child
    // still running.
    let mut expected_pane_lifecycles = vec![
        (seeded_pane_id, PaneLifecycle::Running),
        (left_pane_id, PaneLifecycle::Running),
        (right_pane_id, PaneLifecycle::Running),
    ];
    expected_pane_lifecycles.sort_by_key(|(pane_id, _)| *pane_id);
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        expected_pane_lifecycles
    );

    // Every cell each pane showed before the swap is on screen again.
    let painted_frame = attached_client_stream.receive_painted_frame_when(|painted_frame| {
        get_pane_rows(painted_frame, left_pane_id) == left_settled_rows
    });
    assert_eq!(
        get_pane_rows(&painted_frame, left_pane_id),
        left_settled_rows
    );
    assert_eq!(
        get_pane_rows(&painted_frame, right_pane_id),
        right_settled_rows
    );
    assert_eq!(
        get_retained_line_count(&painted_frame, left_pane_id),
        left_retained_lines
    );
    assert_eq!(
        get_retained_line_count(&painted_frame, right_pane_id),
        right_retained_lines
    );

    // The lines that scrolled off the top are still in scrollback: the view
    // moved to the oldest line it retains shows the first line each child ever
    // printed.
    let left_scrollback_frame = attached_client_stream.scroll_pane_up(left_pane_id, usize::MAX);
    assert_eq!(
        get_pane_rows(&left_scrollback_frame, left_pane_id)
            .first()
            .map(String::as_str),
        Some("left-1")
    );
    let right_scrollback_frame = attached_client_stream.scroll_pane_up(right_pane_id, usize::MAX);
    assert_eq!(
        get_pane_rows(&right_scrollback_frame, right_pane_id)
            .first()
            .map(String::as_str),
        Some("right-1")
    );

    #[cfg(unix)]
    {
        // The swap replaced this process's running image, so the session serves
        // under the process id it started with and every pane's child kept the
        // same parent and the same process id.
        assert!(!session_server_process.has_session_server_exited());
        assert_eq!(
            restarted_endpoint.process_id,
            endpoint_before_restart.process_id
        );
        assert_eq!(
            list_child_process_ids(restarted_endpoint.process_id),
            children_before_restart
        );
    }
    #[cfg(windows)]
    {
        // The swap handed over to a new process, which took the panes back from
        // the process holding them — the one that outlived both session
        // servers.
        let deadline = Instant::now() + WAIT_DURATION;
        while !session_server_process.has_session_server_exited() {
            assert!(
                Instant::now() < deadline,
                "the session server that handed over kept running"
            );
            std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
        }
        assert_ne!(
            restarted_endpoint.process_id,
            endpoint_before_restart.process_id
        );
    }
}

#[test]
fn output_written_across_the_swap_arrives_once_and_in_order() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    // A tall terminal, so every line the child prints stays on screen and the
    // run can be read whole.
    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let output_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_paced_spawn_spec("mark", 12)),
    );

    // The restart goes out while the child is still printing, so the run has to
    // cross the parking, the swap and the reader coming back.
    attached_client_stream.receive_painted_frame_when(|painted_frame| {
        get_pane_rows(painted_frame, output_pane_id).contains(&"mark-3".to_string())
    });
    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    attached_client_stream.receive_events_until_restarting();
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    let (attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
    );

    let completed_painted_frame =
        attached_client_stream.receive_painted_frame_when(|painted_frame| {
            get_pane_rows(painted_frame, output_pane_id).contains(&"mark-12".to_string())
        });
    let expected_output_rows: Vec<String> = (1..=12).map(|mark| format!("mark-{mark}")).collect();
    assert_eq!(
        get_pane_rows(&completed_painted_frame, output_pane_id),
        expected_output_rows
    );
}

#[test]
fn input_sent_after_the_clients_are_told_still_reaches_its_pane() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (mut attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    // A child that ends the moment it reads one line, so the panes the session
    // holds after the swap say whether the line reached it.
    let input_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_key_then_exit_spawn_spec()),
    );
    let mut expected_pane_lifecycles = vec![
        (seeded_pane_id, PaneLifecycle::Running),
        (input_pane_id, PaneLifecycle::Running),
    ];
    expected_pane_lifecycles.sort_by_key(|(pane_id, _)| *pane_id);
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        expected_pane_lifecycles
    );

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    // A client learns the session is going only when it reads this frame, so
    // what follows is what a user types into the window the swap leaves open.
    attached_client_stream.receive_events_until_restarting();
    // Long enough that the swap has reached the point where it stops reading
    // its clients, and short enough to be well inside the wait it gives them.
    // Sending the moment the frame arrives would land in the microseconds
    // before that point and prove nothing.
    std::thread::sleep(TYPED_AFTER_RESTART_FRAME_DURATION);
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(attached_client_stream.client_id),
        SystemTime::now(),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(input_pane_id),
            input_bytes: b"typed\r".to_vec(),
        }),
    );
    attached_client_stream
        .send_request_while_connection_open(20, IpcRequestKind::SubmitCommand(Box::new(envelope)));
    // What a real client sends the moment it reads that frame. Requests arrive
    // in the order they were queued, so the session reads the line above before
    // it reads this, and this is what the swap waits for.
    attached_client_stream.send_request_while_connection_open(21, IpcRequestKind::Leaving);
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };

    // The line reached the child: it read the line, exited, and its pane closed
    // with it. A swap that dropped the line leaves that child waiting, and the
    // pane open, until this deadline fails the test.
    let deadline = Instant::now() + WAIT_DURATION;
    while list_pane_lifecycles(runtime_directory.path(), session_id)
        != vec![(seeded_pane_id, PaneLifecycle::Running)]
    {
        assert!(
            Instant::now() < deadline,
            "the pane the line was sent to is still open, so its child never read it"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

#[test]
fn a_client_that_never_leaves_does_not_hold_the_swap_up() {
    // The swap waits for every told client to say it is leaving. A client whose
    // window froze never says it, so the wait is bounded: the session cuts the
    // connections still open and carries itself out anyway.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    // The stream is held open and says nothing from here on.
    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };

    // The session came back with the pane it was carrying, so the cut cost it
    // nothing it held.
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        vec![(seeded_pane_id, PaneLifecycle::Running)]
    );
    drop(attached_client_stream);
}

#[test]
fn a_pane_a_running_session_opened_prints_what_its_child_wrote() {
    // No restart here. This asks the one question every test above it takes for
    // granted: does a pane a real session server opened print at all?
    //
    // On Windows that pane's terminal is a pseudoconsole living in the helper
    // process, and a pseudoconsole hands over nothing its child printed until
    // the cursor-position query it asks is answered. The session server reads
    // that query out of the pane's output, its terminal engine builds the
    // report, and the report goes back over the link. A failure here puts the
    // fault on that path and clears the swap of it; a pass puts it on the swap.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
    );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    // One line, then a child that stays alive, so the last row the pane shows is
    // that line and nothing races it.
    let output_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_burst_spawn_spec("printed", 1)),
    );

    let painted_frame = attached_client_stream.receive_painted_frame_when(|painted_frame| {
        get_pane_rows(painted_frame, output_pane_id)
            .last()
            .is_some_and(|row| row == "printed-1")
    });
    assert_eq!(
        get_pane_rows(&painted_frame, output_pane_id)
            .last()
            .map(String::as_str),
        Some("printed-1")
    );
}

#[test]
fn a_pane_whose_child_never_exits_does_not_hold_the_swap_up() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    // A child that cannot exit and whose reader has nothing to read: ending
    // that reader and waiting for it would never return.
    let output_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    attached_client_stream.receive_events_until_restarting();
    drop(control_connection);

    // The wait is the assertion: a swap that wedged never advertises a new
    // socket, and this fails on the deadline instead of hanging.
    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };

    let mut expected_pane_lifecycles = vec![
        (seeded_pane_id, PaneLifecycle::Running),
        (output_pane_id, PaneLifecycle::Running),
    ];
    expected_pane_lifecycles.sort_by_key(|(pane_id, _)| *pane_id);
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        expected_pane_lifecycles
    );
}

#[test]
fn a_client_that_comes_back_keeps_its_id_its_focus_and_its_zoom() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (initial_client_stream, endpoint_before_restart) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
    );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let focused_pane_id = build_pane(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );

    // A focus and a zoom this client alone holds, both distinct from what a
    // freshly minted client would come up with.
    submit_session_command(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(focused_pane_id),
            client_id: Some(initial_client_stream.client_id),
        }),
    );
    submit_session_command(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        Command::TogglePaneFullscreen,
    );
    let zoomed_painted_frame = initial_client_stream.receive_painted_frame_when(|painted_frame| {
        painted_frame.client_snapshot.focused_pane_id == Some(focused_pane_id)
            && painted_frame
                .session_snapshot
                .active_tab_snapshot
                .layout_mode
                == LayoutMode::Fullscreen { focused_pane_id }
    });
    assert_eq!(
        zoomed_painted_frame.client_snapshot.client_id,
        initial_client_stream.client_id
    );

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    initial_client_stream.receive_events_until_restarting();
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    let (reconnected_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        Some(initial_client_stream.client_id),
    );

    // The record came across the swap, so the session handed it back rather
    // than minting a fresh client.
    assert_eq!(
        reconnected_client_stream.client_id,
        initial_client_stream.client_id
    );
    let painted_frame = reconnected_client_stream.receive_next_painted_frame();
    assert_eq!(
        painted_frame.client_snapshot.client_id,
        initial_client_stream.client_id
    );
    assert_eq!(
        painted_frame.client_snapshot.focused_pane_id,
        Some(focused_pane_id)
    );
    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .layout_mode,
        LayoutMode::Fullscreen { focused_pane_id }
    );

    // The state file has done its work and is gone, so nothing keeps the
    // session's screens on disk and the router stops reading the session as
    // one that is still replacing its image.
    assert_eq!(
        std::fs::metadata(resolve_resume_file_path(
            runtime_directory.path(),
            session_id
        ))
        .err()
        .map(|error| error.kind()),
        Some(std::io::ErrorKind::NotFound),
        "the state file is removed once the session came back from it"
    );
}

#[test]
fn a_second_caller_naming_a_client_already_streaming_is_given_a_client_of_its_own() {
    // Two `koshi attach` runs can come back for the same record: one that was
    // slow to notice the restart and one already back. Handing the record to
    // both would give two terminals one client, so the second caller gets a
    // client of its own and the first keeps its stream.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (original_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let focused_pane_id = build_pane(
        &mut control_connection,
        session_id,
        original_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );
    let client_id = original_client_stream.client_id;
    submit_session_command(
        &mut control_connection,
        session_id,
        client_id,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(focused_pane_id),
            client_id: Some(client_id),
        }),
    );
    original_client_stream.receive_painted_frame_when(|painted_frame| {
        painted_frame.client_snapshot.focused_pane_id == Some(focused_pane_id)
    });

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    original_client_stream.receive_events_until_restarting();
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };

    let (first_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        Some(client_id),
    );
    assert_eq!(
        first_client_stream.client_id, client_id,
        "the caller that came back first is handed the record"
    );

    let (second_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        Some(client_id),
    );

    assert_ne!(
        second_client_stream.client_id, client_id,
        "the second caller naming the same record is given one of its own"
    );
    assert_ne!(
        second_client_stream.client_id,
        first_client_stream.client_id
    );

    // Both are attached, and the record that crossed the swap kept the focus it
    // came back with while the minted one starts with none of it.
    let mut attached_client_ids: Vec<ClientId> =
        fetch_session_overview(runtime_directory.path(), session_id)
            .clients
            .into_iter()
            .map(|client| client.client_id)
            .collect();
    attached_client_ids.sort();
    let mut expected_attached_client_ids = vec![client_id, second_client_stream.client_id];
    expected_attached_client_ids.sort();
    assert_eq!(attached_client_ids, expected_attached_client_ids);

    let painted_frame = first_client_stream.receive_next_painted_frame();
    assert_eq!(painted_frame.client_snapshot.client_id, client_id);
    assert_eq!(
        painted_frame.client_snapshot.focused_pane_id,
        Some(focused_pane_id),
        "the first caller keeps the focus the record carried across the swap"
    );
    assert_eq!(
        second_client_stream
            .receive_next_painted_frame()
            .client_snapshot
            .client_id,
        second_client_stream.client_id
    );
}

#[test]
fn a_client_that_never_comes_back_is_detached_when_the_window_closes() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let client_id = attached_client_stream.client_id;

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    attached_client_stream.receive_events_until_restarting();
    // The session server closed its end when it wrote that frame, so dropping
    // this end leaves nothing of the connection behind. Nobody claims the
    // record from here.
    drop(attached_client_stream);
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };

    // The record crossed the swap, so the session holds it while it waits.
    assert_eq!(
        fetch_session_overview(runtime_directory.path(), session_id)
            .clients
            .into_iter()
            .map(|client| client.client_id)
            .collect::<Vec<_>>(),
        vec![client_id]
    );

    // When the window closes the record is detached, so nothing is left holding
    // a place for a client that never returned.
    let deadline = Instant::now() + RECONNECT_WAIT_DURATION;
    loop {
        let session_overview = fetch_session_overview(runtime_directory.path(), session_id);
        if session_overview.clients.is_empty() {
            assert_eq!(session_overview.session.attached_client_ids, Vec::new());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the client that never came back is still attached"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

#[test]
fn a_restart_into_a_binary_that_cannot_run_is_refused_and_the_session_keeps_serving() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let mut session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let output_pane_id = build_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );

    // A running program can be renamed on every supported platform, and on Unix
    // its mode can be changed under it; either one is what an update that
    // arrived broken leaves behind.
    #[cfg(unix)]
    let refusal = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o644))
            .expect("the binary loses its execute permission");
        format!("the binary at {} is not executable", binary_path.display())
    };
    #[cfg(windows)]
    let refusal = {
        let moved_binary_path = binary_path.with_extension("moved");
        std::fs::rename(&binary_path, &moved_binary_path).expect("the binary is moved aside");
        let missing_binary_error =
            std::fs::metadata(&binary_path).expect_err("nothing is at that path");
        format!(
            "the binary at {} could not be read: {missing_binary_error}",
            binary_path.display()
        )
    };

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: refusal,
        })
    );

    // Nothing was torn down for the refused restart: the session serves the
    // socket it bound, holds both panes, and its client is still streaming.
    assert!(!session_server_process.has_session_server_exited());
    assert_eq!(
        EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            session_id
        ))
        .expect("the session still advertises its socket")
        .connection_token
        .expose(),
        endpoint_before_restart.connection_token.expose()
    );
    let mut expected_pane_lifecycles = vec![
        (seeded_pane_id, PaneLifecycle::Running),
        (output_pane_id, PaneLifecycle::Running),
    ];
    expected_pane_lifecycles.sort_by_key(|(pane_id, _)| *pane_id);
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        expected_pane_lifecycles
    );
    assert_eq!(
        attached_client_stream
            .receive_next_painted_frame()
            .client_snapshot
            .client_id,
        attached_client_stream.client_id
    );
}

#[test]
fn a_swap_that_cannot_write_its_state_leaves_the_session_serving_with_live_readers() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let mut session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (initial_client_stream, endpoint_before_restart) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
    );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    // Two shells, so each pane can be asked for output of its own after the
    // swap fails.
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let opened_pane_id = build_pane(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        None,
    );

    // A directory where the carried state has to be written: every check the
    // restart makes passes, and the write that follows them cannot land.
    std::fs::create_dir(resolve_resume_file_path(
        runtime_directory.path(),
        session_id,
    ))
    .expect("a directory takes the resume file's place");

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    initial_client_stream.receive_events_until_restarting();
    drop(control_connection);

    // The session put itself back on its feet in the process it was already in,
    // on a socket carrying a fresh token — the same thing a client watches for
    // after a swap that did work.
    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    assert!(!session_server_process.has_session_server_exited());
    #[cfg(unix)]
    assert_eq!(
        restarted_endpoint.process_id,
        endpoint_before_restart.process_id
    );

    let (attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        Some(initial_client_stream.client_id),
    );
    assert_eq!(
        attached_client_stream.client_id,
        initial_client_stream.client_id
    );

    let mut expected_pane_lifecycles = vec![
        (seeded_pane_id, PaneLifecycle::Running),
        (opened_pane_id, PaneLifecycle::Running),
    ];
    expected_pane_lifecycles.sort_by_key(|(pane_id, _)| *pane_id);
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        expected_pane_lifecycles
    );

    // The readers came back: a line typed into each pane reaches its shell and
    // that shell's answer reaches the screen.
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    send_terminal_line(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        seeded_pane_id,
        "echo koshi-seeded-back",
    );
    send_terminal_line(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        opened_pane_id,
        "echo koshi-opened-back",
    );
    attached_client_stream.receive_painted_frame_when(|painted_frame| {
        get_pane_rows(painted_frame, seeded_pane_id).contains(&"koshi-seeded-back".to_string())
            && get_pane_rows(painted_frame, opened_pane_id)
                .contains(&"koshi-opened-back".to_string())
    });
}

#[test]
fn a_pane_child_that_exits_around_the_swap_is_reported_exactly_once() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let _session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let (initial_client_stream, endpoint_before_restart) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
    );
    let (mut control_connection, _) = open_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let input_pane_id = build_pane(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        Some(build_key_then_exit_spawn_spec()),
    );

    // The line the child is waiting for. Its exit closes the pane, so the swap
    // that follows carries a session the pane has just left. The line carries a
    // word rather than being a bare return, so nothing rests on how a line
    // reader treats an empty line.
    send_terminal_line(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        input_pane_id,
        "typed",
    );
    let mut exit_event_count = 0;
    let exit_event = initial_client_stream.receive_event_when(|session_event| {
        matches!(session_event, SessionEvent::PaneProcessExited { pane_id, .. } if *pane_id == input_pane_id)
    });
    let SessionEvent::PaneProcessExited { exit_code, .. } = exit_event else {
        unreachable!("the event matched above")
    };
    assert_eq!(exit_code, Some(0));
    exit_event_count += 1;
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        vec![(seeded_pane_id, PaneLifecycle::Running)]
    );

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    exit_event_count += count_pane_exit_events(
        &initial_client_stream.receive_events_until_restarting(),
        input_pane_id,
    );
    drop(control_connection);

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    let _restarted_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    let (attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        Some(initial_client_stream.client_id),
    );
    exit_event_count += count_pane_exit_events(
        &attached_client_stream.drain_session_events(SETTLE_DURATION),
        input_pane_id,
    );

    // One report for one exit: the swap neither swallowed it nor reaped the
    // child a second time and told the client twice.
    assert_eq!(exit_event_count, 1);
    assert_eq!(
        list_pane_lifecycles(runtime_directory.path(), session_id),
        vec![(seeded_pane_id, PaneLifecycle::Running)]
    );
}

/// Count events in `session_events` that report `pane_id`'s child exiting.
fn count_pane_exit_events(session_events: &[SessionEvent], pane_id: PaneId) -> usize {
    session_events
        .iter()
        .filter(|session_event| {
            matches!(
                session_event,
                SessionEvent::PaneProcessExited {
                    pane_id: reported_pane_id,
                    ..
                } if *reported_pane_id == pane_id
            )
        })
        .count()
}

#[test]
fn the_router_leaves_a_session_that_is_replacing_its_image_alone() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let _router = start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut router = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut router);
    let _session_server_process = RunningProcess {
        process_id: created_session.process_id,
    };

    // What the router sees while a session is replacing its own image: the
    // socket is unbound for that moment, and the resume file the session wrote
    // is sitting beside its endpoint file.
    std::fs::write(
        resolve_resume_file_path(runtime_directory.path(), created_session.session_id),
        b"{}",
    )
    .expect("the resume file is written");
    terminate_process(created_session.process_id);

    // The listing probes every session it holds, and this one does not answer,
    // so it is left out of the answer either way.
    assert_eq!(list_session_ids(&mut router), Vec::new());
    // What the guard changes: the session's advertisement stays on the disk,
    // for the session's own new image to write over.
    assert!(EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id
    )
    .exists());

    // With the resume file gone the same listing takes that advertisement off
    // the disk, which is exactly what the guard held back.
    std::fs::remove_file(resolve_resume_file_path(
        runtime_directory.path(),
        created_session.session_id,
    ))
    .expect("the resume file is removed");
    assert_eq!(list_session_ids(&mut router), Vec::new());
    assert!(!EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id
    )
    .exists());
}

#[test]
fn a_resume_run_that_cannot_bind_its_socket_leaves_no_resume_file_behind() {
    // A new image can fail after it has started. The file it was started from
    // holds every pane's screen and scrollback, and no later run ever reads it,
    // so a run that cannot come up must still take it off the disk.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    // The address a resume run for this session must bind, held by a session
    // server that is already serving it.
    let _session_server_holding_endpoint = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );

    let resume_file_path = resolve_resume_file_path(runtime_directory.path(), session_id);
    koshi_runtime::resume::write_resume_file(
        &resume_file_path,
        &koshi_runtime::resume::ResumeHeader {
            resume_format: koshi_runtime::resume::RESUME_FORMAT,
            session_id,
            session_name: SESSION_SERVER_NAME.to_string(),
            carried_panes: Vec::new(),
        },
        &koshi_runtime::resume::ResumeBody {
            session_by_id: std::collections::HashMap::new(),
            terminal_state_by_pane_id: std::collections::HashMap::new(),
            undecoded_bytes_by_pane_id: std::collections::HashMap::new(),
            graphics_undecoded_bytes_by_pane_id: std::collections::HashMap::new(),
            graphics_screen_continuation_by_pane_id: std::collections::HashMap::new(),
            graphics_screen_wrapper_active_by_pane_id: std::collections::HashMap::new(),
            graphics_tmux_continuation_by_pane_id: std::collections::HashMap::new(),
            graphics_tmux_wrapper_active_by_pane_id: std::collections::HashMap::new(),
            graphics_events_by_pane_id: std::collections::HashMap::new(),
            graphics_transport_by_pane_id: std::collections::HashMap::new(),
            synchronized_output_by_pane_id: std::collections::HashMap::new(),
            carried_quit: None,
        },
    )
    .expect("the resume file is written");
    assert!(
        resume_file_path.exists(),
        "the resume file is on the disk to start with"
    );

    let mut resuming_process = start_koshi_process(
        build_koshi_command(&binary_path, test_home_directory.path())
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_SERVER_NAME)
            .arg("--runtime-dir")
            .arg(runtime_directory.path())
            .arg("--resume")
            .arg(&resume_file_path)
            .stdout(Stdio::null()),
    );
    let exit_status = wait_for_process_exit(&mut resuming_process);

    assert!(
        !exit_status.success(),
        "a resume run that cannot bind its socket must fail"
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file must not be left on the disk"
    );
}

/// Wait for `started_process` to end and hand back how it ended. A process still running
/// when the wait runs out fails the test, and is ended so nothing is left
/// behind.
fn wait_for_process_exit(started_process: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(exit_status) = started_process
            .try_wait()
            .expect("the process's state can be read")
        {
            return exit_status;
        }
        if Instant::now() >= deadline {
            let _ = started_process.kill();
            let _ = started_process.wait();
            panic!("the process was still running after {WAIT_DURATION:?}");
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// Open a connection to the router serving `runtime_directory`, with its handshake
/// already done, retrying until one answers.
fn connect_to_router(runtime_directory: &Path) -> Connection {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(connection) = try_connect_to_router(runtime_directory) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no router answered in {}",
            runtime_directory.display()
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a router connection: read the endpoint file, connect,
/// and send the Hello that opens the connection.
fn try_connect_to_router(runtime_directory: &Path) -> Option<Connection> {
    let endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket_address).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(endpoint.connection_token),
    };
    connection.send(&hello).ok()?;
    let router_response: RouterResponse = connection.recv().ok()?;
    match router_response.answer_result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Ask the router for a new session and hand back where it listens.
fn build_session(connection: &mut Connection) -> SessionAddress {
    match send_router_request(
        connection,
        RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        },
    ) {
        RouterResult::Created(address) => address,
        unexpected_result => {
            panic!("creating a session was answered with {unexpected_result:?}")
        }
    }
}

/// The sessions the router lists, by id.
fn list_session_ids(connection: &mut Connection) -> Vec<SessionId> {
    match send_router_request(connection, RouterRequestKind::ListSessions) {
        RouterResult::Sessions(sessions) => sessions
            .into_iter()
            .map(|session_discovery| session_discovery.session_id)
            .collect(),
        unexpected_result => {
            panic!("listing the sessions was answered with {unexpected_result:?}")
        }
    }
}

/// Ask the router for `request_kind` on an open connection and hand back its answer.
fn send_router_request(
    connection: &mut Connection,
    request_kind: RouterRequestKind,
) -> RouterResult {
    let router_request = RouterRequest {
        request_id: 2,
        request_kind,
    };
    connection
        .send(&router_request)
        .expect("the router reads the request");
    let router_response: RouterResponse =
        connection.recv().expect("the router answers the request");
    assert_eq!(router_response.request_id, Some(2));
    router_response.answer_result
}
