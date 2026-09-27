//! What the cross-process tests in this directory share: ending a process the
//! test did not spawn, taking a copy of the `koshi` binary under test, building
//! the command that runs that binary under a test home, and starting it.
//!
//! Every test binary declaring `mod common;` compiles all of it, and no single
//! binary uses every helper, so an unused one is allowed here.
#![allow(dead_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long [`start_koshi_process`] keeps trying while the operating system reports the
/// program file as busy.
const BUSY_WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long [`start_koshi_process`] pauses between attempts.
const BUSY_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(20);

/// End the process with id `pid`, whatever it is doing.
#[cfg(unix)]
pub fn terminate_process(process_id: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(process_id.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// End the process with id `pid`, whatever it is doing.
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

/// Copy the `koshi` binary into `dir` and hand back the copy's path. A test
/// that renames its binary or changes its mode owns that file alone.
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

/// The `koshi` binary at `binary_path`, set to keep its files under
/// `home_directory` rather than in the developer's own directories, and stripped
/// of the pane identity so it runs as a CLI outside any session. Standard input
/// is closed, and both output streams are pipes the test reads. The runtime
/// directory the child serves is `<home_directory>/run`.
#[cfg(unix)]
pub fn build_koshi_command_at(binary_path: &Path, home_directory: &Path) -> Command {
    let mut process_command = Command::new(binary_path);
    process_command
        .env("HOME", home_directory)
        .env("KOSHI_RUNTIME_DIR", home_directory.join("run"))
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
    process_command.env("XDG_CONFIG_HOME", home_directory.join(".config"));
    process_command
}

/// [`build_koshi_command_at`], run from the binary this build produced.
#[cfg(unix)]
pub fn build_koshi_command_under_home(home_directory: &Path) -> Command {
    build_koshi_command_at(Path::new(env!("CARGO_BIN_EXE_koshi")), home_directory)
}

/// Start the `koshi` binary `process_command` names and hand back the running process.
///
/// Linux refuses to run a file that any process holds open for writing, and
/// answers `ETXTBSY`. The tests here run side by side: one copies the binary
/// with [`copy_koshi_binary`] while another forks to start a process, and the fork
/// inherits that open copy until it reaches its own exec. Starting is retried
/// for as long as [`BUSY_WAIT_DURATION`], and fails the test after that.
pub fn start_koshi_process(process_command: &mut Command) -> Child {
    let deadline = Instant::now() + BUSY_WAIT_DURATION;
    loop {
        match process_command.spawn() {
            Ok(started_process) => return started_process,
            Err(spawn_error) if spawn_error.kind() == ErrorKind::ExecutableFileBusy => {
                assert!(
                    Instant::now() < deadline,
                    "the koshi binary at {} was still busy after {BUSY_WAIT_DURATION:?}",
                    process_command.get_program().to_string_lossy()
                );
                std::thread::sleep(BUSY_POLL_INTERVAL_DURATION);
            }
            Err(spawn_error) => panic!(
                "the koshi binary at {} starts: {spawn_error}",
                process_command.get_program().to_string_lossy()
            ),
        }
    }
}
