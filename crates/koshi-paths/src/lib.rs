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
//! `XDG_STATE_HOME` replaces its matching base; a relative value is ignored. On Windows, an
//! absolute `APPDATA` replaces the `%APPDATA%` known folder and an absolute `LOCALAPPDATA`
//! replaces the `%LOCALAPPDATA%` known folder; a relative or empty value is ignored.
//! [`resolve_runtime_directory`] reads no `XDG_*` variable. Linux and macOS use a fixed path
//! for [`resolve_shared_sessions_directory`].
//!
//! A per-user resolver returns `None` when the platform cannot provide the
//! required base directories. On Linux and macOS, `HOME` must be set and
//! non-empty, or the passwd database must provide a home directory. On
//! Windows, the matching variable must hold an absolute path, or the `%APPDATA%` and
//! `%LOCALAPPDATA%` known folders must resolve. On Windows, [`resolve_shared_sessions_directory`]
//! returns `None` when `%ProgramData%` is unset or not absolute.
//!
//! Resolvers do not inspect the filesystem or create directories. Startup uses
//! [`ensure_private_directory`], [`ensure_shared_base`], and
//! [`ensure_shared_user_directory`] to create the directories it needs.

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The environment variable naming [`resolve_runtime_directory`]. Its value is used only
/// when it is an absolute path.
const RUNTIME_DIRECTORY_ENVIRONMENT_VARIABLE: &str = "KOSHI_RUNTIME_DIR";

/// The Windows environment variable naming the `%APPDATA%` folder, which holds
/// [`resolve_config_directory`] and [`resolve_data_directory`].
#[cfg(windows)]
const ROAMING_APP_DATA_ENVIRONMENT_VARIABLE: &str = "APPDATA";

/// The Windows environment variable naming the `%LOCALAPPDATA%` folder, which holds
/// [`resolve_state_directory`].
#[cfg(windows)]
const LOCAL_APP_DATA_ENVIRONMENT_VARIABLE: &str = "LOCALAPPDATA";

/// The Windows environment variable naming the `%ProgramData%` folder, which
/// holds [`resolve_shared_sessions_directory`].
#[cfg(windows)]
const PROGRAM_DATA_ENVIRONMENT_VARIABLE: &str = "ProgramData";

/// The path the environment variable `variable_name` holds, or `None` when it is
/// unset or not absolute. An empty value is not absolute.
fn find_absolute_path_variable(variable_name: &str) -> Option<PathBuf> {
    std::env::var_os(variable_name)
        .map(PathBuf::from)
        .filter(|variable_path| variable_path.is_absolute())
}

/// On Windows, `koshi\<project_directory_name>` under the folder an absolute
/// `variable_name` names, such as `C:\Users\user\AppData\Roaming\koshi\config` for
/// `APPDATA` and `config`. `None` when the variable is unset or not absolute.
#[cfg(windows)]
fn find_windows_project_directory(
    variable_name: &str,
    project_directory_name: &str,
) -> Option<PathBuf> {
    find_absolute_path_variable(variable_name).map(|app_data_directory| {
        app_data_directory
            .join("koshi")
            .join(project_directory_name)
    })
}

/// Returns the platform directory set for the `koshi` project, or `None` when
/// the required platform base directories cannot be resolved.
fn resolve_project_directories() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "koshi")
}

/// This process's effective user id. It names [`resolve_runtime_directory`] and this user's
/// directory under [`resolve_shared_sessions_directory`].
#[cfg(unix)]
fn get_effective_user_id() -> u32 {
    // SAFETY: `geteuid` reads this process's own identity, takes no argument,
    // and cannot fail.
    unsafe { libc::geteuid() }
}

