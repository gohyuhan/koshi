//! Tests platform-specific path resolution, absolute and ignored environment
//! values, and directory creation. Unix ensure checks cover planted files,
//! links, wrong owners, and wrong modes. Startup paths cover an absolute
//! `KOSHI_RUNTIME_DIR` and a user's directory under a shared base.
//! Every test that reads or writes the process environment holds `ENV_LOCK` and
//! restores each variable's prior value on drop.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::*;

/// Serializes environment reads and writes across tests.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds `ENV_LOCK` and a set of saved variables, restoring every one of
/// them (to its prior value or to unset) on drop.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved_environment_values: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new() -> Self {
        EnvGuard {
            _lock: ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            saved_environment_values: Vec::new(),
        }
    }

    fn set_environment_variable(
        &mut self,
        environment_name: &'static str,
        environment_value: impl AsRef<std::ffi::OsStr>,
    ) {
        self.save_environment_variable(environment_name);
        std::env::set_var(environment_name, environment_value);
    }

    fn unset_environment_variable(&mut self, environment_name: &'static str) {
        self.save_environment_variable(environment_name);
        std::env::remove_var(environment_name);
    }

    fn save_environment_variable(&mut self, environment_name: &'static str) {
        if self
            .saved_environment_values
            .iter()
            .all(|(saved_environment_name, _)| *saved_environment_name != environment_name)
        {
            self.saved_environment_values
                .push((environment_name, std::env::var_os(environment_name)));
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (environment_name, prior_environment_value) in self.saved_environment_values.drain(..) {
            match prior_environment_value {
                Some(environment_value) => std::env::set_var(environment_name, environment_value),
                None => std::env::remove_var(environment_name),
            }
        }
    }
}

#[test]
fn each_resolver_routes_to_its_own_platform_dir() {
    // Holds `ENV_LOCK` while the resolvers read the environment.
    let _environment_guard = EnvGuard::new();

    let project_directories =
        resolve_project_directories().expect("test machine has a home directory");
    assert_eq!(
        resolve_config_directory(),
        Some(project_directories.config_dir().to_path_buf())
    );
    assert_eq!(
        resolve_data_directory(),
        Some(project_directories.data_dir().to_path_buf())
    );
    assert_eq!(
        resolve_state_directory(),
        Some(
            project_directories
                .state_dir()
                .unwrap_or_else(|| project_directories.data_local_dir())
                .to_path_buf()
        )
    );
}

#[test]
fn koshi_dir_env_vars_are_ignored() {
    // Setting `KOSHI_CONFIG_DIR`, `KOSHI_DATA_DIR` and `KOSHI_STATE_DIR`
    // leaves every resolved directory at its platform default.
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("KOSHI_CONFIG_DIR", "/override/config");
    environment_guard.set_environment_variable("KOSHI_DATA_DIR", "/override/data");
    environment_guard.set_environment_variable("KOSHI_STATE_DIR", "/override/state");

    let project_directories =
        resolve_project_directories().expect("test machine has a home directory");
    assert_eq!(
        resolve_config_directory(),
        Some(project_directories.config_dir().to_path_buf())
    );
    assert_eq!(
        resolve_data_directory(),
        Some(project_directories.data_dir().to_path_buf())
    );
    assert_eq!(
        resolve_state_directory(),
        Some(
            project_directories
                .state_dir()
                .unwrap_or_else(|| project_directories.data_local_dir())
                .to_path_buf()
        )
    );
    assert_ne!(
        resolve_config_directory(),
        Some(PathBuf::from("/override/config"))
    );
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
fn the_runtime_directory_is_the_same_whatever_xdg_runtime_directory_holds() {
    // `XDG_RUNTIME_DIR` set, unset, or holding a relative path gives the same
    // runtime directory.
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");
    environment_guard.unset_environment_variable("XDG_RUNTIME_DIR");
    let runtime_directory_without_override = resolve_runtime_directory();

    environment_guard.set_environment_variable("XDG_RUNTIME_DIR", "/run/user/1000");
    let runtime_directory_with_absolute_xdg_value = resolve_runtime_directory();
    environment_guard.unset_environment_variable("XDG_RUNTIME_DIR");
    let runtime_directory_after_unset_xdg_value = resolve_runtime_directory();
    environment_guard.set_environment_variable("XDG_RUNTIME_DIR", "run/user");
    let runtime_directory_with_relative_xdg_value = resolve_runtime_directory();

    assert_eq!(
        runtime_directory_with_absolute_xdg_value,
        runtime_directory_without_override
    );
    assert_eq!(
        runtime_directory_after_unset_xdg_value,
        runtime_directory_without_override
    );
    assert_eq!(
        runtime_directory_with_relative_xdg_value,
        runtime_directory_without_override
    );
}

/// Moving `HOME` and `XDG_DATA_HOME` moves [`resolve_data_directory`] and leaves
/// [`resolve_runtime_directory`] where it was.
#[cfg(unix)]
#[test]
fn the_runtime_directory_does_not_follow_the_home_directory() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");
    let runtime_directory_before_home_change = resolve_runtime_directory();
    let data_directory_before_home_change = resolve_data_directory();

    environment_guard.set_environment_variable("HOME", "/tmp/koshi-another-home");
    environment_guard.set_environment_variable("XDG_DATA_HOME", "/tmp/koshi-another-home/data");

    assert_ne!(resolve_data_directory(), data_directory_before_home_change);
    assert_eq!(
        resolve_runtime_directory(),
        runtime_directory_before_home_change
    );
}

