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
use koshi_ipc::supervisor::compute_supervisor_socket_address;
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::supervisor::SupervisorPtyBackend;
use tempfile::TempDir;

mod common;

use common::{copy_koshi_binary, start_koshi_process};

/// How long a test waits for something it expects promptly, before it calls the
/// wait a failure.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const SUPERVISOR_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// The pane size every test here opens its pane at.
const PANE_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// The word the pane's child prints.
const OUTPUT_MARKER: &str = "koshi-supervisor-marker";

/// A supervisor process the test started. Dropping it ends that process.
struct RunningSupervisor {
    child_process: Child,
}

impl RunningSupervisor {
    /// The process id the supervisor binds its socket under.
    fn get_supervisor_process_id(&self) -> u32 {
        self.child_process.id()
    }
}

impl Drop for RunningSupervisor {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// Everything the link handed back, kept per pane.
#[derive(Default)]
struct PtyOutputCollection {
    /// Output bytes, oldest first, per pane.
    output_bytes_by_pane_id: Mutex<Vec<(PaneId, Vec<u8>)>>,
    /// Exits, in the order they arrived.
    exit_statuses_by_pane_id: Mutex<Vec<(PaneId, ExitStatus)>>,
}

impl PtyOutputCollection {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Everything `pane_id` printed so far, as text.
    fn get_output_text_for_pane(&self, pane_id: PaneId) -> String {
        let output_chunks_by_pane_id = self
            .output_bytes_by_pane_id
            .lock()
            .expect("collected output");
        let output_bytes: Vec<u8> = output_chunks_by_pane_id
            .iter()
            .filter(|(stored_pane_id, _)| *stored_pane_id == pane_id)
            .flat_map(|(_, output_chunk)| output_chunk.clone())
            .collect();
        String::from_utf8_lossy(&output_bytes).into_owned()
    }
}

impl PtySink for PtyOutputCollection {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.output_bytes_by_pane_id
            .lock()
            .expect("collected output")
            .push((pane_id, output_bytes));
        true
    }

    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        self.exit_statuses_by_pane_id
            .lock()
            .expect("collected exits")
            .push((pane_id, exit_status));
    }
}

/// A fresh directory, under a short base so the Unix socket path stays inside
/// the operating system's path-length cap. Removed when the test drops it.
fn build_short_temporary_directory() -> TempDir {
    #[cfg(unix)]
    let temporary_directory_root = PathBuf::from("/tmp");
    #[cfg(windows)]
    let temporary_directory_root = std::env::temp_dir();
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(temporary_directory_root)
        .expect("a temporary directory")
}

/// Start the `koshi` binary at `executable_path` as the supervisor for
/// `session_id`.
fn start_supervisor_process(
    executable_path: &std::path::Path,
    runtime_directory: &std::path::Path,
    session_id: SessionId,
    connection_token: &ConnectionToken,
) -> RunningSupervisor {
    let mut command = std::process::Command::new(executable_path);
    command
        .arg("serve-pty-supervisor")
        .arg(session_id.to_string())
        .arg(connection_token.expose())
        .arg("--runtime-dir")
        .arg(runtime_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    RunningSupervisor {
        child_process: start_koshi_process(&mut command),
    }
}

/// Link to `supervisor`, retrying until it has bound its socket.
fn connect_to_supervisor(
    runtime_directory: &std::path::Path,
    session_id: SessionId,
    supervisor: &RunningSupervisor,
    connection_token: &ConnectionToken,
    sink: Arc<dyn PtySink>,
) -> SupervisorPtyBackend {
    let socket_address = compute_supervisor_socket_address(
        runtime_directory,
        session_id,
        supervisor.get_supervisor_process_id(),
    );
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        match SupervisorPtyBackend::connect(
            &socket_address,
            connection_token.clone(),
            Arc::clone(&sink),
            &[],
        ) {
            Ok(supervisor_backend) => return supervisor_backend,
            Err(connect_error) => {
                assert!(
                    Instant::now() < deadline,
                    "the supervisor at {socket_address} never answered a link: {connect_error}"
                );
                std::thread::sleep(SUPERVISOR_POLL_INTERVAL_DURATION);
            }
        }
    }
}

/// A child that ends as soon as it is given one line, and exits with `0`.
///
/// `set /p` reads a line, the way `read` does. `pause` is not the same thing: it
/// takes a key event, which is not what a client writing bytes to a pane
/// produces.
fn build_line_then_exit_spawn_spec() -> SpawnSpec {
    #[cfg(unix)]
    let (program, shell_command_flag, shell_script) =
        ("/bin/sh", "-c", "read line; exit 0".to_string());
    #[cfg(windows)]
    let (program, shell_command_flag, shell_script) =
        ("cmd.exe", "/C", "set /p x= & exit 0".to_string());
    let program = PathBuf::from(program);
    SpawnSpec {
        shell_kind: ShellKind::from_program(&program),
        program,
        arguments: vec![shell_command_flag.to_string(), shell_script],
        working_directory: None,
        environment_variables: BTreeMap::new(),
    }
}

