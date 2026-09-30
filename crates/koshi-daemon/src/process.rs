//! Starting this crate's own processes.
//!
//! The router, the session server and the pty supervisor each start a helper
//! that outlives them. On Unix each of the three runs serving threads with
//! SIGPIPE blocked. Those steps live here, once each.

#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(test)]
mod tests;

/// Block SIGPIPE on the calling thread's signal mask.
///
/// The blocked signal stays pending and is discarded when the thread ends; a
/// write to a hung-up peer returns an `EPIPE` error under every process-wide
/// disposition.
#[cfg(unix)]
pub(crate) fn block_sigpipe_on_this_thread() {
    let mut signal_set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signal_set);
        libc::sigaddset(&mut signal_set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &signal_set, std::ptr::null_mut());
    }
}

/// The Win32 `DETACHED_PROCESS` creation flag: the started process gets no
/// console and does not inherit the caller's.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The Win32 `CREATE_NEW_PROCESS_GROUP` creation flag: the started process
/// begins a process group of its own.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Set `command` to start a process that outlives this one: no console, a
/// process group of its own, and input and output going nowhere.
///
/// Hands back the same `command` for the caller to spawn.
#[cfg(windows)]
pub(crate) fn configure_detached_process(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
}
