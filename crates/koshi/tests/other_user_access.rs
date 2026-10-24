//! Who besides the user who started a session may reach it.
//!
//! Every test starts the real `koshi` binary as one session's server —
//! `serve-session` with `--runtime-dir` — under a temporary home directory, and
//! then reaches that session the way a user would. Each side's home holds the
//! `koshi.kdl` that side reads. The directories sit under `/tmp` on Unix and
//! under the system temporary directory on Windows.
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
//! On Windows no other user reaches a session. `koshi --headless
//! --allow-other-users` is refused, and the switch in `koshi.kdl` has no
//! effect. The Windows tests pin the refusal, a session server given the switch
//! both ways that creates no shared directory and still serves this user, and
//! `koshi list-sessions` with the switch on. `APPDATA` and `LOCALAPPDATA` point
//! inside the test home, which holds the `koshi.kdl` each command reads.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Stdio;

use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::EndpointFile;
#[cfg(windows)]
use koshi_ipc::protocol::{IpcResponse, IpcResult, ProcessImageId, PROTOCOL_VERSION};
#[cfg(windows)]
use koshi_ipc::transport::Connection;
#[cfg(unix)]
use tempfile::TempDir;

mod common;

#[cfg(windows)]
use common::session_connection::build_hello_request;
#[cfg(windows)]
use common::SessionProcess;
#[cfg(unix)]
use common::{
    build_koshi_command_at, copy_koshi_binary, resolve_config_directory_under_home,
    start_session_server_under_home,
};
use common::{
    build_koshi_command_under_home, build_short_test_directory,
    resolve_runtime_directory_under_home, write_test_config, SESSION_SERVER_NAME,
};
use koshi_test_support::fixtures::start_program_process;

/// Run `process_command` to its end and hand back its exit status and both output
/// streams. Starting goes through [`start_program_process`], which waits out a program
/// file the operating system reports as busy.
fn run_koshi_command(process_command: &mut std::process::Command) -> std::process::Output {
    start_program_process(process_command)
        .wait_with_output()
        .expect("the koshi binary runs to its end")
}

/// The exact `koshi list-sessions` table for one session on this machine: the
/// header row, then that session's id, its name, the server `local`, and the
/// owner, which is `owner_user_id` or `-` when it is `None`. Each column is
/// padded to its widest cell and separated by two spaces, with no trailing
/// spaces.
fn render_single_session_listing(session_id: SessionId, owner_user_id: Option<u32>) -> String {
    let session_id_text = session_id.to_string();
    let session_name_column_width = SESSION_SERVER_NAME.len().max("name".len());
    let owner_text = match owner_user_id {
        Some(owner_user_id) => owner_user_id.to_string(),
        None => "-".to_string(),
    };
    format!(
        "{:session_id_column_width$}  {:session_name_column_width$}  server  owner\n{session_id_text}  {SESSION_SERVER_NAME:session_name_column_width$}  local   {owner_text}\n",
        "id",
        "name",
        session_id_column_width = session_id_text.len(),
    )
}

/// A `koshi.kdl` with the switch on, naming `shared_directory_base` as its
/// shared directory. Each `\` in the path is written as `\\`.
fn build_switched_on_config(shared_directory_base: &Path) -> String {
    format!(
        "version 1\nallow-other-users #true\nshared-sessions-dir \"{}\"\n",
        shared_directory_base
            .display()
            .to_string()
            .replace('\\', "\\\\")
    )
}

// --- Unix ---

