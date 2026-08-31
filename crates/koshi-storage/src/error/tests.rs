//! Tests for [`StorageError`]: its `Display` wording and its [`DomainError`]
//! classification. The severity is pinned per variant: `Io` is recoverable,
//! `Corrupt` is session-fatal.

use super::StorageError;
use koshi_core::error::{DomainCategory, DomainError, Severity};

#[test]
fn io_error_display_carries_the_detail() {
    let err = StorageError::Io {
        detail: "disk full".to_string(),
    };
    assert_eq!(err.to_string(), "storage io error: disk full");
}

#[test]
fn corrupt_error_display_carries_the_detail() {
    let err = StorageError::Corrupt {
        detail: "bad magic".to_string(),
    };
    assert_eq!(err.to_string(), "corrupt stored state: bad magic");
}

#[test]
fn an_empty_detail_displays_only_the_prefix() {
    let io = StorageError::Io {
        detail: String::new(),
    };
    assert_eq!(io.to_string(), "storage io error: ");
    let corrupt = StorageError::Corrupt {
        detail: String::new(),
    };
    assert_eq!(corrupt.to_string(), "corrupt stored state: ");
}

#[test]
fn a_detail_with_newlines_and_non_ascii_displays_verbatim() {
    let io = StorageError::Io {
        detail: "line one\nline two: 設定 ✓".to_string(),
    };
    assert_eq!(
        io.to_string(),
        "storage io error: line one\nline two: 設定 ✓"
    );
    let corrupt = StorageError::Corrupt {
        detail: "\tbad\r\nmagic".to_string(),
    };
    assert_eq!(corrupt.to_string(), "corrupt stored state: \tbad\r\nmagic");
}

#[test]
fn every_storage_error_is_in_the_storage_domain() {
    assert_eq!(
        StorageError::Io {
            detail: String::new()
        }
        .category(),
        DomainCategory::Storage
    );
    assert_eq!(
        StorageError::Corrupt {
            detail: String::new()
        }
        .category(),
        DomainCategory::Storage
    );
}

#[test]
fn an_io_error_is_recoverable_but_corruption_is_session_fatal() {
    assert_eq!(
        StorageError::Io {
            detail: String::new()
        }
        .severity(),
        Severity::Recoverable
    );
    assert_eq!(
        StorageError::Corrupt {
            detail: String::new()
        }
        .severity(),
        Severity::SessionFatal
    );
}
