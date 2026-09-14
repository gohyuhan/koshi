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
use koshi_ipc::endpoint::resolve_advertisement_marker_path;
use koshi_ipc::endpoint::EndpointFile;
#[cfg(windows)]
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult, PROTOCOL_VERSION};
#[cfg(windows)]
use koshi_ipc::transport::Connection;
use tempfile::TempDir;

mod common;

#[cfg(unix)]
use common::copy_koshi_binary;
#[cfg(any(unix, windows))]
use common::start_koshi_process;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll pauses between attempts.
const ATTACH_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// The display name the session server is started under, standing in for the
/// one the router generates.
const SESSION_SERVER_NAME: &str = "workspace";

/// A session server the test started. Dropping it ends that server, so a
/// failed assertion leaves nothing running.
struct RunningSession {
    child_process: Child,
}

impl RunningSession {
    /// Whether the server is still up, and its exit status plus what it wrote
    /// to its error stream once it is not — for a failure message.
    fn format_process_status(&mut self) -> String {
        let Some(exit_status) = self
            .child_process
            .try_wait()
            .expect("the session server's state can be read")
        else {
            return "it is still running".to_string();
        };
        let mut stderr = String::new();
        if let Some(stderr_pipe) = self.child_process.stderr.as_mut() {
            let _ = stderr_pipe.read_to_string(&mut stderr);
        }
        format!("it exited {exit_status}: {}", stderr.trim())
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// Wait for the endpoint file the session server writes once its socket is
/// bound, and hand it back. Fails the test once [`WAIT_DURATION`] has passed with
/// nothing advertised, naming why the server is gone when it is.
fn wait_for_session_endpoint(
    session_process: &mut RunningSession,
    runtime_directory: &Path,
    session_id: SessionId,
) -> EndpointFile {
    let endpoint_file_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(endpoint) = EndpointFile::load_from_path(&endpoint_file_path) {
            return endpoint;
        }
        assert!(
            Instant::now() < deadline,
            "no session server advertised {session_id}; {}",
            session_process.format_process_status()
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
}

/// Wait for `session_process`'s process to exit, and hand back whether it did inside
/// [`WAIT_DURATION`].
fn wait_for_session_server_exit(session_process: &mut RunningSession) -> bool {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if session_process
            .child_process
            .try_wait()
            .expect("the session server's state can be read")
            .is_some()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
}

// --- Unix ---

/// A fresh home directory for the `koshi` processes a test starts to derive
/// their runtime and config directories from. Removed when the test drops it.
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

/// A fresh directory to stand in for the machine-wide shared directory, under a
/// short base for the same length cap. The session server creates this user's
/// directory inside it and binds the socket there.
#[cfg(unix)]
fn build_test_shared_directory_base() -> TempDir {
    TempDir::new_in("/tmp").expect("a temporary shared session directory")
}

/// The runtime directory a `koshi` started by [`build_koshi_command_under_home`] with `home`
/// serves: `run/` inside the home directory.
#[cfg(unix)]
fn resolve_runtime_directory_under_home(home: &Path) -> PathBuf {
    home.join("run")
}

/// The config directory a `koshi` started by [`build_koshi_command_under_home`] with `home`
/// reads: macOS derives it from the home directory alone.
#[cfg(target_os = "macos")]
fn resolve_config_directory_under_home(home: &Path) -> PathBuf {
    home.join("Library/Application Support/koshi")
}

/// The config directory a `koshi` started by [`build_koshi_command_under_home`] with `home`
/// reads: `.config/koshi` inside the home directory.
#[cfg(all(unix, not(target_os = "macos")))]
fn resolve_config_directory_under_home(home: &Path) -> PathBuf {
    home.join(".config/koshi")
}

/// Write `body` as the `koshi.kdl` a process started under `home` reads.
#[cfg(unix)]
fn write_test_config(home: &Path, config_text: &str) {
    let config_directory = resolve_config_directory_under_home(home);
    std::fs::create_dir_all(&config_directory).expect("a config directory under the test home");
    std::fs::write(config_directory.join("koshi.kdl"), config_text)
        .expect("the config file is written");
}

/// A `koshi.kdl` with the switch on, sharing sessions through `shared_directory_base`.
#[cfg(unix)]
fn switched_on_config(shared_directory_base: &Path) -> String {
    format!(
        "version 1\nallow-other-users #true\nshared-sessions-dir \"{}\"\n",
        shared_directory_base.display()
    )
}

/// Let every local user reach the `koshi.kdl` written under `home`: every
/// directory from the config directory up to `home` opens to `0755`, and the
/// file itself to `0644`. The second user's `koshi` has to read that file to
/// learn the switch is on.
#[cfg(unix)]
fn set_config_read_permissions_for_every_user(home: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let config_directory = resolve_config_directory_under_home(home);
    let mut config_directory_path = config_directory.as_path();
    loop {
        std::fs::set_permissions(
            config_directory_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap_or_else(|permission_error| {
            panic!(
                "opening {}: {permission_error}",
                config_directory_path.display()
            )
        });
        if config_directory_path == home {
            break;
        }
        config_directory_path = config_directory_path
            .parent()
            .expect("the config directory sits under the test home");
    }
    let config_file_path = config_directory.join("koshi.kdl");
    std::fs::set_permissions(&config_file_path, std::fs::Permissions::from_mode(0o644))
        .expect("the config file opens to every local user");
}

/// The permission bits of `path`, without the file-type bits.
#[cfg(unix)]
fn get_file_permission_mode(file_path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(file_path)
        .unwrap_or_else(|metadata_error| {
            panic!("reading {}: {metadata_error}", file_path.display())
        })
        .permissions()
        .mode()
        & 0o777
}

/// This process's effective user id.
#[cfg(unix)]
fn get_effective_user_id() -> u32 {
    // SAFETY: `geteuid` reads this process's own identity, takes no argument,
    // and cannot fail.
    unsafe { libc::geteuid() }
}

/// The user id and group id of the `nobody` account, or `None` when this
/// machine has no such account.
#[cfg(unix)]
fn get_nobody_user_and_group_ids() -> Option<(u32, u32)> {
    let account_name =
        std::ffi::CString::new("nobody").expect("the account name holds no zero byte");
    // SAFETY: the pointer passed in is a valid C string that outlives the call.
    // `getpwnam` hands back either null or a pointer into its own storage,
    // which stays valid until the next call from this thread.
    let passwd_record = unsafe { libc::getpwnam(account_name.as_ptr()) };
    if passwd_record.is_null() {
        return None;
    }
    // SAFETY: `passwd_record` is non-null, so it points at a `passwd` record
    // filled by `getpwnam`.
    Some(unsafe { ((*passwd_record).pw_uid, (*passwd_record).pw_gid) })
}

/// A copy of the `koshi` binary at a path every local user may run it from,
/// for the tests that exec it as a second user: the build directory may sit
/// behind directories those users cannot enter. Dropping the handle removes
/// the copy.
#[cfg(unix)]
fn build_koshi_binary_for_every_user() -> (TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let test_home_directory = build_test_home_directory();
    std::fs::set_permissions(
        test_home_directory.path(),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("the directory holding the copy opens to every local user");
    let koshi_binary_path = copy_koshi_binary(test_home_directory.path());
    (test_home_directory, koshi_binary_path)
}

/// The `koshi` binary at `binary`, set to keep its files under `home` rather
/// than in the developer's own directories, and stripped of the pane identity
/// so it runs as a CLI outside any session. Standard input is closed, and both
/// output streams are pipes the test reads. The runtime directory the child
/// serves is `<home>/run`.
#[cfg(unix)]
fn build_koshi_command_at(binary: &Path, home: &Path) -> std::process::Command {
    let mut process_command = std::process::Command::new(binary);
    process_command
        .env("HOME", home)
        .env("KOSHI_RUNTIME_DIR", home.join("run"))
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
    process_command.env("XDG_CONFIG_HOME", home.join(".config"));
    process_command
}

/// [`build_koshi_command_at`], run from the binary this build produced.
#[cfg(unix)]
fn build_koshi_command_under_home(home: &Path) -> std::process::Command {
    build_koshi_command_at(Path::new(env!("CARGO_BIN_EXE_koshi")), home)
}

/// [`build_koshi_command_at`], run as the user `user_id` and the group `group_id` instead of this
/// one. The group is set first, which is the only order that works once the
/// user id has been given up.
#[cfg(unix)]
fn build_koshi_command_as_user(
    binary: &Path,
    home: &Path,
    user_id: u32,
    group_id: u32,
) -> std::process::Command {
    use std::os::unix::process::CommandExt;

    let mut process_command = build_koshi_command_at(binary, home);
    process_command.gid(group_id).uid(user_id);
    process_command
}

/// Run `process_command` to its end and hand back its exit status and both output
/// streams. Starting goes through [`start_koshi_process`], which waits out a program
/// file the operating system reports as busy.
#[cfg(unix)]
fn run_koshi_command(process_command: &mut std::process::Command) -> std::process::Output {
    start_koshi_process(process_command)
        .wait_with_output()
        .expect("the koshi binary runs to its end")
}

/// Start one session's server under `home`, so it reads the `koshi.kdl` written
/// there rather than the developer's own.
#[cfg(unix)]
fn start_session_server_under(
    home: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> RunningSession {
    let child_process = start_koshi_process(
        build_koshi_command_under_home(home)
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_SERVER_NAME)
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .stdout(Stdio::null()),
    );
    RunningSession { child_process }
}

/// The exact `koshi list-sessions` table for one session: the header row, then
/// that session's id and name. Each column is padded to its widest cell and
/// separated by two spaces, with no trailing spaces.
#[cfg(unix)]
fn render_single_session_listing(session_id: SessionId) -> String {
    let session_id_text = session_id.to_string();
    let session_name_column_width = SESSION_SERVER_NAME.len().max("name".len());
    format!(
        "{:session_id_column_width$}  {:session_name_column_width$}  server\n{session_id_text}  {SESSION_SERVER_NAME:session_name_column_width$}  local\n",
        "id",
        "name",
        session_id_column_width = session_id_text.len(),
    )
}

/// The answer `koshi server-version --format json` gives when the only server
/// this user reaches is one session another user started: no router of this
/// user's own, and that session naming the build it runs.
#[cfg(unix)]
fn render_single_server_version_answer(session_id: SessionId, build_version: &str) -> String {
    format!(
        "[\n  {{\n    \"kind\": \"router\",\n    \"session\": null,\n    \
         \"state\": \"not_running\"\n  }},\n  {{\n    \"kind\": \"session\",\n    \
         \"session\": \"{}\",\n    \"state\": \"running\",\n    \"version\": \"{build_version}\"\n  }}\n]\n",
        session_id.get_uuid()
    )
}

/// A session another local user started shows up in this user's listing and
/// takes this user's kill, while `allow-other-users` is on for both of them.
#[cfg(unix)]
#[test]
fn another_local_user_lists_and_kills_a_session_while_the_switch_is_on() {
    if get_effective_user_id() != 0 {
        eprintln!(
            "skipped `another_local_user_lists_and_kills_a_session_while_the_switch_is_on`: \
             running a second user id needs root; re-run under sudo"
        );
        return;
    }
    let Some((user_id, group_id)) = get_nobody_user_and_group_ids() else {
        eprintln!(
            "skipped `another_local_user_lists_and_kills_a_session_while_the_switch_is_on`: \
             this machine has no `nobody` account"
        );
        return;
    };

    let shared_directory_base = build_test_shared_directory_base();
    let config_text = switched_on_config(shared_directory_base.path());

    let owner_home = build_test_home_directory();
    write_test_config(owner_home.path(), &config_text);
    let owner_runtime_directory = resolve_runtime_directory_under_home(owner_home.path());
    std::fs::create_dir_all(&owner_runtime_directory)
        .expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(owner_home.path(), &owner_runtime_directory, session_id);
    let endpoint_file =
        wait_for_session_endpoint(&mut session_process, &owner_runtime_directory, session_id);

    // The switch moved the socket out of the private runtime directory, so the
    // second user below really walks the shared one.
    let owner_shared_directory = shared_directory_base
        .path()
        .join(get_effective_user_id().to_string());
    assert_eq!(
        Path::new(&endpoint_file.socket_address).parent(),
        Some(owner_shared_directory.as_path())
    );

    let other_home = build_test_home_directory();
    write_test_config(other_home.path(), &config_text);
    set_config_read_permissions_for_every_user(other_home.path());
    let (_koshi_binary_directory, koshi_binary_path) = build_koshi_binary_for_every_user();

    let list_output = run_koshi_command(
        build_koshi_command_as_user(&koshi_binary_path, other_home.path(), user_id, group_id)
            .arg("list-sessions"),
    );
    assert_eq!(String::from_utf8_lossy(&list_output.stderr), "");
    assert_eq!(
        list_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&list_output.stdout),
        render_single_session_listing(session_id)
    );

    // The version walk reaches the same session the listing did, so a session
    // another user started names its build here too.
    let version_output = run_koshi_command(
        build_koshi_command_as_user(&koshi_binary_path, other_home.path(), user_id, group_id)
            .arg("server-version")
            .arg("--format")
            .arg("json"),
    );
    assert_eq!(String::from_utf8_lossy(&version_output.stderr), "");
    assert_eq!(
        version_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&version_output.stdout),
        render_single_server_version_answer(session_id, env!("CARGO_PKG_VERSION"))
    );

    let kill_output = run_koshi_command(
        build_koshi_command_as_user(&koshi_binary_path, other_home.path(), user_id, group_id)
            .arg("kill-session")
            .arg(session_id.to_string()),
    );
    // The success reply races the socket shutdown, so the server process
    // ending is what says the kill landed, not the exit code.
    assert!(
        wait_for_session_server_exit(&mut session_process),
        "the session server outlived the other user's kill; it exited {} saying {}",
        kill_output.status,
        String::from_utf8_lossy(&kill_output.stderr).trim()
    );
}

/// The default posture: another local user neither sees the session nor reaches
/// it, and the session keeps running.
#[cfg(unix)]
#[test]
fn another_local_user_finds_nothing_while_the_switch_is_off() {
    if get_effective_user_id() != 0 {
        eprintln!(
            "skipped `another_local_user_finds_nothing_while_the_switch_is_off`: \
             running a second user id needs root; re-run under sudo"
        );
        return;
    }
    let Some((user_id, group_id)) = get_nobody_user_and_group_ids() else {
        eprintln!(
            "skipped `another_local_user_finds_nothing_while_the_switch_is_off`: \
             this machine has no `nobody` account"
        );
        return;
    };

    let owner_home = build_test_home_directory();
    write_test_config(owner_home.path(), "version 1\n");
    let owner_runtime_directory = resolve_runtime_directory_under_home(owner_home.path());
    std::fs::create_dir_all(&owner_runtime_directory)
        .expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(owner_home.path(), &owner_runtime_directory, session_id);
    wait_for_session_endpoint(&mut session_process, &owner_runtime_directory, session_id);

    let other_home = build_test_home_directory();
    write_test_config(other_home.path(), "version 1\n");
    set_config_read_permissions_for_every_user(other_home.path());
    let (_koshi_binary_directory, koshi_binary_path) = build_koshi_binary_for_every_user();

    let list_output = run_koshi_command(
        build_koshi_command_as_user(&koshi_binary_path, other_home.path(), user_id, group_id)
            .arg("list-sessions"),
    );
    assert_eq!(String::from_utf8_lossy(&list_output.stderr), "");
    assert_eq!(
        list_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&list_output.stdout),
        "id  name  server\n"
    );

    let kill_output = run_koshi_command(
        build_koshi_command_as_user(&koshi_binary_path, other_home.path(), user_id, group_id)
            .arg("kill-session")
            .arg(session_id.to_string()),
    );
    assert_eq!(
        kill_output.status.code(),
        Some(CliExitCode::SessionNotFound.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&kill_output.stderr),
        format!("koshi: session {session_id} is not running\n")
    );

    assert_eq!(
        session_process
            .child_process
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
    let shared_directory_base = build_test_shared_directory_base();
    let home = build_test_home_directory();
    write_test_config(
        home.path(),
        &switched_on_config(shared_directory_base.path()),
    );
    let runtime_directory = resolve_runtime_directory_under_home(home.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(home.path(), &runtime_directory, session_id);
    let endpoint_file =
        wait_for_session_endpoint(&mut session_process, &runtime_directory, session_id);

    // The switch moved the socket into this user's directory under the shared
    // one, so the listing below really walks that directory.
    let own_shared_directory = shared_directory_base
        .path()
        .join(get_effective_user_id().to_string());
    assert_eq!(
        Path::new(&endpoint_file.socket_address).parent(),
        Some(own_shared_directory.as_path())
    );
    // The token file did not move with the socket and did not widen with it.
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id);
    assert_eq!(endpoint_path.parent(), Some(runtime_directory.as_path()));
    assert_eq!(get_file_permission_mode(&endpoint_path), 0o600);

    let list_output =
        run_koshi_command(build_koshi_command_under_home(home.path()).arg("list-sessions"));
    assert_eq!(String::from_utf8_lossy(&list_output.stderr), "");
    assert_eq!(
        list_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&list_output.stdout),
        render_single_session_listing(session_id)
    );

    // The version walk covers the same two places the listing does, so this
    // user's own session earns one row here and not two.
    let version_output = run_koshi_command(
        build_koshi_command_under_home(home.path())
            .arg("server-version")
            .arg("--format")
            .arg("json"),
    );
    assert_eq!(String::from_utf8_lossy(&version_output.stderr), "");
    assert_eq!(
        version_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&version_output.stdout),
        render_single_server_version_answer(session_id, env!("CARGO_PKG_VERSION"))
    );
}

/// A fresh install: the socket sits inside the private runtime directory, that
/// directory carries mode `0700`, and the token file beside it carries `0600`.
#[cfg(unix)]
#[test]
fn a_session_with_the_switch_off_keeps_its_socket_in_the_private_runtime_directory() {
    let home = build_test_home_directory();
    write_test_config(home.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under(home.path(), &runtime_directory, session_id);
    let endpoint_file =
        wait_for_session_endpoint(&mut session_process, &runtime_directory, session_id);

    assert_eq!(
        Path::new(&endpoint_file.socket_address).parent(),
        Some(runtime_directory.as_path())
    );
    assert_eq!(get_file_permission_mode(&runtime_directory), 0o700);
    assert_eq!(
        get_file_permission_mode(&EndpointFile::resolve_endpoint_file_path(
            &runtime_directory,
            session_id,
        )),
        0o600
    );
}

/// No koshi component gains privileges of its own: the shipped binary carries
/// neither the set-user-id nor the set-group-id bit.
#[cfg(unix)]
#[test]
fn the_koshi_binary_carries_no_setuid_or_setgid_bit() {
    use std::os::unix::fs::PermissionsExt;

    let binary_permission_mode = std::fs::metadata(env!("CARGO_BIN_EXE_koshi"))
        .expect("the koshi binary is built")
        .permissions()
        .mode();
    assert_eq!(binary_permission_mode & 0o6000, 0);
}

// --- Windows ---

/// A fresh runtime directory to serve, under the temporary base.
#[cfg(windows)]
fn build_windows_test_runtime_directory() -> TempDir {
    TempDir::new_in(std::env::temp_dir()).expect("a temporary runtime directory")
}

/// A fresh directory to stand in for `%ProgramData%`. The session server puts
/// `koshi` inside it and advertises there.
#[cfg(windows)]
fn build_test_program_data_directory() -> TempDir {
    TempDir::new_in(std::env::temp_dir()).expect("a temporary program data directory")
}

/// The machine-wide shared directory a server started with `program_data`
/// advertises in.
#[cfg(windows)]
fn resolve_shared_directory_under_program_data(program_data: &Path) -> PathBuf {
    program_data.join("koshi")
}

/// Start one session's server serving `runtime_directory`, with the switch forced on
/// and `%ProgramData%` pointed at `program_data`.
#[cfg(windows)]
fn start_shared_session_server(
    runtime_directory: &Path,
    program_data: &Path,
    session_id: SessionId,
) -> RunningSession {
    let child_process = start_koshi_process(
        std::process::Command::new(env!("CARGO_BIN_EXE_koshi"))
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_SERVER_NAME)
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .arg("--allow-other-users")
            .env("ProgramData", program_data)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    RunningSession { child_process }
}

/// Wait for the marker naming `session_id` among the sessions other local users
/// may reach, and hand back its path. The server writes it after the endpoint
/// file. Fails the test once [`WAIT_DURATION`] has passed with no marker.
#[cfg(windows)]
fn wait_for_session_advertisement_marker(
    shared_directory: &Path,
    session_id: SessionId,
) -> PathBuf {
    let marker_path = resolve_advertisement_marker_path(shared_directory, session_id);
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if marker_path.exists() {
            return marker_path;
        }
        assert!(
            Instant::now() < deadline,
            "no session server advertised {session_id} in the shared directory"
        );
        std::thread::sleep(ATTACH_POLL_INTERVAL_DURATION);
    }
}

/// Open a connection to the session at `endpoint_file` and complete the Hello,
/// presenting the token the endpoint file carries.
#[cfg(windows)]
fn open_windows_session_connection(endpoint_file: &EndpointFile) -> Connection {
    let mut connection = Connection::connect(&endpoint_file.socket_address)
        .expect("the session's pipe answers a connect");
    let hello = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(endpoint_file.connection_token.clone()),
    };
    connection.send(&hello).expect("the server reads the Hello");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the Hello");
    assert_eq!(ipc_response.request_id, Some(1));
    assert_eq!(
        ipc_response.answer_result,
        IpcResult::Hello {
            protocol_version: PROTOCOL_VERSION,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    );
    connection
}

/// The marker naming a session other local users may reach lives for as long as
/// the session serves, and goes when the session quits.
#[cfg(windows)]
#[test]
fn the_shared_marker_names_the_session_while_it_serves_and_goes_when_it_quits() {
    let runtime_directory = build_windows_test_runtime_directory();
    let program_data = build_test_program_data_directory();
    let shared_directory = resolve_shared_directory_under_program_data(program_data.path());
    let session_id = SessionId::new();
    let mut session_process =
        start_shared_session_server(runtime_directory.path(), program_data.path(), session_id);
    let endpoint_file =
        wait_for_session_endpoint(&mut session_process, runtime_directory.path(), session_id);
    let marker_path = wait_for_session_advertisement_marker(&shared_directory, session_id);

    let mut connection = open_windows_session_connection(&endpoint_file);
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), None),
        std::time::SystemTime::now(),
        Command::Quit,
    );
    connection
        .send(&IpcRequest {
            request_id: 2,
            request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
        })
        .expect("the server reads the quit");

    assert!(
        wait_for_session_server_exit(&mut session_process),
        "the session server outlived the quit"
    );
    assert!(!marker_path.exists(), "the marker outlived the session");
}

