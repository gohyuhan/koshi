//! What the tests in this directory share: ending a process the test did not
//! spawn, taking a copy of the `koshi` binary under test, building the command
//! that runs that binary under a test home, starting it as a router or as a
//! [`SessionProcess`], speaking to the router it serves, and the shell command
//! a test pane runs.
//! [`in_process_session`] runs one session server on a thread of the test
//! process. [`session_connection`] speaks to a session server either way.
//!
//! Every test binary declaring `mod common;` compiles all of it. No single
//! binary uses every helper, and an unused helper is allowed here.
#![allow(dead_code)]

pub mod in_process_session;
pub mod session_connection;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    resolve_router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress, SessionSelector,
};
use koshi_ipc::transport::Connection;
use koshi_test_support::fixtures::start_program_process;
use tempfile::TempDir;

/// How long a poll waits for something a started process or a session server
/// has to do before the test calls it a failure: 20 seconds.
pub const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts: 100 milliseconds.
pub const POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// The display name a test starts a session server under, standing in for the
/// one the router generates.
pub const SESSION_SERVER_NAME: &str = "workspace";

/// End the process with id `process_id`, whatever it is doing.
#[cfg(unix)]
pub fn terminate_process(process_id: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(process_id.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// End the process with id `process_id`, whatever it is doing.
#[cfg(windows)]
pub fn terminate_process(process_id: u32) {
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(process_id.to_string())
        .arg("/F")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A process the test did not start itself, held by its process id. Dropping it
/// ends that process.
pub struct RunningProcess {
    pub process_id: u32,
}

impl Drop for RunningProcess {
    fn drop(&mut self) {
        terminate_process(self.process_id);
    }
}

/// The session servers a test made a router start. Dropping it ends them, so a
/// test that kills its router leaves no session server behind.
pub struct RunningSessions {
    pub session_server_process_ids: Vec<u32>,
}

impl Drop for RunningSessions {
    fn drop(&mut self) {
        for process_id in &self.session_server_process_ids {
            terminate_process(*process_id);
        }
    }
}

/// A session server the test started as its own `koshi serve-session`
/// process. Dropping it ends that process.
pub struct SessionProcess {
    pub child_process: Child,
}

impl SessionProcess {
    /// End the process outright — `SIGKILL` on Unix, `TerminateProcess` on
    /// Windows — and collect it. The session server writes no frame of any
    /// kind after this.
    pub fn terminate_session_server(&mut self) {
        self.child_process
            .kill()
            .expect("the session server can be ended");
        self.child_process
            .wait()
            .expect("the ended session server is collected");
    }

    /// True once the process the test started has ended.
    pub fn has_session_server_exited(&mut self) -> bool {
        self.child_process
            .try_wait()
            .expect("the session server's state can be read")
            .is_some()
    }

    /// `it is still running` while the process runs. Once it has ended: `it
    /// exited <status>: <what it wrote to its error stream>`, for a failure
    /// message. An error stream that is not a pipe reads as empty.
    pub fn format_process_status(&mut self) -> String {
        let Some(exit_status) = self
            .child_process
            .try_wait()
            .expect("the session server's state can be read")
        else {
            return "it is still running".to_string();
        };
        let mut server_error_text = String::new();
        if let Some(stderr_pipe) = self.child_process.stderr.as_mut() {
            let _ = stderr_pipe.read_to_string(&mut server_error_text);
        }
        format!("it exited {exit_status}: {}", server_error_text.trim())
    }

    /// Read the endpoint file `session_id`'s server writes in
    /// `runtime_directory` every [`POLL_INTERVAL_DURATION`], and hand back the
    /// first one that reads.
    ///
    /// # Panics
    /// When none reads within [`WAIT_DURATION`]. The message carries
    /// [`SessionProcess::format_process_status`].
    pub fn wait_for_session_endpoint(
        &mut self,
        runtime_directory: &Path,
        session_id: SessionId,
    ) -> EndpointFile {
        let endpoint_file_path =
            EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
        let wait_deadline = Instant::now() + WAIT_DURATION;
        loop {
            if let Ok(endpoint_file) = EndpointFile::load_from_path(&endpoint_file_path) {
                return endpoint_file;
            }
            assert!(
                Instant::now() < wait_deadline,
                "no session server advertised {session_id}; {}",
                self.format_process_status()
            );
            std::thread::sleep(POLL_INTERVAL_DURATION);
        }
    }

    /// Poll [`SessionProcess::has_session_server_exited`] every
    /// [`POLL_INTERVAL_DURATION`], and hand back whether the process ended
    /// within [`WAIT_DURATION`].
    pub fn wait_for_session_server_exit(&mut self) -> bool {
        let wait_deadline = Instant::now() + WAIT_DURATION;
        loop {
            if self.has_session_server_exited() {
                return true;
            }
            if Instant::now() >= wait_deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL_DURATION);
        }
    }
}

impl Drop for SessionProcess {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// A router the test started. Dropping it ends that router.
pub struct RunningRouter {
    child_process: Child,
}

impl RunningRouter {
    /// The id of the router process.
    pub fn get_process_id(&self) -> u32 {
        self.child_process.id()
    }

    /// True once the router process has ended.
    pub fn has_router_exited(&mut self) -> bool {
        self.child_process
            .try_wait()
            .expect("the router's state can be read")
            .is_some()
    }
}

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// A fresh directory under a short base: `/tmp` on Unix, the temporary
/// directory on Windows. Removed when the test drops it.
///
/// The name is one letter and six random characters, so on Unix the directory
/// is `/tmp/k` plus six characters — 12 bytes — and a home built here serves
/// `<home_directory>/run`, 16 bytes. The longest name a test binds in a runtime
/// directory is the session socket, `session-<uuid>.sock` at 49 bytes, which
/// makes the bound path 66 bytes against the 103 bytes a Unix socket address
/// holds.
pub fn build_short_test_directory() -> TempDir {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in(base_directory)
        .expect("a temporary directory")
}

/// Run `shell_script` through `/bin/sh -c` on Unix and `cmd.exe /C` on
/// Windows.
pub fn build_shell_spawn_spec(shell_script: &str) -> SpawnSpec {
    #[cfg(unix)]
    let (shell_program_path, shell_command_flag) = (PathBuf::from("/bin/sh"), "-c");
    #[cfg(windows)]
    let (shell_program_path, shell_command_flag) = (PathBuf::from("cmd.exe"), "/C");
    SpawnSpec {
        shell_kind: ShellKind::from_program(&shell_program_path),
        program: shell_program_path,
        arguments: vec![shell_command_flag.to_string(), shell_script.to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
    }
}

/// Copy the `koshi` binary into `directory` and hand back the copy's path. A
/// test that renames its binary or changes its mode owns that file alone.
pub fn copy_koshi_binary(directory: &Path) -> PathBuf {
    let binary_path = directory.join(if cfg!(windows) { "koshi.exe" } else { "koshi" });
    std::fs::copy(env!("CARGO_BIN_EXE_koshi"), &binary_path).expect("the koshi binary is copied");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))
            .expect("the copy runs");
    }
    binary_path
}

/// The runtime directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` serves when no `--runtime-dir` names one: `run/` inside the
/// home directory.
pub fn resolve_runtime_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory.join("run")
}

