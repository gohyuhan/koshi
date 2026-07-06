//! Unit tests for [`ChildGuard`]'s kill-on-drop backstop.

use super::*;

/// True while process `pid` is still around (`kill -0` succeeds).
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

#[test]
fn disarming_leaves_the_child_running() {
    let (guard, pid) = spawn_guarded_sleeper();
    let mut child = guard.disarm();
    assert!(process_alive(pid), "disarming must not kill the child");
    let _ = child.kill(); // clean up the still-running child
}
