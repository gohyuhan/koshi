//! Tests for [`IpcError`]: its `Display` wording and its [`DomainError`]
//! classification — link, refused-frame, socket-address-check,
//! endpoint-file-read, token store, remote dial and remote access file errors
//! client-fatal, a failed endpoint-file write session-fatal, a malformed frame
//! recoverable. The wording tests also cover which remote access file a
//! failure names and the changed-certificate refusal.

use super::{IpcError, RemoteFile};
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
fn untrusted_socket_display_names_the_address_and_reason() {
    let err = IpcError::UntrustedSocket {
        addr: "/tmp/evil.sock".to_string(),
        reason: "not directly inside the koshi runtime directory".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "untrusted socket address /tmp/evil.sock: not directly inside the koshi runtime directory"
    );
}

#[test]
fn no_listener_display_names_the_address() {
    let err = IpcError::NoListener {
        addr: "/run/koshi/session-abc.sock".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "no koshi is listening at /run/koshi/session-abc.sock"
    );
}

#[test]
fn socket_busy_display_names_the_address() {
    let err = IpcError::SocketBusy {
        addr: "/run/koshi/session-abc.sock".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "another process is already listening at /run/koshi/session-abc.sock"
    );
}

#[test]
fn endpoint_file_missing_display_names_the_path() {
    let err = IpcError::EndpointFileMissing {
        path: "/run/koshi/session-abc.json".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "no endpoint file at /run/koshi/session-abc.json"
    );
}

#[test]
fn endpoint_file_unreadable_display_names_the_path_and_detail() {
    let err = IpcError::EndpointFileUnreadable {
        path: "/run/koshi/session-abc.json".to_string(),
        detail: "expected value at line 1 column 1".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "endpoint file /run/koshi/session-abc.json is unreadable: expected value at line 1 column 1"
    );
}

#[test]
fn endpoint_file_write_display_names_the_path_and_detail() {
    let err = IpcError::EndpointFileWrite {
        path: "/run/koshi/session-abc.json".to_string(),
        detail: "storage io error: permission denied".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "endpoint file /run/koshi/session-abc.json could not be written: storage io error: permission denied"
    );
}

#[test]
fn token_store_unreadable_display_names_the_path_and_detail() {
    let err = IpcError::TokenStoreUnreadable {
        path: "/var/lib/koshi/remote/tokens".to_string(),
        detail: "format 2 is not the 1 this build reads".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "token store /var/lib/koshi/remote/tokens is unreadable: format 2 is not the 1 this build reads"
    );
}

#[test]
fn token_store_write_display_names_the_path_and_detail() {
    let err = IpcError::TokenStoreWrite {
        path: "/var/lib/koshi/remote/tokens".to_string(),
        detail: "permission denied".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "token store /var/lib/koshi/remote/tokens could not be written: permission denied"
    );
}

#[test]
fn connect_refused_display_names_the_address_and_the_way_out() {
    let err = IpcError::ConnectRefused {
        address: "laptop.local:7654".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "laptop.local:7654 refused the connection: nothing is listening on that port. \
         if remote access is not enabled on that machine, run `koshi share grant` \
         there and answer yes to the offer to open the port"
    );
}

#[test]
fn connect_timed_out_display_names_the_address_and_what_to_check() {
    let err = IpcError::ConnectTimedOut {
        address: "laptop.local:7654".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "connecting to laptop.local:7654 timed out: nothing answered. check that the \
         machine is up, the address and port are right, and the network path \
         allows it"
    );
}

#[test]
fn tls_handshake_failed_display_names_the_address_and_detail() {
    let err = IpcError::TlsHandshakeFailed {
        address: "laptop.local:7654".to_string(),
        detail: "received fatal alert: HandshakeFailure".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "the TLS handshake with laptop.local:7654 failed: received fatal alert: HandshakeFailure"
    );
}

#[test]
fn each_remote_file_displays_as_its_own_name() {
    assert_eq!(RemoteFile::SavedServers.to_string(), "saved servers file");
    assert_eq!(
        RemoteFile::Certificate.to_string(),
        "remote access certificate"
    );
    assert_eq!(
        RemoteFile::RemoteAccessMark.to_string(),
        "remote access record"
    );
}

