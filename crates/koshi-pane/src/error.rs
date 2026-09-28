//! Error types for pane-registry and pane-lifecycle operations.

use koshi_core::ids::PaneId;
use thiserror::Error;

use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};

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

/// An attempt to move a pane through an illegal lifecycle step.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal pane lifecycle transition from {previous_lifecycle:?} on {lifecycle_event:?}")]
pub struct InvalidTransitionError {
    /// The state the pane was in.
    pub previous_lifecycle: PaneLifecycle,
    /// The event that was rejected.
    pub lifecycle_event: PaneLifecycleEvent,
}

#[cfg(test)]
mod tests;
