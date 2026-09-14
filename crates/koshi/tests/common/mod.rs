//! What the cross-process tests in this directory share: ending a process the
//! test did not spawn, taking a copy of the `koshi` binary under test, and
//! starting that binary.
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
