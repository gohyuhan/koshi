//! Tests for [`super`] atomic file replacement.

use super::*;
use crate::error::StorageError;
use tempfile::TempDir;

/// Names every entry in `directory_path` (staging names are random, so tests assert the
/// exact surviving set rather than matching a fixed staging path).
fn list_directory_entries(directory_path: &Path) -> Vec<String> {
    std::fs::read_dir(directory_path)
        .unwrap()
        .filter_map(|directory_entry| directory_entry.ok())
        .map(|directory_entry| directory_entry.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn write_atomic_creates_file_with_exact_bytes() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");

    write_atomic(&destination_path, b"a=2\n").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"a=2\n");
}

#[test]
fn write_atomic_replaces_existing_file_wholesale() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    std::fs::write(&destination_path, b"a=1\n").unwrap();

    write_atomic(&destination_path, b"a=2\n").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"a=2\n");
}

#[test]
fn write_atomic_leaves_no_staging_file_on_success() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");

    write_atomic(&destination_path, b"x").unwrap();

    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["cfg.kdl".to_string()]
    );
}

#[test]
fn write_atomic_cleans_staging_file_and_keeps_target_when_rename_fails() {
    let test_directory = TempDir::new().unwrap();
    // `destination_path` is a directory, so replacement fails after the staging file is written and
    // synced.
    let destination_path = test_directory.path().join("target");
    std::fs::create_dir(&destination_path).unwrap();

    let storage_error = write_atomic(&destination_path, b"x").unwrap_err();

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert!(
        detail.starts_with(&format!("replace {}: ", destination_path.display())),
        "unexpected error detail: {detail}"
    );
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["target".to_string()]
    );
    assert!(destination_path.is_dir(), "target must be left untouched");
}

#[test]
fn write_atomic_reports_io_error_when_staging_directory_is_missing() {
    let test_directory = TempDir::new().unwrap();
    // The parent directory is missing, so staging-file creation fails and creates
    // nothing.
    let destination_path = test_directory.path().join("missing").join("cfg.kdl");

    let storage_error = write_atomic(&destination_path, b"x").unwrap_err();

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert!(
        detail.starts_with(&format!(
            "create temp in {}: ",
            test_directory.path().join("missing").display()
        )),
        "unexpected error detail: {detail}"
    );
    assert_eq!(
        list_directory_entries(test_directory.path()),
        Vec::<String>::new(),
        "nothing must be created"
    );
}

#[cfg(unix)]
#[test]
fn write_atomic_preserves_existing_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    std::fs::write(&destination_path, b"old").unwrap();
    std::fs::set_permissions(&destination_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    write_atomic(&destination_path, b"new").unwrap();

    let file_mode = std::fs::metadata(&destination_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        file_mode, 0o644,
        "atomic overwrite must keep the file's mode"
    );
}

#[cfg(unix)]
#[test]
fn write_atomic_new_file_is_private_by_default() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("secret.kdl");

    write_atomic(&destination_path, b"data").unwrap();

    let file_mode = std::fs::metadata(&destination_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        file_mode, 0o600,
        "a fresh file must be created user-private"
    );
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_symlink_with_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let referent = test_directory.path().join("shared.txt");
    let link = test_directory.path().join("cfg.kdl");
    std::fs::write(&referent, b"other").unwrap();
    std::fs::set_permissions(&referent, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&referent, &link).unwrap();

    write_atomic(&link, b"secret").unwrap();

    // Replacement removes the link and creates a private regular file. The
    // referent keeps its mode and bytes.
    let link_metadata = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        link_metadata.file_type().is_file(),
        "symlink must become a regular file"
    );
    assert_eq!(
        link_metadata.permissions().mode() & 0o777,
        0o600,
        "replacement must not inherit the link target's mode"
    );
    assert_eq!(std::fs::read(&link).unwrap(), b"secret");
    assert_eq!(std::fs::read(&referent).unwrap(), b"other");
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_dangling_symlink_with_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    // The link points at a missing file.
    let link = test_directory.path().join("cfg.kdl");
    std::os::unix::fs::symlink(test_directory.path().join("gone.txt"), &link).unwrap();

    write_atomic(&link, b"data").unwrap();

    // Replacement creates a private regular file with the new bytes.
    let link_metadata = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        link_metadata.file_type().is_file(),
        "dangling symlink must become a regular file"
    );
    assert_eq!(link_metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&link).unwrap(), b"data");
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_fifo_with_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    // A world-readable FIFO occupies the target path.
    let destination_path = test_directory.path().join("cfg.kdl");
    let fifo_command_status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("666")
        .arg(&destination_path)
        .status()
        .unwrap();
    assert!(fifo_command_status.success(), "mkfifo must succeed");

    write_atomic(&destination_path, b"secret").unwrap();

    // Replacement creates a private regular file; the FIFO mode does not carry
    // over.
    let destination_metadata = std::fs::symlink_metadata(&destination_path).unwrap();
    assert!(
        destination_metadata.file_type().is_file(),
        "FIFO must become a regular file"
    );
    assert_eq!(
        destination_metadata.permissions().mode() & 0o777,
        0o600,
        "replacement must not inherit the FIFO's mode"
    );
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"secret");
}

