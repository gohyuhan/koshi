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
use koshi_core::discovery::{PaneState, SessionOverview};
use koshi_core::event::Event;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_ipc::endpoint::{resume_path, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult, WireMouseAction,
};
use koshi_ipc::router::{
    router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress,
};
use koshi_ipc::transport::{Connection, FrameWriter};
use koshi_layout::mode::LayoutMode;
use tempfile::TempDir;

mod common;

use common::{copy_of_koshi, end_process, start_koshi};

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure. It is also the ceiling on a swap: a session that has
/// not come back by then has wedged, which is the failure this suite exists to
/// catch.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(50);

/// How long a test reads on for frames that must not arrive, before it calls
/// their absence settled.
const SETTLE: Duration = Duration::from_millis(750);

/// How long a test waits after reading the restart frame before it sends the
/// input a user typed into the window the swap leaves open. The session waits a
/// second for its clients to leave, so this sits inside that second and well
/// past the moment the session stops reading them on its own.
const TYPED_AFTER_THE_FRAME: Duration = Duration::from_millis(250);

/// How long a test waits for the session server to detach a client record
/// nobody claimed. The session server holds such a record for thirty seconds;
/// the rest is room for the swap and for the poll that watches.
const RECONNECT_WAIT: Duration = Duration::from_secs(75);

/// The display name the session server is started under, standing in for the
/// one the router generates.
const SESSION_NAME: &str = "workspace";

/// The terminal size the attaching client reports in the tests that need a pane
/// short enough for output to scroll off the top of it.
const SHORT_VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The terminal size the attaching client reports in the tests that need every
/// line a pane printed to stay on screen.
const TALL_VIEWPORT: Size = Size {
    cols: 100,
    rows: 40,
};

/// A fresh directory, under a short base so the Unix socket path stays inside
/// the operating system's path-length cap. Removed when the test drops it.
fn short_temp_dir() -> TempDir {
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
    child: Child,
    /// The pipe the ready line was read from, held open for as long as the
    /// guard lives: on Unix the image replacing this one inherits that pipe and
    /// writes its own ready line into it.
    _ready: BufReader<ChildStdout>,
}

impl RunningSession {
    /// True once the process the test started has ended. On Windows a restart
    /// hands over to a new process and this one ends; on Unix it keeps running,
    /// under the same process id, as the new image.
    fn has_exited(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the session server's state can be read")
            .is_some()
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A process the test did not start itself, held by its process id. Dropping it
/// ends that process.
///
/// A restart is guarded with one of these: on Windows the session runs in a
/// process the test never spawned, and on Unix it is the process the test
/// already holds, which a second ending does nothing to.
struct RunningProcess(u32);

impl Drop for RunningProcess {
    fn drop(&mut self) {
        end_process(self.0);
    }
}

/// A router the test started. Dropping it ends that router.
struct RunningRouter(Child);

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The `koshi` binary at `exe`, set to keep its files under `home` rather than
/// in the developer's own directories, and stripped of the pane identity so it
/// never reads the session a developer runs the test from.
///
/// Every variable the platform path resolvers read is pointed at `home`, on
/// every platform, so the config file this process reads is the one the test
/// home holds — none, which leaves every setting at its built-in default.
/// `KOSHI_RUNTIME_DIR` names `<home>/run`, which every caller here then
/// overrides with its own `--runtime-dir` argument.
fn koshi_under(exe: &Path, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KOSHI_RUNTIME_DIR", home.join("run"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("APPDATA", home.join("roaming"))
        .env("LOCALAPPDATA", home.join("local"))
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
    command.env("SHELL", "/bin/sh");
    command
}

/// Start the binary at `exe` as one session's server, under the identity the
/// router would have handed it, and wait for the ready line it prints once its
/// control socket is bound.
fn start_session_server(
    exe: &Path,
    home: &Path,
    runtime_dir: &Path,
    session_id: SessionId,
) -> RunningSession {
    let mut child = start_koshi(
        koshi_under(exe, home)
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_NAME)
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .stdout(Stdio::piped()),
    );
    let mut ready = BufReader::new(child.stdout.take().expect("the ready line is a pipe"));
    let mut line = String::new();
    ready
        .read_line(&mut line)
        .expect("the session server prints where it listens");
    assert!(
        line.contains("\"socket\""),
        "the ready line named no socket: {line}"
    );
    RunningSession {
        child,
        _ready: ready,
    }
}

/// Start the `koshi` binary as the router serving `runtime_dir`.
fn start_router(home: &Path, runtime_dir: &Path) -> RunningRouter {
    let child = start_koshi(
        koshi_under(Path::new(env!("CARGO_BIN_EXE_koshi")), home)
            .arg("serve-router")
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .stdout(Stdio::null()),
    );
    RunningRouter(child)
}

/// Open a connection to `session_id`'s control socket with its handshake
/// already done, retrying until the session answers, and hand back the endpoint
/// file the socket was advertised in.
fn open(runtime_dir: &Path, session_id: SessionId) -> (Connection, EndpointFile) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(opened) = try_open(runtime_dir, session_id) {
            return opened;
        }
        assert!(
            Instant::now() < deadline,
            "no session server answered for {session_id}"
        );
        std::thread::sleep(POLL);
    }
}

