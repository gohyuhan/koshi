//! Resolves the directories where koshi stores configuration, data, state, and
//! runtime files.
//!
//! The resolvers use platform conventions and return per-user paths, except
//! [`resolve_shared_sessions_directory`], which returns a machine-wide path. `KOSHI_RUNTIME_DIR`
//! is the only `KOSHI_*` variable that changes a path; it names [`resolve_runtime_directory`]
//! when it holds an absolute path. Other `KOSHI_*` variables are ignored:
//!
//! | Function | Linux | macOS | Windows |
//! |---|---|---|---|
//! | [`resolve_config_directory`] | `~/.config/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\config` |
//! | [`resolve_data_directory`] | `~/.local/share/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\data` |
//! | [`resolve_state_directory`] | `~/.local/state/koshi` | `~/Library/Application Support/koshi` | `%LOCALAPPDATA%\koshi\data` |
//! | [`resolve_runtime_directory`] | `/tmp/koshi-<uid>` | `/tmp/koshi-<uid>` | `<data_directory>\run` |
//! | [`resolve_shared_sessions_directory`] | `/tmp/koshi` | `/tmp/koshi` | `%ProgramData%\koshi` |
//!
//! The [`directories`] crate resolves [`resolve_config_directory`], [`resolve_data_directory`], and
//! [`resolve_state_directory`]. On Linux, an absolute `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, or
//! `XDG_STATE_HOME` replaces its matching base; a relative value is ignored.
//! [`resolve_runtime_directory`] reads no `XDG_*` variable. Linux and macOS use a fixed path
//! for [`resolve_shared_sessions_directory`].
//!
//! A per-user resolver returns `None` when the platform cannot provide the
//! required base directories. On Linux and macOS, `HOME` must be set and
//! non-empty, or the passwd database must provide a home directory. On
//! Windows, the `%APPDATA%` and `%LOCALAPPDATA%` known folders must resolve. On
//! Windows, [`resolve_shared_sessions_directory`] returns `None` when `%ProgramData%` is unset
//! or not absolute.
//!
//! Resolvers do not inspect the filesystem or create directories. Startup uses
//! [`ensure_directory`], [`ensure_private_directory`], [`ensure_shared_base`], and
//! [`ensure_shared_user_directory`] to create the directories it needs.

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The environment variable naming [`resolve_runtime_directory`]. Its value is used only
/// when it is an absolute path.
const RUNTIME_DIRECTORY_ENV_VAR: &str = "KOSHI_RUNTIME_DIR";

/// Returns the platform directory set for the `koshi` project, or `None` when
/// the required platform base directories cannot be resolved.
fn resolve_project_directories() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "koshi")
}

/// This process's effective user id. It names [`resolve_runtime_directory`] and this user's
/// directory under [`resolve_shared_sessions_directory`].
#[cfg(unix)]
fn effective_user_id() -> u32 {
    // SAFETY: `geteuid` reads this process's own identity, takes no argument,
    // and cannot fail.
    unsafe { libc::geteuid() }
}

/// Returns the directory for `koshi.kdl`, `keybinding.kdl`, `themes/`, and
/// `profile/`. On Linux this is `~/.config/koshi`; see the [module table](self)
/// for every platform.
#[must_use]
pub fn resolve_config_directory() -> Option<PathBuf> {
    resolve_project_directories().map(|project| project.config_dir().to_path_buf())
}

/// Returns the directory for durable data, including session persistence and
/// crash reports. On Linux this is `~/.local/share/koshi`; see the [module
/// table](self).
#[must_use]
pub fn resolve_data_directory() -> Option<PathBuf> {
    resolve_project_directories().map(|project| project.data_dir().to_path_buf())
}

/// Returns the directory for machine-local mutable state, including logs.
/// Linux uses `~/.local/state/koshi`. macOS uses
/// `~/Library/Application Support/koshi`; Windows uses
/// `%LOCALAPPDATA%\koshi\data`.
#[must_use]
pub fn resolve_state_directory() -> Option<PathBuf> {
    resolve_project_directories().map(|project| {
        project
            .state_dir()
            .unwrap_or_else(|| project.data_local_dir())
            .to_path_buf()
    })
}

/// Identifies how [`resolve_runtime_directory_with_rule`] selected its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDirectoryRule {
    /// `KOSHI_RUNTIME_DIR` held an absolute path.
    EnvironmentVariable,
    /// The Unix path is `/tmp/koshi-<effective user id>`.
    UserId,
    /// The Windows path is `run/` under [`resolve_data_directory`].
    DataDirectory,
}

