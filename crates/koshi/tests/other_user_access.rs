//! Who besides the user who started a session may reach it.
//!
//! Every test starts the real `koshi` binary as one session's server —
//! `serve-session` with `--runtime-dir` — and then reaches that session the way
//! a user would. Each side gets its own temporary home directory holding the
//! `koshi.kdl` that side reads, so the sessions here never meet the one a
//! developer is running. The directories sit under a short base because a Unix
//! socket path has an operating-system length cap.
//!
//! `allow-other-users` in `koshi.kdl` is the switch. Off, the session's socket
//! stays inside the private runtime directory, which carries mode `0700`, so no
//! other local user can reach it. On, the socket moves into this user's
//! directory under `shared-sessions-dir`, which every local user may enter, and
//! the sessions there answer every local user's `koshi list-sessions`.
//!
//! Reaching a session as a second user needs a second user id, and only root
//! can take one on. Those tests print why they were skipped and return when
//! this process is not root; every other test runs everywhere.
//!
//! Windows takes the same switch through a different shape: a named pipe has no
//! filesystem location, so an empty marker file in the shared directory is what
//! names a session other local users may reach, and `%ProgramData%` is where
//! that directory sits. A second Windows account cannot be made from a test, so
//! the Windows tests cover the marker, a connection from this user, and the
//! discovery walk over the shared directory. They reach the runtime directory
//! and the shared directory through explicit paths, because Windows resolves
//! the config directory through a Win32 call no environment variable redirects.

use std::io::Read;
use std::path::Path;
#[cfg(any(unix, windows))]
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use koshi_core::command::CliExitCode;
#[cfg(windows)]
use koshi_core::command::{Command, CommandEnvelope, CommandSource};
#[cfg(windows)]
use koshi_core::ids::CommandId;
use koshi_core::ids::SessionId;
#[cfg(windows)]
use koshi_ipc::endpoint::advert_path;
use koshi_ipc::endpoint::EndpointFile;
#[cfg(windows)]
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult, PROTOCOL_VERSION};
#[cfg(windows)]
use koshi_ipc::transport::Connection;
use tempfile::TempDir;

mod common;

#[cfg(unix)]
use common::copy_of_koshi;
use common::start_koshi;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(100);

/// The display name the session server is started under, standing in for the
/// one the router generates.
const SESSION_NAME: &str = "workspace";

/// A session server the test started. Dropping it ends that server, so a
/// failed assertion leaves nothing running.
struct RunningSession(Child);

