//! Wait for a child process of the test process to exit, and leave the exit
//! uncollected.
//!
//! Example — a test starts `/bin/sh -c "exit 0"`, calls
//! [`wait_until_child_has_exited`](child_exit::wait_until_child_has_exited)
//! with its process id, and the child is a zombie when the call returns: the
//! code under test reaps it.

/// Block until the child `child_process_id` of this process has exited, and
/// leave it unreaped. A `waitid` with `WEXITED | WNOWAIT` names the child once
/// it has exited and collects nothing.
///
/// # Panics
/// Panics when the `waitid` fails, such as with `ECHILD` for a
/// `child_process_id` that names no child of this process, or a child that is
/// already reaped.
pub fn wait_until_child_has_exited(child_process_id: u32) {
    // SAFETY: an all-zero `siginfo_t` is a valid value, and `waitid` writes
    // only to `child_signal_information`, which lives for the call.
    let mut child_signal_information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let waitid_return_code = unsafe {
        libc::waitid(
            libc::P_PID,
            child_process_id,
            &mut child_signal_information,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    assert_eq!(
        waitid_return_code,
        0,
        "child {child_process_id} could not be waited for: {}",
        std::io::Error::last_os_error()
    );
}
