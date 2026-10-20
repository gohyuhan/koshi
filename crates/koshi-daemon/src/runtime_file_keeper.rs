//! The thread that holds the runtime files of a server that runs for days:
//! a shared `flock` on each folder that holds one of its files, and an access
//! time [`ACCESS_TIME_LEAD_DURATION`] ahead of the clock on each file, set
//! again every [`KEEPER_PASS_INTERVAL_DURATION`]. Modification times stay as
//! they are.
//!
//! Example: the router's keeper starts at 12:00 on 1 March. It holds a shared
//! lock on `/tmp/koshi-1000` and sets the access time of `router.json`,
//! `router.program`, `router.sock` and `router.lock` to 12:00 on 31 March. At
//! 13:00 it sets them to 13:00 on 31 March.

use std::ffi::CString;
use std::fs::{File, TryLockError};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a keeper waits between two passes over its kept paths: 1 hour.
const KEEPER_PASS_INTERVAL_DURATION: Duration = Duration::from_secs(60 * 60);

/// How far ahead of the clock a keeper pass sets each kept path's access
/// time: 30 days.
const ACCESS_TIME_LEAD_DURATION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A running keeper. Dropping it ends its thread, and every folder lock the
/// thread holds is released before the drop returns.
pub(crate) struct RuntimeFileKeeper {
    /// Sends the message that ends the keeper thread.
    stop_sender: Sender<()>,
    /// The keeper thread, joined on drop. `None` when the thread could not
    /// start.
    keeper_thread: Option<JoinHandle<()>>,
}

impl RuntimeFileKeeper {
    /// Run one keeper pass over `kept_paths` with an access time of
    /// [`ACCESS_TIME_LEAD_DURATION`] after now, then start the thread that
    /// runs the same pass every [`KEEPER_PASS_INTERVAL_DURATION`].
    ///
    /// Every lock the first pass takes is held when this returns. When the
    /// operating system starts no thread, every lock the first pass took is
    /// let go, no further pass runs, and a warning is logged.
    pub(crate) fn start(kept_paths: Vec<PathBuf>) -> Self {
        let mut kept_runtime_files = KeptRuntimeFiles::from_kept_paths(kept_paths);
        kept_runtime_files.run_keeper_pass(SystemTime::now() + ACCESS_TIME_LEAD_DURATION);
        let (stop_sender, stop_receiver) = mpsc::channel::<()>();
        let spawn_result = std::thread::Builder::new()
            .name("koshi-file-keeper".to_string())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) =
                    stop_receiver.recv_timeout(KEEPER_PASS_INTERVAL_DURATION)
                {
                    kept_runtime_files
                        .run_keeper_pass(SystemTime::now() + ACCESS_TIME_LEAD_DURATION);
                }
            });
        let keeper_thread = match spawn_result {
            Ok(keeper_thread) => Some(keeper_thread),
            Err(spawn_error) => {
                tracing::warn!(
                    %spawn_error,
                    "the runtime file keeper could not start; its folder locks are let go"
                );
                None
            }
        };
        Self {
            stop_sender,
            keeper_thread,
        }
    }
}

impl Drop for RuntimeFileKeeper {
    fn drop(&mut self) {
        let _ = self.stop_sender.send(());
        if let Some(keeper_thread) = self.keeper_thread.take() {
            let _ = keeper_thread.join();
        }
    }
}

/// What every keeper pass works on: the kept paths, and the shared lock on
/// each folder that holds one of them.
struct KeptRuntimeFiles {
    /// The files, sockets and folders whose access time each pass sets.
    kept_paths: Vec<PathBuf>,
    /// Each folder holding a kept path that a pass still tries to lock.
    unlocked_directory_paths: Vec<PathBuf>,
    /// Each folder holding a kept path, open with a shared lock on it.
    locked_directories: Vec<File>,
}

impl KeptRuntimeFiles {
    /// The kept paths `kept_paths`, with each folder that holds one of them,
    /// once, still to be locked.
    fn from_kept_paths(kept_paths: Vec<PathBuf>) -> Self {
        let mut unlocked_directory_paths: Vec<PathBuf> = Vec::new();
        for kept_path in &kept_paths {
            let Some(directory_path) = kept_path.parent() else {
                continue;
            };
            if !unlocked_directory_paths
                .iter()
                .any(|unlocked_directory_path| unlocked_directory_path.as_path() == directory_path)
            {
                unlocked_directory_paths.push(directory_path.to_path_buf());
            }
        }
        Self {
            kept_paths,
            unlocked_directory_paths,
            locked_directories: Vec::new(),
        }
    }

