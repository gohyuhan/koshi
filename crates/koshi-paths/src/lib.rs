//! Resolves the directories where koshi stores configuration, data, state, and
//! runtime files.
//!
//! The resolvers use platform conventions and return per-user paths, except
//! [`shared_sessions_dir`], which returns a machine-wide path. `KOSHI_RUNTIME_DIR`
//! is the only `KOSHI_*` variable that changes a path; it names [`runtime_dir`]
//! when it holds an absolute path. Other `KOSHI_*` variables are ignored:
//!
//! | Function | Linux | macOS | Windows |
//! |---|---|---|---|
//! | [`config_dir`] | `~/.config/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\config` |
//! | [`data_dir`] | `~/.local/share/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\data` |
//! | [`state_dir`] | `~/.local/state/koshi` | `~/Library/Application Support/koshi` | `%LOCALAPPDATA%\koshi\data` |
//! | [`runtime_dir`] | `/tmp/koshi-<uid>` | `/tmp/koshi-<uid>` | `<data_dir>\run` |
//! | [`shared_sessions_dir`] | `/tmp/koshi` | `/tmp/koshi` | `%ProgramData%\koshi` |
//!
//! The [`directories`] crate resolves [`config_dir`], [`data_dir`], and
//! [`state_dir`]. On Linux, an absolute `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, or
//! `XDG_STATE_HOME` replaces its matching base; a relative value is ignored.
//! [`runtime_dir`] reads no `XDG_*` variable. Linux and macOS use a fixed path
//! for [`shared_sessions_dir`].
//!
//! A per-user resolver returns `None` when the platform cannot provide the
//! required base directories. On Linux and macOS, `HOME` must be set and
//! non-empty, or the passwd database must provide a home directory. On
//! Windows, the `%APPDATA%` and `%LOCALAPPDATA%` known folders must resolve. On
//! Windows, [`shared_sessions_dir`] returns `None` when `%ProgramData%` is unset
//! or not absolute.
//!
//! Resolvers do not inspect the filesystem or create directories. Startup uses
//! [`ensure_dir`], [`ensure_private_dir`], [`ensure_shared_base`], and
//! [`ensure_shared_user_dir`] to create the directories it needs.

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The environment variable naming [`runtime_dir`]. Its value is used only
/// when it is an absolute path.
const RUNTIME_DIR_VAR: &str = "KOSHI_RUNTIME_DIR";

/// Returns the platform directory set for the `koshi` project, or `None` when
/// the required platform base directories cannot be resolved.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "koshi")
}

/// This process's effective user id. It names [`runtime_dir`] and this user's
/// directory under [`shared_sessions_dir`].
#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: `geteuid` reads this process's own identity, takes no argument,
    // and cannot fail.
    unsafe { libc::geteuid() }
}

/// Returns the directory for `koshi.kdl`, `keybinding.kdl`, `themes/`, and
/// `profile/`. On Linux this is `~/.config/koshi`; see the [module table](self)
/// for every platform.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    project_dirs().map(|project| project.config_dir().to_path_buf())
}

/// Returns the directory for durable data, including session persistence and
/// crash reports. On Linux this is `~/.local/share/koshi`; see the [module
/// table](self).
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|project| project.data_dir().to_path_buf())
}

/// Returns the directory for machine-local mutable state, including logs.
/// Linux uses `~/.local/state/koshi`. macOS uses
/// `~/Library/Application Support/koshi`; Windows uses
/// `%LOCALAPPDATA%\koshi\data`.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    project_dirs().map(|project| {
        project
            .state_dir()
            .unwrap_or_else(|| project.data_local_dir())
            .to_path_buf()
    })
}

/// Identifies how [`runtime_dir_with_rule`] selected its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeDirRule {
    /// `KOSHI_RUNTIME_DIR` held an absolute path.
    Variable,
    /// The Unix path is `/tmp/koshi-<effective user id>`.
    UserId,
    /// The Windows path is `run/` under [`data_dir`].
    DataDir,
}

/// Returns the runtime directory for sockets and other per-boot files together
/// with the rule that selected it.
///
/// An absolute `KOSHI_RUNTIME_DIR` gives [`RuntimeDirRule::Variable`]. Without
/// it, Unix returns `/tmp/koshi-<effective uid>` with
/// [`RuntimeDirRule::UserId`] and never returns `None`. Windows returns
/// `run/` under [`data_dir`] with [`RuntimeDirRule::DataDir`], or `None` when
/// the project data directory cannot be resolved. Create the directory with
/// [`ensure_private_dir`]; runtime files are per-user private.
#[must_use]
pub fn runtime_dir_with_rule() -> Option<(PathBuf, RuntimeDirRule)> {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_VAR)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        return Some((dir, RuntimeDirRule::Variable));
    }
    #[cfg(unix)]
    {
        Some((
            PathBuf::from(format!("/tmp/koshi-{}", euid())),
            RuntimeDirRule::UserId,
        ))
    }
    #[cfg(windows)]
    {
        Some((
            project_dirs()?.data_dir().join("run"),
            RuntimeDirRule::DataDir,
        ))
    }
}

