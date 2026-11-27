//! Pane attention: what a pane's program last asked a person to look at,
//! and since when.
//!
//! A pane holds at most one [`Attention`]. A bell, a desktop notification, or
//! a finished command that ran for [`MIN_ATTENTION_COMMAND_SECONDS`] or more
//! sets it while no person watches the pane, and the first visit clears it.
//! Example: `cargo test` fails with exit code `101` after 12 seconds in a hidden
//! pane, and the pane holds
//! `AttentionReason::CommandFinished { exit_code: Some(101), elapsed_secs: Some(12) }`
//! until a client focuses it.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

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
}

/// The kind of an [`AttentionReason`], with no title, no body and no exit
/// code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttentionReasonKind {
    /// [`AttentionReason::Bell`].
    Bell,
    /// [`AttentionReason::Notify`].
    Notify,
    /// [`AttentionReason::CommandFinished`].
    CommandFinished,
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
        }
    }
}

#[cfg(test)]
mod tests;
