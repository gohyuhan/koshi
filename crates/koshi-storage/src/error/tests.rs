//! Tests for [`StorageError`]: its `Display` wording and its [`DomainError`]
//! classification. Corruption is session-fatal while an I/O failure is only
//! recoverable, so the severity split is pinned per variant.

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
