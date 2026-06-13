//! Pane lifecycle state machine: the states a pane moves through from spawn to
//! teardown.
//!
//! A pane is born `Spawning`, becomes `Running` once its process is live, then
//! ends one of two ways — its child `Exited` (carrying the exit code, or none
//! on signal-kill, and when), or a requested `Closing` (carrying since-when) —
//! before it is finally
//! `Removed` from the registry. Modelling the stages as a type keeps an illegal
//! move — reviving a removed pane, running one mid-teardown — a transition-time
//! error instead of a silent bug.
//!
//! The enum lives here; the transition function that polices the legal moves
//! lands with the operation that drives it.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

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
