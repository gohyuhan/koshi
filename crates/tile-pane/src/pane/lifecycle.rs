//! Pane lifecycle state machine: the states a pane moves through from spawn to
//! teardown.
//!
//! A pane is born `Spawning`, becomes `Running` once its process is live, then
//! ends — its child `Exited` (carrying the exit code, or none on signal-kill,
//! and when), or a requested `Closing` (carrying since-when, and askable from
//! any live stage, even before the child runs) — before it is finally
//! `Removed` from the registry. A dead `Exited` pane may instead respawn (the
//! `RespawnShell` policy), looping back to `Spawning` in place; only `Removed`
//! is terminal. Modelling the stages as a type keeps an
//! illegal move — reviving a removed pane, running one mid-teardown — a
//! transition-time error instead of a silent bug.
//!
//! The enum and its `transition` function — which rejects every move outside
//! the legal set — both live here, driven one step per [`PaneLifecycleEvent`].

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::InvalidTransition;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneLifecycle {
    Spawning,
    Running,
    /// The child exited. `code` is `None` when the pane was signal-killed or
    /// its status was unavailable, mirroring `PaneRecord::exit_code` and the
    /// `PaneProcessExited` event.
    Exited {
        code: Option<i32>,
        at: SystemTime,
    },
    Closing {
        since: SystemTime,
    },
    Removed,
}

impl PaneLifecycle {
    pub fn transition(self, event: PaneLifecycleEvent) -> Result<Self, InvalidTransition> {
        match (self, event) {
            (PaneLifecycle::Spawning, PaneLifecycleEvent::ProcessStarted) => {
                Ok(PaneLifecycle::Running)
            }
            (PaneLifecycle::Spawning, PaneLifecycleEvent::CloseRequested { since }) => {
                Ok(PaneLifecycle::Closing { since })
            }
            (PaneLifecycle::Running, PaneLifecycleEvent::ProcessExited { code, at }) => {
                Ok(PaneLifecycle::Exited { code, at })
            }
            (PaneLifecycle::Running, PaneLifecycleEvent::CloseRequested { since }) => {
                Ok(PaneLifecycle::Closing { since })
            }
            (PaneLifecycle::Exited { .. }, PaneLifecycleEvent::CloseRequested { since }) => {
                Ok(PaneLifecycle::Closing { since })
            }
            (PaneLifecycle::Closing { .. }, PaneLifecycleEvent::Cleaned) => {
                Ok(PaneLifecycle::Removed)
            }
            (PaneLifecycle::Exited { .. }, PaneLifecycleEvent::Respawn) => {
                Ok(PaneLifecycle::Spawning)
            }

            _ => Err(InvalidTransition { from: self, event }),
        }
    }
}

/// What happened to a pane, driving its [`PaneLifecycle`] forward. An event
/// carries any payload its target state needs — an exit's code and time, a
/// close's start time — and is otherwise a bare signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneLifecycleEvent {
    /// The child process became live.
    ProcessStarted,
    /// The child process ended. `code` is `None` when it was signal-killed or
    /// its status was unavailable.
    ProcessExited { code: Option<i32>, at: SystemTime },
    /// A user or policy asked the pane to close.
    CloseRequested { since: SystemTime },
    /// The close transaction finished its cleanup.
    Cleaned,
    /// A policy (`RespawnShell`) restarts a dead pane in place, looping it back
    /// to `Spawning` to recreate the PTY and child.
    Respawn,
}

#[cfg(test)]
mod tests;
