//! Pane state ops: the pure session-state applications behind
//! `Command::NewPane`.
//!
//! Like [`crate::session::tab_ops`], this layer edits state and drafts events
//! only — it never spawns a process or touches a terminal. The runtime builds
//! and validates the split, spawns the pane's process, and only then calls
//! [`commit_new_pane`] to apply it — so a failed spawn never mutates the
//! session, and the pane exists only once its process is live.

use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused, TabFocused};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::process::SpawnSpec;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;

use crate::session::state::Session;

/// What to record on a freshly created pane: the spawn request (working
/// directory and command) the PTY (pseudo-terminal — the OS handle a shell
/// process runs its input/output through) layer later honors. Bundled so the
/// requested program and cwd are never silently dropped — the new pane's
/// record is self-describing for restore and respawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPaneSpec {
    /// Working directory; `None` inherits.
    pub cwd: Option<PathBuf>,
    /// Command to run; `None` launches the default shell.
    pub command: Option<SpawnSpec>,
}

/// Apply an already-built, already-spawned layout edit to `tab_id`: switch the
/// focused client onto the tab (if it is not already there), register the new
/// pane as `Running`, swap in `candidate` as the tab's layout — dropping the zoom
/// that would have hidden the new pane, so it lands visible — and focus the new
/// pane for `focus_client` when one is given and still attached.
///
/// Whose zoom drops depends on who made the split: with a `focus_client`, that
/// one client's (another client's zoom is not disturbed by someone else's split);
/// with none, every zoom of the tab, since nobody owns the edit and a pane added
/// beneath a zoom would be seen by no one and focused by no one.
///
/// The caller (the runtime) has minted `new_pane_id`, built `candidate` with
/// [`koshi_layout::edit::split_leaf`] or [`koshi_layout::edit::add_to_stack`],
/// preflighted its fit against the sizing viewport, and spawned the child under
/// `new_pane_id` — so this only commits state (and, because the child is already
/// live, registers the pane `Running`).
/// This is the single place a new pane's session state is committed: no session
/// field is written for `NewPane` outside this op. `spec` carries the cwd and
/// command recorded on the new pane so a later restore or respawn can recover
/// the request; `created_at` is supplied by the caller so the timestamp crosses
/// the IPC boundary intact and tests stay deterministic.
///
/// Returns the focused client's *previous* tab when this op switched it onto
/// `tab_id` (so the caller can reflow the tab it left), and the events to emit —
/// [`Event::TabFocused`] (only when a client was switched), then
/// [`Event::PaneCreated`], [`Event::LayoutChanged`], and — only when
/// `focus_client` applies — [`Event::PaneFocused`], in that order.
#[must_use]
pub fn commit_new_pane(
    session: &mut Session,
    new_pane_id: PaneId,
    tab_id: TabId,
    candidate: LayoutNode,
    focus_client: Option<ClientId>,
    spec: NewPaneSpec,
    created_at: SystemTime,
) -> (Option<TabId>, Vec<Event>) {
    // Only a still-attached client can be focused. Resolving it once here keeps
    // the tab switch, focus-MRU record, and `PaneFocused` event in agreement: a
    // stale id focuses nothing, exactly like `None`.
    let focus = focus_client.filter(|client_id| session.clients.get(*client_id).is_some());

    let mut events = Vec::new();

    // Switch the focused client onto the tab when it is not already viewing it,
    // so it sees the new pane; remember the tab it left for the caller to reflow.
    let mut previous_tab = None;
    if let Some(client_id) = focus {
        if let Some(client) = session.clients.get_mut(client_id) {
            if client.active_tab() != tab_id {
                let prior_tab = client.active_tab();
                previous_tab = Some(prior_tab);
                client.update_active_tab(tab_id);
                events.push(Event::TabFocused(TabFocused {
                    client_id,
                    tab_id,
                    prior_tab,
                }));
            }
        }
    }

    // Register the new pane. Its process is already live, so it enters `Running`
    // straight away; the spawn request is recorded so a later restore or respawn
    // can recover it.
    let mut record = PaneRecord::new(new_pane_id, created_at);
    record.cwd = spec.cwd;
    record.command = spec.command;
    let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    let _ = session.panes.insert(record);

    // Swap in the pre-built tree and record focus history.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_layout(candidate);
        if focus.is_some() {
            tab.record_focus_mru(new_pane_id);
        }
    }

    // A new pane must be seen by somebody, so a zoom that would hide it is
    // dropped — but only the zoom of whoever is responsible for the split.
    //
    // - **A client made the split** (a keybinding, the in-session CLI, or an
    //   explicit `--client`): that client's zoom drops, so the new pane lands
    //   visible in the tiled layout it was sized against. Every other client
    //   keeps its zoom; one client splitting is not a reason to disturb another
    //   client's view.
    // - **No client made it** (an external caller naming only a tab): nobody owns
    //   the edit, so there is no one client to un-zoom — and leaving every zoom
    //   in place would add the pane underneath them, where no zoomed viewer would
    //   ever see it and none of them focuses it. Every zoom of this tab drops
    //   instead, so the pane the caller asked for actually appears.
    match focus {
        Some(client_id) => {
            if let Some(client) = session.clients.get_mut(client_id) {
                client.clear_zoom(tab_id);
            }
        }
        None => {
            for client in session.clients.list_attached_mut() {
                client.clear_zoom(tab_id);
            }
        }
    }

    // Auto-focus the resolved client on the new pane.
    let mut prior_pane = None;
    if let Some(client_id) = focus {
        if let Some(client) = session.clients.get_mut(client_id) {
            prior_pane = client.update_focused_pane(tab_id, new_pane_id);
        }
    }

    events.push(Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id,
    }));
    events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
    if let Some(client_id) = focus {
        events.push(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id,
            pane_id: new_pane_id,
            prior_pane,
        }));
    }
    (previous_tab, events)
}

#[cfg(test)]
mod tests;