/// One attempt at opening a connection: read the endpoint file, connect, and
/// send the Hello that opens the connection.
///
/// `None` means the session server has yet to bind its socket and advertise the
/// token the Hello presents; the next attempt reads the file again.
fn try_open(runtime_dir: &Path, session_id: SessionId) -> Option<(Connection, EndpointFile)> {
    let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, session_id)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket).ok()?;
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::hello(endpoint.token.clone()),
    };
    connection.send(&hello).ok()?;
    let reply: IpcResponse = connection.recv().ok()?;
    match reply.result {
        IpcResult::Hello { .. } => Some((connection, endpoint)),
        IpcResult::Error(_) => None,
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// Ask the session `kind` on a connection that carries no client's event
/// stream, and hand back its answer.
fn request(connection: &mut Connection, request_id: u64, kind: IpcRequestKind) -> IpcResult {
    let request = IpcRequest { request_id, kind };
    connection
        .send(&request)
        .expect("the session reads the request");
    let reply: IpcResponse = connection.recv().expect("the session answers the request");
    assert_eq!(reply.request_id, Some(request_id));
    reply.result
}

/// Submit `command` on a control connection to `session_id`, targeting
/// `client_id`, and hand back the events it emitted. A rejected command fails
/// the test.
fn submit(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    command: Command,
) -> Vec<Event> {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::external_cli(Some(session_id), Some(client_id)),
        SystemTime::now(),
        command,
    );
    match request(
        connection,
        7,
        IpcRequestKind::SubmitCommand(Box::new(envelope)),
    ) {
        IpcResult::CommandResult(CommandResult::Ok { emitted_events, .. }) => emitted_events,
        other => panic!("the command was answered with {other:?}"),
    }
}

/// Split a new pane off the client's focused one, running `command`, and hand
/// back the pane the session created. `None` launches the platform shell.
fn new_pane(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    command: Option<SpawnSpec>,
) -> PaneId {
    let emitted = submit(
        connection,
        session_id,
        client_id,
        Command::NewPane(NewPaneArgs {
            source: None,
            tab: None,
            direction: Direction::Right,
            stacked: false,
            cwd: None,
            command,
            client: Some(client_id),
        }),
    );
    emitted
        .iter()
        .find_map(|event| match event {
            Event::PaneCreated(created) => Some(created.pane_id),
            _ => None,
        })
        .expect("the new pane is announced")
}

/// Type `line` into `pane`, ending it with the carriage return a terminal
/// sends for the Enter key.
fn type_line(
    connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    pane: PaneId,
    line: &str,
) {
    let mut data = line.as_bytes().to_vec();
    data.push(b'\r');
    submit(
        connection,
        session_id,
        client_id,
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane),
            data,
        }),
    );
}

/// The session's own description of itself, read over its control socket.
fn overview(runtime_dir: &Path, session_id: SessionId) -> SessionOverview {
    koshi_link::ipc_client::fetch_overview(runtime_dir, session_id)
        .expect("the session server describes itself")
}

/// The panes the session holds, in the order it reports them, with the state
/// each one is in.
fn pane_states(runtime_dir: &Path, session_id: SessionId) -> Vec<(PaneId, PaneState)> {
    let mut panes: Vec<(PaneId, PaneState)> = overview(runtime_dir, session_id)
        .panes
        .into_iter()
        .map(|pane| (pane.id, pane.state))
        .collect();
    panes.sort_by_key(|(id, _)| *id);
    panes
}

/// The one pane a freshly seeded session holds, before a test opens any of its
/// own.
fn seeded_pane(runtime_dir: &Path, session_id: SessionId) -> PaneId {
    let held = pane_states(runtime_dir, session_id);
    assert_eq!(held.len(), 1, "a seeded session holds one pane: {held:?}");
    held[0].0
}

/// The endpoint file `session_id` advertises once it is serving on a socket
/// other than the one `before` names.
///
/// A session server mints a fresh connection token every time it binds, so a
/// token other than `before`'s belongs to the socket the session came back on.
/// This is the same fact an attached client watches for.
fn endpoint_after_restart(
    runtime_dir: &Path,
    session_id: SessionId,
    before: &EndpointFile,
) -> EndpointFile {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(endpoint) = EndpointFile::read(&EndpointFile::path(runtime_dir, session_id)) {
            if endpoint.token.expose() != before.token.expose() {
                return endpoint;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the session advertised no new socket after its restart"
        );
        std::thread::sleep(POLL);
    }
}

/// The process ids whose parent is `parent`, in ascending order, as the
/// operating system reports them.
///
/// Unix only: there a pane's child is the session server's own child, and
/// `execvp` keeps it. On Windows a pane's child belongs to the process holding
/// the panes instead.
#[cfg(unix)]
fn children_of(parent: u32) -> Vec<u32> {
    let listed = std::process::Command::new("ps")
        .arg("-A")
        .arg("-o")
        .arg("pid=,ppid=")
        .output()
        .expect("the process list can be read");
    let text = String::from_utf8_lossy(&listed.stdout);
    let mut children: Vec<u32> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            let parent_pid: u32 = fields.next()?.parse().ok()?;
            (parent_pid == parent).then_some(pid)
        })
        .collect();
    children.sort_unstable();
    children
}

/// One attached client's event stream, read on a thread of its own so a test
/// never blocks forever on a frame that does not come, plus the writing half
/// that carries this client's own requests up.
struct Stream {
    /// The id the session minted or handed back for this client.
    client_id: ClientId,
    /// Every frame the reading thread has read, in arrival order. The read that
    /// ends the connection arrives here too.
    frames: Receiver<Result<SessionEvent, IpcError>>,
    /// This client's own uplink.
    writer: FrameWriter,
}

