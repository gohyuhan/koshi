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
    // dst is a directory: renaming the temp *file* over it must fail, which
    // exercises the cleanup path after the temp was already written + fsynced.
    let dst = dir.path().join("target");
    std::fs::create_dir(&dst).unwrap();

    let err = write_atomic(&dst, b"x").unwrap_err();

    assert!(matches!(err, StorageError::Io { .. }));
    assert_eq!(dir_entries(dir.path()), vec!["target".to_string()]);
    assert!(dst.is_dir(), "target must be left untouched");
}

#[test]
fn write_atomic_reports_io_error_when_temp_dir_is_missing() {
    let dir = TempDir::new().unwrap();
    // Parent dir does not exist: staging the temp fails and nothing is created.
    let dst = dir.path().join("missing").join("cfg.kdl");

    let err = write_atomic(&dst, b"x").unwrap_err();

    assert!(matches!(err, StorageError::Io { .. }));
    assert!(
        dir_entries(dir.path()).is_empty(),
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

    // The link is gone, replaced by a private regular file with the new bytes;
    // the file it pointed at must never inherit onto the replacement or change.
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
    // The link points at a file that does not exist.
    let link = dir.path().join("cfg.kdl");
    std::os::unix::fs::symlink(dir.path().join("gone.txt"), &link).unwrap();

    write_atomic(&link, b"data").unwrap();

    // The dead link is replaced by a private regular file with the new bytes.
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
    // A world-readable FIFO sits where the file should go.
    let dst = dir.path().join("cfg.kdl");
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("666")
        .arg(&dst)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo must succeed");

    write_atomic(&dst, b"secret").unwrap();

    // The FIFO is replaced by a private regular file; its loose mode must not
    // carry over onto the new bytes.
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
    // A relative `dst` is anchored against the current directory on entry. Use a
    // unique name in the current directory so parallel tests never collide, and
    // clean it up whether or not the assertion passes.
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
    // A regular file sits where a directory component is needed, so reading the
    // target's mode fails with a not-a-directory error (not NotFound), which the
    // stat-error arm surfaces as an I/O error before any temp is created.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let dst = blocker.join("cfg.kdl");

    let err = write_atomic(&dst, b"data").unwrap_err();

    let StorageError::Io { detail } = err else {
        panic!("expected an Io error, got {err:?}");
    };
    assert!(
        detail.starts_with("stat "),
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
    // Eight writers, each a distinct 4 KiB buffer. A partial/interleaved write
    // would produce bytes matching none of them; the atomic replace must leave
    // exactly one writer's complete buffer and no stray temp.
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
