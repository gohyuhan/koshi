//! Integration tests for the real `portable-pty` PTY backend.
//!
//! Each test spawns a real child process inside a kernel PTY and drives it
//! through the PTY handle's channels: output streamed back, exit status reported,
//! and resize/write/kill against both live and unknown panes. Unix only; the
//! Windows PTY backend is tested in `portable_windows.rs`.
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

/// Standard test terminal size: 80 columns × 24 rows.
const STANDARD_PTY_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};

/// Upper bound on a `kill` whose child process has already exited. Such a `kill` skips
/// the grace window it was given, which is at least twice this long in every
/// test that measures it.
const KILL_BUDGET_DURATION: Duration = Duration::from_secs(1);

/// Serializes PTY creation across the parallel test threads. macOS
/// `openpty(3)` fails with a transient `-6` under concurrent allocation.
static PTY_GATE: Mutex<()> = Mutex::new(());

/// Build a spawn spec for `program` with `command_arguments`, inheriting cwd and env.
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

/// Spawn a pane through [`PTY_GATE`], panicking on failure.
fn spawn_pane(pty_backend: &PortablePtyBackend, spawn_spec: SpawnSpec) -> PtyHandle {
    let _pty_creation_guard = PTY_GATE.lock().expect("pty gate");
    pty_backend
        .spawn_pane(PaneId::new(), spawn_spec, STANDARD_PTY_SIZE)
        .expect("spawn child")
}

/// Poll the PTY handle's output channel until `expected_output_text` appears or `timeout_duration`
/// elapses, and return everything read so far as lossy UTF-8.
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

