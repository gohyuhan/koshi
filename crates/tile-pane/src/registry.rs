//! Pane registry: runtime metadata for every pane, keyed by id.
//!
//! Layout trees hold bare `PaneId` leaves; everything else about a pane —
//! title, command, lifecycle — lives in the registry, so layout stays pure
//! geometry and runtime state has exactly one owner.

/// The pane records of one session. Placeholder shell: the pane metadata
/// model fills in the record type and the map operations.
#[derive(Debug)]
pub struct PaneRegistry;
