//! Session-state commit for checked cross-tab pane placement, for undock, and
//! for dock.
//!
//! The layout crate builds the proposed trees without touching live state.
//! This module installs those trees, repairs per-client references, and removes
//! an empty source tab without removing the transferred pane record. An undock
//! takes the pane's leaf out of its tab and adds the pane to the session's
//! floating panes. A dock takes a floating pane out of the floating panes and
//! installs a tab tree that holds it.

use std::collections::HashSet;

use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabFocused};
use koshi_core::geometry::Rect;
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::placement::CrossTabPlacement;
use koshi_layout::tree::LayoutNode;
use thiserror::Error;

use crate::client::{Client, FloatingPanePosition};
use crate::error::FloatingSetError;
use crate::session::cascade::{remove_pane_from_tab, repair_focus_after_pane_left};
use crate::session::focus::{apply_tiled_pane_focus, build_pane_focused_event};
use crate::session::pane_ops::show_new_floating_pane;
use crate::session::state::{FloatingMember, Session};
use crate::session::tab_ops::{close_and_refocus_tab, find_landing_pane};

/// A checked placement could not be installed because its tabs or source pane
/// no longer match the prepared trees.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlacementCommitError {
    /// The source and destination tabs are the same tab.
    #[error("source and destination tabs must differ")]
    SameTab,
    /// The source tab is absent.
    #[error("source tab is not present")]
    SourceTabNotFound,
    /// The destination tab is absent.
    #[error("destination tab is not present")]
    DestinationTabNotFound,
    /// The source pane is absent from the source tab's current tree.
    #[error("source pane is not present in the source tab")]
    SourcePaneNotFound,
    /// A dock source is not a floating pane of the session.
    #[error("source pane is not a floating pane")]
    FloatingSourceNotFound,
    /// The prepared destination tree is already owned by the source tree.
    #[error("prepared placement contains a pane in both trees")]
    PaneOwnershipConflict,
    /// The client that should follow the placed pane is absent.
    #[error("acting client is not attached")]
    ActingClientNotFound,
    /// An undock source is the only pane of the session's only tab.
    #[error("the last pane of the last tab cannot be undocked; a session needs one tiled pane")]
    LastTiledPane,
    /// The floating set refused the undocked pane.
    #[error(transparent)]
    FloatingMemberRefused(#[from] FloatingSetError),
}

