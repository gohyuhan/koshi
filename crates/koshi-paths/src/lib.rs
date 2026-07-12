//! Platform path resolution — the single answer to "where do koshi's files
//! live".
//!
//! Every directory koshi reads or writes resolves through one of the
//! functions here; nothing else in the workspace hardcodes a location or
//! calls the [`directories`] crate itself. Each function checks its
//! `KOSHI_*_DIR` environment override first, then falls back to the
//! platform's conventional per-user location:
//!
//! | Function | Override | Linux | macOS | Windows |
//! |---|---|---|---|---|
//! | [`config_dir`] | `KOSHI_CONFIG_DIR` | `~/.config/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\config` |
//! | [`data_dir`] | `KOSHI_DATA_DIR` | `~/.local/share/koshi` | `~/Library/Application Support/koshi` | `%APPDATA%\koshi\data` |
//! | [`cache_dir`] | `KOSHI_CACHE_DIR` | `~/.cache/koshi` | `~/Library/Caches/koshi` | `%LOCALAPPDATA%\koshi\cache` |
//! | [`state_dir`] | `KOSHI_STATE_DIR` | `~/.local/state/koshi` | `~/Library/Application Support/koshi` | `%LOCALAPPDATA%\koshi\data` |
//! | [`runtime_dir`] | `KOSHI_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/koshi` | `<data_dir>/run` | `<data_dir>/run` |
//!
//! The Linux column shows the XDG defaults; a set `XDG_*` variable moves the
//! base as usual. Example: `KOSHI_CONFIG_DIR=/tmp/kcfg koshi` makes
//! [`config_dir`] return `/tmp/kcfg` on every platform. An override must be
//! an absolute path: an empty or relative value is ignored like an unset
//! variable, the XDG base-directory rule.
//!
//! Every function returns `Option`: `None` means the platform reports no home
//! directory for the current user (a stripped container, an unset `HOME`), so
//! no conventional per-user location exists. An environment override always
//! resolves, home directory or not.
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

/// The value of environment variable `var` as a path. An unset variable, an
/// empty value, and a relative value all yield `None` — the XDG base
/// directory rule, which treats a non-absolute base as invalid. Example:
/// `KOSHI_CONFIG_DIR=rel/path koshi` resolves to the platform default, the
/// same as no override; a relative base would silently move with the
/// process's working directory.
fn env_dir(var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(var)?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return None;
    }
    Some(path)
}

/// The directory user configuration lives in: `config.kdl`, `keys.kdl`, and
/// the `plugins/` tree. `KOSHI_CONFIG_DIR` overrides; see the [module
/// table](self) for the per-platform default. Example: on Linux with no
/// overrides this is `~/.config/koshi`.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    env_dir("KOSHI_CONFIG_DIR").or_else(|| project_dirs().map(|d| d.config_dir().to_path_buf()))
}

/// The directory for durable user data koshi itself writes — session
/// persistence, crash reports. `KOSHI_DATA_DIR` overrides; see the [module
/// table](self). Example: on Linux with no overrides this is
/// `~/.local/share/koshi`.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    env_dir("KOSHI_DATA_DIR").or_else(|| project_dirs().map(|d| d.data_dir().to_path_buf()))
}

/// The directory for re-creatable caches. `KOSHI_CACHE_DIR` overrides; see
/// the [module table](self). Example: on macOS with no overrides this is
/// `~/Library/Caches/koshi`.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    env_dir("KOSHI_CACHE_DIR").or_else(|| project_dirs().map(|d| d.cache_dir().to_path_buf()))
}

/// The directory for machine-local mutable state — the log file lives here.
/// `KOSHI_STATE_DIR` overrides. Linux has a dedicated state location
/// (`~/.local/state/koshi`); macOS and Windows have none, so the per-user
/// local data directory stands in there (`~/Library/Application
/// Support/koshi`, `%LOCALAPPDATA%\koshi\data`).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    env_dir("KOSHI_STATE_DIR").or_else(|| {
        project_dirs().map(|d| {
            d.state_dir()
                .unwrap_or_else(|| d.data_local_dir())
                .to_path_buf()
        })
    })
}

/// The directory for sockets and other per-boot runtime files.
/// `KOSHI_RUNTIME_DIR` overrides. Linux uses `$XDG_RUNTIME_DIR/koshi` when
/// that variable is set; every other case — macOS, Windows, Linux without
/// `XDG_RUNTIME_DIR` — falls back to `run/` under [`data_dir`]. Create it
/// with [`ensure_private_dir`]: runtime files are per-user private.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    env_dir("KOSHI_RUNTIME_DIR")
        .or_else(|| project_dirs().and_then(|d| d.runtime_dir().map(Path::to_path_buf)))
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
