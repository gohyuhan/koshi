//! The program status that a pane's program reports through OSC 7501, the
//! Program Status Protocol: the state the program is in, what it needs from a
//! person while it is blocked, and the records a pane keeps of each report.
//!
//! Example: `ESC ] 7501 ; state=blocked:kind=permission ESC \` names the state
//! [`ProgramStatusState::Blocked`] and the kind [`BlockedKind::Permission`].

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::attention::AttentionReason;

/// The most program status records one pane keeps: `64`. A report for a new
/// id while a pane holds 64 records removes the record written longest ago.
pub const PROGRAM_STATUS_RECORDS_MAX: usize = 64;

/// The state a program reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgramStatusState {
    /// `idle`: at rest, waiting for the person's next instruction.
    Idle,
    /// `working`: running on its own.
    Working,
    /// `done`: finished a piece of work that is ready to look at.
    Done,
    /// `blocked`: cannot continue until a person acts.
    Blocked,
    /// `error`: failed and stopped.
    Error,
}

/// What a blocked program needs from a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockedKind {
    /// `permission`: approval to do something.
    Permission,
    /// `question`: an answer the person types.
    Question,
    /// `auth`: a login, a token or another credential.
    Auth,
}

/// One program status record of a pane: what the newest report for one
/// record id said. Example: `state=working:id=deploy:progress=40:app=cargo`
/// gives `{ record_id: "deploy", program_status_state: Working,
/// progress_percent: Some(40), app_name: Some("cargo"), .. }`.
///
/// A message without one of the `Option` fields is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramStatusRecord {
    /// The record's path, such as `deploy/us-east`. Empty for the root record.
    pub record_id: String,
    /// The state the report named.
    pub program_status_state: ProgramStatusState,
    /// What the blocked program needs. `None` unless `program_status_state`
    /// is `Blocked`, and `None` when the report named no kind.
    #[serde(deserialize_with = "Option::deserialize")]
    pub blocked_kind: Option<BlockedKind>,
    /// 0 to 100. `None` when the work is indeterminate, and for every state
    /// but `Working` and `Blocked`.
    #[serde(deserialize_with = "Option::deserialize")]
    pub progress_percent: Option<u8>,
    /// The record's own `app`, such as `cargo`. A reader that finds `None`
    /// takes the `app_name` of the nearest ancestor record that has one.
    #[serde(deserialize_with = "Option::deserialize")]
    pub app_name: Option<String>,
    /// The decoded `title` after `sanitize_reported_text`, such as `US East`.
    #[serde(deserialize_with = "Option::deserialize")]
    pub title: Option<String>,
    /// The decoded `msg` after `sanitize_reported_text`, such as `Apply?`.
    #[serde(deserialize_with = "Option::deserialize")]
    pub message: Option<String>,
    /// Whether a person was at the pane at or after the report that wrote the
    /// record.
    pub is_seen: bool,
    /// The read time of the report that wrote the record.
    pub updated_at: SystemTime,
}

impl ProgramStatusRecord {
    /// The attention reason this record raises: `ProgramBlocked` for
    /// `Blocked`, `ProgramDone` for `Done`, `ProgramFailed` for `Error`, each
    /// with the record's id and message, and `None` for `Idle` and `Working`.
    /// Example: a `Blocked` root record with kind `Question` and message
    /// `Which branch?` gives `ProgramBlocked { record_id: "", blocked_kind:
    /// Some(Question), message: Some("Which branch?") }`.
    #[must_use]
    pub fn build_attention_reason(&self) -> Option<AttentionReason> {
        let record_id = self.record_id.clone();
        let message = self.message.clone();
        match self.program_status_state {
            ProgramStatusState::Blocked => Some(AttentionReason::ProgramBlocked {
                record_id,
                blocked_kind: self.blocked_kind,
                message,
            }),
            ProgramStatusState::Done => Some(AttentionReason::ProgramDone { record_id, message }),
            ProgramStatusState::Error => {
                Some(AttentionReason::ProgramFailed { record_id, message })
            }
            ProgramStatusState::Idle | ProgramStatusState::Working => None,
        }
    }
}

#[cfg(test)]
mod tests;
