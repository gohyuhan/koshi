//! Lifecycle state machines for the session model: the typed states a tab and
//! a session move through from creation to teardown.
//!
//! Each lifecycle is a small enum. A tab is born `Creating`, becomes `Active`
//! once its root pane is live, and winds down through `Closing` to `Closed`. A
//! session starts `Starting`, reaches `Running` on its first tab, drops to
//! `Detaching` while no client is attached, and ends `Stopping` then `Stopped`.
//! Modelling the stages as a type turns an illegal move — reviving a closed
//! tab, stopping an already-stopped session — into a transition-time error
//! instead of a silent bug.
//!
//! [`SessionLifecycle::transition`] polices the session's legal moves; the tab
//! transitions land with the operations that drive them.

use serde::{Deserialize, Serialize};

use crate::error::InvalidTransition;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabLifecycle {
    /// The tab exists but its root pane has not spawned yet.
    Creating,
    /// The tab is visible and its panes are interactive.
    Active,
    /// The tab exists in the background while the client displays a different tab.
    Inactive,
    /// The tab is shutting down; panes are being closed.
    Closing,
    /// The tab has closed and should be removed from the session.
    Closed,
}

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
    /// Apply `event`, returning the next state, or [`InvalidTransition`] if the
    /// move is illegal from the current state. `Stopped` is terminal and
    /// rejects every event. The returned `Result` must be used — the next state
    /// is the transition's only effect, so the caller assigns it back.
    pub fn transition(self, event: SessionLifecycleEvent) -> Result<Self, InvalidTransition> {
        match (self, event) {
            (SessionLifecycle::Starting, SessionLifecycleEvent::FirstTabCreated) => {
                Ok(SessionLifecycle::Running)
            }
            (SessionLifecycle::Running, SessionLifecycleEvent::LastClientDetached) => {
                Ok(SessionLifecycle::Detaching)
            }
            (SessionLifecycle::Detaching, SessionLifecycleEvent::ClientAttached) => {
                Ok(SessionLifecycle::Running)
            }
            (SessionLifecycle::Running, SessionLifecycleEvent::StopRequested) => {
                Ok(SessionLifecycle::Stopping)
            }
            (SessionLifecycle::Stopping, SessionLifecycleEvent::StopCompleted) => {
                Ok(SessionLifecycle::Stopped)
            }
            (SessionLifecycle::Detaching, SessionLifecycleEvent::StopRequested) => {
                Ok(SessionLifecycle::Stopping)
            }
            (SessionLifecycle::Starting, SessionLifecycleEvent::StopRequested) => {
                Ok(SessionLifecycle::Stopping)
            }
            _ => Err(InvalidTransition { from: self, event }),
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
    /// Shutdown was requested; session moves to `Stopping` from `Running`, `Detaching`, or `Starting`.
    StopRequested,
    /// Shutdown completed after teardown; moves `Stopping` to `Stopped`.
    StopCompleted,
}

#[cfg(test)]
mod tests;
