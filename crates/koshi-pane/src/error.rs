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
    /// An insert used an id that the registry already holds.
    #[error("pane {id} is already registered")]
    DuplicateId {
        /// The id that is already registered.
        id: PaneId,
        /// The kind of the record that the insert rejected.
        kind: PaneKind,
    },
}

impl DomainError for PaneRegistryError {
    fn category(&self) -> DomainCategory {
        match self {
            PaneRegistryError::DuplicateId { kind, .. } => kind.domain_category(),
        }
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// An attempt to move a pane through an illegal lifecycle step.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal pane lifecycle transition from {from:?} on {event:?}")]
pub struct InvalidTransition {
    /// The state the pane was in.
    pub from: PaneLifecycle,
    /// The event that was rejected.
    pub event: PaneLifecycleEvent,
    /// The kind of the pane, terminal or plugin.
    pub kind: PaneKind,
}

impl DomainError for InvalidTransition {
    fn category(&self) -> DomainCategory {
        self.kind.domain_category()
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

#[cfg(test)]
mod tests;
