//! Per-client scrollback view scrolling: moving a client's view of one pane up
//! into history or back down to live output, and re-anchoring held views as new
//! output pushes lines into scrollback.
//!
//! The offset is per-client view state ([`Client::get_scroll_offset`](koshi_session::client::Client::get_scroll_offset)), so two
//! clients scroll a shared pane independently. Every public entry point keeps
//! the offset inside `[0, scrollback len]` and marks the frame stale when the
//! offset moves.
//!
//! Scrolling moves a view; it never decides whether the view is *held* against
//! live output. That is [`koshi_session::client::Client::is_view_held`], derived from the offset and
//! the client's highlight: scrolling back to the bottom follows live again only
//! when no highlight is holding the view there.

use koshi_core::ids::{ClientId, PaneId};

use crate::server::Server;

impl Server {
    /// Scroll `client_id`'s view of `pane_id` up by `line_count` into scrollback,
    /// clamped to the pane's retained history. An unknown client, or a view
    /// already at the clamp, moves nothing and schedules no repaint. A pane
    /// with no terminal engine retains nothing, so a view scrolled up in it
    /// clamps back to the newest line.
    pub fn scroll_up(&mut self, client_id: ClientId, pane_id: PaneId, line_count: usize) {
        let retained_line_count =
            self.terminal_engine_by_pane_id
                .get(&pane_id)
                .map_or(0, |terminal_engine| {
                    terminal_engine
                        .get_terminal_state()
                        .get_scrollback()
                        .get_retained_line_count()
                });
        let Some(client) = self.get_client_mut(client_id) else {
            return;
        };
        let current_scroll_offset = client.get_scroll_offset(pane_id);
        let target_scroll_offset = current_scroll_offset
            .saturating_add(line_count)
            .min(retained_line_count);
        if target_scroll_offset != current_scroll_offset {
            client.set_scroll_offset(pane_id, target_scroll_offset);
            self.render_scheduler.invalidate();
        }
    }

    /// Scroll `client_id`'s view of `pane_id` down by `line_count` toward live output;
    /// reaching `0` returns it to the newest line, where it follows live again
    /// unless a highlight is holding it. An unknown client or a view already at
    /// the newest line moves nothing and schedules no repaint.
    pub fn scroll_down(&mut self, client_id: ClientId, pane_id: PaneId, line_count: usize) {
        let Some(client) = self.get_client_mut(client_id) else {
            return;
        };
        let current_scroll_offset = client.get_scroll_offset(pane_id);
        let target_scroll_offset = current_scroll_offset.saturating_sub(line_count);
        if target_scroll_offset != current_scroll_offset {
            client.set_scroll_offset(pane_id, target_scroll_offset);
            self.render_scheduler.invalidate();
        }
    }

    /// Jump `client_id`'s view of `pane_id` to the oldest retained line: a
    /// [`scroll_up`](Self::scroll_up) by the maximum, which the clamp lands
    /// exactly on the retained count.
    pub fn scroll_to_top(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.scroll_up(client_id, pane_id, usize::MAX);
    }

    /// Snap `client_id`'s view of `pane_id` back to the newest line: a
    /// [`scroll_down`](Self::scroll_down) by the maximum.
    pub fn scroll_to_bottom(&mut self, client_id: ClientId, pane_id: PaneId) {
        self.scroll_down(client_id, pane_id, usize::MAX);
    }

    /// Re-anchor every client whose view of `pane_id` is held after
    /// `pushed_line_count` lines entered its scrollback, so a held view keeps
    /// showing the same text: its offset rises by `pushed_line_count`, clamped
    /// to `retained_line_count_after` (the count retained
    /// after the push, so a view anchored past a truncated or erased top stops at
    /// the oldest surviving line). A view that is not held follows live output
    /// and is left alone.
    ///
    /// Held is [`koshi_session::client::Client::is_view_held`] — scrolled up, or a highlight up in this
    /// pane. That covers a view held on the *newest* line, which an offset alone
    /// could not express: it rises with the text it holds instead of staying at
    /// the bottom and showing whatever arrives next.
    ///
    /// The walk covers only the session that owns the pane — a pane belongs to
    /// exactly one — and each client is re-anchored on its own, so one client's
    /// held view never moves another's view of the same pane. A pane already
    /// released is a no-op.
    pub(crate) fn anchor_held_views(
        &mut self,
        pane_id: PaneId,
        pushed_line_count: usize,
        retained_line_count_after: usize,
    ) {
        let Some(session) = self.get_session_for_pane_mut(pane_id) else {
            return;
        };
        for client in session.clients.list_attached_clients_mut() {
            if client.is_view_held(pane_id) {
                let current_scroll_offset = client.get_scroll_offset(pane_id);
                client.set_scroll_offset(
                    pane_id,
                    (current_scroll_offset + pushed_line_count).min(retained_line_count_after),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
