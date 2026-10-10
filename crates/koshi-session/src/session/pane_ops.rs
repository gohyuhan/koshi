//! Pane state ops: the pure session-state applications behind
//! `Command::NewPane`.
//!
//! Like [`crate::session::tab_ops`], this layer edits state and drafts events
//! only — it never spawns a process or touches a terminal. The runtime builds
//! and validates the split or the floating pane, spawns the pane's process,
//! and only then calls [`commit_new_pane`] or [`commit_new_floating_pane`] to
//! apply it.

use std::path::PathBuf;

use koshi_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused, TabFocused};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::process::SpawnSpec;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;

use crate::client::{FloatingPanePosition, FloatingPaneView};
use crate::error::FloatingSetError;
use crate::session::focus::build_pane_focused_event;
use crate::session::state::{FloatingMember, Session};

/// What to record on a freshly created pane: the working directory it launched
/// in and the spawn specification behind it. Both land on the new pane's
/// [`PaneRecord`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPaneSpec {
    /// The directory the pane launched in. `None` when none was resolved.
    pub working_directory: Option<PathBuf>,
    /// The spawn specification the pane launched with. `None` when the caller
    /// named no command and the default shell ran.
    pub spawn_spec: Option<SpawnSpec>,
}

/// Register `pane_id` in the session's pane registry as `Running`, carrying
/// `new_pane_spec`'s working directory and spawn specification. A pane id already
/// in the registry keeps its existing record.
pub(crate) fn register_running_pane(
    session: &mut Session,
    pane_id: PaneId,
    new_pane_spec: NewPaneSpec,
) {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record.working_directory = new_pane_spec.working_directory;
    pane_record.spawn_spec = new_pane_spec.spawn_spec;
    let _ = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    let _ = session.panes.register_pane_record(pane_record);
}

/// Apply an already-built, already-spawned layout edit to `tab_id`: switch the
/// designated client onto the tab (if it is not already there), register the new
/// pane as `Running`, swap in `candidate_layout_tree` as the tab's layout — dropping the zoom
/// that would have hidden the new pane, so it lands visible — and focus the new
/// pane for `focus_client_id` when one is given and still attached. The new
/// pane then takes that client's input: no floating pane stays focused for it.
///
/// Whose zoom drops depends on who made the split: with a `focus_client_id`, only
/// that client's zoom of `tab_id`; with none, every attached client's zoom of
/// `tab_id`. A `focus_client_id` that is no longer attached counts as none.
///
/// The caller (the runtime) has minted `new_pane_id`, built `candidate_layout_tree` with
/// [`koshi_layout::edit::split_leaf`] or [`koshi_layout::edit::add_pane_to_stack`],
/// preflighted its fit against the tab size, and spawned the child under
/// `new_pane_id`.
///
/// This is the single place a new pane's session state is committed: no session
/// field is written for `NewPane` outside this op. `new_pane_spec` carries the working
/// directory and spawn specification recorded on the new pane.
///
/// Returns the designated client's *previous* tab when this op switched it onto
/// `tab_id`, else `None`, and the events to emit —
/// [`Event::TabFocused`] (only when a client was switched), then
/// [`Event::PaneCreated`], [`Event::LayoutChanged`], and — only when
/// `focus_client_id` applies — [`Event::PaneFocused`], in that order. The
/// `PaneFocused` names as `previous_pane_id` the floating pane that held the
/// client's input, else the pane it focused in `tab_id` before.
///
/// An unknown `tab_id` is a no-op with no events: nothing is registered and
/// nothing is emitted.
#[must_use]
pub fn commit_new_pane(
    session: &mut Session,
    new_pane_id: PaneId,
    tab_id: TabId,
    candidate_layout_tree: LayoutNode,
    focus_client_id: Option<ClientId>,
    new_pane_spec: NewPaneSpec,
) -> (Option<TabId>, Vec<Event>) {
    if !session.tabs.contains_key(&tab_id) {
        return (None, Vec::new());
    }

    // A `focus_client_id` that is not attached resolves to `None`: no tab switch,
    // no focus-MRU record, and no `PaneFocused` event.
    let focused_client_id =
        focus_client_id.filter(|client_id| session.clients.get_client_by_id(*client_id).is_some());

    let mut emitted_events = Vec::new();

    // Switch the focused client onto the tab when it is not already viewing it,
    // and record the tab it left.
    let mut previous_tab_id = None;
    if let Some(client_id) = focused_client_id {
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            if client.get_active_tab_id() != tab_id {
                let client_previous_tab_id = client.get_active_tab_id();
                previous_tab_id = Some(client_previous_tab_id);
                client.update_active_tab_id(tab_id);
                emitted_events.push(Event::TabFocused(TabFocused {
                    client_id,
                    tab_id,
                    previous_tab_id: client_previous_tab_id,
                }));
            }
        }
    }

    register_running_pane(session, new_pane_id, new_pane_spec);

    // Swap in the pre-built tree; record the new pane in the tab's focus
    // history when a client focuses it.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_layout(candidate_layout_tree);
        if focused_client_id.is_some() {
            tab.record_focus_mru(new_pane_id);
        }
    }

    // Drop the zoom that would hide the new pane, then focus it:
    //
    // - **With a `focus_client_id`**: that client's zoom of `tab_id` drops and it
    //   focuses the new pane. The new pane takes that client's input: no
    //   floating pane stays focused for it. Every other client keeps its zoom
    //   and its focus.
    // - **With none**: every attached client's zoom of `tab_id` drops, and no
    //   client's focus moves.
    let mut previous_pane_id = None;
    match focused_client_id {
        Some(client_id) => {
            if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
                client.clear_zoom(tab_id);
                previous_pane_id = client.focus_tiled_pane(tab_id, new_pane_id);
            }
        }
        None => {
            for client in session.clients.list_attached_clients_mut() {
                client.clear_zoom(tab_id);
            }
        }
    }

    emitted_events.push(Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id: Some(tab_id),
    }));
    emitted_events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
    if let Some(client_id) = focused_client_id {
        emitted_events.push(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(tab_id),
            pane_id: new_pane_id,
            previous_pane_id,
        }));
    }
    (previous_tab_id, emitted_events)
}

