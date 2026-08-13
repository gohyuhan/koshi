//! Windows Job-Object and pseudoconsole backend integration tests.
//!
//! Compile-checked on every build for the Windows target; executed by Windows
//! test runners. Unix builds skip this file entirely.
//!
//! The expected exit code is `137` by construction: `force` calls
//! `TerminateProcess(handle, 137)` and `tree` calls `TerminateJobObject(job, 137)`,
//! and Win32 makes that the terminated process's exit code.
//!
//! The two graceful-close tests measure how long `kill` takes, not only what it
//! returns.
//!
//! `a_pane_takes_input_and_prints_the_child_output` pins the pseudoconsole
//! round trip, which `portable-pty` builds on flags and a handle order
//! Microsoft's reference does not sanction — see the `koshi_pty::portable`
//! module documentation. Nothing in this file answers the pseudoconsole's
//! cursor-position query, which it waits on before letting its child print;
//! the pane's reader answers it.
#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::portable::PortablePtyBackend;

const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// Upper bound on a graceful close that skips the grace window. A skipped wait
/// returns in milliseconds; a wait taken costs the full 3 seconds. 1500ms
/// separates the two and leaves room for a slow shared runner.
const KILL_BUDGET: Duration = Duration::from_millis(1500);

fn spec(program: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(program),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::from_program(Path::new(program)),
    }
}

fn wait_exit(handle: &PtyHandle, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = handle.try_exit_status() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// The cursor-position query (DSR, `CSI 6 n`) the pseudoconsole sends before it
/// lets its child print. The pane's reader answers it and removes it from the
/// output.
const CURSOR_QUERY: &[u8] = b"\x1b[6n";

/// Read the pane's output until `needle` appears or `timeout` runs out, and
/// hand back everything read. Answers nothing.
fn read_until(handle: &PtyHandle, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut read: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        match handle.try_read_output() {
            Some(chunk) => {
                read.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&read).contains(needle) {
                    break;
                }
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
    String::from_utf8_lossy(&read).into_owned()
}

/// The whole pseudoconsole round trip in one pane: bytes written reach the
/// child, what the child prints comes back, and closing the pane reports the
/// child's own exit code.
///
/// The two halves are proved apart. `cmd.exe` prints its own banner naming
/// Windows before it reads a byte, so that banner shows only that the console
/// renders. `set /a 6*7` then proves the input landed: the console echoes the
/// typed line, which holds `6*7` and not `42`, so only a child that ran the
/// command can produce `42`. `exit 7` ends the child with 7, which arrives once
/// the console is closed and the reader has read it out.
///
/// Nothing here answers the pseudoconsole's cursor-position query; the pane's
/// reader does. A pane whose query goes unanswered prints nothing, so the
/// banner below fails first.
#[test]
fn a_pane_takes_input_and_prints_the_child_output() {
    let backend = PortablePtyBackend::new();
    let handle = backend
        .spawn(koshi_core::ids::PaneId::new(), spec("cmd.exe", &[]), SIZE)
        .expect("spawn cmd");

    // Nothing written yet: the banner alone says the console renders.
    let banner = read_until(&handle, "Microsoft Windows", Duration::from_secs(15));
    assert!(
        banner.contains("Microsoft Windows"),
        "the pane printed no console banner; it read {banner:?}",
    );
    assert!(
        !banner
            .as_bytes()
            .windows(CURSOR_QUERY.len())
            .any(|window| window == CURSOR_QUERY),
        "the terminal's opening query reached the output; the pane's reader must take it out",
    );

    backend
        .write(handle.pane_id(), b"set /a 6*7\r\n")
        .expect("write to the pane");
    let printed = read_until(&handle, "42", Duration::from_secs(15));
    assert!(
        printed.contains("42"),
        "the line never reached the child, so it printed no answer; it read {printed:?}",
    );

    backend
        .write(handle.pane_id(), b"exit 7\r\n")
        .expect("write to the pane");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(7)),
    );
}

/// Input written in the first moments of a pane still reaches its child.
///
/// Nothing is read back before the write, so it lands while the pane's terminal
/// is still opening.
///
/// `set /p` reads a line, which is what a client writing bytes to a pane
/// produces. `pause` takes a key event instead and is not a test of this path.
#[test]
fn a_line_typed_before_the_pane_is_read_still_reaches_the_child() {
    let backend = PortablePtyBackend::new();
    // The line ends `set /p`, and `cmd.exe` exits with the code it was told, so
    // an exit of 7 is the line having arrived.
    let handle = backend
        .spawn(
            koshi_core::ids::PaneId::new(),
            spec("cmd.exe", &["/C", "set /p x= & exit 7"]),
            SIZE,
        )
        .expect("spawn cmd");

    backend
        .write(handle.pane_id(), b"typed\r")
        .expect("write to the pane");

    assert_eq!(
        wait_exit(&handle, Duration::from_secs(15)),
        Some(ExitStatus::ExitCode(7)),
        "the line never reached the child; the pane read {:?}",
        read_until(&handle, "\u{0}", Duration::from_millis(100)),
    );
}

#[test]
fn force_terminates_a_running_child() {
    let backend = PortablePtyBackend::new();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let handle = backend
        .spawn(
            koshi_core::ids::PaneId::new(),
            spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            SIZE,
        )
        .expect("spawn cmd");
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("force kill");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
}

#[test]
fn tree_terminates_the_job() {
    let backend = PortablePtyBackend::new();
    let handle = backend
        .spawn(
            koshi_core::ids::PaneId::new(),
            spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            SIZE,
        )
        .expect("spawn cmd");
    backend
        .kill(handle.pane_id(), KillPolicy::Tree)
        .expect("tree kill");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
}

#[test]
fn a_graceful_close_does_not_spend_the_grace_window() {
    let backend = PortablePtyBackend::new();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let handle = backend
        .spawn(
            koshi_core::ids::PaneId::new(),
            spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            SIZE,
        )
        .expect("spawn cmd");
    let started = Instant::now();
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::Graceful {
                timeout: GRACEFUL_TIMEOUT_DURATION,
            },
        )
        .expect("graceful kill");
    let took = started.elapsed();
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
    assert!(
        took < KILL_BUDGET,
        "a graceful close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {took:?}",
    );
}

#[test]
fn a_graceful_tree_close_does_not_spend_the_grace_window() {
    let backend = PortablePtyBackend::new();
    // `ping -n 100` blocks ~100s, so only the kill ends it.
    let handle = backend
        .spawn(
            koshi_core::ids::PaneId::new(),
            spec("cmd.exe", &["/C", "ping -n 100 127.0.0.1 >NUL"]),
            SIZE,
        )
        .expect("spawn cmd");
    let started = Instant::now();
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::GracefulTree {
                timeout: GRACEFUL_TIMEOUT_DURATION,
            },
        )
        .expect("graceful tree kill");
    let took = started.elapsed();
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(137)),
    );
    assert!(
        took < KILL_BUDGET,
        "a graceful tree close must not sit through the {GRACEFUL_TIMEOUT_DURATION:?} window; took {took:?}",
    );
}