impl Stream {
    /// Join `session_id` as a viewing client at `viewport`, the way the
    /// attached client joins it: Hello, then Attach on the same connection.
    ///
    /// `resume` names the client record to come back as after the session
    /// replaced its own image, and is `None` on a first attach.
    fn join(
        runtime_dir: &Path,
        session_id: SessionId,
        viewport: Size,
        resume: Option<ClientId>,
    ) -> (Stream, EndpointFile) {
        let (mut connection, endpoint) = open(runtime_dir, session_id);
        let attach = IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::Attach {
                viewport,
                filter: EventFilterSpec::All,
                resume,
                resume_token: None,
                pane_area: None,
            },
        };
        connection
            .send(&attach)
            .expect("the session reads the attach");
        let reply: IpcResponse = connection.recv().expect("the session answers the attach");
        assert_eq!(reply.request_id, Some(2));
        let IpcResult::Attached {
            client_id,
            session_id: joined,
            ..
        } = reply.result
        else {
            panic!("expected an attach reply, got {:?}", reply.result);
        };
        assert_eq!(joined, session_id);

        let (mut reader, writer) = connection.split();
        let (frames_tx, frames) = mpsc::channel();
        std::thread::spawn(move || loop {
            let frame = reader.recv::<SessionEvent>();
            let ended = frame.is_err();
            if frames_tx.send(frame).is_err() || ended {
                return;
            }
        });
        (
            Stream {
                client_id,
                frames,
                writer,
            },
            endpoint,
        )
    }

    /// The next frame on the stream, or the read that ended it. Fails the test
    /// once [`WAIT`] has passed with nothing arriving.
    fn next(&self) -> Result<SessionEvent, IpcError> {
        match self.frames.recv_timeout(WAIT) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => panic!("no frame arrived within {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the event stream closed with no frame"),
        }
    }

    /// The first event `wanted` accepts. Fails the test once [`WAIT`] has
    /// passed with none, naming every event kind that did arrive.
    fn event_when(&self, wanted: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = Instant::now() + WAIT;
        let mut seen: Vec<&'static str> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(left) {
                Ok(Ok(event)) => {
                    if wanted(&event) {
                        return event;
                    }
                    seen.push(event.name());
                }
                Ok(Err(error)) => panic!("the event stream ended before the event: {error}"),
                Err(RecvTimeoutError::Timeout) => panic!(
                    "the event the test was waiting for never arrived within {WAIT:?}; \
                     the stream carried {seen:?}"
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the event stream closed before the event")
                }
            }
        }
    }

    /// The next painted frame, skipping every frame that carries no picture.
    /// Fails the test on a stream that ends first.
    fn painted(&self) -> PaintedFrame {
        loop {
            match self.next() {
                Ok(SessionEvent::Painted { frame }) => return *frame,
                Ok(_) => {}
                Err(error) => panic!("the event stream ended before a frame: {error}"),
            }
        }
    }

    /// The first painted frame `wanted` accepts. Fails the test once [`WAIT`]
    /// has passed with none, naming what the last painted frame held.
    fn painted_when(&self, wanted: impl Fn(&PaintedFrame) -> bool) -> PaintedFrame {
        let deadline = Instant::now() + WAIT;
        let mut last: Option<PaintedFrame> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(left) {
                Ok(Ok(SessionEvent::Painted { frame })) => {
                    if wanted(&frame) {
                        return *frame;
                    }
                    last = Some(*frame);
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => panic!("the event stream ended before a frame: {error}"),
                Err(RecvTimeoutError::Timeout) => panic!(
                    "no frame showed what the test was waiting for within {WAIT:?}; \
                     the last painted frame held {}",
                    last.as_ref()
                        .map_or("no painted frame at all".to_string(), described)
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the event stream closed with no frame")
                }
            }
        }
    }

    /// Read to the frame that says the session is replacing its own image, and
    /// hand back everything read before it.
    fn until_restarting(&self) -> Vec<SessionEvent> {
        let mut seen = Vec::new();
        loop {
            match self.next() {
                Ok(SessionEvent::Restarting) => return seen,
                Ok(frame) => seen.push(frame),
                Err(error) => panic!("the event stream ended before the restart: {error}"),
            }
        }
    }

    /// The last painted frame to arrive in the next `window`, and `None` when
    /// no frame is painted in it.
    fn last_painted_within(&self, window: Duration) -> Option<PaintedFrame> {
        self.drain(window)
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::Painted { frame } => Some(*frame),
                _ => None,
            })
            .next_back()
    }

    /// Everything that arrives in the next `window`. A stream that ends inside
    /// it contributes the frames it delivered first.
    fn drain(&self, window: Duration) -> Vec<SessionEvent> {
        let deadline = Instant::now() + window;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.frames.recv_timeout(left) {
                Ok(Ok(frame)) => seen.push(frame),
                Ok(Err(_)) | Err(_) => return seen,
            }
        }
        seen
    }

    /// Send one request up this client's own connection. The streaming half
    /// writes no response, so the answer is whatever reaches the event stream.
    fn send(&mut self, request_id: u64, kind: IpcRequestKind) {
        let request = IpcRequest { request_id, kind };
        self.writer
            .send(&request)
            .expect("the session reads the request");
    }

    /// Send one request up this client's own connection, passing over a
    /// connection the session has already closed. This is how a client behaves
    /// while the session is replacing its image: it keeps sending until the
    /// socket under it goes.
    fn send_while_open(&mut self, request_id: u64, kind: IpcRequestKind) {
        let request = IpcRequest { request_id, kind };
        let _ = self.writer.send(&request);
    }

    /// Move this client's view of `pane` up into scrollback by `lines`, and
    /// hand back the first frame painted after the session answered the round.
    ///
    /// The session answers exactly one round per request, so waiting for that
    /// answer is what makes the frame after it the scrolled one.
    fn scroll_up(&mut self, pane: PaneId, lines: usize) -> PaintedFrame {
        self.send(
            11,
            IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane,
                up: true,
                lines,
            }]),
        );
        loop {
            match self.next() {
                Ok(SessionEvent::MouseAnswer { request_id, .. }) => {
                    assert_eq!(request_id, 11);
                    return self.painted();
                }
                Ok(_) => {}
                Err(error) => panic!("the event stream ended before the scroll answer: {error}"),
            }
        }
    }
}

