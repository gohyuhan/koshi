//! Atomic file replacement. Writes a file so a reader never sees it
//! half-written: stage the new bytes in a temp sibling, flush them to disk,
//! then rename the temp over the target. `rename` swaps the destination in one
//! step on every platform, so the target is always either the whole old file
//! or the whole new one — never a truncated mix, even if the process dies
//! mid-write.
//!
//! The temp is created with [`tempfile`]: a unique name, opened with `O_EXCL`
//! (never follows a symlink or truncates an existing file) and — on Unix —
//! mode `0600`. [`write_atomic`] documents the full contract: per-platform
//! permission handling, symlink replacement, and the failure cases.
//!
//! The rename is atomic on every platform. On Unix the target's directory is
//! fsynced afterward so the rename survives a crash; on Windows the rename's
//! durability rests on the filesystem's own journaling.
//!
//! The target's directory is trusted: anyone who can write that directory can
//! replace the file directly, with or without this helper, so koshi only
//! writes under its own user-private directories.
//!
//! Example: `write_atomic("keybinding.kdl", new)` stages a private temp beside
//! `keybinding.kdl`, fsyncs it, then renames it onto `keybinding.kdl`. A crash
//! before the rename leaves the intact old `keybinding.kdl`; the partial bytes
//! sit only in the temp sibling, which normal error paths remove and a hard
//! kill can leave behind as a stray private file — never in the target.

use std::fs;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::error::StorageError;

#[cfg(test)]
mod tests;

/// Writes `data` to `dst`, replacing any existing file atomically.
///
/// Stages `data` in a private temp beside `dst`, gives the temp `dst`'s mode
/// when `dst` is an existing regular file (Unix; a new file keeps the private
/// `0600` default), fsyncs it, renames it over `dst`, then fsyncs the directory
/// (Unix). A relative `dst` is resolved against the current directory once at
/// entry. If any step up to and including the rename fails the temp is removed
/// and `dst` is left untouched.
///
/// A symlink at `dst` is replaced by a regular file, the same as `rename`; the
/// replacement counts as a new file and stays private, never inheriting the
/// mode of the file the link pointed at.
///
/// On Windows, replacing a read-only file fails with an error (the OS forbids
/// it; on Unix the directory's permissions govern, not the file's own mode), as
/// does a path past the OS path-length limit — in both cases `dst` is left
/// untouched.
///
/// Returns [`StorageError::Io`] if the write is not durably persisted; a
/// directory-fsync failure surfaces here even though `dst` may already hold the
/// new bytes.
///
/// Example: overwriting `cfg.kdl` that currently holds `a=1` with `a=2` yields
/// a `cfg.kdl` reading exactly `a=2`; a crash mid-write leaves exactly `a=1`.
pub fn write_atomic(dst: &Path, data: &[u8]) -> Result<(), StorageError> {
    // Anchor a relative path to the current directory once, so the temp and
    // the rename below always resolve inside the same directory even if the
    // process working directory changes mid-call.
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
    // Read the mode to restore before the temp shadows it.
    let target_mode = target_permissions(dst)?;

    // A failed `?` up to and including `persist` drops the NamedTempFile, which
    // removes the temp — so every early-return leaves `dst` and the directory
    // untouched.
    let mut tmp = NamedTempFile::new_in(dir)
        .map_err(|e| io_err(format!("create temp in {}: {e}", dir.display())))?;
    tmp.write_all(data)
        .map_err(|e| io_err(format!("write temp for {}: {e}", dst.display())))?;
    // Set the final mode before the fsync so it lands in the durable inode; the
    // rename then never loosens who can read the file.
    if let Some(perms) = target_mode {
        fs::set_permissions(tmp.path(), perms)
            .map_err(|e| io_err(format!("set perms for {}: {e}", dst.display())))?;
    }
    tmp.as_file()
        .sync_all()
        .map_err(|e| io_err(format!("fsync temp for {}: {e}", dst.display())))?;
    tmp.persist(dst)
        .map_err(|e| io_err(format!("replace {}: {}", dst.display(), e.error)))?;
    fsync_parent_dir(dst)?;
    Ok(())
}

/// The mode to give the temp on Unix: the existing `dst`'s when `dst` is a
/// regular file, or `None` for anything else, so the temp's private `0600`
/// default stands.
///
/// Uses `symlink_metadata` (does not follow links) and inherits only from a
/// regular file: any other node at `dst` — a symlink, FIFO, socket, or device —
/// is destroyed by the rename, so its replacement is a new file and keeps the
/// private default rather than that node's mode. Custom POSIX ACLs are not
/// cloned — `std` has no API for them, and koshi creates only plain user-owned
/// files.
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
/// replaced at all (the rename fails), so there is nothing to preserve.
#[cfg(not(unix))]
fn target_permissions(_dst: &Path) -> Result<Option<fs::Permissions>, StorageError> {
    Ok(None)
}

/// Fsyncs `dst`'s directory so the rename entry is durable — without it a crash
/// right after the rename can lose it, leaving the old file. Unix only; Windows
/// has no portable directory fsync (returns `Ok`).
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

/// `dst`'s directory, so the temp is created on the same filesystem as `dst`
/// and the rename stays atomic. `dst` arrives absolute (relative paths are
/// anchored on entry); the `.` fallback covers a path with no parent (a
/// filesystem root), where the later rename fails and reports the error.
fn parent_dir(dst: &Path) -> &Path {
    dst.parent().unwrap_or(Path::new("."))
}

/// Builds a [`StorageError::Io`] from a detail string.
fn io_err(detail: String) -> StorageError {
    StorageError::Io { detail }
}
