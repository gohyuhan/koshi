//! Building the layout dump from live session state.
//!
//! [`Server::build_session_layout`] answers an IPC `Layout` request. It walks
//! this process's session — tabs in bar order, and for each tab every client
//! viewing it — and packs the split trees, the rectangles those trees solve
//! to, and each client's focus into one [`SessionLayout`]. The dispatcher
//! thread builds it, from the same state a command reads.
//!
//! A tab solves once per viewing client, against that client's own layout
//! mode. Two clients on one tab, one tiled and one with a pane fullscreen,
//! give that tab two sets of rectangles in the same turn. The size is shared
//! across those clients — the smallest viewing terminal on each axis, minus
//! the two chrome rows — so an 80x24 and a 120x40 client both solve against
//! 80x22.

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
    /// `tab` narrows the answer to one tab; absent, every tab is described.
    #[must_use]
    pub fn build_session_layout(&self, tab: Option<TabId>) -> Option<SessionLayout> {
        // One process serves one session: genesis seeds exactly one and no
        // command creates another in-process.
        let session = self.sessions.values().next()?;
        let sizing = self.pane_sizing();

        let mut wanted_tabs: Vec<&Tab> = session
            .tabs
            .values()
            .filter(|candidate| tab.is_none_or(|wanted| candidate.id() == wanted))
            .collect();
        wanted_tabs.sort_by_key(|tab| tab.index());

        let tabs = wanted_tabs
            .into_iter()
            .map(|tab| {
                // One size per tab, not per client: `tab_viewport` is the
                // smallest pane area on each axis among the clients viewing
                // the tab that report one, and it is `None` when no such
                // client views the tab.
                let solved = match session.tab_viewport(tab.id()) {
                    None => Vec::new(),
                    Some(viewport) => session
                        .clients
                        .list_attached()
                        .filter(|client| client.active_tab() == tab.id())
                        .map(|client| {
                            let mode = client.layout_mode(tab.id());
                            let solve =
                                crate::runtime::snapshot::solve_tab(tab, mode, viewport, sizing);
                            SolvedTab {
                                client: client.id(),
                                viewport,
                                mode,
                                panes: solve
                                    .panes
                                    .iter()
                                    .map(|&(id, rect)| SolvedPane { id, rect })
                                    .collect(),
                                suppressed: solve.suppressed,
                                all_suppressed: solve.all_suppressed,
                                stack_headers: solve.stack_headers,
                            }
                        })
                        .collect(),
                };
                TabLayout {
                    id: tab.id(),
                    name: tab.name().to_string(),
                    index: tab.index(),
                    tree: tab.layout().clone(),
                    solved,
                }
            })
            .collect();

        let clients = session
            .clients
            .list_attached()
            .map(|client| ClientFocus {
                id: client.id(),
                active_tab: client.active_tab(),
                focused_pane: client.focused_pane(client.active_tab()),
            })
            .collect();

        Some(SessionLayout {
            id: session.id,
            name: session.name.clone(),
            tabs,
            clients,
        })
    }
}