/// The rows `pane` shows in `frame`, each with its trailing blanks cut off and
/// every blank row at the bottom dropped.
///
/// A blank row between two rows of text is kept, so a gap in a pane's output is
/// visible in what this returns.
///
/// Empty when the frame carries no content for `pane`, and when the pane shows
/// no cells. A frame painted before the pane existed carries no content for it.
fn pane_rows(frame: &PaintedFrame, pane: PaneId) -> Vec<String> {
    let Some(content) = frame.panes.iter().find(|shown| shown.id == pane) else {
        return Vec::new();
    };
    let Some(window) = content.window.as_ref() else {
        return Vec::new();
    };
    let mut rows: Vec<String> = window
        .rows
        .iter()
        .map(|row| {
            row.cells()
                .iter()
                .filter(|cell| cell.width > 0)
                .map(|cell| cell.ch)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

/// Every pane in `frame`, each with the rows it shows, for a failure message.
///
/// Before → after: a frame holding one pane that printed one line reads
/// `pane-… : ["printed-1"]`.
fn described(frame: &PaintedFrame) -> String {
    if frame.panes.is_empty() {
        return "no panes".to_string();
    }
    frame
        .panes
        .iter()
        .map(|shown| format!("{} : {:?}", shown.id, pane_rows(frame, shown.id)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many scrollback lines `pane` holds in `frame`.
fn retained_lines(frame: &PaintedFrame, pane: PaneId) -> usize {
    frame
        .panes
        .iter()
        .find(|shown| shown.id == pane)
        .unwrap_or_else(|| panic!("the frame carries no content for pane {pane}"))
        .scrollback
        .retained_lines
}

/// A child that prints `count` lines reading `<prefix>-1` to `<prefix>-count`
/// as fast as it can, and then stays alive with nothing more to say.
fn burst_child(prefix: &str, count: u32) -> SpawnSpec {
    #[cfg(unix)]
    let script = format!(
        "i=1; while [ $i -le {count} ]; do printf '{prefix}-%d\\n' $i; i=$((i+1)); done; \
         sleep 300"
    );
    // The loop is parenthesised, which ends its body at the closing bracket.
    // `&` alone does not: it reads as one more command inside the body, and the
    // wait below then runs on the first pass instead of after the last one.
    #[cfg(windows)]
    let script =
        format!("(for /L %i in (1,1,{count}) do @echo {prefix}-%i) & ping -n 301 127.0.0.1 >nul");
    shell_child(&script)
}

/// A child that prints `count` lines reading `<prefix>-1` to `<prefix>-count`
/// with a pause between them, and then stays alive.
///
/// The pause is what lets a test send the restart while the child is still
/// printing, so the output really does cross the swap.
fn paced_child(prefix: &str, count: u32) -> SpawnSpec {
    #[cfg(unix)]
    let script = format!(
        "i=1; while [ $i -le {count} ]; do printf '{prefix}-%d\\n' $i; i=$((i+1)); \
         sleep 0.25; done; sleep 300"
    );
    #[cfg(windows)]
    let script = format!(
        "(for /L %i in (1,1,{count}) do @(echo {prefix}-%i & ping -n 2 127.0.0.1 >nul)) & \
         ping -n 301 127.0.0.1 >nul"
    );
    shell_child(&script)
}

/// A child that prints nothing and never exits: the case whose reader has
/// nothing to read and whose process cannot be waited on.
fn idle_child() -> SpawnSpec {
    #[cfg(unix)]
    let script = "sleep 300".to_string();
    #[cfg(windows)]
    let script = "ping -n 301 127.0.0.1 >nul".to_string();
    shell_child(&script)
}

/// A child that waits for one key and then exits reporting success.
fn key_then_exit_child() -> SpawnSpec {
    #[cfg(unix)]
    let script = "read line; exit 0".to_string();
    // `set /p` reads a line, the way `read` does. `pause` is not the same
    // thing: it takes a key event, which is not what a client writing bytes to
    // a pane produces.
    #[cfg(windows)]
    let script = "set /p x= & exit 0".to_string();
    shell_child(&script)
}

/// Run `script` through the platform's own command interpreter.
fn shell_child(script: &str) -> SpawnSpec {
    #[cfg(unix)]
    let (program, flag) = (PathBuf::from("/bin/sh"), "-c");
    #[cfg(windows)]
    let (program, flag) = (PathBuf::from("cmd.exe"), "/C");
    SpawnSpec {
        shell_kind: ShellKind::from_program(&program),
        program,
        args: vec![flag.to_string(), script.to_string()],
        cwd: None,
        env: BTreeMap::new(),
    }
}

#[test]
fn a_restart_keeps_every_pane_its_child_its_screen_and_its_scrollback() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let mut server = start_session_server(&exe, home.path(), dir.path(), session_id);

    // A short terminal, so the thirty lines each pane prints do not all fit and
    // the ones above the top land in scrollback.
    let (stream, before) = Stream::join(dir.path(), session_id, SHORT_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let seeded = seeded_pane(dir.path(), session_id);
    let left = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(burst_child("left", 30)),
    );
    let right = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(burst_child("right", 30)),
    );

    let printed = stream.painted_when(|frame| {
        pane_rows(frame, left)
            .last()
            .is_some_and(|row| row == "left-30")
            && pane_rows(frame, right)
                .last()
                .is_some_and(|row| row == "right-30")
    });
    // The last line each child printed ends with a newline, and that newline
    // moves the view one row on. The frame above can be the one painted between
    // the line and its newline, so this takes the last frame painted once the
    // children have gone quiet, and compares that against the swap.
    let live = stream.last_painted_within(SETTLE).unwrap_or(printed);
    let left_live = pane_rows(&live, left);
    let right_live = pane_rows(&live, right);
    let left_retained = retained_lines(&live, left);
    let right_retained = retained_lines(&live, right);

    #[cfg(unix)]
    let children_before = children_of(before.pid);

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);
    let (mut viewer, _) = Stream::join(dir.path(), session_id, SHORT_VIEWPORT, None);

    // The session that came back holds every pane it held, each with its child
    // still running.
    let mut expected = vec![
        (seeded, PaneState::Running),
        (left, PaneState::Running),
        (right, PaneState::Running),
    ];
    expected.sort_by_key(|(id, _)| *id);
    assert_eq!(pane_states(dir.path(), session_id), expected);

    // Every cell each pane showed before the swap is on screen again.
    let painted = viewer.painted_when(|frame| pane_rows(frame, left) == left_live);
    assert_eq!(pane_rows(&painted, left), left_live);
    assert_eq!(pane_rows(&painted, right), right_live);
    assert_eq!(retained_lines(&painted, left), left_retained);
    assert_eq!(retained_lines(&painted, right), right_retained);

    // The lines that scrolled off the top are still in scrollback: the view
    // moved to the oldest line it retains shows the first line each child ever
    // printed.
    let top = viewer.scroll_up(left, usize::MAX);
    assert_eq!(
        pane_rows(&top, left).first().map(String::as_str),
        Some("left-1")
    );
    let top = viewer.scroll_up(right, usize::MAX);
    assert_eq!(
        pane_rows(&top, right).first().map(String::as_str),
        Some("right-1")
    );

    #[cfg(unix)]
    {
        // The swap replaced this process's running image, so the session serves
        // under the process id it started with and every pane's child kept the
        // same parent and the same process id.
        assert!(!server.has_exited());
        assert_eq!(after.pid, before.pid);
        assert_eq!(children_of(after.pid), children_before);
    }
    #[cfg(windows)]
    {
        // The swap handed over to a new process, which took the panes back from
        // the process holding them — the one that outlived both session
        // servers.
        let deadline = Instant::now() + WAIT;
        while !server.has_exited() {
            assert!(
                Instant::now() < deadline,
                "the session server that handed over kept running"
            );
            std::thread::sleep(POLL);
        }
        assert_ne!(after.pid, before.pid);
    }
}

#[test]
fn output_written_across_the_swap_arrives_once_and_in_order() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    // A tall terminal, so every line the child prints stays on screen and the
    // run can be read whole.
    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(paced_child("mark", 12)),
    );

    // The restart goes out while the child is still printing, so the run has to
    // cross the parking, the swap and the reader coming back.
    stream.painted_when(|frame| pane_rows(frame, pane).contains(&"mark-3".to_string()));
    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);
    let (viewer, _) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);

    let finished =
        viewer.painted_when(|frame| pane_rows(frame, pane).contains(&"mark-12".to_string()));
    let printed: Vec<String> = (1..=12).map(|mark| format!("mark-{mark}")).collect();
    assert_eq!(pane_rows(&finished, pane), printed);
}

