//! Pane attention: what a pane's program last asked a person to look at,
//! and since when.
//!
//! A pane holds at most one [`Attention`]. A bell, a desktop notification, a
//! finished command that ran for [`MIN_ATTENTION_COMMAND_SECONDS`] or more, or
//! a program status record that moves into `blocked`, `done` or `error` sets
//! it while no person watches the pane, and the first visit clears it.
//! Example: `cargo test` fails with exit code `101` after 12 seconds in a hidden
//! pane, and the pane holds
//! `AttentionReason::CommandFinished { exit_code: Some(101), elapsed_secs: Some(12) }`
//! until a client focuses it.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::notification::NotificationSeverity;
use crate::program_status::{BlockedKind, ProgramStatusRecord, ProgramStatusState};

/// The fewest whole seconds a command runs before its finish raises
/// attention: `5`. A command that finishes after 4 seconds raises nothing.
pub const MIN_ATTENTION_COMMAND_SECONDS: u32 = 5;

/// A pane's outstanding request for a person, with the moment it was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    /// The read time of the first signal since the last clear. Each signal
    /// after it keeps it.
    pub raised_at: SystemTime,
    /// What the program sent most recently.
    pub reason: AttentionReason,
    /// The OSC 99 id of the notification that set `reason`, when an OSC 99
    /// notification with an id set it. `OSC 99 ; i=<id> : p=close` for this id
    /// clears the attention. `None` for every other signal. A record without
    /// this field is refused.
    #[serde(deserialize_with = "Option::deserialize")]
    pub withdrawable_notification_id: Option<String>,
}

/// Why a pane wants a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionReason {
    /// A standalone BEL (`0x07`) that did not end an OSC sequence.
    Bell,
    /// A desktop notification request: OSC 9, OSC 777, or OSC 99.
    Notify {
        /// The notification title. `None` for OSC 9, which sends one string,
        /// and for a title that is empty. A message without `title` is
        /// refused.
        #[serde(deserialize_with = "Option::deserialize")]
        title: Option<String>,
        /// The notification text. Empty when the program sent none.
        body: String,
    },
    /// `OSC 133 ; D`: a command finished.
    CommandFinished {
        /// The exit code the sequence carried. `None` when it carried no
        /// code. A message without `exit_code` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        exit_code: Option<i32>,
        /// The whole seconds from the command's `OSC 133 ; C` to this `D`.
        /// `None` when the session server holds no start time for the
        /// command: its `C` arrived before the server's last image swap. A
        /// message without `elapsed_secs` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        elapsed_secs: Option<u32>,
    },
    /// `OSC 7501`: a program status record moved into `blocked`. The program
    /// cannot continue until a person acts.
    ProgramBlocked {
        /// The record's id. Empty for the root record.
        record_id: String,
        /// What the program needs, or `None` when the report named no kind.
        /// A message without `blocked_kind` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        blocked_kind: Option<BlockedKind>,
        /// The record's message. A message without `message` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        message: Option<String>,
    },
    /// `OSC 7501`: a program status record moved into `done`. Work finished
    /// that nobody looked at yet.
    ProgramDone {
        /// The record's id. Empty for the root record.
        record_id: String,
        /// The record's message. A message without `message` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        message: Option<String>,
    },
    /// `OSC 7501`: a program status record moved into `error`. The program
    /// failed and stopped.
    ProgramFailed {
        /// The record's id. Empty for the root record.
        record_id: String,
        /// The record's message. A message without `message` is refused.
        #[serde(deserialize_with = "Option::deserialize")]
        message: Option<String>,
    },
}

/// The kind of an [`AttentionReason`], with no title, no body, no exit code,
/// no record id and no message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttentionReasonKind {
    /// [`AttentionReason::Bell`].
    Bell,
    /// [`AttentionReason::Notify`].
    Notify,
    /// [`AttentionReason::CommandFinished`].
    CommandFinished,
    /// [`AttentionReason::ProgramBlocked`].
    ProgramBlocked,
    /// [`AttentionReason::ProgramDone`].
    ProgramDone,
    /// [`AttentionReason::ProgramFailed`].
    ProgramFailed,
}

