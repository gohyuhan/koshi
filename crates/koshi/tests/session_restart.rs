//! Cross-process tests for a session server that replaces its own process
//! image: a real `koshi serve-session` runs with real panes and real child
//! processes, a real client is attached over its control socket, and the test
//! asks it to restart into the binary it was started from.
//!
//! The tests check the readers held still, a pane's terminal crossing the swap,
//! the swap itself, and the session that comes back afterwards.
//!
//! Every test serves its own temporary runtime directory and its own home
//! directory, both under a short base.
//!
//! Every event stream is read on a thread of its own. A session that sends no
//! frame within [`WAIT_DURATION`] fails the test.
//!
//! Where the evidence is platform-specific — the process id that `execvp`
//! keeps on Unix, the handover to a new process on Windows — the test branches
//! inside the assertion.
//!
//! Every process a test starts is held in a guard that ends it when the test
//! drops it.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use koshi_core::command::{
    CliExitCode, Command, CommandEnvelope, CommandSource, FocusPaneArgs, FocusTarget,
    WriteToPaneArgs,
};
use koshi_core::discovery::{PaneLifecycle, SessionOverview};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId};
use koshi_core::key::{Key, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags};
use koshi_core::process::SpawnSpec;
use koshi_ipc::endpoint::{resolve_resume_file_path, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse, IpcResult,
    WireMouseAction,
};
use koshi_ipc::router::{RouterResult, SessionSelector};
use koshi_ipc::transport::{Connection, FrameWriter};
use koshi_layout::mode::LayoutMode;

mod common;

#[cfg(unix)]
use common::resolve_config_directory_under_home;
use common::session_connection::{
    create_pane, send_session_request, submit_session_command, wait_for_session_connection,
};
use common::{
    build_koshi_command_at, build_no_such_session_result, build_shell_spawn_spec,
    build_short_test_directory, connect_to_router, copy_koshi_binary, create_session,
    send_attach_lookup, start_router_process, terminate_process, wait_for_session_lookup_refusal,
    RunningProcess, SessionProcess, SESSION_SERVER_NAME, WAIT_DURATION,
};
use koshi_test_support::fixtures::start_program_process;

/// How long a poll in this suite pauses between attempts: 50 milliseconds.
const RESTART_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How long a test reads on for frames that must not arrive: 750
/// milliseconds.
const SETTLE_DURATION: Duration = Duration::from_millis(750);

/// How long a test waits after reading the restart frame before it sends input:
/// 250 milliseconds, inside the one second the session waits for its clients
/// to leave.
const TYPED_AFTER_RESTART_FRAME_DURATION: Duration = Duration::from_millis(250);

/// How long a test waits for the session server to detach a client record
/// nobody claimed: 75 seconds. The session server holds such a record for 30
/// seconds.
const RECONNECT_WAIT_DURATION: Duration = Duration::from_secs(75);

/// The bytes of a resume file cut off inside its header: no build reads a
/// header out of them.
const UNREADABLE_RESUME_FILE_BYTES: &[u8] = b"{\"header\":{\"resume_format\":1}";

/// The descriptor number a session server started by a test inherits a
/// pseudoterminal master under.
#[cfg(unix)]
const INHERITED_TERMINAL_FILE_DESCRIPTOR: i32 = 20;

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

/// A session server the test started with its standard output piped, and the
/// reader its ready line came from. The reader holds the pipe open for as long
/// as this lives; on Unix the image that replaces the process inherits that
/// pipe and writes its own ready line into it. Dropping it ends the process.
///
/// On Windows a restart hands over to a new process and the started one ends.
/// On Unix the started one keeps running, under the same process id, as the
/// new image.
struct ReadySessionProcess {
    session_process: SessionProcess,
    _ready_output_reader: BufReader<ChildStdout>,
}

/// The command that starts the binary at `binary_path` as `session_id`'s server,
/// serving `runtime_directory` under `test_home_directory` as
/// [`build_koshi_command_at`] sets it, under the identity the router would have
/// handed it: `serve-session <session_id> workspace --runtime-dir
/// <runtime_directory>`.
///
/// The session server writes its error stream to the test run's own. Every
/// seeded pane launches the shell [`build_koshi_command_at`] names: `/bin/sh`
/// on Unix, and `COMSPEC` on Windows.
fn build_session_server_command(
    binary_path: &Path,
    test_home_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> std::process::Command {
    let mut process_command = build_koshi_command_at(binary_path, test_home_directory);
    process_command.stderr(Stdio::inherit());
    process_command
        .arg("serve-session")
        .arg(session_id.to_string())
        .arg(SESSION_SERVER_NAME)
        .arg("--runtime-dir")
        .arg(runtime_directory);
    process_command
}

/// Start the binary at `binary_path` as one session's server, under the identity the
/// router would have handed it, and wait for the ready line it prints once its
/// control socket is bound.
fn start_session_server(
    binary_path: &Path,
    test_home_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> ReadySessionProcess {
    start_session_server_with_command(&mut build_session_server_command(
        binary_path,
        test_home_directory,
        runtime_directory,
        session_id,
    ))
}

/// Start `process_command`, a session server's whole command line, with its
/// standard output piped, and wait for the ready line it prints once its
/// control socket is bound.
fn start_session_server_with_command(
    process_command: &mut std::process::Command,
) -> ReadySessionProcess {
    let mut child_process = start_program_process(process_command.stdout(Stdio::piped()));
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
    ReadySessionProcess {
        session_process: SessionProcess { child_process },
        _ready_output_reader: ready_output_reader,
    }
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
    let mut pane_input_bytes = terminal_line.as_bytes().to_vec();
    pane_input_bytes.push(b'\r');
    submit_session_command(
        connection,
        session_id,
        client_id,
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(pane_id),
            pane_input_bytes,
        }),
    );
}