/// Returns the runtime directory for sockets and other per-boot files together
/// with the rule that selected it.
///
/// An absolute `KOSHI_RUNTIME_DIR` gives [`RuntimeDirectoryRule::EnvironmentVariable`]. Without
/// it, Unix returns `/tmp/koshi-<effective uid>` with
/// [`RuntimeDirectoryRule::UserId`] and never returns `None`. Windows returns
/// `run/` under [`resolve_data_directory`] with [`RuntimeDirectoryRule::DataDirectory`], or `None` when
/// the project data directory cannot be resolved. Create the directory with
/// [`ensure_private_directory`]; runtime files are per-user private.
#[must_use]
pub fn resolve_runtime_directory_with_rule() -> Option<(PathBuf, RuntimeDirectoryRule)> {
    if let Some(runtime_directory) = std::env::var_os(RUNTIME_DIRECTORY_ENV_VAR)
        .map(PathBuf::from)
        .filter(|runtime_directory| runtime_directory.is_absolute())
    {
        return Some((runtime_directory, RuntimeDirectoryRule::EnvironmentVariable));
    }
    #[cfg(unix)]
    {
        Some((
            PathBuf::from(format!("/tmp/koshi-{}", effective_user_id())),
            RuntimeDirectoryRule::UserId,
        ))
    }
    #[cfg(windows)]
    {
        Some((
            resolve_project_directories()?.data_dir().join("run"),
            RuntimeDirectoryRule::DataDirectory,
        ))
    }
}

/// Returns the runtime directory for sockets and other per-boot files.
///
/// An absolute `KOSHI_RUNTIME_DIR` names the directory. Without it, Unix
/// returns `/tmp/koshi-<effective uid>` and Windows returns `run/` under
/// [`resolve_data_directory`]. Unix never returns `None`; Windows returns `None` when the
/// project data directory cannot be resolved. Create it with
/// [`ensure_private_directory`]. [`resolve_runtime_directory_with_rule`] returns the same path with
/// its selection rule.
#[must_use]
pub fn resolve_runtime_directory() -> Option<PathBuf> {
    resolve_runtime_directory_with_rule().map(|(runtime_directory, _)| runtime_directory)
}

/// Returns the machine-wide directory for shared session sockets. Windows
/// also stores marker files there for sessions listening on named pipes.
///
/// Unix returns `/tmp/koshi`. Windows returns `koshi` under `%ProgramData%`, or
/// `None` when that variable is unset or not absolute. Create the directory
/// with [`ensure_shared_base`], then use [`ensure_shared_user_directory`] to get this
/// user's directory. A `shared-sessions-dir` in `koshi.kdl` uses its own path
/// instead.
#[must_use]
pub fn resolve_shared_sessions_directory() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(PathBuf::from("/tmp/koshi"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .filter(|program_data_path| program_data_path.is_absolute())
            .map(|program_data_path| program_data_path.join("koshi"))
    }
}

/// Returns a [`io::ErrorKind::PermissionDenied`] error with the message
/// `<path> <reason>`, such as `/tmp/koshi-501 is not a directory`.
#[cfg(unix)]
fn directory_refused(refused_path: &Path, refusal_reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {refusal_reason}", refused_path.display()),
    )
}

/// Confirms that the effective user id owns `path`.
///
/// Reads `path` itself, not a symbolic link target. A different owner returns
/// [`io::ErrorKind::PermissionDenied`] with both user ids, such as
/// `/tmp/koshi-501 is owned by uid 0, expected 501`. A metadata read error is
/// returned unchanged.
#[cfg(unix)]
fn verify_owner_is_this_user(filesystem_path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let expected_user_id = effective_user_id();
    let owner_user_id = std::fs::symlink_metadata(filesystem_path)?.uid();
    if owner_user_id != expected_user_id {
        return Err(directory_refused(
            filesystem_path,
            &format!("is owned by uid {owner_user_id}, expected {expected_user_id}"),
        ));
    }
    Ok(())
}

/// Creates `path` without creating its parents. A directory, regular file, or
/// symbolic link already at `path` is success. A missing parent returns
/// [`io::ErrorKind::NotFound`].
#[cfg(unix)]
fn create_directory_if_absent(directory_path: &Path) -> io::Result<()> {
    match std::fs::create_dir(directory_path) {
        Err(filesystem_error) if filesystem_error.kind() != io::ErrorKind::AlreadyExists => {
            Err(filesystem_error)
        }
        _ => Ok(()),
    }
}