/// Returns the directory for `koshi.kdl`, `keybinding.kdl`, `themes/`, and
/// `profile/`. On Linux this is `~/.config/koshi`; see the [module table](self)
/// for every platform.
#[must_use]
pub fn resolve_config_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(config_directory) =
        find_windows_project_directory(ROAMING_APP_DATA_ENVIRONMENT_VARIABLE, "config")
    {
        return Some(config_directory);
    }
    resolve_project_directories().map(|project| project.config_dir().to_path_buf())
}

/// Returns the directory for durable data, including session persistence and
/// crash reports. On Linux this is `~/.local/share/koshi`; see the [module
/// table](self).
#[must_use]
pub fn resolve_data_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(data_directory) =
        find_windows_project_directory(ROAMING_APP_DATA_ENVIRONMENT_VARIABLE, "data")
    {
        return Some(data_directory);
    }
    resolve_project_directories().map(|project| project.data_dir().to_path_buf())
}

/// Returns the directory for machine-local mutable state, including logs.
/// Linux uses `~/.local/state/koshi`. macOS uses
/// `~/Library/Application Support/koshi`; Windows uses
/// `%LOCALAPPDATA%\koshi\data`.
#[must_use]
pub fn resolve_state_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(state_directory) =
        find_windows_project_directory(LOCAL_APP_DATA_ENVIRONMENT_VARIABLE, "data")
    {
        return Some(state_directory);
    }
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
/// An absolute `KOSHI_RUNTIME_DIR` gives [`RuntimeDirectoryRule::EnvironmentVariable`]. Without it,
/// Unix returns `/tmp/koshi-<effective uid>` with [`RuntimeDirectoryRule::UserId`] and never
/// returns `None`. Windows returns `run/` under [`resolve_data_directory`] with
/// [`RuntimeDirectoryRule::DataDirectory`], or `None` when the project data directory cannot be
/// resolved. Create the directory with [`ensure_private_directory`]; runtime files are per-user
/// private.
#[must_use]
pub fn resolve_runtime_directory_with_rule() -> Option<(PathBuf, RuntimeDirectoryRule)> {
    if let Some(runtime_directory) =
        find_absolute_path_variable(RUNTIME_DIRECTORY_ENVIRONMENT_VARIABLE)
    {
        return Some((runtime_directory, RuntimeDirectoryRule::EnvironmentVariable));
    }
    #[cfg(unix)]
    {
        Some((
            PathBuf::from(format!("/tmp/koshi-{}", get_effective_user_id())),
            RuntimeDirectoryRule::UserId,
        ))
    }
    #[cfg(windows)]
    {
        Some((
            resolve_data_directory()?.join("run"),
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
        find_absolute_path_variable(PROGRAM_DATA_ENVIRONMENT_VARIABLE)
            .map(|program_data_path| program_data_path.join("koshi"))
    }
}

/// Returns a [`io::ErrorKind::PermissionDenied`] error with the message
/// `<path> <reason>`, such as `/tmp/koshi-501 is not a directory`.
#[cfg(unix)]
fn build_directory_refused_error(refused_path: &Path, refusal_reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {refusal_reason}", refused_path.display()),
    )
}

/// Opens `directory_path` itself as a directory, never the target of a
/// symbolic link at that path.
///
/// A symbolic link, a regular file, or anything else that is not a directory
/// returns [`io::ErrorKind::PermissionDenied`] with `<path> is not a
/// directory`. Every other open error, such as a directory its owner may not
/// read, is returned unchanged.
#[cfg(unix)]
fn open_directory_without_following(directory_path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(directory_path)
        .map_err(|open_error| match open_error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => {
                build_directory_refused_error(directory_path, "is not a directory")
            }
            _ => open_error,
        })
}