#[test]
fn input_sent_after_the_clients_are_told_still_reaches_its_pane() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (mut stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let seeded = seeded_pane(dir.path(), session_id);
    // A child that ends the moment it reads one line, so the panes the session
    // holds after the swap say whether the line reached it.
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(key_then_exit_child()),
    );
    let mut open_panes = vec![(seeded, PaneState::Running), (pane, PaneState::Running)];
    open_panes.sort_by_key(|(id, _)| *id);
    assert_eq!(pane_states(dir.path(), session_id), open_panes);

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    // A client learns the session is going only when it reads this frame, so
    // what follows is what a user types into the window the swap leaves open.
    stream.until_restarting();
    // Long enough that the swap has reached the point where it stops reading
    // its clients, and short enough to be well inside the wait it gives them.
    // Sending the moment the frame arrives would land in the microseconds
    // before that point and prove nothing.
    std::thread::sleep(TYPED_AFTER_THE_FRAME);
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(stream.client_id),
        SystemTime::now(),
        Command::WriteToPane(WriteToPaneArgs {
            pane: Some(pane),
            data: b"typed\r".to_vec(),
        }),
    );
    stream.send_while_open(20, IpcRequestKind::SubmitCommand(Box::new(envelope)));
    // What a real client sends the moment it reads that frame. Requests arrive
    // in the order they were queued, so the session reads the line above before
    // it reads this, and this is what the swap waits for.
    stream.send_while_open(21, IpcRequestKind::Leaving);
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);

    // The line reached the child: it read the line, exited, and its pane closed
    // with it. A swap that dropped the line leaves that child waiting, and the
    // pane open, until this deadline fails the test.
    let deadline = Instant::now() + WAIT;
    while pane_states(dir.path(), session_id) != vec![(seeded, PaneState::Running)] {
        assert!(
            Instant::now() < deadline,
            "the pane the line was sent to is still open, so its child never read it"
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_client_that_never_leaves_does_not_hold_the_swap_up() {
    // The swap waits for every told client to say it is leaving. A client whose
    // window froze never says it, so the wait is bounded: the session cuts the
    // connections still open and carries itself out anyway.
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let seeded = seeded_pane(dir.path(), session_id);

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    // The stream is held open and says nothing from here on.
    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);

    // The session came back with the pane it was carrying, so the cut cost it
    // nothing it held.
    assert_eq!(
        pane_states(dir.path(), session_id),
        vec![(seeded, PaneState::Running)]
    );
    drop(stream);
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
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, _) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    // One line, then a child that stays alive, so the last row the pane shows is
    // that line and nothing races it.
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(burst_child("printed", 1)),
    );

    let painted = stream.painted_when(|frame| {
        pane_rows(frame, pane)
            .last()
            .is_some_and(|row| row == "printed-1")
    });
    assert_eq!(
        pane_rows(&painted, pane).last().map(String::as_str),
        Some("printed-1")
    );
}

