//! Tests for the platform child-kill control: the pid accessor and real signal
//! delivery to a short-lived child this test spawns and reaps itself.
//!
//! Every test owns the child it signals and never touches a process it did not
//! spawn. On Unix, each group request names a pid that leads no process group,
//! or a pid that the control refuses before any call. On Windows,
//! `force_kill_process_tree` ends the per-child job of a spawned child.

use super::*;

#[cfg(unix)]
mod unix {
    use super::*;

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
        let kill_control = PtyChildKillControl::from_process_id(4321);
        assert_eq!(kill_control.get_child_process_id(), 4321);
    }

    #[test]
    fn request_child_stop_terminates_the_child_with_sigterm() {
        let mut child_process = spawn_sleeper();
        let kill_control = PtyChildKillControl::from_process_id(child_process.id());

        assert_eq!(kill_control.request_child_stop(), StopRequest::Delivered);

        let child_exit_status = child_process.wait().expect("reap child");
        // SIGTERM = 15. `sleep` does not catch it: the child dies by signal 15
        // and carries no exit code.
        assert_eq!(child_exit_status.signal(), Some(15));
        assert_eq!(child_exit_status.code(), None);
    }

    #[test]
    fn a_stop_request_to_a_reaped_child_reports_nothing_received_it() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        // The pid is gone: `kill` answers ESRCH and signals nothing.
        let kill_control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(kill_control.request_child_stop(), StopRequest::NotDelivered);
    }

    #[test]
    fn a_group_stop_request_to_a_reaped_child_reports_nothing_received_it() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        // The pid is gone and never led a group: `killpg` answers ESRCH.
        let kill_control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(
            kill_control.request_process_tree_stop(),
            StopRequest::NotDelivered
        );
    }

    #[test]
    fn force_kill_child_on_a_reaped_child_reports_no_such_process() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        let kill_control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(
            kill_control.force_kill_child(),
            Err(build_no_such_process_error())
        );
    }

    #[test]
    fn force_kill_process_tree_on_a_reaped_child_reports_no_such_process() {
        let mut child_process = spawn_sleeper();
        let process_id = child_process.id();
        child_process.kill().expect("kill the child");
        child_process.wait().expect("reap child");

        let kill_control = PtyChildKillControl::from_process_id(process_id);
        assert_eq!(
            kill_control.force_kill_process_tree(),
            Err(build_no_such_process_error())
        );
    }

    #[test]
    fn a_group_stop_request_that_finds_no_group_reports_nothing_received_it() {
        let mut child_process = spawn_sleeper();
        let kill_control = PtyChildKillControl::from_process_id(child_process.id());

        // The child leads no process group: `killpg` answers ESRCH and signals
        // nothing. The child is still alive for the clean-up below.
        assert_eq!(
            kill_control.request_process_tree_stop(),
            StopRequest::NotDelivered
        );

        kill_control
            .force_kill_child()
            .expect("clean up the still-live child");
        let child_exit_status = child_process.wait().expect("reap child");
        assert_eq!(child_exit_status.signal(), Some(9));
    }

    #[test]
    fn force_kills_the_child_with_sigkill() {
        let mut child_process = spawn_sleeper();
        let kill_control = PtyChildKillControl::from_process_id(child_process.id());

        kill_control.force_kill_child().expect("SIGKILL delivered");

        let child_exit_status = child_process.wait().expect("reap child");
        // SIGKILL = 9.
        assert_eq!(child_exit_status.signal(), Some(9));
        assert_eq!(child_exit_status.code(), None);
    }

    #[test]
    fn a_group_kill_that_finds_no_group_reports_a_signal_error() {
        let mut child_process = spawn_sleeper();
        let kill_control = PtyChildKillControl::from_process_id(child_process.id());

        // The child leads no process group: `killpg` answers ESRCH, which maps
        // to `Signal`, and kills nothing. The child is still alive for the
        // clean-up below.
        assert_eq!(
            kill_control.force_kill_process_tree(),
            Err(build_no_such_process_error())
        );

        kill_control
            .force_kill_child()
            .expect("clean up the still-live child");
        let child_exit_status = child_process.wait().expect("reap child");
        assert_eq!(child_exit_status.signal(), Some(9));
    }

    #[test]
    fn a_pid_that_names_no_child_process_is_never_signalled() {
        // Pid 0 names the caller's own process group, and a pid above
        // `i32::MAX` wraps to a negative id that names a group. The control
        // refuses both before any call.
        for process_id in [0, 2_147_483_648, u32::MAX] {
            let kill_control = PtyChildKillControl::from_process_id(process_id);

            assert_eq!(
                kill_control.request_child_stop(),
                StopRequest::NotDelivered,
                "{process_id}"
            );
            assert_eq!(
                kill_control.request_process_tree_stop(),
                StopRequest::NotDelivered,
                "{process_id}"
            );
            assert_eq!(
                kill_control
                    .force_kill_child()
                    .expect_err("nothing is signalled")
                    .to_string(),
                format!("pty signal error: pid {process_id} names no child process")
            );
            assert_eq!(
                kill_control
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
    use super::*;

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

        let kill_control = PtyChildKillControl::from_process_id_and_handle(
            process_id,
            child_process.as_raw_handle(),
        )
        .expect("construct kill control");
        assert_eq!(kill_control.get_child_process_id(), process_id);

        kill_control
            .force_kill_child()
            .expect("terminate the child");

        let child_exit_status = child_process.wait().expect("reap child");
        // `force_kill_child` passes exit code 137 to `TerminateProcess`.
        assert_eq!(child_exit_status.code(), Some(137));
    }

    #[test]
    fn stop_requests_send_nothing_and_answer_not_delivered() {
        let mut child_process = spawn_pinger();
        let kill_control = PtyChildKillControl::from_process_id_and_handle(
            child_process.id(),
            child_process.as_raw_handle(),
        )
        .expect("construct kill control");

        assert_eq!(kill_control.request_child_stop(), StopRequest::NotDelivered);
        assert_eq!(
            kill_control.request_process_tree_stop(),
            StopRequest::NotDelivered
        );

        // Neither request touched the child: it is still alive to terminate.
        kill_control
            .force_kill_process_tree()
            .expect("terminate the job");
        let child_exit_status = child_process.wait().expect("reap child");
        // `force_kill_process_tree` passes exit code 137 to `TerminateJobObject`.
        assert_eq!(child_exit_status.code(), Some(137));
    }
}
