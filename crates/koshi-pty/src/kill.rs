//! OS-specific child termination, kept behind one cross-platform type.
//!
//! [`crate::kill::PtyChildKillControl`] exposes the same four operations on every platform —
//! [`force`](crate::kill::PtyChildKillControl::force), [`tree`](crate::kill::PtyChildKillControl::tree),
//! [`request_stop`](crate::kill::PtyChildKillControl::request_stop), and
//! [`request_stop_tree`](crate::kill::PtyChildKillControl::request_stop_tree) —
//! so the backend's `kill` path stays platform-agnostic. The signal/Job-Object
//! names that make them work are confined to this module.
//!
//! `force` targets only the child process (`kill(pid)` / `TerminateProcess`);
//! `tree` targets the whole group (`killpg` / `TerminateJobObject`). The stop
//! requests split the same way: `request_stop` asks the child to exit
//! (`SIGTERM` / Ctrl-Break), `request_stop_tree` asks the whole group.

#[cfg(unix)]
use nix::{
    sys::signal::{kill, killpg, Signal},
    unistd::Pid,
};
#[cfg(windows)]
use std::os::windows::io::RawHandle;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, DuplicateHandle, HANDLE},
    System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT},
    System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
    System::Threading::{GetCurrentProcess, TerminateProcess, PROCESS_TERMINATE},
};

use crate::error::PtyError;

/// Terminates a spawned child by PID and process group.
///
/// On Unix the child leads its own process group (`portable-pty` runs `setsid`),
/// so `tree` group-kills via `killpg`; `force` signals only the leader PID.
#[cfg(unix)]
pub struct PtyChildKillControl {
    pid: u32,
}

#[cfg(unix)]
impl PtyChildKillControl {
    /// Create a kill control struct for the child process identified by PID.
    pub fn new(pid: u32) -> Self {
        PtyChildKillControl { pid }
    }

    /// Send `signal` to the child (`kill`) or, when `whole_group`, to its whole
    /// process group (`killpg`). The shared delivery behind the four operations.
    fn signal(&self, whole_group: bool, signal: Signal) -> Result<(), PtyError> {
        let pid = Pid::from_raw(self.pid as i32);
        let sent = if whole_group {
            killpg(pid, signal)
        } else {
            kill(pid, signal)
        };
        sent.map_err(|e| PtyError::Signal {
            detail: e.to_string(),
        })
    }

    /// SIGKILL the child process (leader only).
    pub fn force(&self) -> Result<(), PtyError> {
        self.signal(false, Signal::SIGKILL)
    }

    /// SIGKILL the child's whole process group, reaping any grandchildren.
    pub fn tree(&self) -> Result<(), PtyError> {
        self.signal(true, Signal::SIGKILL)
    }

    /// SIGTERM the child, asking it to exit on its own.
    pub fn request_stop(&self) -> Result<(), PtyError> {
        self.signal(false, Signal::SIGTERM)
    }

    /// SIGTERM the child's whole process group, asking every member to exit on
    /// its own.
    pub fn request_stop_tree(&self) -> Result<(), PtyError> {
        self.signal(true, Signal::SIGTERM)
    }

