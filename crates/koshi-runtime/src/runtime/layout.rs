//! Building the layout dump from live session state.
//!
//! [`Server::build_session_layout`] answers an IPC `Layout` request. It walks
//! this process's session — tabs in bar order, and for each tab every client
//! viewing it — and packs the split trees, the rectangles those trees solve
//! to, every floating pane with what each attached client draws for it, and
//! every attached client's focus into one [`SessionLayout`].
//!
//! Each viewing client gets the solve of its own view of the tab
//! ([`Session::compute_client_solve`]): its zoom, and in each stack the member
//! its focus history opens. Two clients on one tab, one tiled and one with a
//! pane fullscreen, give that tab two sets of rectangles in the same turn.
//! The size is shared across those clients: the per-axis smallest pane area
//! among the clients viewing the tab. A client that reported no pane area
//! contributes its own terminal minus the two chrome rows; a client that
//! reported
//! [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving)
//! contributes none. An 80x24 and a 120x40 client, neither reporting a pane
//! area, both solve against 80x22.
//!
//! [`Session::compute_client_solve`]: koshi_session::session::state::Session::compute_client_solve

use koshi_core::ids::TabId;
use koshi_ipc::layout::{
    ClientFocus, FloatingPaneClientView, FloatingPaneLayout, FloatingPaneVisibility, SessionLayout,
    SolvedPane, SolvedTab, TabLayout,
};
use koshi_session::client::Client;
use koshi_session::session::state::{FloatingMember, Session, Tab};

use crate::server::Server;

#[cfg(test)]
mod tests;

impl Server {
    /// Describe how this process's running session arranges its panes, as one
    /// [`SessionLayout`]. `None` when no session is running — the window
    /// between the last session ending and the process exiting.
    ///
    /// `requested_tab_id` narrows the described tabs to that one; absent, every
    /// tab is described. A `requested_tab_id` the session does not hold leaves
    /// `tabs` empty.
    /// `floating_panes` lists every floating pane and `clients` every attached
    /// client either way, narrowed or not.
    #[must_use]
    pub fn build_session_layout(&self, requested_tab_id: Option<TabId>) -> Option<SessionLayout> {
        let session = self.get_sole_session()?;

        let mut selected_tab_records: Vec<&Tab> = session
            .tabs
            .values()
            .filter(|candidate_tab| {
                requested_tab_id
                    .is_none_or(|requested_tab_id| candidate_tab.get_tab_id() == requested_tab_id)
            })
            .collect();
        selected_tab_records.sort_by_key(|tab_record| tab_record.get_tab_index());

        let tab_layouts = selected_tab_records
            .into_iter()
            .map(|tab_record| {
                // One size per tab, not per client: `get_tab_size` answers the
                // smallest pane area on each axis among the clients viewing
                // the tab that have one, and it is `None` when no such client
                // views the tab.
                let solved_tabs = match session.get_tab_size(tab_record.get_tab_id()) {
                    None => Vec::new(),
                    Some(tab_size) => session
                        .clients
                        .list_attached_clients()
                        .filter(|client_record| {
                            client_record.get_active_tab_id() == tab_record.get_tab_id()
                        })
                        .filter_map(|client_record| {
                            let layout_mode =
                                client_record.get_layout_mode(tab_record.get_tab_id());
                            let layout_solve = session
                                .compute_client_solve(client_record, tab_record.get_tab_id())?;
                            Some(SolvedTab {
                                client_id: client_record.get_client_id(),
                                viewport_size: tab_size,
                                layout_mode,
                                pane_rects: layout_solve
                                    .pane_rects
                                    .iter()
                                    .map(|&(pane_id, pane_rect)| SolvedPane {
                                        pane_id,
                                        outer_rect: pane_rect,
                                    })
                                    .collect(),
                                suppressed_pane_ids: layout_solve.suppressed_pane_ids.clone(),
                                is_every_pane_suppressed: layout_solve.is_every_pane_suppressed,
                                stack_headers: layout_solve.stack_headers.clone(),
                            })
                        })
                        .collect(),
                };
                TabLayout {
                    tab_id: tab_record.get_tab_id(),
                    tab_name: tab_record.get_tab_name().to_string(),
                    tab_index: tab_record.get_tab_index(),
                    layout_tree: tab_record.get_layout_tree().clone(),
                    solved_tabs,
                }
            })
            .collect();

        let floating_pane_layouts = session
            .floating_set
            .list_members()
            .iter()
            .map(|floating_member| FloatingPaneLayout {
                pane_id: floating_member.pane_id,
                desired_size: floating_member.desired_size,
                solved_size: floating_member.solved_size,
                client_views: session
                    .clients
                    .list_attached_clients()
                    .filter_map(|client_record| {
                        build_floating_pane_client_view(session, client_record, floating_member)
                    })
                    .collect(),
            })
            .collect();

        let client_focuses = session
            .clients
            .list_attached_clients()
            .map(|client_record| ClientFocus {
                client_id: client_record.get_client_id(),
                active_tab_id: client_record.get_active_tab_id(),
                focused_pane_id: client_record.get_active_focused_pane_id(),
            })
            .collect();

        Some(SessionLayout {
            session_id: session.session_id,
            session_name: session.session_name.clone(),
            tabs: tab_layouts,
            floating_panes: floating_pane_layouts,
            clients: client_focuses,
        })
    }
}

/// `client_record`'s view of the floating pane of `floating_member`, or `None`
/// when the client holds no view of it.
///
/// The visibility follows the client's shown floating panes
/// ([`Client::list_shown_floating_pane_ids`]):
/// - a shown pane is [`FloatingPaneVisibility::Drawn`] at the top-left cell of
///   the rectangle the session places it in for the client
///   ([`Session::find_client_floating_pane_rect`]), with its index in the shown
///   list as its layer;
/// - every other pane is [`FloatingPaneVisibility::Minimized`].
fn build_floating_pane_client_view(
    session: &Session,
    client_record: &Client,
    floating_member: &FloatingMember,
) -> Option<FloatingPaneClientView> {
    let pane_id = floating_member.pane_id;
    let floating_pane_view = client_record.get_floating_pane_view(pane_id)?;
    let shown_layer_index = client_record
        .list_shown_floating_pane_ids()
        .iter()
        .position(|&shown_pane_id| shown_pane_id == pane_id);
    let visibility = match shown_layer_index {
        None => FloatingPaneVisibility::Minimized,
        Some(layer_index) => match session.find_client_floating_pane_rect(client_record, pane_id) {
            Some(drawn_rect) => FloatingPaneVisibility::Drawn {
                top_left_cell: drawn_rect.origin,
                layer_index,
            },
            None => FloatingPaneVisibility::Minimized,
        },
    };
    Some(FloatingPaneClientView {
        client_id: client_record.get_client_id(),
        position: floating_pane_view.position,
        visibility,
    })
}
