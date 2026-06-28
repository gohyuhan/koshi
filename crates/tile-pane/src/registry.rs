//! Pane registry: runtime metadata for every pane, keyed by id.
//!
//! Layout trees hold bare `PaneId` leaves; everything else about a pane —
//! title, command, lifecycle — lives in the registry, so layout stays pure
//! geometry and runtime state has exactly one owner.

use std::collections::HashMap;

use tile_core::ids::PaneId;

use crate::{error::PaneRegistryError, pane::state::PaneRecord};

/// Owns the [`PaneRecord`] of every pane in one session, keyed by id. The map
/// is private: records go in and out only through the methods below, so the
/// "one id, one record" invariant has a single chokepoint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneRegistry {
    records: HashMap<PaneId, PaneRecord>,
}

impl PaneRegistry {
    /// Creates a new empty pane registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a pane record, keyed by its id. Returns an error if the id is
    /// already registered; the existing record is untouched.
    pub fn insert(&mut self, pane_record: PaneRecord) -> Result<(), PaneRegistryError> {
        if self.records.contains_key(&pane_record.id()) {
            return Err(PaneRegistryError::DuplicateId {
                id: pane_record.id(),
                kind: pane_record.kind().clone(),
            });
        }
        self.records.insert(pane_record.id(), pane_record);
        Ok(())
    }

    /// Returns a reference to the record for a pane id, if it is registered.
    #[must_use]
    pub fn get(&self, pane_id: PaneId) -> Option<&PaneRecord> {
        self.records.get(&pane_id)
    }

    /// Removes and returns the record for a pane id if it is registered.
    pub fn remove(&mut self, pane_id: PaneId) -> Option<PaneRecord> {
        self.records.remove(&pane_id)
    }

    /// Mutable access to a record for in-place field edits (title, lifecycle,
    /// exit status, …).
    ///
    /// `id` is read-only — the record exposes it through [`PaneRecord::id`] but
    /// not as a mutable field — so a record can never desync from its map key
    /// through this handle. Changing a pane's id is therefore a remove under the
    /// old id followed by an insert under the new one, never an in-place edit.
    pub fn get_mut(&mut self, pane_id: PaneId) -> Option<&mut PaneRecord> {
        self.records.get_mut(&pane_id)
    }

    /// Returns an iterator over all registered pane records.
    pub fn list(&self) -> impl Iterator<Item = &PaneRecord> {
        self.records.values()
    }

    /// Returns the count of registered pane records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns true if the registry contains no pane records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests;
