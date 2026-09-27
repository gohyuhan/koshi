//! Pane metadata: the per-pane runtime record the registry owns.
//!
//! A layout tree holds only a `PaneId` at each leaf. [`PaneRecord`] holds
//! everything else about that pane: its spawn specification, its working
//! directory, its close policy and its lifecycle state.

use std::path::PathBuf;

use koshi_core::{ids::PaneId, process::SpawnSpec};
use serde::{Deserialize, Serialize};

use crate::error::InvalidTransitionError;
use crate::pane::{
    lifecycle::{PaneLifecycle, PaneLifecycleEvent},
    policy::PaneClosePolicy,
};

/// Runtime metadata for a single terminal pane. The registry keys the record
/// by `pane_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRecord {
    /// The stable pane id, which matches the layout leaf that references this
    /// pane. The pane id never changes.
    pane_id: PaneId,
    /// The process spawn specification, when the pane has one.
    pub spawn_spec: Option<SpawnSpec>,
    /// The working directory that the pane starts in, when it is known.
    pub working_directory: Option<PathBuf>,
    /// How the pane carries out a requested close.
    pub close_policy: PaneClosePolicy,
    /// Where the pane sits in its lifecycle.
    lifecycle: PaneLifecycle,
}

impl PaneRecord {
    /// A fresh `Spawning` record for a terminal pane.
    pub fn from_terminal_pane(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            spawn_spec: None,
            working_directory: None,
            close_policy: PaneClosePolicy::default(),
            lifecycle: PaneLifecycle::Spawning,
        }
    }

    /// The stable pane id. It matches the layout leaf and the registry key.
    #[must_use]
    pub fn get_pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// Where this pane sits in its lifecycle state machine.
    pub fn get_lifecycle(&self) -> &PaneLifecycle {
        &self.lifecycle
    }

    /// Applies a lifecycle `lifecycle_event` and advances the pane's state. Returns
    /// [`InvalidTransitionError`] when the step is illegal from the current state,
    /// and leaves the state unchanged. This is the only way to change
    /// `lifecycle`.
    pub fn update_lifecycle(
        &mut self,
        lifecycle_event: PaneLifecycleEvent,
    ) -> Result<(), InvalidTransitionError> {
        self.lifecycle = self.lifecycle.transition(lifecycle_event)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
