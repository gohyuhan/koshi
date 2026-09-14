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
    pub fn from_event_sender(inbox_tx: Sender<RuntimeEvent>) -> Self {
        InboxSink { inbox_tx }
    }
}

impl PtySink for InboxSink {
    /// Queue one chunk of child output as [`RuntimeEvent::PtyOutput`]. Returns
    /// `true` when it is queued, `false` when the inbox is closed, which tells
    /// the reader to stop reading this pane.
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.inbox_tx
            .send(RuntimeEvent::PtyOutput {
                pane_id,
                output_bytes,
            })
            .is_ok()
    }

    /// Queue the child's exit as [`RuntimeEvent::ChildExit`]. The backend calls
    /// this after the pane's last output, and reads the pane no further. A
    /// closed inbox drops the event.
    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        let _ = self.inbox_tx.send(RuntimeEvent::ChildExit {
            pane_id,
            exit_status,
        });
    }
}

impl Server {
    /// Register a freshly spawned pane's PTY: start its output on the way to
    /// the inbox, then record its handle (the live-pane token), `size`, and a
    /// new terminal engine of `size` capped by the config's
    /// `scrollback.maximum_line_count` and `scrollback.maximum_byte_count`. Every spawn path
    /// calls this. A record already held for `pane_id` is replaced.
    ///
    /// A handle carrying receivers gets a forwarder thread that drains them; a
    /// handle without receivers already delivers through [`InboxSink`].
    ///
    /// # Panics
    ///
    /// Panics when the operating system refuses to start the forwarder thread.
    pub(crate) fn park_pane_pty(
        &mut self,
        pane_id: PaneId,
        mut pty_handle: PtyHandle,
        pty_size: PtySize,
    ) {
        if let Some((output_receiver, exit_receiver)) = pty_handle.take_output_and_exit_receivers()
        {
            Self::spawn_pty_forwarder(&self.inbox_tx, pane_id, output_receiver, exit_receiver);
        }
        self.pty_handle_by_pane_id.insert(pane_id, pty_handle);
        self.pty_size_by_pane_id.insert(pane_id, pty_size);
        let scrollback_config = &self.config.scrollback;
        let scrollback_limit = ScrollbackLimit::from_line_and_byte_limits(
            scrollback_config.maximum_line_count,
            scrollback_config.maximum_byte_count,
        );
        self.terminal_engine_by_pane_id.insert(
            pane_id,
            TerminalEngine::with_scrollback(pty_size, scrollback_limit),
        );
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
        output_receiver: Receiver<Vec<u8>>,
        exit_receiver: Receiver<ExitStatus>,
    ) {
        let event_sink = InboxSink::from_event_sender(inbox_tx.clone());
        let _ = thread::Builder::new()
            .name("koshi-pty-fwd".to_string())
            .spawn(move || {
                while let Ok(output_bytes) = output_receiver.recv() {
                    if !event_sink.accept_output_bytes(pane_id, output_bytes) {
                        return;
                    }
                }
                if let Ok(exit_status) = exit_receiver.recv() {
                    event_sink.accept_exit_status(pane_id, exit_status);
                }
            })
            .expect("spawn pty forwarder thread");
    }
}

#[cfg(test)]
mod tests;
