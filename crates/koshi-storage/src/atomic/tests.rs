//! Tests for [`super`] atomic file replacement.

use super::*;
use crate::error::StorageError;
use tempfile::TempDir;

/// Names of every entry in `dir` (temp names are random, so tests assert the
/// exact surviving set rather than matching a fixed temp path).
fn dir_entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn write_atomic_creates_file_with_exact_bytes() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");

    write_atomic(&dst, b"a=2\n").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"a=2\n");
}

#[test]
fn write_atomic_replaces_existing_file_wholesale() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    std::fs::write(&dst, b"a=1\n").unwrap();

    write_atomic(&dst, b"a=2\n").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"a=2\n");
}

#[test]
fn write_atomic_leaves_no_temp_on_success() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");

    write_atomic(&dst, b"x").unwrap();

    assert_eq!(dir_entries(dir.path()), vec!["cfg.kdl".to_string()]);
}

#[test]
fn write_atomic_cleans_temp_and_keeps_target_when_rename_fails() {
    let dir = TempDir::new().unwrap();
    // `dst` is a directory, so replacement fails after the temp is written and
    // synced.
    let dst = dir.path().join("target");
    std::fs::create_dir(&dst).unwrap();

    let err = write_atomic(&dst, b"x").unwrap_err();

    let StorageError::Io { detail } = err else {
        panic!("expected an Io error, got {err:?}");
    };
    assert!(
        detail.starts_with(&format!("replace {}: ", dst.display())),
        "unexpected error detail: {detail}"
    );
    assert_eq!(dir_entries(dir.path()), vec!["target".to_string()]);
    assert!(dst.is_dir(), "target must be left untouched");
}

#[test]
fn write_atomic_reports_io_error_when_temp_dir_is_missing() {
    let dir = TempDir::new().unwrap();
    // The parent directory is missing, so temp creation fails and creates
    // nothing.
    let dst = dir.path().join("missing").join("cfg.kdl");

    let err = write_atomic(&dst, b"x").unwrap_err();

    let StorageError::Io { detail } = err else {
        panic!("expected an Io error, got {err:?}");
    };
    assert!(
        detail.starts_with(&format!(
            "create temp in {}: ",
            dir.path().join("missing").display()
        )),
        "unexpected error detail: {detail}"
    );
    assert_eq!(
        dir_entries(dir.path()),
        Vec::<String>::new(),
        "nothing must be created"
    );
}

#[cfg(unix)]
#[test]
fn write_atomic_preserves_existing_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    std::fs::write(&dst, b"old").unwrap();
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644)).unwrap();

    write_atomic(&dst, b"new").unwrap();

    let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "atomic overwrite must keep the file's mode");
}

#[cfg(unix)]
#[test]
fn write_atomic_new_file_is_private_by_default() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("secret.kdl");

    write_atomic(&dst, b"data").unwrap();

    let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a fresh file must be created user-private");
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_symlink_with_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let referent = dir.path().join("shared.txt");
    let link = dir.path().join("cfg.kdl");
    std::fs::write(&referent, b"other").unwrap();
    std::fs::set_permissions(&referent, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&referent, &link).unwrap();

    write_atomic(&link, b"secret").unwrap();

    // Replacement removes the link and creates a private regular file. The
    // referent keeps its mode and bytes.
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_file(),
        "symlink must become a regular file"
    );
    assert_eq!(
        meta.permissions().mode() & 0o777,
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

    let dir = TempDir::new().unwrap();
    // The link points at a missing file.
    let link = dir.path().join("cfg.kdl");
    std::os::unix::fs::symlink(dir.path().join("gone.txt"), &link).unwrap();

    write_atomic(&link, b"data").unwrap();

    // Replacement creates a private regular file with the new bytes.
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_file(),
        "dangling symlink must become a regular file"
    );
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&link).unwrap(), b"data");
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_fifo_with_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    // A world-readable FIFO occupies the target path.
    let dst = dir.path().join("cfg.kdl");
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("666")
        .arg(&dst)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo must succeed");

    write_atomic(&dst, b"secret").unwrap();

    // Replacement creates a private regular file; the FIFO mode does not carry
    // over.
    let meta = std::fs::symlink_metadata(&dst).unwrap();
    assert!(
        meta.file_type().is_file(),
        "FIFO must become a regular file"
    );
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "replacement must not inherit the FIFO's mode"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"secret");
}

#[test]
fn write_atomic_resolves_a_relative_path_against_the_current_dir() {
    // Use a process-specific name and remove any entry from an earlier run.
    let name = format!("koshi-atomic-relative-{}.tmp", std::process::id());
    let rel = Path::new(&name);
    let _ = std::fs::remove_file(rel);

    write_atomic(rel, b"relative\n").unwrap();

    let abs = std::env::current_dir().unwrap().join(&name);
    let bytes = std::fs::read(&abs).unwrap();
    std::fs::remove_file(&abs).unwrap();
    assert_eq!(bytes, b"relative\n");
}

