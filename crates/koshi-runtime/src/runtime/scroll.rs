//! Per-client scrollback view scrolling: moving a client's view of one pane up
//! into history or back down to live output, and re-anchoring scrolled-back
//! views as new output pushes lines into scrollback.
//!
//! The offset is per-client view state ([`Client::scroll_offset`]), so two
//! clients scroll a shared pane independently. Every public entry point clamps
//! the offset to `[0, scrollback len]` and marks the frame stale when the
//! offset actually moves.

use koshi_core::ids::{ClientId, PaneId};
use koshi_session::client::Client;

use crate::runtime::{render_schedule::InvalidationReason, state::Runtime};

impl Runtime {
    /// Scroll `client_id`'s view of `pane_id` up by `lines` into scrollback,
    /// clamped to the pane's retained history. An unknown client, a gone pane,
    /// or a view already at the clamp moves nothing and schedules no repaint.
    pub fn scroll_up(&mut self, client_id: ClientId, pane_id: PaneId, lines: usize) {
        let retained = self
            .terminal_engines
            .get(&pane_id)
            .map_or(0, |engine| engine.state().scrollback().len());
        let Some(client) = self.client_mut(client_id) else {
            return;
        };
        let current = client.scroll_offset(pane_id);
        let target = current.saturating_add(lines).min(retained);
        if target != current {
            client.set_scroll_offset(pane_id, target);
            self.render_scheduler
                .invalidate(InvalidationReason::StatusChanged);
        }
    }

    /// Scroll `client_id`'s view of `pane_id` down by `lines` toward live output;
    /// reaching `0` resumes following live. An unknown client or a view already
    /// following live moves nothing and schedules no repaint.
    pub fn scroll_down(&mut self, client_id: ClientId, pane_id: PaneId, lines: usize) {
        let Some(client) = self.client_mut(client_id) else {
            return;
        };
        let current = client.scroll_offset(pane_id);
        let target = current.saturating_sub(lines);
        if target != current {
            client.set_scroll_offset(pane_id, target);
            self.render_scheduler
                .invalidate(InvalidationReason::StatusChanged);
        }
    }

    /// Jump `client_id`'s view of `pane_id` to the oldest retained line: a
    /// [`scroll_up`](Self::scroll_up) by the maximum, which the clamp lands
    /// exactly on the retained count.
    pub fn scroll_to_top(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.scroll_up(client_id, pane_id, usize::MAX);
    }

    /// Snap `client_id`'s view of `pane_id` back to live output (offset `0`): a
    /// [`scroll_down`](Self::scroll_down) by the maximum.
    pub fn scroll_to_bottom(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.scroll_down(client_id, pane_id, usize::MAX);
    }

    /// Re-anchor every client scrolled back in `pane_id` after `pushed` lines
    /// entered its scrollback, so a parked view keeps showing the same history:
    /// its offset rises by `pushed`, clamped to `len_after` (the count retained
    /// after the push, so a view anchored past a truncated or erased top stops at
    /// the oldest surviving line). A live-following view (offset `0`) is left
    /// following. The walk covers only the session that owns the pane — a pane
    /// belongs to exactly one — and a pane already released is a no-op.
    pub(crate) fn anchor_scrolled_views(
        &mut self,
        pane_id: PaneId,
        pushed: usize,
        len_after: usize,
    ) {
        let Some(session) = self.session_for_pane_mut(pane_id) else {
            return;
        };
        for client in session.clients.list_attached_mut() {
            let current = client.scroll_offset(pane_id);
            if current > 0 {
                client.set_scroll_offset(pane_id, (current + pushed).min(len_after));
            }
        }
    }

    /// Mutable access to the client attached under `client_id` in any session, or
    /// `None` if no attached client has that id. Resolves the owning session via
    /// [`session_for_client_mut`](Self::session_for_client_mut), the shared
    /// client→session lookup.
    pub(crate) fn client_mut(&mut self, client_id: ClientId) -> Option<&mut Client> {
        self.session_for_client_mut(client_id)?
            .clients
            .get_mut(client_id)
    }
}

#[cfg(test)]
mod tests;
