//! The action registry — the table of every action koshi can perform.
//!
//! [`action`](crate::action) defines what an action reference looks like and
//! ships the built-in set; this module holds that set for lookup at run time.
//!
//! The registry answers one question: given this [`ActionReference`], what do we
//! know about it? Mapping keys to actions is the keymap's job; turning an
//! action into a [`Command`](crate::command::Command) is the resolver's.

use std::collections::HashMap;

use crate::action::{build_core_action_seeds, ActionMetadata, ActionReference};

/// Every action koshi can perform, keyed by reference.
///
/// Built with [`new`](ActionRegistry::new), which loads the built-in `core:`
/// table.
#[derive(Debug)]
pub struct ActionRegistry {
    /// Each known action and what the runtime knows about it.
    action_metadata_by_reference: HashMap<ActionReference, ActionMetadata>,
}

impl ActionRegistry {
    /// Build a registry holding the built-in `core:` actions.
    #[must_use]
    pub fn new() -> Self {
        ActionRegistry {
            action_metadata_by_reference: build_core_action_seeds().into_iter().collect(),
        }
    }

    /// The metadata of `action_reference`, or `None` when the reference names no entry.
    #[must_use]
    pub fn find_action_metadata(
        &self,
        action_reference: &ActionReference,
    ) -> Option<&ActionMetadata> {
        self.action_metadata_by_reference.get(action_reference)
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        ActionRegistry::new()
    }
}

#[cfg(test)]
mod tests;
