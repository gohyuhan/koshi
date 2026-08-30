//! PTY output handling: the dispatcher's entry point for child output bytes.
//!
//! A [`RuntimeEvent::PtyOutput`](crate::runtime::event::RuntimeEvent::PtyOutput)
//! carries the raw bytes one pane's child wrote, already keyed by pane id.
//! [`Server::handle_pty_output`] routes them into that pane's
//! [`TerminalEngine`](koshi_terminal::engine::TerminalEngine) — updating its
//! grid, cursor, and modes — writes the engine's device-query replies
//! (answers to DA/DSR/DECRQM: escape sequences the child sends to ask "what
//! terminal are you" / "what's your status" / "is this mode on") back into
//! the pane's PTY, and marks the screen stale for the event loop to schedule a
//! repaint. Shell-integration events are published in marker order. Bytes for
//! a pane with no engine (one closed while the event sat in the inbox) are
//! dropped without touching any state.

use koshi_core::event::{Event, PaneCommandFinished, PaneCommandStarted};
use koshi_core::ids::PaneId;
use koshi_terminal::state::ShellIntegrationFact;

use crate::{runtime::render_schedule::InvalidationReason, server::Server};

impl Server {
    /// Feed one chunk of child output into `pane_id`'s terminal engine, write
    /// any device-query replies the chunk produced back into the pane's PTY,
    /// and mark the screen stale with [`InvalidationReason::PtyOutput`].
    ///
    /// A `pane_id` with no engine — the pane closed while the chunk waited in
    /// the inbox — is ignored: no engine is touched, nothing is published, and
    /// nothing is invalidated. A reply write that fails is logged at error
    /// level and dropped; the querying child gets no answer.
    ///
    /// Lines this chunk scrolls off the top feed the scrollback. Every client
    /// whose view of this pane is held is then re-anchored by that many lines,
    /// clamped to the lines still retained, and keeps showing the same text
    /// while live output accumulates below. A highlight whose every line this
    /// chunk erased (`CSI 3 J`) or evicted past the scrollback cap is dropped
    /// before that re-anchor.
    ///
    /// A chunk that leaves the pane on a different screen (primary or
    /// alternate) than it started on drops every client's highlight in it.
    ///
    /// Shell-integration facts become command lifecycle events in marker order.
    pub fn handle_pty_output(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let Some(engine) = self.terminal_engines.get_mut(&pane_id) else {
            return;
        };
        // The lines this chunk pushed into scrollback are the rise in the
        // buffer's push counter. That counter only grows: `clear` (`CSI 3 J`)
        // and eviction past the cap leave it as it is. The rise stays exact for
        // a chunk that erases or truncates history.
        let scrollback_before = engine.state().scrollback();
        let pushed_before = scrollback_before.total_pushed();
        let len_before = scrollback_before.len();
        let screen_before = engine.state().active_screen();
        let (replies, shell_facts) = engine.advance_with_shell_integration(bytes);
        let scrollback_after = engine.state().scrollback();
        let len_after = scrollback_after.len();
        let pushed = (scrollback_after.total_pushed() - pushed_before) as usize;
        let screen_after = engine.state().active_screen();

        if !replies.is_empty() {
            if let Err(error) = self.pty_backend().write(pane_id, &replies) {
                tracing::error!(
                    %pane_id,
                    %error,
                    replies = replies.len(),
                    "the answer to a pane's device query could not be written"
                );
            }
        }
        if screen_before != screen_after {
            self.clear_pane_selections(pane_id);
        }
        // Held views move only when history gained lines (offsets rise) or
        // shrank under an erase (offsets reclamp). A chunk that touches no
        // history skips the client walk. A highlight whose every line the chunk
        // erased or evicted is dropped before the walk.
        if pushed > 0 || len_after < len_before {
            self.drop_evicted_selections(pane_id);
            self.anchor_held_views(pane_id, pushed, len_after);
        }
        if !shell_facts.is_empty() {
            let events: Vec<Event> = shell_facts
                .into_iter()
                .map(|fact| match fact {
                    ShellIntegrationFact::CommandStarted => {
                        Event::PaneCommandStarted(PaneCommandStarted { pane_id })
                    }
                    ShellIntegrationFact::CommandFinished { exit_code } => {
                        Event::PaneCommandFinished(PaneCommandFinished { pane_id, exit_code })
                    }
                })
                .collect();
            self.publish_events(&events);
        }
        self.render_scheduler
            .invalidate(InvalidationReason::PtyOutput);
    }
}

#[cfg(test)]
mod tests;