#[test]
fn a_remote_file_names_which_file_it_is_as_well_as_its_path() {
    // Each of the three reads as its own thing, so a saved-servers failure on
    // the dialling machine never reads as a token store failure on the
    // serving one.
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            file: RemoteFile::SavedServers,
            path: "/home/alice/.local/share/koshi/remote/servers".to_string(),
            detail: "expected value at line 1 column 1".to_string(),
        }
        .to_string(),
        "the saved servers file at /home/alice/.local/share/koshi/remote/servers is unreadable: \
         expected value at line 1 column 1"
    );
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            file: RemoteFile::Certificate,
            path: "/var/lib/koshi/remote/cert".to_string(),
            detail: "format 2 is not the 1 this build reads".to_string(),
        }
        .to_string(),
        "the remote access certificate at /var/lib/koshi/remote/cert is unreadable: \
         format 2 is not the 1 this build reads"
    );
    assert_eq!(
        IpcError::RemoteFileWrite {
            file: RemoteFile::RemoteAccessMark,
            path: "/var/lib/koshi/remote/enabled".to_string(),
            detail: "permission denied".to_string(),
        }
        .to_string(),
        "the remote access record at /var/lib/koshi/remote/enabled could not be written: \
         permission denied"
    );
}

#[test]
fn a_changed_certificate_names_both_fingerprints_and_the_way_out() {
    let err = IpcError::CertificateChanged {
        address: "laptop.local:7654".to_string(),
        pinned: "aa".repeat(32),
        presented: "bb".repeat(32),
    };
    assert_eq!(
        err.to_string(),
        format!(
            "the certificate of laptop.local:7654 changed: pinned {}, presented {}. \
             if the server was reinstalled on purpose, run \
             `koshi remote forget laptop.local:7654` and connect again.",
            "aa".repeat(32),
            "bb".repeat(32)
        )
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
    assert_eq!(
        IpcError::UntrustedSocket {
            addr: String::new(),
            reason: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::NoListener {
            addr: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::SocketBusy {
            addr: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::EndpointFileMissing {
            path: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::EndpointFileUnreadable {
            path: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::EndpointFileWrite {
            path: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            file: RemoteFile::SavedServers,
            path: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::RemoteFileWrite {
            file: RemoteFile::Certificate,
            path: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::CertificateChanged {
            address: String::new(),
            pinned: String::new(),
            presented: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::TokenStoreUnreadable {
            path: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::TokenStoreWrite {
            path: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::ConnectRefused {
            address: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::ConnectTimedOut {
            address: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
    assert_eq!(
        IpcError::TlsHandshakeFailed {
            address: String::new(),
            detail: String::new()
        }
        .category(),
        DomainCategory::Ipc
    );
}

#[test]
fn token_store_failures_are_client_fatal() {
    assert_eq!(
        IpcError::TokenStoreUnreadable {
            path: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::TokenStoreWrite {
            path: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
}

#[test]
fn remote_dial_failures_are_client_fatal() {
    assert_eq!(
        IpcError::ConnectRefused {
            address: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::ConnectTimedOut {
            address: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::TlsHandshakeFailed {
            address: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
}

#[test]
fn endpoint_file_read_failures_are_client_fatal() {
    assert_eq!(
        IpcError::EndpointFileMissing {
            path: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::EndpointFileUnreadable {
            path: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
}

#[test]
fn a_failed_endpoint_file_write_is_session_fatal() {
    assert_eq!(
        IpcError::EndpointFileWrite {
            path: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::SessionFatal
    );
}

#[test]
fn socket_address_check_failures_are_client_fatal() {
    assert_eq!(
        IpcError::UntrustedSocket {
            addr: String::new(),
            reason: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::NoListener {
            addr: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::SocketBusy {
            addr: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
}

#[test]
fn remote_access_failures_are_client_fatal() {
    // A remote access file that will not read or write stops the command that
    // needed it, the same as the token store beside it.
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            file: RemoteFile::SavedServers,
            path: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::RemoteFileWrite {
            file: RemoteFile::RemoteAccessMark,
            path: String::new(),
            detail: String::new()
        }
        .severity(),
        Severity::ClientFatal
    );
    assert_eq!(
        IpcError::CertificateChanged {
            address: String::new(),
            pinned: String::new(),
            presented: String::new()
        }
        .severity(),
        Severity::ClientFatal
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