#[test]
fn the_runtime_directory_variable_names_it_when_it_is_absolute() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("KOSHI_RUNTIME_DIR", ABSOLUTE_OVERRIDE);

    assert_eq!(
        resolve_runtime_directory_with_rule(),
        Some((
            PathBuf::from(ABSOLUTE_OVERRIDE),
            RuntimeDirectoryRule::EnvironmentVariable,
        ))
    );
}

#[test]
fn the_runtime_directory_follows_the_variable() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("KOSHI_RUNTIME_DIR", ABSOLUTE_OVERRIDE);

    assert_eq!(
        resolve_runtime_directory(),
        Some(PathBuf::from(ABSOLUTE_OVERRIDE))
    );
}

#[test]
fn a_relative_runtime_directory_variable_is_ignored() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");
    let default_runtime_directory_rule = resolve_runtime_directory_with_rule();

    environment_guard.set_environment_variable("KOSHI_RUNTIME_DIR", RELATIVE_OVERRIDE);
    let runtime_directory_rule_with_relative_override = resolve_runtime_directory_with_rule();

    assert_eq!(
        runtime_directory_rule_with_relative_override,
        default_runtime_directory_rule
    );
    assert_ne!(
        runtime_directory_rule_with_relative_override.map(|(_, directory_rule)| directory_rule),
        Some(RuntimeDirectoryRule::EnvironmentVariable)
    );
}

#[test]
fn an_empty_runtime_directory_variable_is_ignored() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");
    let default_runtime_directory_rule = resolve_runtime_directory_with_rule();

    environment_guard.set_environment_variable("KOSHI_RUNTIME_DIR", "");
    let runtime_directory_rule_with_empty_override = resolve_runtime_directory_with_rule();

    assert_eq!(
        runtime_directory_rule_with_empty_override,
        default_runtime_directory_rule
    );
    assert_ne!(
        runtime_directory_rule_with_empty_override.map(|(_, directory_rule)| directory_rule),
        Some(RuntimeDirectoryRule::EnvironmentVariable)
    );
}

/// `\override\runtime` has a root but no drive, which Windows does not count
/// as absolute.
#[cfg(windows)]
#[test]
fn a_runtime_directory_variable_without_a_drive_is_ignored() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");
    let default_runtime_directory_rule = resolve_runtime_directory_with_rule();

    environment_guard.set_environment_variable("KOSHI_RUNTIME_DIR", r"\override\runtime");
    let runtime_directory_rule_without_drive = resolve_runtime_directory_with_rule();

    assert_eq!(
        runtime_directory_rule_without_drive,
        default_runtime_directory_rule
    );
    assert_ne!(
        runtime_directory_rule_without_drive.map(|(_, rule)| rule),
        Some(RuntimeDirectoryRule::EnvironmentVariable)
    );
}

#[cfg(unix)]
#[test]
fn the_runtime_directory_is_named_after_the_effective_user_id() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");

    assert_eq!(
        resolve_runtime_directory_with_rule(),
        Some((
            PathBuf::from(format!("/tmp/koshi-{}", effective_user_id())),
            RuntimeDirectoryRule::UserId
        ))
    );
}

