//! Integration tests for the real `portable-pty` backend.
//!
//! Each test spawns a real child process inside a kernel PTY and drives it
//! through the handle's channels: output streamed back, exit status reported,
//! and resize/write/kill against both live and unknown panes. Unix only; the
//! Windows backend is tested in `portable_windows.rs`.
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

/// Upper bound on a `kill` whose child has already exited. Such a `kill` skips
/// the grace window it was given, which is at least twice this long in every
/// test that measures it.
const KILL_BUDGET: Duration = Duration::from_secs(1);

/// Serializes PTY creation across the parallel test threads. macOS
/// `openpty(3)` fails with a transient `-6` under concurrent allocation.
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
        .spawn(PaneId::new(), spec, SIZE)
        .expect("spawn child")
}

/// Poll the handle's output channel until `needle` appears or `timeout`
/// elapses, and return everything read so far as lossy UTF-8.
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

/// True while `kill -0 pid` succeeds.
fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Every ASCII digit in `out`, concatenated: the pid a script printed.
/// Panics when `out` holds no digit.
fn pid_printed(out: &str) -> String {
    let pid: String = out.chars().filter(char::is_ascii_digit).collect();
    assert!(!pid.is_empty(), "expected a pid, got {out:?}");
    pid
}

/// Poll until process `pid` is gone or `timeout` elapses. True when it is gone.
fn wait_until_gone(pid: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    !process_alive(pid)
}

