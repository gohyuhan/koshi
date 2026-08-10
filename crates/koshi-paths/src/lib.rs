//! Platform path resolution — where koshi's files live.
//!
//! Every directory koshi reads or writes comes from a function here. Each one
//! returns the platform's conventional location — per-user everywhere except
//! [`shared_sessions_dir`], which is machine-wide. koshi reads no `KOSHI_*`
//! variable to relocate its files:
//!
//! | Function | Linux | macOS | Windows |
//! |---|---|---|---|
//! | [`config_dir`] | `~/.config/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\config` |
//! | [`data_dir`] | `~/.local/share/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\data` |
//! | [`cache_dir`] | `~/.cache/koshi` | `~/Library/Caches/koshi` | `%LOCALAPPDATA%\koshi\cache` |
//! | [`state_dir`] | `~/.local/state/koshi` | `~/Library/Application Support/koshi` | `%LOCALAPPDATA%\koshi\data` |
//! | [`runtime_dir`] | `$XDG_RUNTIME_DIR/koshi` | `<data_dir>/run` | `<data_dir>/run` |
//! | [`shared_sessions_dir`] | `/tmp/koshi` | `/tmp/koshi` | `%ProgramData%\koshi` |
//!
//! The Linux column shows the XDG defaults for the per-user directories.
//! Setting an `XDG_*` variable moves their base, because the [`directories`]
//! crate implements the XDG spec. On Linux and macOS [`shared_sessions_dir`]
//! is a fixed path that no variable moves.
//!
//! `None` from a per-user resolver means the platform reports no home
//! directory for the current user — a stripped container, an unset `HOME`.
//! `None` from [`shared_sessions_dir`] means Windows reports no `ProgramData`
//! location.
//!
//! The resolvers touch no filesystem and create nothing. Startup creates the
//! directories it needs through [`ensure_dir`], [`ensure_private_dir`],
//! [`ensure_shared_base`] and [`ensure_shared_user_dir`].

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The platform's per-user directory set for the `koshi` project, or `None`
/// when the current user has no resolvable home directory.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "koshi")
}

/// The directory user configuration lives in: `koshi.kdl` and
/// `keybinding.kdl` at the top, color themes under `themes/`, session layouts
/// under `profile/`. On Linux this is `~/.config/koshi`; see the
/// [module table](self) for every platform.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().to_path_buf())
}

/// The directory for durable data koshi writes — session persistence, crash
/// reports. On Linux this is `~/.local/share/koshi`; see the
/// [module table](self).
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_dir().to_path_buf())
}

/// The directory for re-creatable caches. On macOS this is
/// `~/Library/Caches/koshi`; see the [module table](self).
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.cache_dir().to_path_buf())
}

/// The directory for machine-local mutable state — the log file lives here.
/// Linux has a dedicated state location, `~/.local/state/koshi`. macOS and
/// Windows have none and use the per-user local data directory instead:
/// `~/Library/Application Support/koshi`, `%LOCALAPPDATA%\koshi\data`.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    project_dirs().map(|d| {
        d.state_dir()
            .unwrap_or_else(|| d.data_local_dir())
            .to_path_buf()
    })
}

/// The directory for sockets and other per-boot runtime files. Linux uses
/// `$XDG_RUNTIME_DIR/koshi` when that variable holds an absolute path.
/// Everything else — macOS, Windows, Linux without an absolute
/// `XDG_RUNTIME_DIR` — uses `run/` under [`data_dir`]. Create it with
/// [`ensure_private_dir`]; runtime files are per-user private.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    let dirs = project_dirs()?;
    Some(match dirs.runtime_dir() {
        Some(runtime) => runtime.to_path_buf(),
        None => dirs.data_dir().join("run"),
    })
}

/// The machine-wide directory holding what koshi shares between local users:
/// the shared session sockets, and on Windows the marker files that name the
/// sessions listening on a pipe. On Unix this is `/tmp/koshi`. On Windows it
/// is `koshi` under `%ProgramData%`, and `None` means that variable is unset.
///
/// Create it with [`ensure_shared_base`], then take this user's subdirectory
/// from [`ensure_shared_user_dir`]. A `shared-sessions-dir` in `koshi.kdl`
/// names a directory of its own and is used instead.
#[must_use]
pub fn shared_sessions_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(PathBuf::from("/tmp/koshi"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData").map(|base| PathBuf::from(base).join("koshi"))
    }
}

/// Refuse a shared directory, naming the path and what is wrong with it.
#[cfg(unix)]
fn shared_dir_refused(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {reason}", path.display()),
    )
}

/// Create `path`, without creating its parents. A path something already
/// occupies is success, and what occupies it is the caller's to check.
#[cfg(unix)]
fn create_dir_if_absent(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

/// Confirm `path` is a directory carrying exactly `mode`, setting the mode
/// when it differs and reading it back afterwards.
///
/// The check reads the link itself rather than following it, so a symbolic
/// link planted at `path` is refused as "not a directory".
#[cfg(unix)]
fn verify_shared_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Err(shared_dir_refused(path, "is not a directory"));
    }
    if metadata.permissions().mode() & 0o7777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
            shared_dir_refused(path, &format!("mode could not be set: {error}"))
        })?;
        let found = std::fs::symlink_metadata(path)?.permissions().mode() & 0o7777;
        if found != mode {
            return Err(shared_dir_refused(
                path,
                &format!("mode is {found:04o}, expected {mode:04o}"),
            ));
        }
    }
    Ok(())
}

/// Create `base`, the machine-wide shared directory, and confirm it is safe to
/// use.
///
/// On Unix `base` must be a directory with mode `1777`: every local user may
/// create entries in it, and the sticky bit keeps one user from deleting
/// another's. A directory carrying another mode has `1777` set on it, and the
/// mode is read back afterwards, so one whose mode cannot be corrected — a
/// directory another user owns — is refused instead of used. On Windows it
/// only creates the directory, which inherits the ACLs of its parent.
pub fn ensure_shared_base(base: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        create_dir_if_absent(base)?;
        verify_shared_dir_mode(base, 0o1777)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(base)
    }
}

/// Create this user's directory under `base` and confirm it is safe to use,
/// returning its path.
///
/// On Unix the directory is named after the effective user id — `/tmp/koshi/501`
/// — and must be owned by that user with mode `0755`: only its owner plants
/// sockets there, and every local user may reach them. A directory another
/// user owns is refused. On Windows there is no per-user split: pipe names
/// share one machine-wide namespace, so this creates `base` and returns it.
pub fn ensure_shared_user_dir(base: &Path) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let euid = unsafe { libc::geteuid() };
        let dir = base.join(euid.to_string());
        create_dir_if_absent(&dir)?;
        let owner = std::fs::symlink_metadata(&dir)?.uid();
        if owner != euid {
            return Err(shared_dir_refused(
                &dir,
                &format!("is owned by uid {owner}, expected {euid}"),
            ));
        }
        verify_shared_dir_mode(&dir, 0o755)?;
        Ok(dir)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir_all(base)?;
        Ok(base.to_path_buf())
    }
}

/// Create `path` and any missing parents. Already existing is success.
pub fn ensure_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Create `path` and any missing parents, then restrict it to the owning
/// user: mode `0700` on Unix. On Windows it only creates the directory, which
/// already carries owner-scoped ACLs. Used for [`runtime_dir`].
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