/// Poll for the child's exit status until it arrives or `timeout_duration` elapses.
fn wait_for_pane_exit(pty_handle: &PtyHandle, timeout_duration: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout_duration;
    loop {
        if let Some(status) = pty_handle.try_receive_exit_status() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Returns true while `kill -0 process_id` succeeds.
fn is_process_alive(process_id: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", process_id])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|process_status| process_status.success())
        .unwrap_or(false)
}

/// Extracts every ASCII digit from `child_output_text`: the process id a script printed.
/// Panics when `child_output_text` holds no digit.
fn parse_process_id(child_output_text: &str) -> String {
    let process_id: String = child_output_text
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    assert!(
        !process_id.is_empty(),
        "expected a process id, got {child_output_text:?}"
    );
    process_id
}

/// Poll until process `process_id` exits or `timeout_duration` elapses.
fn wait_until_process_exits(process_id: &str, timeout_duration: Duration) -> bool {
    let deadline = Instant::now() + timeout_duration;
    while is_process_alive(process_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    !is_process_alive(process_id)
}

/// Read the pane's output until `READY` appears, panicking when it never does.
fn wait_for_ready_marker(pty_handle: &PtyHandle) {
    let child_output_text = read_pane_output_until(pty_handle, "READY", Duration::from_secs(5));
    assert!(
        child_output_text.contains("READY"),
        "the child never printed READY: {child_output_text:?}"
    );
}

#[test]
fn spawn_streams_child_output() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/echo", &["hello"]));
    let child_output_text = read_pane_output_until(&pty_handle, "hello", Duration::from_secs(5));
    assert!(
        child_output_text.contains("hello"),
        "expected child output to contain 'hello', got {child_output_text:?}"
    );
}

#[test]
fn spawn_without_working_directory_inherits_koshis_current_directory() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/pwd", &[]));
    let child_output_text = read_pane_output_until(&pty_handle, "\n", Duration::from_secs(5));
    let child_working_directory = PathBuf::from(child_output_text.trim())
        .canonicalize()
        .expect("child cwd exists");
    let koshi_working_directory = std::env::current_dir()
        .expect("koshi cwd exists")
        .canonicalize()
        .expect("koshi cwd resolves");

    assert_eq!(
        child_working_directory, koshi_working_directory,
        "a spawn without an explicit working directory must inherit koshi's working directory"
    );
}

#[test]
fn spawn_with_working_directory_starts_the_child_there() {
    let pty_backend = PortablePtyBackend::new();
    let temporary_directory = tempfile::tempdir().expect("test directory");
    let mut spawn_spec = build_spawn_spec("/bin/pwd", &[]);
    spawn_spec.working_directory = Some(temporary_directory.path().to_path_buf());
    let pty_handle = spawn_pane(&pty_backend, spawn_spec);
    let child_output_text = read_pane_output_until(&pty_handle, "\n", Duration::from_secs(5));
    let child_working_directory = PathBuf::from(child_output_text.trim())
        .canonicalize()
        .expect("child cwd exists");

    assert_eq!(
        child_working_directory,
        temporary_directory
            .path()
            .canonicalize()
            .expect("test directory resolves"),
        "a spawn with an explicit working directory must start the child there"
    );
}

#[test]
fn spawn_environment_variables_reach_the_child() {
    let pty_backend = PortablePtyBackend::new();
    let mut spawn_spec = build_spawn_spec("/bin/sh", &["-c", "echo \"$KOSHI_TEST_ENV\""]);
    spawn_spec
        .environment_variables
        .insert("KOSHI_TEST_ENV".to_string(), "koshi-env-marker".to_string());
    let pty_handle = spawn_pane(&pty_backend, spawn_spec);
    let child_output_text =
        read_pane_output_until(&pty_handle, "koshi-env-marker", Duration::from_secs(5));
    assert!(
        child_output_text.contains("koshi-env-marker"),
        "the spec's environment variables never reached the child, got {child_output_text:?}"
    );
}

#[test]
fn the_koshi_env_overlay_reaches_the_child() {
    // `${PROMPT_EOL_MARK+set}` prints `set` for a variable that exists, and
    // nothing for one that does not: the zsh bootstrap key is empty, so its
    // value alone cannot tell the two apart.
    let pty_backend = PortablePtyBackend::new();
    let mut spawn_spec = build_spawn_spec(
        "/bin/sh",
        &[
            "-c",
            "echo \"T=$TERM C=$COLORTERM P=${PROMPT_EOL_MARK+set} end\"",
        ],
    );
    spawn_spec.shell_kind = ShellKind::Zsh;
    let pty_handle = spawn_pane(&pty_backend, spawn_spec);
    let child_output_text = read_pane_output_until(&pty_handle, "end", Duration::from_secs(5));
    assert!(
        child_output_text.contains("T=xterm-256color C=truecolor P=set end"),
        "koshi's terminal identity and the zsh bootstrap must reach the child, got {child_output_text:?}"
    );
}

#[test]
fn spawn_reports_clean_exit() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/echo", &["bye"]));
    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(exit_status, Some(ExitStatus::ExitCode(0)));
}

#[test]
fn spawn_reports_the_childs_exit_code() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec("/bin/sh", &["-c", "exit 42"]),
    );
    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(exit_status, Some(ExitStatus::ExitCode(42)));
}

#[test]
fn spawn_addresses_the_handle_by_the_callers_pane_id() {
    let pty_backend = PortablePtyBackend::new();
    let _gate = PTY_GATE.lock().expect("pty gate");
    // The caller owns pane identity; the pty_handle comes back keyed by that id.
    let pane_id = PaneId::new();
    let pty_handle = pty_backend
        .spawn_pane(
            pane_id,
            build_spawn_spec("/bin/echo", &["a"]),
            STANDARD_PTY_SIZE,
        )
        .expect("spawn child");
    assert_eq!(pty_handle.get_pane_id(), pane_id);
}

#[test]
fn write_reaches_child_and_echoes_back() {
    let pty_backend = PortablePtyBackend::new();
    // `cat` with no command arguments reads stdin and writes it straight back.
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/cat", &[]));
    pty_backend
        .write_pane_input(pty_handle.get_pane_id(), b"ping\n")
        .expect("write to cat");
    let child_output_text = read_pane_output_until(&pty_handle, "ping", Duration::from_secs(5));
    assert!(
        child_output_text.contains("ping"),
        "expected cat to echo 'ping', got {child_output_text:?}"
    );
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Force)
        .expect("kill cat");
}

