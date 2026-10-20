//! Error types for pane-registry operations.

use koshi_core::ids::PaneId;
use thiserror::Error;

/// Why the pane registry rejected an operation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PaneRegistryError {
    /// An insert used an id that the registry already holds. `PaneId` displays
    /// as `pane-<uuid>`, so the message reads `pane-<uuid> is already
    /// registered`.
    #[error("{pane_id} is already registered")]
    DuplicateId {
        /// The pane identifier that is already registered.
        pane_id: PaneId,
    },
}

#[cfg(test)]
mod tests;
