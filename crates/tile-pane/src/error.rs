//! Error types for pane-registry and pane-lifecycle operations.

use thiserror::Error;
use tile_core::{
    error::{DomainCategory, DomainError, Severity},
    ids::PaneId,
};

use crate::pane::{
    lifecycle::{PaneLifecycle, PaneLifecycleEvent},
    state::PaneKind,
};

/// Why a pane-registry operation was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PaneRegistryError {
    /// A record was inserted under an id the registry already holds.
    #[error("pane {id} is already registered")]
    DuplicateId { id: PaneId, kind: PaneKind },
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
    /// The kind of pane, terminal or plugin
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
