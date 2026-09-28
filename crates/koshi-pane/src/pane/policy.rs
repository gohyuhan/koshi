//! Pane close policy: how a pane shuts down.
//!
//! [`PaneClosePolicy`] sets how a requested close runs and has a default.
//! [`PaneClosePolicy::to_kill_policy`] maps a close onto the process
//! [`KillPolicy`].

use std::time::Duration;

use koshi_core::{constant::GRACEFUL_TIMEOUT_DURATION, process::KillPolicy};
use serde::{Deserialize, Serialize};

/// How a pane carries out a requested close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneClosePolicy {
    /// Close gracefully. `timeout_duration` is how long the process has to clean up.
    /// `timeout_duration` serializes as whole seconds; the sub-second part is dropped.
    Graceful {
        #[serde(with = "koshi_core::process::duration_seconds")]
        timeout_duration: Duration,
    },
    /// Force-kill the process immediately.
    Force,
    /// Reject a close unless the pane is `Exited`. An `Exited` pane closes
    /// gracefully with the default timeout.
    ConfirmIfBusy,
}

impl Default for PaneClosePolicy {
    fn default() -> Self {
        PaneClosePolicy::Graceful {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }
    }
}

impl PaneClosePolicy {
    /// Maps this close policy onto the process [`KillPolicy`] that the PTY
    /// layer applies. `Graceful` passes its own timeout through. `ConfirmIfBusy`
    /// maps to a graceful close with the default timeout.
    #[must_use]
    pub fn to_kill_policy(&self) -> KillPolicy {
        match self {
            PaneClosePolicy::Graceful { timeout_duration } => KillPolicy::Graceful {
                timeout_duration: *timeout_duration,
            },
            PaneClosePolicy::Force => KillPolicy::Force,
            PaneClosePolicy::ConfirmIfBusy => KillPolicy::Graceful {
                timeout_duration: GRACEFUL_TIMEOUT_DURATION,
            },
        }
    }
}

#[cfg(test)]
mod tests;
