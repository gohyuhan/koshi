//! Storage domain error. Defines [`StorageError`], the error type returned by
//! persistence operations, and its [`DomainError`] impl that classifies every
//! variant into [`DomainCategory::Storage`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure persisting or loading state. I/O failures are recoverable (retry
/// or skip the snapshot); a corrupt store means core state is unusable and is
/// session-fatal.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Reading or writing the store failed.
    #[error("storage io error: {detail}")]
    Io { detail: String },
    /// Persisted state failed integrity checks.
    #[error("corrupt stored state: {detail}")]
    Corrupt { detail: String },
}

impl DomainError for StorageError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Storage
    }

    fn severity(&self) -> Severity {
        match self {
            StorageError::Io { .. } => Severity::Recoverable,
            StorageError::Corrupt { .. } => Severity::SessionFatal,
        }
    }
}
