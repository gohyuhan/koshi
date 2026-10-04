//! Starting this crate's own processes, and listing and reaping their
//! children.
//!
//! The router, the session server and the pty supervisor each start a helper
//! that outlives them. On Unix each of the three runs serving threads with
//! SIGPIPE blocked. On Unix the router and the session server list the
//! children an earlier image of their process left, and reap them. Those
//! steps live here, once each.

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

/// Set `command` to start a process that outlives this one: a process group of
/// its own, and input and output going nowhere. On Windows the process also
/// gets no console.
///
/// Hands back the same `command` for the caller to spawn.
pub(crate) fn configure_detached_process(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
}

/// How many process ids the first read of this process's children makes room
/// for. A read that fills that room is made again with twice the room.
#[cfg(target_os = "macos")]
const FIRST_CHILD_PROCESS_ID_BUFFER_COUNT: usize = 64;

/// Every child process of this one, running or exited and not yet reaped: each
/// process under `/proc` that `waitid` names a child of this process. Returns
/// an error when `/proc` cannot be read, or when `waitid` fails with an error
/// other than `ECHILD` or `EINTR`.
///
/// `waitid` runs with `WEXITED | WNOHANG | WNOWAIT`. It answers `0` for a
/// child, running or exited, and leaves an exited child waitable. It answers
/// `ECHILD` for every other process, including one that has exited and been
/// reaped since `/proc` listed it. It reads nothing under `/proc/<pid>`.
///
/// Before → after: `/proc` lists `1`, `4700` and `4821`, and only `4821` is a
/// child of this process → `[4821]`.
#[cfg(target_os = "linux")]
pub(crate) fn list_child_process_ids() -> std::io::Result<Vec<u32>> {
    let mut child_process_ids = Vec::new();
    for process_entry in std::fs::read_dir("/proc")? {
        let process_entry = process_entry?;
        let Some(process_id) = process_entry
            .file_name()
            .to_str()
            .and_then(|process_id_text| process_id_text.parse::<u32>().ok())
        else {
            continue;
        };
        loop {
            // SAFETY: an all-zero `siginfo_t` is valid.
            let mut child_signal_information: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: `waitid` writes only to `child_signal_information`, which
            // lives for the call. `WNOHANG` returns at once, and `WNOWAIT`
            // collects no exit.
            let waitid_return_code = unsafe {
                libc::waitid(
                    libc::P_PID,
                    process_id,
                    &mut child_signal_information,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if waitid_return_code == 0 {
                child_process_ids.push(process_id);
                break;
            }
            let child_wait_error = std::io::Error::last_os_error();
            match child_wait_error.raw_os_error() {
                Some(libc::ECHILD) => break,
                Some(libc::EINTR) => continue,
                _ => return Err(child_wait_error),
            }
        }
    }
    Ok(child_process_ids)
}

/// Every child process of this one, running or exited and not yet reaped, as
/// `proc_listchildpids` names them. A read that fills its buffer is made again
/// with twice the room. Returns an error when the OS refuses the read.
#[cfg(target_os = "macos")]
pub(crate) fn list_child_process_ids() -> std::io::Result<Vec<u32>> {
    let this_process_id = unsafe { libc::getpid() };
    let mut child_process_ids: Vec<libc::pid_t> = vec![0; FIRST_CHILD_PROCESS_ID_BUFFER_COUNT];
    loop {
        let buffer_byte_count = libc::c_int::try_from(std::mem::size_of_val(
            child_process_ids.as_slice(),
        ))
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the child process id buffer is too large",
            )
        })?;
        // SAFETY: the pointer and the byte count describe `child_process_ids`,
        // which the kernel fills with process ids.
        let listed_child_count = unsafe {
            libc::proc_listchildpids(
                this_process_id,
                child_process_ids.as_mut_ptr().cast(),
                buffer_byte_count,
            )
        };
        if listed_child_count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let listed_child_count = usize::try_from(listed_child_count).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the child process count is invalid",
            )
        })?;
        if listed_child_count < child_process_ids.len() {
            let child_process_ids = child_process_ids[..listed_child_count]
                .iter()
                .filter_map(|child_process_id| u32::try_from(*child_process_id).ok())
                .collect();
            return Ok(child_process_ids);
        }
        child_process_ids.resize(child_process_ids.len() * 2, 0);
    }
}

/// Every child process of this one. This platform cannot list child processes.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn list_child_process_ids() -> std::io::Result<Vec<u32>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "child process listing is not supported on this platform",
    ))
}

/// Block until the child `child_process_id` of this process has exited, and
/// reap it. A `waitpid` that a signal interrupts is made again. An id that
/// names no child of this process returns at once.
///
/// Before → after: `child_process_id = 4821`, a child killed by `SIGKILL` →
/// its exit status is collected and it is no longer a zombie.
#[cfg(unix)]
pub(crate) fn wait_for_child_exit(child_process_id: libc::pid_t) {
    loop {
        let mut wait_status: libc::c_int = 0;
        // SAFETY: `waitpid` writes only to `wait_status`, which lives for the
        // call.
        let waited_process_id = unsafe { libc::waitpid(child_process_id, &mut wait_status, 0) };
        let is_wait_interrupted = waited_process_id < 0
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
        if !is_wait_interrupted {
            return;
        }
    }
}
