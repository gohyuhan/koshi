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
/// [`TlsHandshakeFailed`](IpcError::TlsHandshakeFailed)), a refused frame
/// ([`FrameTooLarge`](IpcError::FrameTooLarge)), a socket address that fails
/// its trust or liveness checks
/// ([`UntrustedSocket`](IpcError::UntrustedSocket),
/// [`NoListener`](IpcError::NoListener), [`SocketBusy`](IpcError::SocketBusy)),
/// an endpoint file the caller cannot read
/// ([`EndpointFileMissing`](IpcError::EndpointFileMissing),
/// [`EndpointFileUnreadable`](IpcError::EndpointFileUnreadable)), and a
/// remote access token store the caller cannot read or write
/// ([`TokenStoreUnreadable`](IpcError::TokenStoreUnreadable),
/// [`TokenStoreWrite`](IpcError::TokenStoreWrite)).
///
/// Session-fatal: a failed endpoint-file write
/// ([`EndpointFileWrite`](IpcError::EndpointFileWrite)). It happens during the
/// session's own startup, and a session whose endpoint file never lands is one
/// no caller can reach.
///
/// Recoverable: a frame that arrived whole yet does not decode
/// ([`MalformedFrame`](IpcError::MalformedFrame)). The stream is still aligned
/// on frame boundaries, so the connection can answer and keep going.
#[derive(Debug, Error)]
pub enum IpcError {
    /// The underlying transport failed.
    #[error("ipc transport error: {detail}")]
    Transport { detail: String },
    /// The peer disconnected unexpectedly.
    #[error("ipc peer disconnected")]
    Disconnected,
    /// A frame longer than
    /// [`MAX_FRAME_LEN`](crate::transport::MAX_FRAME_LEN). On receive, the
    /// length prefix named more bytes than the limit and the payload is left
    /// unread, so the stream is off frame boundaries and the connection must
    /// close; `len` is the length the prefix named. On send, encoding stopped
    /// at the byte that crossed the limit and nothing was written; `len` is
    /// the payload size the refused write reached, which for a message
    /// encoded in one piece is its full size.
    #[error("ipc frame of {len} bytes exceeds the {max}-byte limit")]
    FrameTooLarge { len: u64, max: u32 },
    /// A frame whose bytes are not a readable message: the payload arrived
    /// whole but did not decode, or a message failed to encode.
    #[error("ipc frame is not a readable message: {detail}")]
    MalformedFrame { detail: String },
    /// A socket address that failed a trust check, named in `reason`: the
    /// path is not directly inside the koshi runtime directory, that
    /// directory is not private, or (Windows) the pipe name is outside the
    /// `koshi-` namespace.
    #[error("untrusted socket address {addr}: {reason}")]
    UntrustedSocket { addr: String, reason: String },
    /// Nothing listens at the address: what is there is a leftover from a
    /// process that is gone, or nothing exists there at all.
    #[error("no koshi is listening at {addr}")]
    NoListener { addr: String },
    /// A live listener already holds the address this process wants to bind.
    #[error("another process is already listening at {addr}")]
    SocketBusy { addr: String },
    /// No endpoint file at the path: no running koshi has advertised a
    /// control socket there.
    #[error("no endpoint file at {path}")]
    EndpointFileMissing { path: String },
    /// An endpoint file that exists but could not be used: reading it
    /// failed, or its bytes are not a readable endpoint file.
    #[error("endpoint file {path} is unreadable: {detail}")]
    EndpointFileUnreadable { path: String, detail: String },
    /// Writing the endpoint file failed during session startup, so no
    /// caller will ever find this session's socket.
    #[error("endpoint file {path} could not be written: {detail}")]
    EndpointFileWrite { path: String, detail: String },
    /// A remote access token store that exists but could not be used:
    /// reading it failed, its bytes are not a readable store, or its format
    /// number is not the one this build reads.
    #[error("token store {path} is unreadable: {detail}")]
    TokenStoreUnreadable { path: String, detail: String },
    /// Writing the remote access token store failed, so the grant or the
    /// revocation the caller made never reached the disk.
    #[error("token store {path} could not be written: {detail}")]
    TokenStoreWrite { path: String, detail: String },
    /// A remote access file that exists but could not be used: reading it
    /// failed, its bytes are not readable, or its format number is not the
    /// one this build reads.
    #[error("the {file} at {path} is unreadable: {detail}")]
    RemoteFileUnreadable {
        file: RemoteFile,
        path: String,
        detail: String,
    },
    /// Writing a remote access file failed, so what the caller changed never
    /// reached the disk.
    #[error("the {file} at {path} could not be written: {detail}")]
    RemoteFileWrite {
        file: RemoteFile,
        path: String,
        detail: String,
    },
    /// Nothing accepted the TCP connection at `address`.
    #[error(
        "{address} refused the connection: nothing is listening on that port. \
         if remote access is not enabled on that machine, run `koshi share grant` \
         there and answer yes to the offer to open the port"
    )]
    ConnectRefused { address: String },
    /// The TCP connection to `address` was still unanswered when the dial ran
    /// out of time.
    #[error(
        "connecting to {address} timed out: nothing answered. check that the \
         machine is up, the address and port are right, and the network path \
         allows it"
    )]
    ConnectTimedOut { address: String },
    /// The TCP connection to `address` opened and the TLS handshake on it did
    /// not finish, for the reason in `detail`.
    #[error("the TLS handshake with {address} failed: {detail}")]
    TlsHandshakeFailed { address: String, detail: String },
    /// The server at `address` presented a different certificate than the one
    /// pinned the first time it was dialled.
    #[error(
        "the certificate of {address} changed: pinned {pinned}, presented {presented}. \
         if the server was reinstalled on purpose, run `koshi remote forget {address}` \
         and connect again."
    )]
    CertificateChanged {
        address: String,
        pinned: String,
        presented: String,
    },
}

/// Which remote access file an [`IpcError::RemoteFileUnreadable`] or
/// [`IpcError::RemoteFileWrite`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFile {
    /// The servers this user has dialled and saved, on the dialling machine.
    SavedServers,
    /// This machine's own certificate for the remote listener.
    Certificate,
    /// The record that remote access was switched on for this machine.
    RemoteAccessMark,
}

impl std::fmt::Display for RemoteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::SavedServers => "saved servers file",
            Self::Certificate => "remote access certificate",
            Self::RemoteAccessMark => "remote access record",
        };
        f.write_str(name)
    }
}

impl DomainError for IpcError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Ipc
    }

    fn severity(&self) -> Severity {
        match self {
            IpcError::Transport { .. }
            | IpcError::Disconnected
            | IpcError::FrameTooLarge { .. }
            | IpcError::UntrustedSocket { .. }
            | IpcError::NoListener { .. }
            | IpcError::SocketBusy { .. }
            | IpcError::EndpointFileMissing { .. }
            | IpcError::EndpointFileUnreadable { .. }
            | IpcError::TokenStoreUnreadable { .. }
            | IpcError::TokenStoreWrite { .. }
            | IpcError::RemoteFileUnreadable { .. }
            | IpcError::RemoteFileWrite { .. }
            | IpcError::ConnectRefused { .. }
            | IpcError::ConnectTimedOut { .. }
            | IpcError::TlsHandshakeFailed { .. }
            | IpcError::CertificateChanged { .. } => Severity::ClientFatal,
            IpcError::EndpointFileWrite { .. } => Severity::SessionFatal,
            IpcError::MalformedFrame { .. } => Severity::Recoverable,
        }
    }
}

#[cfg(test)]
mod tests;
