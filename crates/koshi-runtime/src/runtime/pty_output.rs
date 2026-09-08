//! PTY output handling: the dispatcher's entry point for child output bytes.
//!
//! A [`RuntimeEvent::PtyOutput`](crate::runtime::event::RuntimeEvent::PtyOutput)
//! carries the raw bytes one pane's child wrote, already keyed by pane id.
//! [`Server::handle_pty_output`] routes them into that pane's
//! [`TerminalEngine`] — updating its
//! grid, cursor, and modes — writes the engine's device-query replies
//! (answers to DA/DSR/DECRQM: escape sequences the child sends to ask "what
//! terminal are you" / "what's your status" / "is this mode on") back into
//! the pane's PTY, and marks the screen stale for the event loop to schedule a
//! repaint. Shell-integration events are published in marker order. Bytes for
//! a pane with no engine (one closed while the event sat in the inbox) are
//! dropped without touching any state.

use std::time::Instant;

use koshi_core::event::{Event, PaneCommandFinished, PaneCommandStarted};
use koshi_core::ids::PaneId;
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::graphics::GraphicsError;
use koshi_terminal::state::ShellIntegrationFact;

use crate::server::Server;

#[derive(Clone, Copy)]
struct TerminalAdvanceBefore {
    pushed: u64,
    scrollback_len: usize,
    screen: koshi_terminal::state::Screen,
    graphics_events: usize,
    graphics_errors_dropped: usize,
}

impl TerminalAdvanceBefore {
    fn capture(engine: &TerminalEngine) -> Self {
        let scrollback = engine.state().scrollback();
        Self {
            pushed: scrollback.total_pushed(),
            scrollback_len: scrollback.len(),
            screen: engine.state().active_screen(),
            graphics_events: engine.graphics_events().count(),
            graphics_errors_dropped: engine.graphics_errors_dropped(),
        }
    }
}

impl Server {
    /// Feed one chunk of child output into `pane_id`'s terminal engine, write
    /// any device-query replies the chunk produced back into the pane's PTY,
    /// and mark the screen stale.
    ///
    /// A `pane_id` with no engine — the pane closed while the chunk waited in
    /// the inbox — is ignored: no engine is touched, nothing is published, and
    /// nothing is invalidated. A reply write that fails is logged at error
    /// level and dropped; the querying child gets no answer. Every graphics
    /// error the chunk completed is logged at warn level with its typed reason.
    /// A graphics error dropped by the bounded queue is logged as a typed
    /// queue-full error with its count.
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
        let cell_size = self.sessions.values().find_map(|session| {
            let tab = session
                .tabs
                .values()
                .find(|tab| tab.layout().contains_pane(pane_id))?;
            session.tab_cell_size(tab.id())
        });
        let Some(engine) = self.terminal_engines.get_mut(&pane_id) else {
            return;
        };
        if let Some(size) = cell_size {
            engine.set_cell_size(size);
        }
        let before = TerminalAdvanceBefore::capture(engine);
        let (replies, shell_facts, advanced) =
            engine.advance_with_shell_integration_at(bytes, Instant::now());
        if !advanced && !bytes.is_empty() {
            return;
        }
        self.finish_terminal_advance(pane_id, before, replies, shell_facts);
    }

    pub(in crate::runtime) fn expire_synchronized_output(
        &mut self,
        pane_id: PaneId,
        now: Instant,
    ) -> bool {
        let Some(engine) = self.terminal_engines.get_mut(&pane_id) else {
            return false;
        };
        let before = TerminalAdvanceBefore::capture(engine);
        let Some((replies, shell_facts)) = engine.expire_synchronized_output(now) else {
            return false;
        };
        self.finish_terminal_advance(pane_id, before, replies, shell_facts);
        true
    }

    fn finish_terminal_advance(
        &mut self,
        pane_id: PaneId,
        before: TerminalAdvanceBefore,
        replies: Vec<u8>,
        shell_facts: Vec<ShellIntegrationFact>,
    ) {
        let Some(engine) = self.terminal_engines.get(&pane_id) else {
            return;
        };
        for error in engine
            .graphics_events()
            .skip(before.graphics_events)
            .filter_map(|event| event.as_ref().err())
        {
            tracing::warn!(%pane_id, %error, "a graphics event in a pane's output failed");
        }
        let dropped_errors = engine
            .graphics_errors_dropped()
            .saturating_sub(before.graphics_errors_dropped);
        if dropped_errors != 0 {
            let error = GraphicsError::QueueFull {
                dropped: dropped_errors,
            };
            tracing::warn!(%pane_id, %error, "image placement errors were dropped");
        }
        let scrollback_after = engine.state().scrollback();
        let len_after = scrollback_after.len();
        let pushed = (scrollback_after.total_pushed() - before.pushed) as usize;
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
        if before.screen != screen_after {
            self.clear_pane_selections(pane_id);
        }
        // Held views move only when history gained lines (offsets rise) or
        // shrank under an erase (offsets reclamp). A chunk that touches no
        // history skips the client walk. A highlight whose every line the chunk
        // erased or evicted is dropped before the walk.
        if pushed > 0 || len_after < before.scrollback_len {
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
        self.render_scheduler.invalidate();
    }
}

#[cfg(test)]
mod tests;