impl AttentionReason {
    /// The kind of this reason. `Notify { title: Some("Build"), body: "done" }`
    /// gives [`AttentionReasonKind::Notify`].
    #[must_use]
    pub fn get_reason_kind(&self) -> AttentionReasonKind {
        match self {
            AttentionReason::Bell => AttentionReasonKind::Bell,
            AttentionReason::Notify { .. } => AttentionReasonKind::Notify,
            AttentionReason::CommandFinished { .. } => AttentionReasonKind::CommandFinished,
            AttentionReason::ProgramBlocked { .. } => AttentionReasonKind::ProgramBlocked,
            AttentionReason::ProgramDone { .. } => AttentionReasonKind::ProgramDone,
            AttentionReason::ProgramFailed { .. } => AttentionReasonKind::ProgramFailed,
        }
    }

    /// The id of the program status record that raised this reason, with the
    /// state that raised it, or `None` for a bell, a notification and a
    /// finished command. `ProgramDone { record_id: "deploy", message: None }`
    /// gives `Some(("deploy", ProgramStatusState::Done))`.
    #[must_use]
    pub fn find_raising_program_status_record(&self) -> Option<(&str, ProgramStatusState)> {
        match self {
            AttentionReason::ProgramBlocked { record_id, .. } => {
                Some((record_id, ProgramStatusState::Blocked))
            }
            AttentionReason::ProgramDone { record_id, .. } => {
                Some((record_id, ProgramStatusState::Done))
            }
            AttentionReason::ProgramFailed { record_id, .. } => {
                Some((record_id, ProgramStatusState::Error))
            }
            AttentionReason::Bell
            | AttentionReason::Notify { .. }
            | AttentionReason::CommandFinished { .. } => None,
        }
    }

    /// The severity and the text of the notification entry this reason
    /// posts: the `<what>` of the entry text `<place> › <pane> · <what>`.
    /// `None` for [`AttentionReason::Bell`], which posts no entry.
    /// `program_status_records` are the records of the pane that waits.
    ///
    /// | Reason | Severity | Text |
    /// | --- | --- | --- |
    /// | `Notify` | `Info` | `<title>: <body>`, `<title>` for an empty body, `<body>` with no title |
    /// | `CommandFinished`, exit code `0` or none | `Info` | `finished after <elapsed>` |
    /// | `CommandFinished`, another exit code | `Warning` | `failed — exit <code> after <elapsed>` |
    /// | `ProgramBlocked` | `Warning` | `<label>: blocked (<kind>) — <message>` |
    /// | `ProgramDone` | `Info` | `<label>: done — <message>` |
    /// | `ProgramFailed` | `Error` | `<label>: failed — <message>` |
    ///
    /// ` after <elapsed>` drops out when `elapsed_secs` is `None`, and
    /// `<elapsed>` reads `45s` under one minute, `2m 14s` under one hour, and
    /// `1h 3m` from one hour. ` (<kind>)` drops out when the report named no
    /// kind, and ` — <message>` when the reason holds no message. `<label>` is the record's app name, or the
    /// app name of its nearest ancestor record that holds one, then ` / ` and
    /// the record's title when the record holds one. A record with no app name
    /// and no title gives no `<label>: `.
    ///
    /// Example: `CommandFinished { exit_code: Some(101), elapsed_secs: Some(134) }`
    /// gives `(Warning, "failed — exit 101 after 2m 14s")`. `ProgramBlocked`
    /// for the record `deploy/eu-west` titled `EU West`, under the record
    /// `deploy` with the app name `deploy`, with the kind `Permission` and
    /// the message `Apply?`, gives
    /// `(Warning, "deploy / EU West: blocked (permission) — Apply?")`.
    #[must_use]
    pub fn format_notification_entry(
        &self,
        program_status_records: &[ProgramStatusRecord],
    ) -> Option<(NotificationSeverity, String)> {
        match self {
            AttentionReason::Bell => None,
            AttentionReason::Notify { title, body } => {
                let notification_text = match title {
                    Some(title) if body.is_empty() => title.clone(),
                    Some(title) => format!("{title}: {body}"),
                    None => body.clone(),
                };
                Some((NotificationSeverity::Info, notification_text))
            }
            AttentionReason::CommandFinished {
                exit_code,
                elapsed_secs,
            } => {
                let elapsed_text = match elapsed_secs {
                    Some(elapsed_secs) => {
                        format!(" after {}", format_elapsed_seconds(*elapsed_secs))
                    }
                    None => String::new(),
                };
                match exit_code {
                    None | Some(0) => Some((
                        NotificationSeverity::Info,
                        format!("finished{elapsed_text}"),
                    )),
                    Some(exit_code) => Some((
                        NotificationSeverity::Warning,
                        format!("failed — exit {exit_code}{elapsed_text}"),
                    )),
                }
            }
            AttentionReason::ProgramBlocked {
                record_id,
                blocked_kind,
                message,
            } => {
                let state_text = match blocked_kind {
                    Some(blocked_kind) => {
                        format!("blocked ({})", blocked_kind.format_protocol_word())
                    }
                    None => "blocked".to_owned(),
                };
                Some((
                    NotificationSeverity::Warning,
                    format_program_status_text(
                        program_status_records,
                        record_id,
                        &state_text,
                        message.as_deref(),
                    ),
                ))
            }
            AttentionReason::ProgramDone { record_id, message } => Some((
                NotificationSeverity::Info,
                format_program_status_text(
                    program_status_records,
                    record_id,
                    "done",
                    message.as_deref(),
                ),
            )),
            AttentionReason::ProgramFailed { record_id, message } => Some((
                NotificationSeverity::Error,
                format_program_status_text(
                    program_status_records,
                    record_id,
                    "failed",
                    message.as_deref(),
                ),
            )),
        }
    }
}

