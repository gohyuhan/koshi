//! IPC domain error. Classifies into [`koshi_core::error::DomainCategory::Ipc`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure on the control channel.
///
/// Client-fatal — the affected connection or caller stops and the session
/// serves on: a broken link ([`Transport`](IpcError::Transport),
/// [`Disconnected`](IpcError::Disconnected)), a remote dial that fails
/// ([`ConnectRefused`](IpcError::ConnectRefused),
/// [`ConnectTimedOut`](IpcError::ConnectTimedOut),
/// [`TlsHandshakeFailed`](IpcError::TlsHandshakeFailed),
/// [`CertificateChanged`](IpcError::CertificateChanged)), a refused frame
/// ([`FrameTooLarge`](IpcError::FrameTooLarge)), a socket address that fails
/// its trust or liveness checks
/// ([`UntrustedSocket`](IpcError::UntrustedSocket),
/// [`NoListener`](IpcError::NoListener), [`SocketBusy`](IpcError::SocketBusy)),
/// an endpoint file the caller cannot read
/// ([`EndpointFileMissing`](IpcError::EndpointFileMissing),
/// [`EndpointFileUnreadable`](IpcError::EndpointFileUnreadable)), and a remote
/// access file the caller cannot read or write
/// ([`RemoteFileUnreadable`](IpcError::RemoteFileUnreadable),
/// [`RemoteFileWrite`](IpcError::RemoteFileWrite)).
///
/// Session-fatal: a failed endpoint-file write
/// ([`EndpointFileWrite`](IpcError::EndpointFileWrite)) and a failed advert
/// marker write ([`AdvertWrite`](IpcError::AdvertWrite)). Both happen during
/// the session's own startup. No caller reaches a session whose endpoint file
/// never lands.
///
/// Recoverable: a frame that arrived whole yet does not decode
/// ([`MalformedFrame`](IpcError::MalformedFrame)). The stream is still aligned
/// on frame boundaries, and the connection answers and keeps going.
#[derive(Debug, Error)]
pub enum IpcError {
    /// The underlying transport failed.
    #[error("ipc transport error: {error_detail}")]
    Transport { error_detail: String },
    /// The peer disconnected unexpectedly.
    #[error("ipc peer disconnected")]
    Disconnected,
    /// A frame longer than
    /// [`MAX_FRAME_BYTE_COUNT`](crate::transport::MAX_FRAME_BYTE_COUNT). On receive, the
    /// length prefix named more bytes than the limit and the payload is left
    /// unread: the stream is off frame boundaries and the connection closes;
    /// `frame_byte_count` is the length the prefix named. On send, encoding
    /// stopped at the byte that crossed the limit and nothing was written;
    /// `frame_byte_count` is the payload size the refused write reached,
    /// which for a message encoded in one piece is its full size.
    #[error(
        "ipc frame of {frame_byte_count} bytes exceeds the {maximum_frame_byte_count}-byte limit"
    )]
    FrameTooLarge {
        frame_byte_count: u64,
        maximum_frame_byte_count: u32,
    },
    /// A frame whose bytes are not a readable message: the payload arrived
    /// whole but did not decode, or a message failed to encode.
    #[error("ipc frame is not a readable message: {error_detail}")]
    MalformedFrame { error_detail: String },
    /// A socket address that failed a trust check, named in `reason`: the
    /// path is not directly inside the directory it must sit in, that
    /// directory is a symbolic link, is not a directory, carries the wrong
    /// mode, or belongs to another user, or (Windows) the pipe name is
    /// outside the `koshi-` namespace.
    #[error("untrusted socket address {socket_address}: {trust_failure_reason}")]
    UntrustedSocket {
        socket_address: String,
        trust_failure_reason: String,
    },
    /// Nothing listens at the address: what is there is a leftover from a
    /// process that is gone, or nothing exists there at all.
    #[error("no koshi is listening at {socket_address}")]
    NoListener { socket_address: String },
    /// A live listener already holds the address this process wants to bind.
    #[error("another process is already listening at {socket_address}")]
    SocketBusy { socket_address: String },
    /// No endpoint file at the path: no running koshi has advertised a
    /// control socket there.
    #[error("no endpoint file at {endpoint_file_path}")]
    EndpointFileMissing { endpoint_file_path: String },
    /// An endpoint file that exists but could not be used: reading it
    /// failed, or its bytes are not a readable endpoint file.
    #[error("endpoint file {endpoint_file_path} is unreadable: {error_detail}")]
    EndpointFileUnreadable {
        endpoint_file_path: String,
        error_detail: String,
    },
    /// Writing the endpoint file failed during session startup. No caller
    /// finds this session's socket.
    #[error("endpoint file {endpoint_file_path} could not be written: {error_detail}")]
    EndpointFileWrite {
        endpoint_file_path: String,
        error_detail: String,
    },
    /// Writing the advert marker failed during session startup. No other user
    /// of this machine finds this session. `path` names the marker.
    #[error("advert marker {advert_marker_path} could not be written: {error_detail}")]
    AdvertWrite {
        advert_marker_path: String,
        error_detail: String,
    },
    /// A remote access file that exists but could not be used: reading it
    /// failed, its bytes are not readable, or its format number is not the
    /// one this build reads.
    #[error("the {remote_file} at {remote_file_path} is unreadable: {error_detail}")]
    RemoteFileUnreadable {
        remote_file: RemoteFile,
        remote_file_path: String,
        error_detail: String,
    },
    /// Writing a remote access file failed. What the caller changed never
    /// reached the disk.
    #[error("the {remote_file} at {remote_file_path} could not be written: {error_detail}")]
    RemoteFileWrite {
        remote_file: RemoteFile,
        remote_file_path: String,
        error_detail: String,
    },
    /// Nothing accepted the TCP connection at `address`.
    #[error(
        "{server_address} refused the connection: nothing is listening on that port. \
         if remote access is not enabled on that machine, run `koshi share grant` \
         there and answer yes to the offer to open the port"
    )]
    ConnectRefused { server_address: String },
    /// The TCP connection to `address` was still unanswered when the dial ran
    /// out of time.
    #[error(
        "connecting to {server_address} timed out: nothing answered. check that the \
         machine is up, the address and port are right, and the network path \
         allows it"
    )]
    ConnectTimedOut { server_address: String },
    /// The TCP connection to `server_address` opened and the TLS handshake on
    /// it did not finish, for the reason in `error_detail`.
    #[error("the TLS handshake with {server_address} failed: {error_detail}")]
    TlsHandshakeFailed {
        server_address: String,
        error_detail: String,
    },
    /// The server at `address` presented a different certificate than the one
    /// pinned the first time it was dialled.
    #[error(
        "the certificate of {server_address} changed: pinned {pinned_certificate}, presented {presented_certificate}. \
         if the server was reinstalled on purpose, run `koshi remote forget {server_address}` \
         and connect again."
    )]
    CertificateChanged {
        server_address: String,
        pinned_certificate: String,
        presented_certificate: String,
    },
}