#[test]
fn a_pane_whose_child_never_exits_does_not_hold_the_swap_up() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let seeded = seeded_pane(dir.path(), session_id);
    // A child that cannot exit and whose reader has nothing to read: ending
    // that reader and waiting for it would never return.
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(idle_child()),
    );

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    drop(control);

    // The wait is the assertion: a swap that wedged never advertises a new
    // socket, and this fails on the deadline instead of hanging.
    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);

    let mut expected = vec![(seeded, PaneState::Running), (pane, PaneState::Running)];
    expected.sort_by_key(|(id, _)| *id);
    assert_eq!(pane_states(dir.path(), session_id), expected);
}

#[test]
fn a_client_that_comes_back_keeps_its_id_its_focus_and_its_zoom() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(idle_child()),
    );

    // A focus and a zoom this client alone holds, both distinct from what a
    // freshly minted client would come up with.
    submit(
        &mut control,
        session_id,
        stream.client_id,
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: Some(stream.client_id),
        }),
    );
    submit(
        &mut control,
        session_id,
        stream.client_id,
        Command::TogglePaneFullscreen,
    );
    let zoomed = stream.painted_when(|frame| {
        frame.client.focused_pane == Some(pane)
            && frame.session.active_tab.layout_mode == LayoutMode::Fullscreen { focused: pane }
    });
    assert_eq!(zoomed.client.id, stream.client_id);

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);
    let (viewer, _) = Stream::join(
        dir.path(),
        session_id,
        TALL_VIEWPORT,
        Some(stream.client_id),
    );

    // The record came across the swap, so the session handed it back rather
    // than minting a fresh client.
    assert_eq!(viewer.client_id, stream.client_id);
    let painted = viewer.painted();
    assert_eq!(painted.client.id, stream.client_id);
    assert_eq!(painted.client.focused_pane, Some(pane));
    assert_eq!(
        painted.session.active_tab.layout_mode,
        LayoutMode::Fullscreen { focused: pane }
    );

    // The state file has done its work and is gone, so nothing keeps the
    // session's screens on disk and the router stops reading the session as
    // one that is still replacing its image.
    assert_eq!(
        std::fs::metadata(resume_path(dir.path(), session_id))
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
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(idle_child()),
    );
    let client_id = stream.client_id;
    submit(
        &mut control,
        session_id,
        client_id,
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(pane),
            client: Some(client_id),
        }),
    );
    stream.painted_when(|frame| frame.client.focused_pane == Some(pane));

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);

    let (first, _) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, Some(client_id));
    assert_eq!(
        first.client_id, client_id,
        "the caller that came back first is handed the record"
    );

    let (second, _) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, Some(client_id));

    assert_ne!(
        second.client_id, client_id,
        "the second caller naming the same record is given one of its own"
    );
    assert_ne!(second.client_id, first.client_id);

    // Both are attached, and the record that crossed the swap kept the focus it
    // came back with while the minted one starts with none of it.
    let mut attached: Vec<ClientId> = overview(dir.path(), session_id)
        .clients
        .into_iter()
        .map(|client| client.id)
        .collect();
    attached.sort();
    let mut expected = vec![client_id, second.client_id];
    expected.sort();
    assert_eq!(attached, expected);

    let painted = first.painted();
    assert_eq!(painted.client.id, client_id);
    assert_eq!(
        painted.client.focused_pane,
        Some(pane),
        "the first caller keeps the focus the record carried across the swap"
    );
    assert_eq!(second.painted().client.id, second.client_id);
}