/// Confirms that `path` is a directory with exactly `mode`.
///
/// Reads `path` itself, not a symbolic link target. A symbolic link or regular
/// file returns [`io::ErrorKind::PermissionDenied`] with `<path> is not a
/// directory`. A mode-change failure returns
/// [`io::ErrorKind::PermissionDenied`] with `<path> mode could not be set:
/// <error>`. A different mode after the change returns the same error kind with
/// `<path> mode is <found:04o>, expected <mode:04o>`. Metadata read errors are
/// returned unchanged.
#[cfg(unix)]
fn verify_directory_mode(directory_path: &Path, directory_mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let path_metadata = std::fs::symlink_metadata(directory_path)?;
    if !path_metadata.is_dir() {
        return Err(directory_refused(directory_path, "is not a directory"));
    }
    if path_metadata.permissions().mode() & 0o7777 != directory_mode {
        std::fs::set_permissions(
            directory_path,
            std::fs::Permissions::from_mode(directory_mode),
        )
        .map_err(|permission_error| {
            directory_refused(
                directory_path,
                &format!("mode could not be set: {permission_error}"),
            )
        })?;
        let observed_mode = std::fs::symlink_metadata(directory_path)?
            .permissions()
            .mode()
            & 0o7777;
        if observed_mode != directory_mode {
            return Err(directory_refused(
                directory_path,
                &format!("mode is {observed_mode:04o}, expected {directory_mode:04o}"),
            ));
        }
    }
    Ok(())
}

/// Creates and validates the machine-wide shared directory at
/// `shared_base_path`.
///
/// On Unix it creates `shared_base_path` without creating its parents. A missing parent
/// returns [`io::ErrorKind::NotFound`]. `shared_base_path` must be a directory with mode
/// `1777`; another mode is replaced and checked. A symbolic link, regular
/// file, failed mode change, or different mode returns
/// [`io::ErrorKind::PermissionDenied`] with the path. An existing directory
/// owned by another user is accepted when its mode is `1777` or can be changed
/// to `1777`. Metadata read errors are returned unchanged. On Windows it
/// creates `shared_base_path` and missing parents with the parent's ACLs.
pub fn ensure_shared_base(shared_base_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        create_directory_if_absent(shared_base_path)?;
        verify_directory_mode(shared_base_path, 0o1777)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(shared_base_path)
    }
}

/// Creates and validates this user's directory under `shared_base_path`, then returns its
/// path.
///
/// On Unix the path is named after the effective user id, such as
/// `/tmp/koshi/501`, and must be owned by that user with mode `0755`. It
/// creates no parents; a missing `base` returns [`io::ErrorKind::NotFound`] and
/// remains uncreated. A different owner, symbolic link, or regular file at the
/// user path returns [`io::ErrorKind::PermissionDenied`] with the path. Another
/// mode is replaced and checked. On Windows it creates `base` and missing
/// parents, returns `base`, and uses no per-user directory.
pub fn ensure_shared_user_directory(shared_base_path: &Path) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        let user_directory_path = shared_base_path.join(effective_user_id().to_string());
        create_directory_if_absent(&user_directory_path)?;
        verify_owner_is_this_user(&user_directory_path)?;
        verify_directory_mode(&user_directory_path, 0o755)?;
        Ok(user_directory_path)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(shared_base_path)?;
        Ok(shared_base_path.to_path_buf())
    }
}

/// Creates `path` and any missing parents. An existing directory is success;
/// a regular file at `path` returns [`io::ErrorKind::AlreadyExists`]. Other
/// filesystem errors are returned unchanged.
pub fn ensure_directory(directory_path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(directory_path)
}

/// Creates `path` and missing parents, then validates the final directory as
/// this user's private runtime directory.
///
/// On Unix the final path must be owned by the effective user id and must be a
/// directory, not a symbolic link. Ownership, file-type, mode-change, and
/// mode-readback refusals return [`io::ErrorKind::PermissionDenied`] with the
/// path and reason. The final directory is set to and checked for mode `0700`.
/// A regular file at the final path returns
/// [`io::ErrorKind::AlreadyExists`]; a dangling symbolic link returns the same
/// error on Unix. On Windows it creates the directory with ACLs inherited from
/// its parent.
pub fn ensure_private_directory(directory_path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(directory_path)?;
    #[cfg(unix)]
    {
        verify_owner_is_this_user(directory_path)?;
        verify_directory_mode(directory_path, 0o700)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
