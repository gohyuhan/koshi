//! Pane metadata: the per-pane runtime record the registry owns.
//!
//! A layout tree holds only a `PaneId` at each leaf. [`PaneRecord`] holds
//! everything else about that pane: its spawn specification and its working
//! directory.

use std::path::PathBuf;

use koshi_core::{ids::PaneId, process::SpawnSpec};
use serde::{Deserialize, Serialize};

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
}

impl PaneRecord {
    /// A record for the terminal pane `pane_id` with no spawn specification
    /// and no working directory.
    pub fn from_terminal_pane(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            spawn_spec: None,
            working_directory: None,
        }
    }

    /// The stable pane id. It matches the layout leaf and the registry key.
    #[must_use]
    pub fn get_pane_id(&self) -> PaneId {
        self.pane_id
    }
}

#[cfg(test)]
mod tests;