#[test]
fn a_client_that_never_comes_back_is_detached_when_the_window_closes() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let client_id = stream.client_id;

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    // The session server closed its end when it wrote that frame, so dropping
    // this end leaves nothing of the connection behind. Nobody claims the
    // record from here.
    drop(stream);
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);

    // The record crossed the swap, so the session holds it while it waits.
    assert_eq!(
        overview(dir.path(), session_id)
            .clients
            .into_iter()
            .map(|client| client.id)
            .collect::<Vec<_>>(),
        vec![client_id]
    );

    // When the window closes the record is detached, so nothing is left holding
    // a place for a client that never returned.
    let deadline = Instant::now() + RECONNECT_WAIT;
    loop {
        let held = overview(dir.path(), session_id);
        if held.clients.is_empty() {
            assert_eq!(held.session.attached_clients, Vec::new());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the client that never came back is still attached"
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_restart_into_a_binary_that_cannot_run_is_refused_and_the_session_keeps_serving() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let mut server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let seeded = seeded_pane(dir.path(), session_id);
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(idle_child()),
    );

    // A running program can be renamed on every supported platform, and on Unix
    // its mode can be changed under it; either one is what an update that
    // arrived broken leaves behind.
    #[cfg(unix)]
    let refusal = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644))
            .expect("the binary loses its execute permission");
        format!("the binary at {} is not executable", exe.display())
    };
    #[cfg(windows)]
    let refusal = {
        let moved = exe.with_extension("moved");
        std::fs::rename(&exe, &moved).expect("the binary is moved aside");
        let missing = std::fs::metadata(&exe).expect_err("nothing is at that path");
        format!(
            "the binary at {} could not be read: {missing}",
            exe.display()
        )
    };

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: refusal,
        })
    );

    // Nothing was torn down for the refused restart: the session serves the
    // socket it bound, holds both panes, and its client is still streaming.
    assert!(!server.has_exited());
    assert_eq!(
        EndpointFile::read(&EndpointFile::path(dir.path(), session_id))
            .expect("the session still advertises its socket")
            .token
            .expose(),
        before.token.expose()
    );
    let mut expected = vec![(seeded, PaneState::Running), (pane, PaneState::Running)];
    expected.sort_by_key(|(id, _)| *id);
    assert_eq!(pane_states(dir.path(), session_id), expected);
    assert_eq!(stream.painted().client.id, stream.client_id);
}

#[test]
fn a_swap_that_cannot_write_its_state_leaves_the_session_serving_with_live_readers() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let mut server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    // Two shells, so each pane can be asked for output of its own after the
    // swap fails.
    let seeded = seeded_pane(dir.path(), session_id);
    let opened = new_pane(&mut control, session_id, stream.client_id, None);

    // A directory where the carried state has to be written: every check the
    // restart makes passes, and the write that follows them cannot land.
    std::fs::create_dir(resume_path(dir.path(), session_id))
        .expect("a directory takes the resume file's place");

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    stream.until_restarting();
    drop(control);

    // The session put itself back on its feet in the process it was already in,
    // on a socket carrying a fresh token — the same thing a client watches for
    // after a swap that did work.
    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);
    assert!(!server.has_exited());
    #[cfg(unix)]
    assert_eq!(after.pid, before.pid);

    let (viewer, _) = Stream::join(
        dir.path(),
        session_id,
        TALL_VIEWPORT,
        Some(stream.client_id),
    );
    assert_eq!(viewer.client_id, stream.client_id);

    let mut expected = vec![(seeded, PaneState::Running), (opened, PaneState::Running)];
    expected.sort_by_key(|(id, _)| *id);
    assert_eq!(pane_states(dir.path(), session_id), expected);

    // The readers came back: a line typed into each pane reaches its shell and
    // that shell's answer reaches the screen.
    let (mut control, _) = open(dir.path(), session_id);
    type_line(
        &mut control,
        session_id,
        viewer.client_id,
        seeded,
        "echo koshi-seeded-back",
    );
    type_line(
        &mut control,
        session_id,
        viewer.client_id,
        opened,
        "echo koshi-opened-back",
    );
    viewer.painted_when(|frame| {
        pane_rows(frame, seeded).contains(&"koshi-seeded-back".to_string())
            && pane_rows(frame, opened).contains(&"koshi-opened-back".to_string())
    });
}

#[test]
fn a_pane_child_that_exits_around_the_swap_is_reported_exactly_once() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let _server = start_session_server(&exe, home.path(), dir.path(), session_id);

    let (stream, before) = Stream::join(dir.path(), session_id, TALL_VIEWPORT, None);
    let (mut control, _) = open(dir.path(), session_id);
    let seeded = seeded_pane(dir.path(), session_id);
    let pane = new_pane(
        &mut control,
        session_id,
        stream.client_id,
        Some(key_then_exit_child()),
    );

    // The line the child is waiting for. Its exit closes the pane, so the swap
    // that follows carries a session the pane has just left. The line carries a
    // word rather than being a bare return, so nothing rests on how a line
    // reader treats an empty line.
    type_line(&mut control, session_id, stream.client_id, pane, "typed");
    let mut reported = 0;
    let exited = stream.event_when(|event| {
        matches!(event, SessionEvent::PaneProcessExited { pane_id, .. } if *pane_id == pane)
    });
    let SessionEvent::PaneProcessExited { exit_code, .. } = exited else {
        unreachable!("the event matched above")
    };
    assert_eq!(exit_code, Some(0));
    reported += 1;
    assert_eq!(
        pane_states(dir.path(), session_id),
        vec![(seeded, PaneState::Running)]
    );

    assert_eq!(
        request(&mut control, 3, IpcRequestKind::Restart),
        IpcResult::Restarting
    );
    reported += exits_of(&stream.until_restarting(), pane);
    drop(control);

    let after = endpoint_after_restart(dir.path(), session_id, &before);
    let _restarted = RunningProcess(after.pid);
    let (viewer, _) = Stream::join(
        dir.path(),
        session_id,
        TALL_VIEWPORT,
        Some(stream.client_id),
    );
    reported += exits_of(&viewer.drain(SETTLE), pane);

    // One report for one exit: the swap neither swallowed it nor reaped the
    // child a second time and told the client twice.
    assert_eq!(reported, 1);
    assert_eq!(
        pane_states(dir.path(), session_id),
        vec![(seeded, PaneState::Running)]
    );
}

/// How many of `frames` report `pane`'s child exiting.
fn exits_of(frames: &[SessionEvent], pane: PaneId) -> usize {
    frames
        .iter()
        .filter(|frame| matches!(frame, SessionEvent::PaneProcessExited { pane_id, .. } if *pane_id == pane))
        .count()
}