impl RunningSession {
    /// Whether the server is still up, and its exit status plus what it wrote
    /// to its error stream once it is not — for a failure message.
    fn state(&mut self) -> String {
        let Some(status) = self
            .0
            .try_wait()
            .expect("the session server's state can be read")
        else {
            return "it is still running".to_string();
        };
        let mut stderr = String::new();
        if let Some(pipe) = self.0.stderr.as_mut() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        format!("it exited {status}: {}", stderr.trim())
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait for the endpoint file `session`'s server writes once its socket is
/// bound, and hand it back. Fails the test once [`WAIT`] has passed with
/// nothing advertised, naming why the server is gone when it is.
fn wait_for_endpoint(
    session: &mut RunningSession,
    runtime_dir: &Path,
    session_id: SessionId,
) -> EndpointFile {
    let path = EndpointFile::path(runtime_dir, session_id);
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(endpoint) = EndpointFile::read(&path) {
            return endpoint;
        }
        assert!(
            Instant::now() < deadline,
            "no session server advertised {session_id}; {}",
            session.state()
        );
        std::thread::sleep(POLL);
    }
}

/// Wait for `session`'s process to exit, and hand back whether it did inside
/// [`WAIT`].
fn waited_for_exit(session: &mut RunningSession) -> bool {
    let deadline = Instant::now() + WAIT;
    loop {
        if session
            .0
            .try_wait()
            .expect("the session server's state can be read")
            .is_some()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

// --- Unix ---

/// A fresh home directory for the `koshi` processes a test starts to derive
/// their runtime and config directories from. Removed when the test drops it.
///
/// The name is one letter and six random characters, which leaves the paths
/// built under it inside the 104-byte cap a Unix socket address has.
#[cfg(unix)]
fn test_home() -> TempDir {
    tempfile::Builder::new()
        .prefix("k")
        .tempdir_in("/tmp")
        .expect("a temporary home directory")
}

/// A fresh directory to stand in for the machine-wide shared directory, under a
/// short base for the same length cap. The session server creates this user's
/// directory inside it and binds the socket there.
#[cfg(unix)]
fn test_shared_base() -> TempDir {
    TempDir::new_in("/tmp").expect("a temporary shared session directory")
}

/// The runtime directory a `koshi` started by [`koshi_under`] with `home`
/// serves: macOS derives it from the home directory alone.
#[cfg(target_os = "macos")]
fn runtime_dir_under(home: &Path) -> PathBuf {
    home.join("Library/Application Support/koshi/run")
}

/// The runtime directory a `koshi` started by [`koshi_under`] with `home`
/// serves: `koshi/` inside `XDG_RUNTIME_DIR`, which [`koshi_under`] points at
/// `home`.
#[cfg(all(unix, not(target_os = "macos")))]
fn runtime_dir_under(home: &Path) -> PathBuf {
    home.join("koshi")
}

/// The config directory a `koshi` started by [`koshi_under`] with `home`
/// reads: macOS derives it from the home directory alone.
#[cfg(target_os = "macos")]
fn config_dir_under(home: &Path) -> PathBuf {
    home.join("Library/Application Support/koshi")
}

/// The config directory a `koshi` started by [`koshi_under`] with `home`
/// reads: `.config/koshi` inside the home directory.
#[cfg(all(unix, not(target_os = "macos")))]
fn config_dir_under(home: &Path) -> PathBuf {
    home.join(".config/koshi")
}

/// Write `body` as the `koshi.kdl` a process started under `home` reads.
#[cfg(unix)]
fn write_config(home: &Path, body: &str) {
    let config = config_dir_under(home);
    std::fs::create_dir_all(&config).expect("a config directory under the test home");
    std::fs::write(config.join("koshi.kdl"), body).expect("the config file is written");
}

/// A `koshi.kdl` with the switch on, sharing sessions through `shared_base`.
#[cfg(unix)]
fn switched_on_config(shared_base: &Path) -> String {
    format!(
        "version 1\nallow-other-users #true\nshared-sessions-dir \"{}\"\n",
        shared_base.display()
    )
}

/// Let every local user reach the `koshi.kdl` written under `home`: every
/// directory from the config directory up to `home` opens to `0755`, and the
/// file itself to `0644`. The second user's `koshi` has to read that file to
/// learn the switch is on.
#[cfg(unix)]
fn let_every_user_read_the_config(home: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let config = config_dir_under(home);
    let mut dir = config.as_path();
    loop {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("opening {}: {error}", dir.display()));
        if dir == home {
            break;
        }
        dir = dir
            .parent()
            .expect("the config directory sits under the test home");
    }
    let file = config.join("koshi.kdl");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
        .expect("the config file opens to every local user");
}

/// The permission bits of `path`, without the file-type bits.
#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// This process's effective user id.
#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: `geteuid` reads this process's own identity, takes no argument,
    // and cannot fail.
    unsafe { libc::geteuid() }
}

/// The user id and group id of the `nobody` account, or `None` when this
/// machine has no such account.
#[cfg(unix)]
fn nobody_ids() -> Option<(u32, u32)> {
    let name = std::ffi::CString::new("nobody").expect("the name holds no zero byte");
    // SAFETY: the pointer passed in is a valid C string that outlives the call.
    // `getpwnam` hands back either null or a pointer into its own storage,
    // which stays valid until the next call from this thread.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is non-null, so it points at a `passwd` `getpwnam` filled.
    Some(unsafe { ((*entry).pw_uid, (*entry).pw_gid) })
}

/// A copy of the `koshi` binary at a path every local user may run it from,
/// for the tests that exec it as a second user: the build directory may sit
/// behind directories those users cannot enter. Dropping the handle removes
/// the copy.
#[cfg(unix)]
fn koshi_every_user_can_run() -> (TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let dir = test_home();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("the directory holding the copy opens to every local user");
    let copy = copy_of_koshi(dir.path());
    (dir, copy)
}

/// The `koshi` binary at `binary`, set to keep its files under `home` rather
/// than in the developer's own directories, and stripped of the pane identity
/// so it runs as a CLI outside any session. Standard input is closed, and both
/// output streams are pipes the test reads.
#[cfg(unix)]
fn koshi_at(binary: &Path, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(binary);
    command
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home)
        // The five variables the runtime injects at pane spawn; `KOSHI` is the
        // marker `InSessionContext::from_env` reads, and a test run from inside
        // a koshi pane would hand every one of them to this child.
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
    command.env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

/// [`koshi_at`], run from the binary this build produced.
#[cfg(unix)]
fn koshi_under(home: &Path) -> std::process::Command {
    koshi_at(Path::new(env!("CARGO_BIN_EXE_koshi")), home)
}

/// [`koshi_at`], run as the user `uid` and the group `gid` instead of this
/// one. The group is set first, which is the only order that works once the
/// user id has been given up.
#[cfg(unix)]
fn koshi_under_as(binary: &Path, home: &Path, uid: u32, gid: u32) -> std::process::Command {
    use std::os::unix::process::CommandExt;

    let mut command = koshi_at(binary, home);
    command.gid(gid).uid(uid);
    command
}

/// Run `command` to its end and hand back its exit status and both output
/// streams. Starting goes through [`start_koshi`], which waits out a program
/// file the operating system reports as busy.
#[cfg(unix)]
fn koshi_output(command: &mut std::process::Command) -> std::process::Output {
    start_koshi(command)
        .wait_with_output()
        .expect("the koshi binary runs to its end")
}

/// Start one session's server under `home`, so it reads the `koshi.kdl` written
/// there rather than the developer's own.
#[cfg(unix)]
fn start_session_server_under(
    home: &Path,
    runtime_dir: &Path,
    session_id: SessionId,
) -> RunningSession {
    let child = start_koshi(
        koshi_under(home)
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_NAME)
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .stdout(Stdio::null()),
    );
    RunningSession(child)
}

