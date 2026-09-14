//! Pane registry: runtime metadata for every pane, keyed by pane id.
//!
//! A layout tree holds bare `PaneId` leaves. The registry holds everything else
//! about a pane: its spawn specification, its working directory and its
//! lifecycle state.

use std::collections::BTreeMap;

use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

use crate::{error::PaneRegistryError, pane::state::PaneRecord};

/// Owns each pane record for one session, keyed by pane id. The map is
/// private; [`Self::list_pane_records`] yields records in id order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneRegistry {
    #[serde(rename = "records")]
    pane_record_by_id: BTreeMap<PaneId, PaneRecord>,
}

impl PaneRegistry {
    /// Creates a new empty pane registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a pane record, keyed by its id. Returns
    /// [`PaneRegistryError::DuplicateId`] when the id is already registered;
    /// that error carries the rejected record's id and kind, and the existing
    /// record stays unchanged.
    pub fn register_pane_record(
        &mut self,
        pane_record: PaneRecord,
    ) -> Result<(), PaneRegistryError> {
        let pane_id = pane_record.get_pane_id();
        if self.pane_record_by_id.contains_key(&pane_id) {
            return Err(PaneRegistryError::DuplicateId {
                pane_id,
                pane_kind: *pane_record.get_pane_kind(),
            });
        }
        self.pane_record_by_id.insert(pane_id, pane_record);
        Ok(())
    }

    /// Returns a reference to the record for `pane_id`. Returns `None` when the
    /// id is not registered.
    #[must_use]
    pub fn get_pane_record_by_id(&self, pane_id: PaneId) -> Option<&PaneRecord> {
        self.pane_record_by_id.get(&pane_id)
    }

    /// Removes the record for `pane_id` and returns it. Returns `None` when the
    /// id is not registered.
    pub fn remove_pane_record(&mut self, pane_id: PaneId) -> Option<PaneRecord> {
        self.pane_record_by_id.remove(&pane_id)
    }

    /// Returns a mutable reference to the record for `pane_id`, or `None` when
    /// the id is not registered. Callers can edit fields in place, including
    /// policies and the working directory. The record keeps its original id,
    /// which [`PaneRecord::get_pane_id`] returns.
    pub fn get_pane_record_mut_by_id(&mut self, pane_id: PaneId) -> Option<&mut PaneRecord> {
        self.pane_record_by_id.get_mut(&pane_id)
    }

    /// Returns an iterator over every registered pane record, in id order.
    pub fn list_pane_records(&self) -> impl Iterator<Item = &PaneRecord> {
        self.pane_record_by_id.values()
    }

    /// Returns the count of registered pane records.
    #[must_use]
    pub fn pane_record_count(&self) -> usize {
        self.pane_record_by_id.len()
    }

    /// Returns `true` when the registry holds one or more pane records.
    #[must_use]
    pub fn has_pane_records(&self) -> bool {
        !self.pane_record_by_id.is_empty()
    }
}

#[cfg(test)]
mod tests;
