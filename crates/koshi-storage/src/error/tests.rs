//! Tests for the `Display` wording of every [`StorageError`] variant.

use super::*;

#[test]
fn io_error_display_carries_the_detail() {
    let storage_error = StorageError::Io {
        detail: "disk full".to_string(),
    };
    assert_eq!(storage_error.to_string(), "storage io error: disk full");
}

#[test]
fn corrupt_error_display_carries_the_detail() {
    let storage_error = StorageError::Corrupt {
        detail: "bad magic".to_string(),
    };
    assert_eq!(storage_error.to_string(), "corrupt stored state: bad magic");
}

#[test]
fn an_empty_detail_displays_only_the_prefix() {
    let storage_io_error = StorageError::Io {
        detail: String::new(),
    };
    assert_eq!(storage_io_error.to_string(), "storage io error: ");
    let corrupt_storage_error = StorageError::Corrupt {
        detail: String::new(),
    };
    assert_eq!(corrupt_storage_error.to_string(), "corrupt stored state: ");
}

#[test]
fn a_detail_with_newlines_and_non_ascii_displays_verbatim() {
    let storage_io_error = StorageError::Io {
        detail: "line one\nline two: 設定 ✓".to_string(),
    };
    assert_eq!(
        storage_io_error.to_string(),
        "storage io error: line one\nline two: 設定 ✓"
    );
    let corrupt_storage_error = StorageError::Corrupt {
        detail: "\tbad\r\nmagic".to_string(),
    };
    assert_eq!(
        corrupt_storage_error.to_string(),
        "corrupt stored state: \tbad\r\nmagic"
    );
}
