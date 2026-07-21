//! PTY output handling: the dispatcher's entry point for child output bytes.
//!
//! A [`RuntimeEvent::PtyOutput`](crate::runtime::event::RuntimeEvent::PtyOutput)
//! carries the raw bytes one pane's child wrote, already keyed by pane id.
//! [`Server::handle_pty_output`] routes them into that pane's
//! [`TerminalEngine`](koshi_terminal::engine::TerminalEngine) — updating its
//! grid, cursor, and modes — writes the engine's device-query replies
//! (answers to DA/DSR/DECRQM: escape sequences the child sends to ask "what
//! terminal are you" / "what's your status" / "is this mode on") back into
//! the pane's PTY, and marks the screen stale so the event loop schedules a
//! repaint. Bytes for a pane with no engine (one closed while the event sat in
//! the inbox) are dropped without touching any state.

use koshi_core::ids::PaneId;

use crate::{runtime::render_schedule::InvalidationReason, server::Server};

impl Server {
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
    /// whose view of this pane is held is then re-anchored by that many lines so
    /// it keeps showing the same text while live output accumulates below. A
    /// highlight whose every line this chunk erased (`CSI 3 J`) or evicted past
    /// the scrollback cap is dropped first — it could never draw again, yet it
    /// would keep holding its client's view.
    ///
    /// A chunk that switches the pane between its primary and alternate screens
    /// drops every client's highlight in it: a highlight names a line by how many
    /// the pane had pushed into scrollback, and the alternate screen keeps no
    /// scrollback and shares no lines, so the name means nothing there.
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
        let screen_before = engine.state().active_screen();
        let replies = engine.advance(bytes);
        let after = engine.state().scrollback();
        let len_after = after.len();
        let pushed = (after.total_pushed() - pushed_before) as usize;
        let screen_after = engine.state().active_screen();

        if !replies.is_empty() {
            let _ = self.pty_backend().write(pane_id, &replies);
        }
        if screen_before != screen_after {
            self.clear_pane_selections(pane_id);
        }
        // Held views need adjusting only when history gained lines (offsets rise)
        // or shrank under an erase (offsets reclamp); the common chunk that
        // touches no history skips the client walk entirely. A highlight whose
        // every line the chunk erased or evicted is dropped first, so it stops
        // holding a view over text that no longer exists.
        if pushed > 0 || len_after < len_before {
            self.drop_evicted_selections(pane_id);
            self.anchor_held_views(pane_id, pushed, len_after);
        }
        self.render_scheduler
            .invalidate(InvalidationReason::PtyOutput);
    }
}

#[cfg(test)]
mod tests;