/// Read the pane's output until `READY` appears, panicking when it never does.
fn wait_ready(handle: &PtyHandle) {
    let out = read_until(handle, "READY", Duration::from_secs(5));
    assert!(
        out.contains("READY"),
        "the child never printed READY: {out:?}"
    );
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
fn spawn_with_cwd_starts_the_child_there() {
    let backend = PortablePtyBackend::new();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut launch = spec("/bin/pwd", &[]);
    launch.cwd = Some(dir.path().to_path_buf());
    let handle = spawn_pane(&backend, launch);
    let out = read_until(&handle, "\n", Duration::from_secs(5));
    let child_cwd = PathBuf::from(out.trim())
        .canonicalize()
        .expect("child cwd exists");

    assert_eq!(
        child_cwd,
        dir.path().canonicalize().expect("temp dir resolves"),
        "a spawn with an explicit cwd must start the child there"
    );
}

#[test]
fn spawn_env_reaches_the_child() {
    let backend = PortablePtyBackend::new();
    let mut launch = spec("/bin/sh", &["-c", "echo \"$KOSHI_TEST_ENV\""]);
    launch
        .env
        .insert("KOSHI_TEST_ENV".to_string(), "koshi-env-marker".to_string());
    let handle = spawn_pane(&backend, launch);
    let out = read_until(&handle, "koshi-env-marker", Duration::from_secs(5));
    assert!(
        out.contains("koshi-env-marker"),
        "the spec's env never reached the child, got {out:?}"
    );
}

#[test]
fn the_koshi_env_overlay_reaches_the_child() {
    // `${PROMPT_EOL_MARK+set}` prints `set` for a variable that exists, and
    // nothing for one that does not: the zsh bootstrap key is empty, so its
    // value alone cannot tell the two apart.
    let backend = PortablePtyBackend::new();
    let mut launch = spec(
        "/bin/sh",
        &[
            "-c",
            "echo \"T=$TERM C=$COLORTERM P=${PROMPT_EOL_MARK+set} end\"",
        ],
    );
    launch.shell_kind = ShellKind::Zsh;
    let handle = spawn_pane(&backend, launch);
    let out = read_until(&handle, "end", Duration::from_secs(5));
    assert!(
        out.contains("T=xterm-256color C=truecolor P=set end"),
        "koshi's terminal identity and the zsh bootstrap must reach the child, got {out:?}"
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
fn spawn_reports_the_childs_exit_code() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/sh", &["-c", "exit 42"]));
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(status, Some(ExitStatus::ExitCode(42)));
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
fn resize_changes_the_window_size_the_child_sees() {
    let backend = PortablePtyBackend::new();
    // `stty size` prints the terminal's `rows cols`. The first print shows the
    // spawn size; the second, after `read` returns, shows the resized size.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "stty size; read x; stty size"]),
    );
    let out = read_until(&handle, "24 80", Duration::from_secs(5));
    assert!(
        out.contains("24 80"),
        "the child did not see the spawn size 80x24, got {out:?}"
    );

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
        .write(handle.pane_id(), b"\n")
        .expect("write to the pane");
    let out = read_until(&handle, "40 120", Duration::from_secs(5));
    assert!(
        out.contains("40 120"),
        "the child did not see the resized size 120x40, got {out:?}"
    );
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(0))
    );
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
fn a_closed_pane_is_unknown_to_every_later_call() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/cat", &[]));
    let pane = handle.pane_id();
    backend.kill(pane, KillPolicy::Force).expect("first kill");

    assert_eq!(
        backend.kill(pane, KillPolicy::Force),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(
        backend.write(pane, b"x"),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(
        backend.resize(pane, SIZE),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(backend.live_cwd(pane), None);
}

#[test]
fn live_cwd_reports_the_directory_the_child_runs_in() {
    let backend = PortablePtyBackend::new();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut launch = spec("/bin/sh", &["-c", "echo READY; read x"]);
    launch.cwd = Some(dir.path().to_path_buf());
    let handle = spawn_pane(&backend, launch);
    // `READY` printed means the shell is running inside `dir`.
    wait_ready(&handle);

    assert_eq!(
        backend
            .live_cwd(handle.pane_id())
            .map(|cwd| cwd.canonicalize().expect("child cwd exists")),
        Some(dir.path().canonicalize().expect("temp dir resolves"))
    );
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("kill shell");
}

#[test]
fn live_cwd_of_an_exited_child_is_none() {
    let backend = PortablePtyBackend::new();
    let handle = spawn_pane(&backend, spec("/bin/echo", &["gone"]));
    assert_eq!(
        wait_exit(&handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(0))
    );
    assert_eq!(backend.live_cwd(handle.pane_id()), None);
}

#[test]
fn kill_force_terminates_running_child() {
    let backend = PortablePtyBackend::new();
    // `cat` blocks reading stdin forever; only a signal ends it.
    let handle = spawn_pane(&backend, spec("/bin/cat", &[]));
    backend
        .kill(handle.pane_id(), KillPolicy::Force)
        .expect("force kill");
    // `kill` joins the watcher, which publishes the exit before it ends, so
    // the status is already on the channel.
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
    // The child is already gone: Graceful sends no signal and skips the wait.
    let started = Instant::now();
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::Graceful {
                timeout: Duration::from_secs(2),
            },
        )
        .expect("graceful kill");
    let took = started.elapsed();
    assert!(
        took < KILL_BUDGET,
        "a graceful kill of an exited child sat through the window; took {took:?}"
    );
}

#[test]
fn exit_status_reports_exact_signal_number() {
    // The child signals itself with a known signal, and the status carries
    // that exact number. portable-pty hands back `strsignal(3)` text that
    // differs by platform ("Terminated" on Linux, "Terminated: 15" on macOS).
    // SIGUSR1/2 have text ending in a non-signal ordinal ("User defined
    // signal 1") and numbers that differ by OS (Linux 10/12, macOS/BSD 30/31).
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
    // `Signaled(9)` proves `force` sends an untrappable SIGKILL.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "trap '' HUP; echo READY; read x"]),
    );
    // `READY` prints after `trap`, so the trap is installed before the kill.
    wait_ready(&handle);
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
    // SIGTERM is trapped, so the grace window lapses and `kill` escalates.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "trap '' TERM; echo READY; read x"]),
    );
    // `READY` prints after `trap`, so the trap is installed before the kill.
    wait_ready(&handle);
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
    // No trap: the default SIGTERM disposition ends it inside the window, so
    // it dies of SIGTERM (15) and is never escalated to SIGKILL (9).
    let handle = spawn_pane(&backend, spec("/bin/sh", &["-c", "echo READY; read x"]));
    wait_ready(&handle);
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
    // The shell backgrounds a long sleep (its child, same process group),
    // prints that sleep's pid, then waits. `Tree` kills the whole group and
    // takes the sleep with it; `Force` kills the leader only.
    let handle = spawn_pane(
        &backend,
        spec("/bin/sh", &["-c", "sleep 300 & echo $!; wait"]),
    );

    let out = read_until(&handle, "\n", Duration::from_secs(5));
    let grandchild = pid_printed(&out);
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

    // The orphan is reparented and reaped asynchronously.
    assert!(
        wait_until_gone(&grandchild, Duration::from_secs(3)),
        "Tree must reap the grandchild sleep (pid {grandchild})"
    );
}

#[test]
fn tree_reaps_a_descendant_even_after_the_leader_has_exited() {
    let backend = PortablePtyBackend::new();
    // The leader ignores SIGHUP, then backgrounds a `sleep` in its own process
    // group: the sleep inherits the ignore across fork+exec and survives the
    // SIGHUP the kernel sends the foreground group when the session leader
    // exits. The leader prints the sleep's pid and exits (no `wait`), so at
    // kill time the watcher has reaped the leader and set `exited`, and the
    // sleep lives on in the leaderless group. `Tree` still sends `killpg`.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &["-c", r#"trap "" HUP; sleep 300 & echo "$! READY""#],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant = pid_printed(&out);

    // The leader exits on its own; the watcher reaps it and sets `exited`.
    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(
        status,
        Some(ExitStatus::ExitCode(0)),
        "the leader should exit on its own, got {status:?}"
    );
    assert!(
        process_alive(&descendant),
        "the SIGHUP-ignoring child should outlive the leader"
    );

    backend
        .kill(handle.pane_id(), KillPolicy::Tree)
        .expect("tree kill");

    assert!(
        wait_until_gone(&descendant, Duration::from_secs(3)),
        "Tree must killpg the group and reap the descendant (pid {descendant}) \
         even after the leader exited"
    );
}

