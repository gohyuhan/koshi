//! Platform path resolution — the single answer to "where do koshi's files
//! live".
//!
//! Every directory koshi reads or writes resolves through one of the
//! functions here; nothing else in the workspace hardcodes a location or
//! calls the [`directories`] crate itself. Each function returns the
//! platform's conventional per-user location — there is no environment
//! override, and koshi reads no `KOSHI_*` variable to relocate its files:
//!
//! | Function | Linux | macOS | Windows |
//! |---|---|---|---|
//! | [`config_dir`] | `~/.config/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\config` |
//! | [`data_dir`] | `~/.local/share/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\data` |
//! | [`cache_dir`] | `~/.cache/koshi` | `~/Library/Caches/koshi` | `%LOCALAPPDATA%\koshi\cache` |
//! | [`state_dir`] | `~/.local/state/koshi` | `~/Library/Application Support/koshi` | `%LOCALAPPDATA%\koshi\data` |
//! | [`runtime_dir`] | `$XDG_RUNTIME_DIR/koshi` | `<data_dir>/run` | `<data_dir>/run` |
//!
//! The Linux column shows the XDG defaults; a set `XDG_*` variable moves the
//! base as usual, since the [`directories`] crate implements the XDG spec —
//! that is the OS's own base-directory rule, not a koshi setting.
//!
//! Every function returns `Option`: `None` means the platform reports no home
//! directory for the current user (a stripped container, an unset `HOME`), so
//! no conventional per-user location exists.
//!
//! The resolvers are pure queries — they touch no filesystem and create
//! nothing. Startup creates the directories it needs through [`ensure_dir`]
//! and [`ensure_private_dir`].

use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// The platform's per-user directory set for the `koshi` project, or `None`
/// when the current user has no resolvable home directory.
fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "koshi")
}

/// The directory user configuration lives in: `koshi.kdl`, `theme.kdl`,
/// `keybinding.kdl`. See the [module table](self) for the per-platform
/// location. Example: on Linux this is `~/.config/koshi`.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.config_dir().to_path_buf())
}

/// The directory for durable user data koshi itself writes — session
/// persistence, crash reports. See the [module table](self). Example: on Linux
/// this is `~/.local/share/koshi`.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_dir().to_path_buf())
}

/// The directory for re-creatable caches. See the [module table](self).
/// Example: on macOS this is `~/Library/Caches/koshi`.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.cache_dir().to_path_buf())
}

/// The directory for machine-local mutable state — the log file lives here.
/// Linux has a dedicated state location (`~/.local/state/koshi`); macOS and
/// Windows have none, so the per-user local data directory stands in there
/// (`~/Library/Application Support/koshi`, `%LOCALAPPDATA%\koshi\data`).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    project_dirs().map(|d| {
        d.state_dir()
            .unwrap_or_else(|| d.data_local_dir())
            .to_path_buf()
    })
}

/// The directory for sockets and other per-boot runtime files. Linux uses
/// `$XDG_RUNTIME_DIR/koshi` when that variable is set; every other case —
/// macOS, Windows, Linux without `XDG_RUNTIME_DIR` — falls back to `run/`
/// under [`data_dir`]. Create it with [`ensure_private_dir`]: runtime files
/// are per-user private.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    project_dirs()
        .and_then(|d| d.runtime_dir().map(Path::to_path_buf))
        .or_else(|| data_dir().map(|d| d.join("run")))
}

/// Create `path` and any missing parents. Already existing is success.
pub fn ensure_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Create `path` and any missing parents, then restrict it to the owning
/// user: mode `0700` on Unix. Windows per-user directories already carry
/// owner-scoped ACLs, so creation alone suffices there (socket-equivalent
/// named pipes get their own ACL at listen time). Used for [`runtime_dir`].
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
