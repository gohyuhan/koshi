//! Tests for the platform child-kill control: the pid accessor and real signal
//! delivery to a short-lived child this test spawns and reaps itself.
//!
//! Every test owns the child it signals and never touches a process it did not
//! spawn. Group-kill happy paths (`tree`/`request_stop_tree`) are not exercised
//! against a spawned child: a plain `Command` child shares the test runner's
//! process group, so a real `killpg` on it would signal the test harness. Those
//! paths only work against a session-leader child, which the backend arranges in
//! production but a unit test cannot create safely.

#[cfg(unix)]
mod unix {
    use crate::error::PtyError;
    use crate::kill::PtyChildKillControl;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    /// A child that sleeps long enough that it never exits on its own before the
    /// test signals it.
    fn spawn_sleeper() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child")
    }

    #[test]
    fn pid_returns_the_pid_the_control_was_built_with() {
        let control = PtyChildKillControl::new(4321);
        assert_eq!(control.pid(), 4321);
    }

    #[test]
    fn request_stop_terminates_the_child_with_sigterm() {
        let mut child = spawn_sleeper();
        let control = PtyChildKillControl::new(child.id());

        control.request_stop().expect("SIGTERM delivered");

        let status = child.wait().expect("reap child");
        // SIGTERM = 15; sleep does not catch it, so it dies by that signal and
        // carries no exit code.
        assert_eq!(status.signal(), Some(15));
        assert_eq!(status.code(), None);
    }

    #[test]
    fn force_kills_the_child_with_sigkill() {
        let mut child = spawn_sleeper();
        let control = PtyChildKillControl::new(child.id());

        control.force().expect("SIGKILL delivered");

        let status = child.wait().expect("reap child");
        // SIGKILL = 9.
        assert_eq!(status.signal(), Some(9));
        assert_eq!(status.code(), None);
    }

    #[test]
    fn a_group_kill_that_finds_no_group_reports_a_signal_error() {
        let mut child = spawn_sleeper();
        let control = PtyChildKillControl::new(child.id());

        // The child is not a process-group leader, so no group has its pid;
        // `killpg` finds nothing (ESRCH) and the failure maps to `Signal`. It
        // kills nothing, so the child is still alive to clean up below.
        let result = control.tree();
        match result {
            Err(PtyError::Signal { detail }) => assert!(!detail.is_empty()),
            other => panic!("expected a signal error, got {other:?}"),
        }

        control.force().expect("clean up the still-live child");
        let status = child.wait().expect("reap child");
        assert_eq!(status.signal(), Some(9));
    }
}

#[cfg(windows)]
mod windows {
    use crate::kill::PtyChildKillControl;
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;

    #[test]
    fn new_reports_the_pid_and_force_terminates_the_child() {
        // `ping -n 30` runs about 30 seconds; the test kills it at once.
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .spawn()
            .expect("spawn ping child");
        let pid = child.id();

        let control =
            PtyChildKillControl::new(pid, child.as_raw_handle()).expect("construct kill control");
        assert_eq!(control.pid(), pid);

        control.force().expect("terminate the child");

        let status = child.wait().expect("reap child");
        // `force` passes exit code 137 to `TerminateProcess`.
        assert_eq!(status.code(), Some(137));
    }
}
