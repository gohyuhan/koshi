//! Integration tests for the real `portable-pty` backend.
//!
//! These spawn actual child processes inside kernel PTYs and exercise the full
//! reader/watcher wiring: output streamed back over the handle's channel, exit
//! status reported, and resize/write/kill against both live and unknown panes.
//! Unix-only — the Windows backend is a separate target.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_pty::backend::state::{PtyBackend, PtyHandle};
use koshi_pty::error::PtyError;
use koshi_pty::portable::PortablePtyBackend;

/// Standard test window size: 80 columns × 24 rows.
const SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// Serializes PTY creation across the parallel test threads. macOS `openpty(3)`
/// races under concurrent allocation (transient `-6`); koshi itself only ever
/// spawns from its single runtime thread, so gating here matches production and
/// keeps spawning deterministic without serializing the rest of each test.
static PTY_GATE: Mutex<()> = Mutex::new(());

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

/// Spawn a pane through [`PTY_GATE`], panicking on failure.
fn spawn_pane(backend: &PortablePtyBackend, spec: SpawnSpec) -> PtyHandle {
    let _gate = PTY_GATE.lock().expect("pty gate");
    backend
        .spawn(koshi_core::ids::PaneId::new(), spec, SIZE)
        .expect("spawn child")
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

/// True while process `pid` is still around (`kill -0` succeeds). Used to assert
/// a grandchild was reaped — shelling out keeps the test free of a libc dep.
fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn spawn_streams_child_output() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/echo", &["hello"]));
    let out = read_until(&handle, "hello", Duration::from_secs(5));
    assert!(
        out.contains("hello"),
        "expected child output to contain 'hello', got {out:?}"
    );
}

#[test]
fn spawn_without_cwd_inherits_koshis_current_directory() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/pwd", &[]));
    let out = read_until(&handle, "\n", Duration::from_secs(5));
    let child_cwd = PathBuf::from(out.trim())
        .canonicalize()
        .expect("child cwd exists");
    let koshi_cwd = std::env::current_dir()
        .expect("koshi cwd exists")
        .canonicalize()
        .expect("koshi cwd resolves");

    assert_eq!(
        child_cwd, koshi_cwd,
        "a spawn without an explicit cwd must inherit koshi's cwd"
    );
}

#[test]
fn spawn_reports_clean_exit() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/echo", &["bye"]));
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(status, Some(ExitStatus::ExitCode(0)));
}

#[test]
fn spawn_addresses_the_handle_by_the_callers_pane_id() {
    let backend = PortablePtyBackend::new();
    let _gate = PTY_GATE.lock().expect("pty gate");
    // The caller owns pane identity; the handle comes back keyed by that id.
    let pane = PaneId::new();
    let handle = backend
        .spawn(pane, spec("/bin/echo", &["a"]), SIZE)
        .expect("spawn child");
    assert_eq!(handle.pane_id(), pane);
}

#[test]
fn write_reaches_child_and_echoes_back() {
    let backend = PortablePtyBackend::new();
    // `cat` with no args reads stdin and writes it straight back out.
    let handle = spawn_pane(&backend, spec("/bin/cat", &[]));
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
    let handle = spawn_pane(&backend, spec("/bin/cat", &[]));
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
    let handle = spawn_pane(&backend, spec("/bin/cat", &[]));
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("force kill");
    // kill() joins the watcher only after it publishes the exit, so the
    // signal-based status is already waiting on the channel.
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(
        status,
        Some(ExitStatus::Signaled(9)),
        "Force must SIGKILL the child, got {status:?}"
    );
}

#[test]
fn kill_graceful_lets_finished_child_exit_cleanly() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/echo", &["done"]));
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

