//! Atomic file replacement. Readers see the complete old file or the complete
//! new file, never a torn middle.
//!
//! [`write_atomic`] uses [`tempfile`] to create a uniquely named temp file
//! beside the target, calls `sync_all` on that temp, replaces the target with
//! the platform's atomic replacement operation, and calls `sync_all` on the
//! target directory on Unix. The temp starts private with mode `0600` on Unix.
//! Exclusive creation does not follow a symlink or truncate an existing name.
//!
//! Windows does not sync the directory. It retries replacement errors
//! `ERROR_ACCESS_DENIED` (5) and `ERROR_SHARING_VIOLATION` (32), up to 25
//! attempts.
//!
//! The target directory is not protected. A caller that can write that
//! directory can replace the target directly. koshi uses user-private
//! directories.
//!
//! Normal error paths drop the temp file. On Unix, a hard process termination
//! can leave the named temp file behind.
//!
//! Example: `write_atomic("keybinding.kdl", new)` stages a private temp beside
//! `keybinding.kdl`, syncs it, and replaces the target. A crash before the
//! replacement leaves the old file whole; partial bytes stay in the temp
//! sibling. A hard kill on Unix can leave that private sibling behind.

use std::fs;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::error::StorageError;

#[cfg(test)]
mod tests;

/// Writes `data` to `dst`, replacing any existing file atomically.
///
/// Resolves a relative `dst` against the current directory once at entry. An
/// empty `dst` returns [`StorageError::Io`] with detail `empty destination
/// path`, and stages nothing. A filesystem root such as `/` returns
/// [`StorageError::Io`] with detail `no parent directory for /`, and stages
/// nothing.
///
/// Stages `data` in a private temp beside `dst`. On Unix, an existing regular
/// file gives the temp its mode; a missing or non-regular target leaves the
/// temp at private mode `0600`. Syncs the temp, replaces `dst`, and syncs the
/// parent directory on Unix. A failed step through the replacement removes the
/// temp and leaves `dst` unchanged.
///
/// A symlink at `dst` is replaced by a private regular file. The replacement
/// does not inherit the mode of the link's referent. On Windows, a read-only
/// target or a path past the OS path-length limit fails without changing `dst`.
/// On Unix, the target directory's permissions decide whether replacement
/// succeeds.
///
/// # Errors
///
/// Returns [`StorageError::Io`] when current-directory resolution, Unix target
/// stat, temp creation, writing, permission setting, syncing, replacement, or
/// Unix parent-directory syncing fails. A parent-directory sync failure occurs
/// after replacement, so `dst` may already hold the new bytes.
///
/// Example: overwriting `cfg.kdl` that contains `a=1` with `a=2` leaves it
/// containing exactly `a=2`; a crash before replacement leaves exactly `a=1`.
pub fn write_atomic(dst: &Path, data: &[u8]) -> Result<(), StorageError> {
    // Resolve a relative path against the current directory once. Both the
    // temp and replacement use this path if the working directory changes.
    if dst.as_os_str().is_empty() {
        return Err(io_err("empty destination path".to_string()));
    }
    let anchored;
    let dst = if dst.is_absolute() {
        dst
    } else {
        anchored = std::env::current_dir()
            .map_err(|e| io_err(format!("resolve cwd for {}: {e}", dst.display())))?
            .join(dst);
        anchored.as_path()
    };
    let Some(dir) = dst.parent() else {
        return Err(io_err(format!("no parent directory for {}", dst.display())));
    };
    // Read `dst`'s mode before replacement.
    let target_mode = target_permissions(dst)?;

    // Errors through `persist` drop `NamedTempFile` and remove its temp. Every
    // earlier error leaves `dst` unchanged.
    let mut tmp = NamedTempFile::new_in(dir)
        .map_err(|e| io_err(format!("create temp in {}: {e}", dir.display())))?;
    tmp.write_all(data)
        .map_err(|e| io_err(format!("write temp for {}: {e}", dst.display())))?;
    // Set the mode on the open temp before syncing it. The replaced inode
    // carries this mode.
    if let Some(perms) = target_mode {
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| io_err(format!("set perms for {}: {e}", dst.display())))?;
    }
    tmp.as_file()
        .sync_all()
        .map_err(|e| io_err(format!("fsync temp for {}: {e}", dst.display())))?;
    persist_over(tmp, dst)?;
    fsync_parent_dir(dir, dst)?;
    Ok(())
}

