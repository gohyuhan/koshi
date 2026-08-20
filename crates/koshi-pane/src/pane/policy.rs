//! Pane close and exit policies: how a pane shuts down, and what becomes of it
//! when its process ends.
//!
//! [`PaneClosePolicy`] sets how a requested close runs. [`PaneExitPolicy`] sets
//! what happens when the child process ends on its own. Each policy has a
//! default. [`PaneClosePolicy::kill_policy`] maps a close onto the process
//! [`KillPolicy`]. The policy for an empty tab lives with the session model.

use std::time::Duration;

use koshi_core::{constant::GRACEFUL_TIMEOUT_DURATION, process::KillPolicy};
use serde::{Deserialize, Serialize};

/// How a pane carries out a requested close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneClosePolicy {
    /// Close gracefully. `timeout` is how long the process has to clean up.
    Graceful {
        #[serde(with = "koshi_core::process::duration_secs")]
        timeout: Duration,
    },
    /// Force-kill the process immediately.
    Force,
    /// Prompt the user when the pane is busy, then close gracefully.
    ConfirmIfBusy,
}

impl Default for PaneClosePolicy {
    fn default() -> Self {
        PaneClosePolicy::Graceful {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }
    }
}

impl PaneClosePolicy {
    /// Maps this close policy onto the process [`KillPolicy`] that the PTY
    /// layer applies. `Graceful` passes its own timeout through. `ConfirmIfBusy`
    /// maps to a graceful close with the default timeout.
    #[must_use]
    pub fn kill_policy(&self) -> KillPolicy {
        match self {
            PaneClosePolicy::Graceful { timeout } => KillPolicy::Graceful { timeout: *timeout },
            PaneClosePolicy::Force => KillPolicy::Force,
            PaneClosePolicy::ConfirmIfBusy => KillPolicy::Graceful {
                timeout: GRACEFUL_TIMEOUT_DURATION,
            },
        }
    }
}

/// What happens to a pane when its child process ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PaneExitPolicy {
    /// Close the pane when its child process ends.
    #[default]
    CloseOnExit,
    /// Start a new shell in the pane when the child process ends.
    RespawnShell,
}

#[cfg(test)]
mod tests;