#[test]
fn resize_known_pane_is_ok() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/cat", &[]));
    pty_backend
        .resize_pane(
            pty_handle.get_pane_id(),
            PtySize {
                column_count: 120,
                row_count: 40,
            },
        )
        .expect("resize live pane");
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Force)
        .expect("kill cat");
}

#[test]
fn resize_changes_the_window_size_the_child_sees() {
    let pty_backend = PortablePtyBackend::new();
    // `stty size` prints the terminal's `rows cols`. The first print shows the
    // spawn size; the second, after `read` returns, shows the resized size.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec("/bin/sh", &["-c", "stty size; read x; stty size"]),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "24 80", Duration::from_secs(5));
    assert!(
        child_output_text.contains("24 80"),
        "the child did not see the spawn size 80x24, got {child_output_text:?}"
    );

    pty_backend
        .resize_pane(
            pty_handle.get_pane_id(),
            PtySize {
                column_count: 120,
                row_count: 40,
            },
        )
        .expect("resize live pane");
    pty_backend
        .write_pane_input(pty_handle.get_pane_id(), b"\n")
        .expect("write to the pane");
    let child_output_text = read_pane_output_until(&pty_handle, "40 120", Duration::from_secs(5));
    assert!(
        child_output_text.contains("40 120"),
        "the child did not see the resized size 120x40, got {child_output_text:?}"
    );
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(0))
    );
}

#[test]
fn resize_unknown_pane_errs() {
    let pty_backend = PortablePtyBackend::new();
    let unknown_pane_id = PaneId::new();
    assert_eq!(
        pty_backend.resize_pane(unknown_pane_id, STANDARD_PTY_SIZE),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id
        })
    );
}

#[test]
fn write_unknown_pane_errs() {
    let pty_backend = PortablePtyBackend::new();
    let unknown_pane_id = PaneId::new();
    assert_eq!(
        pty_backend.write_pane_input(unknown_pane_id, b"x"),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id
        })
    );
}

#[test]
fn kill_unknown_pane_returns_unknown_pane_error() {
    let pty_backend = PortablePtyBackend::new();
    let unknown_pane_id = PaneId::new();
    assert_eq!(
        pty_backend.kill_pane(unknown_pane_id, KillPolicy::Force),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id
        })
    );
}

#[test]
fn a_closed_pane_is_unknown_to_every_subsequent_call() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/cat", &[]));
    let pane_id = pty_handle.get_pane_id();
    pty_backend
        .kill_pane(pane_id, KillPolicy::Force)
        .expect("first kill");

    assert_eq!(
        pty_backend.kill_pane(pane_id, KillPolicy::Force),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(
        pty_backend.write_pane_input(pane_id, b"x"),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(
        pty_backend.resize_pane(pane_id, STANDARD_PTY_SIZE),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(pty_backend.find_live_working_directory(pane_id), None);
}

#[test]
fn live_working_directory_reports_the_directory_the_child_runs_in() {
    let pty_backend = PortablePtyBackend::new();
    let temporary_directory = tempfile::tempdir().expect("test directory");
    let mut spawn_spec = build_spawn_spec("/bin/sh", &["-c", "echo READY; read x"]);
    spawn_spec.working_directory = Some(temporary_directory.path().to_path_buf());
    let pty_handle = spawn_pane(&pty_backend, spawn_spec);
    // `READY` printed means the shell is running inside `test_directory`.
    wait_for_ready_marker(&pty_handle);

    assert_eq!(
        pty_backend
            .find_live_working_directory(pty_handle.get_pane_id())
            .map(|working_directory| {
                working_directory
                    .canonicalize()
                    .expect("child working directory exists")
            }),
        Some(
            temporary_directory
                .path()
                .canonicalize()
                .expect("test directory resolves")
        )
    );
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Force)
        .expect("kill shell");
}

#[test]
fn live_working_directory_of_an_exited_child_is_none() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/echo", &["gone"]));
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::ExitCode(0))
    );
    assert_eq!(
        pty_backend.find_live_working_directory(pty_handle.get_pane_id()),
        None
    );
}