/// Run `kill` on a separate thread and assert it returns `Ok(())` within
/// `budget`. A hang fails the test instead of wedging the whole suite.
///
/// On Linux a surviving descendant keeps the slave fd open, so the reader
/// thread never sees EOF. macOS/BSD `revoke()` the controlling terminal when
/// the session leader exits, which closes that fd in every process.
fn assert_kill_returns(
    backend: PortablePtyBackend,
    pane: PaneId,
    policy: KillPolicy,
    budget: Duration,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(backend.kill(pane, policy));
    });
    assert_eq!(
        rx.recv_timeout(budget),
        Ok(Ok(())),
        "kill({policy:?}) hung while a descendant kept the pty open"
    );
}

#[test]
fn force_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let backend = PortablePtyBackend::new();
    // The leader backgrounds a HUP-ignoring child that blocks holding the slave
    // PTY open (through stdout/stderr; `&` points its stdin at /dev/null), then
    // waits. `Force` kills only the leader; the child traps the SIGHUP the
    // kernel sends on session-leader death and keeps the pty open, so the
    // reader never sees EOF. `kill` returns without joining the reader.
    //
    // The child prints its own pid and `READY` on one line after installing
    // the trap (`$$` inside the backgrounded `sh -c` is that child's pid), so
    // reading up to `READY` finds the pid already buffered and the trap up.
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
    let descendant = pid_printed(&out);
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

    // The leader-only kill leaves the descendant running; reap it.
    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant])
        .status();
}

#[test]
fn graceful_escalation_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let backend = PortablePtyBackend::new();
    // The leader ignores SIGTERM (so graceful escalates to SIGKILL) and
    // backgrounds a HUP-ignoring child that blocks holding the slave open.
    // Escalation kills only the leader, and `kill` returns without joining
    // the reader. The child prints its pid and `READY` last, so reading up to
    // `READY` finds the pid already buffered and the trap up.
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
    let descendant = pid_printed(&out);
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
    // through `GracefulTree`: the leader traps SIGHUP, backgrounds a `sleep`
    // that inherits the ignore, prints its pid, and exits (no `wait`). At kill
    // time the leader is already reaped and the sleep lives on in the
    // leaderless group. The leader's exit skips the grace phase; the closing
    // group-kill reaps the descendant.
    let handle = spawn_pane(
        &backend,
        spec(
            "/bin/sh",
            &["-c", r#"trap "" HUP; sleep 300 & echo "$! READY""#],
        ),
    );
    let out = read_until(&handle, "READY", Duration::from_secs(5));
    let descendant = pid_printed(&out);

    let status = wait_exit(&handle, Duration::from_secs(5));
    assert_eq!(
        status,
        Some(ExitStatus::ExitCode(0)),
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

    assert!(
        wait_until_gone(&descendant, Duration::from_secs(3)),
        "GracefulTree must killpg the group and reap the descendant (pid {descendant})"
    );
}

#[test]
fn graceful_tree_stop_request_reaches_a_descendant_in_the_grace_window() {
    let backend = PortablePtyBackend::new();
    // The stop request is group-wide. The `sleep` is backgrounded BEFORE the
    // leader traps SIGTERM (an ignore installed first would be inherited), so
    // it keeps the default disposition while the leader is TERM-immune and
    // loops forever. `READY` prints after the trap, so at kill time the leader
    // is immune and only the `sleep` reacts to the stop request: it dies
    // during the grace window, while the leader still holds the kill in its
    // wait phase and before the closing group-kill fires.
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
    let descendant = pid_printed(&out);
    assert!(process_alive(&descendant), "the sleep should be running");

    // Kill on a helper thread: the leader never exits on its own, and the
    // graceful phase blocks for its full window.
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
    // lives: only the group-wide SIGTERM can have reached it.
    assert!(
        wait_until_gone(&descendant, Duration::from_millis(1500)),
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
    // The child is already gone: GracefulTree skips the wait, and the
    // group-kill on the empty group is a no-op.
    let started = Instant::now();
    backend
        .kill(
            handle.pane_id(),
            KillPolicy::GracefulTree {
                timeout: Duration::from_secs(2),
            },
        )
        .expect("graceful-tree kill");
    let took = started.elapsed();
    assert!(
        took < KILL_BUDGET,
        "a graceful-tree kill of an exited child sat through the window; took {took:?}"
    );
}

#[test]
fn graceful_tree_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let backend = PortablePtyBackend::new();
    // Leader and descendant both ignore SIGTERM, so the group-wide stop
    // request leaves them running and the graceful phase waits out its
    // window. The descendant also ignores SIGHUP and blocks holding the slave
    // open. The final `killpg` reaps the whole group, and `kill` returns
    // without joining the reader. The child prints its pid and `READY` last,
    // so reading up to `READY` finds the pid already buffered.
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
    let descendant = pid_printed(&out);
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

    // Kills the descendant if the group-kill left it.
    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant])
        .status();
}