#[cfg(unix)]
#[test]
fn write_atomic_reports_io_error_when_a_path_component_is_a_file() {
    let dir = TempDir::new().unwrap();
    // A regular file blocks a directory component. Unix target stat returns an
    // I/O error before temp creation.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let dst = blocker.join("cfg.kdl");

    let err = write_atomic(&dst, b"data").unwrap_err();

    let StorageError::Io { detail } = err else {
        panic!("expected an Io error, got {err:?}");
    };
    assert!(
        detail.starts_with(&format!("stat {}: ", dst.display())),
        "unexpected error detail: {detail}"
    );
    // The blocker file is untouched and no temp was staged beside it.
    assert_eq!(std::fs::read(&blocker).unwrap(), b"x");
    assert_eq!(dir_entries(dir.path()), vec!["not-a-dir".to_string()]);
}

#[test]
fn concurrent_writers_never_leave_partial_content() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    // Each writer supplies a distinct 4 KiB buffer. The final file must equal
    // one complete buffer, with no temp left beside it.
    let contents: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 4096]).collect();

    std::thread::scope(|s| {
        for c in &contents {
            let dst = &dst;
            s.spawn(move || write_atomic(dst, c).unwrap());
        }
    });

    let final_bytes = std::fs::read(&dst).unwrap();
    assert!(
        contents.contains(&final_bytes),
        "final file must be exactly one writer's complete content"
    );
    assert_eq!(dir_entries(dir.path()), vec!["cfg.kdl".to_string()]);
}

#[test]
fn write_atomic_stages_the_temp_in_the_targets_own_directory() {
    let dir = TempDir::new().unwrap();
    // The missing target directory is named in the temp-creation error. No
    // entry is created under `dir`.
    let missing = dir.path().join("missing");
    let dst = missing.join("cfg.kdl");

    let err = write_atomic(&dst, b"x").unwrap_err();

    let StorageError::Io { detail } = err else {
        panic!("expected an Io error, got {err:?}");
    };
    assert!(
        detail.starts_with(&format!("create temp in {}: ", missing.display())),
        "unexpected error detail: {detail}"
    );
    assert_eq!(dir_entries(dir.path()), Vec::<String>::new());
}

#[test]
fn write_atomic_names_the_target_when_the_rename_is_blocked() {
    let dir = TempDir::new().unwrap();
    // A directory at `dst` makes replacement fail after the temp is written and
    // synced.
    let dst = dir.path().join("target");
    std::fs::create_dir(&dst).unwrap();

    let err = write_atomic(&dst, b"x").unwrap_err();

    let StorageError::Io { detail } = err else {
        panic!("expected an Io error, got {err:?}");
    };
    assert!(
        detail.starts_with(&format!("replace {}: ", dst.display())),
        "unexpected error detail: {detail}"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn write_atomic_replaces_only_the_named_hard_link() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    let alias = dir.path().join("alias.kdl");
    std::fs::write(&dst, b"old").unwrap();
    std::fs::hard_link(&dst, &alias).unwrap();

    write_atomic(&dst, b"new").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"new");
    assert_eq!(std::fs::read(&alias).unwrap(), b"old");
}

#[cfg(windows)]
#[test]
fn write_atomic_rejects_a_read_only_file_without_changing_it() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    std::fs::write(&dst, b"old").unwrap();
    let mut readonly = std::fs::metadata(&dst).unwrap().permissions();
    readonly.set_readonly(true);
    std::fs::set_permissions(&dst, readonly).unwrap();

    let error = write_atomic(&dst, b"new").unwrap_err();

    let StorageError::Io { detail } = error else {
        panic!("expected an Io error, got {error:?}");
    };
    assert!(
        detail.starts_with(&format!("replace {}: ", dst.display())),
        "unexpected error detail: {detail}"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"old");

    let mut writable = std::fs::metadata(&dst).unwrap().permissions();
    writable.set_readonly(false);
    std::fs::set_permissions(&dst, writable).unwrap();
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_a_read_only_file_and_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    std::fs::write(&dst, b"old").unwrap();
    // The target mode is 0400 and its directory remains writable. Replacement
    // succeeds and carries the mode onto the new bytes.
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o400)).unwrap();

    write_atomic(&dst, b"new").unwrap();

    let meta = std::fs::symlink_metadata(&dst).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o400);
    assert_eq!(std::fs::read(&dst).unwrap(), b"new");
    assert_eq!(dir_entries(dir.path()), vec!["cfg.kdl".to_string()]);
}

#[test]
fn write_atomic_replaces_a_longer_file_with_empty_data() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    std::fs::write(&dst, b"a=1\nb=2\n").unwrap();

    write_atomic(&dst, b"").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"");
    assert_eq!(dir_entries(dir.path()), vec!["cfg.kdl".to_string()]);
}

#[test]
fn write_atomic_writes_binary_bytes_verbatim() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("state.bin");
    let data = [0u8, 0xff, b'\n', b'\r', 0x1b, 0x00, 0x7f];

    write_atomic(&dst, &data).unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), data);
}