#[test]
fn kill_force_terminates_running_child() {
    let pty_backend = PortablePtyBackend::new();
    // `cat` blocks reading stdin forever; only a signal ends it.
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/cat", &[]));
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Force)
        .expect("force kill");
    // `kill` joins the watcher, which publishes the exit before it ends, so
    // the status is already on the channel.
    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(
        exit_status,
        Some(ExitStatus::Signaled(9)),
        "Force must SIGKILL the child process, got {exit_status:?}"
    );
}

#[test]
fn kill_graceful_lets_finished_child_exit_cleanly() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/echo", &["done"]));
    // Echo exits on its own; confirm that before issuing the graceful kill.
    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(exit_status, Some(ExitStatus::ExitCode(0)));
    // The child is already gone: Graceful sends no signal and skips the wait.
    let started = Instant::now();
    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::Graceful {
                timeout_duration: Duration::from_secs(2),
            },
        )
        .expect("graceful kill");
    let took = started.elapsed();
    assert!(
        took < KILL_BUDGET_DURATION,
        "a graceful kill of an exited child process sat through the window; took {took:?}"
    );
}

#[test]
fn exit_status_reports_exact_signal_number() {
    // The child signals itself with a known signal, and the status carries
    // that exact number. portable-pty hands back `strsignal(3)` text that
    // differs by platform ("Terminated" on Linux, "Terminated: 15" on macOS).
    // SIGUSR1/2 have text ending in a non-signal ordinal ("User defined
    // signal 1") and numbers that differ by OS (Linux 10/12, macOS/BSD 30/31).
    let (sigusr1_signal_number, sigusr2_signal_number) = if cfg!(target_os = "linux") {
        (10, 12)
    } else {
        (30, 31)
    };
    let pty_backend = PortablePtyBackend::new();
    for (signal_name, signal_number) in [
        ("HUP", 1),
        ("TERM", 15),
        ("SEGV", 11),
        ("USR1", sigusr1_signal_number),
        ("USR2", sigusr2_signal_number),
    ] {
        let script = format!("kill -{signal_name} $$");
        let pty_handle = spawn_pane(
            &pty_backend,
            build_spawn_spec("/bin/sh", &["-c", script.as_str()]),
        );
        let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
        assert_eq!(
            exit_status,
            Some(ExitStatus::Signaled(signal_number)),
            "signal {signal_name} should map to {signal_number}, got {exit_status:?}"
        );
    }
}

#[test]
fn force_kills_a_sighup_ignoring_child() {
    let pty_backend = PortablePtyBackend::new();
    // Ignores SIGHUP and blocks in the `read` builtin (no child to orphan).
    // `Signaled(9)` proves `force` sends an untrappable SIGKILL.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec("/bin/sh", &["-c", "trap '' HUP; echo READY; read x"]),
    );
    // `READY` prints after `trap`, so the trap is installed before the kill.
    wait_for_ready_marker(&pty_handle);
    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Force)
        .expect("force kill");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(9)),
        "Force must SIGKILL a SIGHUP-ignoring child"
    );
}

#[test]
fn graceful_escalates_to_sigkill_when_sigterm_is_ignored() {
    let pty_backend = PortablePtyBackend::new();
    // SIGTERM is trapped, so the grace window lapses and `kill` escalates.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec("/bin/sh", &["-c", "trap '' TERM; echo READY; read x"]),
    );
    // `READY` prints after `trap`, so the trap is installed before the kill.
    wait_for_ready_marker(&pty_handle);
    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::Graceful {
                timeout_duration: Duration::from_millis(300),
            },
        )
        .expect("graceful kill");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(9)),
        "Graceful must escalate to SIGKILL past the window"
    );
}

