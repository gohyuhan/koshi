//! Tests for the runtime-directory fixture.

use super::*;

#[test]
fn runtime_directory_exists_until_its_handle_drops() {
    let runtime_directory_path = {
        let directory = build_test_runtime_directory();
        assert!(directory.path().is_dir());
        directory.path().to_owned()
    };

    assert!(!runtime_directory_path.exists());
}