/// Which of the four files under `remote/` an
/// [`IpcError::RemoteFileUnreadable`] or [`IpcError::RemoteFileWrite`] names.
/// `Display` writes `saved servers file`, `remote access certificate`,
/// `remote access record` or `remote access token store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFile {
    /// The servers this user has dialled and saved, on the dialling machine.
    SavedServers,
    /// This machine's own certificate for the remote listener.
    Certificate,
    /// The record that remote access was switched on for this machine.
    RemoteAccessMark,
    /// The remote access grants this machine has handed out.
    TokenStore,
}

impl std::fmt::Display for RemoteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remote_file_name = match self {
            Self::SavedServers => "saved servers file",
            Self::Certificate => "remote access certificate",
            Self::RemoteAccessMark => "remote access record",
            Self::TokenStore => "remote access token store",
        };
        f.write_str(remote_file_name)
    }
}

impl DomainError for IpcError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Ipc
    }

    fn get_severity(&self) -> Severity {
        match self {
            IpcError::Transport { .. }
            | IpcError::Disconnected
            | IpcError::FrameTooLarge { .. }
            | IpcError::UntrustedSocket { .. }
            | IpcError::NoListener { .. }
            | IpcError::SocketBusy { .. }
            | IpcError::EndpointFileMissing { .. }
            | IpcError::EndpointFileUnreadable { .. }
            | IpcError::RemoteFileUnreadable { .. }
            | IpcError::RemoteFileWrite { .. }
            | IpcError::ConnectRefused { .. }
            | IpcError::ConnectTimedOut { .. }
            | IpcError::TlsHandshakeFailed { .. }
            | IpcError::CertificateChanged { .. } => Severity::ClientFatal,
            IpcError::EndpointFileWrite { .. } | IpcError::AdvertWrite { .. } => {
                Severity::SessionFatal
            }
            IpcError::MalformedFrame { .. } => Severity::Recoverable,
        }
    }
}

#[cfg(test)]
mod tests;
