//! Session domain errors. Classify into [`DomainCategory::Session`].

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

use crate::session::lifecycle::{SessionLifecycle, SessionLifecycleEvent};

/// An attempt to move a session through an illegal lifecycle step.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("illegal session lifecycle transition from {from:?} on {event:?}")]
pub struct InvalidTransition {
    /// The state the session was in.
    pub from: SessionLifecycle,
    /// The event that was rejected.
    pub event: SessionLifecycleEvent,
}

impl DomainError for InvalidTransition {
    fn category(&self) -> DomainCategory {
        DomainCategory::Session
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
