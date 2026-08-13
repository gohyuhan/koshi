//! Cross-process tests for the helper process that holds one session's panes.
//!
//! The supervisor here is the real `koshi` binary started under its own
//! subcommand, not a copy of it running on a thread of this process. That is the
//! hop a session server takes on Windows: it starts this process, links to the
//! socket the process binds, opens panes over that link, and reads their output
//! back over it.
//!
//! Nothing here is gated to one operating system. The supervisor subcommand
//! builds and runs on every platform, so the same hop is covered everywhere.
//!
//! Every process a test starts is held in a guard that ends it when the test
//! drops it, so a failed assertion leaves nothing running.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, PtySize, ShellKind, SpawnSpec};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::supervisor::supervisor_socket_addr;
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::supervisor::SupervisorPtyBackend;
use tempfile::TempDir;

mod common;

use common::{copy_of_koshi, start_koshi};

/// How long a test waits for something it expects promptly, before it calls the
/// wait a failure.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(50);

/// The pane size every test here opens its pane at.
const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// The word the pane's child prints.
const MARKER: &str = "koshi-supervisor-marker";

/// A supervisor process the test started. Dropping it ends that process.
struct RunningSupervisor(Child);

impl RunningSupervisor {
    /// The process id the supervisor binds its socket under.
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for RunningSupervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Everything the link handed back, kept per pane.
#[derive(Default)]
struct Collected {
    /// Output bytes, oldest first, per pane.
    output: Mutex<Vec<(PaneId, Vec<u8>)>>,
    /// Exits, in the order they arrived.
    exits: Mutex<Vec<(PaneId, ExitStatus)>>,
}

impl Collected {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Everything `pane` printed so far, as text.
    fn text(&self, pane: PaneId) -> String {
        let held = self.output.lock().expect("collected output");
        let bytes: Vec<u8> = held
            .iter()
            .filter(|(id, _)| *id == pane)
            .flat_map(|(_, chunk)| chunk.clone())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl PtySink for Collected {
    fn output(&self, pane: PaneId, bytes: Vec<u8>) -> bool {
        self.output
            .lock()
            .expect("collected output")
            .push((pane, bytes));
        true
    }

    fn exit(&self, pane: PaneId, status: ExitStatus) {
        self.exits
            .lock()
            .expect("collected exits")
            .push((pane, status));
    }
}

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

/// Start the `koshi` binary at `exe` as the supervisor for `session_id`.
fn start_supervisor(
    exe: &std::path::Path,
    runtime_dir: &std::path::Path,
    session_id: SessionId,
    token: &ConnectionToken,
) -> RunningSupervisor {
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve-pty-supervisor")
        .arg(session_id.to_string())
        .arg(token.expose())
        .arg("--runtime-dir")
        .arg(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    RunningSupervisor(start_koshi(&mut command))
}

/// Link to `supervisor`, retrying until it has bound its socket.
fn link_to(
    runtime_dir: &std::path::Path,
    session_id: SessionId,
    supervisor: &RunningSupervisor,
    token: &ConnectionToken,
    sink: Arc<dyn PtySink>,
) -> SupervisorPtyBackend {
    let addr = supervisor_socket_addr(runtime_dir, session_id, supervisor.pid());
    let deadline = Instant::now() + WAIT;
    loop {
        match SupervisorPtyBackend::connect(&addr, token.clone(), Arc::clone(&sink), &[]) {
            Ok(backend) => return backend,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the supervisor at {addr} never answered a link: {error}"
                );
                std::thread::sleep(POLL);
            }
        }
    }
}

/// A child that ends as soon as it is given one line, and exits with `0`.
///
/// `set /p` reads a line, the way `read` does. `pause` is not the same thing: it
/// takes a key event, which is not what a client writing bytes to a pane
/// produces.
fn line_then_exit_child() -> SpawnSpec {
    #[cfg(unix)]
    let (program, flag, script) = ("/bin/sh", "-c", "read line; exit 0".to_string());
    #[cfg(windows)]
    let (program, flag, script) = ("cmd.exe", "/C", "set /p x= & exit 0".to_string());
    let program = PathBuf::from(program);
    SpawnSpec {
        shell_kind: ShellKind::from_program(&program),
        program,
        args: vec![flag.to_string(), script],
        cwd: None,
        env: BTreeMap::new(),
    }
}

/// A child that prints [`MARKER`] and then stays alive, so what arrives is its
/// output and not the flush of a child that ended.
fn printing_child() -> SpawnSpec {
    #[cfg(unix)]
    let (program, flag, script) = ("/bin/sh", "-c", format!("printf '{MARKER}'; sleep 300"));
    #[cfg(windows)]
    let (program, flag, script) = ("cmd.exe", "/K", format!("echo {MARKER}"));
    let program = PathBuf::from(program);
    SpawnSpec {
        shell_kind: ShellKind::from_program(&program),
        program,
        args: vec![flag.to_string(), script],
        cwd: None,
        env: BTreeMap::new(),
    }
}

#[test]
fn a_pane_opened_in_the_supervisor_process_prints_back_over_the_link() {
    // The one hop a session server takes for every pane it owns on Windows: a
    // separate process holds the pane's terminal, and the child's bytes come
    // back over the link. Nothing here answers the pane terminal's
    // cursor-position query; the pane's own reader does, inside the supervisor.
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let token = ConnectionToken::generate();
    let supervisor = start_supervisor(&exe, dir.path(), session_id, &token);

    let collected = Collected::new();
    let backend = link_to(
        dir.path(),
        session_id,
        &supervisor,
        &token,
        Arc::clone(&collected) as Arc<dyn PtySink>,
    );

    let pane = PaneId::new();
    backend
        .spawn(pane, printing_child(), SIZE)
        .expect("the supervisor opens the pane");

    let deadline = Instant::now() + WAIT;
    while !collected.text(pane).contains(MARKER) {
        assert!(
            Instant::now() < deadline,
            "the pane's output never crossed the link; it held {:?}",
            collected.text(pane)
        );
        std::thread::sleep(POLL);
    }

    assert_eq!(
        collected.exits.lock().expect("collected exits").as_slice(),
        [],
        "the pane that printed is still running"
    );

    backend.shut_down().expect("the supervisor is told to end");
}

#[test]
fn a_line_written_the_moment_a_pane_opens_reaches_its_child() {
    // Both halves of the one hop, at the worst moment for it: the pane is
    // written to as soon as the supervisor answers that it opened, which is
    // before that pane's terminal has said anything. The child ends on the
    // first line it is given, so its exit crossing the link is that line having
    // arrived.
    let home = short_temp_dir();
    let dir = short_temp_dir();
    let exe = copy_of_koshi(home.path());
    let session_id = SessionId::new();
    let token = ConnectionToken::generate();
    let supervisor = start_supervisor(&exe, dir.path(), session_id, &token);

    let collected = Collected::new();
    let backend = link_to(
        dir.path(),
        session_id,
        &supervisor,
        &token,
        Arc::clone(&collected) as Arc<dyn PtySink>,
    );

    let pane = PaneId::new();
    backend
        .spawn(pane, line_then_exit_child(), SIZE)
        .expect("the supervisor opens the pane");
    backend
        .write(pane, b"typed\r")
        .expect("the line is written");

    let deadline = Instant::now() + WAIT;
    while collected.exits.lock().expect("collected exits").is_empty() {
        assert!(
            Instant::now() < deadline,
            "the line never reached the child; the pane printed {:?}",
            collected.text(pane)
        );
        std::thread::sleep(POLL);
    }
    assert_eq!(
        collected.exits.lock().expect("collected exits").as_slice(),
        [(pane, ExitStatus::ExitCode(0))]
    );

    backend.shut_down().expect("the supervisor is told to end");
}