#[cfg(windows)]
#[test]
fn the_runtime_directory_is_run_under_the_data_directory() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("KOSHI_RUNTIME_DIR");

    assert_eq!(
        resolve_runtime_directory_with_rule(),
        resolve_data_directory().map(|data_directory| {
            (
                data_directory.join("run"),
                RuntimeDirectoryRule::DataDirectory,
            )
        })
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_paths_land_under_library() {
    let _environment_guard = EnvGuard::new();
    let platform_directories = directories::BaseDirs::new().expect("home directory");
    let home_directory_path = platform_directories.home_dir();

    assert_eq!(
        resolve_config_directory(),
        Some(home_directory_path.join("Library/Application Support/koshi"))
    );
    assert_eq!(
        resolve_data_directory(),
        Some(home_directory_path.join("Library/Application Support/koshi"))
    );
    assert_eq!(
        resolve_state_directory(),
        Some(home_directory_path.join("Library/Application Support/koshi"))
    );
}

#[cfg(windows)]
#[test]
fn windows_config_dir_lands_under_appdata_config() {
    let _environment_guard = EnvGuard::new();
    let platform_directories = directories::BaseDirs::new().expect("home directory");

    assert_eq!(
        resolve_config_directory(),
        Some(platform_directories.data_dir().join("koshi").join("config"))
    );
}

#[cfg(windows)]
#[test]
fn windows_state_dir_lands_under_local_appdata_data() {
    let _environment_guard = EnvGuard::new();
    let platform_directories = directories::BaseDirs::new().expect("home directory");

    assert_eq!(
        resolve_state_directory(),
        Some(
            platform_directories
                .data_local_dir()
                .join("koshi")
                .join("data")
        )
    );
}

#[cfg(target_os = "linux")]
#[test]
fn absolute_xdg_variables_move_the_per_user_directories() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("XDG_CONFIG_HOME", "/xdg/config");
    environment_guard.set_environment_variable("XDG_DATA_HOME", "/xdg/data");
    environment_guard.set_environment_variable("XDG_STATE_HOME", "/xdg/state");

    assert_eq!(
        resolve_config_directory(),
        Some(PathBuf::from("/xdg/config/koshi"))
    );
    assert_eq!(
        resolve_data_directory(),
        Some(PathBuf::from("/xdg/data/koshi"))
    );
    assert_eq!(
        resolve_state_directory(),
        Some(PathBuf::from("/xdg/state/koshi"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn relative_xdg_variables_are_ignored() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("HOME", "/tmp/koshi-xdg-home");
    environment_guard.set_environment_variable("XDG_CONFIG_HOME", "xdg/config");
    environment_guard.set_environment_variable("XDG_DATA_HOME", "xdg/data");
    environment_guard.set_environment_variable("XDG_STATE_HOME", "xdg/state");

    assert_eq!(
        resolve_config_directory(),
        Some(PathBuf::from("/tmp/koshi-xdg-home/.config/koshi"))
    );
    assert_eq!(
        resolve_data_directory(),
        Some(PathBuf::from("/tmp/koshi-xdg-home/.local/share/koshi"))
    );
    assert_eq!(
        resolve_state_directory(),
        Some(PathBuf::from("/tmp/koshi-xdg-home/.local/state/koshi"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn empty_xdg_variables_are_ignored() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("HOME", "/tmp/koshi-empty-xdg-home");
    environment_guard.set_environment_variable("XDG_CONFIG_HOME", "");
    environment_guard.set_environment_variable("XDG_DATA_HOME", "");
    environment_guard.set_environment_variable("XDG_STATE_HOME", "");

    assert_eq!(
        resolve_config_directory(),
        Some(PathBuf::from("/tmp/koshi-empty-xdg-home/.config/koshi"))
    );
    assert_eq!(
        resolve_data_directory(),
        Some(PathBuf::from(
            "/tmp/koshi-empty-xdg-home/.local/share/koshi"
        ))
    );
    assert_eq!(
        resolve_state_directory(),
        Some(PathBuf::from(
            "/tmp/koshi-empty-xdg-home/.local/state/koshi"
        ))
    );
}

#[test]
fn ensure_directory_creates_nested_and_accepts_existing() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let nested_directory_path = test_directory.path().join("a").join("b");

    ensure_directory(&nested_directory_path).expect("first create");
    ensure_directory(&nested_directory_path).expect("existing directory is success");
    assert!(nested_directory_path.is_dir());
}

#[test]
fn ensure_directory_reports_the_blocking_cause() {
    // A file where a parent directory must go fails with the OS's own error
    // kind: `NotADirectory` (`ENOTDIR`) on Unix, `AlreadyExists`
    // (`ERROR_ALREADY_EXISTS`) on Windows.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let blocking_file_path = test_directory.path().join("occupied");
    std::fs::write(&blocking_file_path, b"x").expect("plant blocking file");

    let directory_error =
        ensure_directory(&blocking_file_path.join("child")).expect_err("file blocks the directory");
    #[cfg(unix)]
    assert_eq!(directory_error.kind(), std::io::ErrorKind::NotADirectory);
    #[cfg(windows)]
    assert_eq!(directory_error.kind(), std::io::ErrorKind::AlreadyExists);
}

#[test]
fn ensure_directory_refuses_a_file_at_the_path() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let blocking_file_path = test_directory.path().join("occupied");
    std::fs::write(&blocking_file_path, b"x").expect("plant the file");

    let directory_error =
        ensure_directory(&blocking_file_path).expect_err("a file is not a directory");

    assert_eq!(directory_error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&blocking_file_path).expect("read the planted file"),
        b"x"
    );
}

#[test]
fn ensure_private_directory_creates_owner_only() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let private_directory_path = test_directory.path().join("run");

    ensure_private_directory(&private_directory_path).expect("create");
    assert!(private_directory_path.is_dir());
    #[cfg(unix)]
    assert_eq!(get_directory_mode(&private_directory_path), 0o700);
}

#[test]
fn ensure_private_directory_creates_every_missing_parent() {
    // One call creates the whole chain `data/koshi/run`.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let private_directory_path = test_directory.path().join("data").join("koshi").join("run");

    ensure_private_directory(&private_directory_path).expect("create the whole chain");

    assert!(private_directory_path.is_dir());
    #[cfg(unix)]
    assert_eq!(get_directory_mode(&private_directory_path), 0o700);
}

#[test]
fn the_runtime_directory_the_variable_names_is_created_private() {
    // The startup path every consumer runs: `KOSHI_RUNTIME_DIR` names the
    // directory, `resolve_runtime_directory` answers it, `ensure_private_directory` creates it and
    // every missing parent below it.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let runtime_directory_path = test_directory.path().join("state").join("run");
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("KOSHI_RUNTIME_DIR", &runtime_directory_path);

    let resolved_runtime_directory =
        resolve_runtime_directory().expect("the variable names the runtime directory");
    assert_eq!(resolved_runtime_directory, runtime_directory_path);
    ensure_private_directory(&resolved_runtime_directory).expect("create the runtime directory");

    assert!(resolved_runtime_directory.is_dir());
    #[cfg(unix)]
    assert_eq!(get_directory_mode(&resolved_runtime_directory), 0o700);
}

#[test]
fn ensure_private_directory_reports_a_file_parent() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let blocking_file_path = test_directory.path().join("occupied");
    let child_directory_path = blocking_file_path.join("child");
    std::fs::write(&blocking_file_path, b"x").expect("plant blocking file");

    let error =
        ensure_private_directory(&child_directory_path).expect_err("file blocks the directory");
    #[cfg(unix)]
    assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
    #[cfg(windows)]
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&blocking_file_path).expect("read the blocking file"),
        b"x"
    );
    assert!(
        !child_directory_path.exists(),
        "the blocked child directory must not be created"
    );
}

