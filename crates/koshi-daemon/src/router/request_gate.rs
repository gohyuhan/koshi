//! The gate between the router's connections and its dispatcher: which
//! connections the router serves, and what happens to a request read off one.
//!
//! The accept thread admits each connection through the gate, and the
//! connection counts as open until its serving thread ends. A serving thread
//! hands each request to the dispatcher under the gate's lock, and the intake
//! changes under that same lock. A request is handed over before a change, or
//! meets the intake the change set.
//!
//! | Intake       | A new connection             | A request read off an admitted connection |
//! |--------------|------------------------------|-------------------------------------------|
//! | `Open`       | admitted                     | handed to the dispatcher                  |
//! | `Restarting` | admitted                     | refused with `ROUTER_RESTARTING_MESSAGE`  |
//! | `Closed`     | closed with nothing written  | its connection closes with no answer      |
//!
//! While the intake is `Restarting`, a `Restart` request is answered
//! `Restarting`.
//!
//! Example: the dispatcher's idle window passes with no connection open. It
//! closes the intake, and a caller that connects next reads the end of the
//! stream before any answer to its Hello.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use koshi_ipc::router::{RouterRequestKind, RouterResult, ROUTER_RESTARTING_MESSAGE};

use super::{build_refused_result, RouterEvent};

#[cfg(test)]
mod tests;

/// What the router lets through to its dispatcher, and how many of the
/// connections it admitted are still open. Shared by the accept thread, every
/// serving thread, and the dispatcher.
#[derive(Debug, Default)]
pub(super) struct RouterRequestGate {
    /// The intake and the count of open connections, under one lock.
    gate_state: Mutex<RouterGateState>,
    /// Notified each time an admitted connection ends.
    connection_ended: Condvar,
}

/// What a [`RouterRequestGate`] holds.
#[derive(Debug, Default)]
struct RouterGateState {
    /// What happens to a new connection and to a request read off an admitted
    /// one.
    intake: RouterIntake,
    /// How many admitted connections a serving thread still holds.
    open_connection_count: usize,
}

/// What a [`RouterRequestGate`] lets through, as the module's table states.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum RouterIntake {
    /// Every connection is admitted, and every request reaches the dispatcher.
    #[default]
    Open,
    /// Every connection is admitted, and no request reaches the dispatcher.
    Restarting,
    /// No connection is admitted, and no request is answered.
    Closed,
}

impl RouterRequestGate {
    /// Admit one accepted connection: count it as open, and hand back the
    /// entry its serving thread holds until it ends. `None` while the intake
    /// is [`RouterIntake::Closed`]: the connection is not served.
    pub(super) fn admit_connection(self: &Arc<Self>) -> Option<AdmittedRouterConnection> {
        let mut gate_state = self.lock_gate_state();
        if gate_state.intake == RouterIntake::Closed {
            return None;
        }
        gate_state.open_connection_count += 1;
        Some(AdmittedRouterConnection {
            router_request_gate: Arc::clone(self),
        })
    }

    /// Refuse every request read from here on, as [`RouterIntake::Restarting`]
    /// states. Connections are still admitted.
    pub(super) fn start_refusing_requests(&self) {
        self.lock_gate_state().intake = RouterIntake::Restarting;
    }

    /// Close the intake and give `true` when no admitted connection is open.
    /// With one open, change nothing and give `false`.
    pub(super) fn close_if_no_connection_is_open(&self) -> bool {
        let mut gate_state = self.lock_gate_state();
        if gate_state.open_connection_count > 0 {
            return false;
        }
        gate_state.intake = RouterIntake::Closed;
        true
    }

    /// Wait until no admitted connection is open or `drain_deadline` passes,
    /// then close the intake. Gives how many admitted connections are still
    /// open at the close.
    pub(super) fn close_once_connections_end(&self, drain_deadline: Instant) -> usize {
        let mut gate_state = self.lock_gate_state();
        while gate_state.open_connection_count > 0 {
            let remaining_drain_duration = drain_deadline.saturating_duration_since(Instant::now());
            if remaining_drain_duration.is_zero() {
                break;
            }
            gate_state = self
                .connection_ended
                .wait_timeout(gate_state, remaining_drain_duration)
                .expect("router request gate")
                .0;
        }
        gate_state.intake = RouterIntake::Closed;
        gate_state.open_connection_count
    }

    /// Open the intake again: connections are admitted, and requests reach
    /// the dispatcher.
    pub(super) fn reopen_intake(&self) {
        self.lock_gate_state().intake = RouterIntake::Open;
    }

    /// Lock the gate's state.
    ///
    /// # Panics
    /// When a thread panicked while it held the lock.
    fn lock_gate_state(&self) -> MutexGuard<'_, RouterGateState> {
        self.gate_state.lock().expect("router request gate")
    }
}

/// One admitted connection, counted open in its [`RouterRequestGate`] while
/// its serving thread holds this. Dropping it counts the connection as ended
/// and wakes [`RouterRequestGate::close_once_connections_end`].
#[derive(Debug)]
pub(super) struct AdmittedRouterConnection {
    /// The gate that admitted the connection.
    router_request_gate: Arc<RouterRequestGate>,
}

impl AdmittedRouterConnection {
    /// Ask the dispatcher for `request_kind` over `router_events_sender`, and
    /// wait for its answer.
    ///
    /// - [`RouterIntake::Open`]: the request goes to the dispatcher under the
    ///   gate's lock, and the answer is the one the dispatcher sends back.
    /// - [`RouterIntake::Restarting`]: a `Restart` is answered
    ///   [`RouterResult::Restarting`] and every other request is refused with
    ///   [`ROUTER_RESTARTING_MESSAGE`], with no dispatcher asked.
    /// - `None` means no answer: the intake is [`RouterIntake::Closed`], or
    ///   the dispatcher is gone.
    pub(super) fn ask_dispatcher(
        &self,
        router_events_sender: &Sender<RouterEvent>,
        request_kind: RouterRequestKind,
    ) -> Option<RouterResult> {
        let (response_sender, response_receiver) = mpsc::channel();
        {
            let gate_state = self.router_request_gate.lock_gate_state();
            match gate_state.intake {
                RouterIntake::Open => {}
                RouterIntake::Restarting => {
                    if request_kind == RouterRequestKind::Restart {
                        return Some(RouterResult::Restarting);
                    }
                    return Some(build_refused_result(ROUTER_RESTARTING_MESSAGE.to_string()));
                }
                RouterIntake::Closed => return None,
            }
            let request_event = RouterEvent::Request {
                request_kind,
                response_sender,
            };
            if router_events_sender.send(request_event).is_err() {
                return None;
            }
        }
        response_receiver.recv().ok()
    }
}

impl Drop for AdmittedRouterConnection {
    fn drop(&mut self) {
        let mut gate_state = self.router_request_gate.lock_gate_state();
        gate_state.open_connection_count -= 1;
        self.router_request_gate.connection_ended.notify_all();
    }
}
