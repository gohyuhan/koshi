//! Pane metadata: the per-pane runtime record the registry owns.
//!
//! A layout tree holds only a `PaneId` at each leaf. [`PaneRecord`] holds
//! everything else about that pane: its spawn specification, its working
//! directory, what its program asked a person to look at, which of those
//! requests it posted a notification entry for, and what its programs
//! reported about themselves.

use std::path::PathBuf;

use koshi_core::{
    attention::{Attention, AttentionReasonKind},
    ids::PaneId,
    process::SpawnSpec,
    program_status::ProgramStatusRecord,
};
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
    /// What this pane last asked a person to look at, and since when. `None`
    /// for a pane that waits for nobody: it never asked, or a client attended
    /// it since. A record without `attention` is refused.
    #[serde(deserialize_with = "Option::deserialize")]
    pub attention: Option<Attention>,
    /// The program status records the pane's programs reported through
    /// OSC 7501, in the order they were written, oldest first. At most
    /// [`PROGRAM_STATUS_RECORDS_MAX`](koshi_core::program_status::PROGRAM_STATUS_RECORDS_MAX),
    /// one for each record id. A record without `program_status_records` is
    /// refused.
    pub program_status_records: Vec<ProgramStatusRecord>,
    /// The reason kinds this pane posted a notification entry for since a
    /// client last attended it, in the order it posted them, each once.
    /// Empty for a pane that posted none since. A record without
    /// `notified_attention_kinds` is refused.
    pub notified_attention_kinds: Vec<AttentionReasonKind>,
}

impl PaneRecord {
    /// A record for the terminal pane `pane_id` with no spawn specification,
    /// no working directory, no attention, no program status record, and no
    /// notified attention kind.
    pub fn from_terminal_pane(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            spawn_spec: None,
            working_directory: None,
            attention: None,
            program_status_records: Vec::new(),
            notified_attention_kinds: Vec::new(),
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