#[test]
fn ensure_private_directory_refuses_a_regular_file_planted_in_its_place() {
    // The planted file is refused and its bytes are left as they were.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let private_directory_path = test_directory.path().join("run");
    std::fs::write(&private_directory_path, b"not a directory").expect("plant the file");

    let error =
        ensure_private_directory(&private_directory_path).expect_err("a file is not a directory");

    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&private_directory_path).expect("read the planted file"),
        b"not a directory"
    );
}

#[cfg(unix)]
#[test]
fn ensure_private_directory_repairs_a_pre_existing_wide_open_directory() {
    // A directory already at mode `0755` is reset to `0700`.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let private_directory_path = test_directory.path().join("run");
    plant_directory(&private_directory_path, 0o755);

    ensure_private_directory(&private_directory_path).expect("repair");

    assert_eq!(
        get_directory_mode(&private_directory_path),
        0o700,
        "a pre-existing 0755 directory must be tightened to 0700, not left as-is"
    );
}

#[cfg(unix)]
#[test]
fn ensure_private_directory_clears_the_sticky_bit() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let private_directory_path = test_directory.path().join("run");
    plant_directory(&private_directory_path, 0o1700);
    assert_eq!(get_directory_mode(&private_directory_path), 0o1700);

    ensure_private_directory(&private_directory_path).expect("repair");

    assert_eq!(get_directory_mode(&private_directory_path), 0o700);
}

