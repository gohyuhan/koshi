//! Replacing the running koshi program with another koshi command.

use std::process::Command;

#[cfg(test)]
mod tests;

/// Replace this process's running image with `command`. The call returns only
/// when the exec failed, and hands back that error.
///
/// Before it calls `execvp`, `exec` resets SIGPIPE to `SIG_DFL` in this
/// process, even with no setup step configured on the command, and runs the
/// command's `pre_exec` closures after that reset (the standard library's
/// `sys/process/unix/unix.rs`, in `do_exec`). After a failed exec this
/// function sets SIGPIPE back to `SIG_IGN` before it returns.
///
/// The SIGPIPE reset is the only change this function undoes. The caller
/// undoes every setup step it adds to `command`.
///
/// A successful exec closes every descriptor the standard library opened
/// close-on-exec at the instant the old image ends, and keeps the process id.
pub fn exec_and_keep_ignoring_sigpipe(command: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let exec_error = command.exec();
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    exec_error
}
