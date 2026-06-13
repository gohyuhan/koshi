//! Error types for pane-registry operations.

use thiserror::Error;
use tile_core::ids::PaneId;

/// Why a pane-registry operation was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PaneRegistryError {
    /// A record was inserted under an id the registry already holds.
    #[error("pane {0} is already registered")]
    DuplicateId(PaneId),
}