/// A child that prints [`OUTPUT_MARKER`] and then stays alive, so what arrives is its
/// output and not the flush of a child that ended.
fn build_printing_spawn_spec() -> SpawnSpec {
    #[cfg(unix)]
    let (program, shell_command_flag, shell_script) = (
        "/bin/sh",
        "-c",
        format!("printf '{OUTPUT_MARKER}'; sleep 300"),
    );
    #[cfg(windows)]
    let (program, shell_command_flag, shell_script) =
        ("cmd.exe", "/K", format!("echo {OUTPUT_MARKER}"));
    let program = PathBuf::from(program);
    SpawnSpec {
        shell_kind: ShellKind::from_program(&program),
        program,
        arguments: vec![shell_command_flag.to_string(), shell_script],
        working_directory: None,
        environment_variables: BTreeMap::new(),
    }
}

#[test]
fn a_pane_opened_in_the_supervisor_process_prints_back_over_the_link() {
    // The one hop a session server takes for every pane it owns on Windows: a
    // separate process holds the pane's terminal, and the child's bytes come
    // back over the link. Nothing here answers the pane terminal's
    // cursor-position query; the pane's own reader does, inside the supervisor.
    let home_directory = build_short_temporary_directory();
    let runtime_directory = build_short_temporary_directory();
    let executable_path = copy_koshi_binary(home_directory.path());
    let session_id = SessionId::new();
    let connection_token = ConnectionToken::generate();
    let supervisor = start_supervisor_process(
        &executable_path,
        runtime_directory.path(),
        session_id,
        &connection_token,
    );

    let output_collection = PtyOutputCollection::new();
    let pty_backend = connect_to_supervisor(
        runtime_directory.path(),
        session_id,
        &supervisor,
        &connection_token,
        Arc::clone(&output_collection) as Arc<dyn PtySink>,
    );

    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(pane_id, build_printing_spawn_spec(), PANE_SIZE)
        .expect("the supervisor opens the pane");

    let deadline = Instant::now() + WAIT_DURATION;
    while !output_collection
        .get_output_text_for_pane(pane_id)
        .contains(OUTPUT_MARKER)
    {
        assert!(
            Instant::now() < deadline,
            "the pane's output never crossed the link; it held {:?}",
            output_collection.get_output_text_for_pane(pane_id)
        );
        std::thread::sleep(SUPERVISOR_POLL_INTERVAL_DURATION);
    }

    assert_eq!(
        output_collection
            .exit_statuses_by_pane_id
            .lock()
            .expect("collected exits")
            .as_slice(),
        [],
        "the pane that printed is still running"
    );

    pty_backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");
}

#[test]
fn a_line_written_the_moment_a_pane_opens_reaches_its_child() {
    // Both halves of the one hop, at the worst moment for it: the pane is
    // written to as soon as the supervisor answers that it opened, which is
    // before that pane's terminal has said anything. The child ends on the
    // first line it is given, so its exit crossing the link is that line having
    // arrived.
    let home_directory = build_short_temporary_directory();
    let runtime_directory = build_short_temporary_directory();
    let executable_path = copy_koshi_binary(home_directory.path());
    let session_id = SessionId::new();
    let connection_token = ConnectionToken::generate();
    let supervisor = start_supervisor_process(
        &executable_path,
        runtime_directory.path(),
        session_id,
        &connection_token,
    );

    let output_collection = PtyOutputCollection::new();
    let pty_backend = connect_to_supervisor(
        runtime_directory.path(),
        session_id,
        &supervisor,
        &connection_token,
        Arc::clone(&output_collection) as Arc<dyn PtySink>,
    );

    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(pane_id, build_line_then_exit_spawn_spec(), PANE_SIZE)
        .expect("the supervisor opens the pane");
    pty_backend
        .write_pane_input(pane_id, b"typed\r")
        .expect("the line is written");

    let deadline = Instant::now() + WAIT_DURATION;
    while output_collection
        .exit_statuses_by_pane_id
        .lock()
        .expect("collected exits")
        .is_empty()
    {
        assert!(
            Instant::now() < deadline,
            "the line never reached the child; the pane printed {:?}",
            output_collection.get_output_text_for_pane(pane_id)
        );
        std::thread::sleep(SUPERVISOR_POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        output_collection
            .exit_statuses_by_pane_id
            .lock()
            .expect("collected exits")
            .as_slice(),
        [(pane_id, ExitStatus::ExitCode(0))]
    );

    pty_backend
        .shutdown_supervisor()
        .expect("the supervisor is told to end");
}
