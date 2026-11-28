//! Program status records in the session server: how one OSC 7501 fact
//! changes the records a pane keeps (`PaneRecord::program_status_records`).
//!
//! Example: pane 5 holds a `working` root record, and its program writes
//! `state=blocked:kind=question`. The root record becomes `blocked` with kind
//! `question`, and the fact raises
//! `ProgramBlocked { record_id: "", blocked_kind: Some(Question), message: None }`.

use std::time::SystemTime;

use koshi_core::attention::AttentionReason;
use koshi_core::program_status::{
    ProgramStatusRecord, ProgramStatusState, PROGRAM_STATUS_RECORDS_MAX,
};
use koshi_terminal::state::ProgramStatusFact;

/// Apply `program_status_fact`, read at `read_at`, to
/// `program_status_records`, a pane's records in the order they were written,
/// oldest first. Return the attention reason the fact raises, or `None`.
///
/// - `Reported` replaces the record with its id whole, or adds one. The
///   record moves to the end, takes `updated_at: read_at` and `is_seen`. A
///   new id while the pane holds [`PROGRAM_STATUS_RECORDS_MAX`] records first
///   removes the first record, the one written longest ago. The report raises
///   the record's reason
///   ([`ProgramStatusRecord::build_attention_reason`]) when the record moves
///   into `blocked`, `done` or `error` from another state or from no record,
///   and when it rewrites the record that raised `outstanding_reason`, the
///   reason the pane waits on, in the same state.
/// - `Cleared` removes the record with its id and every record beneath it:
///   `deploy` removes `deploy` and `deploy/us-east`, and keeps `deploy-old`.
///   An empty id removes every record.
/// - `PromptStarted` removes every `idle`, `working` and `blocked` record.
/// - `TerminalReset` removes every record.
///
/// Only `Reported` raises.
pub(crate) fn apply_program_status_fact(
    program_status_records: &mut Vec<ProgramStatusRecord>,
    program_status_fact: ProgramStatusFact,
    read_at: SystemTime,
    is_seen: bool,
    outstanding_reason: Option<&AttentionReason>,
) -> Option<AttentionReason> {
    match program_status_fact {
        ProgramStatusFact::Reported {
            record_id,
            program_status_state,
            blocked_kind,
            progress_percent,
            app_name,
            title,
            message,
        } => {
            let previous_record_index = program_status_records
                .iter()
                .position(|program_status_record| program_status_record.record_id == record_id);
            let previous_program_status_state = match previous_record_index {
                Some(record_index) => Some(
                    program_status_records
                        .remove(record_index)
                        .program_status_state,
                ),
                None => {
                    if program_status_records.len() >= PROGRAM_STATUS_RECORDS_MAX {
                        program_status_records.remove(0);
                    }
                    None
                }
            };
            let is_outstanding_record_rewritten = outstanding_reason
                .and_then(AttentionReason::find_raising_program_status_record)
                == Some((record_id.as_str(), program_status_state));
            let program_status_record = ProgramStatusRecord {
                record_id,
                program_status_state,
                blocked_kind,
                progress_percent,
                app_name,
                title,
                message,
                is_seen,
                updated_at: read_at,
            };
            let is_state_changed = previous_program_status_state != Some(program_status_state);
            let raised_reason = if is_state_changed || is_outstanding_record_rewritten {
                program_status_record.build_attention_reason()
            } else {
                None
            };
            program_status_records.push(program_status_record);
            raised_reason
        }
        ProgramStatusFact::Cleared {
            record_id: cleared_record_id,
        } => {
            program_status_records.retain(|program_status_record| {
                let record_id = program_status_record.record_id.as_str();
                let is_cleared_record_or_beneath = cleared_record_id.is_empty()
                    || record_id == cleared_record_id
                    || record_id
                        .strip_prefix(cleared_record_id.as_str())
                        .is_some_and(|record_id_rest| record_id_rest.starts_with('/'));
                !is_cleared_record_or_beneath
            });
            None
        }
        ProgramStatusFact::PromptStarted => {
            program_status_records.retain(|program_status_record| {
                matches!(
                    program_status_record.program_status_state,
                    ProgramStatusState::Done | ProgramStatusState::Error
                )
            });
            None
        }
        ProgramStatusFact::TerminalReset => {
            program_status_records.clear();
            None
        }
    }
}

#[cfg(test)]
mod tests;
