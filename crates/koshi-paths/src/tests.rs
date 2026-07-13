//! Tests for the path resolvers: override precedence, empty-override
//! handling, per-platform defaults, the runtime fallback chain, and the
//! ensure helpers. Every test that touches the process environment holds
//! `ENV_LOCK` and restores the prior values on drop, so tests stay correct
//! under the parallel test runner.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::*;

/// Serializes environment mutation across tests; the process environment is
/// global state.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// An absolute override fixture on every platform: `/o/<leaf>` on Unix,
/// `C:\o\<leaf>` on Windows — a Unix-style `/o/...` has no drive prefix on
/// Windows and would be rejected as relative.
fn abs(leaf: &str) -> PathBuf {
    #[cfg(windows)]
    let root = PathBuf::from("C:\\o");
    #[cfg(not(windows))]
    let root = PathBuf::from("/o");
    root.join(leaf)
}

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
fn env_override_wins_for_every_dir() {
    let mut env = EnvGuard::new();
    env.set("KOSHI_CONFIG_DIR", abs("config"));
    env.set("KOSHI_DATA_DIR", abs("data"));
    env.set("KOSHI_CACHE_DIR", abs("cache"));
    env.set("KOSHI_STATE_DIR", abs("state"));
    env.set("KOSHI_RUNTIME_DIR", abs("runtime"));

    assert_eq!(config_dir(), Some(abs("config")));
    assert_eq!(data_dir(), Some(abs("data")));
    assert_eq!(cache_dir(), Some(abs("cache")));
    assert_eq!(state_dir(), Some(abs("state")));
    assert_eq!(runtime_dir(), Some(abs("runtime")));
}

#[test]
fn empty_override_is_ignored() {
    let mut env = EnvGuard::new();
    env.set("KOSHI_CONFIG_DIR", "");

    assert_eq!(
        config_dir(),
        project_dirs().map(|d| d.config_dir().to_path_buf())
    );
}

#[test]
fn relative_override_is_ignored() {
    // The XDG base-directory rule: a non-absolute base is invalid and treated
    // as unset — `KOSHI_CONFIG_DIR=rel/path` must not create dirs that move
    // with the process's working directory.
    let mut env = EnvGuard::new();
    env.set("KOSHI_CONFIG_DIR", "rel/path");

    assert_eq!(
        config_dir(),
        project_dirs().map(|d| d.config_dir().to_path_buf())
    );
}

#[test]
fn whitespace_only_override_is_ignored() {
    // `" "` is a distinct code path from `""`: `OsString::is_empty` is false
    // for it (length 1), so it falls through to the `is_absolute()` check
    // instead — that check must also reject it, exactly as an empty value
    // does. `KOSHI_CONFIG_DIR=" "` must not create a directory literally
    // named a single space next to the working directory.
    let mut env = EnvGuard::new();
    env.set("KOSHI_CONFIG_DIR", " ");

    assert_eq!(
        config_dir(),
        project_dirs().map(|d| d.config_dir().to_path_buf())
    );
}

#[test]
fn unicode_override_is_used_verbatim() {
    // An absolute override is passed straight through with no normalization:
    // `KOSHI_CONFIG_DIR=/o/café/設定` must resolve to exactly that path, not a
    // mangled or truncated one.
    let mut env = EnvGuard::new();
    let unicode = abs("café/設定");
    env.set("KOSHI_CONFIG_DIR", &unicode);

    assert_eq!(config_dir(), Some(unicode));
}

#[test]
fn defaults_use_the_platform_config_location() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_CONFIG_DIR");
    env.unset("KOSHI_DATA_DIR");
    env.unset("KOSHI_CACHE_DIR");
    env.unset("KOSHI_STATE_DIR");

    // Each resolver must route to its own directories method — a config
    // query answered from the data location is exactly the bug this guards.
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

#[cfg(target_os = "macos")]
#[test]
fn macos_paths_land_under_library() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_CONFIG_DIR");
    env.unset("KOSHI_CACHE_DIR");
    env.unset("KOSHI_STATE_DIR");

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

#[cfg(target_os = "linux")]
#[test]
fn linux_runtime_dir_follows_xdg_runtime_dir() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");
    env.set("XDG_RUNTIME_DIR", "/run/user/1000");

    assert_eq!(runtime_dir(), Some(PathBuf::from("/run/user/1000/koshi")));
}

#[cfg(windows)]
#[test]
fn windows_config_dir_lands_under_appdata_config() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_CONFIG_DIR");

    let config = config_dir().expect("home directory");
    assert!(
        config.ends_with("koshi\\config"),
        "config_dir was {config:?}"
    );
}

#[test]
fn runtime_dir_falls_back_to_data_dir_run() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");
    env.set("KOSHI_DATA_DIR", abs("data"));
    // Without the XDG runtime base, every platform falls through to the
    // data-dir leg; unsetting it makes the assertion exact on Linux too.
    #[cfg(target_os = "linux")]
    env.unset("XDG_RUNTIME_DIR");

    assert_eq!(runtime_dir(), Some(abs("data").join("run")));
}

#[test]
fn empty_runtime_dir_override_is_ignored_not_just_unset_ones() {
    // `runtime_dir_falls_back_to_data_dir_run` above only ever tests an
    // *unset* `KOSHI_RUNTIME_DIR`. An explicitly *empty* override
    // (`KOSHI_RUNTIME_DIR=""`) is a different code path — it must be rejected
    // by `env_dir`'s `is_empty()` check, not merely absent from the
    // environment — before falling through to the same XDG/data-dir chain.
    let mut env = EnvGuard::new();
    env.set("KOSHI_RUNTIME_DIR", "");
    env.set("KOSHI_DATA_DIR", abs("data"));
    #[cfg(target_os = "linux")]
    env.unset("XDG_RUNTIME_DIR");

    assert_eq!(runtime_dir(), Some(abs("data").join("run")));
}

#[test]
fn runtime_dir_with_zero_overrides_falls_back_to_platform_data_dir_run() {
    // No test above exercises `runtime_dir()` with every `KOSHI_*` override
    // unset: `runtime_dir_falls_back_to_data_dir_run` always pins
    // `KOSHI_DATA_DIR` to a fixture path. This drives the full un-overridden
    // chain end to end — env override (absent) -> `ProjectDirs::runtime_dir()`
    // (Linux XDG only; unset here so it also misses) -> the platform default
    // `data_dir` + `"run"` — so a break in any link show up here, not just in
    // the fixture-pinned path.
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");
    env.unset("KOSHI_DATA_DIR");
    #[cfg(target_os = "linux")]
    env.unset("XDG_RUNTIME_DIR");

    let dirs = project_dirs().expect("test machine has a home directory");
    assert_eq!(
        runtime_dir(),
        Some(dirs.data_dir().to_path_buf().join("run"))
    );
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
