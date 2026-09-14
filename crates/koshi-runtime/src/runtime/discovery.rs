//! Building the discovery overview from live session state.
//!
//! [`Server::build_overview`] answers an IPC `Discovery` request: it walks the
//! process's session — tabs in bar order, every pane a tab's layout holds,
//! every attached client — and packs them into one [`SessionOverview`], the
//! serializable answer the CLI filters for whichever listing or inspect query
//! it was given. The dispatcher thread builds it, from the same state a
//! command reads.

use std::collections::HashMap;

use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle as DiscoveryPaneLifecycle, SessionDiscovery,
    SessionOverview, TabDiscovery,
};
use koshi_core::ids::PaneId;
use koshi_core::process::SpawnSpec;
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_session::session::state::{Session, Tab};

use koshi_terminal::engine::TerminalEngine;

use crate::server::Server;

impl Server {
    /// Describe this process's running session as one [`SessionOverview`],
    /// or `None` when no session is running (the window between the last
    /// session ending and the process exiting).
    ///
    /// `session.pane_count` counts every pane the session registry holds, and
    /// each `tabs[column_index].pane_count` every leaf of that tab's layout.
    /// Both keep counting a pane that `pane_discoveries` gives no row.
    #[must_use]
    pub fn build_overview(&self) -> Option<SessionOverview> {
        let session = self.get_sole_session()?;

        let mut tabs: Vec<&Tab> = session.tabs.values().collect();
        tabs.sort_by_key(|tab| tab.get_tab_index());
        let tab_discoveries = tabs
            .iter()
            .map(|tab| TabDiscovery {
                tab_id: tab.get_tab_id(),
                session_id: session.session_id,
                tab_name: tab.get_tab_name().to_string(),
                tab_index: tab.get_tab_index(),
                active_pane_id: tab.list_focus_mru().first().copied(),
                pane_count: tab.get_layout_tree().list_leaf_pane_ids().len(),
            })
            .collect();

        let pane_discoveries =
            list_pane_discoveries(session, &tabs, &self.terminal_engine_by_pane_id);

        let client_discoveries: Vec<ClientDiscovery> = session
            .clients
            .list_attached_clients()
            .map(|client| ClientDiscovery {
                client_id: client.get_client_id(),
                session_id: session.session_id,
                attached_at: client.get_attached_at(),
                viewport_size: client.get_viewport_size(),
                active_tab_id: client.get_active_tab(),
                focused_pane_id: client.get_focused_pane(client.get_active_tab()),
                lock_mode: client.get_lock_mode(),
                origin: Some(client.get_origin()),
                pane_area: client.get_reported_pane_area(),
            })
            .collect();

        Some(SessionOverview {
            session: SessionDiscovery {
                session_id: session.session_id,
                session_name: session.session_name.clone(),
                created_at: session.created_at,
                attached_client_ids: client_discoveries
                    .iter()
                    .map(|client| client.client_id)
                    .collect(),
                pane_count: session.panes.pane_record_count(),
            },
            tabs: tab_discoveries,
            panes: pane_discoveries,
            clients: client_discoveries,
        })
    }
}

/// One [`PaneDiscovery`] row per pane that a tab's layout holds and the session
/// registry knows, in the tab-bar order of the tabs holding them and layout
/// order within each tab. A pane whose lifecycle is `Removed` gets no row, and
/// neither does a layout leaf the registry does not hold. The title is the pane
/// terminal's OSC 0/1/2 title, once the child has set one.
fn list_pane_discoveries(
    session: &Session,
    tabs: &[&Tab],
    terminal_state_by_pane_id: &HashMap<PaneId, TerminalEngine>,
) -> Vec<PaneDiscovery> {
    let mut pane_discoveries = Vec::with_capacity(session.panes.pane_record_count());
    for tab in tabs {
        for pane_id in tab.get_layout_tree().list_leaf_pane_ids() {
            let Some(pane_record) = session.panes.get_pane_record_by_id(pane_id) else {
                continue;
            };
            let pane_lifecycle = match pane_record.get_lifecycle() {
                PaneLifecycle::Spawning => DiscoveryPaneLifecycle::Spawning,
                PaneLifecycle::Running => DiscoveryPaneLifecycle::Running,
                PaneLifecycle::Exited { exit_code, .. } => DiscoveryPaneLifecycle::Exited {
                    exit_code: *exit_code,
                },
                PaneLifecycle::Closing { .. } => DiscoveryPaneLifecycle::Closing,
                PaneLifecycle::Removed => continue,
            };
            let focused_by_client_ids = session
                .clients
                .list_attached_clients()
                .filter(|client| client.get_focused_pane(client.get_active_tab()) == Some(pane_id))
                .map(|client| client.get_client_id())
                .collect();
            pane_discoveries.push(PaneDiscovery {
                pane_id,
                tab_id: tab.get_tab_id(),
                session_id: session.session_id,
                pane_title: terminal_state_by_pane_id
                    .get(&pane_id)
                    .and_then(|engine| engine.get_terminal_state().get_title().map(str::to_owned)),
                working_directory: pane_record.working_directory.clone(),
                command_argv: pane_record.spawn_spec.as_ref().map(spawn_argv),
                lifecycle: pane_lifecycle,
                focused_by_client_ids,
            });
        }
    }
    pane_discoveries
}

/// A spawn spec as the argv discovery reports: the program first, then its
/// arguments.
fn spawn_argv(spec: &SpawnSpec) -> Vec<String> {
    let mut argv = Vec::with_capacity(spec.arguments.len() + 1);
    argv.push(spec.program.to_string_lossy().into_owned());
    argv.extend(spec.arguments.iter().cloned());
    argv
}

#[cfg(test)]
mod tests;