/// The exact `koshi list-sessions` table for one session: the header row, then
/// that session's id and name. Each column is padded to its widest cell and
/// separated by two spaces, with no trailing spaces.
#[cfg(unix)]
fn one_session_listing(session_id: SessionId) -> String {
    let id = session_id.to_string();
    format!(
        "{:width$}  name\n{id}  {SESSION_NAME}\n",
        "id",
        width = id.len()
    )
}

/// A session another local user started shows up in this user's listing and
/// takes this user's kill, while `allow-other-users` is on for both of them.
#[cfg(unix)]
#[test]
fn another_local_user_lists_and_kills_a_session_while_the_switch_is_on() {
    if euid() != 0 {
        eprintln!(
            "skipped `another_local_user_lists_and_kills_a_session_while_the_switch_is_on`: \
             running a second user id needs root; re-run under sudo"
        );
        return;
    }
    let Some((uid, gid)) = nobody_ids() else {
        eprintln!(
            "skipped `another_local_user_lists_and_kills_a_session_while_the_switch_is_on`: \
             this machine has no `nobody` account"
        );
        return;
    };

    let shared_base = test_shared_base();
    let config = switched_on_config(shared_base.path());

    let owner_home = test_home();
    write_config(owner_home.path(), &config);
    let owner_runtime = runtime_dir_under(owner_home.path());
    std::fs::create_dir_all(&owner_runtime).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session = start_session_server_under(owner_home.path(), &owner_runtime, session_id);
    let endpoint = wait_for_endpoint(&mut session, &owner_runtime, session_id);

    // The switch moved the socket out of the private runtime directory, so the
    // second user below really walks the shared one.
    let owner_shared_dir = shared_base.path().join(euid().to_string());
    assert_eq!(
        Path::new(&endpoint.socket).parent(),
        Some(owner_shared_dir.as_path())
    );

    let other_home = test_home();
    write_config(other_home.path(), &config);
    let_every_user_read_the_config(other_home.path());
    let (_koshi_dir, koshi) = koshi_every_user_can_run();

    let listed =
        koshi_output(koshi_under_as(&koshi, other_home.path(), uid, gid).arg("list-sessions"));
    assert_eq!(String::from_utf8_lossy(&listed.stderr), "");
    assert_eq!(listed.status.code(), Some(CliExitCode::Success.code()));
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout),
        one_session_listing(session_id)
    );

    let killed = koshi_output(
        koshi_under_as(&koshi, other_home.path(), uid, gid)
            .arg("kill-session")
            .arg(session_id.to_string()),
    );
    // The success reply races the socket shutdown, so the server process
    // ending is what says the kill landed, not the exit code.
    assert!(
        waited_for_exit(&mut session),
        "the session server outlived the other user's kill; it exited {} saying {}",
        killed.status,
        String::from_utf8_lossy(&killed.stderr).trim()
    );
}

