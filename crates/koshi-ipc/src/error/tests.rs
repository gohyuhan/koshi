//! Tests for [`IpcError`]: its `Display` wording and its [`DomainError`]
//! classification. Link and refused-frame errors are client-fatal — they tear
//! down only the affected connection, never the session — while a malformed
//! frame is recoverable because the stream stays aligned on frame boundaries.

use super::IpcError;
use koshi_core::error::{DomainCategory, DomainError, Severity};

#[test]
fn transport_error_display_carries_the_detail() {
    let err = IpcError::Transport {
        detail: "socket reset".to_string(),
    };
    assert_eq!(err.to_string(), "ipc transport error: socket reset");
}

#[test]
fn disconnected_error_display_is_a_fixed_message() {
    assert_eq!(IpcError::Disconnected.to_string(), "ipc peer disconnected");
}

#[test]
fn frame_too_large_display_names_both_sizes() {
    let err = IpcError::FrameTooLarge {
        len: 20_000_000,
        max: 16_777_216,
    };
    assert_eq!(
        err.to_string(),
        "ipc frame of 20000000 bytes exceeds the 16777216-byte limit"
    );
}

#[test]
fn malformed_frame_display_carries_the_detail() {
    let err = IpcError::MalformedFrame {
        detail: "expected value at line 1 column 1".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "ipc frame is not a readable message: expected value at line 1 column 1"
    );
}

#[test]
fn every_ipc_error_is_in_the_ipc_domain() {
    assert_eq!(
        IpcError::Transport {
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(IpcError::Disconnected.category(), DomainCategory::Ipc);
    assert_eq!(
        IpcError::FrameTooLarge { len: 0, max: 0 }.category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::MalformedFrame {
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
}

#[test]
fn link_and_refused_frame_errors_are_client_fatal() {
    assert_eq!(
        IpcError::Transport {
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(IpcError::Disconnected.severity(), Severity::ClientFatal);
    assert_eq!(
        IpcError::FrameTooLarge { len: 0, max: 0 }.severity(),
        Severity::ClientFatal
    );
}

#[test]
fn a_malformed_frame_is_recoverable() {
    assert_eq!(
        IpcError::MalformedFrame {
            detail: String::new()
        }
        .severity(),
        Severity::Recoverable
    );
}