/// Runs only as root: handing a directory to another user needs root. As any
/// other user it prints a skip notice and returns.
#[cfg(unix)]
#[test]
fn ensure_private_directory_refuses_a_directory_another_user_owns() {
    if effective_user_id() != 0 {
        eprintln!(
            "skipped `ensure_private_directory_refuses_a_directory_another_user_owns`: \
             planting a directory owned by another user needs root; re-run under sudo"
        );
        return;
    }
    let test_directory = tempfile::tempdir().expect("tempdir");
    let private_directory_path = test_directory.path().join("run");
    plant_directory(&private_directory_path, 0o700);
    std::os::unix::fs::chown(&private_directory_path, Some(1), None)
        .expect("hand the directory to another user");

    let ownership_error = ensure_private_directory(&private_directory_path)
        .expect_err("another user's directory is refused");

    assert_eq!(ownership_error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        ownership_error.to_string(),
        format!(
            "{} is owned by uid 1, expected {}",
            private_directory_path.display(),
            effective_user_id()
        )
    );
}

#[cfg(unix)]
#[test]
fn ensure_private_directory_refuses_a_symbolic_link() {
    // The link's target is a directory that passes every other check.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let target_directory_path = test_directory.path().join("target");
    plant_directory(&target_directory_path, 0o700);
    let private_directory_path = test_directory.path().join("run");
    std::os::unix::fs::symlink(&target_directory_path, &private_directory_path)
        .expect("plant the link");

    let private_directory_error =
        ensure_private_directory(&private_directory_path).expect_err("a link is not a directory");

    assert_eq!(
        private_directory_error.kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        private_directory_error.to_string(),
        format!("{} is not a directory", private_directory_path.display())
    );
}

#[cfg(unix)]
#[test]
fn ensure_private_directory_refuses_a_dangling_symbolic_link() {
    // The link is left in place and its missing target stays uncreated.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let target_directory_path = test_directory.path().join("missing");
    let private_directory_path = test_directory.path().join("run");
    std::os::unix::fs::symlink(&target_directory_path, &private_directory_path)
        .expect("plant the link");

    let private_directory_error = ensure_private_directory(&private_directory_path)
        .expect_err("a dangling link is not a directory");

    assert_eq!(private_directory_error.kind(), io::ErrorKind::AlreadyExists);
    assert!(std::fs::symlink_metadata(&private_directory_path)
        .expect("read the link")
        .file_type()
        .is_symlink());
    assert!(
        !target_directory_path.exists(),
        "the link's target must be left uncreated"
    );
}

// --- The machine-wide shared directory ---

/// The permission bits of `directory_path` itself, without following a link and without
/// the file-type bits. The sticky bit is inside the range this reads.
#[cfg(unix)]
fn get_directory_mode(directory_path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(directory_path)
        .unwrap_or_else(|metadata_error| {
            panic!("reading {}: {metadata_error}", directory_path.display())
        })
        .permissions()
        .mode()
        & 0o7777
}

/// Create `directory_path`, with any missing parents, as a directory carrying exactly
/// `directory_mode`.
#[cfg(unix)]
fn plant_directory(directory_path: &Path, directory_mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(directory_path).expect("plant the directory");
    std::fs::set_permissions(
        directory_path,
        std::fs::Permissions::from_mode(directory_mode),
    )
    .expect("plant the mode");
}