#[test]
fn exit_status_reports_exact_signal_number() {
    // The child signals *itself* with a known signal, so we can assert the
    // exact number `map_status` recovers — not just "some signal". This is the
    // real check on `sig_no`: portable-pty hands back `strsignal(3)` text that
    // differs by platform ("Terminated" on Linux, "Terminated: 15" on macOS),
    // and the previous `matches!(_, Signaled(_))` test passed even when the
    // mapping produced 0.
    // SIGUSR1/2 pin the greedy-parse regression: their strsignal text ends in a
    // non-signal ordinal ("User defined signal 1"), and their numbers differ by
    // OS (Linux 10/12, macOS/BSD 30/31), so a naive trailing-number parse would
    // wrongly report 1/2 (SIGHUP/SIGINT).
    let (usr1, usr2) = if cfg!(target_os = "linux") {
        (10, 12)
    } else {
        (30, 31)
    };
    let backend = PortablePtyBackend::new();
    for (name, num) in [
        ("HUP", 1),
        ("TERM", 15),
        ("SEGV", 11),
        ("USR1", usr1),
        ("USR2", usr2),
    ] {
        let script = format!("kill -{name} $$");
        let handle = spawn_pane(&backend, spec("/bin/sh", &["-c", script.as_str()]));
        let status = wait_exit(&handle, Duration::from_secs(5));
        assert_eq!(
            status,
            Some(ExitStatus::Signaled(num)),
            "signal {name} should map to {num}, got {status:?}"
        );
    }
}

#[test]
fn force_kills_a_sighup_ignoring_child() {
    let backend = PortablePtyBackend::new();
    // Ignores SIGHUP and blocks in the `read` builtin (no child to orphan).
    // portable-pty's old killer only sent SIGHUP — which this traps — so reaching
    // `Signaled(9)` proves `force` escalates to a real, untrappable SIGKILL.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "trap '' HUP; echo READY; read x"]),
    );
    // Wait until the trap is installed (printed after `trap`) before signalling.
    read_until(&handle, "READY", Duration::from_secs(5));
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("force kill");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(9)),
        "Force must SIGKILL a SIGHUP-ignoring child"
    );
}

#[test]
fn graceful_escalates_to_sigkill_when_sigterm_is_ignored() {
    let backend = PortablePtyBackend::new();
    // SIGTERM is trapped, so the grace window lapses and `kill` must escalate.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "trap '' TERM; echo READY; read x"]),
    );
    // Without this the kill can race shell startup and land before the trap,
    // killing the child with the default SIGTERM disposition instead.
    read_until(&handle, "READY", Duration::from_secs(5));
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::Graceful {
                timeout: Duration::from_millis(300),
            },
        )
        .expect("graceful kill");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(9)),
        "Graceful must escalate to SIGKILL past the window"
    );
}

#[test]
fn graceful_lets_a_cooperative_child_exit_on_sigterm() {
    let backend = PortablePtyBackend::new();
    // No trap: the default SIGTERM disposition terminates it inside the window,
    // so it dies of SIGTERM (15) and is never escalated to SIGKILL (9).
    let handle = spawn_pane(&backend, spec("/bin/sh", &["-c", "echo READY; read x"]));
    read_until(&handle, "READY", Duration::from_secs(5));
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::Graceful {
                timeout: Duration::from_secs(2),
            },
        )
        .expect("graceful kill");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(15)),
        "a cooperative child should exit on SIGTERM, not be SIGKILLed"
    );
}

#[test]
fn tree_reaps_the_grandchild() {
    let backend = PortablePtyBackend::new();
    // The shell backgrounds a long sleep (its child, same process group), prints
    // that sleep's pid, then waits. `Tree` must killpg the whole group and take
    // the sleep with it; `Force` (leader only) would leave it orphaned and alive.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "sleep 300 & echo $!; wait"]),
    );

    let out = read_until(&handle, "\n", Duration::from_secs(5));
    let grandchild: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !grandchild.is_empty(),
        "expected the sleep pid, got {out:?}"
    );
    assert!(
        process_alive(&grandchild),
        "sleep should run before the kill"
    );

    backend
        .kill(handle.pane_id(), KillPolicy::Tree)
        .expect("tree kill");
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(9)),
        "the shell leader should be SIGKILLed by the group kill"
    );

    // Reparent-and-reap of the orphan is asynchronous; poll briefly for it to go.
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(&grandchild) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_alive(&grandchild),
        "Tree must reap the grandchild sleep (pid {grandchild})"
    );
}

