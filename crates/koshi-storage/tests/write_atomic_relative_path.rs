//! [`write_atomic`] given a relative path, run from a temporary current
//! directory.
//!
//! The current directory belongs to the whole process. This file holds one
//! test, so its binary changes the current directory under no other test.

use std::path::Path;

use koshi_storage::atomic::write_atomic;
use tempfile::TempDir;

#[test]
fn write_atomic_resolves_a_relative_path_against_the_current_directory() {
    let original_current_directory =
        std::env::current_dir().expect("the current directory can be read");
    let test_directory = TempDir::new().expect("a temporary directory");
    std::env::set_current_dir(test_directory.path())
        .expect("the temporary directory becomes the current directory");

    let write_result = write_atomic(Path::new("relative.kdl"), b"relative\n");
    let read_result = std::fs::read(test_directory.path().join("relative.kdl"));
    std::env::set_current_dir(&original_current_directory)
        .expect("the original directory becomes the current directory again");

    write_result.expect("the relative path is written");
    assert_eq!(
        read_result.expect("the file sits in the current directory"),
        b"relative\n"
    );
}
