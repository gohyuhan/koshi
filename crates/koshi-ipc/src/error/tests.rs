//! Tests for the `Display` wording of every [`IpcError`] variant, including
//! which remote access file a failure names and the changed-certificate
//! refusal.

use super::{IpcError, RemoteFile};

#[test]
fn transport_error_display_carries_the_detail() {
    let ipc_error = IpcError::Transport {
        error_detail: "socket reset".to_string(),
    };
    assert_eq!(ipc_error.to_string(), "ipc transport error: socket reset");
}

#[test]
fn disconnected_error_display_is_a_fixed_message() {
    assert_eq!(IpcError::Disconnected.to_string(), "ipc peer disconnected");
}

#[test]
fn frame_too_large_display_names_both_sizes() {
    let ipc_error = IpcError::FrameTooLarge {
        frame_byte_count: 20_000_000,
        maximum_frame_byte_count: 16_777_216,
    };
    assert_eq!(
        ipc_error.to_string(),
        "ipc frame of 20000000 bytes exceeds the 16777216-byte limit"
    );
}

#[test]
fn malformed_frame_display_carries_the_detail() {
    let ipc_error = IpcError::MalformedFrame {
        error_detail: "expected value at line 1 column 1".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "ipc frame is not a readable message: expected value at line 1 column 1"
    );
}

#[test]
fn untrusted_socket_display_names_the_address_and_reason() {
    let ipc_error = IpcError::UntrustedSocket {
        socket_address: "/tmp/evil.sock".to_string(),
        trust_failure_reason: "not directly inside the koshi runtime directory".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "untrusted socket address /tmp/evil.sock: not directly inside the koshi runtime directory"
    );
}

#[test]
fn no_listener_display_names_the_address() {
    let ipc_error = IpcError::NoListener {
        socket_address: "/run/koshi/session-abc.sock".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "no koshi is listening at /run/koshi/session-abc.sock"
    );
}

#[test]
fn socket_busy_display_names_the_address() {
    let ipc_error = IpcError::SocketBusy {
        socket_address: "/run/koshi/session-abc.sock".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "another process is already listening at /run/koshi/session-abc.sock"
    );
}

#[test]
fn endpoint_file_missing_display_names_the_path() {
    let ipc_error = IpcError::EndpointFileMissing {
        endpoint_file_path: "/run/koshi/session-abc.json".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "no endpoint file at /run/koshi/session-abc.json"
    );
}

#[test]
fn endpoint_file_unreadable_display_names_the_path_and_detail() {
    let ipc_error = IpcError::EndpointFileUnreadable {
        endpoint_file_path: "/run/koshi/session-abc.json".to_string(),
        error_detail: "expected value at line 1 column 1".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "endpoint file /run/koshi/session-abc.json is unreadable: expected value at line 1 column 1"
    );
}

#[test]
fn endpoint_file_write_display_names_the_path_and_detail() {
    let ipc_error = IpcError::EndpointFileWrite {
        endpoint_file_path: "/run/koshi/session-abc.json".to_string(),
        error_detail: "storage io error: permission denied".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "endpoint file /run/koshi/session-abc.json could not be written: storage io error: permission denied"
    );
}

#[test]
fn advertisement_marker_write_display_names_the_marker_and_detail() {
    let ipc_error = IpcError::AdvertisementMarkerWrite {
        advertisement_marker_path: "/tmp/koshi/501/session-1".to_string(),
        error_detail: "No such file or directory (os error 2)".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "advertisement marker /tmp/koshi/501/session-1 could not be written: \
         No such file or directory (os error 2)"
    );
}

#[test]
fn connect_refused_display_names_the_address_and_the_way_out() {
    let ipc_error = IpcError::ConnectRefused {
        server_address: "laptop.local:7654".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "laptop.local:7654 refused the connection: nothing is listening on that port. \
         if remote access is not enabled on that machine, run `koshi share grant` \
         there and answer yes to the offer to open the port"
    );
}

#[test]
fn connect_timed_out_display_names_the_address_and_what_to_check() {
    let ipc_error = IpcError::ConnectTimedOut {
        server_address: "laptop.local:7654".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
        "connecting to laptop.local:7654 timed out: nothing answered. check that the \
         machine is up, the address and port are right, and the network path \
         allows it"
    );
}

#[test]
fn tls_handshake_failed_display_names_the_address_and_detail() {
    let ipc_error = IpcError::TlsHandshakeFailed {
        server_address: "laptop.local:7654".to_string(),
        error_detail: "received fatal alert: HandshakeFailure".to_string(),
    };
    assert_eq!(
        ipc_error.to_string(),
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
        RemoteFile::RemoteAccessRecord.to_string(),
        "remote access record"
    );
    assert_eq!(
        RemoteFile::TokenStore.to_string(),
        "remote access token store"
    );
}

#[test]
fn a_remote_file_names_which_file_it_is_as_well_as_its_path() {
    // Each of the four reads as its own thing, so a saved-servers failure on
    // the dialling machine never reads as a token store failure on the
    // serving one.
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            remote_file: RemoteFile::SavedServers,
            remote_file_path: "/home/alice/.local/share/koshi/remote/servers".to_string(),
            error_detail: "expected value at line 1 column 1".to_string(),
        }
        .to_string(),
        "the saved servers file at /home/alice/.local/share/koshi/remote/servers is unreadable: \
         expected value at line 1 column 1"
    );
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            remote_file: RemoteFile::Certificate,
            remote_file_path: "/var/lib/koshi/remote/cert".to_string(),
            error_detail: "format 2 is not the 1 this build reads".to_string(),
        }
        .to_string(),
        "the remote access certificate at /var/lib/koshi/remote/cert is unreadable: \
         format 2 is not the 1 this build reads"
    );
    assert_eq!(
        IpcError::RemoteFileWrite {
            remote_file: RemoteFile::RemoteAccessRecord,
            remote_file_path: "/var/lib/koshi/remote/enabled".to_string(),
            error_detail: "permission denied".to_string(),
        }
        .to_string(),
        "the remote access record at /var/lib/koshi/remote/enabled could not be written: \
         permission denied"
    );
    assert_eq!(
        IpcError::RemoteFileUnreadable {
            remote_file: RemoteFile::TokenStore,
            remote_file_path: "/var/lib/koshi/remote/tokens".to_string(),
            error_detail: "format 2 is not the 1 this build reads".to_string(),
        }
        .to_string(),
        "the remote access token store at /var/lib/koshi/remote/tokens is unreadable: \
         format 2 is not the 1 this build reads"
    );
    assert_eq!(
        IpcError::RemoteFileWrite {
            remote_file: RemoteFile::TokenStore,
            remote_file_path: "/var/lib/koshi/remote/tokens".to_string(),
            error_detail: "permission denied".to_string(),
        }
        .to_string(),
        "the remote access token store at /var/lib/koshi/remote/tokens could not be written: \
         permission denied"
    );
}

#[test]
fn a_changed_certificate_names_both_fingerprints_and_the_way_out() {
    let ipc_error = IpcError::CertificateChanged {
        server_address: "laptop.local:7654".to_string(),
        pinned_certificate: "aa".repeat(32),
        presented_certificate: "bb".repeat(32),
    };
    assert_eq!(
        ipc_error.to_string(),
        format!(
            "the certificate of laptop.local:7654 changed: pinned {}, presented {}. \
             if the server was reinstalled on purpose, run \
             `koshi remote forget laptop.local:7654` and connect again.",
            "aa".repeat(32),
            "bb".repeat(32)
        )
    );
}
