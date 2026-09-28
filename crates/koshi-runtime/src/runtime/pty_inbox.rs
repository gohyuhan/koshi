//! Getting a pane's child output and exit into the runtime event inbox, and
//! recording a spawned pane's PTY.
//!
//! [`InboxSink`] sends [`RuntimeEvent::PtyOutput`] and
//! [`RuntimeEvent::ChildExit`] into the inbox. The PTY backend calls it from
//! the pane's own threads.
use std::sync::mpsc::Sender;

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, PtySize};
use koshi_pty::backend::state::PtySink;
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::scrollback::ScrollbackLimit;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// A [`PtySink`] that drops every pane's child output and exit straight into
/// the runtime inbox.
pub struct InboxSink {
    /// The inbox every event is sent on. These events queue with every other
    /// inbox event in arrival order.
    inbox_sender: Sender<RuntimeEvent>,
}

impl InboxSink {
    /// A sink feeding `inbox_sender`.
    #[must_use]
    pub fn from_event_sender(inbox_sender: Sender<RuntimeEvent>) -> Self {
        InboxSink { inbox_sender }
    }
}

impl PtySink for InboxSink {
    /// Queue one chunk of child output as [`RuntimeEvent::PtyOutput`]. Returns
    /// `true` when it is queued, `false` when the inbox is closed, which tells
    /// the reader to stop reading this pane.
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.inbox_sender
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
        let _ = self.inbox_sender.send(RuntimeEvent::ChildExit {
            pane_id,
            exit_status,
        });
    }
}

impl Server {
    /// Register a freshly spawned pane's PTY: mark `pane_id` live, record
    /// `pty_size`, and add a new terminal engine of `pty_size` capped by the
    /// config's `scrollback.maximum_line_count` and
    /// `scrollback.maximum_byte_count`. Every spawn path calls this. A record
    /// already held for `pane_id` is replaced.
    pub(crate) fn park_pane_pty(&mut self, pane_id: PaneId, pty_size: PtySize) {
        self.live_pane_ids.insert(pane_id);
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
}

#[cfg(test)]
mod tests;
