//! Error types for pane-registry and pane-lifecycle operations.

use koshi_core::{
    error::{DomainCategory, DomainError, Severity},
    ids::PaneId,
};
use thiserror::Error;

use crate::pane::{
    lifecycle::{PaneLifecycle, PaneLifecycleEvent},
    state::PaneKind,
};

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
        /// The kind of the record that the insert rejected.
        pane_kind: PaneKind,
    },
}

impl DomainError for PaneRegistryError {
    fn category(&self) -> DomainCategory {
        match self {
            PaneRegistryError::DuplicateId { pane_kind, .. } => pane_kind.domain_category(),
        }
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// An attempt to move a pane through an illegal lifecycle step.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal pane lifecycle transition from {previous_lifecycle:?} on {lifecycle_event:?}")]
pub struct InvalidTransitionError {
    /// The state the pane was in.
    pub previous_lifecycle: PaneLifecycle,
    /// The event that was rejected.
    pub lifecycle_event: PaneLifecycleEvent,
    /// The kind of the pane, terminal or plugin.
    pub pane_kind: PaneKind,
}

impl DomainError for InvalidTransitionError {
    fn category(&self) -> DomainCategory {
        self.pane_kind.domain_category()
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

#[cfg(test)]
mod tests;
