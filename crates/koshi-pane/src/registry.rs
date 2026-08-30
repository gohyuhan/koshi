//! Pane registry: runtime metadata for every pane, keyed by id.
//!
//! A layout tree holds bare `PaneId` leaves. The registry holds everything else
//! about a pane: its command, its working directory and its lifecycle state.

use std::collections::HashMap;

use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

use crate::{error::PaneRegistryError, pane::state::PaneRecord};

/// Owns the [`PaneRecord`] of every pane in one session, keyed by id. The map
/// is private. Records go in and out only through the methods below.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneRegistry {
    records: HashMap<PaneId, PaneRecord>,
}

impl PaneRegistry {
    /// Creates a new empty pane registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a pane record, keyed by its id. Returns
    /// [`PaneRegistryError::DuplicateId`] when the id is already registered.
    /// The error carries the id and the kind of the rejected record. The
    /// existing record stays untouched.
    pub fn insert(&mut self, pane_record: PaneRecord) -> Result<(), PaneRegistryError> {
        if self.records.contains_key(&pane_record.id()) {
            return Err(PaneRegistryError::DuplicateId {
                id: pane_record.id(),
                kind: *pane_record.kind(),
            });
        }
        self.records.insert(pane_record.id(), pane_record);
        Ok(())
    }

    /// Returns a reference to the record for `pane_id`. Returns `None` when the
    /// id is not registered.
    #[must_use]
    pub fn get(&self, pane_id: PaneId) -> Option<&PaneRecord> {
        self.records.get(&pane_id)
    }

    /// Removes the record for `pane_id` and returns it. Returns `None` when the
    /// id is not registered.
    pub fn remove(&mut self, pane_id: PaneId) -> Option<PaneRecord> {
        self.records.remove(&pane_id)
    }

    /// Returns a mutable reference to the record for `pane_id`. Use it to edit
    /// fields in place, such as the policies or the working directory. Returns
    /// `None` when the id is not registered.
    ///
    /// The record keeps the id it was created with; [`PaneRecord::id`] reads
    /// it.
    pub fn get_mut(&mut self, pane_id: PaneId) -> Option<&mut PaneRecord> {
        self.records.get_mut(&pane_id)
    }

    /// Returns an iterator over every registered pane record. The order is not
    /// defined.
    pub fn list(&self) -> impl Iterator<Item = &PaneRecord> {
        self.records.values()
    }

    /// Returns the count of registered pane records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns `true` when the registry holds no pane records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests;