#[test]
fn write_atomic_resolves_a_relative_path_against_the_current_dir() {
    // Use a process-specific relative file name and remove any entry from an earlier run.
    let relative_file_name = format!("koshi-atomic-relative-{}.tmp", std::process::id());
    let relative_path = Path::new(&relative_file_name);
    let _ = std::fs::remove_file(relative_path);

    write_atomic(relative_path, b"relative\n").unwrap();

    let absolute_path = std::env::current_dir().unwrap().join(&relative_file_name);
    let file_bytes = std::fs::read(&absolute_path).unwrap();
    std::fs::remove_file(&absolute_path).unwrap();
    assert_eq!(file_bytes, b"relative\n");
}

#[cfg(unix)]
#[test]
fn write_atomic_reports_io_error_when_a_path_component_is_a_file() {
    let test_directory = TempDir::new().unwrap();
    // A regular file blocks a directory component. Unix target stat returns an
    // I/O error before staging-file creation.
    let blocker = test_directory.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let destination_path = blocker.join("cfg.kdl");

    let storage_error = write_atomic(&destination_path, b"data").unwrap_err();

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert!(
        detail.starts_with(&format!("stat {}: ", destination_path.display())),
        "unexpected error detail: {detail}"
    );
    // The blocker file is untouched and no staging file was placed beside it.
    assert_eq!(std::fs::read(&blocker).unwrap(), b"x");
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["not-a-dir".to_string()]
    );
}

#[test]
fn concurrent_writers_never_leave_partial_content() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    // Each writer supplies a distinct 4 KiB buffer. The final file must equal
    // one complete buffer, with no staging file left beside it.
    let file_contents: Vec<Vec<u8>> = (0..8u8)
        .map(|content_index| vec![b'a' + content_index; 4096])
        .collect();

    std::thread::scope(|thread_scope| {
        for file_bytes in &file_contents {
            let destination_path_ref = &destination_path;
            thread_scope.spawn(move || write_atomic(destination_path_ref, file_bytes).unwrap());
        }
    });

    let final_bytes = std::fs::read(&destination_path).unwrap();
    assert!(
        file_contents.contains(&final_bytes),
        "final file must be exactly one writer's complete content"
    );
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["cfg.kdl".to_string()]
    );
}

#[test]
fn write_atomic_stages_the_file_in_the_targets_own_directory() {
    let test_directory = TempDir::new().unwrap();
    // The missing target directory is named in the staging-file error. No
    // entry is created under `test_directory`.
    let missing_directory_path = test_directory.path().join("missing");
    let destination_path = missing_directory_path.join("cfg.kdl");

    let storage_error = write_atomic(&destination_path, b"x").unwrap_err();

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert!(
        detail.starts_with(&format!(
            "create temp in {}: ",
            missing_directory_path.display()
        )),
        "unexpected error detail: {detail}"
    );
    assert_eq!(
        list_directory_entries(test_directory.path()),
        Vec::<String>::new()
    );
}