#[test]
fn graceful_lets_a_cooperative_child_exit_on_sigterm() {
    let pty_backend = PortablePtyBackend::new();
    // No trap: the default SIGTERM disposition ends it inside the window, so
    // it dies of SIGTERM (15) and is never escalated to SIGKILL (9).
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec("/bin/sh", &["-c", "echo READY; read x"]),
    );
    wait_for_ready_marker(&pty_handle);
    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::Graceful {
                timeout_duration: Duration::from_secs(2),
            },
        )
        .expect("graceful kill");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(15)),
        "a cooperative child should exit on SIGTERM, not be SIGKILLed"
    );
}

#[test]
fn tree_reaps_the_grandchild() {
    let pty_backend = PortablePtyBackend::new();
    // The shell backgrounds a long sleep (its child, same process group),
    // prints that sleep's pid, then waits. `Tree` kills the whole group and
    // takes the sleep with it; `Force` kills the leader only.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec("/bin/sh", &["-c", "sleep 300 & echo $!; wait"]),
    );

    let child_output_text = read_pane_output_until(&pty_handle, "\n", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);
    assert!(
        is_process_alive(&descendant_process_id),
        "sleep should run before the kill"
    );

    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Tree)
        .expect("tree kill");
    assert_eq!(
        wait_for_pane_exit(&pty_handle, Duration::from_secs(5)),
        Some(ExitStatus::Signaled(9)),
        "the shell leader should be SIGKILLed by the group kill"
    );

    // The orphan is reparented and reaped asynchronously.
    assert!(
        wait_until_process_exits(&descendant_process_id, Duration::from_secs(3)),
        "Tree must reap the descendant sleep (pid {descendant_process_id})"
    );
}

#[test]
fn tree_reaps_a_descendant_even_after_the_leader_has_exited() {
    let pty_backend = PortablePtyBackend::new();
    // The leader ignores SIGHUP, then backgrounds a `sleep` in its own process
    // group: the sleep inherits the ignore across fork+exec and survives the
    // SIGHUP the kernel sends the foreground group when the session leader
    // exits. The leader prints the sleep's pid and exits (no `wait`), so at
    // kill time the watcher has reaped the leader and set `exited`, and the
    // sleep lives on in the leaderless group. `Tree` still sends `killpg`.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec(
            "/bin/sh",
            &["-c", r#"trap "" HUP; sleep 300 & echo "$! READY""#],
        ),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "READY", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);

    // The leader exits on its own; the watcher reaps it and sets `exited`.
    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(
        exit_status,
        Some(ExitStatus::ExitCode(0)),
        "the leader should exit on its own, got {exit_status:?}"
    );
    assert!(
        is_process_alive(&descendant_process_id),
        "the SIGHUP-ignoring descendant should outlive the leader"
    );

    pty_backend
        .kill_pane(pty_handle.get_pane_id(), KillPolicy::Tree)
        .expect("tree kill");

    assert!(
        wait_until_process_exits(&descendant_process_id, Duration::from_secs(3)),
        "Tree must killpg the group and reap the descendant (pid {descendant_process_id}) \
         even after the leader exited"
    );
}

/// Run `kill` on a separate thread and assert it returns `Ok(())` within
/// `budget`. A hang fails the test instead of wedging the whole suite.
///
/// On Linux a surviving descendant keeps the slave fd open, so the reader
/// thread never sees EOF. macOS/BSD `revoke()` the controlling terminal when
/// the session leader exits, which closes that fd in every process.
fn assert_kill_policy_completes(
    pty_backend: PortablePtyBackend,
    pane_id: PaneId,
    kill_policy: KillPolicy,
    kill_budget_duration: Duration,
) {
    let (kill_result_sender, kill_result_receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = kill_result_sender.send(pty_backend.kill_pane(pane_id, kill_policy));
    });
    assert_eq!(
        kill_result_receiver.recv_timeout(kill_budget_duration),
        Ok(Ok(())),
        "kill({kill_policy:?}) hung while a descendant kept the PTY open"
    );
}