    /// One keeper pass:
    ///
    /// 1. Each folder in `unlocked_directory_paths` is opened and given a
    ///    shared `flock`. A folder another open file holds an exclusive lock
    ///    on stays in the list, and the next pass tries it again. A folder
    ///    that cannot be opened or locked for any other reason leaves the
    ///    list, and the failure is logged at warn level.
    /// 2. The access time of each kept path, and of each entry directly inside
    ///    a kept folder, is set to `access_time`, as [`set_access_time`]
    ///    states. A path with nothing at it is passed over. Every other
    ///    failure is logged at warn level.
    fn run_keeper_pass(&mut self, access_time: SystemTime) {
        let mut still_unlocked_directory_paths = Vec::new();
        for directory_path in std::mem::take(&mut self.unlocked_directory_paths) {
            match lock_directory_shared(&directory_path) {
                Ok(locked_directory) => self.locked_directories.push(locked_directory),
                Err(TryLockError::WouldBlock) => {
                    still_unlocked_directory_paths.push(directory_path);
                }
                Err(TryLockError::Error(lock_error)) => {
                    tracing::warn!(
                        directory_path = %directory_path.display(),
                        %lock_error,
                        "a runtime folder could not be locked; its files keep their access times only"
                    );
                }
            }
        }
        self.unlocked_directory_paths = still_unlocked_directory_paths;
        for kept_path in &self.kept_paths {
            set_kept_path_access_time(kept_path, access_time);
        }
    }
}

/// Open the folder at `directory_path` and take a shared `flock` on it, without
/// waiting.
///
/// # Errors
/// [`TryLockError::WouldBlock`] while another open file holds an exclusive
/// lock on the folder, and [`TryLockError::Error`] with the error of the open
/// or of the lock.
fn lock_directory_shared(directory_path: &Path) -> Result<File, TryLockError> {
    let directory = File::open(directory_path).map_err(TryLockError::Error)?;
    directory.try_lock_shared()?;
    Ok(directory)
}

/// Set the access time of `kept_path` to `access_time`, and, when `kept_path`
/// is a folder, that of each entry directly inside it too. A path with nothing
/// at it is passed over. Every other failure is logged at warn level.
fn set_kept_path_access_time(kept_path: &Path, access_time: SystemTime) {
    let kept_path_metadata = match std::fs::symlink_metadata(kept_path) {
        Ok(kept_path_metadata) => kept_path_metadata,
        Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => return,
        Err(metadata_error) => {
            warn_access_time_failure(kept_path, &metadata_error);
            return;
        }
    };
    let mut timed_paths = vec![kept_path.to_path_buf()];
    if kept_path_metadata.is_dir() {
        match std::fs::read_dir(kept_path) {
            Ok(directory_entries) => {
                timed_paths.extend(
                    directory_entries
                        .flatten()
                        .map(|directory_entry| directory_entry.path()),
                );
            }
            Err(read_error) => warn_access_time_failure(kept_path, &read_error),
        }
    }
    for timed_path in &timed_paths {
        match set_access_time(timed_path, access_time) {
            Ok(()) => {}
            Err(set_error) if set_error.kind() == io::ErrorKind::NotFound => {}
            Err(set_error) => warn_access_time_failure(timed_path, &set_error),
        }
    }
}

/// Log at warn level that the access time of `timed_path` could not be set,
/// with `access_time_error`.
fn warn_access_time_failure(timed_path: &Path, access_time_error: &io::Error) {
    tracing::warn!(
        timed_path = %timed_path.display(),
        %access_time_error,
        "the access time of a runtime file could not be set"
    );
}

/// Set the access time of the file, folder, socket or link at `timed_path` to
/// `access_time` with `utimensat`, without following a link. The
/// modification time stays as it is, and the change time becomes now.
///
/// # Errors
/// The error of `utimensat`, or [`io::ErrorKind::InvalidInput`] for a path
/// that holds a NUL byte or an `access_time` before `UNIX_EPOCH`.
fn set_access_time(timed_path: &Path, access_time: SystemTime) -> io::Result<()> {
    let timed_path_bytes = CString::new(timed_path.as_os_str().as_bytes())?;
    let access_time_since_epoch = access_time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let file_times = [
        libc::timespec {
            tv_sec: access_time_since_epoch
                .as_secs()
                .min(libc::time_t::MAX as u64) as libc::time_t,
            tv_nsec: access_time_since_epoch.subsec_nanos() as libc::c_long,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    ];
    // SAFETY: `timed_path_bytes` is a NUL-terminated string and `file_times`
    // holds the two `timespec` values `utimensat` reads. Both live for the
    // call, and `utimensat` writes to neither.
    let set_result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            timed_path_bytes.as_ptr(),
            file_times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if set_result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