#[test]
fn write_atomic_writes_a_one_mebibyte_buffer_whole() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("big.bin");
    let data: Vec<u8> = (0u8..=250).cycle().take(1024 * 1024).collect();

    write_atomic(&dst, &data).unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), data);
    assert_eq!(dir_entries(dir.path()), vec!["big.bin".to_string()]);
}

#[test]
fn write_atomic_accepts_a_non_ascii_file_name() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("設定✓.kdl");

    write_atomic(&dst, b"a=1\n").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"a=1\n");
    assert_eq!(dir_entries(dir.path()), vec!["設定✓.kdl".to_string()]);
}

#[test]
fn write_atomic_twice_leaves_only_the_last_bytes_and_no_temp() {
    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");

    write_atomic(&dst, b"first").unwrap();
    write_atomic(&dst, b"second").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"second");
    assert_eq!(dir_entries(dir.path()), vec!["cfg.kdl".to_string()]);
}

#[test]
fn write_atomic_succeeds_beside_a_target_that_blocked_an_earlier_replace() {
    let dir = TempDir::new().unwrap();
    // A directory at `blocked` makes replacement fail. The sibling write then
    // succeeds without a leftover temp.
    let blocked = dir.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    let dst = dir.path().join("cfg.kdl");

    write_atomic(&blocked, b"x").unwrap_err();
    write_atomic(&dst, b"a=1\n").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"a=1\n");
    let mut entries = dir_entries(dir.path());
    entries.sort();
    assert_eq!(entries, vec!["blocked".to_string(), "cfg.kdl".to_string()]);
}

#[cfg(unix)]
#[test]
fn write_atomic_copies_a_mode_wider_than_the_umask() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let dst = dir.path().join("cfg.kdl");
    std::fs::write(&dst, b"old").unwrap();
    // Existing mode 0666 is copied to the temp and remains unchanged by the
    // umask.
    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o666)).unwrap();

    write_atomic(&dst, b"new").unwrap();

    let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o666);
    assert_eq!(std::fs::read(&dst).unwrap(), b"new");
}

#[cfg(unix)]
#[test]
fn write_atomic_replaces_a_symlink_to_a_directory_with_a_private_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let referent = dir.path().join("shared");
    std::fs::create_dir(&referent).unwrap();
    std::fs::write(referent.join("inner.txt"), b"other").unwrap();
    let link = dir.path().join("cfg.kdl");
    std::os::unix::fs::symlink(&referent, &link).unwrap();

    write_atomic(&link, b"secret").unwrap();

    // Replacement removes the link and leaves the referent directory and entry
    // unchanged.
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_file(),
        "symlink must become a regular file"
    );
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&link).unwrap(), b"secret");
    assert_eq!(dir_entries(&referent), vec!["inner.txt".to_string()]);
    assert_eq!(std::fs::read(referent.join("inner.txt")).unwrap(), b"other");
}

#[cfg(unix)]
#[test]
fn write_atomic_through_a_symlinked_parent_directory_lands_in_the_real_directory() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let dst = link.join("cfg.kdl");

    write_atomic(&dst, b"a=1\n").unwrap();

    assert_eq!(std::fs::read(real.join("cfg.kdl")).unwrap(), b"a=1\n");
    assert_eq!(dir_entries(&real), vec!["cfg.kdl".to_string()]);
    let mut entries = dir_entries(dir.path());
    entries.sort();
    assert_eq!(entries, vec!["link".to_string(), "real".to_string()]);
}

#[test]
fn an_empty_path_is_refused_before_a_temp_is_staged() {
    // An empty path is rejected before resolution or temp staging.
    let error = write_atomic(Path::new(""), b"x").expect_err("an empty path names no file");

    let StorageError::Io { detail } = error else {
        panic!("expected an Io error, got {error:?}");
    };
    assert_eq!(detail, "empty destination path");
}

#[test]
fn a_filesystem_root_is_refused_before_a_temp_is_staged() {
    // A filesystem root has no parent for temp staging.
    let root = std::env::current_dir()
        .unwrap()
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();

    let error = write_atomic(&root, b"x").expect_err("a filesystem root names no file");

    let StorageError::Io { detail } = error else {
        panic!("expected an Io error, got {error:?}");
    };
    assert_eq!(
        detail,
        format!("no parent directory for {}", root.display())
    );
}

#[test]
fn a_failed_write_is_a_recoverable_storage_error() {
    use koshi_core::error::{DomainCategory, DomainError, Severity};

    let dir = TempDir::new().unwrap();
    // A directory at `dst` makes replacement fail.
    let dst = dir.path().join("target");
    std::fs::create_dir(&dst).unwrap();

    let error = write_atomic(&dst, b"x").expect_err("a directory at dst blocks the rename");

    assert_eq!(error.category(), DomainCategory::Storage);
    assert_eq!(error.severity(), Severity::Recoverable);
}
