//! What an attached client sees when its session server dies.
//!
//! A real session server runs as its own process; the test joins it the way the
//! client does — Hello then Attach on one connection — and reads the stream
//! after the server is killed. A killed server writes no goodbye, so the read
//! fails, which the client turns into "the session ended unexpectedly" and a
//! non-zero exit. A session told to end writes the quit frame first, which the
//! client turns into "the session ended" and a zero exit.
//!
//! Each test serves its own temporary runtime directory, under a short base
//! because a Unix socket path has an operating-system length cap.
//!
//! Reading a frame blocks forever, so the walk to the ending runs on a thread
//! this one can stop waiting on: a stream that never ends fails the test
//! instead of hanging it.
//!
//! Some tests run `koshi attach` as its own process and read its exit code and
//! message; others watch whether the session server itself ends when its last
//! client leaves, which is what `auto-close-session` decides. Each gets its own
//! home directory holding the `koshi.kdl` that process reads. Unix-only: on
//! Windows the config directory comes from a Win32 call no environment
//! variable redirects.

#[cfg(unix)]
use std::io::Read;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use koshi_core::command::CliExitCode;
use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
#[cfg(unix)]
use koshi_ipc::router::resolve_router_endpoint_path;
use koshi_ipc::transport::Connection;
use koshi_test_support::fixtures::build_test_runtime_directory;
#[cfg(unix)]
use tempfile::TempDir;

mod common;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const ATTACH_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// The terminal size the attaching client in this test reports.
const ATTACH_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The display name the session server is started under, standing in for the
/// one the router generates.
const SESSION_SERVER_NAME: &str = "workspace";

/// A session server the test started. Dropping it ends that server, so a
/// failed assertion leaves nothing running.
struct RunningSession {
    child_process: Child,
}

