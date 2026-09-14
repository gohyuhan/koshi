//! OS-specific child termination, kept behind one cross-platform type.
//!
//! [`crate::kill::PtyChildKillControl`] exposes the same four operations on every platform:
//! [`force_kill_child`](crate::kill::PtyChildKillControl::force_kill_child),
//! [`force_kill_process_tree`](crate::kill::PtyChildKillControl::force_kill_process_tree),
//! [`request_child_stop`](crate::kill::PtyChildKillControl::request_child_stop), and
//! [`request_process_tree_stop`](crate::kill::PtyChildKillControl::request_process_tree_stop).
//! The signal and Job-Object names behind them stay inside this module.
//!
//! `force_kill_child` targets only the child process (`kill(pid)` /
//! `TerminateProcess`); `force_kill_process_tree` targets the whole group
//! (`killpg` / `TerminateJobObject`). The stop requests split the same way:
//! `request_child_stop` asks the child to exit, `request_process_tree_stop`
//! asks the whole group. Both answer with a
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
/// The child leads its own process group (`portable-pty` runs `setsid`):
/// `force_kill_process_tree` group-kills via `killpg`; `force_kill_child`
/// signals only the leader PID.
#[cfg(unix)]
pub struct PtyChildKillControl {
    process_id: u32,
}

#[cfg(unix)]
impl PtyChildKillControl {
    /// Create a kill control struct for the child process identified by PID.
    pub fn from_process_id(process_id: u32) -> Self {
        PtyChildKillControl { process_id }
    }

    /// The process this control signals, or `None` when the pid names no child.
    ///
    /// `0` names the caller's own process group, and a pid above `i32::MAX`
    /// wraps to a negative id naming an arbitrary process group. Neither is a
    /// child, so neither is signalled.
    fn resolve_target_process_id(&self) -> Option<Pid> {
        let process_id = i32::try_from(self.process_id).ok()?;
        (process_id > 0).then(|| Pid::from_raw(process_id))
    }

    /// Send `signal` to the child (`kill`) or, when `whole_group`, to its whole
    /// process group (`killpg`). Any error maps to [`PtyError::Signal`]
    /// carrying the errno's name and description (`ESRCH: No such process`).
    ///
    /// A pid of `0`, and one above `i32::MAX`, is [`PtyError::Signal`] carrying
    /// `pid <n> names no child process`, and nothing is signalled.
    fn send_signal(
        &self,
        should_signal_process_group: bool,
        signal: Signal,
    ) -> Result<(), PtyError> {
        let Some(process_id) = self.resolve_target_process_id() else {
            return Err(PtyError::Signal {
                detail: format!("pid {} names no child process", self.process_id),
            });
        };
        let signal_result = if should_signal_process_group {
            killpg(process_id, signal)
        } else {
            kill(process_id, signal)
        };
        signal_result.map_err(|signal_error| PtyError::Signal {
            detail: signal_error.to_string(),
        })
    }

    /// SIGKILL the child process (leader only).
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when `kill` fails: `ESRCH` when the child
    /// is already gone, `EPERM` when this process may not signal it.
    pub fn force_kill_child(&self) -> Result<(), PtyError> {
        self.send_signal(false, Signal::SIGKILL)
    }

    /// SIGKILL the child's whole process group, reaping any grandchildren.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when `killpg` fails: `ESRCH` when no
    /// group carries the child's PID, `EPERM` when a member may not be
    /// signalled.
    pub fn force_kill_process_tree(&self) -> Result<(), PtyError> {
        self.send_signal(true, Signal::SIGKILL)
    }

    /// SIGTERM the child, asking it to exit on its own.
    ///
    /// Any error answers [`StopRequest::NotDelivered`]: `ESRCH` when the
    /// child is already gone, `EPERM` when this process may not signal it. A
    /// pid of `0`, and one above `i32::MAX`, answers
    /// [`StopRequest::NotDelivered`] with nothing signalled.
    pub fn request_child_stop(&self) -> StopRequest {
        let Some(process_id) = self.resolve_target_process_id() else {
            return StopRequest::NotDelivered;
        };
        match kill(process_id, Signal::SIGTERM) {
            Ok(()) => StopRequest::Delivered,
            Err(_) => StopRequest::NotDelivered,
        }
    }

    /// SIGTERM the child's whole process group, asking every member to exit on
    /// its own.
    ///
    /// `EPERM` answers [`StopRequest::Unknown`]: it reports that at least one
    /// member could not be signalled, and the remaining members may still
    /// have received the signal. Any other error (`ESRCH` when no group
    /// carries the child's PID) answers [`StopRequest::NotDelivered`], and so
    /// does a pid of `0` or one above `i32::MAX`, with nothing signalled.
    pub fn request_process_tree_stop(&self) -> StopRequest {
        let Some(process_id) = self.resolve_target_process_id() else {
            return StopRequest::NotDelivered;
        };
        match killpg(process_id, Signal::SIGTERM) {
            Ok(()) => StopRequest::Delivered,
            Err(Errno::EPERM) => StopRequest::Unknown,
            Err(_) => StopRequest::NotDelivered,
        }
    }

    /// The PID of the child process this control targets.
    pub fn get_child_process_id(&self) -> u32 {
        self.process_id
    }
}

