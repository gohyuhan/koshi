//! The `PtyBackend` trait, the `PtySink` a backend delivers each pane's output
//! and exit to, and the `CarriedPtyPane` record a pane is handed on as.
//!
//! A PTY (pseudo-terminal) is the OS-level channel a spawned shell or program
//! runs inside; it makes the program behave as if attached to a real terminal.

use std::path::PathBuf;

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
pub const UNOBSERVED_EXIT: ExitStatus = ExitStatus::ExitCode(-1);

/// The PTY backend: spawns children in PTYs and drives their I/O and teardown.
///
/// `Send + Sync`: one backend is shared across the reader/writer threads and
/// the runtime. Implementors own the child processes, keyed by [`PaneId`], and
/// deliver each pane's child output and exit to the [`PtySink`] they were
/// built with.
pub trait PtyBackend: Send + Sync {
    /// Spawn a child in a new PTY of the given size for `pane_id`. The child's
    /// output and exit status go to the backend's [`PtySink`]. The caller owns the pane identity; the backend keys its records
    /// by `pane_id`, and `resize_pane`/`write_pane_input`/`kill_pane` calls with that id address
    /// this pane.
    ///
    /// `pane_id` must not already be live in the backend; spawning over a live
    /// id orphans the previous child's PTY and I/O threads. A caller re-running
    /// a command in an existing pane must [`kill_pane`](PtyBackend::kill_pane) it first.
    /// An implementation either refuses the call with [`PtyError::Spawn`] or
    /// asserts in a debug build.
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<(), PtyError>;
    /// Resize an existing pane's PTY.
    fn resize_pane(&self, pane_id: PaneId, pty_size: PtySize) -> Result<(), PtyError>;
    /// Write bytes to a pane's child stdin.
    fn write_pane_input(&self, pane_id: PaneId, input_bytes: &[u8]) -> Result<(), PtyError>;
    /// Terminate a pane's child according to `kill_policy`.
    ///
    /// After the call, no exit for the pane reaches a [`PtySink`] and its
    /// output stops being forwarded.
    fn kill_pane(&self, pane_id: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError>;
    /// The live working directory of `pane_id`'s child, asked from the OS
    /// (Linux `/proc/<pid>/cwd`, macOS `proc_pidinfo`). `None` when the pane
    /// has no live child or the platform has no lookup (Windows).
    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<PathBuf>;
}

/// Where a backend delivers a pane's child output and exit status.
///
/// The pane's reader thread hands each chunk to the sink itself. `Send +
/// Sync`: the reader and watcher threads of every pane share one sink.
pub trait PtySink: Send + Sync {
    /// Take one chunk of `pane_id`'s child output. Returning `false` means this
    /// consumer is done with `pane_id`: the reader stops reading it and nothing
    /// more is delivered for it — not even [`accept_exit_status`](PtySink::accept_exit_status). Every
    /// other pane keeps running.
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool;

    /// Take `pane_id`'s final exit status, delivered at most once.
    ///
    /// Called on one of the pane's own threads; which one is not fixed. The
    /// call may close the pane through [`PtyBackend::kill_pane`]; the backend does
    /// not join the thread it is running on.
    ///
    /// It comes after the last [`accept_output_bytes`](PtySink::accept_output_bytes) call for that pane:
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
    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus);
}

/// One live pane, as a process about to replace its own image hands it on.
///
/// The descriptor and the process id are what the next image needs to take the
/// pane back; the PTY size is what that image must record as the window the
/// child already has; the exit status is how the child ended, when this
/// process saw it end.
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
    pub process_id: u32,
    /// The last size the pane's terminal was set to.
    pub pty_size: PtySize,
    /// How the pane's child ended, if this process's watcher reaped it. `None`
    /// while the child runs, and the next image waits on the process id itself.
    pub exit_status: Option<ExitStatus>,
}
