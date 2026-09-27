//! Storage errors returned by persistence operations.

use thiserror::Error;

/// A persistence or load failure.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A read or write operation failed.
    #[error("storage io error: {detail}")]
    Io { detail: String },
    /// Persisted state failed an integrity check.
    #[error("corrupt stored state: {detail}")]
    Corrupt { detail: String },
}

#[cfg(test)]
mod tests;
