//! Highlight upkeep: dropping a client's highlights when the text under them
//! is gone, when its own input reaches the pane's program, and when the pane
//! swaps screens.
//!
//! A viewer makes a highlight from its own gesture — press, drag, release: it
//! resolves the pointer to lines and columns against the frame it painted and
//! asks for the highlight through [`VisualCommand::SetSelection`]. The rules
//! that end a highlight live here.

use koshi_core::command::{ClearSelectionArgs, VisualCommand};
use koshi_core::ids::{ClientId, PaneId};

use crate::server::Server;

impl Server {
    /// Drop every highlight in `pane_id` whose lines have all been dropped from
    /// the pane's text — erased by the child (`CSI 3 J`) or evicted by the
    /// scrollback cap. A highlight with any line still retained is left as it
    /// is.
    ///
    /// A dropped highlight stops holding its client's view
    /// ([`Client::is_view_held`]). The scroll offset is left as it is, so a
    /// client the drop leaves at `offset > 0` stays scrolled up until it
    /// scrolls down.
    ///
    /// A `pane_id` with no terminal engine, and a `pane_id` in no session,
    /// change nothing.
    ///
    /// [`Client::is_view_held`]: koshi_session::client::Client::is_view_held
    pub(crate) fn drop_evicted_selections(&mut self, pane_id: PaneId) {
        let Some(engine) = self.terminal_engines.get(&pane_id) else {
            return;
        };
        let first_row = engine.state().text_view().first_row();
        let Some(session) = self.session_for_pane_mut(pane_id) else {
            return;
        };
        for client in session.clients.list_attached_mut() {
            let all_rows_gone = client.selection(pane_id).is_some_and(|selection| {
                koshi_terminal::selection::order(selection.anchor, selection.cursor)
                    .end
                    .row
                    < first_row
            });
            if all_rows_gone {
                client.clear_selection(pane_id);
            }
        }
    }

    /// Drop `client_id`'s highlight in `pane_id` through
    /// [`VisualCommand::ClearSelection`] on the command pipeline, which
    /// publishes one
    /// [`Event::SelectionChanged`](koshi_core::event::Event::SelectionChanged).
    /// Called once the client's key or click has reached the pane's child.
    ///
    /// An unknown client, and a client with no highlight in `pane_id`,
    /// dispatch nothing.
    pub(crate) fn clear_selection_on_pane_input(&mut self, client_id: ClientId, pane_id: PaneId) {
        let selecting = self
            .client_mut(client_id)
            .is_some_and(|client| client.selection(pane_id).is_some());
        if selecting {
            self.dispatch_visual(
                client_id,
                VisualCommand::ClearSelection(ClearSelectionArgs { pane: pane_id }),
            );
        }
    }

    /// Drop every attached client's highlight in `pane_id`. Called when the
    /// pane switches between its primary and alternate screens. A `pane_id` in
    /// no session changes nothing.
    pub(crate) fn clear_pane_selections(&mut self, pane_id: PaneId) {
        let Some(session) = self.session_for_pane_mut(pane_id) else {
            return;
        };
        for client in session.clients.list_attached_mut() {
            client.clear_selection(pane_id);
        }
    }
}

#[cfg(test)]
mod tests;
