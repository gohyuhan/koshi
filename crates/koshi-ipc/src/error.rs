//! IPC domain error. Classifies into [`koshi_core::error::DomainCategory::Ipc`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure on the control channel.
///
/// A broken link ([`Transport`](IpcError::Transport),
/// [`Disconnected`](IpcError::Disconnected)) and a refused frame
/// ([`FrameTooLarge`](IpcError::FrameTooLarge)) are client-fatal: the affected
/// connection must tear down, but the session keeps serving others. A frame
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
}

impl DomainError for IpcError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Ipc
    }

    fn severity(&self) -> Severity {
        match self {
            IpcError::Transport { .. }
            | IpcError::Disconnected
            | IpcError::FrameTooLarge { .. } => Severity::ClientFatal,
            IpcError::MalformedFrame { .. } => Severity::Recoverable,
        }
    }
}

#[cfg(test)]
mod tests;