/// Undock `floating_member.pane_id`, a leaf of `source_tab_id`: add
/// `floating_member` to the session's floating panes, take the pane's leaf
/// out of the tab, and show the pane for the acting client. Returns the
/// ordered events.
///
/// The pane's registry record, PTY and every per-pane view stay attached to
/// its id. The pane is never closed: no [`Event::PaneClosing`] and no
/// [`Event::PaneRemoved`] is emitted.
///
/// 1. The member joins the floating set as its newest member, with a
///    minimized view of it for every attached client: at `acting_view`'s
///    position for `acting_view`'s client, at the default placement for every
///    other client ([`Session::add_floating_member`]).
/// 2. The leaf leaves the tab as a closed pane's leaf does in
///    [`remove_pane_cascade`](crate::session::cascade::remove_pane_cascade):
///    the tab's tree collapses, normalizes and is solved again, every zoom on
///    the pane ends, and every client focused on the pane in the tab takes the
///    inheriting focus, ranked against the pane's rect in the tab's solve
///    before the undock. A tab left with no pane closes, and its viewers move
///    to the nearest surviving tab.
/// 3. When `acting_view`'s client can draw floating panes, it shows the pane
///    above its other floating panes and focuses it. When it cannot, the pane
///    stays minimized for it. No other client shows or focuses the pane.
///
/// Events: step 2's [`Event::LayoutChanged`] and focus events, or its
/// [`Event::TabClosed`] and the events of the viewers that move, then the
/// acting client's [`Event::PaneFocused`] with `tab_id: None`, naming the pane
/// that took its input after step 2 as `previous_pane_id`.
///
/// # Errors
///
/// Checked in this order, before any change:
/// - [`PlacementCommitError::SourceTabNotFound`]: the session holds no
///   `source_tab_id`;
/// - [`PlacementCommitError::SourcePaneNotFound`]: the pane is not a leaf of
///   that tab;
/// - [`PlacementCommitError::LastTiledPane`]: the pane is the only pane of the
///   session's only tab;
/// - [`PlacementCommitError::ActingClientNotFound`]: `acting_view`'s client is
///   not attached;
/// - [`PlacementCommitError::FloatingMemberRefused`]: the floating set
///   refuses the member ([`Session::add_floating_member`]).
pub fn commit_undock_placement(
    session: &mut Session,
    source_tab_id: TabId,
    floating_member: FloatingMember,
    acting_view: (ClientId, FloatingPanePosition),
) -> Result<Vec<Event>, PlacementCommitError> {
    let source_pane_id = floating_member.pane_id;
    let (acting_client_id, _) = acting_view;
    let source_leaf_pane_ids = session
        .tabs
        .get(&source_tab_id)
        .ok_or(PlacementCommitError::SourceTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids();
    if !source_leaf_pane_ids.contains(&source_pane_id) {
        return Err(PlacementCommitError::SourcePaneNotFound);
    }
    if session.tabs.len() == 1 && source_leaf_pane_ids.len() == 1 {
        return Err(PlacementCommitError::LastTiledPane);
    }
    if session.clients.get_client_by_id(acting_client_id).is_none() {
        return Err(PlacementCommitError::ActingClientNotFound);
    }
    session.add_floating_member(floating_member, Some(acting_view))?;

    let mut emitted_events = remove_pane_from_tab(session, source_tab_id, source_pane_id, None);
    emitted_events.extend(show_new_floating_pane(
        session,
        acting_client_id,
        source_pane_id,
    ));
    Ok(emitted_events)
}

/// Dock the floating pane `source_pane_id` into `destination_tab_id`: take it
/// out of the session's floating panes and install `destination_tree`, the
/// destination tab's tree with the pane placed in it
/// ([`place_floating_pane_in_tab`](koshi_layout::placement::place_floating_pane_in_tab)).
/// Returns the ordered events.
///
/// The pane's registry record, PTY and every per-pane view stay attached to
/// its id. The pane is never closed: no [`Event::PaneClosing`] and no
/// [`Event::PaneRemoved`] is emitted.
///
/// 1. The pane leaves the floating set and every client's floating view
///    ([`Session::remove_floating_member`]). A client whose floating focus
///    was on it focuses the newest floating pane it still shows, else its
///    input returns to its active tab's focused pane.
/// 2. `destination_tree` becomes the destination tab's tree, solved again
///    ([`Session::update_tab_layout`]).
/// 3. The acting client leaves any zoom in the destination tab. When the
///    destination is another tab, the acting client switches to it and, with
///    no pane focused there, focuses its landing pane ([`find_landing_pane`]).
///    The floating panes it shows stay shown. The destination tab, and the
///    tab the acting client left, are solved again at their tab sizes
///    ([`Session::update_tab_solve`]).
///
/// No client focuses the docked pane, no floating pane is minimized, and the
/// destination tab's focus history does not record the pane. With `logs`
/// still shown, docking `htop` leaves `logs` with the acting client's input;
/// with no floating pane left shown, the input returns to the pane the client
/// focused in its active tab.
///
/// Events: one [`Event::PaneFocused`] for each other client whose input the
/// pane took ([`build_pane_focused_event`]), then [`Event::LayoutChanged`] for
/// the destination tab and the events of its solve, then, for another tab,
/// the acting client's [`Event::TabFocused`] and, when the landing pane does
/// not take its input, an [`Event::PaneFocused`] with `previous_pane_id: None`
/// for the landing pane, then the events of the tab size solves, then the
/// acting client's [`Event::PaneFocused`] when its input moved.
///
/// # Errors
///
/// Checked in this order, before any change:
/// - [`PlacementCommitError::FloatingSourceNotFound`]: `source_pane_id` is
///   no floating pane of the session;
/// - [`PlacementCommitError::DestinationTabNotFound`]: the session holds no
///   `destination_tab_id`;
/// - [`PlacementCommitError::PaneOwnershipConflict`]: the tree
///   `destination_tree` repeats a pane, or does not hold exactly the
///   destination tab's leaves and `source_pane_id`;
/// - [`PlacementCommitError::ActingClientNotFound`]: `acting_client_id` is not
///   attached.
pub fn commit_dock_placement(
    session: &mut Session,
    source_pane_id: PaneId,
    destination_tab_id: TabId,
    destination_tree: LayoutNode,
    acting_client_id: ClientId,
) -> Result<Vec<Event>, PlacementCommitError> {
    if !session.floating_set.has_pane(source_pane_id) {
        return Err(PlacementCommitError::FloatingSourceNotFound);
    }
    let mut expected_pane_ids: HashSet<PaneId> = session
        .tabs
        .get(&destination_tab_id)
        .ok_or(PlacementCommitError::DestinationTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids()
        .into_iter()
        .collect();
    expected_pane_ids.insert(source_pane_id);
    let destination_leaf_pane_ids = destination_tree.list_leaf_pane_ids();
    let destination_pane_ids: HashSet<PaneId> = destination_leaf_pane_ids.iter().copied().collect();
    if destination_pane_ids.len() != destination_leaf_pane_ids.len()
        || destination_pane_ids != expected_pane_ids
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let acting_client = session
        .clients
        .get_client_by_id(acting_client_id)
        .ok_or(PlacementCommitError::ActingClientNotFound)?;
    let acting_previous_tab_id = acting_client.get_active_tab_id();
    let acting_previous_focused_pane_id = acting_client.get_active_focused_pane_id();
    let focusing_client_ids: Vec<ClientId> = session
        .clients
        .list_attached_clients()
        .filter(|client| {
            client.get_client_id() != acting_client_id
                && client.get_focused_floating_pane_id() == Some(source_pane_id)
        })
        .map(Client::get_client_id)
        .collect();

    let _ = session.remove_floating_member(source_pane_id);
    let mut emitted_events = Vec::new();
    for client_id in focusing_client_ids {
        if let Some(client) = session.clients.get_client_by_id(client_id) {
            emitted_events.extend(build_pane_focused_event(client, Some(source_pane_id)));
        }
    }
    emitted_events.push(Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    }));
    emitted_events.extend(session.update_tab_layout(destination_tab_id, destination_tree));

    if let Some(acting_client) = session.clients.get_client_mut_by_id(acting_client_id) {
        acting_client.clear_zoom(destination_tab_id);
        acting_client.update_active_tab_id(destination_tab_id);
    }
    let mut landed_pane_id = None;
    if acting_previous_tab_id != destination_tab_id {
        emitted_events.push(Event::TabFocused(TabFocused {
            client_id: acting_client_id,
            tab_id: destination_tab_id,
            previous_tab_id: acting_previous_tab_id,
        }));
        let has_focused_pane = session
            .clients
            .get_client_by_id(acting_client_id)
            .is_some_and(|client| client.get_focused_pane_id(destination_tab_id).is_some());
        if !has_focused_pane {
            landed_pane_id = find_landing_pane(session, destination_tab_id);
        }
    }
    if let Some(landed_pane_id) = landed_pane_id {
        if let Some(acting_client) = session.clients.get_client_mut_by_id(acting_client_id) {
            acting_client.update_focused_pane(destination_tab_id, landed_pane_id);
            if acting_client.get_active_focused_pane_id() != Some(landed_pane_id) {
                emitted_events.push(Event::PaneFocused(PaneFocused {
                    client_id: acting_client_id,
                    tab_id: Some(destination_tab_id),
                    pane_id: landed_pane_id,
                    previous_pane_id: None,
                }));
            }
        }
        if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
            destination_tab.record_focus_mru(landed_pane_id);
        }
    }
    emitted_events.extend(session.update_tab_solve(destination_tab_id));
    if acting_previous_tab_id != destination_tab_id {
        emitted_events.extend(session.update_tab_solve(acting_previous_tab_id));
    }
    if let Some(acting_client) = session.clients.get_client_by_id(acting_client_id) {
        emitted_events.extend(build_pane_focused_event(
            acting_client,
            acting_previous_focused_pane_id,
        ));
    }
    Ok(emitted_events)
}

