//! Starting this crate's own processes, and listing and reaping their
//! children.
//!
//! The router, the session server and the pty supervisor each start a helper
//! that outlives them. On Unix each of the three runs serving threads with
//! SIGPIPE blocked. On Unix the router and the session server list the
//! children an earlier image of their process left, and reap them. Those
//! steps live here, once each.

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

/// How many process ids the first read of this process's children makes room
/// for. A read that fills that room is made again with twice the room.
#[cfg(target_os = "macos")]
const FIRST_CHILD_PROCESS_ID_BUFFER_COUNT: usize = 64;

/// Every child process of this one, running or exited and not yet reaped: each
/// process under `/proc` whose `stat` line names this process as its parent. A
/// `/proc` that cannot be read gives an empty list.
///
/// The `stat` line holds the process id, the command name between `(` and the
/// last `)`, the state, and then the parent's process id. Before → after:
/// `4821 (sleep 30) S 4700 …` → `4821` is listed when this process is `4700`.
#[cfg(target_os = "linux")]
pub(crate) fn list_child_process_ids() -> Vec<u32> {
    let this_process_id = std::process::id();
    let Ok(process_entries) = std::fs::read_dir("/proc") else {
        tracing::warn!("the child processes could not be listed");
        return Vec::new();
    };
    process_entries
        .filter_map(Result::ok)
        .filter_map(|process_entry| process_entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|process_id| {
            let Ok(process_stat_line) = std::fs::read_to_string(format!("/proc/{process_id}/stat"))
            else {
                return false;
            };
            let Some((_, fields_after_command_name)) = process_stat_line.rsplit_once(')') else {
                return false;
            };
            let parent_process_id = fields_after_command_name
                .split_whitespace()
                .nth(1)
                .and_then(|parent_process_id_text| parent_process_id_text.parse::<u32>().ok());
            parent_process_id == Some(this_process_id)
        })
        .collect()
}

/// Every child process of this one, running or exited and not yet reaped, as
/// `proc_listchildpids` names them. A read that fills its buffer is made again
/// with twice the room. A read the OS refuses gives an empty list.
#[cfg(target_os = "macos")]
pub(crate) fn list_child_process_ids() -> Vec<u32> {
    let this_process_id = unsafe { libc::getpid() };
    let mut child_process_ids: Vec<libc::pid_t> = vec![0; FIRST_CHILD_PROCESS_ID_BUFFER_COUNT];
    loop {
        let Ok(buffer_byte_count) =
            libc::c_int::try_from(std::mem::size_of_val(child_process_ids.as_slice()))
        else {
            tracing::warn!("the child processes could not be listed");
            return Vec::new();
        };
        // SAFETY: the pointer and the byte count describe `child_process_ids`,
        // which the kernel fills with process ids.
        let listed_child_count = unsafe {
            libc::proc_listchildpids(
                this_process_id,
                child_process_ids.as_mut_ptr().cast(),
                buffer_byte_count,
            )
        };
        let Ok(listed_child_count) = usize::try_from(listed_child_count) else {
            tracing::warn!("the child processes could not be listed");
            return Vec::new();
        };
        if listed_child_count < child_process_ids.len() {
            return child_process_ids[..listed_child_count]
                .iter()
                .filter_map(|child_process_id| u32::try_from(*child_process_id).ok())
                .collect();
        }
        child_process_ids.resize(child_process_ids.len() * 2, 0);
    }
}

/// Every child process of this one. On this platform the list is always
/// empty.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn list_child_process_ids() -> Vec<u32> {
    tracing::warn!("the child processes cannot be listed on this platform");
    Vec::new()
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
