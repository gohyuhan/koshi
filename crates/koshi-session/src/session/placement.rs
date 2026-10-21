//! Session-state commit for checked cross-tab pane placement and for undock.
//!
//! The layout crate builds the proposed trees without touching live state.
//! This module installs those trees, repairs per-client references, and removes
//! an empty source tab without removing the transferred pane record. An undock
//! takes the pane's leaf out of its tab and adds the pane to the session's
//! floating panes.

use std::collections::HashSet;

use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabFocused};
use koshi_core::geometry::Rect;
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::placement::CrossTabPlacement;
use koshi_layout::solver::{solve_layout_with_sizing, PaneSizing};
use thiserror::Error;

use crate::client::{FloatingPanePosition, FloatingPaneView};
use crate::error::FloatingSetError;
use crate::session::cascade::{remove_pane_from_tab, repair_focus_after_pane_left};
use crate::session::focus::build_pane_focused_event;
use crate::session::state::{FloatingMember, Session};
use crate::session::tab_ops::close_and_refocus_tab;

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
/// out of the tab, and give the acting client the pane's input. Returns the
/// ordered events.
///
/// The pane's registry record, PTY and every per-pane view stay attached to
/// its id. The pane is never closed: no [`Event::PaneClosing`] and no
/// [`Event::PaneRemoved`] is emitted.
///
/// 1. The member joins the floating set as its newest member.
/// 2. The leaf leaves the tab as a closed pane's leaf does in
///    [`remove_pane_cascade`](crate::session::cascade::remove_pane_cascade),
///    ranking focus against the tab solved in
///    [`Session::compute_focus_repair_tab_rect`] with `pane_sizing`: the tab's
///    tree collapses and normalizes, every zoom on the pane ends, and every
///    client focused on the pane in the tab takes the inheriting focus. A tab
///    left with no pane closes, and its viewers move to the nearest surviving
///    tab.
/// 3. `acting_view`'s client stores the position as its view of the pane,
///    focuses the pane and draws it above its other floating panes. No other
///    client stores a view or focuses the pane.
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
///   refuses the member ([`FloatingSet::add_member`](crate::session::state::FloatingSet::add_member)).
pub fn commit_undock_placement(
    session: &mut Session,
    source_tab_id: TabId,
    floating_member: FloatingMember,
    acting_view: (ClientId, FloatingPanePosition),
    pane_sizing: PaneSizing,
) -> Result<Vec<Event>, PlacementCommitError> {
    let source_pane_id = floating_member.pane_id;
    let (acting_client_id, acting_position) = acting_view;
    let source_leaf_pane_ids = session
        .tabs
        .get(&source_tab_id)
        .ok_or(PlacementCommitError::SourceTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids();
    if !source_leaf_pane_ids.contains(&source_pane_id) {
        return Err(PlacementCommitError::SourcePaneNotFound);
    }
    if session.tabs.len() == 1 && source_leaf_pane_ids == vec![source_pane_id] {
        return Err(PlacementCommitError::LastTiledPane);
    }
    if session.clients.get_client_by_id(acting_client_id).is_none() {
        return Err(PlacementCommitError::ActingClientNotFound);
    }
    session.floating_set.add_member(floating_member)?;

    let source_tab_rect = session.compute_focus_repair_tab_rect(source_tab_id);
    let mut emitted_events = remove_pane_from_tab(
        session,
        source_tab_id,
        source_pane_id,
        source_tab_rect,
        pane_sizing,
        None,
    );
    if let Some(acting_client) = session.clients.get_client_mut_by_id(acting_client_id) {
        acting_client.set_floating_pane_view(
            source_pane_id,
            FloatingPaneView {
                position: acting_position,
                is_minimized: false,
            },
        );
        let previous_focused_pane_id = acting_client.get_active_focused_pane_id();
        let _ = acting_client.focus_floating_pane(source_pane_id);
        emitted_events.extend(build_pane_focused_event(
            acting_client,
            previous_focused_pane_id,
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
/// Every client zoomed on a pane that left its tab returns to its tiled view.
/// Every client focused on such a pane in the tab it left takes the pane that
/// [`repair_focus`](crate::session::focus::repair_focus) picks, with
/// `pane_sizing`, from the tab's current tree, ranked against the rect the
/// departed pane held. Both solves use
/// [`Session::compute_focus_repair_tab_rect`] of the tab: the departed rect
/// with the clients as they were before the placement, the current tree with
/// the acting client already in the destination tab. That pane becomes the
/// tab's most recent focus, and an [`Event::PaneFocused`] names it. With no
/// eligible pane, the client focuses the tab's landing pane in the same way,
/// and an [`Event::TerminalTooSmallEntered`] follows.
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
    pane_sizing: PaneSizing,
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
    let source_pane_rect =
        find_solved_pane_rect(session, source_tab_id, source_pane_id, pane_sizing);
    let destination_departures: Vec<(PaneId, Rect)> = destination_moved_pane_ids
        .iter()
        .map(|&pane_id| {
            let pane_rect =
                find_solved_pane_rect(session, destination_tab_id, pane_id, pane_sizing);
            (pane_id, pane_rect)
        })
        .collect();

    // Destination installation is the first live tree write. All fallible
    // checks happened before this point.
    if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
        destination_tab.update_layout(prepared_placement.destination_tree);
        for &pane_id in &destination_moved_pane_ids {
            destination_tab.remove_focus_mru(pane_id);
        }
        for &pane_id in &source_moved_pane_ids {
            destination_tab.record_focus_mru(pane_id);
        }
    }
    let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    })];

    if let Some(source_tree) = prepared_placement.source_tree {
        if let Some(source_tab) = session.tabs.get_mut(&source_tab_id) {
            source_tab.update_layout(source_tree);
            for &pane_id in &source_moved_pane_ids {
                source_tab.remove_focus_mru(pane_id);
            }
            for &pane_id in &destination_moved_pane_ids {
                source_tab.record_focus_mru(pane_id);
            }
        }
        emitted_events.push(Event::LayoutChanged(LayoutChanged {
            tab_id: source_tab_id,
        }));
    }

    let mut acting_previous_pane_id = None;
    if let Some(acting_client) = session.clients.get_client_mut_by_id(acting_client_id) {
        acting_client.clear_zoom(destination_tab_id);
        acting_client.update_active_tab_id(destination_tab_id);
        acting_previous_pane_id =
            acting_client.focus_tiled_pane(destination_tab_id, source_pane_id);
    }
    if acting_previous_tab_id != destination_tab_id {
        emitted_events.push(Event::TabFocused(TabFocused {
            client_id: acting_client_id,
            tab_id: destination_tab_id,
            previous_tab_id: acting_previous_tab_id,
        }));
    }
    if acting_previous_pane_id != Some(source_pane_id) {
        emitted_events.push(Event::PaneFocused(PaneFocused {
            client_id: acting_client_id,
            tab_id: Some(destination_tab_id),
            pane_id: source_pane_id,
            previous_pane_id: acting_previous_pane_id,
        }));
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
    if !is_source_tab_closing {
        let source_tab_rect = session.compute_focus_repair_tab_rect(source_tab_id);
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            source_tab_id,
            source_pane_id,
            source_pane_rect,
            source_tab_rect,
            pane_sizing,
        ));
    }
    let destination_tab_rect = session.compute_focus_repair_tab_rect(destination_tab_id);
    for (departed_pane_id, departed_pane_rect) in destination_departures {
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            destination_tab_id,
            departed_pane_id,
            departed_pane_rect,
            destination_tab_rect,
            pane_sizing,
        ));
    }

    if is_source_tab_closing {
        emitted_events.extend(close_and_refocus_tab(session, source_tab_id, None));
    }

    Ok(emitted_events)
}

/// The rect `pane_id` holds in `tab_id`'s current tree solved with
/// `pane_sizing` in [`Session::compute_focus_repair_tab_rect`] of the tab. An
/// empty rect at the origin when the tab is absent or holds no leaf for
/// `pane_id`.
fn find_solved_pane_rect(
    session: &Session,
    tab_id: TabId,
    pane_id: PaneId,
    pane_sizing: PaneSizing,
) -> Rect {
    let Some(tab) = session.tabs.get(&tab_id) else {
        return Rect::build_empty_at_origin();
    };
    let tab_rect = session.compute_focus_repair_tab_rect(tab_id);
    solve_layout_with_sizing(tab.get_layout_tree(), tab_rect, pane_sizing)
        .pane_rects
        .into_iter()
        .find(|&(solved_pane_id, _)| solved_pane_id == pane_id)
        .map_or_else(Rect::build_empty_at_origin, |(_, pane_rect)| pane_rect)
}

#[cfg(test)]
mod tests;
