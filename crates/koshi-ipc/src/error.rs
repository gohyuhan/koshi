//! IPC domain error. Classifies into [`koshi_core::error::DomainCategory::Ipc`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure on the control channel.
///
/// A broken link ([`Transport`](IpcError::Transport),
/// [`Disconnected`](IpcError::Disconnected)) and a refused frame
/// ([`FrameTooLarge`](IpcError::FrameTooLarge)) are client-fatal: the affected
/// connection must tear down, but the session keeps serving others. A socket
/// address that fails its trust or liveness checks
/// ([`UntrustedSocket`](IpcError::UntrustedSocket),
/// [`NoListener`](IpcError::NoListener), [`SocketBusy`](IpcError::SocketBusy))
/// is client-fatal too: no connection comes up at all, as is an endpoint
/// file that is missing, unusable, or unwritable
/// ([`EndpointFileMissing`](IpcError::EndpointFileMissing),
/// [`EndpointFileUnreadable`](IpcError::EndpointFileUnreadable),
/// [`EndpointFileWrite`](IpcError::EndpointFileWrite)): without it no caller
/// can find the socket or the token. A frame
/// that arrived whole yet does not decode
/// ([`MalformedFrame`](IpcError::MalformedFrame)) is recoverable: the stream
/// is still aligned on frame boundaries, so the connection can answer and
/// keep going.
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
    /// Writing the endpoint file failed, so no caller can find this
    /// session's socket.
    #[error("endpoint file {path} could not be written: {detail}")]
    EndpointFileWrite { path: String, detail: String },
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
            | IpcError::EndpointFileWrite { .. } => Severity::ClientFatal,
            IpcError::MalformedFrame { .. } => Severity::Recoverable,
        }
    }
}

#[cfg(test)]
mod tests;