/// The user who started the session still reaches it once the switch is on: the
/// same pipe, the same token, the same Hello.
#[cfg(windows)]
#[test]
fn a_client_of_this_user_completes_the_hello_on_a_shared_session() {
    let runtime_directory = build_windows_test_runtime_directory();
    let program_data = build_test_program_data_directory();
    let session_id = SessionId::new();
    let mut session_process =
        start_shared_session_server(runtime_directory.path(), program_data.path(), session_id);
    let endpoint_file =
        wait_for_session_endpoint(&mut session_process, runtime_directory.path(), session_id);
    wait_for_session_advertisement_marker(
        &resolve_shared_directory_under_program_data(program_data.path()),
        session_id,
    );

    // The Hello is checked inside `open_windows_session_connection`, which fails the test on any other
    // answer.
    let _connection = open_windows_session_connection(&endpoint_file);
}

/// Turning the switch on leaves the single-user flow alone: the walk over the
/// shared directory passes over this user's own session, which the endpoint
/// file already names, so the session is found once and nothing is counted as
/// unanswered.
#[cfg(windows)]
#[test]
fn this_users_own_session_is_found_once_over_the_shared_directory() {
    let runtime_directory = build_windows_test_runtime_directory();
    let program_data = build_test_program_data_directory();
    let shared_directory = resolve_shared_directory_under_program_data(program_data.path());
    let session_id = SessionId::new();
    let mut session_process =
        start_shared_session_server(runtime_directory.path(), program_data.path(), session_id);
    wait_for_session_endpoint(&mut session_process, runtime_directory.path(), session_id);
    wait_for_session_advertisement_marker(&shared_directory, session_id);

    // The marker is in the shared directory, and the walk over it hands back
    // nothing: this user's own session is never asked for the empty token it
    // would refuse.
    assert_eq!(
        koshi_link::ipc_client::list_foreign_sessions(&shared_directory, runtime_directory.path()),
        Vec::new()
    );

    let session_discovery =
        koshi_link::discovery::fetch_all_session_overviews(runtime_directory.path());
    assert_eq!(session_discovery.unasked_session_count, 0);
    let listed_session_ids: Vec<SessionId> = session_discovery
        .sessions
        .iter()
        .map(|overview| overview.session.session_id)
        .filter(|listed_session_id| *listed_session_id == session_id)
        .collect();
    assert_eq!(listed_session_ids, vec![session_id]);
}