#[test]
fn tree_reaps_a_descendant_even_after_the_leader_has_exited() {
    let backend = PortablePtyBackend::new();
    // The leader sets SIGHUP to ignore, then backgrounds a `sleep` in its OWN
    // process group: the child inherits that ignore across fork+exec, so it
    // survives the SIGHUP the kernel sends the foreground group when the session
    // leader exits. The leader prints the sleep's pid and EXITS (no `wait`), so by
    // kill time the watcher has reaped the leader and set `exited`, yet the sleep
    // lives on as a member of the now-leaderless-but-non-empty group. `Tree` must
    // still `killpg` the group and reap it — gating `Tree` on the leader's
    // `exited` flag (which tracks only the leader) would leak the descendant.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &["-c", r#"trap "" HUP; sleep 300 & echo "$! READY""#],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !descendant.is_empty(),
        "expected the child pid, got {out:?}"
    );

    // The leader exits on its own → watcher reaps it → `exited` is set before kill.
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert!(
        status.is_some(),
        "the leader should exit on its own, got {status:?}"
    );
    assert!(
        process_alive(&descendant),
        "the SIGHUP-ignoring child should outlive the leader"
    );

    backend
        .kill(handle.pane_id(), KillPolicy::Tree)
        .expect("tree kill");

    // `killpg` reaps the surviving group member even though the leader is gone.
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(&descendant) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_alive(&descendant),
        "Tree must killpg the group and reap the descendant (pid {descendant}) \
         even after the leader exited"
    );
}

/// Run `kill` on a separate thread and assert it returns within `budget` — a hang
/// fails the test instead of wedging the whole suite.
///
/// The hang these guard against is **Linux** behaviour: a surviving descendant
/// keeps the slave fd open, so the reader thread never sees EOF and a
/// `reader.join()` would block forever. macOS/BSD `revoke()` the controlling
/// terminal when the session leader exits, force-closing that fd in every
/// process, so `kill` never hangs there — these tests pass on macOS and only
/// bite on Linux/CI (where the regression actually manifests).
fn assert_kill_returns(
    backend: PortablePtyBackend,
    pane: PaneId,
    policy: KillPolicy,
    budget: Duration,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = backend.kill(pane, policy);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(budget).is_ok(),
        "kill({policy:?}) hung while a descendant kept the pty open"
    );
}

#[test]
fn force_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let backend = PortablePtyBackend::new();
    // The leader backgrounds a HUP-ignoring child that blocks (holding the slave
    // PTY open), then waits. `Force` kills only the leader; the kernel's
    // session-leader-death SIGHUP — which would otherwise reap the foreground
    // group — is trapped by the child, so it survives with the pty still open and
    // the reader never sees EOF. kill() must NOT join the reader. The child traps
    // HUP and stays alive in a keep-alive loop (it holds the slave via
    // stdout/stderr; `&` points its stdin at /dev/null, which is fine).
    //
    // The child prints its own pid then `READY` on one line *after* installing
    // the trap: parsing waits for `READY` (printed last), so the pid is already
    // buffered — no race between two separate echoes, and the trap is proven up
    // before the kill. (`$$` inside the backgrounded `sh -c` is that child's pid.)
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &[
                "-c",
                r#"sh -c 'trap "" HUP; echo "$$ READY"; while :; do sleep 1; done' & wait"#,
            ],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !descendant.is_empty(),
        "expected the child pid, got {out:?}"
    );
    assert!(
        process_alive(&descendant),
        "the descendant should hold the pty open"
    );

    assert_kill_returns(
        backend,
        handle.pane_id(),
        KillPolicy::Force,
        Duration::from_secs(10),
    );

    // The leader-only kill leaves the descendant running by design; reap it.
    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant])
        .status();
}

#[test]
fn graceful_escalation_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let backend = PortablePtyBackend::new();
    // Leader ignores SIGTERM (so graceful escalates to SIGKILL) and backgrounds a
    // HUP-ignoring child that blocks holding the slave open. Escalation kills only
    // the leader, so the same detach-the-reader rule must apply or kill() blocks.
    // The child prints its own pid then `READY` last, so parsing on `READY` finds
    // the pid already buffered (no two-echo race) and proves the trap is up first.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &[
                "-c",
                r#"trap "" TERM; sh -c 'trap "" HUP; echo "$$ READY"; while :; do sleep 1; done' & wait"#,
            ],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !descendant.is_empty(),
        "expected the child pid, got {out:?}"
    );
    assert!(
        process_alive(&descendant),
        "the descendant should hold the pty open"
    );

    assert_kill_returns(
        backend,
        handle.pane_id(),
        KillPolicy::Graceful {
            timeout: Duration::from_millis(300),
        },
        Duration::from_secs(10),
    );

    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant])
        .status();
}

