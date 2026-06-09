//! PTY domain error. Classifies into [`DomainCategory::Pty`].

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

/// A failure spawning or driving a child PTY. Pane-level failures are
/// recoverable: a dead PTY closes its pane without crashing the session.
#[derive(Debug, Error)]
pub enum PtyError {
    /// The child process could not be spawned.
    #[error("failed to spawn pty: {detail}")]
    Spawn { detail: String },
    /// Reading from or writing to the PTY failed.
    #[error("pty io error: {detail}")]
    Io { detail: String },
}

impl DomainError for PtyError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Pty
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