/// Replaces `dst` with the staged temp in one attempt. Unix `rename` replaces
/// the target in one step, including while another writer holds it. A failed
/// persist drops and removes the temp, leaving `dst` untouched.
#[cfg(not(windows))]
fn persist_over(tmp: NamedTempFile, dst: &Path) -> Result<(), StorageError> {
    tmp.persist(dst)
        .map(|_| ())
        .map_err(|e| io_err(format!("replace {}: {}", dst.display(), e.error)))
}

/// Replaces `dst`, retrying up to 25 times. An `ERROR_ACCESS_DENIED` (5) or
/// `ERROR_SHARING_VIOLATION` (32) failure is retried when `dst` is neither a
/// directory nor a read-only file. The sleep is `attempt * 4` milliseconds:
/// 4 ms after attempt 1 through 96 ms after attempt 24. Other errors, a
/// directory or read-only target, and a failed attempt 25 return
/// [`StorageError::Io`]. A failed persist drops and removes the temp, leaving
/// `dst` untouched.
#[cfg(windows)]
fn persist_over(mut tmp: NamedTempFile, dst: &Path) -> Result<(), StorageError> {
    const MAX_ATTEMPTS: u32 = 25;
    for attempt in 1..=MAX_ATTEMPTS {
        let err = match tmp.persist(dst) {
            Ok(_) => return Ok(()),
            Err(e) => {
                tmp = e.file;
                e.error
            }
        };
        // Codes 5 and 32 are the retryable replacement errors. A directory or
        // read-only file at `dst` makes either error permanent.
        let read_only = fs::metadata(dst).is_ok_and(|meta| meta.permissions().readonly());
        let transient =
            matches!(err.raw_os_error(), Some(5) | Some(32)) && !dst.is_dir() && !read_only;
        if attempt == MAX_ATTEMPTS || !transient {
            return Err(io_err(format!("replace {}: {err}", dst.display())));
        }
        std::thread::sleep(std::time::Duration::from_millis(u64::from(attempt) * 4));
    }
    unreachable!("the loop returns on its final attempt")
}

/// Returns the mode for an existing regular `dst` on Unix. Returns `None` for
/// a missing or non-regular target; the temp then keeps private mode `0600`.
///
/// Uses `symlink_metadata`, which inspects a link rather than its referent. A
/// symlink, FIFO, socket, or device at `dst` returns `None`, and replacement
/// creates a private regular file. Custom POSIX ACLs are not cloned.
///
/// Returns [`StorageError::Io`] when stat fails with an error other than
/// not-found, such as when a regular file blocks a directory component.
///
/// Example: a regular `dst` at `0644` returns `Some(0644)`; a missing `dst`, a
/// symlink, or a FIFO returns `None` and leaves the replacement at `0600`.
#[cfg(unix)]
fn target_permissions(dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    match fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_file() => Ok(Some(meta.permissions())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(format!("stat {}: {e}", dst.display()))),
    }
}

/// Returns `None` on every platform but Unix. On Windows, `std` models only
/// the read-only flag; this function does not copy it.
#[cfg(not(unix))]
fn target_permissions(_dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    Ok(None)
}

/// Syncs the directory `dir` that holds `dst` on Unix. Names `dst` in errors
/// and returns [`StorageError::Io`] when opening or syncing the directory
/// fails.
#[cfg(unix)]
fn fsync_parent_dir(dir: &Path, dst: &Path) -> Result<(), StorageError> {
    let handle =
        fs::File::open(dir).map_err(|e| io_err(format!("open dir for {}: {e}", dst.display())))?;
    handle
        .sync_all()
        .map_err(|e| io_err(format!("fsync dir for {}: {e}", dst.display())))
}

/// Returns `Ok(())` and touches nothing on every platform but Unix.
#[cfg(not(unix))]
fn fsync_parent_dir(_dir: &Path, _dst: &Path) -> Result<(), StorageError> {
    Ok(())
}

fn io_err(detail: String) -> StorageError {
    StorageError::Io { detail }
}
