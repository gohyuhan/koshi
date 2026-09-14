//! Building the layout dump from live session state.
//!
//! [`Server::build_session_layout`] answers an IPC `Layout` request. It walks
//! this process's session — tabs in bar order, and for each tab every client
//! viewing it — and packs the split trees, the rectangles those trees solve
//! to, and every attached client's focus into one [`SessionLayout`].
//!
//! A tab solves once per viewing client, against that client's own layout
//! mode. Two clients on one tab, one tiled and one with a pane fullscreen,
//! give that tab two sets of rectangles in the same turn. The size is shared
//! across those clients: the per-axis smallest pane area among the clients
//! viewing the tab. A client that reported no pane area contributes its own
//! terminal minus the two chrome rows; a client that reported
//! [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving)
//! contributes none. An 80x24 and a 120x40 client, neither reporting a pane
//! area, both solve against 80x22.

use koshi_core::ids::TabId;
use koshi_ipc::layout::{ClientFocus, SessionLayout, SolvedPane, SolvedTab, TabLayout};
use koshi_session::session::state::Tab;

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
    /// `clients` lists every attached client either way, narrowed or not.
    #[must_use]
    pub fn build_session_layout(&self, requested_tab_id: Option<TabId>) -> Option<SessionLayout> {
        let session = self.get_sole_session()?;
        let pane_sizing = self.get_pane_sizing();

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
                // One size per tab, not per client: `tab_viewport` is the
                // smallest pane area on each axis among the clients viewing
                // the tab that have one, and it is `None` when no such client
                // views the tab.
                let solved_tabs = match session.get_tab_viewport(tab_record.get_tab_id()) {
                    None => Vec::new(),
                    Some(effective_cell_size) => session
                        .clients
                        .list_attached_clients()
                        .filter(|client_record| {
                            client_record.get_active_tab() == tab_record.get_tab_id()
                        })
                        .map(|client_record| {
                            let layout_mode =
                                client_record.get_layout_mode(tab_record.get_tab_id());
                            let layout_solve = crate::runtime::snapshot::solve_tab_layout(
                                tab_record,
                                layout_mode,
                                effective_cell_size,
                                pane_sizing,
                            );
                            SolvedTab {
                                client_id: client_record.get_client_id(),
                                viewport_size: effective_cell_size,
                                layout_mode,
                                pane_rects: layout_solve
                                    .pane_rects
                                    .iter()
                                    .map(|&(pane_id, pane_rect)| SolvedPane {
                                        pane_id,
                                        outer_rect: pane_rect,
                                    })
                                    .collect(),
                                suppressed_pane_ids: layout_solve.suppressed_pane_ids,
                                is_every_pane_suppressed: layout_solve.is_all_panes_suppressed,
                                stack_headers: layout_solve.stack_headers,
                            }
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

        let client_focuses = session
            .clients
            .list_attached_clients()
            .map(|client_record| ClientFocus {
                client_id: client_record.get_client_id(),
                active_tab_id: client_record.get_active_tab(),
                focused_pane_id: client_record.get_focused_pane(client_record.get_active_tab()),
            })
            .collect();

        Some(SessionLayout {
            session_id: session.session_id,
            session_name: session.session_name.clone(),
            tabs: tab_layouts,
            clients: client_focuses,
        })
    }
}
