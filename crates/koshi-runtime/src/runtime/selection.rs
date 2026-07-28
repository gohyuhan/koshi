//! Highlight upkeep: dropping a client's highlights when the text under them
//! is gone, when its own input reaches the pane's program, and when the pane
//! swaps screens.
//!
//! The gesture that makes a highlight — press, drag, release — is the viewer's:
//! it resolves the pointer to lines and columns against the frame it painted
//! and asks for the highlight through [`VisualCommand::SetSelection`]. What
//! lives here is the other half: the rules that end a highlight the session
//! itself can see coming, none of which the viewer has any way to notice.

use koshi_core::command::{ClearSelectionArgs, VisualCommand};
use koshi_core::ids::{ClientId, PaneId};

use crate::server::Server;

impl Server {
    /// Drop every highlight in `pane_id` whose lines have all been dropped from
    /// the pane's text — erased by the child (`CSI 3 J`) or evicted by the
    /// scrollback cap.
    ///
    /// Such a highlight can never draw again, yet it would keep holding its
    /// client's view against live output ([`Client::is_view_held`]) with
    /// nothing on screen to explain why; dropping it lets the view follow live
    /// output again. A highlight with any line still retained keeps what
    /// remains.
    ///
    /// The scroll offset stays. After the drop, `offset > 0` with no highlight
    /// is exactly the state of a client who scrolled up by hand, and it behaves
    /// the same way: the view stays where it is until the client scrolls down.
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
            let dead = client.selection(pane_id).is_some_and(|selection| {
                koshi_terminal::selection::order(selection.anchor, selection.cursor)
                    .end
                    .row
                    < first_row
            });
            if dead {
                client.clear_selection(pane_id);
            }
        }
    }

    /// Drop `client_id`'s highlight in `pane_id` once its input has reached the
    /// pane's child: the key or click belongs to the program running there, and
    /// visual mode ends. A client with no highlight there dispatches nothing.
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

    /// Drop every client's highlight in `pane_id`.
    ///
    /// Called when the pane switches between its primary and alternate screens.
    /// A row number counts the lines the pane pushed into scrollback, which the
    /// alternate screen does not have and does not share — so a highlight made
    /// on one screen names nothing on the other, and the text it was on is not
    /// displayed either way.
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
