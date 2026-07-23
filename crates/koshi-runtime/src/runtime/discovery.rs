//! Building the discovery overview from live session state.
//!
//! [`Server::build_overview`] answers an IPC `Discovery` request: it walks the
//! process's session — tabs in bar order, every pane record, every attached
//! client — and packs them into one [`SessionOverview`], the serializable
//! answer the CLI filters for whichever listing or inspect query it was
//! given. The dispatcher thread builds it, so every field reads the same
//! state a command would.

use std::collections::HashMap;

use koshi_core::discovery::{
    ClientInfo, PaneInfo, PaneState, SessionInfo, SessionOverview, TabInfo,
};
use koshi_core::ids::PaneId;
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_session::session::state::{Session, Tab};

use koshi_terminal::engine::TerminalEngine;

use crate::server::Server;

impl Server {
    /// Describe this process's running session as one [`SessionOverview`],
    /// or `None` when no session is running (the window between the last
    /// session ending and the process exiting).
    #[must_use]
    pub fn build_overview(&self) -> Option<SessionOverview> {
        // One process serves one session: genesis seeds exactly one and no
        // command creates another in-process.
        let session = self.sessions.values().next()?;

        let mut tabs: Vec<&Tab> = session.tabs.values().collect();
        tabs.sort_by_key(|tab| tab.index());
        let tab_infos = tabs
            .iter()
            .map(|tab| TabInfo {
                id: tab.id(),
                session_id: session.id,
                name: tab.name().to_string(),
                index: tab.index(),
                active_pane: tab.focus_mru().first().copied(),
                pane_count: tab.layout().leaf_panes().len(),
            })
            .collect();

        let panes = pane_infos(session, &tabs, &self.terminal_engines);

        let clients: Vec<ClientInfo> = session
            .clients
            .list_attached()
            .map(|client| ClientInfo {
                id: client.id(),
                session_id: session.id,
                attached_at: client.attached_at(),
                viewport_size: client.viewport(),
                active_tab: client.active_tab(),
                focused_pane: client.focused_pane(client.active_tab()),
                lock_state: client.lock_mode(),
            })
            .collect();

        Some(SessionOverview {
            session: SessionInfo {
                id: session.id,
                name: session.name.clone(),
                created_at: session.created_at,
                attached_clients: clients.iter().map(|client| client.id).collect(),
                pane_count: session.panes.len(),
            },
            tabs: tab_infos,
            panes,
            clients,
        })
    }
}

/// One [`PaneInfo`] row per registered pane, in the tab-bar order of the tabs
/// holding them and layout order within each tab. A pane whose lifecycle is
/// `Removed` has already left every layout tree, so it produces no row. The
/// title is the pane terminal's OSC 0/1/2 title, once the child has set one.
fn pane_infos(
    session: &Session,
    tabs: &[&Tab],
    engines: &HashMap<PaneId, TerminalEngine>,
) -> Vec<PaneInfo> {
    let mut infos = Vec::with_capacity(session.panes.len());
    for tab in tabs {
        for pane_id in tab.layout().leaf_panes() {
            let Some(record) = session.panes.get(pane_id) else {
                continue;
            };
            let state = match record.lifecycle() {
                PaneLifecycle::Spawning => PaneState::Spawning,
                PaneLifecycle::Running => PaneState::Running,
                PaneLifecycle::Exited { code, .. } => PaneState::Exited { code: *code },
                PaneLifecycle::Closing { .. } => PaneState::Closing,
                PaneLifecycle::Removed => continue,
            };
            let focused_by_clients = session
                .clients
                .list_attached()
                .filter(|client| client.focused_pane(client.active_tab()) == Some(pane_id))
                .map(|client| client.id())
                .collect();
            infos.push(PaneInfo {
                id: pane_id,
                tab_id: tab.id(),
                session_id: session.id,
                title: engines
                    .get(&pane_id)
                    .and_then(|engine| engine.state().title().map(str::to_owned)),
                cwd: record.cwd.clone(),
                command: record.command.as_ref().map(spawn_argv),
                state,
                focused_by_clients,
            });
        }
    }
    infos
}

/// A spawn spec as the argv discovery reports: the program first, then its
/// arguments.
fn spawn_argv(spec: &koshi_core::process::SpawnSpec) -> Vec<String> {
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(spec.program.to_string_lossy().into_owned());
    argv.extend(spec.args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests;
