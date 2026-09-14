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
//! [`PaneLifecycleEvent`] drives the state one step at a time. Six steps are
//! legal.
//!
//! - `Spawning` on `ProcessStarted` becomes `Running`.
//! - `Spawning` on `CloseRequested` becomes `Closing`.
//! - `Running` on `ProcessExited` becomes `Exited`.
//! - `Running` on `CloseRequested` becomes `Closing`.
//! - `Exited` on `CloseRequested` becomes `Closing`.
//! - `Closing` on `Cleaned` becomes `Removed`.
//!
//! `PaneLifecycle::transition` rejects every other pair.
//! [`PaneRecord::update_lifecycle`] is the only way to apply a step to a pane.
//!
//! [`PaneRecord::update_lifecycle`]: crate::pane::state::PaneRecord::update_lifecycle

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{error::InvalidTransitionError, pane::state::PaneKind};

/// Where a pane sits between spawn and removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneLifecycle {
    /// The pane exists. The child process has not started.
    Spawning,
    /// The child process is running.
    Running,
    /// The child process ended at `exited_at`. `exit_code` is `None` when a signal killed
    /// the child or when no exit status was available.
    Exited {
        #[serde(rename = "code")]
        exit_code: Option<i32>,
        #[serde(rename = "at")]
        exited_at: SystemTime,
    },
    /// The pane is shutting down. `close_requested_at` is the time of the close request.
    Closing {
        #[serde(rename = "since")]
        close_requested_at: SystemTime,
    },
    /// The pane is removed from the registry. This state is terminal.
    Removed,
}

impl PaneLifecycle {
    /// Applies `lifecycle_event` to this state and returns the next state. Returns
    /// [`InvalidTransition`] when the pair is not one of the six legal steps.
    /// `pane_kind` fills in that error.
    pub(crate) fn transition(
        self,
        lifecycle_event: PaneLifecycleEvent,
        pane_kind: PaneKind,
    ) -> Result<Self, InvalidTransitionError> {
        match (self, lifecycle_event) {
            (PaneLifecycle::Spawning, PaneLifecycleEvent::ProcessStarted) => {
                Ok(PaneLifecycle::Running)
            }
            (
                PaneLifecycle::Spawning | PaneLifecycle::Running | PaneLifecycle::Exited { .. },
                PaneLifecycleEvent::CloseRequested { close_requested_at },
            ) => Ok(PaneLifecycle::Closing { close_requested_at }),
            (
                PaneLifecycle::Running,
                PaneLifecycleEvent::ProcessExited {
                    exit_code,
                    exited_at,
                },
            ) => Ok(PaneLifecycle::Exited {
                exit_code,
                exited_at,
            }),
            (PaneLifecycle::Closing { .. }, PaneLifecycleEvent::Cleaned) => {
                Ok(PaneLifecycle::Removed)
            }

            _ => Err(InvalidTransitionError {
                previous_lifecycle: self,
                lifecycle_event,
                pane_kind,
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
    /// The child process ended at `exited_at`. `exit_code` is `None` when a signal killed
    /// the child or when no exit status was available.
    ProcessExited {
        #[serde(rename = "code")]
        exit_code: Option<i32>,
        #[serde(rename = "at")]
        exited_at: SystemTime,
    },
    /// A user or a policy asked the pane to close. `close_requested_at` is the
    /// time of the request.
    CloseRequested {
        #[serde(rename = "since")]
        close_requested_at: SystemTime,
    },
    /// The close finished its cleanup.
    Cleaned,
}

#[cfg(test)]
mod tests;
