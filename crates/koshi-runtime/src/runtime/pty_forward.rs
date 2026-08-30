//! Getting a pane's child output and exit into the runtime event inbox.
//!
//! Both routes send [`RuntimeEvent::PtyOutput`] and [`RuntimeEvent::ChildExit`]
//! into the one inbox.
//!
//! [`InboxSink`] is the route the running binary takes: the PTY backend calls
//! it from the pane's own reader thread, and starts no other thread.
//!
//! The other route serves a backend that hands back a [`PtyHandle`] carrying
//! channels, such as the fake backend the tests drive. Those receivers block;
//! the pane gets a forwarder thread that blocks on them. Parking a pane picks
//! the route from the handle it was given: a handle with no receivers is
//! already wired to a sink.
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::SystemTime;

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, PtySize};
use koshi_pty::backend::state::{PtyHandle, PtySink};
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::scrollback::ScrollbackLimit;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// A [`PtySink`] that drops every pane's child output and exit straight into
/// the runtime inbox.
///
/// A backend holding this sink starts no per-pane forwarder thread: the
/// pane's reader thread builds the event and sends it itself.
pub struct InboxSink {
    /// The inbox every event is sent on. A clone of the server's own sender:
    /// these events queue with all the others in arrival order.
    inbox_tx: Sender<RuntimeEvent>,
}

impl InboxSink {
    /// A sink feeding `inbox_tx`.
    #[must_use]
    pub fn new(inbox_tx: Sender<RuntimeEvent>) -> Self {
        InboxSink { inbox_tx }
    }
}

impl PtySink for InboxSink {
    /// Queue one chunk of child output as [`RuntimeEvent::PtyOutput`]. Returns
    /// `true` when it is queued, `false` when the inbox is closed, which tells
    /// the reader to stop reading this pane.
    fn output(&self, pane_id: PaneId, bytes: Vec<u8>) -> bool {
        self.inbox_tx
            .send(RuntimeEvent::PtyOutput { pane_id, bytes })
            .is_ok()
    }

    /// Queue the child's exit as [`RuntimeEvent::ChildExit`], stamped with
    /// `SystemTime::now()`. The backend calls this after the pane's last
    /// output, and reads the pane no further. A closed inbox drops the event.
    fn exit(&self, pane_id: PaneId, status: ExitStatus) {
        let _ = self.inbox_tx.send(RuntimeEvent::ChildExit {
            pane_id,
            status,
            exited_at: SystemTime::now(),
        });
    }
}

impl Server {
    /// Register a freshly spawned pane's PTY: start its output on the way to
    /// the inbox, then record its handle (the live-pane token), `size`, and a
    /// new terminal engine of `size` capped by the config's
    /// `scrollback.max_lines` and `scrollback.max_bytes`. Every spawn path
    /// calls this. A record already held for `pane_id` is replaced.
    ///
    /// A handle carrying receivers gets a forwarder thread that drains them; a
    /// handle without receivers already delivers through [`InboxSink`].
    ///
    /// # Panics
    ///
    /// Panics when the operating system refuses to start the forwarder thread.
    pub(crate) fn park_pane_pty(&mut self, pane_id: PaneId, mut handle: PtyHandle, size: PtySize) {
        if let Some((output_rx, exit_rx)) = handle.take_receivers() {
            Self::spawn_pty_forwarder(&self.inbox_tx, pane_id, output_rx, exit_rx);
        }
        self.pty_handles.insert(pane_id, handle);
        self.pty_sizes.insert(pane_id, size);
        let scrollback = &self.config.scrollback;
        let limit = ScrollbackLimit::new(scrollback.max_lines, scrollback.max_bytes);
        self.terminal_engines
            .insert(pane_id, TerminalEngine::with_scrollback(size, limit));
    }

    /// Start the one relay thread for `pane_id`. It forwards every chunk from
    /// `output_rx` in arrival order, then, once `output_rx` closes (the child's
    /// PTY reached end of file and all output is drained), forwards the one
    /// status from `exit_rx`. It builds both events through [`InboxSink`].
    ///
    /// The thread ends without forwarding the exit when the inbox closes, and
    /// ends when `exit_rx` closes with no status on it.
    ///
    /// # Panics
    ///
    /// Panics when the operating system refuses to start the thread.
    fn spawn_pty_forwarder(
        inbox_tx: &Sender<RuntimeEvent>,
        pane_id: PaneId,
        output_rx: Receiver<Vec<u8>>,
        exit_rx: Receiver<ExitStatus>,
    ) {
        let sink = InboxSink::new(inbox_tx.clone());
        let _ = thread::Builder::new()
            .name("koshi-pty-fwd".to_string())
            .spawn(move || {
                while let Ok(bytes) = output_rx.recv() {
                    if !sink.output(pane_id, bytes) {
                        return;
                    }
                }
                if let Ok(status) = exit_rx.recv() {
                    sink.exit(pane_id, status);
                }
            })
            .expect("spawn pty forwarder thread");
    }
}

#[cfg(test)]
mod tests;
