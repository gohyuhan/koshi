//! The [`PtyBackend`] trait and the [`PtyHandle`] a spawned pane is driven through.

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use tile_core::{
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
    /// Spawn a child in a new PTY of the given size, returning a handle that
    /// streams its output and exit status.
    fn spawn(&self, spec: SpawnSpec, size: PtySize) -> Result<PtyHandle, PtyError>;
    /// Resize an existing pane's PTY.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError>;
    /// Write bytes to a pane's child stdin.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError>;
    /// Terminate a pane's child according to `kill_policy`.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError>;
}

/// The read side of one spawned pane: its id and the async channels the backend
/// delivers child output and exit status on.
///
/// PTY I/O is blocking, so the backend drives it on dedicated OS threads (a
/// reader pumping child output, a watcher awaiting child exit) that hand their
/// results across these `tokio::sync::mpsc` channels. The runtime owns the handle
/// and `await`s [`recv_output`](PtyHandle::recv_output) /
/// [`recv_exit`](PtyHandle::recv_exit) on its event loop, so it never blocks a
/// task on PTY I/O. [`try_read_output`](PtyHandle::try_read_output) /
/// [`try_exit_status`](PtyHandle::try_exit_status) are non-blocking polls for
/// callers not driving an async loop. Dropping the handle closes the receivers,
/// which is how the backend threads learn the caller is gone.
#[derive(Debug)]
pub struct PtyHandle {
    pane_id: PaneId,
    output: UnboundedReceiver<Vec<u8>>,
    exit: UnboundedReceiver<ExitStatus>,
}

impl PtyHandle {
    /// Build a handle for `pane_id`, returning it with the output and exit
    /// senders the backend retains to push child output and the final exit.
    pub fn new(pane_id: PaneId) -> (Self, UnboundedSender<Vec<u8>>, UnboundedSender<ExitStatus>) {
        let (output_sender, output_receiver) = unbounded_channel();
        let (exit_sender, exit_receiver) = unbounded_channel();
        let new_pty_handle = PtyHandle {
            pane_id,
            output: output_receiver,
            exit: exit_receiver,
        };

        (new_pty_handle, output_sender, exit_sender)
    }

    /// The pane this handle addresses.
    #[must_use]
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// Await the next chunk of child output, or `None` once the child is gone
    /// and every sender has dropped.
    pub async fn recv_output(&mut self) -> Option<Vec<u8>> {
        self.output.recv().await
    }

    /// Await the child's exit status, or `None` if the watcher dropped without
    /// reporting one.
    pub async fn recv_exit(&mut self) -> Option<ExitStatus> {
        self.exit.recv().await
    }

    /// The next chunk of child output, or `None` if none is pending.
    pub fn try_read_output(&mut self) -> Option<Vec<u8>> {
        self.output.try_recv().ok()
    }

    /// The child's exit status, or `None` if it has not exited yet.
    pub fn try_exit_status(&mut self) -> Option<ExitStatus> {
        self.exit.try_recv().ok()
    }
}
