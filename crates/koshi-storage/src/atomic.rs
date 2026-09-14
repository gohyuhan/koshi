//! Atomic file replacement. Readers see the complete old file or the complete
//! new file, never a torn middle.
//!
//! [`write_atomic`] uses [`tempfile`] to create a uniquely named staging file
//! beside the target, calls `sync_all` on that staging file, replaces the target with
//! the platform's atomic replacement operation, and calls `sync_all` on the
//! target directory on Unix. The staging file starts private with mode `0600` on Unix.
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
//! Normal error paths drop the staging file. On Unix, a hard process termination
//! can leave the named staging file behind.
//!
//! Example: `write_atomic("keybinding.kdl", new)` stages a private file beside
//! `keybinding.kdl`, syncs it, and replaces the target. A crash before the
//! replacement leaves the old file whole; partial bytes stay in the staging
//! sibling. A hard kill on Unix can leave that private sibling behind.

use std::fs;
use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::error::StorageError;

#[cfg(test)]
mod tests;

/// Writes `file_bytes` to `destination_path`, replacing any existing file atomically.
///
/// Resolves a relative `destination_path` against the current directory once at entry. An
/// empty `destination_path` returns [`StorageError::Io`] with detail `empty destination
/// path`, and stages nothing. A filesystem root such as `/` returns
/// [`StorageError::Io`] with detail `no parent directory for /`, and stages
/// nothing.
///
/// Stages `file_bytes` in a private staging file beside `destination_path`.
/// On Unix, an existing regular file gives the staging file its mode; a
/// missing or non-regular target leaves the staging file at private mode
/// `0600`. Syncs the staging file, replaces `destination_path`, and syncs the
/// parent directory on Unix. A failed step through the replacement removes the
/// staging file and leaves `destination_path` unchanged.
///
/// A symlink at `destination_path` is replaced by a private regular file. The replacement
/// does not inherit the mode of the link's referent. On Windows, a read-only
/// target or a path past the OS path-length limit fails without changing `destination_path`.
/// On Unix, the target directory's permissions decide whether replacement
/// succeeds.
///
/// # Errors
///
/// Returns [`StorageError::Io`] when current-directory resolution, Unix target
/// stat, staging-file creation, writing, permission setting, syncing, replacement, or
/// Unix parent-directory syncing fails. A parent-directory sync failure occurs
/// after replacement, so `destination_path` may already hold the new bytes.
///
/// Example: overwriting `cfg.kdl` that contains `a=1` with `a=2` leaves it
/// containing exactly `a=2`; a crash before replacement leaves exactly `a=1`.
pub fn write_atomic(destination_path: &Path, file_bytes: &[u8]) -> Result<(), StorageError> {
    // Resolve a relative path against the current directory once. Both the
    // Staging-file creation and replacement use this path if the working directory changes.
    if destination_path.as_os_str().is_empty() {
        return Err(build_storage_io_error("empty destination path".to_string()));
    }
    let anchored_destination_path;
    let destination_path = if destination_path.is_absolute() {
        destination_path
    } else {
        anchored_destination_path = std::env::current_dir()
            .map_err(|io_error| {
                build_storage_io_error(format!(
                    "resolve cwd for {}: {io_error}",
                    destination_path.display()
                ))
            })?
            .join(destination_path);
        anchored_destination_path.as_path()
    };
    let Some(parent_directory_path) = destination_path.parent() else {
        return Err(build_storage_io_error(format!(
            "no parent directory for {}",
            destination_path.display()
        )));
    };
    // Read `destination_path`'s mode before replacement.
    let existing_target_permissions = get_target_permissions(destination_path)?;

    // Errors through `persist` drop `NamedTempFile` and remove its staging file. Every
    // earlier error leaves `destination_path` unchanged.
    let mut staged_file = NamedTempFile::new_in(parent_directory_path).map_err(|io_error| {
        build_storage_io_error(format!(
            "create temp in {}: {io_error}",
            parent_directory_path.display()
        ))
    })?;
    staged_file.write_all(file_bytes).map_err(|io_error| {
        build_storage_io_error(format!(
            "write temp for {}: {io_error}",
            destination_path.display()
        ))
    })?;
    // Set the mode on the open staging file before syncing it. The replaced inode
    // carries this mode.
    if let Some(target_permissions) = existing_target_permissions {
        staged_file
            .as_file()
            .set_permissions(target_permissions)
            .map_err(|io_error| {
                build_storage_io_error(format!(
                    "set perms for {}: {io_error}",
                    destination_path.display()
                ))
            })?;
    }
    staged_file.as_file().sync_all().map_err(|io_error| {
        build_storage_io_error(format!(
            "fsync temp for {}: {io_error}",
            destination_path.display()
        ))
    })?;
    replace_destination_with_staged_file(staged_file, destination_path)?;
    fsync_parent_directory(parent_directory_path, destination_path)?;
    Ok(())
}