impl RunningSession {
    /// End the server outright — `SIGKILL` on Unix, `TerminateProcess` on
    /// Windows — and collect it, so no goodbye of any kind can be written.
    fn terminate_session_server(&mut self) {
        self.child_process
            .kill()
            .expect("the session server can be ended");
        self.child_process
            .wait()
            .expect("the ended session server is collected");
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// Start the `koshi` binary as one session's server serving `runtime_directory`,
/// under the identity the router would have handed it.
fn start_session_server(runtime_directory: &Path, session_id: SessionId) -> RunningSession {
    let child_process = std::process::Command::new(env!("CARGO_BIN_EXE_koshi"))
        .arg("serve-session")
        .arg(session_id.to_string())
        .arg(SESSION_SERVER_NAME)
        .arg("--runtime-dir")
        .arg(runtime_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the koshi binary starts");
    RunningSession { child_process }
}

/// Open a connection to the session server, with its handshake already done,
/// retrying until the server answers.
fn open_session_connection(runtime_directory: &Path, session_id: SessionId) -> Connection {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(connection) = try_open_session_connection(runtime_directory, session_id) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no session server answered for {session_id}"
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a connection: read the endpoint file, connect, and
/// send the Hello that opens the connection.
///
/// `None` means the session server has yet to bind its socket and advertise
/// the token the Hello presents; the next attempt reads the file again.
fn try_open_session_connection(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Option<Connection> {
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .ok()?;
    let mut connection = Connection::connect(&endpoint.socket_address).ok()?;
    let hello = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: endpoint.connection_token,
            is_remote: false,
        },
    };
    connection.send(&hello).ok()?;
    let ipc_response: IpcResponse = connection.recv().ok()?;
    match ipc_response.answer_result {
        IpcResult::Hello { .. } => Some(connection),
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Attach on `connection` the way the attached client does. The connection
/// carries only that client's event stream afterwards.
fn attach_test_client(connection: &mut Connection, session_id: SessionId) {
    let request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Attach {
            viewport: ATTACH_VIEWPORT_SIZE,
            event_filter: EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
    };
    connection
        .send(&request)
        .expect("the server reads the attach");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the attach");
    assert_eq!(ipc_response.request_id, Some(2));
    let IpcResult::Attached {
        session_id: joined, ..
    } = ipc_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            ipc_response.answer_result
        );
    };
    assert_eq!(joined, session_id);
}

/// Read `connection`'s event stream the way the attached client reads it — a
/// frame that says nothing about the ending is passed over — and hand back the
/// frame or the read failure that ended it. Fails the test once [`WAIT_DURATION`] has
/// passed with no ending.
fn read_session_ending(mut connection: Connection) -> Result<SessionEvent, IpcError> {
    let (ending_tx, ending_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ending = loop {
            match connection.recv::<SessionEvent>() {
                Ok(SessionEvent::Detached) => break Ok(SessionEvent::Detached),
                Ok(SessionEvent::Quit) => break Ok(SessionEvent::Quit),
                Ok(_) => {}
                Err(receive_error) => break Err(receive_error),
            }
        };
        let _ = ending_tx.send(ending);
    });
    ending_rx
        .recv_timeout(WAIT_DURATION)
        .expect("the event stream ends")
}

/// A fresh home directory for the `koshi` processes a test starts to derive
/// their runtime directory from, so those processes never meet the session a
/// developer is running. Removed when the test drops it.
///
/// The name is one letter and six random characters, so the home is
/// `/tmp/k` plus six characters — 12 bytes — and the directory a `koshi`
/// started under it serves is `<home>/run`, 16 bytes. The longest name these
/// tests bind in that directory is the session socket, `session-<uuid>.sock`
/// at 49 bytes, which makes the bound path 66 bytes against the 103 bytes a
/// Unix socket address holds.
#[cfg(unix)]
fn build_test_home_directory() -> TempDir {
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in("/tmp")
        .expect("a temporary home directory")
}

/// The runtime directory a `koshi` started by [`build_koshi_command_under_home`] with `home`
/// serves: `run/` inside the home directory.
#[cfg(unix)]
fn build_runtime_directory_under(home: &Path) -> PathBuf {
    home.join("run")
}

/// The config directory a `koshi` started by [`build_koshi_command_under_home`] with `home`
/// reads: macOS derives it from the home directory alone.
#[cfg(target_os = "macos")]
fn config_dir_under(home: &Path) -> PathBuf {
    home.join("Library/Application Support/koshi")
}

/// The config directory a `koshi` started by [`build_koshi_command_under_home`] with `home`
/// reads: `.config/koshi` inside the home directory.
#[cfg(all(unix, not(target_os = "macos")))]
fn config_dir_under(home: &Path) -> PathBuf {
    home.join(".config/koshi")
}

/// Write `body` as the `koshi.kdl` a process started under `home` reads.
#[cfg(unix)]
fn write_test_config(home: &Path, config_text: &str) {
    let config_directory = config_dir_under(home);
    std::fs::create_dir_all(&config_directory).expect("a config directory under the test home");
    std::fs::write(config_directory.join("koshi.kdl"), config_text)
        .expect("the config file is written");
}

/// Start one session's server under `home`, so it reads the `koshi.kdl` written
/// there rather than the developer's own.
#[cfg(unix)]
fn start_session_server_under(
    home: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> RunningSession {
    let child_process = build_koshi_command_under_home(home)
        .arg("serve-session")
        .arg(session_id.to_string())
        .arg(SESSION_SERVER_NAME)
        .arg("--runtime-dir")
        .arg(runtime_directory)
        .stdout(Stdio::null())
        .spawn()
        .expect("the koshi binary starts");
    RunningSession { child_process }
}

/// Wait for `session_process`'s process to exit, and hand back whether it did inside
/// [`WAIT_DURATION`].
#[cfg(unix)]
fn wait_for_session_server_exit(session_process: &mut RunningSession) -> bool {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if matches!(session_process.child_process.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
}

/// The `koshi` binary, set to keep its files under `home` rather than in the
/// developer's own directories, and stripped of the pane identity so it runs
/// as a CLI outside any session. Standard input is closed, and both output
/// streams are pipes the test reads. The runtime directory the child serves is
/// `<home>/run`.
#[cfg(unix)]
fn build_koshi_command_under_home(home: &Path) -> std::process::Command {
    let mut process_command = std::process::Command::new(env!("CARGO_BIN_EXE_koshi"));
    process_command
        .env("HOME", home)
        .env("KOSHI_RUNTIME_DIR", home.join("run"))
        // The five variables the runtime injects at pane spawn; `KOSHI` is the
        // marker `InSessionContext::from_env` reads, and a test run from
        // inside a koshi pane would hand every one of them to this child.
        .env_remove("KOSHI")
        .env_remove("KOSHI_SESSION_ID")
        .env_remove("KOSHI_CLIENT_ID")
        .env_remove("KOSHI_PANE_ID")
        .env_remove("KOSHI_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // On Linux `XDG_CONFIG_HOME` beats `$HOME/.config`, so a machine that sets
    // it would send this child outside the test home for its `koshi.kdl`, past
    // the one the test wrote. macOS never reads this.
    #[cfg(all(unix, not(target_os = "macos")))]
    process_command.env("XDG_CONFIG_HOME", home.join(".config"));
    process_command
}

/// The router serving the runtime directory it names, which the attaching
/// client started on finding none running. Dropping this ends that router, so
/// a failed assertion leaves nothing running.
#[cfg(unix)]
struct RunningRouter {
    runtime_directory: PathBuf,
}

#[cfg(unix)]
impl Drop for RunningRouter {
    fn drop(&mut self) {
        let Ok(endpoint) =
            EndpointFile::load_from_path(&resolve_router_endpoint_path(&self.runtime_directory))
        else {
            return;
        };
        common::terminate_process(endpoint.process_id);
    }
}

/// A `koshi attach` the test started. Dropping it ends that client, so a
/// failed assertion leaves nothing running.
#[cfg(unix)]
struct RunningClient {
    child_process: Child,
}

#[cfg(unix)]
impl Drop for RunningClient {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// Start the `koshi` binary as a client attaching to `session_id`, the way a
/// user types `koshi attach <id>`.
#[cfg(unix)]
fn start_attaching_client(home: &Path, session_id: SessionId) -> RunningClient {
    let child_process = build_koshi_command_under_home(home)
        .arg("attach")
        .arg(session_id.to_string())
        .spawn()
        .expect("the koshi binary starts");
    RunningClient { child_process }
}

/// Why a started client is no longer running, for a failure message. `None`
/// while it is still up; otherwise its exit status and stderr, which name the
/// cause a "no client attached" failure would otherwise hide.
#[cfg(unix)]
fn describe_client_exit(client: &mut RunningClient) -> Option<String> {
    let exit_status = client.child_process.try_wait().ok().flatten()?;
    let mut stderr = String::new();
    if let Some(pipe) = client.child_process.stderr.as_mut() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Some(format!(
        "the client exited {exit_status}: {}",
        stderr.trim()
    ))
}

/// Wait until the session server answers, so the router the attaching client
/// starts holds this session after its opening sweep.
#[cfg(unix)]
fn wait_for_session_server(runtime_directory: &Path, session_id: SessionId) {
    drop(open_session_connection(runtime_directory, session_id));
}

/// Wait until the session server reports one attached client, so the client
/// under test is reading the event stream before the test ends the session.
#[cfg(unix)]
fn wait_for_attached_client(
    runtime_directory: &Path,
    session_id: SessionId,
    client: &mut RunningClient,
) {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        let overview =
            koshi_link::ipc_client::fetch_session_overview(runtime_directory, session_id)
                .expect("the session server describes itself");
        if overview.clients.len() == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no client attached to {session_id}; {}",
            describe_client_exit(client)
                .unwrap_or_else(|| "the client is still running".to_string())
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
}

/// What the attaching client left behind: its exit status, its output and its
/// errors, read once it has ended. Fails the test once [`WAIT_DURATION`] has passed
/// with the client still running.
#[cfg(unix)]
fn read_client_ending(client: &mut RunningClient) -> (std::process::ExitStatus, String, String) {
    let deadline = Instant::now() + WAIT_DURATION;
    let exit_status = loop {
        if let Some(exit_status) = client
            .child_process
            .try_wait()
            .expect("the client's state can be read")
        {
            break exit_status;
        }
        assert!(
            Instant::now() < deadline,
            "the attaching client kept running"
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    client
        .child_process
        .stdout
        .take()
        .expect("the client's output is a pipe")
        .read_to_string(&mut stdout)
        .expect("the client's output reads as text");
    client
        .child_process
        .stderr
        .take()
        .expect("the client's errors are a pipe")
        .read_to_string(&mut stderr)
        .expect("the client's errors read as text");
    (exit_status, stdout, stderr)
}

#[test]
fn a_killed_session_server_ends_the_stream_with_a_read_failure() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let mut session_process = start_session_server(runtime_directory.path(), session_id);

    let mut viewer_connection = open_session_connection(runtime_directory.path(), session_id);
    attach_test_client(&mut viewer_connection, session_id);

    session_process.terminate_session_server();

    // The server wrote neither goodbye frame, so the read reaches end of
    // stream on a socket the operating system closed with its process.
    let read_error =
        read_session_ending(viewer_connection).expect_err("the stream ends with a read failure");
    assert_eq!(read_error.to_string(), "ipc peer disconnected");
}

#[cfg(unix)]
#[test]
fn a_killed_session_server_ends_the_attaching_client_with_the_death_message() {
    let home = build_test_home_directory();
    let runtime_directory = build_runtime_directory_under(home.path());
    let session_id = SessionId::new();
    let mut session_process = start_session_server(&runtime_directory, session_id);
    let _router = RunningRouter {
        runtime_directory: runtime_directory.clone(),
    };

    wait_for_session_server(&runtime_directory, session_id);
    let mut client = start_attaching_client(home.path(), session_id);
    wait_for_attached_client(&runtime_directory, session_id, &mut client);

    session_process.terminate_session_server();

    let (exit_status, _, stderr) = read_client_ending(&mut client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        stderr,
        format!(
            "koshi: the session ended unexpectedly\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`\n"
        )
    );
}

/// A real client rides the session replacing its own process image: it leaves
/// the socket it was on, waits for the one the new image binds, and attaches
/// again there. The detach at the end ends the client, which only a client
/// reading a live connection reads.
#[cfg(unix)]
#[test]
fn an_attaching_client_comes_back_after_the_session_replaces_its_image() {
    let home = build_test_home_directory();
    let runtime_directory = build_runtime_directory_under(home.path());
    let session_id = SessionId::new();
    let _session = start_session_server(&runtime_directory, session_id);
    let _router = RunningRouter {
        runtime_directory: runtime_directory.clone(),
    };

    wait_for_session_server(&runtime_directory, session_id);
    let mut client = start_attaching_client(home.path(), session_id);
    wait_for_attached_client(&runtime_directory, session_id, &mut client);

    let endpoint_before_restart = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id),
    )
    .expect("the session advertises a socket");
    let mut control_connection = open_session_connection(&runtime_directory, session_id);
    control_connection
        .send(&IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Restart,
        })
        .expect("the server reads the restart");
    let ipc_response: IpcResponse = control_connection
        .recv()
        .expect("the server answers the restart");
    assert_eq!(ipc_response.answer_result, IpcResult::Restarting);
    drop(control_connection);

    // Every image binds a socket under a fresh token, so a token other than the
    // one read above is the new image serving.
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        let endpoint_attempt = EndpointFile::load_from_path(
            &EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id),
        );
        if endpoint_attempt.is_ok_and(|advertised_endpoint| {
            advertised_endpoint.connection_token.expose()
                != endpoint_before_restart.connection_token.expose()
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session advertised no new socket; {}",
            describe_client_exit(&mut client)
                .unwrap_or_else(|| "the client is still running".to_string())
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
    // The record is carried across the swap, so it is there before the client
    // comes back and a detach can land while nobody holds it. Asked for until
    // the client takes it.
    let deadline = Instant::now() + WAIT_DURATION;
    while client
        .child_process
        .try_wait()
        .expect("the client's state can be read")
        .is_none()
    {
        let detach_output = build_koshi_command_under_home(home.path())
            .arg("detach")
            .arg("--all")
            .arg(session_id.to_string())
            .output()
            .expect("the koshi binary starts");
        assert_eq!(
            detach_output.status.code(),
            Some(CliExitCode::Success.get_exit_code()),
            "the detach left {}",
            String::from_utf8_lossy(&detach_output.stderr)
        );
        assert!(
            Instant::now() < deadline,
            "the client never took a detach, so it never came back on the new socket"
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }

    let (exit_status, stdout, stderr) = read_client_ending(&mut client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(stderr, "");
    assert!(
        stdout.ends_with(&format!("detached from session {session_id}\n")),
        "the client ended with {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_detach_ends_the_attaching_client_with_a_success() {
    let home = build_test_home_directory();
    let runtime_directory = build_runtime_directory_under(home.path());
    let session_id = SessionId::new();
    let _session = start_session_server(&runtime_directory, session_id);
    let _router = RunningRouter {
        runtime_directory: runtime_directory.clone(),
    };

    wait_for_session_server(&runtime_directory, session_id);
    let mut client = start_attaching_client(home.path(), session_id);
    wait_for_attached_client(&runtime_directory, session_id, &mut client);

    // The session keeps running, so the goodbye frame the server writes as it
    // closes the client's queue is the whole ending the client reads.
    let detach_output = build_koshi_command_under_home(home.path())
        .arg("detach")
        .arg("--all")
        .arg(session_id.to_string())
        .output()
        .expect("the koshi binary starts");
    assert_eq!(
        detach_output.status.code(),
        Some(CliExitCode::Success.get_exit_code()),
        "the detach left {}",
        String::from_utf8_lossy(&detach_output.stderr)
    );

    let (exit_status, stdout, stderr) = read_client_ending(&mut client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(stderr, "");
    // The client leaves the alternate screen before it prints, so what it says
    // about the ending is the last thing on its output.
    assert!(
        stdout.ends_with(&format!("detached from session {session_id}\n")),
        "the client ended with {stdout:?}"
    );
}

/// `auto-close-session #true`: the session server process really ends when its
/// last client leaves, not merely that a flag was set.
#[cfg(unix)]
#[test]
fn auto_close_ends_the_session_server_process_when_the_last_client_leaves() {
    let home = build_test_home_directory();
    write_test_config(home.path(), "version 1\nauto-close-session #true\n");
    let runtime_directory = build_runtime_directory_under(home.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(home.path(), &runtime_directory, session_id);

    let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
    attach_test_client(&mut viewer_connection, session_id);
    // The connection ending is what the server reads as this client leaving.
    drop(viewer_connection);

    assert!(
        wait_for_session_server_exit(&mut session_process),
        "the session server outlived its last client"
    );
}

/// A session told to end writes the quit frame on every attached stream before
/// its process goes, so the client says the session ended instead of reporting
/// it dead.
#[cfg(unix)]
#[test]
fn a_kill_session_ends_every_attached_stream_with_the_quit_frame() {
    let home = build_test_home_directory();
    write_test_config(home.path(), "version 1\n");
    let runtime_directory = build_runtime_directory_under(home.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(home.path(), &runtime_directory, session_id);

    let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
    attach_test_client(&mut viewer_connection, session_id);

    // `kill-session` names no client, so the session ends rather than one
    // client leaving it.
    let kill_output = build_koshi_command_under_home(home.path())
        .arg("kill-session")
        .arg(session_id.to_string())
        .output()
        .expect("the koshi binary starts");

    assert_eq!(
        read_session_ending(viewer_connection).expect("the stream ends with a frame"),
        SessionEvent::Quit,
        "the kill left {}",
        String::from_utf8_lossy(&kill_output.stderr).trim()
    );
    assert!(
        wait_for_session_server_exit(&mut session_process),
        "the session server outlived the kill"
    );
}

/// The default leaves the session server running with nothing attached, which is
/// what makes `koshi attach` able to rejoin it.
#[cfg(unix)]
#[test]
fn a_session_server_outlives_its_last_client_by_default() {
    let home = build_test_home_directory();
    write_test_config(home.path(), "version 1\n");
    let runtime_directory = build_runtime_directory_under(home.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(home.path(), &runtime_directory, session_id);

    let mut viewer_connection = open_session_connection(&runtime_directory, session_id);
    attach_test_client(&mut viewer_connection, session_id);
    drop(viewer_connection);

    // It answers a fresh connection after the client that was attached is gone.
    let rejoined_connection = open_session_connection(&runtime_directory, session_id);
    drop(rejoined_connection);
    assert_eq!(
        session_process
            .child_process
            .try_wait()
            .expect("the session server's state can be read"),
        None,
        "the session server ended without being asked to"
    );
}