/// The config directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` reads: macOS derives it from the home directory alone.
#[cfg(target_os = "macos")]
pub fn resolve_config_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory.join("Library/Application Support/koshi")
}

/// The config directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` reads: `.config/koshi` inside the home directory.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn resolve_config_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory.join(".config/koshi")
}

/// The config directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` reads: `AppData\Roaming\koshi\config` inside the home
/// directory, under the `APPDATA` that command sets.
#[cfg(windows)]
pub fn resolve_config_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory
        .join("AppData")
        .join("Roaming")
        .join("koshi")
        .join("config")
}

/// The data directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` reads: macOS derives it from the home directory alone.
#[cfg(target_os = "macos")]
pub fn resolve_data_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory.join("Library/Application Support/koshi")
}

/// The data directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` reads: `.local/share/koshi` inside the home directory,
/// under the `XDG_DATA_HOME` that command sets.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn resolve_data_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory.join(".local/share/koshi")
}

/// The data directory a `koshi` started by [`build_koshi_command_at`] with
/// `home_directory` reads: `AppData\Roaming\koshi\data` inside the home
/// directory, under the `APPDATA` that command sets.
#[cfg(windows)]
pub fn resolve_data_directory_under_home(home_directory: &Path) -> PathBuf {
    home_directory
        .join("AppData")
        .join("Roaming")
        .join("koshi")
        .join("data")
}

