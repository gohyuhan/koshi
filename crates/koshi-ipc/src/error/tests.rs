//! Tests for [`IpcError`]: its `Display` wording and its [`DomainError`]
//! classification. Both variants are client-fatal — a broken control link tears
//! down only the affected client, never the session.

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
fn every_ipc_error_is_in_the_ipc_domain() {
    assert_eq!(
        IpcError::Transport {
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(IpcError::Disconnected.category(), DomainCategory::Ipc);
}

#[test]
fn every_ipc_error_is_client_fatal() {
    assert_eq!(
        IpcError::Transport {
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(IpcError::Disconnected.severity(), Severity::ClientFatal);
}
