//! Tests for the path resolvers: each resolver routes to its own per-platform
//! location, the runtime directory answers the same path whatever
//! `XDG_RUNTIME_DIR` holds, `KOSHI_RUNTIME_DIR` names it only when absolute,
//! every other `KOSHI_*` variable is ignored, and the ensure helpers refuse
//! what another user could have planted while carrying the modes the
//! machine-wide shared directories need. Every test that touches the process
//! environment holds `ENV_LOCK` and restores the prior values on drop, so
//! tests stay correct under the parallel test runner.

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
    // `KOSHI_RUNTIME_DIR` is the only `KOSHI_*` path override koshi reads:
    // setting any other one must not move a resolved directory off its
    // platform default.
    let mut env = EnvGuard::new();
    env.set("KOSHI_CONFIG_DIR", "/override/config");
    env.set("KOSHI_DATA_DIR", "/override/data");
    env.set("KOSHI_CACHE_DIR", "/override/cache");
    env.set("KOSHI_STATE_DIR", "/override/state");

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
    assert_ne!(config_dir(), Some(PathBuf::from("/override/config")));
}

/// An absolute path on this platform. Windows counts a leading separator
/// alone as root-relative, so its value carries a drive letter.
#[cfg(unix)]
const ABSOLUTE_OVERRIDE: &str = "/override/runtime";
#[cfg(windows)]
const ABSOLUTE_OVERRIDE: &str = r"C:\override\runtime";

/// A relative path, which `KOSHI_RUNTIME_DIR` ignores on every platform.
const RELATIVE_OVERRIDE: &str = "override/runtime";

#[test]
fn the_runtime_dir_is_the_same_whatever_xdg_runtime_dir_holds() {
    // A user with `$XDG_RUNTIME_DIR` set, unset, or holding a relative path
    // reaches the same sockets.
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");
    env.unset("XDG_RUNTIME_DIR");
    let absent = runtime_dir();

    env.set("XDG_RUNTIME_DIR", "/run/user/1000");
    let set = runtime_dir();
    env.unset("XDG_RUNTIME_DIR");
    let unset = runtime_dir();
    env.set("XDG_RUNTIME_DIR", "run/user");
    let relative = runtime_dir();

    assert_eq!(set, absent);
    assert_eq!(unset, absent);
    assert_eq!(relative, absent);
}

/// The home directory moves the per-user directories and leaves the runtime
/// directory where it was. Moving `HOME` is the control: it proves the
/// resolvers read the changed environment at all.
#[cfg(unix)]
#[test]
fn the_runtime_dir_does_not_follow_the_home_directory() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");
    let runtime_before = runtime_dir();
    let data_before = data_dir();

    env.set("HOME", "/tmp/koshi-another-home");
    env.set("XDG_DATA_HOME", "/tmp/koshi-another-home/data");

    assert_ne!(data_dir(), data_before);
    assert_eq!(runtime_dir(), runtime_before);
}

#[test]
fn the_runtime_dir_variable_names_it_when_it_is_absolute() {
    let mut env = EnvGuard::new();
    env.set("KOSHI_RUNTIME_DIR", ABSOLUTE_OVERRIDE);

    assert_eq!(
        runtime_dir_with_rule(),
        Some((PathBuf::from(ABSOLUTE_OVERRIDE), RuntimeDirRule::Variable))
    );
}

#[test]
fn a_relative_runtime_dir_variable_is_ignored() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");
    let default = runtime_dir_with_rule();

    env.set("KOSHI_RUNTIME_DIR", RELATIVE_OVERRIDE);
    let answer = runtime_dir_with_rule();

    assert_eq!(answer, default);
    assert_ne!(answer.map(|(_, rule)| rule), Some(RuntimeDirRule::Variable));
}

#[cfg(unix)]
#[test]
fn the_runtime_dir_is_named_after_the_effective_user_id() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");

    assert_eq!(
        runtime_dir_with_rule(),
        Some((
            PathBuf::from(format!("/tmp/koshi-{}", euid())),
            RuntimeDirRule::UserId
        ))
    );
}