/// Write `config_text` as the `koshi.kdl` a process started under
/// `home_directory` reads.
pub fn write_test_config(home_directory: &Path, config_text: &str) {
    let config_directory = resolve_config_directory_under_home(home_directory);
    std::fs::create_dir_all(&config_directory).expect("a config directory under the test home");
    std::fs::write(config_directory.join("koshi.kdl"), config_text)
        .expect("the config file is written");
}

/// The `koshi` binary at `binary_path`, set to keep its files under
/// `home_directory` rather than in the developer's own directories, and stripped
/// of the pane identity so it runs as a CLI outside any session. Standard input
/// is closed, and both output streams are pipes the test reads. The runtime
/// directory the child serves is [`resolve_runtime_directory_under_home`].
///
/// `HOME` and `USERPROFILE` name `home_directory`. On Unix other than macOS,
/// `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`
/// name `.config`, `.local/share`, `.local/state` and `.xdg-runtime` inside
/// it. On Windows, `APPDATA` and `LOCALAPPDATA` name `AppData\Roaming` and
/// `AppData\Local` inside it. On every platform the config, data and state
/// directories the child resolves, and the runtime directories of koshi 0.1.0
/// and 0.2.0 it walks, all sit inside `home_directory`.
///
/// The child runs in `home_directory`, so a pane it opens starts there. On Unix
/// `SHELL` names `/bin/sh`, and `ENV`, `BASH_ENV` and `ZDOTDIR` are removed: a
/// pane runs the system shell and reads no startup file the developer's
/// environment names.
pub fn build_koshi_command_at(binary_path: &Path, home_directory: &Path) -> Command {
    let mut process_command = Command::new(binary_path);
    process_command
        .current_dir(home_directory)
        .env("HOME", home_directory)
        .env("USERPROFILE", home_directory)
        .env(
            "KOSHI_RUNTIME_DIR",
            resolve_runtime_directory_under_home(home_directory),
        )
        // The five variables the runtime sets in a pane. The child gets none
        // of them, so `InSessionContext::from_env` finds no `KOSHI` marker.
        .env_remove("KOSHI")
        .env_remove("KOSHI_SESSION_ID")
        .env_remove("KOSHI_CLIENT_ID")
        .env_remove("KOSHI_PANE_ID")
        .env_remove("KOSHI_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    process_command
        .env("SHELL", "/bin/sh")
        .env_remove("ENV")
        .env_remove("BASH_ENV")
        .env_remove("ZDOTDIR");
    #[cfg(all(unix, not(target_os = "macos")))]
    process_command
        .env("XDG_CONFIG_HOME", home_directory.join(".config"))
        .env("XDG_DATA_HOME", home_directory.join(".local/share"))
        .env("XDG_STATE_HOME", home_directory.join(".local/state"))
        .env("XDG_RUNTIME_DIR", home_directory.join(".xdg-runtime"));
    #[cfg(windows)]
    process_command
        .env("APPDATA", home_directory.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home_directory.join("AppData").join("Local"));
    process_command
}

/// [`build_koshi_command_at`], run from the binary this build produced.
pub fn build_koshi_command_under_home(home_directory: &Path) -> Command {
    build_koshi_command_at(Path::new(env!("CARGO_BIN_EXE_koshi")), home_directory)
}

/// Start one session's server under `home_directory`, serving
/// `runtime_directory`, under the identity the router would have handed it:
/// `serve-session <session_id> workspace`. Standard output is closed; the error
/// stream stays a pipe.
pub fn start_session_server_under_home(
    home_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> SessionProcess {
    SessionProcess {
        child_process: start_program_process(
            build_koshi_command_under_home(home_directory)
                .arg("serve-session")
                .arg(session_id.to_string())
                .arg(SESSION_SERVER_NAME)
                .arg("--runtime-dir")
                .arg(runtime_directory)
                .stdout(Stdio::null()),
        ),
    }
}

/// Start the binary at `binary_path` as the router serving `runtime_directory`,
/// under `home_directory` as [`build_koshi_command_at`] sets it. Every session
/// server the router starts inherits that home. Both output streams are closed.
pub fn start_router_from_binary(
    binary_path: &Path,
    home_directory: &Path,
    runtime_directory: &Path,
) -> RunningRouter {
    let child_process = start_program_process(
        build_koshi_command_at(binary_path, home_directory)
            .arg("serve-router")
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    RunningRouter { child_process }
}

/// [`start_router_from_binary`], run from the binary this build produced.
pub fn start_router_process(home_directory: &Path, runtime_directory: &Path) -> RunningRouter {
    start_router_from_binary(
        Path::new(env!("CARGO_BIN_EXE_koshi")),
        home_directory,
        runtime_directory,
    )
}

/// Open a connection to the router serving `runtime_directory`, with its
/// handshake already done, retrying until one answers.
pub fn connect_to_router(runtime_directory: &Path) -> Connection {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(connection) = try_connect_to_router(runtime_directory) {
            return connection;
        }
        assert!(
            Instant::now() < wait_deadline,
            "no router answered in {}",
            runtime_directory.display()
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a router connection: read the endpoint file,
/// connect, and send the Hello that opens the connection.
///
/// `None` means no router answered yet. A router that has just replaced
/// another writes its own endpoint file a moment after it binds, so a Hello
/// carrying the older file's token is refused; the next attempt reads the new
/// file.
fn try_connect_to_router(runtime_directory: &Path) -> Option<Connection> {
    let router_endpoint_file =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)).ok()?;
    let mut connection = Connection::connect(&router_endpoint_file.socket_address).ok()?;
    let hello_request = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(router_endpoint_file.connection_token),
    };
    connection.send(&hello_request).ok()?;
    let router_response: RouterResponse = connection.recv().ok()?;
    match router_response.answer_result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Ask the router for `request_kind` on an open connection, as request `2`, and
/// hand back its answer.
pub fn send_router_request(
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

/// Ask the router for a new session and hand back where it listens.
pub fn create_session(connection: &mut Connection) -> SessionAddress {
    match send_router_request(
        connection,
        RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        },
    ) {
        RouterResult::Created(session_address) => session_address,
        unexpected_result => panic!("creating a session was answered with {unexpected_result:?}"),
    }
}

/// Send the router an `AttachLookup` for the session `session_selector` names,
/// and hand back the answer.
pub fn send_attach_lookup(
    connection: &mut Connection,
    session_selector: &SessionSelector,
) -> RouterResult {
    send_router_request(
        connection,
        RouterRequestKind::AttachLookup {
            session_selector: session_selector.clone(),
        },
    )
}

/// Send [`send_attach_lookup`] for `session_selector` until the answer is an
/// error, and hand back that error. Fails the test when every answer within
/// [`WAIT_DURATION`] names a session.
pub fn wait_for_session_lookup_refusal(
    connection: &mut Connection,
    session_selector: &SessionSelector,
) -> RouterResult {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let lookup_result = send_attach_lookup(connection, session_selector);
        if matches!(lookup_result, RouterResult::Error(_)) {
            return lookup_result;
        }
        assert!(
            Instant::now() < wait_deadline,
            "the router kept finding {session_selector:?}"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// The refusal the router answers a lookup of `session_id` with when it holds
/// no such session.
pub fn build_no_such_session_result(session_id: SessionId) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::NotFound,
        message: format!("no session {session_id} is running"),
    })
}