/// The default posture: another local user neither sees the session nor reaches
/// it, and the session keeps running.
#[cfg(unix)]
#[test]
fn another_local_user_finds_nothing_while_the_switch_is_off() {
    if euid() != 0 {
        eprintln!(
            "skipped `another_local_user_finds_nothing_while_the_switch_is_off`: \
             running a second user id needs root; re-run under sudo"
        );
        return;
    }
    let Some((uid, gid)) = nobody_ids() else {
        eprintln!(
            "skipped `another_local_user_finds_nothing_while_the_switch_is_off`: \
             this machine has no `nobody` account"
        );
        return;
    };

    let owner_home = test_home();
    write_config(owner_home.path(), "version 1\n");
    let owner_runtime = runtime_dir_under(owner_home.path());
    std::fs::create_dir_all(&owner_runtime).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session = start_session_server_under(owner_home.path(), &owner_runtime, session_id);
    wait_for_endpoint(&mut session, &owner_runtime, session_id);

    let other_home = test_home();
    write_config(other_home.path(), "version 1\n");
    let_every_user_read_the_config(other_home.path());
    let (_koshi_dir, koshi) = koshi_every_user_can_run();

    let listed =
        koshi_output(koshi_under_as(&koshi, other_home.path(), uid, gid).arg("list-sessions"));
    assert_eq!(String::from_utf8_lossy(&listed.stderr), "");
    assert_eq!(listed.status.code(), Some(CliExitCode::Success.code()));
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "id  name\n");

    let killed = koshi_output(
        koshi_under_as(&koshi, other_home.path(), uid, gid)
            .arg("kill-session")
            .arg(session_id.to_string()),
    );
    assert_eq!(
        killed.status.code(),
        Some(CliExitCode::SessionNotFound.code())
    );
    assert_eq!(
        String::from_utf8_lossy(&killed.stderr),
        format!("koshi: session {session_id} is not running\n")
    );

    assert_eq!(
        session
            .0
            .try_wait()
            .expect("the session server's state can be read"),
        None,
        "the session server ended without being asked to"
    );
}

