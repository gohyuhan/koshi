//! Pane state ops: the pure session-state applications behind
//! `Command::NewPane`.
//!
//! Like [`crate::session::tab_ops`], this layer edits state and drafts events
//! only — it never spawns a process or touches a terminal. The runtime builds
//! and validates the split, spawns the pane's process, and only then calls
//! [`commit_new_pane`] to apply it.

use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused, TabFocused};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::process::SpawnSpec;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;

use crate::session::state::Session;

/// What to record on a freshly created pane: the working directory it launched
/// in and the spawn request behind it. Both land on the new pane's
/// [`PaneRecord`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPaneSpec {
    /// The directory the pane launched in. `None` when none was resolved.
    pub cwd: Option<PathBuf>,
    /// The spawn request the pane launched with. `None` when the caller named
    /// no command and the default shell ran.
    pub command: Option<SpawnSpec>,
}

/// Register `pane_id` in the session's pane registry as `Running`, carrying
/// `spec`'s cwd and command and stamped `created_at`. An id already in the
/// registry keeps its existing record.
pub(crate) fn register_running_pane(
    session: &mut Session,
    pane_id: PaneId,
    spec: NewPaneSpec,
    created_at: SystemTime,
) {
    let mut record = PaneRecord::new(pane_id, created_at);
    record.cwd = spec.cwd;
    record.command = spec.command;
    let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    let _ = session.panes.insert(record);
}

/// Apply an already-built, already-spawned layout edit to `tab_id`: switch the
/// focused client onto the tab (if it is not already there), register the new
/// pane as `Running`, swap in `candidate` as the tab's layout — dropping the zoom
/// that would have hidden the new pane, so it lands visible — and focus the new
/// pane for `focus_client` when one is given and still attached.
///
/// Whose zoom drops depends on who made the split: with a `focus_client`, only
/// that client's zoom of `tab_id`; with none, every attached client's zoom of
/// `tab_id`. A `focus_client` that is no longer attached counts as none.
///
/// The caller (the runtime) has minted `new_pane_id`, built `candidate` with
/// [`koshi_layout::edit::split_leaf`] or [`koshi_layout::edit::add_to_stack`],
/// preflighted its fit against the sizing viewport, and spawned the child under
/// `new_pane_id`.
///
/// This is the single place a new pane's session state is committed: no session
/// field is written for `NewPane` outside this op. `spec` carries the cwd and
/// command recorded on the new pane; `created_at` is the caller's timestamp,
/// never read from the clock here.
///
/// Returns the focused client's *previous* tab when this op switched it onto
/// `tab_id` (so the caller can reflow the tab it left), and the events to emit —
/// [`Event::TabFocused`] (only when a client was switched), then
/// [`Event::PaneCreated`], [`Event::LayoutChanged`], and — only when
/// `focus_client` applies — [`Event::PaneFocused`], in that order.
///
/// An unknown `tab_id` is a no-op with no events: nothing is registered and
/// nothing is emitted.
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
    if !session.tabs.contains_key(&tab_id) {
        return (None, Vec::new());
    }

    // A `focus_client` that is not attached resolves to `None`: no tab switch,
    // no focus-MRU record, and no `PaneFocused` event.
    let focus = focus_client.filter(|client_id| session.clients.get(*client_id).is_some());

    let mut events = Vec::new();

    // Switch the focused client onto the tab when it is not already viewing it,
    // and record the tab it left.
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

    register_running_pane(session, new_pane_id, spec, created_at);

    // Swap in the pre-built tree; record the new pane in the tab's focus
    // history when a client focuses it.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_layout(candidate);
        if focus.is_some() {
            tab.record_focus_mru(new_pane_id);
        }
    }

    // Drop the zoom that would hide the new pane, then focus it:
    //
    // - **With a `focus_client`**: that client's zoom of `tab_id` drops and it
    //   focuses the new pane. Every other client keeps its zoom and its focus.
    // - **With none**: every attached client's zoom of `tab_id` drops, and no
    //   client's focus moves.
    let mut prior_pane = None;
    match focus {
        Some(client_id) => {
            if let Some(client) = session.clients.get_mut(client_id) {
                client.clear_zoom(tab_id);
                prior_pane = client.update_focused_pane(tab_id, new_pane_id);
            }
        }
        None => {
            for client in session.clients.list_attached_mut() {
                client.clear_zoom(tab_id);
            }
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
