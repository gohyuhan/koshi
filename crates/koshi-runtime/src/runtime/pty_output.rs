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
struct TerminalAdvanceStateBefore {
    pushed_line_count: u64,
    retained_line_count: usize,
    active_screen: koshi_terminal::state::Screen,
    graphics_event_count: usize,
    dropped_graphics_error_count: usize,
}

impl TerminalAdvanceStateBefore {
    fn capture_terminal_advance_state(terminal_engine: &TerminalEngine) -> Self {
        let scrollback_state = terminal_engine.get_terminal_state().get_scrollback();
        Self {
            pushed_line_count: scrollback_state.get_total_pushed_line_count(),
            retained_line_count: scrollback_state.get_retained_line_count(),
            active_screen: terminal_engine.get_terminal_state().get_active_screen(),
            graphics_event_count: terminal_engine.list_graphics_events().count(),
            dropped_graphics_error_count: terminal_engine.get_dropped_graphics_error_count(),
        }
    }
}

impl Server {
    /// Feed one chunk of child output into `pane_id`'s terminal engine, write
    /// any device-query replies the chunk produced back into the pane's PTY,
    /// and mark the screen stale.
    ///
    /// Before the chunk is read, the engine takes the cell size of the tab
    /// that holds `pane_id`
    /// ([`Session::get_tab_cell_size`](koshi_session::session::state::Session::get_tab_cell_size)),
    /// or the session's floating cell size when `pane_id` is a floating pane
    /// ([`Session::get_floating_cell_size`](koshi_session::session::state::Session::get_floating_cell_size)).
    /// With no such cell size, the engine keeps its own.
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
    pub fn handle_pty_output(&mut self, pane_id: PaneId, output_bytes: &[u8]) {
        let terminal_cell_size = self.session_by_id.values().find_map(|session| {
            if let Some(tab) = session.find_tab_by_pane_id(pane_id) {
                return session.get_tab_cell_size(tab.get_tab_id());
            }
            if session.floating_set.has_pane(pane_id) {
                return session.get_floating_cell_size();
            }
            None
        });
        let Some(terminal_engine) = self.terminal_engine_by_pane_id.get_mut(&pane_id) else {
            return;
        };
        if let Some(terminal_cell_size) = terminal_cell_size {
            terminal_engine.set_cell_size(terminal_cell_size);
        }
        let terminal_advance_state_before =
            TerminalAdvanceStateBefore::capture_terminal_advance_state(terminal_engine);
        let (reply_bytes, shell_integration_facts, is_terminal_advanced) = terminal_engine
            .process_pty_output_with_shell_integration_at(output_bytes, Instant::now());
        if !is_terminal_advanced && !output_bytes.is_empty() {
            return;
        }
        self.finish_terminal_advance(
            pane_id,
            terminal_advance_state_before,
            reply_bytes,
            shell_integration_facts,
        );
    }

    pub(in crate::runtime) fn expire_synchronized_output(
        &mut self,
        pane_id: PaneId,
        current_time: Instant,
    ) {
        let Some(terminal_engine) = self.terminal_engine_by_pane_id.get_mut(&pane_id) else {
            return;
        };
        let terminal_advance_state_before =
            TerminalAdvanceStateBefore::capture_terminal_advance_state(terminal_engine);
        let Some((reply_bytes, shell_integration_facts)) =
            terminal_engine.expire_synchronized_output(current_time)
        else {
            return;
        };
        self.finish_terminal_advance(
            pane_id,
            terminal_advance_state_before,
            reply_bytes,
            shell_integration_facts,
        );
    }

    fn finish_terminal_advance(
        &mut self,
        pane_id: PaneId,
        terminal_advance_state_before: TerminalAdvanceStateBefore,
        reply_bytes: Vec<u8>,
        shell_integration_facts: Vec<ShellIntegrationFact>,
    ) {
        let Some(terminal_engine) = self.terminal_engine_by_pane_id.get(&pane_id) else {
            return;
        };
        for graphics_error in terminal_engine
            .list_graphics_events()
            .skip(terminal_advance_state_before.graphics_event_count)
            .filter_map(|event| event.as_ref().err())
        {
            tracing::warn!(%pane_id, %graphics_error, "a graphics event in a pane's output failed");
        }
        let dropped_graphics_error_count = terminal_engine
            .get_dropped_graphics_error_count()
            .saturating_sub(terminal_advance_state_before.dropped_graphics_error_count);
        if dropped_graphics_error_count != 0 {
            let graphics_error = GraphicsError::QueueFull {
                dropped_event_count: dropped_graphics_error_count,
            };
            tracing::warn!(%pane_id, %graphics_error, "image placement errors were dropped");
        }
        let scrollback_after = terminal_engine.get_terminal_state().get_scrollback();
        let retained_line_count_after = scrollback_after.get_retained_line_count();
        let pushed_line_count = (scrollback_after.get_total_pushed_line_count()
            - terminal_advance_state_before.pushed_line_count)
            as usize;
        let active_screen_after = terminal_engine.get_terminal_state().get_active_screen();

        if !reply_bytes.is_empty() {
            if let Err(write_error) = self
                .get_pty_backend()
                .write_pane_input(pane_id, &reply_bytes)
            {
                tracing::error!(
                    %pane_id,
                    %write_error,
                    reply_byte_count = reply_bytes.len(),
                    "the answer to a pane's device query could not be written"
                );
            }
        }
        if terminal_advance_state_before.active_screen != active_screen_after {
            self.clear_pane_selections(pane_id);
        }
        // Held views move only when history gained lines (offsets rise) or
        // shrank under an erase (offsets reclamp). A chunk that touches no
        // history skips the client walk. A highlight whose every line the chunk
        // erased or evicted is dropped before the walk.
        if pushed_line_count > 0
            || retained_line_count_after < terminal_advance_state_before.retained_line_count
        {
            self.drop_evicted_selections(pane_id);
            self.anchor_held_views(pane_id, pushed_line_count, retained_line_count_after);
        }
        if !shell_integration_facts.is_empty() {
            let shell_integration_events: Vec<Event> = shell_integration_facts
                .into_iter()
                .map(|shell_integration_fact| match shell_integration_fact {
                    ShellIntegrationFact::CommandStarted => {
                        Event::PaneCommandStarted(PaneCommandStarted { pane_id })
                    }
                    ShellIntegrationFact::CommandFinished { exit_code } => {
                        Event::PaneCommandFinished(PaneCommandFinished { pane_id, exit_code })
                    }
                })
                .collect();
            self.publish_events(&shell_integration_events);
        }
        self.render_scheduler.invalidate();
    }
}

#[cfg(test)]
mod tests;