#[cfg(unix)]
#[test]
fn the_shared_directory_uses_the_machine_wide_location() {
    assert_eq!(
        resolve_shared_sessions_directory(),
        Some(PathBuf::from("/tmp/koshi"))
    );
}

#[cfg(windows)]
#[test]
fn the_shared_directory_is_koshi_under_program_data() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("ProgramData", r"C:\TestProgramData");

    assert_eq!(
        resolve_shared_sessions_directory(),
        Some(PathBuf::from(r"C:\TestProgramData\koshi"))
    );
}

#[cfg(windows)]
#[test]
fn a_machine_reporting_no_program_data_has_no_shared_directory() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.unset_environment_variable("ProgramData");

    assert_eq!(resolve_shared_sessions_directory(), None);
}

#[cfg(windows)]
#[test]
fn a_relative_program_data_gives_no_shared_directory() {
    let mut environment_guard = EnvGuard::new();
    environment_guard.set_environment_variable("ProgramData", r"TestProgramData\koshi");

    assert_eq!(resolve_shared_sessions_directory(), None);
}

#[test]
fn ensure_shared_base_creates_it_and_accepts_it_again() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");

    ensure_shared_base(&shared_base_path).expect("first create");
    ensure_shared_base(&shared_base_path).expect("existing dir is success");

    assert!(shared_base_path.is_dir());
    // Mode `1777`: world-writable with the sticky bit.
    #[cfg(unix)]
    assert_eq!(get_directory_mode(&shared_base_path), 0o1777);
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_repairs_a_directory_left_without_the_sticky_bit() {
    // A directory at `0777` has `1777` set on it.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    plant_directory(&shared_base_path, 0o777);

    ensure_shared_base(&shared_base_path).expect("repair");

    assert_eq!(get_directory_mode(&shared_base_path), 0o1777);
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_refuses_a_missing_parent_instead_of_creating_it() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let missing_parent_directory_path = test_directory.path().join("missing");
    let shared_base_path = missing_parent_directory_path.join("koshi");

    let shared_base_error =
        ensure_shared_base(&shared_base_path).expect_err("a missing parent is not created here");

    assert_eq!(shared_base_error.kind(), io::ErrorKind::NotFound);
    assert!(
        !missing_parent_directory_path.exists(),
        "the parent must be left uncreated"
    );
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_refuses_a_symbolic_link_planted_in_its_place() {
    // The link's target is a directory that passes every other check.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let target_directory_path = test_directory.path().join("target");
    plant_directory(&target_directory_path, 0o1777);
    let shared_base_path = test_directory.path().join("koshi");
    std::os::unix::fs::symlink(&target_directory_path, &shared_base_path).expect("plant the link");

    let shared_base_error =
        ensure_shared_base(&shared_base_path).expect_err("a link is not a directory");

    assert_eq!(shared_base_error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        shared_base_error.to_string(),
        format!("{} is not a directory", shared_base_path.display())
    );
}

#[cfg(unix)]
#[test]
fn ensure_shared_base_refuses_a_regular_file_planted_in_its_place() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    std::fs::write(&shared_base_path, b"not a directory").expect("plant the file");

    let shared_base_error =
        ensure_shared_base(&shared_base_path).expect_err("a file is not a directory");

    assert_eq!(shared_base_error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        shared_base_error.to_string(),
        format!("{} is not a directory", shared_base_path.display())
    );
}