/// Apply an already-spawned floating pane: append `floating_member` to the
/// session's floating set, register its pane as `Running` with `new_pane_spec`'s
/// working directory and spawn specification, store `designated_view`'s
/// position as that client's view of the pane, and focus the pane for that
/// client.
///
/// The designated client focuses the new pane and draws it on top of its other
/// floating panes: the new pane takes that client's input. A
/// [`FloatingPanePosition::Default`] position stores no view. A
/// `designated_view` naming a client that is not attached stores nothing and
/// focuses nothing. No other client's focus moves and no tab changes.
///
/// The caller (the runtime) has minted `floating_member.pane_id`: no tab
/// holds it and the pane registry has no record of it. A registry record
/// already under that id stays as it is.
///
/// Returns the events to emit: [`Event::PaneCreated`] with `tab_id: None`,
/// then, for an attached designated client, [`Event::PaneFocused`] with
/// `tab_id: None` and the pane that held its input before as
/// `previous_pane_id`.
///
/// # Errors
///
/// The [`FloatingSetError`] that [`FloatingSet::add_member`](crate::session::state::FloatingSet::add_member)
/// returns when the set refuses the member. The session does not change.
pub fn commit_new_floating_pane(
    session: &mut Session,
    floating_member: FloatingMember,
    designated_view: Option<(ClientId, FloatingPanePosition)>,
    new_pane_spec: NewPaneSpec,
) -> Result<Vec<Event>, FloatingSetError> {
    let new_pane_id = floating_member.pane_id;
    session.floating_set.add_member(floating_member)?;
    register_running_pane(session, new_pane_id, new_pane_spec);
    let mut emitted_events = vec![Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id: None,
    })];
    if let Some((client_id, position)) = designated_view {
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            client.set_floating_pane_view(
                new_pane_id,
                FloatingPaneView {
                    position,
                    is_minimized: false,
                },
            );
            let previous_focused_pane_id = client.get_active_focused_pane_id();
            let _ = client.focus_floating_pane(new_pane_id);
            emitted_events.extend(build_pane_focused_event(client, previous_focused_pane_id));
        }
    }
    Ok(emitted_events)
}

#[cfg(test)]
mod tests;
