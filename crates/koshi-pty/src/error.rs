//! PTY (pseudo-terminal — the OS channel a spawned shell or program runs
//! inside) domain error.

use koshi_core::ids::PaneId;
use thiserror::Error;

/// A failure spawning or driving a child PTY. A dead PTY closes its pane
/// without crashing the session.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PtyError {
    /// The child process could not be spawned.
    #[error("failed to spawn pty: {detail}")]
    Spawn { detail: String },
    /// Reading from or writing to the PTY failed.
    #[error("pty io error: {detail}")]
    Io { detail: String },
    /// An operation named a pane the backend never spawned (or already removed).
    #[error("invalid pane: id - {pane_id}")]
    UnknownPane { pane_id: PaneId },
    /// Delivering a termination signal (Unix) or a Job-Object/`TerminateProcess`
    /// call (Windows) to the child failed, or a pane's Job Objects could not be
    /// created at spawn (Windows).
    #[error("pty signal error: {detail}")]
    Signal { detail: String },
    /// The pane's child has not taken the input queued for it, and the input
    /// written next would pass [`crate::backend::state::MAX_QUEUED_PANE_INPUT_BYTE_COUNT`].
    #[error("pane {pane_id} is not reading its input: {queued_byte_count} bytes wait for it")]
    InputQueueFull {
        pane_id: PaneId,
        queued_byte_count: usize,
    },
}

#[cfg(test)]
mod tests;
