//! Windows Job-Object and pseudoconsole PTY backend integration tests.
//!
//! Compile-checked on every build for the Windows target; executed by Windows
//! test runners. Unix builds skip this file entirely.
//!
//! Each test reads its pane back through a PTY sink that records each pane's
//! output and exit. A closed pane reports no exit to its sink: each kill test
//! opens a handle to the child process before the kill and reads the exit code
//! through it.
//!
//! The expected exit code is `137` by construction: `force` calls
//! `TerminateProcess(process_handle, 137)` and `tree` calls
//! `TerminateJobObject(job, 137)`, and Win32 makes that the terminated
//! process's exit code.
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

mod common;

use std::time::Duration;

use common::{
    build_pty_backend, build_spawn_spec, kill_pane_within_deadline, read_pane_output_until,
    wait_for_pane_exit, STANDARD_PTY_SIZE,
};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_pty::backend::state::PtyBackend;
use koshi_pty::portable::PortablePtyBackend;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
};

/// Upper bound on a graceful close that skips the grace window. A skipped wait
/// returns in milliseconds; a wait taken costs the full 3 seconds. 1500ms
/// separates the two and leaves room for a slow shared runner.
const KILL_BUDGET_DURATION: Duration = Duration::from_millis(1500);

/// The exit code `force` and `tree` hand to `TerminateProcess` and
/// `TerminateJobObject`.
const TERMINATED_EXIT_CODE: u32 = 137;

/// A handle to one child process, opened for reading its exit code and closed
/// on drop. The exit code stays readable through it after the process ends.
struct ChildProcessHandle(HANDLE);

impl ChildProcessHandle {
    /// Opens the process of `pane_id`'s child. Panics when the pane has no
    /// child or `OpenProcess` fails.
    fn from_pane_child(pty_backend: &PortablePtyBackend, pane_id: PaneId) -> Self {
        let child_process_id = pty_backend
            .get_child_process_id(pane_id)
            .expect("a live pane has a child");
        let process_handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child_process_id) };
        assert!(
            !process_handle.is_null(),
            "OpenProcess failed for pid {child_process_id}"
        );
        Self(process_handle)
    }

    /// The process's exit code, or `STILL_ACTIVE` (259) while it runs. Panics
    /// when `GetExitCodeProcess` fails.
    fn get_exit_code(&self) -> u32 {
        let mut exit_code: u32 = 0;
        let exit_code_read_result = unsafe { GetExitCodeProcess(self.0, &mut exit_code) };
        assert_ne!(exit_code_read_result, 0, "GetExitCodeProcess failed");
        exit_code
    }
}

impl Drop for ChildProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The cursor-position query (DSR, `CSI 6 n`) the pseudoconsole sends before it
/// lets its child print. `spawn` queues the answer on the pane's input, and the
/// pane's reader removes the query from the output. A pane whose query goes
/// unanswered prints nothing.
const CURSOR_QUERY_BYTES: &[u8] = b"\x1b[6n";

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
    let (pty_backend, pane_output_recorder) = build_pty_backend();
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(pane_id, build_spawn_spec("cmd.exe", &[]), STANDARD_PTY_SIZE)
        .expect("spawn cmd");

    // Nothing written yet: the banner alone says the console renders.
    let console_banner_text = read_pane_output_until(
        &pane_output_recorder,
        pane_id,
        "Microsoft Windows",
        Duration::from_secs(15),
    );
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
        .write_pane_input(pane_id, b"set /a 6*7\r\n")
        .expect("write to the pane");
    let command_output_text = read_pane_output_until(
        &pane_output_recorder,
        pane_id,
        "42",
        Duration::from_secs(15),
    );
    assert!(
        command_output_text.contains("42"),
        "the line never reached the child, so it printed no answer; it read {command_output_text:?}",
    );

    pty_backend
        .write_pane_input(pane_id, b"exit 7\r\n")
        .expect("write to the pane");
    assert_eq!(
        wait_for_pane_exit(&pane_output_recorder, pane_id, Duration::from_secs(15)),
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
    let (pty_backend, pane_output_recorder) = build_pty_backend();
    // The line ends `set /p`, and `cmd.exe` exits with the code it was told, so
    // an exit of 7 is the line having arrived.
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "set /p x= & exit 7"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");

    pty_backend
        .write_pane_input(pane_id, b"typed\r")
        .expect("write to the pane");

    assert_eq!(
        wait_for_pane_exit(&pane_output_recorder, pane_id, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(7)),
        "the line never reached the child; the pane read {:?}",
        read_pane_output_until(
            &pane_output_recorder,
            pane_id,
            "\u{0}",
            Duration::from_millis(100)
        ),
    );
}

#[test]
fn force_terminates_a_running_child() {
    let (pty_backend, _) = build_pty_backend();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let child_process_handle = ChildProcessHandle::from_pane_child(&pty_backend, pane_id);
    kill_pane_within_deadline(&pty_backend, pane_id, KillPolicy::Force);
    // `kill` joins the watcher, which has waited out the child by then.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
}

#[test]
fn tree_terminates_the_job() {
    let (pty_backend, _) = build_pty_backend();
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let child_process_handle = ChildProcessHandle::from_pane_child(&pty_backend, pane_id);
    kill_pane_within_deadline(&pty_backend, pane_id, KillPolicy::Tree);
    // `kill` joins the watcher, which has waited out the child by then.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
}

#[test]
fn a_graceful_close_does_not_spend_the_grace_window() {
    let (pty_backend, _) = build_pty_backend();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let child_process_handle = ChildProcessHandle::from_pane_child(&pty_backend, pane_id);
    let kill_elapsed_duration = kill_pane_within_deadline(
        &pty_backend,
        pane_id,
        KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        },
    );
    // `kill` joins the watcher, which has waited out the child by then.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
    assert!(
        kill_elapsed_duration < KILL_BUDGET_DURATION,
        "a graceful close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {kill_elapsed_duration:?}",
    );
}

#[test]
fn a_graceful_tree_close_does_not_spend_the_grace_window() {
    let (pty_backend, _) = build_pty_backend();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let child_process_handle = ChildProcessHandle::from_pane_child(&pty_backend, pane_id);
    let kill_elapsed_duration = kill_pane_within_deadline(
        &pty_backend,
        pane_id,
        KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        },
    );
    // `kill` joins the watcher, which has waited out the child by then.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
    assert!(
        kill_elapsed_duration < KILL_BUDGET_DURATION,
        "a graceful tree close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {kill_elapsed_duration:?}",
    );
}
