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
    use crate::kill::{PtyChildKillControl, StopRequest};
    use nix::errno::Errno;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    /// The error `kill`/`killpg` on a PID or group that does not exist maps to.
    fn no_such_process() -> PtyError {
        PtyError::Signal {
            detail: Errno::ESRCH.to_string(),
        }
    }

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

        assert_eq!(control.request_stop(), StopRequest::Delivered);

        let status = child.wait().expect("reap child");
        // SIGTERM = 15; sleep does not catch it, so it dies by that signal and
        // carries no exit code.
        assert_eq!(status.signal(), Some(15));
        assert_eq!(status.code(), None);
    }

    #[test]
    fn a_stop_request_to_a_reaped_child_reports_nothing_received_it() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        child.kill().expect("kill the child");
        child.wait().expect("reap child");

        // The pid is gone, so `kill` answers ESRCH and nothing was signalled.
        let control = PtyChildKillControl::new(pid);
        assert_eq!(control.request_stop(), StopRequest::NotDelivered);
    }

    #[test]
    fn a_group_stop_request_to_a_reaped_child_reports_nothing_received_it() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        child.kill().expect("kill the child");
        child.wait().expect("reap child");

        // The pid is gone and never led a group, so `killpg` answers ESRCH.
        let control = PtyChildKillControl::new(pid);
        assert_eq!(control.request_stop_tree(), StopRequest::NotDelivered);
    }

    #[test]
    fn force_on_a_reaped_child_reports_no_such_process() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        child.kill().expect("kill the child");
        child.wait().expect("reap child");

        let control = PtyChildKillControl::new(pid);
        assert_eq!(control.force(), Err(no_such_process()));
    }

    #[test]
    fn tree_on_a_reaped_child_reports_no_such_process() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        child.kill().expect("kill the child");
        child.wait().expect("reap child");

        let control = PtyChildKillControl::new(pid);
        assert_eq!(control.tree(), Err(no_such_process()));
    }

    #[test]
    fn a_group_stop_request_that_finds_no_group_reports_nothing_received_it() {
        let mut child = spawn_sleeper();
        let control = PtyChildKillControl::new(child.id());

        // The child is not a process-group leader, so no group carries its pid
        // and `killpg` answers ESRCH. It signals nothing, so the child is still
        // alive to clean up below.
        assert_eq!(control.request_stop_tree(), StopRequest::NotDelivered);

        control.force().expect("clean up the still-live child");
        let status = child.wait().expect("reap child");
        assert_eq!(status.signal(), Some(9));
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
        assert_eq!(control.tree(), Err(no_such_process()));

        control.force().expect("clean up the still-live child");
        let status = child.wait().expect("reap child");
        assert_eq!(status.signal(), Some(9));
    }
}

#[cfg(windows)]
mod windows {
    use crate::kill::{PtyChildKillControl, StopRequest};
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;

    /// A child that runs about 30 seconds; the test terminates it at once.
    fn spawn_pinger() -> std::process::Child {
        Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .spawn()
            .expect("spawn ping child")
    }

    #[test]
    fn new_reports_the_pid_and_force_terminates_the_child() {
        let mut child = spawn_pinger();
        let pid = child.id();

        let control =
            PtyChildKillControl::new(pid, child.as_raw_handle()).expect("construct kill control");
        assert_eq!(control.pid(), pid);

        control.force().expect("terminate the child");

        let status = child.wait().expect("reap child");
        // `force` passes exit code 137 to `TerminateProcess`.
        assert_eq!(status.code(), Some(137));
    }

    #[test]
    fn stop_requests_send_nothing_and_answer_not_delivered() {
        let mut child = spawn_pinger();
        let control = PtyChildKillControl::new(child.id(), child.as_raw_handle())
            .expect("construct kill control");

        assert_eq!(control.request_stop(), StopRequest::NotDelivered);
        assert_eq!(control.request_stop_tree(), StopRequest::NotDelivered);

        // Neither request touched the child: it is still alive to terminate.
        control.tree().expect("terminate the job");
        let status = child.wait().expect("reap child");
        // `tree` passes exit code 137 to `TerminateJobObject`.
        assert_eq!(status.code(), Some(137));
    }
}
