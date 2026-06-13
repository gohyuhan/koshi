//! Error types for pane-registry and pane-lifecycle operations.

use thiserror::Error;
use tile_core::{
    error::{DomainCategory, DomainError, Severity},
    ids::PaneId,
};

use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};

/// Why a pane-registry operation was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PaneRegistryError {
    /// A record was inserted under an id the registry already holds.
    #[error("pane {0} is already registered")]
    DuplicateId(PaneId),
}

impl DomainError for PaneRegistryError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Terminal
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
}

impl DomainError for InvalidTransition {
    fn category(&self) -> DomainCategory {
        DomainCategory::Terminal
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
