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
//! The expected exit code is `137`: `KillPolicy::Force` calls
//! `TerminateProcess(process_handle, 137)`, `KillPolicy::Tree` calls
//! `TerminateJobObject(job, 137)`, and Win32 makes that the terminated
//! process's exit code.
//!
//! The two graceful-close tests measure how long `kill_pane` takes, not only
//! what it returns.
//!
//! `a_pane_takes_input_and_prints_the_child_output` pins the round trip of a
//! pseudoconsole opened with two flags outside Microsoft's documented set —
//! see the `koshi_pty::portable` module documentation. Nothing in this file
//! answers the pseudoconsole's cursor-position query; see
//! [`CURSOR_QUERY_BYTES`].
#![cfg(windows)]

mod common;

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
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
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The longest a graceful close that skips the grace window may take. A
/// skipped wait returns in milliseconds, and a wait taken lasts the full 3
/// seconds.
const KILL_BUDGET_DURATION: Duration = Duration::from_millis(1500);

/// The exit code `KillPolicy::Force` and `KillPolicy::Tree` hand to
/// `TerminateProcess` and `TerminateJobObject`.
const TERMINATED_EXIT_CODE: u32 = 137;

/// A handle to one child process, opened for reading its exit code and closed
/// on drop. The exit code stays readable through it after the process ends.
struct ChildProcessHandle(OwnedHandle);

impl ChildProcessHandle {
    /// Opens the process of `pane_id`'s child. Panics when the pane has no
    /// child or `OpenProcess` fails.
    fn from_pane_child(pty_backend: &PortablePtyBackend, pane_id: PaneId) -> Self {
        let child_process_id = pty_backend
            .get_child_process_id(pane_id)
            .expect("a live pane has a child");
        // SAFETY: `OpenProcess` takes its arguments by value and reads no
        // memory.
        let process_handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child_process_id) };
        assert!(
            !process_handle.is_null(),
            "OpenProcess failed for pid {child_process_id}"
        );
        // SAFETY: `process_handle` is the non-NULL handle that `OpenProcess`
        // returned, and nothing else owns it.
        Self(unsafe { OwnedHandle::from_raw_handle(process_handle) })
    }

    /// The process's exit code, or `STILL_ACTIVE` (259) while it runs. Panics
    /// when `GetExitCodeProcess` fails.
    fn get_exit_code(&self) -> u32 {
        let mut exit_code: u32 = 0;
        // SAFETY: `self.0` keeps the process handle open, and `exit_code` is a
        // writable `u32`.
        let exit_code_read_result =
            unsafe { GetExitCodeProcess(self.0.as_raw_handle(), &mut exit_code) };
        assert_ne!(exit_code_read_result, 0, "GetExitCodeProcess failed");
        exit_code
    }
}

/// Spawn `ping -n 100 127.0.0.1` under `cmd.exe` in a new pane of
/// `pty_backend`: a child that runs for about 100 seconds. Returns the pane id
/// and a handle to the child process.
fn spawn_long_running_pane(pty_backend: &PortablePtyBackend) -> (PaneId, ChildProcessHandle) {
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");
    let child_process_handle = ChildProcessHandle::from_pane_child(pty_backend, pane_id);
    (pane_id, child_process_handle)
}

/// The cursor-position query (DSR, `CSI 6 n`) the pseudoconsole sends before it
/// lets its child print. `spawn_pane` queues the answer on the pane's input, and the
/// pane's reader removes the query from the output. A pane whose query goes
/// unanswered prints nothing.
const CURSOR_QUERY_BYTES: &[u8] = b"\x1b[6n";

/// The whole pseudoconsole round trip in one pane: bytes written reach the
/// child, what the child prints comes back, and closing the pane reports the
/// child's own exit code.
///
/// The test checks the two halves apart. `cmd.exe` prints a banner that names
/// Windows before it reads a byte. Then the pane gets `set /a 6*7`: the console
/// echo of the typed line holds `6*7`, and the child prints `42`. `exit 7` ends
/// the child with 7, which arrives once the console is closed and the reader
/// has read it out.
///
/// Nothing here answers the pseudoconsole's cursor-position query
/// ([`CURSOR_QUERY_BYTES`]). With the query unanswered, the banner check fails.
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

