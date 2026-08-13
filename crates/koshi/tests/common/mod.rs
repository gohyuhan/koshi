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

/// How long [`start_koshi`] keeps trying while the operating system reports the
/// program file as busy.
const BUSY_WAIT: Duration = Duration::from_secs(20);

/// How long [`start_koshi`] pauses between attempts.
const BUSY_POLL: Duration = Duration::from_millis(20);

/// End the process with id `pid`, whatever it is doing.
#[cfg(unix)]
pub fn end_process(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// End the process with id `pid`, whatever it is doing.
#[cfg(windows)]
pub fn end_process(pid: u32) {
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/F")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Copy the `koshi` binary into `dir` and hand back the copy's path. A test
/// that renames its binary or changes its mode owns that file alone.
pub fn copy_of_koshi(dir: &Path) -> PathBuf {
    let exe = dir.join(if cfg!(windows) { "koshi.exe" } else { "koshi" });
    std::fs::copy(env!("CARGO_BIN_EXE_koshi"), &exe).expect("the koshi binary is copied");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
            .expect("the copy runs");
    }
    exe
}

/// Start the `koshi` binary `command` names and hand back the running process.
///
/// Linux refuses to run a file that any process holds open for writing, and
/// answers `ETXTBSY`. The tests here run side by side: one copies the binary
/// with [`copy_of_koshi`] while another forks to start a process, and the fork
/// inherits that open copy until it reaches its own exec. Starting is retried
/// for as long as [`BUSY_WAIT`], and fails the test after that.
pub fn start_koshi(command: &mut Command) -> Child {
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(error) if error.kind() == ErrorKind::ExecutableFileBusy => {
                assert!(
                    Instant::now() < deadline,
                    "the koshi binary at {} was still busy after {BUSY_WAIT:?}",
                    command.get_program().to_string_lossy()
                );
                std::thread::sleep(BUSY_POLL);
            }
            Err(error) => panic!(
                "the koshi binary at {} starts: {error}",
                command.get_program().to_string_lossy()
            ),
        }
    }
}
