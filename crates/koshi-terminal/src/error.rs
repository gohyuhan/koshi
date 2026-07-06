//! Terminal domain error. Classifies into [`DomainCategory::Terminal`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure in terminal emulation (parsing or grid operations). Recoverable:
/// a malformed sequence is dropped and emulation continues.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// A control sequence could not be processed.
    #[error("terminal parse error: {detail}")]
    Parse { detail: String },
}

impl DomainError for TerminalError {
    /// Returns the error category as Terminal.
    fn category(&self) -> DomainCategory {
        DomainCategory::Terminal
    }

    /// Returns the severity as Recoverable, since malformed sequences are dropped
    /// and emulation continues without crashing.
    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