/// Install a prepared cross-tab placement and return its ordered events.
///
/// `source_tree` is `None` only when the source pane was the source tab's sole
/// leaf. In that case the source tab is removed after the destination tree is
/// installed. The commit verifies unique ownership, exact pane-set conservation,
/// and that only the source pane leaves the source tree. The pane registry and
/// every per-pane view remain attached to the pane id.
///
/// The acting client moves to the destination tab and focuses the source pane,
/// which takes its input: no floating pane stays focused for it. Its
/// [`Event::PaneFocused`] names as `previous_pane_id` the floating pane that
/// held its input, else the pane it focused in the destination tab before.
///
/// The acting client's floating panes are minimized
/// ([`apply_tiled_pane_focus`]): one [`Event::PaneMinimizedChanged`] each
/// comes before its [`Event::PaneFocused`].
///
/// Both trees are solved again ([`Session::update_tab_layout`]), then, with
/// the acting client in the destination tab, both tabs are solved again at
/// their tab sizes ([`Session::update_tab_solve`]).
///
/// Every client zoomed on a pane that left its tab returns to its tiled view.
/// Every client focused on such a pane in the tab it left takes the pane that
/// [`repair_focus`](crate::session::focus::repair_focus) picks from the tab's
/// solve, ranked against the rect the departed pane held in the tab's solve
/// before the placement. That pane becomes the tab's most recent focus, and an
/// [`Event::PaneFocused`] names it. With no eligible pane, the client focuses
/// the tab's landing pane in the same way, and an
/// [`Event::TerminalTooSmallEntered`] follows for a client that views that
/// tab.
///
/// A source tab viewed by an `80x3` acting client and an `80x22` client is
/// ranked at `80x3` for the departed rect and at `80x22` for the panes left.
pub fn commit_cross_tab_placement(
    session: &mut Session,
    source_tab_id: TabId,
    destination_tab_id: TabId,
    source_pane_id: PaneId,
    prepared_placement: CrossTabPlacement,
    acting_client_id: ClientId,
) -> Result<Vec<Event>, PlacementCommitError> {
    if source_tab_id == destination_tab_id {
        return Err(PlacementCommitError::SameTab);
    }
    let source_leaf_pane_ids_before = session
        .tabs
        .get(&source_tab_id)
        .ok_or(PlacementCommitError::SourceTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids();
    let destination_leaf_pane_ids_before = session
        .tabs
        .get(&destination_tab_id)
        .ok_or(PlacementCommitError::DestinationTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids();
    if !source_leaf_pane_ids_before.contains(&source_pane_id) {
        return Err(PlacementCommitError::SourcePaneNotFound);
    }

    let source_leaf_pane_ids_after = prepared_placement
        .source_tree
        .as_ref()
        .map(|layout_tree| layout_tree.list_leaf_pane_ids())
        .unwrap_or_default();
    let destination_leaf_pane_ids_after = prepared_placement.destination_tree.list_leaf_pane_ids();
    let source_pane_ids_after: HashSet<PaneId> =
        source_leaf_pane_ids_after.iter().copied().collect();
    let destination_pane_ids_after: HashSet<PaneId> =
        destination_leaf_pane_ids_after.iter().copied().collect();
    if source_pane_ids_after.len() != source_leaf_pane_ids_after.len()
        || destination_pane_ids_after.len() != destination_leaf_pane_ids_after.len()
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    if !destination_pane_ids_after.contains(&source_pane_id)
        || !source_pane_ids_after.is_disjoint(&destination_pane_ids_after)
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let acting_previous_tab_id = session
        .clients
        .get_client_by_id(acting_client_id)
        .map(|client| client.get_active_tab_id())
        .ok_or(PlacementCommitError::ActingClientNotFound)?;

    let source_pane_ids_before: HashSet<PaneId> =
        source_leaf_pane_ids_before.iter().copied().collect();
    let destination_pane_ids_before: HashSet<PaneId> =
        destination_leaf_pane_ids_before.iter().copied().collect();
    if source_pane_ids_before.len() != source_leaf_pane_ids_before.len()
        || destination_pane_ids_before.len() != destination_leaf_pane_ids_before.len()
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    if !source_pane_ids_before.is_disjoint(&destination_pane_ids_before) {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let source_moved_pane_ids: HashSet<PaneId> = source_pane_ids_before
        .difference(&source_pane_ids_after)
        .copied()
        .collect();
    let destination_moved_pane_ids: HashSet<PaneId> = destination_pane_ids_before
        .difference(&destination_pane_ids_after)
        .copied()
        .collect();
    if source_moved_pane_ids.len() != 1
        || !source_moved_pane_ids.contains(&source_pane_id)
        || destination_moved_pane_ids.len() > 1
    {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let all_pane_ids_before: HashSet<PaneId> = source_pane_ids_before
        .union(&destination_pane_ids_before)
        .copied()
        .collect();
    let all_pane_ids_after: HashSet<PaneId> = source_pane_ids_after
        .union(&destination_pane_ids_after)
        .copied()
        .collect();
    if all_pane_ids_before != all_pane_ids_after {
        return Err(PlacementCommitError::PaneOwnershipConflict);
    }
    let is_source_tab_closing = prepared_placement.source_tree.is_none();
    let source_pane_rect = find_solved_pane_rect(session, source_tab_id, source_pane_id);
    let destination_departures: Vec<(PaneId, Rect)> = destination_moved_pane_ids
        .iter()
        .map(|&pane_id| {
            let pane_rect = find_solved_pane_rect(session, destination_tab_id, pane_id);
            (pane_id, pane_rect)
        })
        .collect();

    // Destination installation is the first live tree write. All fallible
    // checks happened before this point.
    let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    })];
    emitted_events
        .extend(session.update_tab_layout(destination_tab_id, prepared_placement.destination_tree));
    if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
        for &pane_id in &destination_moved_pane_ids {
            destination_tab.remove_focus_mru(pane_id);
        }
        for &pane_id in &source_moved_pane_ids {
            destination_tab.record_focus_mru(pane_id);
        }
    }

    if let Some(source_tree) = prepared_placement.source_tree {
        emitted_events.push(Event::LayoutChanged(LayoutChanged {
            tab_id: source_tab_id,
        }));
        emitted_events.extend(session.update_tab_layout(source_tab_id, source_tree));
        if let Some(source_tab) = session.tabs.get_mut(&source_tab_id) {
            for &pane_id in &source_moved_pane_ids {
                source_tab.remove_focus_mru(pane_id);
            }
            for &pane_id in &destination_moved_pane_ids {
                source_tab.record_focus_mru(pane_id);
            }
        }
    }

    if let Some(acting_client) = session.clients.get_client_mut_by_id(acting_client_id) {
        acting_client.clear_zoom(destination_tab_id);
        acting_client.update_active_tab_id(destination_tab_id);
        if acting_previous_tab_id != destination_tab_id {
            emitted_events.push(Event::TabFocused(TabFocused {
                client_id: acting_client_id,
                tab_id: destination_tab_id,
                previous_tab_id: acting_previous_tab_id,
            }));
        }
        emitted_events.extend(apply_tiled_pane_focus(
            acting_client,
            destination_tab_id,
            source_pane_id,
        ));
    }
    if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
        destination_tab.record_focus_mru(source_pane_id);
    }

    for client in session.clients.list_attached_clients_mut() {
        client.clear_zoom_of_pane(source_pane_id);
        for &(departed_pane_id, _) in &destination_departures {
            client.clear_zoom_of_pane(departed_pane_id);
        }
    }
    emitted_events.extend(session.update_tab_solve(destination_tab_id));
    if !is_source_tab_closing {
        emitted_events.extend(session.update_tab_solve(source_tab_id));
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            source_tab_id,
            source_pane_id,
            source_pane_rect,
        ));
    }
    for (departed_pane_id, departed_pane_rect) in destination_departures {
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            destination_tab_id,
            departed_pane_id,
            departed_pane_rect,
        ));
    }

    if is_source_tab_closing {
        emitted_events.extend(close_and_refocus_tab(session, source_tab_id, None));
    }

    Ok(emitted_events)
}

/// The rect `pane_id` holds in `tab_id`'s solve ([`Tab::get_layout_solve`]).
/// An empty rect at the origin when the tab is absent or its solve holds no
/// rect for `pane_id`.
///
/// [`Tab::get_layout_solve`]: crate::session::state::Tab::get_layout_solve
fn find_solved_pane_rect(session: &Session, tab_id: TabId, pane_id: PaneId) -> Rect {
    session
        .tabs
        .get(&tab_id)
        .and_then(|tab| tab.get_layout_solve().find_pane_rect(pane_id))
        .unwrap_or_else(Rect::build_empty_at_origin)
}

#[cfg(test)]
mod tests;
