//! The lifecycle state machine for a session: the typed states it moves
//! through from creation to teardown.
//!
//! A session starts `Starting`, reaches `Running` on its first tab, drops to
//! `Detaching` while no client is attached, and ends `Stopping` then
//! `Stopped`. [`SessionLifecycle::transition`] accepts seven state-and-event
//! pairs and rejects every other.

use serde::{Deserialize, Serialize};

use crate::error::InvalidTransition;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionLifecycle {
    /// The session exists but has not created its first tab yet.
    Starting,
    /// The session is running with at least one client attached.
    Running,
    /// The session has no clients attached but may have clients reconnect.
    Detaching,
    /// The session is shutting down and will not accept new clients.
    Stopping,
    /// The session has closed and is terminal.
    Stopped,
}

impl SessionLifecycle {
    /// Apply `lifecycle_event`, returning the next state, or [`InvalidTransition`]
    /// carrying `self` and `lifecycle_event` if the move is illegal from the current
    /// state. `Stopped` is terminal and rejects every event. `self` is left as
    /// it was; the next state exists only in the returned value.
    pub fn transition(
        self,
        lifecycle_event: SessionLifecycleEvent,
    ) -> Result<Self, InvalidTransition> {
        match (self, lifecycle_event) {
            (SessionLifecycle::Starting, SessionLifecycleEvent::FirstTabCreated) => {
                Ok(SessionLifecycle::Running)
            }
            (SessionLifecycle::Running, SessionLifecycleEvent::LastClientDetached) => {
                Ok(SessionLifecycle::Detaching)
            }
            (SessionLifecycle::Detaching, SessionLifecycleEvent::ClientAttached) => {
                Ok(SessionLifecycle::Running)
            }
            (
                SessionLifecycle::Starting
                | SessionLifecycle::Running
                | SessionLifecycle::Detaching,
                SessionLifecycleEvent::StopRequested,
            ) => Ok(SessionLifecycle::Stopping),
            (SessionLifecycle::Stopping, SessionLifecycleEvent::StopCompleted) => {
                Ok(SessionLifecycle::Stopped)
            }
            _ => Err(InvalidTransition {
                previous_lifecycle: self,
                lifecycle_event,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionLifecycleEvent {
    /// The session created its first tab, transitioning from `Starting` to `Running`.
    FirstTabCreated,
    /// The last attached client disconnected; session moves to `Detaching` if `Running`.
    LastClientDetached,
    /// A client attached to a `Detaching` session, reviving it to `Running`.
    ClientAttached,
    /// Shutdown was requested; session moves to `Stopping` from `Running`, `Detaching`, or
    /// `Starting`.
    StopRequested,
    /// Shutdown completed after teardown; moves `Stopping` to `Stopped`.
    StopCompleted,
}

#[cfg(test)]
mod tests;
