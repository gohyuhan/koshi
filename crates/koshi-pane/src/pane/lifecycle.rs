//! Pane lifecycle state machine: the states a pane moves through from spawn to
//! teardown.
//!
//! A pane holds one of five states.
//!
//! - `Spawning` — the pane exists. The child process has not started.
//! - `Running` — the child process is live.
//! - `Exited` — the child process ended. The state carries the exit code and
//!   the time.
//! - `Closing` — a user or a policy asked the pane to close. The state carries
//!   the request time.
//! - `Removed` — the pane is removed from the registry. This state is terminal.
//!
//! [`PaneLifecycleEvent`] drives the state one step at a time. Seven steps are
//! legal.
//!
//! - `Spawning` on `ProcessStarted` becomes `Running`.
//! - `Spawning` on `CloseRequested` becomes `Closing`.
//! - `Running` on `ProcessExited` becomes `Exited`.
//! - `Running` on `CloseRequested` becomes `Closing`.
//! - `Exited` on `CloseRequested` becomes `Closing`.
//! - `Exited` on `Respawn` becomes `Spawning`.
//! - `Closing` on `Cleaned` becomes `Removed`.
//!
//! [`PaneLifecycle::transition`] rejects every other pair.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{error::InvalidTransition, pane::state::PaneKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneLifecycle {
    /// The pane exists. The child process has not started.
    Spawning,
    /// The child process is running.
    Running,
    /// The child process ended. `code` is `None` when a signal killed the
    /// child. `code` is also `None` when the exit status was not available.
    Exited { code: Option<i32>, at: SystemTime },
    /// The pane is shutting down. `since` is the time of the close request.
    Closing { since: SystemTime },
    /// The pane is removed from the registry. This state is terminal.
    Removed,
}

impl PaneLifecycle {
    /// Applies `event` to this state and returns the next state. Returns
    /// [`InvalidTransition`] when the pair is not one of the seven legal steps.
    /// `kind` only fills in that error.
    pub fn transition(
        self,
        event: PaneLifecycleEvent,
        kind: PaneKind,
    ) -> Result<Self, InvalidTransition> {
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

            _ => Err(InvalidTransition {
                from: self,
                event,
                kind,
            }),
        }
    }
}

/// What happened to a pane. Each event drives [`PaneLifecycle`] one step
/// forward. An event carries the payload that its next state needs, or no
/// payload at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneLifecycleEvent {
    /// The child process became live.
    ProcessStarted,
    /// The child process ended. `code` is `None` when a signal killed the
    /// child. `code` is also `None` when the exit status was not available.
    ProcessExited { code: Option<i32>, at: SystemTime },
    /// A user or a policy asked the pane to close. `since` is the time of the
    /// request.
    CloseRequested { since: SystemTime },
    /// The close finished its cleanup.
    Cleaned,
    /// The `RespawnShell` policy restarts an exited pane in place. The pane
    /// returns to `Spawning` and creates a new PTY and child process.
    Respawn,
}

#[cfg(test)]
mod tests;
