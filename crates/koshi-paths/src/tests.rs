//! Tests for the path resolvers: each resolver routes to its own per-platform
//! location, the runtime fallback chain, that `KOSHI_*` environment variables
//! are ignored, and the ensure helpers. Every test that touches the process
//! environment holds `ENV_LOCK` and restores the prior values on drop, so tests
//! stay correct under the parallel test runner.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::*;

/// Serializes environment mutation across tests; the process environment is
/// global state.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds `ENV_LOCK` and a set of saved variables, restoring every one of
/// them (to its prior value or to unset) on drop.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        EnvGuard {
            _lock: ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            saved: Vec::new(),
        }
    }

    fn set(&mut self, var: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        self.save(var);
        std::env::set_var(var, value);
    }

    #[allow(dead_code)] // used only on Linux (XDG), where the runtime test unsets it
    fn unset(&mut self, var: &'static str) {
        self.save(var);
        std::env::remove_var(var);
    }

    fn save(&mut self, var: &'static str) {
        if self.saved.iter().all(|(name, _)| *name != var) {
            self.saved.push((var, std::env::var_os(var)));
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (var, prior) in self.saved.drain(..) {
            match prior {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
    }
}

#[test]
fn each_resolver_routes_to_its_own_platform_dir() {
    // Serialize against the tests that mutate the environment, even though
    // these resolvers read none of it.
    let _env = EnvGuard::new();

    // A config query answered from the data location is exactly the bug this
    // guards.
    let dirs = project_dirs().expect("test machine has a home directory");
    assert_eq!(config_dir(), Some(dirs.config_dir().to_path_buf()));
    assert_eq!(data_dir(), Some(dirs.data_dir().to_path_buf()));
    assert_eq!(cache_dir(), Some(dirs.cache_dir().to_path_buf()));
    assert_eq!(
        state_dir(),
        Some(
            dirs.state_dir()
                .unwrap_or_else(|| dirs.data_local_dir())
                .to_path_buf()
        )
    );
}

#[test]
fn koshi_dir_env_vars_are_ignored() {
    // koshi reads no `KOSHI_*` path override: setting one must not move any
    // resolved directory off its platform default.
    let mut env = EnvGuard::new();
    env.set("KOSHI_CONFIG_DIR", "/override/config");
    env.set("KOSHI_DATA_DIR", "/override/data");
    env.set("KOSHI_CACHE_DIR", "/override/cache");
    env.set("KOSHI_STATE_DIR", "/override/state");
    env.set("KOSHI_RUNTIME_DIR", "/override/runtime");

    let dirs = project_dirs().expect("test machine has a home directory");
    assert_eq!(config_dir(), Some(dirs.config_dir().to_path_buf()));
    assert_eq!(data_dir(), Some(dirs.data_dir().to_path_buf()));
    assert_eq!(cache_dir(), Some(dirs.cache_dir().to_path_buf()));
    assert_ne!(config_dir(), Some(PathBuf::from("/override/config")));
    assert_ne!(runtime_dir(), Some(PathBuf::from("/override/runtime")));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_paths_land_under_library() {
    let _env = EnvGuard::new();

    let config = config_dir().expect("home directory");
    assert!(
        config.ends_with("Library/Application Support/koshi"),
        "config_dir was {config:?}"
    );
    let cache = cache_dir().expect("home directory");
    assert!(
        cache.ends_with("Library/Caches/koshi"),
        "cache_dir was {cache:?}"
    );
    let state = state_dir().expect("home directory");
    assert!(
        state.ends_with("Library/Application Support/koshi"),
        "state_dir was {state:?}"
    );
}

#[cfg(windows)]
#[test]
fn windows_config_dir_lands_under_appdata_config() {
    let _env = EnvGuard::new();

    let config = config_dir().expect("home directory");
    assert!(
        config.ends_with("koshi\\config"),
        "config_dir was {config:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_runtime_dir_follows_xdg_runtime_dir() {
    // `$XDG_RUNTIME_DIR` is the OS's own base-directory rule, read by the
    // `directories` crate — not a koshi override.
    let mut env = EnvGuard::new();
    env.set("XDG_RUNTIME_DIR", "/run/user/1000");

    assert_eq!(runtime_dir(), Some(PathBuf::from("/run/user/1000/koshi")));
}

// On macOS and Windows there is no per-boot runtime base, so `runtime_dir`
// always falls through to `<data_dir>/run`. On Linux the same fallback fires
// only when `$XDG_RUNTIME_DIR` is unset.
#[cfg(not(target_os = "linux"))]
#[test]
fn runtime_dir_falls_back_to_data_dir_run() {
    let _env = EnvGuard::new();
    assert_eq!(runtime_dir(), data_dir().map(|d| d.join("run")));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_runtime_dir_falls_back_to_data_dir_run_without_xdg() {
    let mut env = EnvGuard::new();
    env.unset("XDG_RUNTIME_DIR");
    assert_eq!(runtime_dir(), data_dir().map(|d| d.join("run")));
}

#[test]
fn ensure_dir_creates_nested_and_accepts_existing() {
    let root = tempfile::tempdir().expect("tempdir");
    let nested = root.path().join("a").join("b");

    ensure_dir(&nested).expect("first create");
    ensure_dir(&nested).expect("existing dir is success");
    assert!(nested.is_dir());
}

#[test]
fn ensure_dir_reports_the_blocking_cause() {
    // A file where a parent directory must go: creation fails with the OS's
    // own error (the actionable cause), not a panic or silent success.
    let root = tempfile::tempdir().expect("tempdir");
    let file = root.path().join("occupied");
    std::fs::write(&file, b"x").expect("plant blocking file");

    let error = ensure_dir(&file.join("child")).expect_err("file blocks the dir");
    assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
}

#[test]
fn ensure_private_dir_creates_owner_only() {
    let root = tempfile::tempdir().expect("tempdir");
    let private = root.path().join("run");

    ensure_private_dir(&private).expect("create");
    assert!(private.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&private)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[cfg(unix)]
#[test]
fn ensure_private_dir_repairs_a_pre_existing_wide_open_directory() {
    // `ensure_dir_creates_nested_and_accepts_existing` proves the *directory*
    // half of "already existing is success" for `ensure_dir`. This is the
    // matching case for `ensure_private_dir`'s *permission* half: the prior
    // state is "the directory is already there, but at mode 0755 (world
    // readable/executable) from some earlier run" — `create_dir_all` alone
    // would silently leave it wide open. `ensure_private_dir` must reset it
    // to 0700 on every call, not just on first creation.
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("tempdir");
    let private = root.path().join("run");
    std::fs::create_dir_all(&private).expect("pre-create");
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755))
        .expect("plant wide-open mode");

    ensure_private_dir(&private).expect("repair");

    let mode = std::fs::metadata(&private)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o700,
        "a pre-existing 0755 dir must be tightened to 0700, not left as-is"
    );
}
