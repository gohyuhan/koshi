//! IPC domain error. Classifies into [`DomainCategory::Ipc`] (`TILE_12`).

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

/// A failure on the control channel. A broken IPC link is client-fatal: the
/// affected client must tear down, but the session keeps serving others.
#[derive(Debug, Error)]
pub enum IpcError {
    /// The underlying transport failed.
    #[error("ipc transport error: {detail}")]
    Transport { detail: String },
    /// The peer disconnected unexpectedly.
    #[error("ipc peer disconnected")]
    Disconnected,
}

impl DomainError for IpcError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Ipc
    }

    fn severity(&self) -> Severity {
        Severity::ClientFatal
    }
}
