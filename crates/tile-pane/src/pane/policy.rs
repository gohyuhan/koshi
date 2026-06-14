//! Pane close and exit policies: how a pane is asked to shut down, and what
//! becomes of it when its process ends.
//!
//! [`PaneClosePolicy`] governs a requested close — graceful with a grace
//! period, forced, or confirm-if-busy. [`PaneExitPolicy`] governs the pane's
//! fate when its child exits on its own — close it or respawn a shell. Each
//! carries its production default, and
//! [`PaneClosePolicy::kill_policy`] maps a requested close onto the process
//! [`KillPolicy`]. The tab-level empty-tab policy lives with the session model.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tile_core::{constant::GRACEFUL_TIMEOUT_DURATION, process::KillPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneClosePolicy {
    Graceful {
        #[serde(with = "tile_core::process::duration_secs")]
        timeout: Duration,
    },
    Force,
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
    /// Map this close policy onto the process [`KillPolicy`] the PTY layer
    /// applies. `ConfirmIfBusy` resolves to a graceful close — the prompt is a
    /// UI step; once confirmed, the close proceeds gracefully.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PaneExitPolicy {
    #[default]
    CloseOnExit,
    RespawnShell,
}

#[cfg(test)]
mod tests;