    /// The PID of the child process this control targets.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// Owns a Windows Job Object handle and closes it on drop.
///
/// The job groups the child and its descendants so [`tree`] can terminate them
/// together. It is created without `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so closing
/// the handle does not kill members; this keeps [`force`] (child only) and [`tree`]
/// (whole group) distinct — matching the Unix `kill`/`killpg` split.
///
/// [`force`]: PtyChildKillControl::force
/// [`tree`]: PtyChildKillControl::tree
#[cfg(windows)]
struct OwnedJob(HANDLE);

#[cfg(windows)]
impl Drop for OwnedJob {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

// A raw `HANDLE` is `!Send`, but a job handle is safe to use from any thread and
// the backend keeps `PaneEntry` behind a `Mutex` shared with the reader/watcher
// threads, which requires `Send`.
#[cfg(windows)]
unsafe impl Send for OwnedJob {}

/// Owns a duplicated handle to the child process and closes it on drop.
///
/// `force` terminates through this handle instead of reopening the PID, so once
/// the child has exited a recycled PID can never be killed by mistake — the
/// handle refers to the exact process object, dead or alive.
#[cfg(windows)]
struct OwnedHandle(HANDLE);

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for OwnedHandle {}

/// Terminates a spawned child by process handle and Job Object.
///
/// `force` terminates only the child process via a duplicated, reuse-safe handle;
/// `tree` terminates every process in the job (`TerminateJobObject`), reaping the
/// child's descendants — the Windows analogue of `kill(pid)` vs `killpg(pgid)`.
#[cfg(windows)]
pub struct PtyChildKillControl {
    pid: u32,
    job: OwnedJob,
    process: OwnedHandle,
}

#[cfg(windows)]
impl PtyChildKillControl {
    /// Create a job and assign the child to it; descendants join automatically,
    /// so [`tree`](Self::tree) can later reap the whole group.
    pub fn new(pid: u32, child_handle: RawHandle) -> Result<Self, PtyError> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(PtyError::Signal {
                    detail: "CreateJobObjectW failed".to_string(),
                });
            }
            // Own it now so an early return below still closes the handle.
            let job = OwnedJob(job);

            if AssignProcessToJobObject(job.0, child_handle as HANDLE) == 0 {
                return Err(PtyError::Signal {
                    detail: "AssignProcessToJobObject failed".to_string(),
                });
            }

            // Duplicate the child handle into one we own, carrying only
            // PROCESS_TERMINATE. `force` terminates through this rather than
            // reopening `self.pid`, so it can never hit a process that recycled
            // the PID after the child exited.
            let mut process: HANDLE = std::ptr::null_mut();
            let current = GetCurrentProcess();
            if DuplicateHandle(
                current,
                child_handle as HANDLE,
                current,
                &mut process,
                PROCESS_TERMINATE,
                0,
                0,
            ) == 0
            {
                return Err(PtyError::Signal {
                    detail: "DuplicateHandle failed".to_string(),
                });
            }

            Ok(PtyChildKillControl {
                pid,
                job,
                process: OwnedHandle(process),
            })
        }
    }

    /// Terminate only the child process; its descendants are left running.
    pub fn force(&self) -> Result<(), PtyError> {
        if unsafe { TerminateProcess(self.process.0, 137) } == 0 {
            return Err(PtyError::Signal {
                detail: "TerminateProcess failed".to_string(),
            });
        }
        Ok(())
    }

    /// Terminate every process in the job, reaping the child's descendants.
    pub fn tree(&self) -> Result<(), PtyError> {
        if unsafe { TerminateJobObject(self.job.0, 137) } == 0 {
            return Err(PtyError::Signal {
                detail: "TerminateJobObject failed".to_string(),
            });
        }
        Ok(())
    }

    /// Best-effort Ctrl-Break to the child; callers escalate to `force` if it
    /// does not exit.
    ///
    /// NOTE: `GenerateConsoleCtrlEvent` targets a process group, and a child is
    /// its own group only when spawned with `CREATE_NEW_PROCESS_GROUP` — which
    /// portable-pty's ConPTY spawn does not set and `CommandBuilder` does not
    /// expose. So for these children the call usually does nothing and graceful
    /// effectively becomes wait-then-`force`. (No POSIX signals on Windows;
    /// fixing this needs control over `CreateProcess` we don't have here.)
    pub fn request_stop(&self) -> Result<(), PtyError> {
        if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid) } == 0 {
            return Err(PtyError::Signal {
                detail: "GenerateConsoleCtrlEvent failed".to_string(),
            });
        }
        Ok(())
    }

    /// Best-effort Ctrl-Break to the child's process group; callers escalate to
    /// [`tree`](Self::tree) if members do not exit.
    ///
    /// `GenerateConsoleCtrlEvent` already addresses a process group, so this
    /// shares [`request_stop`](Self::request_stop)'s delivery — including its
    /// NOTE about children not spawned with `CREATE_NEW_PROCESS_GROUP`.
    pub fn request_stop_tree(&self) -> Result<(), PtyError> {
        self.request_stop()
    }

    /// The PID of the child process this control targets.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}