/// A child that ends with an NTSTATUS error code reports the `i32` with the
/// same bits: `cmd.exe` exiting with `-1073741819` reports `0xC0000005`.
#[test]
fn a_child_ending_with_an_ntstatus_error_code_reports_the_same_bits() {
    let (pty_backend, pane_output_recorder) = build_pty_backend();
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd.exe", &["/C", "exit -1073741819"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");

    assert_eq!(
        wait_for_pane_exit(&pane_output_recorder, pane_id, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(0xC000_0005_u32.cast_signed())),
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

/// The pane program starts in the working directory and with the environment
/// variables its spawn specification names.
///
/// `echo` prints the variable's value and `type` prints a file it finds only in
/// that working directory: each word proves one half.
#[test]
fn a_pane_program_starts_in_its_working_directory_with_its_environment_variables() {
    let working_directory = tempfile::tempdir().expect("create a working directory");
    std::fs::write(
        working_directory.path().join("koshi-marker.txt"),
        "marker-from-folder",
    )
    .expect("write the marker file");
    let mut spawn_spec = build_spawn_spec(
        "cmd.exe",
        &[
            "/C",
            "echo %KOSHI_SPAWN_TEST%& type koshi-marker.txt & exit 3",
        ],
    );
    spawn_spec.working_directory = Some(working_directory.path().to_path_buf());
    spawn_spec.environment_variables.insert(
        "KOSHI_SPAWN_TEST".to_string(),
        "value-from-overlay".to_string(),
    );
    let (pty_backend, pane_output_recorder) = build_pty_backend();
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(pane_id, spawn_spec, STANDARD_PTY_SIZE)
        .expect("spawn cmd");

    let pane_output_text = read_pane_output_until(
        &pane_output_recorder,
        pane_id,
        "marker-from-folder",
        Duration::from_secs(15),
    );
    assert!(
        pane_output_text.contains("value-from-overlay"),
        "the variable never reached the child; it read {pane_output_text:?}",
    );
    assert!(
        pane_output_text.contains("marker-from-folder"),
        "the child did not start in its working directory; it read {pane_output_text:?}",
    );
    assert_eq!(
        wait_for_pane_exit(&pane_output_recorder, pane_id, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(3)),
    );
}

/// A program named without its extension is found in `PATH` with a `PATHEXT`
/// extension appended: `cmd` starts `cmd.exe`, which exits with the code it is
/// told.
#[test]
fn a_program_named_without_its_extension_is_found_in_path() {
    let (pty_backend, pane_output_recorder) = build_pty_backend();
    let pane_id = PaneId::new();
    pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("cmd", &["/C", "exit 5"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn cmd");

    assert_eq!(
        wait_for_pane_exit(&pane_output_recorder, pane_id, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(5)),
    );
}

#[test]
fn a_force_kill_terminates_a_running_child() {
    let (pty_backend, _) = build_pty_backend();
    let (pane_id, child_process_handle) = spawn_long_running_pane(&pty_backend);
    kill_pane_within_deadline(&pty_backend, pane_id, KillPolicy::Force);
    // `kill_pane` joins the watcher, and the watcher ends after the child's
    // exit.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
}

#[test]
fn a_tree_kill_terminates_the_pane_job() {
    let (pty_backend, _) = build_pty_backend();
    let (pane_id, child_process_handle) = spawn_long_running_pane(&pty_backend);
    kill_pane_within_deadline(&pty_backend, pane_id, KillPolicy::Tree);
    // `kill_pane` joins the watcher, and the watcher ends after the child's
    // exit.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
}

#[test]
fn a_graceful_close_does_not_spend_the_grace_window() {
    let (pty_backend, _) = build_pty_backend();
    let (pane_id, child_process_handle) = spawn_long_running_pane(&pty_backend);
    let kill_elapsed_duration = kill_pane_within_deadline(
        &pty_backend,
        pane_id,
        KillPolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        },
    );
    // `kill_pane` joins the watcher, and the watcher ends after the child's
    // exit.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
    assert!(
        kill_elapsed_duration < KILL_BUDGET_DURATION,
        "a graceful close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {kill_elapsed_duration:?}",
    );
}

#[test]
fn a_graceful_tree_close_does_not_spend_the_grace_window() {
    let (pty_backend, _) = build_pty_backend();
    let (pane_id, child_process_handle) = spawn_long_running_pane(&pty_backend);
    let kill_elapsed_duration = kill_pane_within_deadline(
        &pty_backend,
        pane_id,
        KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        },
    );
    // `kill_pane` joins the watcher, and the watcher ends after the child's
    // exit.
    assert_eq!(child_process_handle.get_exit_code(), TERMINATED_EXIT_CODE);
    assert!(
        kill_elapsed_duration < KILL_BUDGET_DURATION,
        "a graceful tree close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {kill_elapsed_duration:?}",
    );
}
