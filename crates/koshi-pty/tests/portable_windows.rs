//! Windows Job-Object and pseudoconsole PTY backend integration tests.
//!
//! Compile-checked on every build for the Windows target; executed by Windows
//! test runners. Unix builds skip this file entirely.
//!
//! The expected exit code is `137` by construction: `force` calls
//! `TerminateProcess(pty_handle, 137)` and `tree` calls `TerminateJobObject(job, 137)`,
//! and Win32 makes that the terminated process's exit code.
//!
//! The two graceful-close tests measure how long `kill` takes, not only what it
//! returns.
//!
//! `a_pane_takes_input_and_prints_the_child_output` pins the pseudoconsole
//! round trip, which `portable-pty` builds on flags and a PTY handle order
//! Microsoft's reference does not sanction — see the `koshi_pty::portable`
//! module documentation. Nothing in this file answers the pseudoconsole's
//! cursor-position query; see [`CURSOR_QUERY_BYTES`].
#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::portable::PortablePtyBackend;

const STANDARD_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// Upper bound on a graceful close that skips the grace window. A skipped wait
/// returns in milliseconds; a wait taken costs the full 3 seconds. 1500ms
/// separates the two and leaves room for a slow shared runner.
const KILL_BUDGET_DURATION: Duration = Duration::from_millis(1500);

fn build_spawn_spec(program: &str, command_arguments: &[&str]) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(program),
        arguments: command_arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::from_program(Path::new(program)),
    }
}

fn wait_for_pane_exit(pty_handle: &PtyHandle, timeout_duration: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout_duration;
    loop {
        if let Some(exit_status) = pty_handle.try_receive_exit_status() {
            return Some(exit_status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// The cursor-position query (DSR, `CSI 6 n`) the pseudoconsole sends before it
/// lets its child print. `spawn` queues the answer on the pane's input, and the
/// pane's reader removes the query from the output. A pane whose query goes
/// unanswered prints nothing.
const CURSOR_QUERY_BYTES: &[u8] = b"\x1b[6n";

/// Read the pane's output until `expected_output_text` appears or `timeout_duration` runs out, and
/// hand back everything read. Writes nothing to the pane.
fn read_pane_output_until(
    pty_handle: &PtyHandle,
    expected_output_text: &str,
    timeout_duration: Duration,
) -> String {
    let deadline = Instant::now() + timeout_duration;
    let mut child_output_bytes: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        match pty_handle.try_receive_output_chunk() {
            Some(output_chunk) => {
                child_output_bytes.extend_from_slice(&output_chunk);
                if String::from_utf8_lossy(&child_output_bytes).contains(expected_output_text) {
                    break;
                }
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
    String::from_utf8_lossy(&child_output_bytes).into_owned()
}

/// The whole pseudoconsole round trip in one pane: bytes written reach the
/// child, what the child prints comes back, and closing the pane reports the
/// child's own exit code.
///
/// The two halves are proved apart. `cmd.exe` prints its own banner naming
/// Windows before it reads a byte, and that banner shows only that the console
/// renders. `set /a 6*7` then proves the input landed: the console echoes the
/// typed line, which holds `6*7` and not `42`, so only a child that ran the
/// command can produce `42`. `exit 7` ends the child with 7, which arrives once
/// the console is closed and the reader has read it out.
///
/// Nothing here answers the pseudoconsole's cursor-position query
/// ([`CURSOR_QUERY_BYTES`]); an unanswered query makes the banner check fail first.
#[test]
fn a_pane_takes_input_and_prints_the_child_output() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = pty_backend
        .spawn_pane(
            koshi_core::ids::PaneId::new(),
            build_spawn_spec("cmd.exe", &[]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");

    // Nothing written yet: the banner alone says the console renders.
    let console_banner_text =
        read_pane_output_until(&pty_handle, "Microsoft Windows", Duration::from_secs(15));
    assert!(
        console_banner_text.contains("Microsoft Windows"),
        "the pane printed no console banner; it read {console_banner_text:?}",
    );
    assert!(
        !console_banner_text
            .as_bytes()
            .windows(CURSOR_QUERY_BYTES.len())
            .any(|query_window_bytes| query_window_bytes == CURSOR_QUERY_BYTES),
        "the terminal's opening query reached the output; the pane's reader must take it out",
    );

    pty_backend
        .write_pane_input(pty_handle.get_pane_id(), b"set /a 6*7\r\n")
        .expect("write to the pane");
    let command_output_text = read_pane_output_until(&pty_handle, "42", Duration::from_secs(15));
    assert!(
        command_output_text.contains("42"),
        "the line never reached the child, so it printed no answer; it read {command_output_text:?}",
    );

    pty_backend
        .write_pane_input(pty_handle.get_pane_id(), b"exit 7\r\n")
        .expect("write to the pane");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(7)),
    );
}

/// Input written in the first moments of a pane still reaches its child.
///
/// Nothing is read back before the write, so it lands while the pane's terminal
/// is still opening.
///
/// `set /p` reads a line, which is what a client writing bytes to a pane
/// produces.
#[test]
fn a_line_typed_before_the_pane_is_read_still_reaches_the_child() {
    let pty_backend = PortablePtyBackend::new();
    // The line ends `set /p`, and `cmd.exe` exits with the code it was told, so
    // an exit of 7 is the line having arrived.
    let pty_handle = pty_backend
        .spawn_pane(
            koshi_core::ids::PaneId::new(),
            build_spawn_spec("cmd.exe", &["/C", "set /p x= & exit 7"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");

    pty_backend
        .write_pane_input(pty_handle.get_pane_id(), b"typed\r")
        .expect("write to the pane");

    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(7)),
        "the line never reached the child; the pane read {:?}",
        read_pane_output_until(&pty_handle, "\u{0}", Duration::from_millis(100)),
    );
}

#[test]
fn force_terminates_a_running_child() {
    let pty_backend = PortablePtyBackend::new();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let pty_handle = pty_backend
        .spawn_pane(
            koshi_core::ids::PaneId::new(),
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Force)
        .expect("force kill");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
}

#[test]
fn tree_terminates_the_job() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = pty_backend
        .spawn_pane(
            koshi_core::ids::PaneId::new(),
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Tree)
        .expect("tree kill");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
}

#[test]
fn a_graceful_close_does_not_spend_the_grace_window() {
    let pty_backend = PortablePtyBackend::new();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let pty_handle = pty_backend
        .spawn_pane(
            koshi_core::ids::PaneId::new(),
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let kill_started_at = Instant::now();
    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::Graceful {
                timeout_duration: GRACEFUL_TIMEOUT_DURATION,
            },
        )
        .expect("graceful kill");
    let kill_duration = kill_started_at.elapsed();
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
    assert!(
        kill_duration < KILL_BUDGET_DURATION,
        "a graceful close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {kill_duration:?}",
    );
}

#[test]
fn a_graceful_tree_close_does_not_spend_the_grace_window() {
    let pty_backend = PortablePtyBackend::new();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let pty_handle = pty_backend
        .spawn_pane(
            koshi_core::ids::PaneId::new(),
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let kill_started_at = Instant::now();
    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::GracefulTree {
                timeout_duration: GRACEFUL_TIMEOUT_DURATION,
            },
        )
        .expect("graceful tree kill");
    let kill_duration = kill_started_at.elapsed();
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
    assert!(
        kill_duration < KILL_BUDGET_DURATION,
        "a graceful tree close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {kill_duration:?}",
    );
}