/// The session's own description of itself, read over its control socket.
fn fetch_session_overview(runtime_directory: &Path, session_id: SessionId) -> SessionOverview {
    koshi_link::discovery::fetch_session_overview(runtime_directory, None, session_id, None)
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
/// A session server mints a fresh connection token every time it binds: a
/// token other than `endpoint_before_restart`'s belongs to the socket the
/// session came back on. An attached client watches for the same change.
fn wait_for_restarted_session_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
    endpoint_before_restart: &EndpointFile,
) -> EndpointFile {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(loaded_endpoint) = EndpointFile::load_from_path(
            &EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id),
        ) {
            if loaded_endpoint.connection_token.expose_secret()
                != endpoint_before_restart.connection_token.expose_secret()
            {
                return loaded_endpoint;
            }
        }
        assert!(
            Instant::now() < wait_deadline,
            "the session advertised no new socket after its restart"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// The process ids whose parent is `parent_process_id`, in ascending order,
/// as the operating system reports them.
///
/// Unix only: there a pane's child is the session server's own child, and
/// `execvp` keeps it. On Windows a pane's child belongs to the process that
/// holds the panes.
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

/// One attached client's event stream, read on a thread of its own and taken
/// with a deadline, plus the writing half that carries this client's own
/// requests up.
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
    /// Join `session_id` as a viewing client at `viewport_size`, the way the
    /// attached client joins it: Hello, then Attach on the same connection.
    ///
    /// `resume_client_id` names the client record to come back as after the
    /// session replaced its own image, and is `None` on a first attach.
    fn attach_test_client(
        runtime_directory: &Path,
        session_id: SessionId,
        viewport_size: Size,
        resume_client_id: Option<ClientId>,
    ) -> (AttachedClientStream, EndpointFile) {
        let (mut connection, endpoint_file) =
            wait_for_session_connection(runtime_directory, session_id);
        let attach_request = IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::Attach {
                viewport_size,
                resume_client_id,
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
            session_id: joined_session_id,
            ..
        } = ipc_response.answer_result
        else {
            panic!(
                "expected an attach reply, got {:?}",
                ipc_response.answer_result
            );
        };
        assert_eq!(joined_session_id, session_id);

        let (mut session_event_reader, request_writer) = connection.split();
        let (session_events_sender, session_events) = mpsc::channel();
        std::thread::spawn(move || loop {
            let session_event_result = session_event_reader.recv::<SessionEvent>();
            let is_stream_ended = session_event_result.is_err();
            if session_events_sender.send(session_event_result).is_err() || is_stream_ended {
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
        let wait_deadline = Instant::now() + WAIT_DURATION;
        let mut received_event_names: Vec<&'static str> = Vec::new();
        loop {
            let remaining_duration = wait_deadline.saturating_duration_since(Instant::now());
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
        let wait_deadline = Instant::now() + WAIT_DURATION;
        let mut last_painted_frame: Option<PaintedFrame> = None;
        loop {
            let remaining_duration = wait_deadline.saturating_duration_since(Instant::now());
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
        let settle_deadline = Instant::now() + time_window;
        let mut received_events = Vec::new();
        while let Some(remaining_duration) = settle_deadline.checked_duration_since(Instant::now())
        {
            match self.session_events.recv_timeout(remaining_duration) {
                Ok(Ok(session_event)) => received_events.push(session_event),
                Ok(Err(_)) | Err(_) => return received_events,
            }
        }
        received_events
    }

    /// Send one request up this client's own connection. The streaming half
    /// writes no response; the answer is whatever reaches the event stream.
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
    /// The session answers exactly one round per request, and the first frame
    /// after that answer is the scrolled one.
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
/// A blank row between two rows of text is kept.
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
        .scrollback_metadata
        .retained_line_count
}

/// A child that prints `line_count` lines reading `<line_prefix>-1` to
/// `<line_prefix>-<line_count>` as fast as it can, and then stays alive and
/// prints nothing more.
fn build_burst_spawn_spec(line_prefix: &str, line_count: u32) -> SpawnSpec {
    #[cfg(unix)]
    let shell_script = format!(
        "line_number=1; while [ $line_number -le {line_count} ]; do printf '{line_prefix}-%d\\n' $line_number; line_number=$((line_number+1)); done; \
         sleep 300"
    );
    // The parentheses end the loop body at the closing bracket, and the
    // `ping` after `&` runs once, after the last pass.
    #[cfg(windows)]
    let shell_script = format!(
        "(for /L %i in (1,1,{line_count}) do @echo {line_prefix}-%i) & ping -n 301 127.0.0.1 >nul"
    );
    build_shell_spawn_spec(&shell_script)
}

/// A child that prints `line_count` lines reading `<line_prefix>-1` to
/// `<line_prefix>-<line_count>`, 0.25 seconds apart on Unix and about one
/// second apart on Windows, and then stays alive.
fn build_paced_spawn_spec(line_prefix: &str, line_count: u32) -> SpawnSpec {
    #[cfg(unix)]
    let shell_script = format!(
        "line_number=1; while [ $line_number -le {line_count} ]; do printf '{line_prefix}-%d\\n' $line_number; line_number=$((line_number+1)); \
         sleep 0.25; done; sleep 300"
    );
    #[cfg(windows)]
    let shell_script = format!(
        "(for /L %i in (1,1,{line_count}) do @(echo {line_prefix}-%i & ping -n 2 127.0.0.1 >nul)) & \
         ping -n 301 127.0.0.1 >nul"
    );
    build_shell_spawn_spec(&shell_script)
}

/// A child that prints nothing and never exits: the case whose reader has
/// nothing to read and whose process cannot be waited on.
fn build_idle_spawn_spec() -> SpawnSpec {
    #[cfg(unix)]
    let shell_script = "sleep 300".to_string();
    #[cfg(windows)]
    let shell_script = "ping -n 301 127.0.0.1 >nul".to_string();
    build_shell_spawn_spec(&shell_script)
}

/// A child that reads one line and then exits reporting success.
fn build_line_then_exit_spawn_spec() -> SpawnSpec {
    #[cfg(unix)]
    let shell_script = "read line; exit 0".to_string();
    // `set /p` reads a line, as `read` does.
    #[cfg(windows)]
    let shell_script = "set /p x= & exit 0".to_string();
    build_shell_spawn_spec(&shell_script)
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

    // A short terminal: the thirty lines each pane prints do not all fit, and
    // the ones above the top land in scrollback.
    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            SHORT_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let left_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_burst_spawn_spec("left", 30)),
    );
    let right_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_burst_spawn_spec("right", 30)),
    );

    let initial_painted_frame =
        attached_client_stream.receive_painted_frame_when(|painted_frame| {
            get_pane_rows(painted_frame, left_pane_id)
                .last()
                .is_some_and(|row_text| row_text == "left-30")
                && get_pane_rows(painted_frame, right_pane_id)
                    .last()
                    .is_some_and(|row_text| row_text == "right-30")
        });
    // The last line each child printed ends with a newline, and that newline
    // moves the view one row on. The frame above can be the one painted between
    // the line and its newline. This takes the last frame painted once the
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

    #[cfg(unix)]
    let app_config_path = {
        let config_directory = resolve_config_directory_under_home(test_home_directory.path());
        std::fs::create_dir_all(&config_directory).expect("create config directory");
        let app_config_path = config_directory.join("koshi.kdl");
        std::fs::write(&app_config_path, "version 1\n").expect("write released config");
        app_config_path
    };

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
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_to_string(&app_config_path).expect("read migrated config"),
        "version 2\n"
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
        // The swap replaced this process's running image: the session serves
        // under the process id it started with, and every pane's child kept the
        // same parent and the same process id.
        assert!(!session_server_process
            .session_process
            .has_session_server_exited());
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
        let wait_deadline = Instant::now() + WAIT_DURATION;
        while !session_server_process
            .session_process
            .has_session_server_exited()
        {
            assert!(
                Instant::now() < wait_deadline,
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

    // A tall terminal: every line the child prints stays on screen.
    let (attached_client_stream, endpoint_before_restart) =
        AttachedClientStream::attach_test_client(
            runtime_directory.path(),
            session_id,
            TALL_ATTACH_VIEWPORT_SIZE,
            None,
        );
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let output_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_paced_spawn_spec("mark", 12)),
    );

    // The restart goes out while the child is still printing: the run crosses
    // the parking, the swap, and the reader coming back.
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    // A child that ends the moment it reads one line: the panes the session
    // holds after the swap show whether the line reached it.
    let input_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_line_then_exit_spawn_spec()),
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
    // A client learns the session is going only when it reads this frame.
    // What follows is what a user types into the window the swap leaves open.
    attached_client_stream.receive_events_until_restarting();
    // The sleep ends after the swap stops reading its clients, and inside the
    // wait it gives them.
    std::thread::sleep(TYPED_AFTER_RESTART_FRAME_DURATION);
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(attached_client_stream.client_id),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(input_pane_id),
            pane_input_bytes: b"typed\r".to_vec(),
        }),
    );
    attached_client_stream.send_request_while_connection_open(
        20,
        IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    );
    // What a real client sends the moment it reads that frame. Requests arrive
    // in the order they were queued: the session reads the line above before
    // it reads this, and the swap waits for this.
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
    let wait_deadline = Instant::now() + WAIT_DURATION;
    while list_pane_lifecycles(runtime_directory.path(), session_id)
        != vec![(seeded_pane_id, PaneLifecycle::Running)]
    {
        assert!(
            Instant::now() < wait_deadline,
            "the pane the line was sent to is still open, so its child never read it"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

#[test]
fn a_client_that_never_leaves_does_not_hold_the_swap_up() {
    // The swap waits a bounded time for every told client to say it is
    // leaving. A client whose window froze never says it: the session cuts the
    // connections still open and restarts anyway.
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
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

    // The session came back with the pane it was carrying.
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    // One line, then a child that stays alive: the last row the pane shows is
    // that line.
    let output_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_burst_spawn_spec("printed", 1)),
    );

    let painted_frame = attached_client_stream.receive_painted_frame_when(|painted_frame| {
        get_pane_rows(painted_frame, output_pane_id)
            .last()
            .is_some_and(|row_text| row_text == "printed-1")
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    // A child that never exits and prints nothing for its reader to read.
    let output_pane_id = create_pane(
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

    // The wait is the assertion: a swap that never advertises a new socket
    // fails this on the deadline.
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let focused_pane_id = create_pane(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );

    // A focus and a zoom this client alone holds, both different from what a
    // new client starts with.
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

    // The record came across the swap: the session handed back the same
    // client.
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

    // The state file is gone: no copy of the session's screens stays on disk,
    // and the router no longer reads the session as one that is replacing its
    // image.
    assert_eq!(
        std::fs::metadata(resolve_resume_file_path(
            runtime_directory.path(),
            session_id
        ))
        .err()
        .map(|io_error| io_error.kind()),
        Some(std::io::ErrorKind::NotFound),
        "the state file is removed once the session came back from it"
    );
}

#[test]
fn a_second_caller_naming_a_client_already_streaming_is_given_a_client_of_its_own() {
    // Two `koshi attach` runs come back for the same record: one that was slow
    // to notice the restart and one already back. The second caller gets a
    // client of its own, and the first keeps its stream.
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let focused_pane_id = create_pane(
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
            .map(|client_discovery| client_discovery.client_id)
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let client_id = attached_client_stream.client_id;

    assert_eq!(
        send_session_request(&mut control_connection, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    attached_client_stream.receive_events_until_restarting();
    // The session server closed its end when it wrote that frame; dropping
    // this end ends the connection. Nobody claims the record from here.
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

    // The record crossed the swap, and the session holds it while it waits.
    assert_eq!(
        fetch_session_overview(runtime_directory.path(), session_id)
            .clients
            .into_iter()
            .map(|client_discovery| client_discovery.client_id)
            .collect::<Vec<_>>(),
        vec![client_id]
    );

    // When the window closes, the record is detached.
    let reconnect_deadline = Instant::now() + RECONNECT_WAIT_DURATION;
    loop {
        let session_overview = fetch_session_overview(runtime_directory.path(), session_id);
        if session_overview.clients.is_empty() {
            assert_eq!(session_overview.session.attached_client_ids, Vec::new());
            break;
        }
        assert!(
            Instant::now() < reconnect_deadline,
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let output_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );

    // The program file is made unusable while the session runs: on Unix it
    // loses its execute permission, and on Windows it is renamed aside.
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
            code: IpcErrorCode::RequestFailed,
            message: refusal,
        })
    );

    // Nothing was torn down for the refused restart: the session serves the
    // socket it bound, holds both panes, and its client is still streaming.
    assert!(!session_server_process
        .session_process
        .has_session_server_exited());
    assert_eq!(
        EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            session_id
        ))
        .expect("the session still advertises its socket")
        .connection_token
        .expose_secret(),
        endpoint_before_restart.connection_token.expose_secret()
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

/// Replace the program at `program_path` with a shell script that answers
/// `--version` with `koshi 9.9.9` and runs the `koshi` at `koshi_binary_path`
/// for every other command line. The script is written beside the program and
/// renamed over it.
#[cfg(unix)]
fn replace_with_other_version_koshi(program_path: &Path, koshi_binary_path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let replacement_path = program_path.with_extension("next");
    std::fs::write(
        &replacement_path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'koshi 9.9.9'; exit 0; fi\nexec '{}' \"$@\"\n",
            koshi_binary_path.display()
        ),
    )
    .expect("the replacement program is written");
    std::fs::set_permissions(&replacement_path, std::fs::Permissions::from_mode(0o755))
        .expect("the replacement program runs");
    std::fs::rename(&replacement_path, program_path).expect("the program file is replaced");
}

#[cfg(unix)]
#[test]
fn a_session_whose_program_file_holds_another_version_restarts_on_the_next_connection() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let koshi_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let koshi_binary_path = copy_koshi_binary(koshi_directory.path());
    let session_id = SessionId::new();
    let mut session_server_process = start_session_server(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );
    let (_first_connection, endpoint_before_restart) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let children_before_restart = list_child_process_ids(endpoint_before_restart.process_id);

    replace_with_other_version_koshi(&binary_path, &koshi_binary_path);
    let _second_connection = Connection::connect(&endpoint_before_restart.socket_address)
        .expect("the session takes the connection");

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    assert!(!session_server_process
        .session_process
        .has_session_server_exited());
    assert_eq!(
        restarted_endpoint.process_id,
        endpoint_before_restart.process_id
    );
    assert_eq!(
        list_child_process_ids(restarted_endpoint.process_id),
        children_before_restart
    );
}

#[cfg(unix)]
#[test]
fn a_session_started_through_a_link_restarts_once_the_link_names_another_version() {
    // `bin/koshi` links to `versions/0.5.0/koshi`, a copy of the koshi under
    // test, and the session starts as `bin/koshi`. The link is then pointed at
    // `versions/9.9.9/koshi`, which answers `--version` with `koshi 9.9.9`.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let install_directory = build_short_test_directory();
    let koshi_directory = build_short_test_directory();
    let first_version_directory = install_directory.path().join("versions/0.5.0");
    std::fs::create_dir_all(&first_version_directory).expect("the version folder is made");
    let first_version_path = copy_koshi_binary(&first_version_directory);
    let koshi_binary_path = copy_koshi_binary(koshi_directory.path());
    let link_path = install_directory.path().join("bin/koshi");
    std::fs::create_dir_all(install_directory.path().join("bin")).expect("the link folder is made");
    std::os::unix::fs::symlink(&first_version_path, &link_path).expect("the link is made");
    let session_id = SessionId::new();
    let mut session_server_process = start_session_server(
        &link_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );
    let (_first_connection, endpoint_before_restart) =
        wait_for_session_connection(runtime_directory.path(), session_id);

    let other_version_path = install_directory.path().join("versions/9.9.9/koshi");
    std::fs::create_dir_all(install_directory.path().join("versions/9.9.9"))
        .expect("the version folder is made");
    replace_with_other_version_koshi(&other_version_path, &koshi_binary_path);
    let next_link_path = install_directory.path().join("bin/koshi.next");
    std::os::unix::fs::symlink(&other_version_path, &next_link_path).expect("the link is made");
    std::fs::rename(&next_link_path, &link_path).expect("the link is pointed at 9.9.9");
    let _second_connection = Connection::connect(&endpoint_before_restart.socket_address)
        .expect("the session takes the connection");

    let restarted_endpoint = wait_for_restarted_session_endpoint(
        runtime_directory.path(),
        session_id,
        &endpoint_before_restart,
    );
    assert!(!session_server_process
        .session_process
        .has_session_server_exited());
    assert_eq!(
        restarted_endpoint.process_id,
        endpoint_before_restart.process_id
    );
}

#[cfg(unix)]
#[test]
fn a_restart_whose_config_migration_fails_still_swaps_and_keeps_every_pane() {
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let output_pane_id = create_pane(
        &mut control_connection,
        session_id,
        attached_client_stream.client_id,
        Some(build_idle_spawn_spec()),
    );

    let config_directory = resolve_config_directory_under_home(test_home_directory.path());
    std::fs::create_dir_all(&config_directory).expect("create config directory");
    let app_config_path = config_directory.join("koshi.kdl");
    std::fs::write(&app_config_path, "version 1\n").expect("write released config");
    std::fs::create_dir(config_directory.join(".migration.lock"))
        .expect("block the migration lock");

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
    assert_eq!(
        std::fs::read_to_string(&app_config_path).expect("read unchanged config"),
        "version 1\n"
    );
    let (attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    // Two shells: each pane is asked for output of its own after the swap
    // fails.
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let opened_pane_id = create_pane(
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
    assert!(!session_server_process
        .session_process
        .has_session_server_exited());
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
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
    let (mut control_connection, _) =
        wait_for_session_connection(runtime_directory.path(), session_id);
    let seeded_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let input_pane_id = create_pane(
        &mut control_connection,
        session_id,
        initial_client_stream.client_id,
        Some(build_line_then_exit_spawn_spec()),
    );

    // The line the child is waiting for. Its exit closes the pane, and the
    // swap that follows carries a session the pane has just left. The line
    // carries a word, not a bare return.
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
    let _router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut router_connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut router_connection);
    let created_session_selector = SessionSelector::SessionId(created_session.session_id);
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

    // Once the killed session server is gone, nothing listens at its address.
    // The resume file marks a swap in flight: the lookup answers that the
    // session is restarting, and the session stays listed.
    assert_eq!(
        wait_for_session_lookup_refusal(&mut router_connection, &created_session_selector),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message: format!(
                "session {} is running but did not answer: it is restarting",
                created_session.session_id
            ),
        })
    );
    // What the guard changes: the session's advertisement stays on the disk,
    // for the session's own new image to write over.
    assert!(EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id
    )
    .exists());

    // With the resume file gone the same lookup takes that advertisement off
    // the disk, which is exactly what the guard held back.
    std::fs::remove_file(resolve_resume_file_path(
        runtime_directory.path(),
        created_session.session_id,
    ))
    .expect("the resume file is removed");
    assert_eq!(
        send_attach_lookup(&mut router_connection, &created_session_selector),
        build_no_such_session_result(created_session.session_id)
    );
    assert!(!EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id
    )
    .exists());
}

