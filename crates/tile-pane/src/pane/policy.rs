//! Pane close and exit policies: how a pane is asked to shut down, and what
//! becomes of it when its process ends.
//!
//! [`PaneClosePolicy`] governs a requested close — graceful with a grace
//! period, forced, or confirm-if-busy. [`PaneExitPolicy`] governs the pane's
//! fate when its child exits on its own — close it, hold the dead pane visible,
//! or respawn a shell. The enums are the vocabulary; their defaults and the
//! mapping onto the process-kill policy land with the operations that apply
//! them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneClosePolicy {
    Graceful { timeout: Duration },
    Force,
    ConfirmIfBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneExitPolicy {
    CloseOnExit,
    HoldOnExit,
    RespawnShell,
}