/// `elapsed_secs` as a notification reads it: `45s` under one minute,
/// `2m 14s` under one hour, and `1h 3m` from one hour, in whole units.
/// `60` gives `1m 0s` and `3600` gives `1h 0m`.
fn format_elapsed_seconds(elapsed_secs: u32) -> String {
    let minute_count = elapsed_secs / 60;
    let hour_count = minute_count / 60;
    if hour_count > 0 {
        return format!("{hour_count}h {}m", minute_count % 60);
    }
    if minute_count > 0 {
        return format!("{minute_count}m {}s", elapsed_secs % 60);
    }
    format!("{elapsed_secs}s")
}

/// `<label>: <state_text> — <message>` for the program status record
/// `record_id` of `program_status_records`, as
/// [`AttentionReason::format_notification_entry`] states. The app name is
/// the record's own, else that of the nearest ancestor record that holds one:
/// `deploy/eu-west`, then `deploy`, then the root record `""`.
fn format_program_status_text(
    program_status_records: &[ProgramStatusRecord],
    record_id: &str,
    state_text: &str,
    message: Option<&str>,
) -> String {
    let find_record = |searched_record_id: &str| {
        program_status_records
            .iter()
            .find(|program_status_record| program_status_record.record_id == searched_record_id)
    };
    let record_title = find_record(record_id)
        .and_then(|program_status_record| program_status_record.title.as_deref());
    let mut ancestor_record_id = record_id;
    let app_name = loop {
        let ancestor_app_name = find_record(ancestor_record_id)
            .and_then(|program_status_record| program_status_record.app_name.as_deref());
        if ancestor_app_name.is_some() || ancestor_record_id.is_empty() {
            break ancestor_app_name;
        }
        ancestor_record_id = match ancestor_record_id.rsplit_once('/') {
            Some((parent_record_id, _)) => parent_record_id,
            None => "",
        };
    };
    let label = match (app_name, record_title) {
        (Some(app_name), Some(record_title)) => Some(format!("{app_name} / {record_title}")),
        (Some(app_name), None) => Some(app_name.to_owned()),
        (None, Some(record_title)) => Some(record_title.to_owned()),
        (None, None) => None,
    };
    let mut program_status_text = String::new();
    if let Some(label) = label {
        program_status_text.push_str(&label);
        program_status_text.push_str(": ");
    }
    program_status_text.push_str(state_text);
    if let Some(message) = message {
        program_status_text.push_str(" — ");
        program_status_text.push_str(message);
    }
    program_status_text
}

#[cfg(test)]
mod tests;
