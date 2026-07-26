//! Storage domain error: [`StorageError`], returned by persistence operations.
//! Its [`DomainError`] impl puts every variant in
//! [`DomainCategory::Storage`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure persisting or loading state. I/O failures are recoverable; a
/// corrupt store leaves core state unusable and is session-fatal.
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

#[cfg(test)]
mod tests;
