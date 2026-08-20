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
//! requests split the same way: `request_stop` asks the child to exit,
//! `request_stop_tree` asks the whole group. Both answer with a
//! [`crate::kill::StopRequest`], which says whether anything received the
//! request.
//!
//! On Windows every child also joins one job shared by the whole process, so
//! the panes of a process that dies without closing them die with it.

#[cfg(unix)]
use nix::{
    errno::Errno,
    sys::signal::{kill, killpg, Signal},
    unistd::Pid,
};
#[cfg(windows)]
use std::os::windows::io::RawHandle;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, DuplicateHandle, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    },
    System::Threading::{GetCurrentProcess, TerminateProcess, PROCESS_TERMINATE},
};

use crate::error::PtyError;

/// What became of a request asking a child to exit on its own.
///
/// Callers spend the grace window on `Delivered` and on `Unknown`, and spend
/// none on `NotDelivered`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRequest {
    /// The target received the request.
    Delivered,
    /// Nothing received the request.
    NotDelivered,
    /// Part of the target may have received the request.
    Unknown,
}

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
    ///
    /// Any error means the signal did not arrive: `ESRCH` when the child is
    /// already gone, `EPERM` when this process may not signal it.
    pub fn request_stop(&self) -> StopRequest {
        match kill(Pid::from_raw(self.pid as i32), Signal::SIGTERM) {
            Ok(()) => StopRequest::Delivered,
            Err(_) => StopRequest::NotDelivered,
        }
    }

    /// SIGTERM the child's whole process group, asking every member to exit on
    /// its own.
    ///
    /// `EPERM` reports that at least one member could not be signalled, so the
    /// remaining members may still have received the signal.
    pub fn request_stop_tree(&self) -> StopRequest {
        match killpg(Pid::from_raw(self.pid as i32), Signal::SIGTERM) {
            Ok(()) => StopRequest::Delivered,
            Err(Errno::EPERM) => StopRequest::Unknown,
            Err(_) => StopRequest::NotDelivered,
        }
    }

    /// The PID of the child process this control targets.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

/// Owns a Windows Job Object handle and closes it on drop.
///
/// One of these is created per child, grouping that child and its descendants
/// so [`tree`] can terminate them together. That per-child job carries no
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so closing its handle terminates no
/// member: [`force`] ends the child alone, [`tree`] ends the whole group.
///
/// [`panes_die_with_this_process`] holds one more, with that limit set.
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
// threads, which requires `Send`. The shared job below is read from every
// thread that opens a pane, which requires `Sync`.
#[cfg(windows)]
unsafe impl Send for OwnedJob {}

#[cfg(windows)]
unsafe impl Sync for OwnedJob {}

/// The one job every child of this process joins besides its own.
///
/// `None` once creating it or setting its limit failed; a caller that cannot
/// join it refuses to open the pane.
#[cfg(windows)]
static PANES_DIE_WITH_THIS_PROCESS: OnceLock<Option<OwnedJob>> = OnceLock::new();

/// The job whose closing ends every child of this process, created on first
/// use and held open until this process exits.
///
/// Windows terminates every process in a job carrying
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` when the job's last handle closes, and
/// process exit closes that handle however the process ended — a clean exit, a
/// crash, or a kill from outside.
///
/// `None` when the job could not be created or its limit could not be set.
#[cfg(windows)]
fn panes_die_with_this_process() -> Option<HANDLE> {
    PANES_DIE_WITH_THIS_PROCESS
        .get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let job = OwnedJob(job);

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("the limit block is far below 4 GiB"),
            );
            if set == 0 {
                return None;
            }
            Some(job)
        })
        .as_ref()
        .map(|job| job.0)
}

/// Owns a duplicated handle to the child process and closes it on drop.
///
/// `force` terminates through this handle rather than reopening the PID. The
/// handle names the exact process object, dead or alive, so a PID another
/// process took over after the child exited is never terminated.
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
    /// Join the child to the job that ends with this process, create a job of
    /// its own and join it to that too; descendants join automatically, so
    /// [`tree`](Self::tree) reaps the whole group when it is called.
    ///
    /// The shared job is joined first: `AssignProcessToJobObject` takes a
    /// process that already belongs to a job only into an empty one, and only
    /// the freshly created per-child job is empty.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when either job cannot be created or
    /// joined, so a child that could outlive this process never opens a pane.
    pub fn new(pid: u32, child_handle: RawHandle) -> Result<Self, PtyError> {
        unsafe {
            let Some(shared) = panes_die_with_this_process() else {
                return Err(PtyError::Signal {
                    detail: "the job that ends this process's panes could not be created"
                        .to_string(),
                });
            };
            if AssignProcessToJobObject(shared, child_handle as HANDLE) == 0 {
                return Err(PtyError::Signal {
                    detail: "AssignProcessToJobObject failed for the job that ends this \
                             process's panes"
                        .to_string(),
                });
            }

            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(PtyError::Signal {
                    detail: "CreateJobObjectW failed".to_string(),
                });
            }
            // Owned from here on: every return below closes the handle.
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

    /// Sends nothing and always answers [`StopRequest::NotDelivered`]: the child
    /// cannot be asked to exit on its own, so callers go straight to
    /// [`force`](Self::force).
    pub fn request_stop(&self) -> StopRequest {
        StopRequest::NotDelivered
    }

    /// Sends nothing and always answers [`StopRequest::NotDelivered`]: the
    /// child's process group cannot be asked to exit on its own, so callers go
    /// straight to [`tree`](Self::tree).
    pub fn request_stop_tree(&self) -> StopRequest {
        StopRequest::NotDelivered
    }

    /// The PID of the child process this control targets.
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

#[cfg(test)]
mod tests;