/// Turning the switch on leaves the single-user flow alone: the session this
/// user started is listed once, not asked twice and not counted as unanswered.
/// The listing walks the shared directory, where this user's own session sits.
#[cfg(unix)]
#[test]
fn this_users_own_session_is_listed_once_while_the_switch_is_on() {
    let shared_base = test_shared_base();
    let home = test_home();
    write_config(home.path(), &switched_on_config(shared_base.path()));
    let runtime_dir = runtime_dir_under(home.path());
    std::fs::create_dir_all(&runtime_dir).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session = start_session_server_under(home.path(), &runtime_dir, session_id);
    let endpoint = wait_for_endpoint(&mut session, &runtime_dir, session_id);

    // The switch moved the socket into this user's directory under the shared
    // one, so the listing below really walks that directory.
    let own_shared_dir = shared_base.path().join(euid().to_string());
    assert_eq!(
        Path::new(&endpoint.socket).parent(),
        Some(own_shared_dir.as_path())
    );
    // The token file did not move with the socket and did not widen with it.
    let endpoint_path = EndpointFile::path(&runtime_dir, session_id);
    assert_eq!(endpoint_path.parent(), Some(runtime_dir.as_path()));
    assert_eq!(mode_of(&endpoint_path), 0o600);

    let listed = koshi_output(koshi_under(home.path()).arg("list-sessions"));
    assert_eq!(String::from_utf8_lossy(&listed.stderr), "");
    assert_eq!(listed.status.code(), Some(CliExitCode::Success.code()));
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout),
        one_session_listing(session_id)
    );
}

/// A fresh install: the socket sits inside the private runtime directory, that
/// directory carries mode `0700`, and the token file beside it carries `0600`.
#[cfg(unix)]
#[test]
fn a_session_with_the_switch_off_keeps_its_socket_in_the_private_runtime_directory() {
    let home = test_home();
    write_config(home.path(), "version 1\n");
    let runtime_dir = runtime_dir_under(home.path());
    std::fs::create_dir_all(&runtime_dir).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session = start_session_server_under(home.path(), &runtime_dir, session_id);
    let endpoint = wait_for_endpoint(&mut session, &runtime_dir, session_id);

    assert_eq!(
        Path::new(&endpoint.socket).parent(),
        Some(runtime_dir.as_path())
    );
    assert_eq!(mode_of(&runtime_dir), 0o700);
    assert_eq!(
        mode_of(&EndpointFile::path(&runtime_dir, session_id)),
        0o600
    );
}

/// No koshi component gains privileges of its own: the shipped binary carries
/// neither the set-user-id nor the set-group-id bit.
#[cfg(unix)]
#[test]
fn the_koshi_binary_carries_no_setuid_or_setgid_bit() {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(env!("CARGO_BIN_EXE_koshi"))
        .expect("the koshi binary is built")
        .permissions()
        .mode();
    assert_eq!(mode & 0o6000, 0);
}

// --- Windows ---

/// A fresh runtime directory to serve, under the temporary base.
#[cfg(windows)]
fn test_runtime_dir() -> TempDir {
    TempDir::new_in(std::env::temp_dir()).expect("a temporary runtime directory")
}

/// A fresh directory to stand in for `%ProgramData%`. The session server puts
/// `koshi` inside it and advertises there.
#[cfg(windows)]
fn test_program_data() -> TempDir {
    TempDir::new_in(std::env::temp_dir()).expect("a temporary program data directory")
}

/// The machine-wide shared directory a server started with `program_data`
/// advertises in.
#[cfg(windows)]
fn shared_dir_under(program_data: &Path) -> PathBuf {
    program_data.join("koshi")
}

/// Start one session's server serving `runtime_dir`, with the switch forced on
/// and `%ProgramData%` pointed at `program_data`.
#[cfg(windows)]
fn start_shared_session_server(
    runtime_dir: &Path,
    program_data: &Path,
    session_id: SessionId,
) -> RunningSession {
    let child = start_koshi(
        std::process::Command::new(env!("CARGO_BIN_EXE_koshi"))
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_NAME)
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .arg("--allow-other-users")
            .env("ProgramData", program_data)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    RunningSession(child)
}

/// Wait for the marker naming `session_id` among the sessions other local users
/// may reach, and hand back its path. The server writes it after the endpoint
/// file. Fails the test once [`WAIT`] has passed with no marker.
#[cfg(windows)]
fn wait_for_marker(shared_dir: &Path, session_id: SessionId) -> PathBuf {
    let path = advert_path(shared_dir, session_id);
    let deadline = Instant::now() + WAIT;
    loop {
        if path.exists() {
            return path;
        }
        assert!(
            Instant::now() < deadline,
            "no session server advertised {session_id} in the shared directory"
        );
        std::thread::sleep(POLL);
    }
}

