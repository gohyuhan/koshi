//! Wait for a child process of the test process to exit, and leave the exit
//! uncollected.
//!
//! Example — a test starts `/bin/sh -c "exit 0"`, calls
//! [`wait_until_child_has_exited`](child_exit::wait_until_child_has_exited)
//! with its process id, and the child is a zombie when the call returns: the
//! code under test reaps it.

use std::time::{Duration, Instant};

const CHILD_EXIT_WAIT_TIMEOUT_DURATION: Duration = Duration::from_secs(10);
const CHILD_EXIT_WAIT_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// Wait up to ten seconds for the child `child_process_id` of this process to
/// exit, and leave it unreaped. `waitid` with `WEXITED | WNOWAIT` collects
/// nothing.
///
/// # Panics
/// Panics when the `waitid` fails, such as with `ECHILD` for a
/// `child_process_id` that names no child of this process or a child that is
/// already reaped. It also panics when the child does not exit within ten
/// seconds.
pub fn wait_until_child_has_exited(child_process_id: u32) {
    wait_until_child_has_exited_with_timeout(child_process_id, CHILD_EXIT_WAIT_TIMEOUT_DURATION);
}

fn wait_until_child_has_exited_with_timeout(child_process_id: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let waitid_child_process_id = libc::id_t::try_from(child_process_id)
        .expect("a child process id fits the wait identifier type");
    loop {
        assert!(
            Instant::now() < deadline,
            "child {child_process_id} did not exit within {timeout:?}"
        );
        // SAFETY: an all-zero `siginfo_t` is valid, and `waitid` writes only
        // to this value, which lives for the call.
        let mut child_signal_information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `child_signal_information` is writable for the call, and
        // `WNOHANG` returns without waiting for the child to exit.
        let waitid_return_code = unsafe {
            libc::waitid(
                libc::P_PID,
                waitid_child_process_id,
                &mut child_signal_information,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if waitid_return_code != 0 {
            let waitid_error = std::io::Error::last_os_error();
            if waitid_error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            panic!("child {child_process_id} could not be waited for: {waitid_error}");
        }
        // SAFETY: `waitid` initialized the siginfo fields after a successful
        // call, and the value remains valid for this read.
        if unsafe { child_signal_information.si_pid() } != 0 {
            return;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(CHILD_EXIT_WAIT_POLL_INTERVAL_DURATION),
        );
    }
}

#[cfg(test)]
mod tests;
