//! Pane state ops: the pure session-state applications behind
//! `Command::NewPane`.
//!
//! Like [`crate::session::tab_ops`], this layer edits state and drafts events
//! only — it never spawns a process or touches a terminal. The runtime builds
//! and validates the split or the floating pane, spawns the pane's process,
//! and only then calls [`commit_new_pane`] or [`commit_new_floating_pane`] to
//! apply it.

use std::path::PathBuf;

use koshi_core::event::{Event, LayoutChanged, PaneCreated, TabFocused};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::process::SpawnSpec;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneRecord;

use crate::client::FloatingPanePosition;
use crate::error::FloatingSetError;
use crate::session::focus::{apply_tiled_pane_focus, build_pane_focused_event};
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

/// Register `pane_id`, whose child is already spawned, in the session's pane
/// registry, carrying `new_pane_spec`'s working directory and spawn
/// specification. A pane id already in the registry keeps its existing record.
pub(crate) fn register_spawned_pane(
    session: &mut Session,
    pane_id: PaneId,
    new_pane_spec: NewPaneSpec,
) {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record.working_directory = new_pane_spec.working_directory;
    pane_record.spawn_spec = new_pane_spec.spawn_spec;
    let _ = session.panes.register_pane_record(pane_record);
}

/// Apply an already-built, already-spawned layout edit to `tab_id`: switch the
/// designated client onto the tab (if it is not already there), register the new
/// pane, swap in `candidate_layout_tree` as the tab's layout — dropping the zoom
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
/// The tab is solved again with the new tree ([`Session::update_tab_layout`]),
/// and, when the designated client switched onto it, `tab_id` and the tab the
/// client left are solved again at their new tab sizes
/// ([`Session::update_tab_solve`]).
///
/// Returns the designated client's *previous* tab when this op switched it onto
/// `tab_id`, else `None`, and the events to emit —
/// [`Event::TabFocused`] (only when a client was switched), then
/// [`Event::PaneCreated`], [`Event::LayoutChanged`] and the events of the new
/// tree's solve, then — only when `focus_client_id` applies — one
/// [`Event::PaneMinimizedChanged`] per floating pane the client showed and
/// [`Event::PaneFocused`] ([`apply_tiled_pane_focus`]), then the events of
/// the tab size solves, in that order. The `PaneFocused` names as
/// `previous_pane_id` the floating pane that held the client's input, else the
/// pane it focused in `tab_id` before.
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

    register_spawned_pane(session, new_pane_id, new_pane_spec);

    // Swap in the pre-built tree; record the new pane in the tab's focus
    // history when a client focuses it.
    emitted_events.push(Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id: Some(tab_id),
    }));
    emitted_events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
    emitted_events.extend(session.update_tab_layout(tab_id, candidate_layout_tree));
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
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
    match focused_client_id {
        Some(client_id) => {
            if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
                client.clear_zoom(tab_id);
                emitted_events.extend(apply_tiled_pane_focus(client, tab_id, new_pane_id));
            }
        }
        None => {
            for client in session.clients.list_attached_clients_mut() {
                client.clear_zoom(tab_id);
            }
        }
    }
    if let Some(previous_tab_id) = previous_tab_id {
        emitted_events.extend(session.update_tab_solve(tab_id));
        emitted_events.extend(session.update_tab_solve(previous_tab_id));
    }
    (previous_tab_id, emitted_events)
}

/// Apply an already-spawned floating pane: append `floating_member` to the
/// session's floating set with a minimized view of it for every attached
/// client ([`Session::add_floating_member`]), register its pane with
/// `new_pane_spec`'s working directory and spawn specification, and show it
/// for `acting_view`'s client.
///
/// `acting_view`'s client takes `acting_view`'s position as its view of the
/// pane. When that client can draw floating panes
/// ([`Session::can_client_draw_floating_panes`]), it shows the new pane above
/// its other floating panes and focuses it: the new pane takes that client's
/// input. When it cannot, the pane stays minimized for it and its input stays
/// where it was. Every other client holds the pane minimized at the default
/// placement, and its focus does not move. No tab changes.
///
/// The caller (the runtime) has minted `floating_member.pane_id`: no tab
/// holds it and the pane registry has no record of it. A registry record
/// already under that id stays as it is.
///
/// Returns the events to emit: [`Event::PaneCreated`] with `tab_id: None`,
/// then, when `acting_view`'s client shows the pane, [`Event::PaneFocused`]
/// with `tab_id: None` and the pane that held its input before as
/// `previous_pane_id`.
///
/// # Errors
///
/// The [`FloatingSetError`] that [`Session::add_floating_member`] returns
/// when the set refuses the member. The session does not change.
pub fn commit_new_floating_pane(
    session: &mut Session,
    floating_member: FloatingMember,
    acting_view: Option<(ClientId, FloatingPanePosition)>,
    new_pane_spec: NewPaneSpec,
) -> Result<Vec<Event>, FloatingSetError> {
    let new_pane_id = floating_member.pane_id;
    session.add_floating_member(floating_member, acting_view)?;
    register_spawned_pane(session, new_pane_id, new_pane_spec);
    let mut emitted_events = vec![Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id: None,
    })];
    if let Some((acting_client_id, _)) = acting_view {
        emitted_events.extend(show_new_floating_pane(
            session,
            acting_client_id,
            new_pane_id,
        ));
    }
    Ok(emitted_events)
}

/// Show and focus the floating pane `pane_id`, which `client_id` holds
/// minimized, when that client can draw floating panes
/// ([`Session::can_client_draw_floating_panes`]): it draws the pane above its
/// other floating panes, and the pane takes its input
/// ([`Client::focus_floating_pane`](crate::client::Client::focus_floating_pane)).
///
/// Returns the [`Event::PaneFocused`] of that input change
/// ([`build_pane_focused_event`]). A client that is not attached, or that
/// cannot draw floating panes, changes nothing and returns `None`.
pub(crate) fn show_new_floating_pane(
    session: &mut Session,
    client_id: ClientId,
    pane_id: PaneId,
) -> Option<Event> {
    let can_draw_floating_panes = session
        .clients
        .get_client_by_id(client_id)
        .is_some_and(|client| session.can_client_draw_floating_panes(client));
    if !can_draw_floating_panes {
        return None;
    }
    let client = session.clients.get_client_mut_by_id(client_id)?;
    let previous_focused_pane_id = client.get_active_focused_pane_id();
    let _ = client.focus_floating_pane(pane_id);
    build_pane_focused_event(client, previous_focused_pane_id)
}

#[cfg(test)]
mod tests;