/// Open a connection to the session at `endpoint` and complete the Hello,
/// presenting the token the endpoint file carries.
#[cfg(windows)]
fn open(endpoint: &EndpointFile) -> Connection {
    let mut connection =
        Connection::connect(&endpoint.socket).expect("the session's pipe answers a connect");
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::hello(endpoint.token.clone()),
    };
    connection.send(&hello).expect("the server reads the Hello");
    let reply: IpcResponse = connection.recv().expect("the server answers the Hello");
    assert_eq!(reply.request_id, Some(1));
    assert_eq!(
        reply.result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
    connection
}

/// The marker naming a session other local users may reach lives for as long as
/// the session serves, and goes when the session quits.
#[cfg(windows)]
#[test]
fn the_shared_marker_names_the_session_while_it_serves_and_goes_when_it_quits() {
    let runtime_dir = test_runtime_dir();
    let program_data = test_program_data();
    let shared_dir = shared_dir_under(program_data.path());
    let session_id = SessionId::new();
    let mut session =
        start_shared_session_server(runtime_dir.path(), program_data.path(), session_id);
    let endpoint = wait_for_endpoint(&mut session, runtime_dir.path(), session_id);
    let marker = wait_for_marker(&shared_dir, session_id);

    let mut connection = open(&endpoint);
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::external_cli(Some(session_id)),
        std::time::SystemTime::now(),
        Command::Quit,
    );
    connection
        .send(&IpcRequest {
            request_id: 2,
            kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("the server reads the quit");

    assert!(
        waited_for_exit(&mut session),
        "the session server outlived the quit"
    );
    assert!(!marker.exists(), "the marker outlived the session");
}

/// The user who started the session still reaches it once the switch is on: the
/// same pipe, the same token, the same Hello.
#[cfg(windows)]
#[test]
fn a_client_of_this_user_completes_the_hello_on_a_shared_session() {
    let runtime_dir = test_runtime_dir();
    let program_data = test_program_data();
    let session_id = SessionId::new();
    let mut session =
        start_shared_session_server(runtime_dir.path(), program_data.path(), session_id);
    let endpoint = wait_for_endpoint(&mut session, runtime_dir.path(), session_id);
    wait_for_marker(&shared_dir_under(program_data.path()), session_id);

    // The Hello is checked inside `open`, which fails the test on any other
    // answer.
    let _connection = open(&endpoint);
}

/// Turning the switch on leaves the single-user flow alone: the walk over the
/// shared directory passes over this user's own session, which the endpoint
/// file already names, so the session is found once and nothing is counted as
/// unanswered.
#[cfg(windows)]
#[test]
fn this_users_own_session_is_found_once_over_the_shared_directory() {
    let runtime_dir = test_runtime_dir();
    let program_data = test_program_data();
    let shared_dir = shared_dir_under(program_data.path());
    let session_id = SessionId::new();
    let mut session =
        start_shared_session_server(runtime_dir.path(), program_data.path(), session_id);
    wait_for_endpoint(&mut session, runtime_dir.path(), session_id);
    wait_for_marker(&shared_dir, session_id);

    // The marker is in the shared directory, and the walk over it hands back
    // nothing: this user's own session is never asked for the empty token it
    // would refuse.
    assert_eq!(
        koshi::ipc_client::foreign_sessions(&shared_dir, runtime_dir.path()),
        Vec::new()
    );

    let found = koshi::discovery::fetch_all(runtime_dir.path());
    assert_eq!(found.unasked, 0);
    let listed: Vec<SessionId> = found
        .sessions
        .iter()
        .map(|overview| overview.session.id)
        .filter(|id| *id == session_id)
        .collect();
    assert_eq!(listed, vec![session_id]);
}
