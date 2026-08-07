//! Getting a pane's child output and exit into the runtime event inbox.
//!
//! Both routes end the same way — [`RuntimeEvent::PtyOutput`] and
//! [`RuntimeEvent::ChildExit`] land in the single inbox, so a child's I/O
//! reaches the dispatcher exactly like every other event.
//!
//! [`InboxSink`] is the route the running binary takes: the PTY backend calls
//! it from the pane's own reader thread, and no other thread is started.
//!
//! The other route is for a backend that hands back a [`PtyHandle`] carrying
//! channels instead — the fake backend the tests drive, for one. Those
//! receivers block, so the pane gets a forwarder thread to block on them.
//! Parking a pane picks the route from the handle it was given: a handle with
//! no receivers was already wired to a sink.
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
    /// The inbox every event is sent on. Cloned from the server's own sender,
    /// so these events queue with all the others in arrival order.
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
    /// Queue one chunk of child output. A closed inbox means the runtime is
    /// gone, which is reported back as "stop reading this pane".
    fn output(&self, pane_id: PaneId, bytes: Vec<u8>) -> bool {
        self.inbox_tx
            .send(RuntimeEvent::PtyOutput { pane_id, bytes })
            .is_ok()
    }

    /// Queue the child's exit, stamped with the time it was observed. The
    /// backend calls this after the pane's last output, so the user sees
    /// everything the child printed before the pane closes, and stops reading
    /// the pane afterwards, so nothing arrives for a pane already removed.
    fn exit(&self, pane_id: PaneId, status: ExitStatus) {
        let _ = self.inbox_tx.send(RuntimeEvent::ChildExit {
            pane_id,
            status,
            exited_at: SystemTime::now(),
        });
    }
}

impl Server {
    /// Register a freshly spawned pane's PTY: make sure its output is on its
    /// way to the inbox, then record its handle (as the live-pane token), its
    /// size, and a new terminal engine. Every spawn path funnels through here
    /// so output forwarding is wired identically wherever a pane is born.
    ///
    /// A handle with receivers came from a backend with no sink, so the pane
    /// gets a forwarder thread to drain them; a handle without receivers is
    /// already delivering through [`InboxSink`] and needs no thread.
    pub(crate) fn park_pane_pty(&mut self, pane_id: PaneId, mut handle: PtyHandle, size: PtySize) {
        if let Some((output_rx, exit_rx)) = handle.take_receivers() {
            Self::spawn_pty_forwarder(&self.inbox_tx, pane_id, output_rx, exit_rx);
        }
        self.pty_handles.insert(pane_id, handle);
        self.pty_sizes.insert(pane_id, size);
        // Honor the user's configured scrollback caps for every pane created
        // after the config loaded (genesis, new panes, profile panes).
        let scrollback = &self.config.scrollback;
        let limit = ScrollbackLimit::new(scrollback.max_lines, scrollback.max_bytes);
        self.terminal_engines
            .insert(pane_id, TerminalEngine::with_scrollback(size, limit));
    }

    /// Spawn the single relay thread for one pane. It forwards every output
    /// chunk, then — once the output channel closes (the child's PTY reached
    /// EOF, end of file, so all output is drained) — forwards the exit,
    /// stamping the time it observed it. Draining output before the exit
    /// preserves the order the user sees: all of the child's output, then the
    /// pane closes. The thread stops when the inbox drops (shutdown).
    ///
    /// The events themselves are built by [`InboxSink`], the same way the
    /// backend builds them when it delivers to a sink directly, so a pane
    /// reaches the dispatcher identically whichever route carried it.
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