#[test]
fn write_atomic_names_the_target_when_the_rename_is_blocked() {
    let test_directory = TempDir::new().unwrap();
    // A directory at `destination_path` makes replacement fail after the staging file is written and
    // synced.
    let destination_path = test_directory.path().join("target");
    std::fs::create_dir(&destination_path).unwrap();

    let storage_error = write_atomic(&destination_path, b"x").unwrap_err();

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert!(
        detail.starts_with(&format!("replace {}: ", destination_path.display())),
        "unexpected error detail: {detail}"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn write_atomic_replaces_only_the_named_hard_link() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    let alias = test_directory.path().join("alias.kdl");
    std::fs::write(&destination_path, b"old").unwrap();
    std::fs::hard_link(&destination_path, &alias).unwrap();

    write_atomic(&destination_path, b"new").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"new");
    assert_eq!(std::fs::read(&alias).unwrap(), b"old");
}

#[cfg(windows)]
#[test]
fn write_atomic_rejects_a_read_only_file_without_changing_it() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    std::fs::write(&destination_path, b"old").unwrap();
    let mut read_only_permissions = std::fs::metadata(&destination_path).unwrap().permissions();
    read_only_permissions.set_readonly(true);
    std::fs::set_permissions(&destination_path, read_only_permissions).unwrap();

    let storage_error = write_atomic(&destination_path, b"new").unwrap_err();

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert!(
        detail.starts_with(&format!("replace {}: ", destination_path.display())),
        "unexpected error detail: {detail}"
    );
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"old");

    let mut writable_permissions = std::fs::metadata(&destination_path).unwrap().permissions();
    writable_permissions.set_readonly(false);
    std::fs::set_permissions(&destination_path, writable_permissions).unwrap();
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_a_read_only_file_and_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    std::fs::write(&destination_path, b"old").unwrap();
    // The target mode is 0400 and its directory remains writable. Replacement
    // succeeds and carries the mode onto the new bytes.
    std::fs::set_permissions(&destination_path, std::fs::Permissions::from_mode(0o400)).unwrap();

    write_atomic(&destination_path, b"new").unwrap();

    let destination_metadata = std::fs::symlink_metadata(&destination_path).unwrap();
    assert_eq!(destination_metadata.permissions().mode() & 0o777, 0o400);
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"new");
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["cfg.kdl".to_string()]
    );
}

#[test]
fn write_atomic_replaces_a_longer_file_with_empty_data() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    std::fs::write(&destination_path, b"a=1\nb=2\n").unwrap();

    write_atomic(&destination_path, b"").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"");
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["cfg.kdl".to_string()]
    );
}

#[test]
fn write_atomic_writes_binary_bytes_verbatim() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("state.bin");
    let file_bytes = [0u8, 0xff, b'\n', b'\r', 0x1b, 0x00, 0x7f];

    write_atomic(&destination_path, &file_bytes).unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), file_bytes);
}

#[test]
fn write_atomic_writes_a_one_mebibyte_buffer_whole() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("big.bin");
    let file_bytes: Vec<u8> = (0u8..=250).cycle().take(1024 * 1024).collect();

    write_atomic(&destination_path, &file_bytes).unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), file_bytes);
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["big.bin".to_string()]
    );
}

#[test]
fn write_atomic_accepts_a_non_ascii_file_name() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("設定✓.kdl");

    write_atomic(&destination_path, b"a=1\n").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"a=1\n");
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["設定✓.kdl".to_string()]
    );
}

#[test]
fn write_atomic_twice_leaves_only_the_last_bytes_and_no_staging_file() {
    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");

    write_atomic(&destination_path, b"first").unwrap();
    write_atomic(&destination_path, b"second").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"second");
    assert_eq!(
        list_directory_entries(test_directory.path()),
        vec!["cfg.kdl".to_string()]
    );
}