/// Let every local user reach the `koshi.kdl` written under `home_directory`: every
/// directory from the config directory up to `home_directory` opens to `0755`, and the
/// file itself to `0644`. The second user's `koshi` has to read that file to
/// learn the switch is on.
#[cfg(unix)]
fn set_config_read_permissions_for_every_user(home_directory: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let config_directory = resolve_config_directory_under_home(home_directory);
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
        if config_directory_path == home_directory {
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

/// The permission bits of `file_path`, without the file-type bits.
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

    let test_home_directory = build_short_test_directory();
    std::fs::set_permissions(
        test_home_directory.path(),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("the directory holding the copy opens to every local user");
    let koshi_binary_path = copy_koshi_binary(test_home_directory.path());
    (test_home_directory, koshi_binary_path)
}

/// [`build_koshi_command_at`], run as the user `user_id` and the group `group_id` instead of this
/// one. The group is set first, which is the only order that works once the
/// user id has been given up.
#[cfg(unix)]
fn build_koshi_command_as_user(
    binary_path: &Path,
    home_directory: &Path,
    user_id: u32,
    group_id: u32,
) -> std::process::Command {
    use std::os::unix::process::CommandExt;

    let mut process_command = build_koshi_command_at(binary_path, home_directory);
    process_command.gid(group_id).uid(user_id);
    process_command
}

/// The answer `koshi server-version --format json` gives when the only server
/// this user reaches is one session another user started: no router of this
/// user's own, and that session naming the build it runs.
#[cfg(unix)]
fn render_single_server_version_answer(session_id: SessionId, build_version: &str) -> String {
    format!(
        "[\n  {{\n    \"server_kind\": \"router\",\n    \"session_id\": null,\n    \
         \"state\": \"not running\"\n  }},\n  {{\n    \"server_kind\": \"session\",\n    \
         \"session_id\": \"{session_id}\",\n    \"state\": \"running\",\n    \"version\": \"{build_version}\"\n  }}\n]\n"
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

    let shared_directory_base = build_short_test_directory();
    let config_text = build_switched_on_config(shared_directory_base.path());

    let owner_home = build_short_test_directory();
    write_test_config(owner_home.path(), &config_text);
    let owner_runtime_directory = resolve_runtime_directory_under_home(owner_home.path());
    std::fs::create_dir_all(&owner_runtime_directory)
        .expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(owner_home.path(), &owner_runtime_directory, session_id);
    let endpoint_file =
        session_process.wait_for_session_endpoint(&owner_runtime_directory, session_id);

    // The switch moved the socket out of the private runtime directory, so the
    // second user below walks the shared one.
    let owner_shared_directory = shared_directory_base
        .path()
        .join(get_effective_user_id().to_string());
    assert_eq!(
        Path::new(&endpoint_file.socket_address).parent(),
        Some(owner_shared_directory.as_path())
    );

    let other_home = build_short_test_directory();
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
        render_single_session_listing(session_id, Some(get_effective_user_id()))
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
        session_process.wait_for_session_server_exit(),
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

    let owner_home = build_short_test_directory();
    write_test_config(owner_home.path(), "version 1\n");
    let owner_runtime_directory = resolve_runtime_directory_under_home(owner_home.path());
    std::fs::create_dir_all(&owner_runtime_directory)
        .expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(owner_home.path(), &owner_runtime_directory, session_id);
    session_process.wait_for_session_endpoint(&owner_runtime_directory, session_id);

    let other_home = build_short_test_directory();
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
        "id  name  server  owner\n"
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
    let shared_directory_base = build_short_test_directory();
    let home_directory = build_short_test_directory();
    write_test_config(
        home_directory.path(),
        &build_switched_on_config(shared_directory_base.path()),
    );
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    let endpoint_file = session_process.wait_for_session_endpoint(&runtime_directory, session_id);

    // The switch moved the socket into this user's directory under the shared
    // one, so the listing below walks that directory.
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

    let list_output = run_koshi_command(
        build_koshi_command_under_home(home_directory.path()).arg("list-sessions"),
    );
    assert_eq!(String::from_utf8_lossy(&list_output.stderr), "");
    assert_eq!(
        list_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&list_output.stdout),
        render_single_session_listing(session_id, None)
    );

    // The version walk covers the same two places the listing does, so this
    // user's own session earns one row here and not two.
    let version_output = run_koshi_command(
        build_koshi_command_under_home(home_directory.path())
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
    let home_directory = build_short_test_directory();
    write_test_config(home_directory.path(), "version 1\n");
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let session_id = SessionId::new();
    let mut session_process =
        start_session_server_under_home(home_directory.path(), &runtime_directory, session_id);
    let endpoint_file = session_process.wait_for_session_endpoint(&runtime_directory, session_id);

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

/// Start one session's server serving `runtime_directory`, with its files under
/// `home_directory`, `--allow-other-users` on its command line, and
/// `%ProgramData%` pointed at `program_data`.
#[cfg(windows)]
fn start_session_server_with_the_switch_forced(
    home_directory: &Path,
    runtime_directory: &Path,
    program_data: &Path,
    session_id: SessionId,
) -> SessionProcess {
    let child_process = start_program_process(
        build_koshi_command_under_home(home_directory)
            .arg("serve-session")
            .arg(session_id.to_string())
            .arg(SESSION_SERVER_NAME)
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .arg("--allow-other-users")
            .env("ProgramData", program_data)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    SessionProcess {
        child_process,
        runtime_directory: runtime_directory.to_path_buf(),
        session_id,
    }
}

/// Open a connection to the session at `endpoint_file` and complete the Hello,
/// presenting the token the endpoint file carries. The Hello answer names
/// [`PROTOCOL_VERSION`], this build, and the session server's own process
/// image, which is not this test process's image.
#[cfg(windows)]
fn open_windows_session_connection(endpoint_file: &EndpointFile) -> Connection {
    let mut connection = Connection::connect(&endpoint_file.socket_address)
        .expect("the session's pipe answers a connect");
    connection
        .send(&build_hello_request(endpoint_file))
        .expect("the server reads the Hello");
    let ipc_response: IpcResponse = connection.recv().expect("the server answers the Hello");
    assert_eq!(ipc_response.request_id, Some(1));
    match ipc_response.answer_result {
        IpcResult::Hello {
            protocol_version,
            build_version,
            process_image_id,
        } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(build_version, env!("CARGO_PKG_VERSION"));
            assert_ne!(process_image_id, ProcessImageId::get_own());
        }
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
    connection
}

/// `koshi --headless --allow-other-users` is refused before any session is
/// created: exit code 2, the reason on the error stream, and no endpoint file
/// in the runtime directory.
#[cfg(windows)]
#[test]
fn the_allow_other_users_flag_is_refused_on_windows() {
    let test_home_directory = build_short_test_directory();

    let refused_output = run_koshi_command(
        build_koshi_command_under_home(test_home_directory.path())
            .args(["--headless", "--allow-other-users"]),
    );

    assert_eq!(
        String::from_utf8_lossy(&refused_output.stderr).trim_end(),
        "koshi: invalid arguments: --allow-other-users works on macOS and Linux only; \
         on Windows only you reach your sessions"
    );
    assert_eq!(String::from_utf8_lossy(&refused_output.stdout), "");
    assert_eq!(
        refused_output.status.code(),
        Some(CliExitCode::UsageOrConfig.get_exit_code())
    );
    assert!(
        !resolve_runtime_directory_under_home(test_home_directory.path()).exists(),
        "a refused flag creates no runtime directory"
    );
}

/// A session server started with `--allow-other-users`, and with the switch
/// and `shared-sessions-dir` in its `koshi.kdl`, creates neither the shared
/// directory `koshi.kdl` names nor `koshi` under `%ProgramData%`, and serves
/// this user over the pipe its endpoint file names.
#[cfg(windows)]
#[test]
fn a_session_server_given_the_switch_creates_no_shared_directory_and_serves_this_user() {
    let test_home_directory = build_short_test_directory();
    let shared_directory_base = build_short_test_directory();
    let configured_shared_directory = shared_directory_base.path().join("shared");
    write_test_config(
        test_home_directory.path(),
        &build_switched_on_config(&configured_shared_directory),
    );
    let runtime_directory = build_short_test_directory();
    let program_data = build_short_test_directory();
    let session_id = SessionId::new();
    let mut session_process = start_session_server_with_the_switch_forced(
        test_home_directory.path(),
        runtime_directory.path(),
        program_data.path(),
        session_id,
    );
    let endpoint_file =
        session_process.wait_for_session_endpoint(runtime_directory.path(), session_id);

    // The Hello is checked inside `open_windows_session_connection`, which fails the test on any
    // other answer.
    let _connection = open_windows_session_connection(&endpoint_file);
    assert!(
        !configured_shared_directory.exists(),
        "the shared directory koshi.kdl names was created"
    );
    assert!(
        !program_data.path().join("koshi").exists(),
        "koshi under %ProgramData% was created"
    );
}

/// With the switch on in `koshi.kdl`, `koshi list-sessions` lists this user's
/// own session once and prints nothing on its error stream.
#[cfg(windows)]
#[test]
fn this_users_own_session_is_listed_once_with_the_switch_on() {
    let test_home_directory = build_short_test_directory();
    write_test_config(
        test_home_directory.path(),
        "version 1\nallow-other-users #true\n",
    );
    let runtime_directory = resolve_runtime_directory_under_home(test_home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("a runtime directory under the test home");
    let program_data = build_short_test_directory();
    let session_id = SessionId::new();
    let mut session_process = start_session_server_with_the_switch_forced(
        test_home_directory.path(),
        &runtime_directory,
        program_data.path(),
        session_id,
    );
    session_process.wait_for_session_endpoint(&runtime_directory, session_id);

    let list_output = run_koshi_command(
        build_koshi_command_under_home(test_home_directory.path())
            .arg("list-sessions")
            .env("ProgramData", program_data.path()),
    );
    assert_eq!(String::from_utf8_lossy(&list_output.stderr), "");
    assert_eq!(
        list_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        String::from_utf8_lossy(&list_output.stdout),
        render_single_session_listing(session_id, None)
    );
}