#[test]
fn force_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let pty_backend = PortablePtyBackend::new();
    // The leader backgrounds a HUP-ignoring child that blocks holding the slave
    // PTY open (through stdout/stderr; `&` points its stdin at /dev/null), then
    // waits. `Force` kills only the leader; the child traps the SIGHUP the
    // kernel sends on session-leader death and keeps the pty open, so the
    // reader never sees EOF. `kill` returns without joining the reader.
    //
    // The child prints its own pid and `READY` on one line after installing
    // the trap (`$$` inside the backgrounded `sh -c` is that child's pid), so
    // reading up to `READY` finds the pid already buffered and the trap up.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec(
            "/bin/sh",
            &[
                "-c",
                r#"sh -c 'trap "" HUP; echo "$$ READY"; while :; do sleep 1; done' & wait"#,
            ],
        ),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "READY", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);
    assert!(
        is_process_alive(&descendant_process_id),
        "the descendant should hold the PTY open"
    );

    assert_kill_policy_completes(
        pty_backend,
        pty_handle.get_pane_id(),
        KillPolicy::Force,
        Duration::from_secs(10),
    );

    // The leader-only kill leaves the descendant running; reap it.
    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant_process_id])
        .status();
}

#[test]
fn graceful_escalation_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let pty_backend = PortablePtyBackend::new();
    // The leader ignores SIGTERM (so graceful escalates to SIGKILL) and
    // backgrounds a HUP-ignoring child that blocks holding the slave open.
    // Escalation kills only the leader, and `kill` returns without joining
    // the reader. The child prints its pid and `READY` last, so reading up to
    // `READY` finds the pid already buffered and the trap up.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec(
            "/bin/sh",
            &[
                "-c",
                r#"trap "" TERM; sh -c 'trap "" HUP; echo "$$ READY"; while :; do sleep 1; done' & wait"#,
            ],
        ),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "READY", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);
    assert!(
        is_process_alive(&descendant_process_id),
        "the descendant should hold the PTY open"
    );

    assert_kill_policy_completes(
        pty_backend,
        pty_handle.get_pane_id(),
        KillPolicy::Graceful {
            timeout_duration: Duration::from_millis(300),
        },
        Duration::from_secs(10),
    );

    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant_process_id])
        .status();
}

#[test]
fn graceful_tree_reaps_a_descendant_after_the_leader_exits() {
    let pty_backend = PortablePtyBackend::new();
    // Same shape as `tree_reaps_a_descendant_even_after_the_leader_has_exited`,
    // through `GracefulTree`: the leader traps SIGHUP, backgrounds a `sleep`
    // that inherits the ignore, prints its pid, and exits (no `wait`). At kill
    // time the leader is already reaped and the sleep lives on in the
    // leaderless group. The leader's exit skips the grace phase; the closing
    // group-kill reaps the descendant.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec(
            "/bin/sh",
            &["-c", r#"trap "" HUP; sleep 300 & echo "$! READY""#],
        ),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "READY", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);

    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(
        exit_status,
        Some(ExitStatus::ExitCode(0)),
        "the leader should exit on its own, got {exit_status:?}"
    );
    assert!(
        is_process_alive(&descendant_process_id),
        "the SIGHUP-ignoring child should outlive the leader"
    );

    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::GracefulTree {
                timeout_duration: Duration::from_secs(2),
            },
        )
        .expect("graceful-tree kill");

    assert!(
        wait_until_process_exits(&descendant_process_id, Duration::from_secs(3)),
        "GracefulTree must killpg the group and reap the descendant (pid {descendant_process_id})"
    );
}

