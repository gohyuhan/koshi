//! Tests for the execute check.

use super::*;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

/// Write an empty file named `file_name` into `directory_path` with the
/// permission bits `permission_mode`, and hand back its path.
fn write_file_with_mode(directory_path: &Path, file_name: &str, permission_mode: u32) -> PathBuf {
    let file_path = directory_path.join(file_name);
    std::fs::write(&file_path, b"").expect("the file is written");
    std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(permission_mode))
        .expect("the mode is set");
    file_path
}

#[test]
fn can_user_execute_reads_the_execute_permission_of_a_path() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let runnable_path = write_file_with_mode(test_root.path(), "runnable", 0o755);
    let plain_path = write_file_with_mode(test_root.path(), "plain", 0o644);

    assert!(can_user_execute(&runnable_path));
    assert!(!can_user_execute(&plain_path));
    assert!(can_user_execute(test_root.path()));
    assert!(!can_user_execute(&test_root.path().join("missing")));
    assert!(!can_user_execute(Path::new("/opt/koshi\0/bin")));
}
