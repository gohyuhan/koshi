//! Atomic file replacement. A reader never sees a half-written file.
//!
//! The new bytes go to a temp file beside the target, get fsynced, then get
//! renamed over it. `rename` swaps the destination in one step on every
//! platform. The target holds either the whole old file or the whole new one,
//! even when the process dies mid-write.
//!
//! The temp comes from [`tempfile`]. It carries a unique name, and mode `0600`
//! on Unix. It opens with `O_EXCL`, which never follows a symlink and never
//! truncates an existing file.
//!
//! Unix fsyncs the target's directory after the rename. Windows does not fsync
//! the directory. Windows retries a replace that another writer blocks, up to
//! 25 attempts.
//!
//! Nothing here guards the target's directory. Anyone who can write that
//! directory can replace the file directly. koshi writes only under its own
//! user-private directories.
//!
//! [`write_atomic`] carries the full contract: per-platform permission
//! handling, symlink replacement, and the failure cases.
//!
//! Example: `write_atomic("keybinding.kdl", new)` stages a private temp beside
//! `keybinding.kdl`, fsyncs it, then renames it on top. A crash before the
//! rename leaves the old `keybinding.kdl` whole, and the partial bytes sit in
//! the temp sibling, never in the target. Every error path removes that temp;
//! a hard kill can leave it behind as a stray private file.

use std::fs;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::error::StorageError;

#[cfg(test)]
mod tests;

/// Writes `data` to `dst`, replacing any existing file atomically.
///
/// Joins a relative `dst` to the current directory once, at entry. An empty
/// `dst` is [`StorageError::Io`] carrying `empty destination path`, and
/// nothing is staged. A `dst` that names no file
/// once anchored — a filesystem root such as `/` — is [`StorageError::Io`]
/// carrying `no parent directory for /`, and nothing is staged. Stages `data`
/// in a private temp beside `dst`. On Unix the temp takes `dst`'s mode
/// when `dst` is an existing regular file; a new file keeps the private `0600`
/// default. Fsyncs the temp, renames it over `dst`, then fsyncs the directory
/// on Unix. If any step up to and including the rename fails, removes the temp
/// and leaves `dst` untouched.
///
/// Replaces a symlink at `dst` with a regular file, the same as `rename`. The
/// replacement counts as a new file and stays private. It never inherits the
/// mode of the file the link pointed at.
///
/// On Windows the replace fails for a read-only file, and for a path past the
/// OS path-length limit. `dst` stays untouched in both cases. On Unix the
/// directory's permissions decide whether the replace succeeds.
///
/// Returns [`StorageError::Io`] if the write is not durably persisted. A
/// directory-fsync failure surfaces here even though `dst` may already hold the
/// new bytes.
///
/// Example: overwriting `cfg.kdl` that currently holds `a=1` with `a=2` yields
/// a `cfg.kdl` reading exactly `a=2`; a crash mid-write leaves exactly `a=1`.
pub fn write_atomic(dst: &Path, data: &[u8]) -> Result<(), StorageError> {
    // Joins a relative path to the current directory once. The temp and the
    // rename below use this path; a change of the working directory mid-call
    // moves neither of them.
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
    // Read `dst`'s mode before the rename replaces `dst`.
    let target_mode = target_permissions(dst)?;

    // A failed `?` up to and including `persist` drops the NamedTempFile, which
    // removes the temp. Every early return leaves `dst` and the directory
    // untouched.
    let mut tmp = NamedTempFile::new_in(dir)
        .map_err(|e| io_err(format!("create temp in {}: {e}", dir.display())))?;
    tmp.write_all(data)
        .map_err(|e| io_err(format!("write temp for {}: {e}", dst.display())))?;
    // The mode goes on the open temp, before the fsync; the fsynced inode
    // carries it. The renamed file is never readable by anyone that mode
    // excludes.
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

/// Renames the staged temp over `dst` in a single attempt. Unix `rename`
/// replaces the target in one step even while other writers hold it. A failed
/// persist drops the temp, which removes it; `dst` is left untouched.
#[cfg(not(windows))]
fn persist_over(tmp: NamedTempFile, dst: &Path) -> Result<(), StorageError> {
    tmp.persist(dst)
        .map(|_| ())
        .map_err(|e| io_err(format!("replace {}: {}", dst.display(), e.error)))
}

/// Renames the staged temp over `dst`, retrying up to 25 times. An attempt
/// that fails with `ERROR_ACCESS_DENIED` (5) or `ERROR_SHARING_VIOLATION` (32)
/// while `dst` is neither a directory nor a read-only file is retried after a
/// sleep of `attempt * 4` milliseconds (4 ms after the first attempt, 96 ms
/// after the 24th). Any other error, a directory or a read-only file at `dst`,
/// or a failed 25th attempt is reported at once as [`StorageError::Io`]. A
/// failed persist drops the temp, which removes it; `dst` is untouched.
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
        // 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION. A directory
        // and a read-only file at `dst` refuse the rename however often it is
        // tried.
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

/// The mode to give the temp on Unix: the existing `dst`'s when `dst` is a
/// regular file, or `None` for anything else. With `None` the temp keeps its
/// private `0600` default.
///
/// Uses `symlink_metadata`, which does not follow links. Any other node at
/// `dst` — a symlink, FIFO, socket, or device — gives `None`; the rename
/// replaces that node with a new private file. A missing `dst` gives `None`.
/// Custom POSIX ACLs are not cloned.
///
/// Returns [`StorageError::Io`] when the stat fails with any error but
/// not-found, such as a regular file in `dst`'s directory path.
///
/// Example: `dst` exists at `0644` → `Some(0644)`, and the replaced file keeps
/// `0644` instead of the temp's `0600`; a missing `dst` → `None`; `dst` a
/// symlink to (or a FIFO at) `0644` → `None`, and the replacement file is
/// `0600`.
#[cfg(unix)]
fn target_permissions(dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    match fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_file() => Ok(Some(meta.permissions())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(format!("stat {}: {e}", dst.display()))),
    }
}

/// The mode to give the temp on every platform but Unix: always `None`. `std`
/// models only the read-only flag there, and the rename fails on a read-only
/// `dst`.
#[cfg(not(unix))]
fn target_permissions(_dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    Ok(None)
}

/// Fsyncs `dir`, which holds `dst`. Names `dst` in its errors. Returns
/// [`StorageError::Io`] when the directory cannot be opened or fsynced. Unix
/// only.
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

/// Builds a [`StorageError::Io`] from a detail string.
fn io_err(detail: String) -> StorageError {
    StorageError::Io { detail }
}
