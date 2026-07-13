//! Unit tests for [`ChildGuard`]'s kill-on-drop backstop and the pure
//! status/size conversions. The guard tests spawn a real Unix PTY and are
//! Unix-gated; the conversion tests run on every platform.

use super::*;

/// True while process `pid` is still around (`kill -0` succeeds).
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Launch a long-lived child inside a PTY, returning a guard over it and the
/// child's pid.
#[cfg(unix)]
fn spawn_guarded_sleeper() -> (ChildGuard, u32) {
    let pair = native_pty_system()
        .openpty(to_pp_size(PtySize { cols: 80, rows: 24 }))
        .expect("openpty");
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg("sleep 300");
    let child = ChildGuard::new(pair.slave.spawn_command(cmd).expect("spawn"));
    drop(pair.slave);
    let pid = child.process_id().expect("pid");
    (child, pid)
}

#[cfg(unix)]
#[test]
fn dropping_an_armed_guard_kills_the_child() {
    let (guard, pid) = spawn_guarded_sleeper();
    assert!(process_alive(pid), "child should be running before drop");
    drop(guard);
    // kill is asynchronous; poll briefly for the child to go.
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_alive(pid),
        "an armed guard must kill the child on drop"
    );
}

#[cfg(unix)]
#[test]
fn disarming_leaves_the_child_running() {
    let (guard, pid) = spawn_guarded_sleeper();
    let mut child = guard.disarm();
    assert!(process_alive(pid), "disarming must not kill the child");
    let _ = child.kill(); // clean up the still-running child
}

// `sig_no`, `map_status`, and `to_pp_size` are pure string/struct conversions
// with no platform syscalls, so everything below runs on every platform.

#[test]
fn sig_no_parses_the_macos_colon_number_form() {
    // macOS/BSD `strsignal(3)` text: "<description>: <n>".
    assert_eq!(sig_no("Terminated: 15"), 15);
    assert_eq!(sig_no("Hangup: 1"), 1);
}

#[test]
fn sig_no_parses_the_null_strsignal_fallback_form() {
    // portable-pty's own fallback when `strsignal` returns null.
    assert_eq!(sig_no("Signal 23"), 23);
    assert_eq!(sig_no("Signal 0"), 0);
}

#[test]
fn sig_no_maps_every_known_glibc_bare_description() {
    // Linux/glibc `strsignal(3)` text carries no trailing number at all.
    let cases: &[(&str, i32)] = &[
        ("Hangup", 1),
        ("Interrupt", 2),
        ("Quit", 3),
        ("Illegal instruction", 4),
        ("Trace/breakpoint trap", 5),
        ("Aborted", 6),
        ("Bus error", 7),
        ("Floating point exception", 8),
        ("Killed", 9),
        ("User defined signal 1", 10),
        ("Segmentation fault", 11),
        ("User defined signal 2", 12),
        ("Broken pipe", 13),
        ("Alarm clock", 14),
        ("Terminated", 15),
    ];
    for (desc, want) in cases {
        assert_eq!(sig_no(desc), *want, "sig_no({desc:?})");
    }
}

#[test]
fn sig_no_does_not_greedily_misparse_a_trailing_ordinal_as_a_signal_number() {
    // Regression pin (see the `sig_no` doc comment): "User defined signal 1"
    // ends in the digit `1`, and "User defined signal 2" ends in `2`. A naive
    // "parse the trailing number" implementation would misreport SIGUSR1/2
    // (10/12) as SIGHUP/SIGINT (1/2) — `sig_no("User defined signal 1")`
    // returning `1` instead of `10` is exactly that regression, wrong because
    // the description has no `": "` separator and does not start with
    // `"Signal "`, so it must fall through to the exact-match table, not a
    // trailing-digit scan.
    assert_eq!(sig_no("User defined signal 1"), 10);
    assert_eq!(sig_no("User defined signal 2"), 12);
}

#[test]
fn sig_no_unrecognized_description_is_zero() {
    assert_eq!(sig_no("Unknown Signal Foo"), 0);
    assert_eq!(sig_no(""), 0);
    // Has the "Signal " prefix but no parsable number after it: the
    // `strip_prefix` succeeds, the `.parse::<i32>()` fails, so this must fall
    // through to the exact-match table (which also misses) rather than panic
    // or silently return a non-zero value.
    assert_eq!(sig_no("Signal abc"), 0);
    // Has a ": " separator but the tail isn't numeric either — same
    // fall-through requirement.
    assert_eq!(sig_no("foo: bar"), 0);
}

#[test]
fn to_pp_size_carries_cols_and_rows_and_zeroes_the_pixel_fields() {
    let got = to_pp_size(PtySize { cols: 80, rows: 24 });
    assert_eq!(got.cols, 80);
    assert_eq!(got.rows, 24);
    assert_eq!(got.pixel_width, 0);
    assert_eq!(got.pixel_height, 0);
}

#[test]
fn to_pp_size_carries_boundary_dimensions_unchanged() {
    let got = to_pp_size(PtySize { cols: 0, rows: 0 });
    assert_eq!((got.cols, got.rows), (0, 0));

    let got = to_pp_size(PtySize {
        cols: u16::MAX,
        rows: u16::MAX,
    });
    assert_eq!((got.cols, got.rows), (u16::MAX, u16::MAX));
}

#[test]
fn map_status_maps_a_clean_exit_code() {
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(0)),
        ExitStatus::ExitCode(0)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(137)),
        ExitStatus::ExitCode(137)
    );
}

#[test]
fn map_status_wraps_an_exit_code_above_i32_max_instead_of_panicking() {
    // `s.exit_code() as i32` on a `u32` is an `as` cast, not `try_into`: it
    // wraps rather than panicking or saturating. `u32::MAX` (0xFFFF_FFFF) `as
    // i32` is exactly `-1`, and `i32::MAX as u32 + 1` wraps to `i32::MIN`.
    // Pinning the wrap here means a change to a checked/saturating
    // conversion would be caught as a behavior change, not silently allowed.
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(u32::MAX)),
        ExitStatus::ExitCode(-1)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_exit_code(
            i32::MAX as u32 + 1
        )),
        ExitStatus::ExitCode(i32::MIN)
    );
}

#[test]
fn map_status_maps_a_signal_through_sig_no() {
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal("Terminated")),
        ExitStatus::Signaled(15)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal("Terminated: 15")),
        ExitStatus::Signaled(15)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal(
            "User defined signal 1"
        )),
        ExitStatus::Signaled(10)
    );
    assert_eq!(
        map_status(portable_pty::ExitStatus::with_signal("nonsense")),
        ExitStatus::Signaled(0)
    );
}
