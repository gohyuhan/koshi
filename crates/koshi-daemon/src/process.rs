//! Starting and replacing this crate's own processes.
//!
//! The router, the session server and the pty supervisor all start a helper
//! that has to outlive them, and on Unix all three run serving threads that
//! must not die of SIGPIPE. Those steps live here, once each.

#[cfg(unix)]
use std::process::Command;
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
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Replace this process's running image with `command`. The call returns only
/// when the exec failed, and hands back that error.
///
/// `exec` runs the command's setup steps and then, before calling `execvp`,
/// resets SIGPIPE to `SIG_DFL` in this process. It does that even with no
/// setup step configured on the command (the standard library's
/// `sys/process/unix/unix.rs`, in `do_exec`). A failed exec therefore puts
/// `SIG_IGN` back here before returning, so the process that carries on keeps
/// ignoring the signal a write to a peer that hung up raises.
///
/// The SIGPIPE reset is the only change this function undoes, so a setup step
/// added to `command` must be undone by the caller beside this call.
///
/// A successful exec closes every descriptor the standard library opened
/// close-on-exec at the instant the old image ends, and keeps the process id.
#[cfg(unix)]
pub(crate) fn exec_and_keep_ignoring_sigpipe(command: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    error
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
/// Hands back the same `command`, so the caller spawns it.
#[cfg(windows)]
pub(crate) fn detached(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
}