/// Owns a Windows Job Object handle and closes it on drop.
///
/// One of these is created per child, grouping that child and its descendants
/// so [`force_kill_process_tree`] can terminate them together. That per-child job carries no
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so closing its handle terminates no
/// member: [`force_kill_child`] ends the child alone, [`force_kill_process_tree`]
/// ends the whole group.
///
/// [`panes_die_with_this_process`] holds one more, with that limit set.
///
/// [`force_kill_child`]: PtyChildKillControl::force_kill_child
/// [`force_kill_process_tree`]: PtyChildKillControl::force_kill_process_tree
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

// SAFETY: a job handle may be used and closed from any thread.
#[cfg(windows)]
unsafe impl Send for OwnedJob {}

// SAFETY: a job handle may be used from several threads at once.
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
            let shared_job_handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if shared_job_handle.is_null() {
                return None;
            }
            let shared_job = OwnedJob(shared_job_handle);

            let mut job_limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            job_limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set_information_result = SetInformationJobObject(
                shared_job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(job_limits).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("the limit block is far below 4 GiB"),
            );
            if set_information_result == 0 {
                return None;
            }
            Some(shared_job)
        })
        .as_ref()
        .map(|shared_job| shared_job.0)
}

/// Owns a duplicated handle to the child process and closes it on drop.
///
/// `force` terminates through this handle. The handle names the exact process
/// object, dead or alive; a PID another process took over after the child
/// exited is never terminated.
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

// SAFETY: a process handle may be used and closed from any thread.
#[cfg(windows)]
unsafe impl Send for OwnedHandle {}

/// Terminates a spawned child by process handle and Job Object.
///
/// `force_kill_child` terminates only the child process through its duplicated handle;
/// `force_kill_process_tree` terminates every process in the job (`TerminateJobObject`), reaping
/// the child's descendants.
#[cfg(windows)]
pub struct PtyChildKillControl {
    process_id: u32,
    child_job: OwnedJob,
    process_handle: OwnedHandle,
}

#[cfg(windows)]
impl PtyChildKillControl {
    /// Join the child to the job that ends with this process, then create a
    /// job of its own and join it to that too. Descendants join the per-child
    /// job automatically, and [`force_kill_process_tree`](Self::force_kill_process_tree)
    /// reaps the whole group.
    ///
    /// The shared job is joined first. `AssignProcessToJobObject` takes a
    /// process that already belongs to a job only into an empty job, and only
    /// the freshly created per-child job is empty.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when the shared job does not exist, when
    /// the child cannot join either job, when the per-child job cannot be
    /// created, or when the child's process handle cannot be duplicated. A
    /// child that could outlive this process never opens a pane.
    pub fn from_process_id_and_handle(
        process_id: u32,
        child_handle: RawHandle,
    ) -> Result<Self, PtyError> {
        unsafe {
            let Some(shared_job_handle) = panes_die_with_this_process() else {
                return Err(PtyError::Signal {
                    detail: "the job that ends this process's panes could not be created"
                        .to_string(),
                });
            };
            if AssignProcessToJobObject(shared_job_handle, child_handle as HANDLE) == 0 {
                return Err(PtyError::Signal {
                    detail: "AssignProcessToJobObject failed for the job that ends this \
                             process's panes"
                        .to_string(),
                });
            }

            let child_job_handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if child_job_handle.is_null() {
                return Err(PtyError::Signal {
                    detail: "CreateJobObjectW failed".to_string(),
                });
            }
            // Owned from here on: every return below closes the handle.
            let child_job = OwnedJob(child_job_handle);

            if AssignProcessToJobObject(child_job.0, child_handle as HANDLE) == 0 {
                return Err(PtyError::Signal {
                    detail: "AssignProcessToJobObject failed".to_string(),
                });
            }

            // Duplicate the child handle into one this control owns, carrying
            // only PROCESS_TERMINATE. `force_kill_child` terminates through it; a process
            // that recycled the PID after the child exited is never hit.
            let mut process_handle: HANDLE = std::ptr::null_mut();
            let current_process_handle = GetCurrentProcess();
            if DuplicateHandle(
                current_process_handle,
                child_handle as HANDLE,
                current_process_handle,
                &mut process_handle,
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
                process_id,
                child_job,
                process_handle: OwnedHandle(process_handle),
            })
        }
    }

    /// Terminate only the child process with exit code 137; its descendants
    /// are left running.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when `TerminateProcess` fails.
    pub fn force_kill_child(&self) -> Result<(), PtyError> {
        if unsafe { TerminateProcess(self.process_handle.0, 137) } == 0 {
            return Err(PtyError::Signal {
                detail: "TerminateProcess failed".to_string(),
            });
        }
        Ok(())
    }

    /// Terminate every process in the job with exit code 137, reaping the
    /// child's descendants.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when `TerminateJobObject` fails.
    pub fn force_kill_process_tree(&self) -> Result<(), PtyError> {
        if unsafe { TerminateJobObject(self.child_job.0, 137) } == 0 {
            return Err(PtyError::Signal {
                detail: "TerminateJobObject failed".to_string(),
            });
        }
        Ok(())
    }

    /// Sends nothing and always answers [`StopRequest::NotDelivered`]: a
    /// Windows child cannot be asked to exit on its own.
    pub fn request_child_stop(&self) -> StopRequest {
        StopRequest::NotDelivered
    }

    /// Sends nothing and always answers [`StopRequest::NotDelivered`]: a
    /// Windows job cannot be asked to exit on its own.
    pub fn request_process_tree_stop(&self) -> StopRequest {
        StopRequest::NotDelivered
    }

    /// The PID of the child process this control targets.
    pub fn get_child_process_id(&self) -> u32 {
        self.process_id
    }
}

#[cfg(test)]
mod tests;
