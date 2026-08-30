//! The `PtyBackend` trait, the `PtyHandle` struct a spawned pane is driven
//! through, and the `CarriedPtyPane` record a pane is handed on as.
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

/// The exit status a pane reports for a child whose end nothing observed.
///
/// Reported by a `waitpid` that answers `ECHILD`, by a `portable-pty` wait that
/// fails, and for a pane the supervisor no longer holds when a new link settles
/// its pane list.
pub(crate) const UNOBSERVED_EXIT: ExitStatus = ExitStatus::ExitCode(-1);

/// The PTY backend: spawns children in PTYs and drives their I/O and teardown.
///
/// `Send + Sync`: one backend is shared across the reader/writer threads and
/// the runtime. Implementors own the child processes, keyed by [`PaneId`];
/// the [`PtyHandle`] returned from [`spawn`](PtyBackend::spawn) is the read side.
pub trait PtyBackend: Send + Sync {
    /// Spawn a child in a new PTY of the given size for `pane_id`, returning a
    /// handle (addressed by that same id) that streams its output and exit
    /// status. The caller owns the pane identity; the backend keys its records
    /// by `pane_id`, and `resize`/`write`/`kill` calls with that id address
    /// this pane.
    ///
    /// `pane_id` must not already be live in the backend; spawning over a live
    /// id orphans the previous child's PTY and I/O threads. A caller re-running
    /// a command in an existing pane must [`kill`](PtyBackend::kill) it first.
    /// An implementation either refuses the call with [`PtyError::Spawn`] or
    /// asserts in a debug build.
    fn spawn(&self, pane_id: PaneId, spec: SpawnSpec, size: PtySize)
        -> Result<PtyHandle, PtyError>;
    /// Resize an existing pane's PTY.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError>;
    /// Write bytes to a pane's child stdin.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError>;
    /// Terminate a pane's child according to `kill_policy`.
    ///
    /// After the call, no exit for the pane reaches a [`PtySink`] and its
    /// output stops being forwarded.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError>;
    /// The live working directory of `pane`'s child, asked from the OS
    /// (Linux `/proc/<pid>/cwd`, macOS `proc_pidinfo`). `None` when the pane
    /// has no live child or the platform has no lookup (Windows).
    fn live_cwd(&self, pane: PaneId) -> Option<PathBuf>;
}

/// Where a backend delivers a pane's child output and exit status.
///
/// The pane's reader thread hands each chunk to the sink itself; no relay
/// thread runs per pane. `Send + Sync`: the reader and watcher threads of
/// every pane share one sink.
pub trait PtySink: Send + Sync {
    /// Take one chunk of `pane`'s child output. Returning `false` means this
    /// consumer is done with `pane`: the reader stops reading it and nothing
    /// more is delivered for it — not even [`exit`](PtySink::exit). Every
    /// other pane keeps running.
    fn output(&self, pane: PaneId, bytes: Vec<u8>) -> bool;

    /// Take `pane`'s final exit status, delivered at most once.
    ///
    /// Called on one of the pane's own threads; which one is not fixed. The
    /// call may close the pane through [`PtyBackend::kill`]; the backend does
    /// not join the thread it is running on.
    ///
    /// It comes after the last [`output`](PtySink::output) call for that pane:
    /// a consumer sees everything the child printed before it sees the child
    /// end. On Windows the backend closes the pane's terminal once the child
    /// ends: the console flushes what it still holds, the reader drains it to
    /// its end, and the exit follows.
    ///
    /// A disowned descendant can hold a Unix terminal open after the child is
    /// gone and keep printing into it. There the exit comes once output stops
    /// arriving, or after a bounded wait if it never stops. A pane whose output
    /// resumes past that point is no longer read; nothing arrives after the
    /// exit.
    fn exit(&self, pane: PaneId, status: ExitStatus);
}

/// One live pane, as a process about to replace its own image hands it on.
///
/// The descriptor and the process id are what the next image needs to take the
/// pane back; the size is what that image must record as the window the child
/// already has; the exit is how the child ended, when this process saw it end.
///
/// Every backend answers with this record: a
/// [`PortablePtyBackend`](crate::portable::PortablePtyBackend) fills in the
/// descriptor it owns, and a
/// [`SupervisorPtyBackend`](crate::supervisor::SupervisorPtyBackend) leaves it
/// `None`, the descriptor being the supervisor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarriedPtyPane {
    /// The pane this record is for.
    pub pane_id: PaneId,
    /// The pane's own terminal descriptor. `None` for a terminal that exposes
    /// none, which no image can carry.
    #[cfg(unix)]
    pub terminal_fd: Option<std::os::fd::RawFd>,
    /// The child's process id, waited on again once the pane is taken back.
    pub pid: u32,
    /// The last size the pane's terminal was set to.
    pub size: PtySize,
    /// How the pane's child ended, if this process's watcher reaped it. `None`
    /// while the child runs, and the next image waits on the process id itself.
    pub exit: Option<ExitStatus>,
}

/// The read side of one spawned pane: its id and the channels the backend
/// delivers child output and exit status on.
///
/// The channels are held as `Option`: [`take_receivers`](PtyHandle::take_receivers)
/// moves them out for a forwarder thread to block on, after which the drained
/// handle stays live as a per-pane token (`contains_key`/`remove` still
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
        let handle = PtyHandle {
            pane_id,
            output: Some(output_receiver),
            exit: Some(exit_receiver),
        };
        (handle, output_sender, exit_sender)
    }

    /// Build a handle for `pane_id` that carries no channels, for a backend
    /// delivering that pane's output and exit through a [`PtySink`] instead.
    ///
    /// The handle stays the pane's live token — `contains_key`/`remove` still
    /// address it — while [`take_receivers`](PtyHandle::take_receivers) and
    /// both `try_*` polls return `None`.
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

    /// Move the output and exit receivers out of the handle. Returns `None` if
    /// they were already taken or the handle is [`detached`](PtyHandle::detached).
    pub fn take_receivers(&mut self) -> Option<(Receiver<Vec<u8>>, Receiver<ExitStatus>)> {
        let output = self.output.take()?;
        let exit = self.exit.take()?;
        Some((output, exit))
    }

    /// The next chunk of child output, or `None` if none is pending, the
    /// backend dropped its sender, or the receivers have been taken.
    pub fn try_read_output(&self) -> Option<Vec<u8>> {
        self.output.as_ref().and_then(|rx| rx.try_recv().ok())
    }

    /// The child's exit status, or `None` if it has not exited yet, the
    /// backend dropped its sender, or the receivers have been taken. A status
    /// is returned once; the next call answers `None`.
    pub fn try_exit_status(&self) -> Option<ExitStatus> {
        self.exit.as_ref().and_then(|rx| rx.try_recv().ok())
    }
}

#[cfg(test)]
mod tests;