#[test]
fn write_atomic_succeeds_beside_a_target_that_blocked_an_earlier_replace() {
    let test_directory = TempDir::new().unwrap();
    // A directory at `blocked_directory_path` makes replacement fail. The sibling write then
    // succeeds without a leftover staging file.
    let blocked_directory_path = test_directory.path().join("blocked");
    std::fs::create_dir(&blocked_directory_path).unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");

    write_atomic(&blocked_directory_path, b"x").unwrap_err();
    write_atomic(&destination_path, b"a=1\n").unwrap();

    assert_eq!(std::fs::read(&destination_path).unwrap(), b"a=1\n");
    let mut directory_entries = list_directory_entries(test_directory.path());
    directory_entries.sort();
    assert_eq!(
        directory_entries,
        vec!["blocked".to_string(), "cfg.kdl".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn write_atomic_copies_a_mode_wider_than_the_umask() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let destination_path = test_directory.path().join("cfg.kdl");
    std::fs::write(&destination_path, b"old").unwrap();
    // Existing mode 0666 is copied to the staging file and remains unchanged by the
    // umask.
    std::fs::set_permissions(&destination_path, std::fs::Permissions::from_mode(0o666)).unwrap();

    write_atomic(&destination_path, b"new").unwrap();

    let file_mode = std::fs::metadata(&destination_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o666);
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"new");
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_a_symlink_to_a_directory_with_a_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let referent = test_directory.path().join("shared");
    std::fs::create_dir(&referent).unwrap();
    std::fs::write(referent.join("inner.txt"), b"other").unwrap();
    let link = test_directory.path().join("cfg.kdl");
    std::os::unix::fs::symlink(&referent, &link).unwrap();

    write_atomic(&link, b"secret").unwrap();

    // Replacement removes the link and leaves the referent directory and entry
    // unchanged.
    let link_metadata = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        link_metadata.file_type().is_file(),
        "symlink must become a regular file"
    );
    assert_eq!(link_metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&link).unwrap(), b"secret");
    assert_eq!(
        list_directory_entries(&referent),
        vec!["inner.txt".to_string()]
    );
    assert_eq!(std::fs::read(referent.join("inner.txt")).unwrap(), b"other");
}

#[cfg(unix)]
#[test]
fn write_atomic_through_a_symlinked_parent_directory_lands_in_the_real_directory() {
    let test_directory = TempDir::new().unwrap();
    let real_directory_path = test_directory.path().join("real");
    std::fs::create_dir(&real_directory_path).unwrap();
    let link = test_directory.path().join("link");
    std::os::unix::fs::symlink(&real_directory_path, &link).unwrap();
    let destination_path = link.join("cfg.kdl");

    write_atomic(&destination_path, b"a=1\n").unwrap();

    assert_eq!(
        std::fs::read(real_directory_path.join("cfg.kdl")).unwrap(),
        b"a=1\n"
    );
    assert_eq!(
        list_directory_entries(&real_directory_path),
        vec!["cfg.kdl".to_string()]
    );
    let mut directory_entries = list_directory_entries(test_directory.path());
    directory_entries.sort();
    assert_eq!(
        directory_entries,
        vec!["link".to_string(), "real".to_string()]
    );
}

#[test]
fn an_empty_path_is_refused_before_a_staging_file_is_created() {
    // An empty path is rejected before resolution or staging.
    let storage_error = write_atomic(Path::new(""), b"x").expect_err("an empty path names no file");

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert_eq!(detail, "empty destination path");
}

#[test]
fn a_filesystem_root_is_refused_before_a_staging_file_is_created() {
    // A filesystem root has no parent for staging.
    let root = std::env::current_dir()
        .unwrap()
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();

    let storage_error = write_atomic(&root, b"x").expect_err("a filesystem root names no file");

    let StorageError::Io { detail } = storage_error else {
        panic!("expected an Io error, got {storage_error:?}");
    };
    assert_eq!(
        detail,
        format!("no parent directory for {}", root.display())
    );
}

#[test]
fn a_failed_write_is_a_recoverable_storage_error() {
    use koshi_core::error::{DomainCategory, DomainError, Severity};

    let test_directory = TempDir::new().unwrap();
    // A directory at `destination_path` makes replacement fail.
    let destination_path = test_directory.path().join("target");
    std::fs::create_dir(&destination_path).unwrap();

    let storage_error = write_atomic(&destination_path, b"x")
        .expect_err("a directory at the destination path blocks the rename");

    assert_eq!(storage_error.category(), DomainCategory::Storage);
    assert_eq!(storage_error.get_severity(), Severity::Recoverable);
}
