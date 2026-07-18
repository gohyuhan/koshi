//! Atomic file replacement. Writes a file so a reader never sees it
//! half-written: write the new bytes to a temp sibling, flush them to disk,
//! then rename the temp over the target. `rename` swaps the destination in one
//! step on every platform, so the target is always either the whole old file
//! or the whole new one — never a truncated mix, even if the process dies
//! mid-write.
//!
//! Example: `write_atomic("keybinding.kdl", new)` writes `keybinding.kdl.tmp`,
//! fsyncs it, then renames it onto `keybinding.kdl`. A crash before the rename
//! leaves the intact old `keybinding.kdl`; the partial bytes are only ever in
//! the discarded `.tmp`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::StorageError;

#[cfg(test)]
mod tests;

/// Writes `data` to `dst`, replacing any existing file atomically.
///
/// Writes to `<dst>.tmp`, fsyncs it, then renames it over `dst`. If any step
/// fails the temp file is removed and `dst` is left untouched.
///
/// Example: overwriting `cfg.kdl` that currently holds `a=1` with `a=2` yields
/// a `cfg.kdl` reading exactly `a=2`; a crash mid-write leaves exactly `a=1`.
pub fn write_atomic(dst: &Path, data: &[u8]) -> Result<(), StorageError> {
    let tmp = tmp_path(dst);
    if let Err(e) = write_tmp(&tmp, data) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = replace(&tmp, dst) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Renames `src_tmp` over `dst` atomically, replacing any existing `dst`.
///
/// `src_tmp` and `dst` must sit on the same filesystem (a cross-device rename
/// is not atomic and fails). Callers that build their own temp file — e.g. a
/// snapshot writer — call this directly instead of [`write_atomic`].
pub fn replace(src_tmp: &Path, dst: &Path) -> Result<(), StorageError> {
    fs::rename(src_tmp, dst).map_err(|e| {
        io_err(format!(
            "replace {} -> {}: {e}",
            src_tmp.display(),
            dst.display()
        ))
    })
}

/// Writes `data` to `tmp`, then flushes file contents and metadata to disk so
/// the bytes survive a crash before the rename.
fn write_tmp(tmp: &Path, data: &[u8]) -> Result<(), StorageError> {
    let mut f = File::create(tmp).map_err(|e| io_err(format!("create {}: {e}", tmp.display())))?;
    f.write_all(data)
        .map_err(|e| io_err(format!("write {}: {e}", tmp.display())))?;
    // ponytail: fsync the file only; fsyncing the parent dir (so the rename
    // itself is durable) is not portable — Windows can't fsync a dir. The
    // per-OS dir-fsync is XPLAT-004 hardening.
    f.sync_all()
        .map_err(|e| io_err(format!("fsync {}: {e}", tmp.display())))?;
    Ok(())
}

/// `dst` with a `.tmp` suffix on its file name, keeping it in `dst`'s
/// directory so the rename stays on one filesystem.
///
// ponytail: fixed `.tmp` name — two concurrent writers to the same `dst`
// would clobber each other's temp. Config-migrate is single-writer; a
// pid/random-suffixed temp is XPLAT-004 hardening if a concurrent writer
// ever appears.
fn tmp_path(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    dst.with_file_name(name)
}

/// Builds a [`StorageError::Io`] from a detail string.
fn io_err(detail: String) -> StorageError {
    StorageError::Io { detail }
}