/// Returns the runtime directory for sockets and other per-boot files.
///
/// An absolute `KOSHI_RUNTIME_DIR` names the directory. Without it, Unix
/// returns `/tmp/koshi-<effective uid>` and Windows returns `run/` under
/// [`data_dir`]. Unix never returns `None`; Windows returns `None` when the
/// project data directory cannot be resolved. Create it with
/// [`ensure_private_dir`]. [`runtime_dir_with_rule`] returns the same path with
/// its selection rule.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    runtime_dir_with_rule().map(|(dir, _)| dir)
}

/// Returns the machine-wide directory for shared session sockets. Windows
/// also stores marker files there for sessions listening on named pipes.
///
/// Unix returns `/tmp/koshi`. Windows returns `koshi` under `%ProgramData%`, or
/// `None` when that variable is unset or not absolute. Create the directory
/// with [`ensure_shared_base`], then use [`ensure_shared_user_dir`] to get this
/// user's directory. A `shared-sessions-dir` in `koshi.kdl` uses its own path
/// instead.
#[must_use]
pub fn shared_sessions_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(PathBuf::from("/tmp/koshi"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .filter(|base| base.is_absolute())
            .map(|base| base.join("koshi"))
    }
}

/// Returns a [`io::ErrorKind::PermissionDenied`] error with the message
/// `<path> <reason>`, such as `/tmp/koshi-501 is not a directory`.
#[cfg(unix)]
fn dir_refused(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {reason}", path.display()),
    )
}

/// Confirms that the effective user id owns `path`.
///
/// Reads `path` itself, not a symbolic link target. A different owner returns
/// [`io::ErrorKind::PermissionDenied`] with both user ids, such as
/// `/tmp/koshi-501 is owned by uid 0, expected 501`. A metadata read error is
/// returned unchanged.
#[cfg(unix)]
fn verify_owner_is_this_user(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let expected_uid = euid();
    let owner_uid = std::fs::symlink_metadata(path)?.uid();
    if owner_uid != expected_uid {
        return Err(dir_refused(
            path,
            &format!("is owned by uid {owner_uid}, expected {expected_uid}"),
        ));
    }
    Ok(())
}

/// Creates `path` without creating its parents. A directory, regular file, or
/// symbolic link already at `path` is success. A missing parent returns
/// [`io::ErrorKind::NotFound`].
#[cfg(unix)]
fn create_dir_if_absent(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
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
fn verify_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(dir_refused(path, "is not a directory"));
    }
    if metadata.permissions().mode() & 0o7777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| dir_refused(path, &format!("mode could not be set: {error}")))?;
        let found_mode = std::fs::symlink_metadata(path)?.permissions().mode() & 0o7777;
        if found_mode != mode {
            return Err(dir_refused(
                path,
                &format!("mode is {found_mode:04o}, expected {mode:04o}"),
            ));
        }
    }
    Ok(())
}

/// Creates and validates the machine-wide shared directory at `base`.
///
/// On Unix it creates `base` without creating its parents. A missing parent
/// returns [`io::ErrorKind::NotFound`]. `base` must be a directory with mode
/// `1777`; another mode is replaced and checked. A symbolic link, regular
/// file, failed mode change, or different mode returns
/// [`io::ErrorKind::PermissionDenied`] with the path. An existing directory
/// owned by another user is accepted when its mode is `1777` or can be changed
/// to `1777`. Metadata read errors are returned unchanged. On Windows it
/// creates `base` and missing parents with the parent's ACLs.
pub fn ensure_shared_base(base: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        create_dir_if_absent(base)?;
        verify_dir_mode(base, 0o1777)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(base)
    }
}

/// Creates and validates this user's directory under `base`, then returns its
/// path.
///
/// On Unix the path is named after the effective user id, such as
/// `/tmp/koshi/501`, and must be owned by that user with mode `0755`. It
/// creates no parents; a missing `base` returns [`io::ErrorKind::NotFound`] and
/// remains uncreated. A different owner, symbolic link, or regular file at the
/// user path returns [`io::ErrorKind::PermissionDenied`] with the path. Another
/// mode is replaced and checked. On Windows it creates `base` and missing
/// parents, returns `base`, and uses no per-user directory.
pub fn ensure_shared_user_dir(base: &Path) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        let dir = base.join(euid().to_string());
        create_dir_if_absent(&dir)?;
        verify_owner_is_this_user(&dir)?;
        verify_dir_mode(&dir, 0o755)?;
        Ok(dir)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(base)?;
        Ok(base.to_path_buf())
    }
}

/// Creates `path` and any missing parents. An existing directory is success;
/// a regular file at `path` returns [`io::ErrorKind::AlreadyExists`]. Other
/// filesystem errors are returned unchanged.
pub fn ensure_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
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
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        verify_owner_is_this_user(path)?;
        verify_dir_mode(path, 0o700)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
