//! The `PtyBackend` trait and the `PtyHandle` struct that a spawned pane is driven through.
//!
//! A PTY (pseudo-terminal) is the OS-level channel a spawned shell or program
//! runs inside; it makes the program behave as if attached to a real terminal.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use koshi_core::{
    ids::PaneId,
    process::{ExitStatus, KillPolicy, PtySize, SpawnSpec},
};

use crate::error::PtyError;

/// The PTY backend: spawns children in PTYs and drives their I/O and teardown.
///
/// `Send + Sync` so one backend can be shared across the reader/writer threads
/// and the runtime. Implementors own the child processes, keyed by [`PaneId`];
/// the [`PtyHandle`] returned from [`spawn`](PtyBackend::spawn) is the read side.
pub trait PtyBackend: Send + Sync {
    /// Spawn a child in a new PTY of the given size for `pane_id`, returning a
    /// handle (addressed by that same id) that streams its output and exit
    /// status. The caller owns the pane identity; the backend keys its records
    /// by `pane_id` so later `resize`/`write`/`kill` address the same pane.
    ///
    /// `pane_id` must not already be live in the backend: spawning over a live id
    /// would orphan the previous child's PTY and I/O threads. A caller re-running
    /// a command in an existing pane (respawn) must [`kill`](PtyBackend::kill) it
    /// first. Implementations assert this in debug builds.
    fn spawn(&self, pane_id: PaneId, spec: SpawnSpec, size: PtySize)
        -> Result<PtyHandle, PtyError>;
    /// Resize an existing pane's PTY.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError>;
    /// Write bytes to a pane's child stdin.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError>;
    /// Terminate a pane's child according to `kill_policy`.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError>;
    /// The live working directory of `pane`'s child, asked from the OS
    /// (Linux `/proc/<pid>/cwd`, macOS `proc_pidinfo`). `None` when the pane
    /// has no live child or the platform has no lookup (Windows).
    fn live_cwd(&self, pane: PaneId) -> Option<PathBuf>;
}

/// The read side of one spawned pane: its id and the channels the backend
/// delivers child output and exit status on.
///
/// The channels are held as `Option`: [`take_receivers`](PtyHandle::take_receivers)
/// moves them out so a forwarder thread can block on them, after which the
/// drained handle stays live as a per-pane token (`contains_key`/`remove` still
/// address it) and the `try_*` polls return `None`. While the receivers are
/// held, the `try_*` methods poll them without blocking. The backend keeps the
/// sending ends (see [`PtyHandle::new`]); dropping the handle closes the receivers.
#[derive(Debug)]
pub struct PtyHandle {
    pane_id: PaneId,
    output: Option<Receiver<Vec<u8>>>,
    exit: Option<Receiver<ExitStatus>>,
}

impl PtyHandle {
    /// Build a handle for `pane_id`, returning it with the output and exit
    /// senders the backend retains to push child output and the final exit.
    pub fn new(pane_id: PaneId) -> (Self, Sender<Vec<u8>>, Sender<ExitStatus>) {
        let (output_sender, output_receiver) = channel();
        let (exit_sender, exit_receiver) = channel();
        let new_pty_handle = PtyHandle {
            pane_id,
            output: Some(output_receiver),
            exit: Some(exit_receiver),
        };

        (new_pty_handle, output_sender, exit_sender)
    }

    /// The pane this handle addresses.
    #[must_use]
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// Move the output and exit receivers out of the handle, transferring
    /// ownership to a forwarder thread that blocks on them. Returns `None` if
    /// they were already taken.
    pub fn take_receivers(&mut self) -> Option<(Receiver<Vec<u8>>, Receiver<ExitStatus>)> {
        let output = self.output.take()?;
        let exit = self.exit.take()?;
        Some((output, exit))
    }

    /// The next chunk of child output, or `None` if none is pending or the
    /// receivers have been taken.
    pub fn try_read_output(&self) -> Option<Vec<u8>> {
        self.output.as_ref().and_then(|rx| rx.try_recv().ok())
    }

    /// The child's exit status, or `None` if it has not exited yet or the
    /// receivers have been taken.
    pub fn try_exit_status(&self) -> Option<ExitStatus> {
        self.exit.as_ref().and_then(|rx| rx.try_recv().ok())
    }
}

#[cfg(test)]
mod tests;
