//! Pane registry: runtime metadata for every pane, keyed by id.
//!
//! A layout tree holds bare `PaneId` leaves. The registry holds everything else
//! about a pane: its command, its working directory and its lifecycle state.

use std::collections::BTreeMap;

use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

use crate::{error::PaneRegistryError, pane::state::PaneRecord};

/// Owns each pane record for one session, keyed by pane id. The map is
/// private; [`Self::list`] yields records in id order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneRegistry {
    records: BTreeMap<PaneId, PaneRecord>,
}

impl PaneRegistry {
    /// Creates a new empty pane registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a pane record, keyed by its id. Returns
    /// [`PaneRegistryError::DuplicateId`] when the id is already registered;
    /// that error carries the rejected record's id and kind, and the existing
    /// record stays unchanged.
    pub fn insert(&mut self, pane_record: PaneRecord) -> Result<(), PaneRegistryError> {
        let pane_id = pane_record.id();
        if self.records.contains_key(&pane_id) {
            return Err(PaneRegistryError::DuplicateId {
                id: pane_id,
                kind: *pane_record.kind(),
            });
        }
        self.records.insert(pane_id, pane_record);
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

    /// Returns a mutable reference to the record for `pane_id`, or `None` when
    /// the id is not registered. Callers can edit fields in place, including
    /// policies and the working directory. The record keeps its original id,
    /// which [`PaneRecord::id`] returns.
    pub fn get_mut(&mut self, pane_id: PaneId) -> Option<&mut PaneRecord> {
        self.records.get_mut(&pane_id)
    }

    /// Returns an iterator over every registered pane record, in id order.
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