#[test]
fn ensure_shared_user_directory_hands_back_this_users_own_directory() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");

    let user_directory = ensure_shared_user_directory(&shared_base_path).expect("first create");
    let existing_user_directory =
        ensure_shared_user_directory(&shared_base_path).expect("existing directory is success");

    assert_eq!(user_directory, existing_user_directory);
    assert!(user_directory.is_dir());
    #[cfg(unix)]
    {
        assert_eq!(
            user_directory,
            shared_base_path.join(effective_user_id().to_string())
        );
        // Mode `0755`.
        assert_eq!(get_directory_mode(&user_directory), 0o755);
    }
    // On Windows the shared base path itself is the directory.
    #[cfg(windows)]
    assert_eq!(user_directory, shared_base_path);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_refuses_a_missing_base_instead_of_creating_it() {
    // A missing shared base path is refused and stays uncreated.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");

    let shared_user_directory_error = ensure_shared_user_directory(&shared_base_path)
        .expect_err("a missing shared base path is not created here");

    assert_eq!(shared_user_directory_error.kind(), io::ErrorKind::NotFound);
    assert!(
        !shared_base_path.exists(),
        "the shared base path must be left uncreated"
    );
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_refuses_a_base_that_is_a_regular_file() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    std::fs::write(&shared_base_path, b"not a directory").expect("plant the file");

    let shared_user_directory_error = ensure_shared_user_directory(&shared_base_path)
        .expect_err("a file holds no user directory");

    assert_eq!(
        shared_user_directory_error.kind(),
        io::ErrorKind::NotADirectory
    );
    assert_eq!(
        std::fs::read(&shared_base_path).expect("read the planted file"),
        b"not a directory"
    );
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_opens_a_directory_left_closed_to_other_users() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");
    plant_directory(
        &shared_base_path.join(effective_user_id().to_string()),
        0o700,
    );

    let user_directory = ensure_shared_user_directory(&shared_base_path).expect("repair");

    assert_eq!(get_directory_mode(&user_directory), 0o755);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_closes_a_directory_left_open_to_other_users_writing() {
    // A directory at `0777` has `0755` set on it.
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");
    plant_directory(
        &shared_base_path.join(effective_user_id().to_string()),
        0o777,
    );

    let user_directory = ensure_shared_user_directory(&shared_base_path).expect("repair");

    assert_eq!(get_directory_mode(&user_directory), 0o755);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_clears_the_sticky_bit() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");
    let planted = shared_base_path.join(effective_user_id().to_string());
    plant_directory(&planted, 0o1755);
    assert_eq!(get_directory_mode(&planted), 0o1755);

    let user_directory = ensure_shared_user_directory(&shared_base_path).expect("repair");

    assert_eq!(user_directory, planted);
    assert_eq!(get_directory_mode(&user_directory), 0o755);
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_refuses_a_symbolic_link_planted_in_its_place() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");
    let target_directory_path = test_directory.path().join("target");
    plant_directory(&target_directory_path, 0o755);
    let user_directory = shared_base_path.join(effective_user_id().to_string());
    std::os::unix::fs::symlink(&target_directory_path, &user_directory).expect("plant the link");

    let shared_user_directory_error =
        ensure_shared_user_directory(&shared_base_path).expect_err("a link is not a directory");

    assert_eq!(
        shared_user_directory_error.kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        shared_user_directory_error.to_string(),
        format!("{} is not a directory", user_directory.display())
    );
}

#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_refuses_a_regular_file_planted_in_its_place() {
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");
    let user_directory = shared_base_path.join(effective_user_id().to_string());
    std::fs::write(&user_directory, b"not a directory").expect("plant the file");

    let shared_user_directory_error =
        ensure_shared_user_directory(&shared_base_path).expect_err("a file is not a directory");

    assert_eq!(
        shared_user_directory_error.kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        shared_user_directory_error.to_string(),
        format!("{} is not a directory", user_directory.display())
    );
    assert_eq!(
        std::fs::read(&user_directory).expect("read the planted file"),
        b"not a directory"
    );
}

/// Runs only as root: handing a directory to another user needs root. As any
/// other user it prints a skip notice and returns.
#[cfg(unix)]
#[test]
fn ensure_shared_user_directory_refuses_a_directory_another_user_owns() {
    if effective_user_id() != 0 {
        eprintln!(
            "skipped `ensure_shared_user_directory_refuses_a_directory_another_user_owns`: \
             planting a directory owned by another user needs root; re-run under sudo"
        );
        return;
    }
    let test_directory = tempfile::tempdir().expect("tempdir");
    let shared_base_path = test_directory.path().join("koshi");
    ensure_shared_base(&shared_base_path).expect("create the shared base path");
    let user_directory = shared_base_path.join(effective_user_id().to_string());
    plant_directory(&user_directory, 0o755);
    std::os::unix::fs::chown(&user_directory, Some(1), None)
        .expect("hand the directory to another user");

    let ownership_error = ensure_shared_user_directory(&shared_base_path)
        .expect_err("another user's directory is refused");

    assert_eq!(ownership_error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        ownership_error.to_string(),
        format!(
            "{} is owned by uid 1, expected {}",
            user_directory.display(),
            effective_user_id()
        )
    );
}