#[cfg(windows)]
#[test]
fn the_runtime_dir_is_run_under_the_data_dir() {
    let mut env = EnvGuard::new();
    env.unset("KOSHI_RUNTIME_DIR");

    assert_eq!(
        runtime_dir_with_rule(),
        data_dir().map(|dir| (dir.join("run"), RuntimeDirRule::DataDir))
    );
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
    // The OS's own kind for "a file sits where a directory must go": `ENOTDIR`
    // on Unix, `ERROR_ALREADY_EXISTS` on Windows.
    #[cfg(unix)]
    assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
    #[cfg(windows)]
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
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

#[test]
fn ensure_private_dir_creates_every_missing_parent() {
    // On Windows the runtime directory is `run/` under the data directory, and
    // neither one need exist yet, so the whole chain is created in one call.
    let root = tempfile::tempdir().expect("tempdir");
    let private = root.path().join("data").join("koshi").join("run");

    ensure_private_dir(&private).expect("create the whole chain");

    assert!(private.is_dir());
    #[cfg(unix)]
    assert_eq!(mode_of(&private), 0o700);
}

#[test]
fn ensure_private_dir_refuses_a_regular_file_planted_in_its_place() {
    // A file where the runtime directory must go is refused, and its bytes are
    // left alone: nothing truncates it and nothing uses it as a directory.
    let root = tempfile::tempdir().expect("tempdir");
    let private = root.path().join("run");
    std::fs::write(&private, b"not a directory").expect("plant the file");

    let error = ensure_private_dir(&private).expect_err("a file is not a directory");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&private).expect("read the planted file"),
        b"not a directory"
    );
}

#[cfg(unix)]
#[test]
fn ensure_private_dir_repairs_a_pre_existing_wide_open_directory() {
    // The permission half of "already existing is success", matching
    // `ensure_dir_creates_nested_and_accepts_existing` for `ensure_dir`. The
    // directory is already there from an earlier run at mode 0755, world
    // readable and executable. `ensure_private_dir` resets it to 0700 on
    // every call, not only on first creation.
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

/// Only root can hand a directory to another user, so this test says why it
/// was skipped and returns when this process is not root.
#[cfg(unix)]
#[test]
fn ensure_private_dir_refuses_a_directory_another_user_owns() {
    if euid() != 0 {
        eprintln!(
            "skipped `ensure_private_dir_refuses_a_directory_another_user_owns`: \
             planting a directory owned by another user needs root; re-run under sudo"
        );
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let private = root.path().join("run");
    plant_dir(&private, 0o700);
    std::os::unix::fs::chown(&private, Some(1), None).expect("hand the directory to another user");

    let error = ensure_private_dir(&private).expect_err("another user's directory is refused");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!(
            "{} is owned by uid 1, expected {}",
            private.display(),
            euid()
        )
    );
}

#[cfg(unix)]
#[test]
fn ensure_private_dir_refuses_a_symbolic_link() {
    // The link points at a directory that passes every other check, which is
    // the squat this refuses: the target can be moved afterwards.
    let root = tempfile::tempdir().expect("tempdir");
    let target = root.path().join("target");
    plant_dir(&target, 0o700);
    let private = root.path().join("run");
    std::os::unix::fs::symlink(&target, &private).expect("plant the link");

    let error = ensure_private_dir(&private).expect_err("a link is not a directory");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!("{} is not a directory", private.display())
    );
}

// --- The machine-wide shared directory ---

/// The permission bits of `path` itself, without following a link and without
/// the file-type bits. The sticky bit is inside the range this reads.
#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

/// Create `path` as a directory carrying exactly `mode`, standing in for a
/// directory an earlier run or another user left behind.
#[cfg(unix)]
fn plant_dir(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path).expect("plant the directory");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("plant the mode");
}

/// This process's effective user id, which names its directory under the
/// shared one.
#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: `geteuid` reads this process's own identity, takes no argument,
    // and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
#[test]
fn the_shared_directory_is_the_machine_wide_tmp_location() {
    assert_eq!(shared_sessions_dir(), Some(PathBuf::from("/tmp/koshi")));
}

#[cfg(windows)]
#[test]
fn the_shared_directory_is_koshi_under_program_data() {
    let mut env = EnvGuard::new();
    env.set("ProgramData", r"C:\TestProgramData");

    assert_eq!(
        shared_sessions_dir(),
        Some(PathBuf::from(r"C:\TestProgramData\koshi"))
    );
}

#[cfg(windows)]
#[test]
fn a_machine_reporting_no_program_data_has_no_shared_directory() {
    let mut env = EnvGuard::new();
    env.unset("ProgramData");

    assert_eq!(shared_sessions_dir(), None);
}

