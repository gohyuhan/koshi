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
//! On Windows every pane program starts inside two jobs: one shared by the
//! whole process, whose members end when the process ends however it ended,
//! and one of the pane's own, which `force_kill_process_tree` terminates.

#[cfg(unix)]
use nix::{
    errno::Errno,
    sys::signal::{kill, killpg, Signal},
    unistd::Pid,
};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use std::sync::{Arc, OnceLock};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    },
    System::Threading::TerminateProcess,
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

    /// Send `signal` to the child (`kill`) or, when `should_signal_process_group`, to its whole
    /// process group (`killpg`). Any error maps to [`PtyError::Signal`] carrying the errno's name
    /// and description (`ESRCH: No such process`).
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

    /// SIGKILL every process in the child's process group. Nothing is reaped.
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

/// The job every pane program of this process starts inside besides its own
/// pane's job, created on first use and held open until this process exits.
///
/// The job carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Windows terminates
/// every process in such a job when the job's last handle closes, and process
/// exit closes that handle however the process ended — a clean exit, a crash,
/// or a kill from outside.
///
/// `None` once creating it or setting its limit failed; no pane opens then.
#[cfg(windows)]
static SHARED_KILL_ON_CLOSE_JOB: OnceLock<Option<OwnedHandle>> = OnceLock::new();

/// The job of [`SHARED_KILL_ON_CLOSE_JOB`], created on first use.
///
/// `None` when the job could not be created or its limit could not be set.
#[cfg(windows)]
fn get_shared_kill_on_close_job() -> Option<&'static OwnedHandle> {
    SHARED_KILL_ON_CLOSE_JOB
        .get_or_init(|| {
            let shared_job = create_job_object()?;
            let mut job_limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            job_limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `shared_job` is an open job handle, and the call reads
            // the `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` size passed from
            // `job_limits`, which lives for the call.
            let set_information_result = unsafe {
                SetInformationJobObject(
                    shared_job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(job_limits).cast(),
                    u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .expect("the limit block is far below 4 GiB"),
                )
            };
            if set_information_result == 0 {
                return None;
            }
            Some(shared_job)
        })
        .as_ref()
}

/// Create an unnamed job object whose handle no child process inherits, or
/// `None` when `CreateJobObjectW` fails.
#[cfg(windows)]
fn create_job_object() -> Option<OwnedHandle> {
    // SAFETY: `CreateJobObjectW` accepts NULL security attributes and a NULL
    // name.
    let job_handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job_handle.is_null() {
        return None;
    }
    // SAFETY: `job_handle` is the non-NULL handle that `CreateJobObjectW`
    // returned, and nothing else owns it.
    Some(unsafe { OwnedHandle::from_raw_handle(job_handle) })
}

/// A pane's two jobs: the job of [`SHARED_KILL_ON_CLOSE_JOB`], and a job of
/// the pane's own that is created before the pane program starts.
///
/// The pane program starts inside both, and every process it starts belongs
/// to both. The pane's own job carries no `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
/// closing its handle ends no member.
#[cfg(windows)]
pub(crate) struct PaneJob {
    shared_job: &'static OwnedHandle,
    own_job: OwnedHandle,
}

#[cfg(windows)]
impl PaneJob {
    /// Create the pane's own job. The first call in a process also creates the
    /// shared job.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] carrying `the job that ends this process's
    /// panes could not be created` when the shared job does not exist, and
    /// `CreateJobObjectW failed` when the pane's own job cannot be created.
    pub(crate) fn create() -> Result<Self, PtyError> {
        let Some(shared_job) = get_shared_kill_on_close_job() else {
            return Err(PtyError::Signal {
                detail: "the job that ends this process's panes could not be created".to_string(),
            });
        };
        let Some(own_job) = create_job_object() else {
            return Err(PtyError::Signal {
                detail: "CreateJobObjectW failed".to_string(),
            });
        };
        Ok(PaneJob {
            shared_job,
            own_job,
        })
    }

    /// The shared job, then the pane's own job: the order a new process joins
    /// them in. The pane's own job has no member when the process joins it.
    pub(crate) fn list_job_handles(&self) -> [HANDLE; 2] {
        [
            self.shared_job.as_raw_handle(),
            self.own_job.as_raw_handle(),
        ]
    }
}

/// The exit code a pane program ends with when
/// [`PtyChildKillControl::force_kill_child`] or
/// [`PtyChildKillControl::force_kill_process_tree`] terminates it: 137.
#[cfg(windows)]
const TERMINATED_EXIT_CODE: u32 = 137;

/// Terminates a pane program through its process handle and its pane's job.
///
/// `force_kill_child` terminates only the program (`TerminateProcess`);
/// `force_kill_process_tree` terminates every process in the pane's own job
/// (`TerminateJobObject`), the program's descendants included.
#[cfg(windows)]
pub struct PtyChildKillControl {
    process_id: u32,
    /// The handle `CreateProcessW` returned for the program. It names the
    /// exact process object, dead or alive: a process that took over the
    /// program's process ID after it ended is never terminated.
    process_handle: Arc<OwnedHandle>,
    pane_job: PaneJob,
}

#[cfg(windows)]
impl PtyChildKillControl {
    /// Build the kill control of the pane program `process_id`, which started
    /// inside `pane_job`'s jobs, from the program's process handle.
    pub(crate) fn from_pane_job(
        process_id: u32,
        process_handle: Arc<OwnedHandle>,
        pane_job: PaneJob,
    ) -> Self {
        PtyChildKillControl {
            process_id,
            process_handle,
            pane_job,
        }
    }

    /// Terminate only the child process with exit code `137`; its descendants
    /// are left running.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when `TerminateProcess` fails.
    pub fn force_kill_child(&self) -> Result<(), PtyError> {
        let process_handle = self.process_handle.as_raw_handle();
        // SAFETY: `self.process_handle` keeps the process handle open.
        if unsafe { TerminateProcess(process_handle, TERMINATED_EXIT_CODE) } == 0 {
            return Err(PtyError::Signal {
                detail: "TerminateProcess failed".to_string(),
            });
        }
        Ok(())
    }

    /// Terminate every process in the pane's own job with exit code `137`: the
    /// child and every process it started.
    ///
    /// # Errors
    /// Returns [`PtyError::Signal`] when `TerminateJobObject` fails.
    pub fn force_kill_process_tree(&self) -> Result<(), PtyError> {
        let own_job_handle = self.pane_job.own_job.as_raw_handle();
        // SAFETY: `self.pane_job` keeps the job handle open.
        if unsafe { TerminateJobObject(own_job_handle, TERMINATED_EXIT_CODE) } == 0 {
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
