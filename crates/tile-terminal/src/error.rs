//! Terminal domain error. Classifies into [`DomainCategory::Terminal`] (`TILE_12`).

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

/// A failure in terminal emulation (parsing or grid operations). Recoverable:
/// a malformed sequence is dropped and emulation continues.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// A control sequence could not be processed.
    #[error("terminal parse error: {detail}")]
    Parse { detail: String },
}

impl DomainError for TerminalError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Terminal
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