#[test]
fn graceful_tree_reaps_a_descendant_after_the_leader_exits() {
    let backend = PortablePtyBackend::new();
    // Same shape as `tree_reaps_a_descendant_even_after_the_leader_has_exited`,
    // but through `GracefulTree`: the leader traps SIGHUP, backgrounds a `sleep`
    // that inherits the ignore, prints its pid, and exits (no `wait`), so by kill
    // time the leader is already reaped yet the sleep lives on in the leaderless
    // group. The leader's exit skips the grace phase; the closing group-kill
    // still reaps the descendant.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &["-c", r#"trap "" HUP; sleep 300 & echo "$! READY""#],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !descendant.is_empty(),
        "expected the child pid, got {out:?}"
    );

    let status = wait_exit(&handle, Duration::from_secs(5));
    assert!(
        status.is_some(),
        "the leader should exit on its own, got {status:?}"
    );
    assert!(
        process_alive(&descendant),
        "the SIGHUP-ignoring child should outlive the leader"
    );

    backend
        .kill(
            handle.pane_id(),
            KillPolicy::GracefulTree {
                timeout: Duration::from_secs(2),
            },
        )
        .expect("graceful-tree kill");

    // Reparent-and-reap of the orphan is asynchronous; poll briefly for it to go.
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(&descendant) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_alive(&descendant),
        "GracefulTree must killpg the group and reap the descendant (pid {descendant})"
    );
}

#[test]
fn graceful_tree_stop_request_reaches_a_descendant_in_the_grace_window() {
    let backend = PortablePtyBackend::new();
    // The stop request is group-wide: the `sleep` is backgrounded BEFORE the
    // leader traps SIGTERM (an ignore installed first would be inherited), so
    // it keeps the default disposition while the leader turns TERM-immune and
    // loops forever. `READY` prints after the trap, so by kill time the leader
    // is proven immune and only the `sleep` can react to the stop request —
    // it must die during the grace window, while the leader still holds the
    // kill in its wait phase, before the closing group-kill can fire.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &[
                "-c",
                r#"sleep 300 & pid=$!; trap "" TERM; echo "$pid READY"; while :; do sleep 1; done"#,
            ],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !descendant.is_empty(),
        "expected the child pid, got {out:?}"
    );
    assert!(process_alive(&descendant), "the sleep should be running");

    // Kill on a helper thread: the graceful phase blocks for its full window
    // because the leader never exits on its own.
    let pane_id = handle.pane_id();
    let killer = thread::spawn(move || {
        backend.kill(
            pane_id,
            KillPolicy::GracefulTree {
                timeout: Duration::from_secs(3),
            },
        )
    });

    // The descendant dies well inside the 3s window, while the leader still
    // lives: at this point only the group-wide SIGTERM can have reached it.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while process_alive(&descendant) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_alive(&descendant),
        "the group-wide stop request must reach the descendant (pid {descendant})"
    );

    killer
        .join()
        .expect("kill thread")
        .expect("graceful-tree kill");
}

#[test]
fn graceful_tree_lets_a_finished_child_exit_cleanly() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/echo", &["done"]));
    // Echo exits on its own; confirm that before issuing the kill.
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(status, Some(ExitStatus::ExitCode(0)));
    // The child is already gone: GracefulTree skips the wait and the group-kill
    // is a harmless no-op on the empty group, returning promptly.
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::GracefulTree {
                timeout: Duration::from_secs(2),
            },
        )
        .expect("graceful-tree kill");
}

#[test]
fn graceful_tree_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let backend = PortablePtyBackend::new();
    // Leader and descendant both ignore SIGTERM — the group-wide stop request
    // leaves them running, so the graceful phase waits out its window — and the
    // descendant also ignores SIGHUP and blocks holding the slave open. The
    // final `killpg` reaps the whole group, but kill() must still detach the
    // reader or it blocks while the descendant holds the pty. Prints pid then
    // `READY` last, so parsing on `READY` finds the pid already buffered.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &[
                "-c",
                r#"trap "" TERM; sh -c 'trap "" TERM HUP; echo "$$ READY"; while :; do sleep 1; done' & wait"#,
            ],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(
        !descendant.is_empty(),
        "expected the child pid, got {out:?}"
    );
    assert!(
        process_alive(&descendant),
        "the descendant should hold the pty open"
    );

    assert_kill_returns(
        backend,
        handle.pane_id(),
        KillPolicy::GracefulTree {
            timeout: Duration::from_millis(300),
        },
        Duration::from_secs(10),
    );

    // The group-kill should already have reaped it; belt-and-braces cleanup.
    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant])
        .status();
}