#[test]
fn a_resume_run_that_cannot_bind_its_socket_leaves_no_resume_file_behind() {
    // A new image can fail after it has started. The file it was started from
    // holds every pane's screen and scrollback, and a run that cannot come up
    // still removes it from the disk.
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
            carried_pane_state_by_pane_id: std::collections::HashMap::new(),
            carried_quit: None,
        },
    )
    .expect("the resume file is written");
    assert!(
        resume_file_path.exists(),
        "the resume file is on the disk to start with"
    );

    let mut resuming_process = start_program_process(
        build_session_server_command(
            &binary_path,
            test_home_directory.path(),
            runtime_directory.path(),
            session_id,
        )
        .arg("--resume")
        .arg(&resume_file_path)
        .stdout(Stdio::null()),
    );
    let exit_status = wait_for_process_exit(&mut resuming_process);

    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code()),
        "a resume run that cannot bind its socket must fail"
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file must not be left on the disk"
    );
}

/// Wait for `started_process` to end and hand back how it ended. A process
/// still running when the wait runs out is ended, and the test fails.
fn wait_for_process_exit(started_process: &mut Child) -> std::process::ExitStatus {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(exit_status) = started_process
            .try_wait()
            .expect("the process's state can be read")
        {
            return exit_status;
        }
        if Instant::now() >= wait_deadline {
            let _ = started_process.kill();
            let _ = started_process.wait();
            panic!("the process was still running after {WAIT_DURATION:?}");
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// Open a pipe whose two ends close on exec, and hand back `[read end, write
/// end]`. A test's session server writes a child's process id into the write
/// end before its own exec.
#[cfg(unix)]
fn open_process_id_pipe() -> [libc::c_int; 2] {
    let mut process_id_pipe_file_descriptors: [libc::c_int; 2] = [0; 2];
    // SAFETY: `pipe` writes two descriptors into the two-element array the
    // pointer names.
    let pipe_answer = unsafe { libc::pipe(process_id_pipe_file_descriptors.as_mut_ptr()) };
    assert_eq!(pipe_answer, 0);
    for pipe_file_descriptor in process_id_pipe_file_descriptors {
        // SAFETY: `fcntl` with `F_SETFD` takes a descriptor and a flag, and
        // reads no memory of this process.
        let fcntl_answer =
            unsafe { libc::fcntl(pipe_file_descriptor, libc::F_SETFD, libc::FD_CLOEXEC) };
        assert_eq!(fcntl_answer, 0);
    }
    process_id_pipe_file_descriptors
}

/// Read the one process id written into the pipe whose read end is
/// `process_id_read_file_descriptor`.
///
/// # Panics
/// Panics when the pipe holds fewer bytes than one process id.
#[cfg(unix)]
fn read_process_id_from_pipe(process_id_read_file_descriptor: libc::c_int) -> libc::pid_t {
    let mut process_id: libc::pid_t = 0;
    // SAFETY: `read` writes at most `size_of::<pid_t>()` bytes into
    // `process_id`, which holds exactly that many.
    let read_byte_count = unsafe {
        libc::read(
            process_id_read_file_descriptor,
            (&mut process_id as *mut libc::pid_t).cast(),
            std::mem::size_of::<libc::pid_t>(),
        )
    };
    assert_eq!(
        read_byte_count,
        std::mem::size_of::<libc::pid_t>() as isize,
        "the child's process id arrives"
    );
    process_id
}

/// Wait until `process_id` is reaped: `kill(process_id, 0)` answers `ESRCH`,
/// where a zombie still answers `0`.
///
/// # Panics
/// Panics when [`WAIT_DURATION`] runs out first, and when the check fails with
/// an error other than `ESRCH`.
#[cfg(unix)]
fn wait_until_process_is_reaped(process_id: libc::pid_t) {
    let reap_deadline = Instant::now() + WAIT_DURATION;
    loop {
        // SAFETY: `kill` with signal `0` sends no signal and reads no memory
        // of this process.
        let kill_answer = unsafe { libc::kill(process_id, 0) };
        if kill_answer != 0 {
            break;
        }
        assert!(
            Instant::now() < reap_deadline,
            "process {process_id} was never reaped"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "process {process_id} is gone, not merely unreachable"
    );
}

#[test]
fn a_resume_file_whose_header_does_not_read_comes_back_as_one_fresh_shell_showing_the_notice() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory.path(), session_id);
    std::fs::write(&resume_file_path, UNREADABLE_RESUME_FILE_BYTES)
        .expect("the cut-off resume file is placed");

    let _session_server_process = start_session_server_with_command(
        build_session_server_command(
            &binary_path,
            test_home_directory.path(),
            runtime_directory.path(),
            session_id,
        )
        .arg("--resume")
        .arg(&resume_file_path),
    );

    let fresh_pane_id = get_seeded_pane_id(runtime_directory.path(), session_id);
    let (mut attached_client_stream, _) = AttachedClientStream::attach_test_client(
        runtime_directory.path(),
        session_id,
        TALL_ATTACH_VIEWPORT_SIZE,
        None,
    );
    let painted_frame = attached_client_stream.receive_painted_frame_when(|painted_frame| {
        painted_frame.is_recovery_notice_visible
            && painted_frame
                .pane_snapshots
                .iter()
                .any(|pane_snapshot| pane_snapshot.pane_id == fresh_pane_id)
    });
    assert!(
        painted_frame.is_recovery_notice_visible,
        "{}",
        describe_painted_frame(&painted_frame)
    );
    attached_client_stream.send_session_request(
        3,
        IpcRequestKind::Keyboard {
            key_input: KeyInput {
                key: KeyIdentity::Key(Key::Char('x')),
                key_event_kind: KeyEventKind::Press,
                shifted_key: None,
                base_layout_key: None,
                associated_text: String::new(),
                modifier_flags: KeyModifierFlags::NONE,
            },
        },
    );
    let cleared_frame = attached_client_stream
        .receive_painted_frame_when(|painted_frame| !painted_frame.is_recovery_notice_visible);
    assert!(
        cleared_frame
            .pane_snapshots
            .iter()
            .any(|pane_snapshot| pane_snapshot.pane_id == fresh_pane_id),
        "{}",
        describe_painted_frame(&cleared_frame)
    );
    assert!(
        !resume_file_path.exists(),
        "the resume file is taken off the disk"
    );
}

#[cfg(unix)]
#[test]
fn a_resume_file_whose_header_does_not_read_lets_the_inherited_terminal_and_ended_child_go() {
    use std::os::unix::process::CommandExt;

    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory.path(), session_id);
    std::fs::write(&resume_file_path, UNREADABLE_RESUME_FILE_BYTES)
        .expect("the cut-off resume file is placed");
    // SAFETY: `open` reads the NUL-terminated path the C string literal
    // holds.
    let terminal_master_file_descriptor = unsafe {
        libc::open(
            c"/dev/ptmx".as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    assert!(
        terminal_master_file_descriptor >= 0,
        "the pseudoterminal master opens"
    );
    // SAFETY: `grantpt` and `unlockpt` take a descriptor and read no memory
    // of this process.
    let grantpt_answer = unsafe { libc::grantpt(terminal_master_file_descriptor) };
    assert_eq!(grantpt_answer, 0);
    // SAFETY: as above.
    let unlockpt_answer = unsafe { libc::unlockpt(terminal_master_file_descriptor) };
    assert_eq!(unlockpt_answer, 0);
    let terminal_path = std::ffi::CString::new(
        koshi_pty::portable::find_terminal_master_name(terminal_master_file_descriptor)
            .expect("the master names its terminal"),
    )
    .expect("the terminal path holds no NUL byte");
    // SAFETY: `terminal_path` is a `CString` that outlives the call, so the
    // pointer names a NUL-terminated path.
    let terminal_follower_file_descriptor = unsafe {
        libc::open(
            terminal_path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    assert!(
        terminal_follower_file_descriptor >= 0,
        "the other end of the pseudoterminal opens"
    );
    let [process_id_read_file_descriptor, process_id_write_file_descriptor] =
        open_process_id_pipe();
    let mut process_command = build_session_server_command(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );
    process_command.arg("--resume").arg(&resume_file_path);
    // The session server starts as the parent of a child that ends half a
    // second later, and holds the master under
    // `INHERITED_TERMINAL_FILE_DESCRIPTOR`. The child's process id goes down
    // the pipe.
    // SAFETY: the closure runs in the forked process before exec. It calls
    // only `fork`, `close`, `usleep`, `_exit`, `write`, and `dup2`, allocates
    // nothing, and writes only from the stack value `ended_child_process_id`.
    unsafe {
        process_command.pre_exec(move || {
            let ended_child_process_id = libc::fork();
            if ended_child_process_id == 0 {
                libc::close(terminal_master_file_descriptor);
                libc::usleep(500_000);
                libc::_exit(0);
            }
            if ended_child_process_id < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::write(
                process_id_write_file_descriptor,
                (&ended_child_process_id as *const libc::pid_t).cast(),
                std::mem::size_of::<libc::pid_t>(),
            );
            if libc::dup2(
                terminal_master_file_descriptor,
                INHERITED_TERMINAL_FILE_DESCRIPTOR,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let _session_server_process = start_session_server_with_command(&mut process_command);
    // SAFETY: `close` takes descriptors this test opened and uses no more.
    unsafe {
        libc::close(terminal_master_file_descriptor);
        libc::close(process_id_write_file_descriptor);
    }
    let ended_child_process_id = read_process_id_from_pipe(process_id_read_file_descriptor);

    let release_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let mut read_byte = [0u8; 1];
        // SAFETY: `read` writes at most 1 byte into the 1-byte `read_byte`.
        let read_byte_count = unsafe {
            libc::read(
                terminal_follower_file_descriptor,
                read_byte.as_mut_ptr().cast(),
                1,
            )
        };
        let read_error_number = std::io::Error::last_os_error().raw_os_error();
        let is_master_open = read_byte_count > 0
            || (read_byte_count < 0
                && matches!(read_error_number, Some(libc::EAGAIN) | Some(libc::EINTR)));
        if !is_master_open {
            break;
        }
        assert!(
            Instant::now() < release_deadline,
            "the inherited terminal is still open"
        );
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
    wait_until_process_is_reaped(ended_child_process_id);
    // SAFETY: `close` takes descriptors this test opened and uses no more.
    unsafe {
        libc::close(terminal_follower_file_descriptor);
        libc::close(process_id_read_file_descriptor);
    }
}

#[cfg(unix)]
#[test]
fn a_resume_file_whose_header_does_not_read_ends_and_reaps_a_child_that_ignores_the_hangup() {
    use std::os::unix::process::CommandExt;

    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_short_test_directory();
    let binary_path = copy_koshi_binary(test_home_directory.path());
    let session_id = SessionId::new();
    let resume_file_path = resolve_resume_file_path(runtime_directory.path(), session_id);
    std::fs::write(&resume_file_path, UNREADABLE_RESUME_FILE_BYTES)
        .expect("the cut-off resume file is placed");
    let [process_id_read_file_descriptor, process_id_write_file_descriptor] =
        open_process_id_pipe();
    let mut process_command = build_session_server_command(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
        session_id,
    );
    process_command.arg("--resume").arg(&resume_file_path);
    // The session server starts as the parent of a child that leads its own
    // session, as a pane child does, ignores `SIGHUP`, as `nohup` makes it, and
    // sleeps for 30 seconds. The child's process id goes down the pipe.
    // SAFETY: the closure runs in the forked process before exec. It calls
    // only `fork`, `setsid`, `signal`, `sleep`, `_exit`, and `write`,
    // allocates nothing, and writes only from the stack value
    // `hangup_ignoring_child_process_id`.
    unsafe {
        process_command.pre_exec(move || {
            let hangup_ignoring_child_process_id = libc::fork();
            if hangup_ignoring_child_process_id == 0 {
                libc::setsid();
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::sleep(30);
                libc::_exit(0);
            }
            if hangup_ignoring_child_process_id < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::write(
                process_id_write_file_descriptor,
                (&hangup_ignoring_child_process_id as *const libc::pid_t).cast(),
                std::mem::size_of::<libc::pid_t>(),
            );
            Ok(())
        });
    }

    let _session_server_process = start_session_server_with_command(&mut process_command);
    // SAFETY: `close` takes a descriptor this test opened and uses no more.
    unsafe {
        libc::close(process_id_write_file_descriptor);
    }
    let hangup_ignoring_child_process_id =
        read_process_id_from_pipe(process_id_read_file_descriptor);

    wait_until_process_is_reaped(hangup_ignoring_child_process_id);
    // SAFETY: `close` takes a descriptor this test opened and uses no more.
    unsafe {
        libc::close(process_id_read_file_descriptor);
    }
}
