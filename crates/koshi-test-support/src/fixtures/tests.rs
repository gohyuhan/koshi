//! Tests for the runtime-directory fixture.

use super::*;

#[test]
fn runtime_dir_exists_until_its_handle_drops() {
    let path = {
        let directory = test_runtime_dir();
        assert!(directory.path().is_dir());
        directory.path().to_owned()
    };

    assert!(!path.exists());
}