/// Confirms that the effective user id owns `opened_directory`, the directory
/// [`open_directory_without_following`] opened at `directory_path`.
///
/// A different owner returns [`io::ErrorKind::PermissionDenied`] with both
/// user ids, such as `/tmp/koshi-501 is owned by uid 0, expected 501`. A
/// metadata read error is returned unchanged.
#[cfg(unix)]
fn verify_owner_is_this_user(
    opened_directory: &std::fs::File,
    directory_path: &Path,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let expected_user_id = get_effective_user_id();
    let owner_user_id = opened_directory.metadata()?.uid();
    if owner_user_id != expected_user_id {
        return Err(build_directory_refused_error(
            directory_path,
            &format!("is owned by uid {owner_user_id}, expected {expected_user_id}"),
        ));
    }
    Ok(())
}

/// Creates `directory_path` without creating its parents. A directory, regular
/// file, or symbolic link already at `directory_path` is success. A missing
/// parent returns [`io::ErrorKind::NotFound`].
#[cfg(unix)]
fn create_directory_if_absent(directory_path: &Path) -> io::Result<()> {
    match std::fs::create_dir(directory_path) {
        Err(filesystem_error) if filesystem_error.kind() != io::ErrorKind::AlreadyExists => {
            Err(filesystem_error)
        }
        _ => Ok(()),
    }
}

/// Confirms that `opened_directory`, the directory
/// [`open_directory_without_following`] opened at `directory_path`, has
/// exactly `directory_mode`, and sets that mode when it differs.
///
/// The mode is read and set through `opened_directory`: a symbolic link
/// planted at `directory_path` after the open is never followed. A mode-change
/// failure returns [`io::ErrorKind::PermissionDenied`] with `<path> mode could
/// not be set: <error>`. A different mode after the change returns the same
/// error kind with `<path> mode is <found:04o>, expected <mode:04o>`. Metadata
/// read errors are returned unchanged.
#[cfg(unix)]
fn verify_directory_mode(
    opened_directory: &std::fs::File,
    directory_path: &Path,
    directory_mode: u32,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if opened_directory.metadata()?.permissions().mode() & 0o7777 != directory_mode {
        opened_directory
            .set_permissions(std::fs::Permissions::from_mode(directory_mode))
            .map_err(|permission_error| {
                build_directory_refused_error(
                    directory_path,
                    &format!("mode could not be set: {permission_error}"),
                )
            })?;
        let observed_mode = opened_directory.metadata()?.permissions().mode() & 0o7777;
        if observed_mode != directory_mode {
            return Err(build_directory_refused_error(
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
/// to `1777`. Open and metadata read errors are returned unchanged. On Windows it
/// creates `shared_base_path` and missing parents with the parent's ACLs.
pub fn ensure_shared_base(shared_base_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        create_directory_if_absent(shared_base_path)?;
        let shared_base_directory = open_directory_without_following(shared_base_path)?;
        verify_directory_mode(&shared_base_directory, shared_base_path, 0o1777)
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
/// creates no parents; a missing `shared_base_path` returns [`io::ErrorKind::NotFound`] and
/// remains uncreated. A different owner, symbolic link, or regular file at the
/// user path returns [`io::ErrorKind::PermissionDenied`] with the path. Another
/// mode is replaced and checked. On Windows it creates `shared_base_path` and missing
/// parents, returns `shared_base_path`, and uses no per-user directory.
pub fn ensure_shared_user_directory(shared_base_path: &Path) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        let user_directory_path = shared_base_path.join(get_effective_user_id().to_string());
        create_directory_if_absent(&user_directory_path)?;
        let user_directory = open_directory_without_following(&user_directory_path)?;
        verify_owner_is_this_user(&user_directory, &user_directory_path)?;
        verify_directory_mode(&user_directory, &user_directory_path, 0o755)?;
        Ok(user_directory_path)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(shared_base_path)?;
        Ok(shared_base_path.to_path_buf())
    }
}

/// Creates `directory_path` and missing parents, then validates the final
/// directory as this user's private runtime directory.
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
        let private_directory = open_directory_without_following(directory_path)?;
        verify_owner_is_this_user(&private_directory, directory_path)?;
        verify_directory_mode(&private_directory, directory_path, 0o700)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
