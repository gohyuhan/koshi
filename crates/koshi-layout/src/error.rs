//! Layout domain error. Classifies into [`DomainCategory::Layout`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure in the layout engine. Recoverable: a rejected resize or solve
/// leaves the prior layout intact and reports why.
#[derive(Debug, Error)]
pub enum LayoutError {
    /// A resize would take a pane below its minimum size.
    #[error("layout minimum-size violation: {detail}")]
    MinSize { detail: String },
    /// The geometry solver could not satisfy the constraints.
    #[error("layout solve failed: {detail}")]
    Solve { detail: String },
}

impl DomainError for LayoutError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Layout
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
