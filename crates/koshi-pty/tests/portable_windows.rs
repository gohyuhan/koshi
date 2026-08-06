//! Windows Job-Object backend integration tests.
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