#[test]
fn graceful_tree_stop_request_reaches_a_descendant_in_the_grace_window() {
    let pty_backend = PortablePtyBackend::new();
    // The stop request is group-wide. The `sleep` is backgrounded BEFORE the
    // leader traps SIGTERM (an ignore installed first would be inherited), so
    // it keeps the default disposition while the leader is TERM-immune and
    // loops forever. `READY` prints after the trap, so at kill time the leader
    // is immune and only the `sleep` reacts to the stop request: it dies
    // during the grace window, while the leader still holds the kill in its
    // wait phase and before the closing group-kill fires.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec(
            "/bin/sh",
            &[
                "-c",
                r#"sleep 300 & pid=$!; trap "" TERM; echo "$pid READY"; while :; do sleep 1; done"#,
            ],
        ),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "READY", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);
    assert!(
        is_process_alive(&descendant_process_id),
        "the sleep should be running"
    );

    // Kill on a helper thread: the leader never exits on its own, and the
    // graceful phase blocks for its full window.
    let pane_id = pty_handle.get_pane_id();
    let kill_thread = thread::spawn(move || {
        pty_backend.kill_pane(
            pane_id,
            KillPolicy::GracefulTree {
                timeout_duration: Duration::from_secs(3),
            },
        )
    });

    // The descendant dies well inside the 3s window, while the leader still
    // lives: only the group-wide SIGTERM can have reached it.
    assert!(
        wait_until_process_exits(&descendant_process_id, Duration::from_millis(1500)),
        "the group-wide stop request must reach the descendant (pid {descendant_process_id})"
    );

    kill_thread
        .join()
        .expect("kill thread")
        .expect("graceful-tree kill");
}

#[test]
fn graceful_tree_lets_a_finished_child_exit_cleanly() {
    let pty_backend = PortablePtyBackend::new();
    let pty_handle = spawn_pane(&pty_backend, build_spawn_spec("/bin/echo", &["done"]));
    // Echo exits on its own; confirm that before issuing the kill.
    let exit_status = wait_for_pane_exit(&pty_handle, Duration::from_secs(5));
    assert_eq!(exit_status, Some(ExitStatus::ExitCode(0)));
    // The child is already gone: GracefulTree skips the wait, and the
    // group-kill on the empty group is a no-op.
    let kill_started_at = Instant::now();
    pty_backend
        .kill_pane(
            pty_handle.get_pane_id(),
            KillPolicy::GracefulTree {
                timeout_duration: Duration::from_secs(2),
            },
        )
        .expect("graceful-tree kill");
    let kill_elapsed_duration = kill_started_at.elapsed();
    assert!(
        kill_elapsed_duration < KILL_BUDGET_DURATION,
        "a graceful-tree kill of an exited child process sat through the window; took {kill_elapsed_duration:?}"
    );
}

#[test]
fn graceful_tree_does_not_hang_when_a_descendant_keeps_the_pty_open() {
    let pty_backend = PortablePtyBackend::new();
    // Leader and descendant both ignore SIGTERM, so the group-wide stop
    // request leaves them running and the graceful phase waits through its
    // window. The descendant also ignores SIGHUP and blocks holding the slave
    // open. The final `killpg` reaps the whole group, and `kill` returns
    // without joining the reader. The child prints its pid and `READY` last,
    // so reading up to `READY` finds the pid already buffered.
    let pty_handle = spawn_pane(
        &pty_backend,
        build_spawn_spec(
            "/bin/sh",
            &[
                "-c",
                r#"trap "" TERM; sh -c 'trap "" TERM HUP; echo "$$ READY"; while :; do sleep 1; done' & wait"#,
            ],
        ),
    );
    let child_output_text = read_pane_output_until(&pty_handle, "READY", Duration::from_secs(5));
    let descendant_process_id = parse_process_id(&child_output_text);
    assert!(
        is_process_alive(&descendant_process_id),
        "the descendant should hold the PTY open"
    );

    assert_kill_policy_completes(
        pty_backend,
        pty_handle.get_pane_id(),
        KillPolicy::GracefulTree {
            timeout_duration: Duration::from_millis(300),
        },
        Duration::from_secs(10),
    );

    // Kills the descendant if the group-kill left it.
    let _ = std::process::Command::new("kill")
        .args(["-9", &descendant_process_id])
        .status();
}