#[test]
fn the_router_leaves_a_session_that_is_replacing_its_image_alone() {
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let _router = start_router(home.path(), dir.path());
    let mut router = router_connect(dir.path());

    let created = create_session(&mut router);
    let _session = RunningProcess(created.pid);

    // What the router sees while a session is replacing its own image: the
    // socket is unbound for that moment, and the resume file the session wrote
    // is sitting beside its endpoint file.
    std::fs::write(resume_path(dir.path(), created.id), b"{}").expect("the resume file is written");
    end_process(created.pid);

    // The listing probes every session it holds, and this one does not answer,
    // so it is left out of the answer either way.
    assert_eq!(list_session_ids(&mut router), Vec::new());
    // What the guard changes: the session's advertisement stays on the disk,
    // for the session's own new image to write over.
    assert!(EndpointFile::path(dir.path(), created.id).exists());

    // With the resume file gone the same listing takes that advertisement off
    // the disk, which is exactly what the guard held back.
    std::fs::remove_file(resume_path(dir.path(), created.id)).expect("the resume file is removed");
    assert_eq!(list_session_ids(&mut router), Vec::new());
    assert!(!EndpointFile::path(dir.path(), created.id).exists());
}

#[test]
fn a_resume_run_that_cannot_bind_its_socket_leaves_no_resume_file_behind() {
    // A new image can fail after it has started. The file it was started from
    // holds every pane's screen and scrollback, and no later run ever reads it,
    // so a run that cannot come up must still take it off the disk.
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    // The address a resume run for this session must bind, held by a session
    // server that is already serving it.
    let _holding_the_address = start_session_server(&exe, home.path(), dir.path(), session_id);

    let resume_file = resume_path(dir.path(), session_id);
    koshi_runtime::resume::write(
        &resume_file,
        &koshi_runtime::resume::ResumeHeader {
            format: koshi_runtime::resume::RESUME_FORMAT,
            session_id,
            session_name: SESSION_NAME.to_string(),
            panes: Vec::new(),
        },
        &koshi_runtime::resume::ResumeBody {
            sessions: std::collections::HashMap::new(),
            engines: std::collections::HashMap::new(),
            undecoded: std::collections::HashMap::new(),
            graphics_undecoded: std::collections::HashMap::new(),
            graphics_screen_continuation: std::collections::HashMap::new(),
            graphics_screen_wrapper_active: std::collections::HashMap::new(),
            graphics_tmux_continuation: std::collections::HashMap::new(),
            graphics_tmux_wrapper_active: std::collections::HashMap::new(),
            graphics_events: std::collections::HashMap::new(),
            graphics_transport: std::collections::HashMap::new(),
            quit: None,
        },
    )
    .expect("the resume file is written");
    assert!(
        resume_file.exists(),
        "the resume file is on the disk to start with"
    );

    let mut resuming = start_koshi(
        koshi_under(&exe, home.path())
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_NAME)
            .arg("--runtime-dir")
            .arg(dir.path())
            .arg("--resume")
            .arg(&resume_file)
            .stdout(Stdio::null()),
    );
    let status = wait_for_exit(&mut resuming);

    assert!(
        !status.success(),
        "a resume run that cannot bind its socket must fail"
    );
    assert!(
        !resume_file.exists(),
        "the resume file must not be left on the disk"
    );
}

/// Wait for `child` to end and hand back how it ended. A process still running
/// when the wait runs out fails the test, and is ended so nothing is left
/// behind.
fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("the process's state can be read") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the process was still running after {WAIT:?}");
        }
        std::thread::sleep(POLL);
    }
}

/// Open a connection to the router serving `runtime_dir`, with its handshake
/// already done, retrying until one answers.
fn router_connect(runtime_dir: &Path) -> Connection {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(connection) = try_router_connect(runtime_dir) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no router answered in {}",
            runtime_dir.display()
        );
        std::thread::sleep(POLL);
    }
}

/// One attempt at opening a router connection: read the endpoint file, connect,
/// and send the Hello that opens the connection.
fn try_router_connect(runtime_dir: &Path) -> Option<Connection> {
    let endpoint = EndpointFile::read(&router_endpoint_path(runtime_dir)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        kind: RouterRequestKind::hello(endpoint.token),
    };
    connection.send(&hello).ok()?;
    let reply: RouterResponse = connection.recv().ok()?;
    match reply.result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// Ask the router for a new session and hand back where it listens.
fn create_session(connection: &mut Connection) -> SessionAddress {
    match router_request(
        connection,
        RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        },
    ) {
        RouterResult::Created(address) => address,
        other => panic!("creating a session was answered with {other:?}"),
    }
}

/// The sessions the router lists, by id.
fn list_session_ids(connection: &mut Connection) -> Vec<SessionId> {
    match router_request(connection, RouterRequestKind::ListSessions) {
        RouterResult::Sessions(sessions) => sessions.into_iter().map(|row| row.id).collect(),
        other => panic!("listing the sessions was answered with {other:?}"),
    }
}

/// Ask the router `kind` on an open connection and hand back its answer.
fn router_request(connection: &mut Connection, kind: RouterRequestKind) -> RouterResult {
    let request = RouterRequest {
        request_id: 2,
        kind,
    };
    connection
        .send(&request)
        .expect("the router reads the request");
    let reply: RouterResponse = connection.recv().expect("the router answers the request");
    assert_eq!(reply.request_id, Some(2));
    reply.result
}
