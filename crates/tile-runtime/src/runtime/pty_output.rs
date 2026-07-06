//! PTY output handling: the dispatcher's entry point for child output bytes.
//!
//! A [`RuntimeEvent::PtyOutput`](crate::runtime::event::RuntimeEvent::PtyOutput)
//! carries the raw bytes one pane's child wrote, already keyed by pane id.
//! [`Runtime::handle_pty_output`] routes them into that pane's
//! [`TerminalEngine`](tile_terminal::engine::TerminalEngine) — updating its
//! grid, cursor, and modes — writes the engine's device-query replies
//! (DA/DSR/DECRQM answers) back into the pane's PTY, and marks the screen
//! stale so the event loop schedules a repaint. Bytes for a pane with no
//! engine (one closed while the event sat in the inbox) are dropped without
//! touching any state.

use tile_core::ids::PaneId;

use crate::runtime::{render_schedule::InvalidationReason, state::Runtime};

impl Runtime {
    /// Feed one chunk of child output into `pane_id`'s terminal engine, write
    /// any device-query replies the chunk produced back into the pane's PTY,
    /// and mark the screen stale with [`InvalidationReason::PtyOutput`].
    ///
    /// A `pane_id` with no engine — the pane closed while the chunk waited in
    /// the inbox — is ignored: no engine is touched and nothing is
    /// invalidated. A reply write that fails is dropped: the pane's PTY is
    /// already gone, and its exit is on the way through the inbox.
    ///
    /// Lines this chunk scrolls off the top feed the scrollback; every client
    /// scrolled back in this pane is then re-anchored by that many lines so its
    /// parked view keeps showing the same history while live output accumulates
    /// below.
    pub fn handle_pty_output(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let Some(engine) = self.terminal_engines.get_mut(&pane_id) else {
            return;
        };
        // Count lines that entered scrollback across the chunk by diffing the
        // buffer's monotonic push counter — `clear` (`CSI 3 J`) never resets it,
        // so the delta is exact even when the chunk erases or truncates history.
        let before = engine.state().scrollback();
        let pushed_before = before.total_pushed();
        let len_before = before.len();
        let replies = engine.advance(bytes);
        let after = engine.state().scrollback();
        let len_after = after.len();
        let pushed = (after.total_pushed() - pushed_before) as usize;

        if !replies.is_empty() {
            let _ = self.pty_backend().write(pane_id, &replies);
        }
        // Parked offsets need adjusting only when history gained lines (offsets
        // rise) or shrank under an erase (offsets reclamp); the common chunk that
        // touches no history skips the client walk entirely.
        if pushed > 0 || len_after < len_before {
            self.anchor_scrolled_views(pane_id, pushed, len_after);
        }
        self.render_scheduler
            .invalidate(InvalidationReason::PtyOutput);
    }
}

#[cfg(test)]
mod tests;
