//! Atomic file replacement. A reader never sees a half-written file.
//!
//! The new bytes go to a temp file beside the target, get fsynced, then get
//! renamed over it. `rename` swaps the destination in one step on every
//! platform, so the target holds either the whole old file or the whole new
//! one, even if the process dies mid-write.
//!
//! The temp comes from [`tempfile`]. It carries a unique name, and mode `0600`
//! on Unix. It opens with `O_EXCL`, which never follows a symlink and never
//! truncates an existing file.
//!
//! Unix fsyncs the target's directory after the rename. Windows relies on the
//! filesystem's own journaling, and briefly retries a replace that races
//! another writer, so concurrent writes converge.
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
/// Resolves a relative `dst` against the current directory once, at entry.
/// Stages `data` in a private temp beside `dst`. On Unix the temp takes `dst`'s
/// mode when `dst` is an existing regular file; a new file keeps the private
/// `0600` default. Fsyncs the temp, renames it over `dst`, then fsyncs the
/// directory on Unix. If any step up to and including the rename fails, removes
/// the temp and leaves `dst` untouched.
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
    // Anchor a relative path to the current directory once. The temp and the
    // rename below then resolve inside the same directory even if the process
    // working directory changes mid-call.
    let anchored;
    let dst = if dst.is_absolute() {
        dst
    } else {
        anchored = std::env::current_dir()
            .map_err(|e| io_err(format!("resolve cwd for {}: {e}", dst.display())))?
            .join(dst);
        anchored.as_path()
    };
    let dir = parent_dir(dst);
    // Read `dst`'s mode before the rename replaces `dst`.
    let target_mode = target_permissions(dst)?;

    // A failed `?` up to and including `persist` drops the NamedTempFile, which
    // removes the temp. Every early return leaves `dst` and the directory
    // untouched.
    let mut tmp = NamedTempFile::new_in(dir)
        .map_err(|e| io_err(format!("create temp in {}: {e}", dir.display())))?;
    tmp.write_all(data)
        .map_err(|e| io_err(format!("write temp for {}: {e}", dst.display())))?;
    // Set the final mode before the fsync, so the durable inode carries it. The
    // renamed file is never readable by anyone the final mode excludes.
    if let Some(perms) = target_mode {
        fs::set_permissions(tmp.path(), perms)
            .map_err(|e| io_err(format!("set perms for {}: {e}", dst.display())))?;
    }
    tmp.as_file()
        .sync_all()
        .map_err(|e| io_err(format!("fsync temp for {}: {e}", dst.display())))?;
    persist_over(tmp, dst)?;
    fsync_parent_dir(dst)?;
    Ok(())
}

/// Renames the staged temp over `dst` in a single attempt: Unix `rename`
/// replaces the target in one step even while other writers hold it. A failed
/// persist drops the temp, removing it, so `dst` is left untouched.
#[cfg(not(windows))]
fn persist_over(tmp: NamedTempFile, dst: &Path) -> Result<(), StorageError> {
    tmp.persist(dst)
        .map(|_| ())
        .map_err(|e| io_err(format!("replace {}: {}", dst.display(), e.error)))
}

/// Renames the staged temp over `dst`, retrying up to 25 times with a growing
/// backoff. Windows refuses to replace a file another writer is momentarily
/// renaming over, failing with `ERROR_ACCESS_DENIED` /
/// `ERROR_SHARING_VIOLATION`; those are retried. A directory at `dst` is a
/// permanent block and is reported at once. A failed persist drops the temp,
/// so `dst` is untouched.
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
        // ERROR_ACCESS_DENIED (5) / ERROR_SHARING_VIOLATION (32): another writer
        // holds the target mid-rename. A directory at `dst` never clears, so it
        // is not a retryable lock.
        let transient = matches!(err.raw_os_error(), Some(5) | Some(32)) && !dst.is_dir();
        if attempt == MAX_ATTEMPTS || !transient {
            return Err(io_err(format!("replace {}: {err}", dst.display())));
        }
        std::thread::sleep(std::time::Duration::from_millis(u64::from(attempt) * 4));
    }
    unreachable!("the loop returns on its final attempt")
}

/// The mode to give the temp on Unix: the existing `dst`'s when `dst` is a
/// regular file, or `None` for anything else, so the temp's private `0600`
/// default stands.
///
/// Uses `symlink_metadata`, which does not follow links, and inherits only
/// from a regular file. Any other node at `dst` — a symlink, FIFO, socket, or
/// device — is destroyed by the rename, so its replacement is a new file and
/// keeps the private default. Custom POSIX ACLs are not cloned.
///
/// Example: `dst` exists at `0644` → returns `Some(0644)` so the replaced file
/// keeps `0644` instead of the temp's `0600`; a missing `dst` → `None`; `dst` a
/// symlink to (or a FIFO at) `0644` → `None`, so the replacement file is `0600`.
#[cfg(unix)]
fn target_permissions(dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    match fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_file() => Ok(Some(meta.permissions())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(format!("stat {}: {e}", dst.display()))),
    }
}

/// The mode to give the temp on Windows: always `None`. The only permission
/// `std` models there is the read-only flag, and a read-only target cannot be
/// replaced at all — the rename fails.
#[cfg(not(unix))]
fn target_permissions(_dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    Ok(None)
}

/// Fsyncs `dst`'s directory so the rename entry survives a crash. Unix only;
/// Windows has no portable directory fsync and returns `Ok`.
#[cfg(unix)]
fn fsync_parent_dir(dst: &Path) -> Result<(), StorageError> {
    let dir = fs::File::open(parent_dir(dst))
        .map_err(|e| io_err(format!("open dir for {}: {e}", dst.display())))?;
    dir.sync_all()
        .map_err(|e| io_err(format!("fsync dir for {}: {e}", dst.display())))
}

#[cfg(not(unix))]
fn fsync_parent_dir(_dst: &Path) -> Result<(), StorageError> {
    Ok(())
}

/// `dst`'s directory, which puts the temp on the same filesystem as `dst` so
/// the rename stays atomic. `dst` arrives absolute; relative paths are anchored
/// on entry. The `.` fallback covers a path with no parent, a filesystem root,
/// where the later rename fails and reports the error.
fn parent_dir(dst: &Path) -> &Path {
    dst.parent().unwrap_or(Path::new("."))
}

/// Builds a [`StorageError::Io`] from a detail string.
fn io_err(detail: String) -> StorageError {
    StorageError::Io { detail }
}
