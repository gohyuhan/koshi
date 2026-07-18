//! Tests for [`super`] atomic file replacement.

use super::*;
use crate::error::StorageError;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique, freshly-created temp dir for one test. Unique across parallel
/// tests via pid + a per-process counter; removed at the end of the test.
fn tmpdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "koshi_atomic_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn write_atomic_creates_file_with_exact_bytes() {
    let dir = tmpdir();
    let dst = dir.join("cfg.kdl");

    write_atomic(&dst, b"a=2\n").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"a=2\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn write_atomic_replaces_existing_file_wholesale() {
    let dir = tmpdir();
    let dst = dir.join("cfg.kdl");
    std::fs::write(&dst, b"a=1\n").unwrap();

    write_atomic(&dst, b"a=2\n").unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"a=2\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn write_atomic_removes_temp_on_success() {
    let dir = tmpdir();
    let dst = dir.join("cfg.kdl");

    write_atomic(&dst, b"x").unwrap();

    assert!(!tmp_path(&dst).exists(), "temp file must not linger");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn write_atomic_cleans_temp_and_keeps_target_when_rename_fails() {
    let dir = tmpdir();
    // dst is a directory: renaming the temp *file* over it must fail, which
    // exercises the cleanup path after the temp was already written + fsynced.
    let dst = dir.join("target");
    std::fs::create_dir(&dst).unwrap();

    let err = write_atomic(&dst, b"x").unwrap_err();

    assert!(matches!(err, StorageError::Io { .. }));
    assert!(!tmp_path(&dst).exists(), "temp file must be cleaned up");
    assert!(dst.is_dir(), "target must be left untouched");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn write_atomic_reports_io_error_when_temp_cannot_be_created() {
    let dir = tmpdir();
    // Parent dir does not exist: creating the temp fails before it exists, so
    // the cleanup branch is a no-op remove and the error surfaces cleanly.
    let dst = dir.join("missing").join("cfg.kdl");

    let err = write_atomic(&dst, b"x").unwrap_err();

    assert!(matches!(err, StorageError::Io { .. }));
    assert!(!tmp_path(&dst).exists(), "no temp file must be left behind");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn replace_moves_temp_over_destination() {
    let dir = tmpdir();
    let tmp = dir.join("cfg.kdl.tmp");
    let dst = dir.join("cfg.kdl");
    std::fs::write(&tmp, b"new\n").unwrap();
    std::fs::write(&dst, b"old\n").unwrap();

    replace(&tmp, &dst).unwrap();

    assert_eq!(std::fs::read(&dst).unwrap(), b"new\n");
    assert!(!tmp.exists(), "source temp must be gone after rename");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn replace_reports_io_error_for_missing_source() {
    let dir = tmpdir();
    let missing = dir.join("nope.tmp");
    let dst = dir.join("cfg.kdl");

    let err = replace(&missing, &dst).unwrap_err();

    assert!(matches!(err, StorageError::Io { .. }));
    std::fs::remove_dir_all(&dir).unwrap();
}
