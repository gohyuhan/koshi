//! PTY output handling: the dispatcher's entry point for child output bytes.
//!
//! A [`RuntimeEvent::PtyOutput`](crate::runtime::event::RuntimeEvent::PtyOutput)
//! carries the raw bytes one pane's child wrote, already keyed by pane id.
//! [`Runtime::handle_pty_output`] routes them into that pane's
//! [`TerminalEngine`](tile_terminal::engine::TerminalEngine) — updating its
//! grid, cursor, and modes — and marks the screen stale so the event loop
//! schedules a repaint. Bytes for a pane with no engine (one closed while the
//! event sat in the inbox) are dropped without touching any state.

use tile_core::ids::PaneId;

use crate::runtime::{render_schedule::InvalidationReason, state::Runtime};

impl Runtime {
    /// Feed one chunk of child output into `pane_id`'s terminal engine and
    /// mark the screen stale with [`InvalidationReason::PtyOutput`].
    ///
    /// A `pane_id` with no engine — the pane closed while the chunk waited in
    /// the inbox — is ignored: no engine is touched and nothing is
    /// invalidated.
    pub fn handle_pty_output(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let Some(engine) = self.terminal_engines.get_mut(&pane_id) else {
            return;
        };
        engine.advance(bytes);
        self.render_scheduler
            .invalidate(InvalidationReason::PtyOutput);
    }
}

#[cfg(test)]
mod tests;
