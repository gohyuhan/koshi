//! Tests for the platform child-kill control: the pid accessor and real signal
//! delivery to a short-lived child this test spawns and reaps itself.
//!
//! Every test owns the child it signals and never touches a process it did not
//! spawn. Group-kill happy paths (`tree`/`request_process_tree_stop`) are not exercised
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
    fn build_no_such_process_error() -> PtyError {
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
    fn control_reports_the_child_process_id_it_was_built_with() {
        let control = PtyChildKillControl::from_process_id(4321);
        assert_eq!(control.get_child_process_id(), 4321);
    }

    #[test]
    fn request_child_stop_terminates_the_child_with_sigterm() {
        let mut child_process = spawn_sleeper();
        let control = PtyChildKillControl::from_process_id(child_process.id());

        assert_eq!(control.request_child_stop(), StopRequest::Delivered);

        let child_exit_status = child_process.wait().expect("reap child");
        // SIGTERM = 15; sleep does not catch it, so it dies by that signal and
        // carries no exit code.
        assert_eq!(child_exit_status.signal(), Some(15));
        assert_eq!(child_exit_status.code(), None);
    }

    #[test]
    fn a_stop_request_to_a_reaped_child_reports_nothing_received_it() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        // The pid is gone, so `kill` answers ESRCH and nothing was signalled.
        let control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(control.request_child_stop(), StopRequest::NotDelivered);
    }

    #[test]
    fn a_group_stop_request_to_a_reaped_child_reports_nothing_received_it() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        // The pid is gone and never led a group, so `killpg` answers ESRCH.
        let control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(
            control.request_process_tree_stop(),
            StopRequest::NotDelivered
        );
    }

    #[test]
    fn force_kill_child_on_a_reaped_child_reports_no_such_process() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        let control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(
            control.force_kill_child(),
            Err(build_no_such_process_error())
        );
    }

    #[test]
    fn force_kill_process_tree_on_a_reaped_child_reports_no_such_process() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        let control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(
            control.force_kill_process_tree(),
            Err(build_no_such_process_error())
        );
    }

    #[test]
    fn a_group_stop_request_that_finds_no_group_reports_nothing_received_it() {
        let mut child_process = spawn_sleeper();
        let control = PtyChildKillControl::from_process_id(child_process.id());

        // The child is not a process-group leader, so no group carries its pid
        // and `killpg` answers ESRCH. It signals nothing, so the child is still
        // alive to clean up below.
        assert_eq!(
            control.request_process_tree_stop(),
            StopRequest::NotDelivered
        );

        control
            .force_kill_child()
            .expect("clean up the still-live child");
        let child_exit_status = child_process.wait().expect("reap child");
        assert_eq!(child_exit_status.signal(), Some(9));
    }

    #[test]
    fn force_kills_the_child_with_sigkill() {
        let mut child_process = spawn_sleeper();
        let control = PtyChildKillControl::from_process_id(child_process.id());

        control.force_kill_child().expect("SIGKILL delivered");

        let child_exit_status = child_process.wait().expect("reap child");
        // SIGKILL = 9.
        assert_eq!(child_exit_status.signal(), Some(9));
        assert_eq!(child_exit_status.code(), None);
    }

    #[test]
    fn a_group_kill_that_finds_no_group_reports_a_signal_error() {
        let mut child_process = spawn_sleeper();
        let control = PtyChildKillControl::from_process_id(child_process.id());

        // The child is not a process-group leader, so no group has its pid;
        // `killpg` finds nothing (ESRCH) and the failure maps to `Signal`. It
        // kills nothing, so the child is still alive to clean up below.
        assert_eq!(
            control.force_kill_process_tree(),
            Err(build_no_such_process_error())
        );

        control
            .force_kill_child()
            .expect("clean up the still-live child");
        let child_exit_status = child_process.wait().expect("reap child");
        assert_eq!(child_exit_status.signal(), Some(9));
    }

    #[test]
    fn a_pid_that_names_no_child_process_is_never_signalled() {
        // Pid 0 names the caller's own process group, and a pid above
        // `i32::MAX` wraps to a negative id naming an arbitrary group. Both
        // would signal the test runner, so both are refused before any call.
        for process_id in [0, 2_147_483_648, u32::MAX] {
            let control = PtyChildKillControl::from_process_id(process_id);

            assert_eq!(
                control.request_child_stop(),
                StopRequest::NotDelivered,
                "{process_id}"
            );
            assert_eq!(
                control.request_process_tree_stop(),
                StopRequest::NotDelivered,
                "{process_id}"
            );
            assert_eq!(
                control
                    .force_kill_child()
                    .expect_err("nothing is signalled")
                    .to_string(),
                format!("pty signal error: pid {process_id} names no child process")
            );
            assert_eq!(
                control
                    .force_kill_process_tree()
                    .expect_err("nothing is signalled")
                    .to_string(),
                format!("pty signal error: pid {process_id} names no child process")
            );
        }
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
    fn constructor_reports_child_process_id_and_force_terminates_child() {
        let mut child_process = spawn_pinger();
        let process_id = child_process.id();

        let control = PtyChildKillControl::from_process_id_and_handle(
            process_id,
            child_process.as_raw_handle(),
        )
        .expect("construct kill control");
        assert_eq!(control.get_child_process_id(), process_id);

        control.force_kill_child().expect("terminate the child");

        let child_exit_status = child_process.wait().expect("reap child");
        // `force` passes exit code 137 to `TerminateProcess`.
        assert_eq!(child_exit_status.code(), Some(137));
    }

    #[test]
    fn stop_requests_send_nothing_and_answer_not_delivered() {
        let mut child_process = spawn_pinger();
        let control = PtyChildKillControl::from_process_id_and_handle(
            child_process.id(),
            child_process.as_raw_handle(),
        )
        .expect("construct kill control");

        assert_eq!(control.request_child_stop(), StopRequest::NotDelivered);
        assert_eq!(
            control.request_process_tree_stop(),
            StopRequest::NotDelivered
        );

        // Neither request touched the child: it is still alive to terminate.
        control
            .force_kill_process_tree()
            .expect("terminate the job");
        let child_exit_status = child_process.wait().expect("reap child");
        // `tree` passes exit code 137 to `TerminateJobObject`.
        assert_eq!(child_exit_status.code(), Some(137));
    }
}
