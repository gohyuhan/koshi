//! Integration tests for the real `portable-pty` backend.
//!
//! These spawn actual child processes inside kernel PTYs and exercise the full
//! reader/watcher wiring: output streamed back over the handle's channel, exit
//! status reported, and resize/write/kill against both live and unknown panes.
//! Unix-only — the Windows backend is a separate target.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use tile_core::ids::PaneId;
use tile_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use tile_pty::backend::state::{PtyBackend, PtyHandle};
use tile_pty::error::PtyError;
use tile_pty::portable::PortablePtyBackend;

/// Standard test window size.
const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// Build a spawn spec for `program` with `args`, inheriting cwd and env.
fn spec(program: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from(program),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::from_program(Path::new(program)),
    }
}

/// Poll the handle's output channel until `needle` appears or `timeout` elapses,
/// returning everything accumulated (lossy UTF-8) for the assertion/diagnostics.
fn read_until(handle: &PtyHandle, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut acc: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        match handle.try_read_output() {
            Some(chunk) => {
                acc.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&acc).contains(needle) {
                    break;
                }
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
    String::from_utf8_lossy(&acc).into_owned()
}

/// Poll for the child's exit status until it arrives or `timeout` elapses.
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
fn spawn_streams_child_output() {
    let backend = PortablePtyBackend::new();
    let handle = backend
        .spawn(spec("/bin/echo", &["hello"]), SIZE)
        .expect("spawn echo");
    let out = read_until(&handle, "hello", Duration::from_secs(5));
    assert!(
        out.contains("hello"),
        "expected child output to contain 'hello', got {out:?}"
    );
}

#[test]
fn spawn_reports_clean_exit() {
    let backend = PortablePtyBackend::new();
    let handle = backend
        .spawn(spec("/bin/echo", &["bye"]), SIZE)
        .expect("spawn echo");
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(status, Some(ExitStatus::ExitCode(0)));
}

#[test]
fn spawn_mints_unique_pane_ids() {
    let backend = PortablePtyBackend::new();
    let a = backend
        .spawn(spec("/bin/echo", &["a"]), SIZE)
        .expect("spawn a");
    let b = backend
        .spawn(spec("/bin/echo", &["b"]), SIZE)
        .expect("spawn b");
    assert_ne!(a.pane_id(), b.pane_id());
}

#[test]
fn write_reaches_child_and_echoes_back() {
    let backend = PortablePtyBackend::new();
    // `cat` with no args reads stdin and writes it straight back out.
    let handle = backend
        .spawn(spec("/bin/cat", &[]), SIZE)
        .expect("spawn cat");
    backend
        .write(handle.pane_id(), b"ping\n")
        .expect("write to cat");
    let out = read_until(&handle, "ping", Duration::from_secs(5));
    assert!(
        out.contains("ping"),
        "expected cat to echo 'ping', got {out:?}"
    );
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("kill cat");
}

#[test]
fn resize_known_pane_is_ok() {
    let backend = PortablePtyBackend::new();
    let handle = backend
        .spawn(spec("/bin/cat", &[]), SIZE)
        .expect("spawn cat");
    backend
        .resize(
            handle.pane_id(),
            PtySize {
                cols: 120,
                rows: 40,
            },
        )
        .expect("resize live pane");
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("kill cat");
}

#[test]
fn resize_unknown_pane_errs() {
    let backend = PortablePtyBackend::new();
    let ghost = PaneId::new();
    assert_eq!(
        backend.resize(ghost, SIZE),
        Err(PtyError::UnknownPane { pane: ghost })
    );
}

#[test]
fn write_unknown_pane_errs() {
    let backend = PortablePtyBackend::new();
    let ghost = PaneId::new();
    assert_eq!(
        backend.write(ghost, b"x"),
        Err(PtyError::UnknownPane { pane: ghost })
    );
}

#[test]
fn kill_unknown_pane_errs() {
    let backend = PortablePtyBackend::new();
    let ghost = PaneId::new();
    assert_eq!(
        backend.kill(ghost, KillPolicy::Force),
        Err(PtyError::UnknownPane { pane: ghost })
    );
}

#[test]
fn kill_force_terminates_running_child() {
    let backend = PortablePtyBackend::new();
    // `cat` blocks reading stdin forever; only a signal ends it.
    let handle = backend
        .spawn(spec("/bin/cat", &[]), SIZE)
        .expect("spawn cat");
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("force kill");
    // kill() joins the watcher only after it publishes the exit, so the
    // signal-based status is already waiting on the channel.
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert!(
        matches!(status, Some(ExitStatus::Signaled(_))),
        "expected a signal-based exit, got {status:?}"
    );
}

#[test]
fn kill_graceful_lets_finished_child_exit_cleanly() {
    let backend = PortablePtyBackend::new();
    let handle = backend
        .spawn(spec("/bin/echo", &["done"]), SIZE)
        .expect("spawn echo");
    // Echo exits on its own; confirm that before issuing the graceful kill.
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(status, Some(ExitStatus::ExitCode(0)));
    // Graceful sees the already-set `exited` flag and skips SIGKILL entirely,
    // returning promptly without waiting out the timeout.
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::Graceful {
                timeout: Duration::from_secs(2),
            },
        )
        .expect("graceful kill");
}