/// Replaces `destination_path` with the staged file in one attempt. Unix `rename` replaces
/// the target in one step, including while another writer holds it. A failed
/// persistence drops and removes the staging file, leaving `destination_path` untouched.
#[cfg(not(windows))]
fn replace_destination_with_staged_file(
    staged_file: NamedTempFile,
    destination_path: &Path,
) -> Result<(), StorageError> {
    staged_file
        .persist(destination_path)
        .map(|_| ())
        .map_err(|persist_error| {
            build_storage_io_error(format!(
                "replace {}: {}",
                destination_path.display(),
                persist_error.error
            ))
        })
}

/// Replaces `destination_path`, retrying up to 25 times. An `ERROR_ACCESS_DENIED` (5) or
/// `ERROR_SHARING_VIOLATION` (32) failure is retried when `destination_path` is neither a
/// directory nor a read-only file. The sleep is `attempt * 4` milliseconds:
/// 4 ms after attempt 1 through 96 ms after attempt 24. Other errors, a
/// directory or read-only target, and a failed attempt 25 return
/// [`StorageError::Io`]. A failed persist drops and removes the staging file, leaving
/// `destination_path` untouched.
#[cfg(windows)]
fn replace_destination_with_staged_file(
    mut staged_file: NamedTempFile,
    destination_path: &Path,
) -> Result<(), StorageError> {
    const MAX_REPLACEMENT_ATTEMPT_COUNT: u32 = 25;
    for replacement_attempt_count in 1..=MAX_REPLACEMENT_ATTEMPT_COUNT {
        let replacement_error = match staged_file.persist(destination_path) {
            Ok(_) => return Ok(()),
            Err(persist_error) => {
                staged_file = persist_error.file;
                persist_error.error
            }
        };
        // Codes 5 and 32 are the retryable replacement errors. A directory or
        // read-only file at `destination_path` makes either error permanent.
        let is_read_only = fs::metadata(destination_path)
            .is_ok_and(|file_metadata| file_metadata.permissions().readonly());
        let is_transient = matches!(replacement_error.raw_os_error(), Some(5) | Some(32))
            && !destination_path.is_dir()
            && !is_read_only;
        if replacement_attempt_count == MAX_REPLACEMENT_ATTEMPT_COUNT || !is_transient {
            return Err(build_storage_io_error(format!(
                "replace {}: {replacement_error}",
                destination_path.display()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(
            u64::from(replacement_attempt_count) * 4,
        ));
    }
    unreachable!("the loop returns on its final attempt")
}

/// Returns the mode for an existing regular `destination_path` on Unix. Returns `None` for
/// a missing or non-regular target; the staging file then keeps private mode `0600`.
///
/// Uses `symlink_metadata`, which inspects a link rather than its referent. A
/// symlink, FIFO, socket, or device at `destination_path` returns `None`, and replacement
/// creates a private regular file. Custom POSIX ACLs are not cloned.
///
/// Returns [`StorageError::Io`] when stat fails with an error other than
/// not-found, such as when a regular file blocks a directory component.
///
/// Example: a regular `destination_path` at `0644` returns `Some(0644)`; a missing `destination_path`, a
/// symlink, or a FIFO returns `None` and leaves the replacement at `0600`.
#[cfg(unix)]
fn get_target_permissions(
    destination_path: &Path,
) -> Result<Option<fs::Permissions>, StorageError> {
    match fs::symlink_metadata(destination_path) {
        Ok(file_metadata) if file_metadata.file_type().is_file() => {
            Ok(Some(file_metadata.permissions()))
        }
        Ok(_) => Ok(None),
        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(io_error) => Err(build_storage_io_error(format!(
            "stat {}: {io_error}",
            destination_path.display()
        ))),
    }
}

/// Returns `None` on every platform but Unix. On Windows, `std` models only
/// the read-only flag; this function does not copy it.
#[cfg(not(unix))]
fn get_target_permissions(
    _destination_path: &Path,
) -> Result<Option<fs::Permissions>, StorageError> {
    Ok(None)
}

/// Syncs the directory `parent_directory_path` that holds `destination_path` on Unix. Names `destination_path` in errors
/// and returns [`StorageError::Io`] when opening or syncing the directory
/// fails.
#[cfg(unix)]
fn fsync_parent_directory(
    parent_directory_path: &Path,
    destination_path: &Path,
) -> Result<(), StorageError> {
    let directory_handle = fs::File::open(parent_directory_path).map_err(|io_error| {
        build_storage_io_error(format!(
            "open dir for {}: {io_error}",
            destination_path.display()
        ))
    })?;
    directory_handle.sync_all().map_err(|io_error| {
        build_storage_io_error(format!(
            "fsync dir for {}: {io_error}",
            destination_path.display()
        ))
    })
}

/// Returns `Ok(())` and touches nothing on every platform but Unix.
#[cfg(not(unix))]
fn fsync_parent_directory(
    _parent_directory_path: &Path,
    _destination_path: &Path,
) -> Result<(), StorageError> {
    Ok(())
}

fn build_storage_io_error(error_detail: String) -> StorageError {
    StorageError::Io {
        detail: error_detail,
    }
}
