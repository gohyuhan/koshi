//! Per-pane PTY forwarding: parking a freshly spawned pane's handle and
//! relaying its child output and exit into the runtime event inbox.
//!
//! A spawned pane's [`PtyHandle`] carries blocking receivers for the child's
//! output and exit. Rather than the event loop polling them, each pane gets two
//! forwarder threads that block on those receivers and push
//! [`RuntimeEvent::PtyOutput`] / [`RuntimeEvent::ChildExit`] into the single
//! inbox — so the child's I/O reaches the dispatcher the same way every other
//! event does.
//!
//! `park_pane_pty` takes the PTY maps and inbox sender by explicit reference
//! (not `&mut self`) so a caller mid-way through a session borrow can still
//! register a pane, matching the `reflow_changed` idiom.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::SystemTime;

use tile_core::ids::PaneId;
use tile_core::process::{ExitStatus, PtySize};
use tile_pty::backend::state::PtyHandle;
use tile_terminal::engine::TerminalEngine;

use crate::runtime::event::RuntimeEvent;
use crate::runtime::state::Runtime;

impl Runtime {
    /// Register a freshly spawned pane's PTY: hand its receivers to forwarder
    /// threads, then record its handle (as the live-pane token), its size, and
    /// a new terminal engine. Every spawn path funnels through here so output
    /// forwarding is wired identically wherever a pane is born.
    pub(crate) fn park_pane_pty(
        pty_handles: &mut HashMap<PaneId, PtyHandle>,
        pty_sizes: &mut HashMap<PaneId, PtySize>,
        terminal_engines: &mut HashMap<PaneId, TerminalEngine>,
        inbox_tx: &Sender<RuntimeEvent>,
        pane_id: PaneId,
        mut handle: PtyHandle,
        size: PtySize,
    ) {
        if let Some((output_rx, exit_rx)) = handle.take_receivers() {
            Self::spawn_pty_forwarder(inbox_tx, pane_id, output_rx, exit_rx);
        }
        pty_handles.insert(pane_id, handle);
        pty_sizes.insert(pane_id, size);
        terminal_engines.insert(pane_id, TerminalEngine::new(size));
    }

    /// Spawn the single relay thread for one pane. It forwards every output
    /// chunk, then — once the output channel closes (the child's PTY reached
    /// EOF, so all output is drained) — forwards the exit, stamping the time it
    /// observed it. Draining output before the exit preserves the order the user
    /// sees: all of the child's output, then the pane closes. The thread stops
    /// when the inbox drops (shutdown).
    fn spawn_pty_forwarder(
        inbox_tx: &Sender<RuntimeEvent>,
        pane_id: PaneId,
        output_rx: Receiver<Vec<u8>>,
        exit_rx: Receiver<ExitStatus>,
    ) {
        let inbox = inbox_tx.clone();
        thread::spawn(move || {
            while let Ok(bytes) = output_rx.recv() {
                if inbox
                    .send(RuntimeEvent::PtyOutput { pane_id, bytes })
                    .is_err()
                {
                    return;
                }
            }
            if let Ok(status) = exit_rx.recv() {
                let _ = inbox.send(RuntimeEvent::ChildExit {
                    pane_id,
                    status,
                    exited_at: SystemTime::now(),
                });
            }
        });
    }
}