#[test]
fn ensure_shared_base_creates_it_and_accepts_it_again() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");

    ensure_shared_base(&base).expect("first create");
    ensure_shared_base(&base).expect("existing dir is success");

    assert!(base.is_dir());
    // Every local user may create an entry, and the sticky bit leaves each
    // entry removable only by the user who made it.
    #[cfg(unix)]
    assert_eq!(mode_of(&base), 0o1777);
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_repairs_a_directory_left_without_the_sticky_bit() {
    // Mode 0777 without the sticky bit lets any local user delete another
    // user's socket, so the mode is set rather than accepted as it stands.
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    plant_dir(&base, 0o777);

    ensure_shared_base(&base).expect("repair");

    assert_eq!(mode_of(&base), 0o1777);
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_refuses_a_symbolic_link_planted_in_its_place() {
    // The link points at a directory that passes every other check, which is
    // the squat this refuses: the target can be moved afterwards.
    let root = tempfile::tempdir().expect("tempdir");
    let target = root.path().join("target");
    plant_dir(&target, 0o1777);
    let base = root.path().join("koshi");
    std::os::unix::fs::symlink(&target, &base).expect("plant the link");

    let error = ensure_shared_base(&base).expect_err("a link is not a directory");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!("{} is not a directory", base.display())
    );
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_refuses_a_regular_file_planted_in_its_place() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    std::fs::write(&base, b"not a directory").expect("plant the file");

    let error = ensure_shared_base(&base).expect_err("a file is not a directory");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!("{} is not a directory", base.display())
    );
}

#[test]
fn ensure_shared_user_dir_hands_back_this_users_own_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    ensure_shared_base(&base).expect("create the base");

    let dir = ensure_shared_user_dir(&base).expect("first create");
    let again = ensure_shared_user_dir(&base).expect("existing dir is success");

    assert_eq!(dir, again);
    assert!(dir.is_dir());
    #[cfg(unix)]
    {
        assert_eq!(dir, base.join(euid().to_string()));
        // Only this user plants a socket here; every local user may reach one.
        assert_eq!(mode_of(&dir), 0o755);
    }
    // Pipe names share one machine-wide namespace, so Windows has no per-user
    // split and the base itself is the directory.
    #[cfg(windows)]
    assert_eq!(dir, base);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_dir_refuses_a_missing_base_instead_of_creating_it() {
    // Only `ensure_shared_base` may create the machine-wide directory, because
    // only it gives that directory mode 1777. A base created here as a parent
    // would carry the process umask instead, and one local user could then
    // delete another user's socket.
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");

    let error = ensure_shared_user_dir(&base).expect_err("a missing base is not created here");

    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!base.exists(), "the base must be left uncreated");
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_dir_opens_a_directory_left_closed_to_other_users() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    ensure_shared_base(&base).expect("create the base");
    plant_dir(&base.join(euid().to_string()), 0o700);

    let dir = ensure_shared_user_dir(&base).expect("repair");

    assert_eq!(mode_of(&dir), 0o755);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_dir_closes_a_directory_left_open_to_other_users_writing() {
    // Mode 0777 would let any local user plant a socket beside this user's
    // own, so the mode is narrowed rather than accepted as it stands.
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    ensure_shared_base(&base).expect("create the base");
    plant_dir(&base.join(euid().to_string()), 0o777);

    let dir = ensure_shared_user_dir(&base).expect("repair");

    assert_eq!(mode_of(&dir), 0o755);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_dir_refuses_a_symbolic_link_planted_in_its_place() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    ensure_shared_base(&base).expect("create the base");
    let target = root.path().join("target");
    plant_dir(&target, 0o755);
    let dir = base.join(euid().to_string());
    std::os::unix::fs::symlink(&target, &dir).expect("plant the link");

    let error = ensure_shared_user_dir(&base).expect_err("a link is not a directory");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!("{} is not a directory", dir.display())
    );
}

/// Only root can hand a directory to another user, so this test says why it
/// was skipped and returns when this process is not root.
#[cfg(unix)]
#[test]
fn ensure_shared_user_dir_refuses_a_directory_another_user_owns() {
    if euid() != 0 {
        eprintln!(
            "skipped `ensure_shared_user_dir_refuses_a_directory_another_user_owns`: \
             planting a directory owned by another user needs root; re-run under sudo"
        );
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("koshi");
    ensure_shared_base(&base).expect("create the base");
    let dir = base.join(euid().to_string());
    plant_dir(&dir, 0o755);
    std::os::unix::fs::chown(&dir, Some(1), None).expect("hand the directory to another user");

    let error = ensure_shared_user_dir(&base).expect_err("another user's directory is refused");

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        format!("{} is owned by uid 1, expected {}", dir.display(), euid())
    );
}
