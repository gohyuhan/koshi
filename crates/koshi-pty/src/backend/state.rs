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
    /// `pane_id` must not already be live in the backend; spawning over a live
    /// id orphans the previous child's PTY and I/O threads. A caller re-running
    /// a command in an existing pane must [`kill`](PtyBackend::kill) it first.
    /// Implementations assert this in debug builds.
    fn spawn(&self, pane_id: PaneId, spec: SpawnSpec, size: PtySize)
        -> Result<PtyHandle, PtyError>;
    /// Resize an existing pane's PTY.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError>;
    /// Write bytes to a pane's child stdin.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError>;
    /// Terminate a pane's child according to `kill_policy`.
    ///
    /// The caller is closing the pane, so no exit for it reaches a
    /// [`PtySink`] afterwards and its output stops being forwarded.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError>;
    /// The live working directory of `pane`'s child, asked from the OS
    /// (Linux `/proc/<pid>/cwd`, macOS `proc_pidinfo`). `None` when the pane
    /// has no live child or the platform has no lookup (Windows).
    fn live_cwd(&self, pane: PaneId) -> Option<PathBuf>;
}

/// Where a backend delivers a pane's child output and exit status.
///
/// A consumer implementing this trait is handed each chunk by the reader thread
/// itself, so a pane needs no relay thread — unlike the channel-and-handle route
/// in [`PtyHandle`], where one thread per pane moves chunks onto the consumer's
/// queue. `Send + Sync`: the reader and watcher threads of every pane share
/// one sink.
pub trait PtySink: Send + Sync {
    /// Take one chunk of `pane`'s child output. Returning `false` means this
    /// consumer is done with `pane`: the reader stops reading it and nothing
    /// more is delivered for it — not even [`exit`](PtySink::exit). Every
    /// other pane keeps running.
    fn output(&self, pane: PaneId, bytes: Vec<u8>) -> bool;

    /// Take `pane`'s final exit status, delivered at most once.
    ///
    /// Called on one of the pane's own threads, and which one is not fixed —
    /// so this may close the pane through
    /// [`PtyBackend::kill`] from inside the call, and the
    /// backend will not wait on the thread it is already running.
    ///
    /// It comes after the last [`output`](PtySink::output) call for that pane,
    /// so a consumer sees everything the child printed before it sees the child
    /// end. On Windows that ordering comes from the backend closing the pane's
    /// terminal once the child ends: the console flushes what it still holds,
    /// the reader drains it to its end, and the exit follows.
    ///
    /// A disowned descendant can hold a Unix terminal open after the child is
    /// gone and keep printing into it. There the exit comes once output stops
    /// arriving, or after a bounded wait if it never stops. A pane whose output
    /// resumes past that point is no longer read, so nothing arrives after the
    /// exit either way.
    fn exit(&self, pane: PaneId, status: ExitStatus);
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

    /// Build a handle for `pane_id` that carries no channels, for a backend
    /// delivering that pane's output and exit through a [`PtySink`] instead.
    ///
    /// The handle stays the pane's live token — `contains_key`/`remove` still
    /// address it — while [`take_receivers`](PtyHandle::take_receivers) and
    /// both `try_*` polls return `None`, so a caller that would otherwise
    /// start a relay thread for the pane starts none.
    #[must_use]
    pub fn detached(pane_id: PaneId) -> Self {
        PtyHandle {
            pane_id,
            output: None,
            exit: None,
        }
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
