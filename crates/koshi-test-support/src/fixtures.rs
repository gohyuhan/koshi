//! Shared test fixtures.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_core::key::{
    BindingModifierFlags, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
};
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::router::{compute_router_socket_address, resolve_router_endpoint_path};
use tempfile::TempDir;

/// How long [`start_program_process`] keeps trying while the operating system
/// reports the program file as held open for writing: 20 seconds.
pub const BUSY_PROGRAM_WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long [`start_program_process`] pauses between attempts: 20
/// milliseconds.
pub const BUSY_PROGRAM_RETRY_INTERVAL_DURATION: Duration = Duration::from_millis(20);

/// Each keybinding modifier beside the reported modifier that stands for it.
/// The two sets number their bits differently, so a chord's bit pattern is
/// never a reported bit pattern.
const REPORTED_MODIFIER_BY_BINDING_MODIFIER: [(BindingModifierFlags, KeyModifierFlags); 4] = [
    (BindingModifierFlags::CTRL, KeyModifierFlags::CTRL),
    (BindingModifierFlags::ALT, KeyModifierFlags::ALT),
    (BindingModifierFlags::SHIFT, KeyModifierFlags::SHIFT),
    (BindingModifierFlags::SUPER, KeyModifierFlags::SUPER),
];

/// The complete key event a terminal reports for one chord: a press, with no
/// alternative keys and no associated text.
///
/// [`KeyInput::to_binding_chord`] on the result gives `chord` back.
#[must_use]
pub fn build_key_input_for_chord(chord: KeyChord) -> KeyInput {
    let mut modifier_flags = KeyModifierFlags::NONE;
    for (binding_modifier, reported_modifier) in REPORTED_MODIFIER_BY_BINDING_MODIFIER {
        if chord.modifier_flags.has_all_modifiers(binding_modifier) {
            modifier_flags = modifier_flags.union(reported_modifier);
        }
    }
    KeyInput {
        key: KeyIdentity::Key(chord.key),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags,
    }
}

/// The process id `2147483647`, the largest `pid_t`. No running process holds
/// it.
pub const NO_SUCH_PROCESS_ID: u32 = 2_147_483_647;

/// Create an isolated runtime directory and remove it when the returned
/// [`TempDir`] drops.
///
/// Unix uses `/tmp` as the parent directory. Windows uses
/// [`std::env::temp_dir`].
///
/// # Panics
///
/// Panics when the directory cannot be created.
#[must_use]
pub fn build_test_runtime_directory() -> TempDir {
    #[cfg(unix)]
    let base_directory = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base_directory = std::env::temp_dir();
    TempDir::new_in(base_directory).expect("a temporary runtime directory")
}

/// Write a program named `program_stem` into `program_directory` that adds
/// one line to the file at `run_log_path` each time it runs, then prints
/// `printed_line`: a shell script on Unix, and a `.cmd` file on Windows. Hands
/// back the program's path.
///
/// Example: `printed_line` `koshi 9.9.9` makes a program that stands in for a
/// koshi binary answering `--version` with that line.
///
/// # Panics
///
/// Panics when the program cannot be written or made runnable.
pub fn write_printing_program(
    program_directory: &Path,
    program_stem: &str,
    run_log_path: &Path,
    printed_line: &str,
) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let program_path = program_directory.join(program_stem);
        let program_text = format!(
            "#!/bin/sh\necho run >> '{}'\necho '{printed_line}'\n",
            run_log_path.display()
        );
        std::fs::write(&program_path, program_text).expect("the program is written");
        std::fs::set_permissions(&program_path, std::fs::Permissions::from_mode(0o755))
            .expect("the program runs");
        program_path
    }
    #[cfg(windows)]
    {
        let program_path = program_directory.join(format!("{program_stem}.cmd"));
        let program_text = format!(
            "@echo off\r\necho run>>\"{}\"\r\necho {printed_line}\r\n",
            run_log_path.display()
        );
        std::fs::write(&program_path, program_text).expect("the program is written");
        program_path
    }
}

/// Start the program `process_command` names, and hand back the running
/// process.
///
/// A start that fails with [`ErrorKind::ExecutableFileBusy`] (`ETXTBSY`: a
/// process holds the program file open for writing) is tried again every
/// [`BUSY_PROGRAM_RETRY_INTERVAL_DURATION`].
///
/// # Panics
///
/// Panics when the program file is still busy after
/// [`BUSY_PROGRAM_WAIT_DURATION`], or the start fails for any other reason.
pub fn start_program_process(process_command: &mut Command) -> Child {
    let busy_deadline = Instant::now() + BUSY_PROGRAM_WAIT_DURATION;
    loop {
        match process_command.spawn() {
            Ok(started_process) => return started_process,
            Err(spawn_error) if spawn_error.kind() == ErrorKind::ExecutableFileBusy => {
                assert!(
                    Instant::now() < busy_deadline,
                    "the program at {} was still busy after {BUSY_PROGRAM_WAIT_DURATION:?}",
                    process_command.get_program().to_string_lossy()
                );
                std::thread::sleep(BUSY_PROGRAM_RETRY_INTERVAL_DURATION);
            }
            Err(spawn_error) => panic!(
                "the program at {} starts: {spawn_error}",
                process_command.get_program().to_string_lossy()
            ),
        }
    }
}

/// How many times a program from [`write_printing_program`] that logs to
/// `run_log_path` has run. `0` when the log does not exist.
#[must_use]
pub fn count_program_runs(run_log_path: &Path) -> usize {
    std::fs::read_to_string(run_log_path)
        .map(|run_log_text| run_log_text.lines().count())
        .unwrap_or(0)
}

/// Advertise `session_id` in `runtime_directory` under the token made from
/// `connection_secret` and under `process_id`, the way a session server does
/// every time it binds, and hand back what was written.
///
/// # Panics
///
/// Panics when the endpoint file cannot be written.
pub fn write_session_endpoint_file(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_secret: &str,
    process_id: u32,
) -> EndpointFile {
    let session_endpoint = EndpointFile {
        socket_address: compute_socket_address(runtime_directory, session_id),
        connection_token: ConnectionToken::from_secret(connection_secret),
        process_id,
    };
    session_endpoint
        .write_to_path(&EndpointFile::resolve_endpoint_file_path(
            runtime_directory,
            session_id,
        ))
        .expect("write the session endpoint file");
    session_endpoint
}

/// Advertise a router in `runtime_directory` under the token made from
/// `connection_secret` and the process id `5000`, the way a router does each
/// time it starts.
///
/// # Panics
///
/// Panics when the endpoint file cannot be written.
pub fn write_router_endpoint_file(runtime_directory: &Path, connection_secret: &str) {
    EndpointFile {
        socket_address: compute_router_socket_address(runtime_directory),
        connection_token: ConnectionToken::from_secret(connection_secret),
        process_id: 5000,
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");
}

/// Take the exclusive update lock in `runtime_directory` the way `koshi
/// update` does, and hand back the file that holds it.
///
/// # Panics
///
/// Panics when the lock file cannot be opened or locked.
pub fn hold_update_lock(runtime_directory: &Path) -> std::fs::File {
    let update_lock_file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(koshi_ipc::endpoint::resolve_update_lock_path(
            runtime_directory,
        ))
        .expect("open the update lock file");
    update_lock_file.lock().expect("take the update lock");
    update_lock_file
}

#[cfg(test)]
mod tests;
