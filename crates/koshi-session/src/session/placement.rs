//! Session-state commit for checked cross-tab pane placement, for undock, and
//! for dock.
//!
//! The layout crate builds the proposed trees without touching live state.
//! This module installs those trees, repairs per-client references, and removes
//! an empty source tab without removing the transferred pane record. An undock
//! takes the pane's leaf out of its tab and adds the pane to the session's
//! floating panes. A dock takes a floating pane out of the floating panes and
//! installs a tab tree that holds it.
//!
//! The acting client, and every client whose input is on a pane that moves,
//! follow that pane to its new place, where it takes their input. Every other
//! client keeps its input, its active tab and its shown floating panes. A
//! focus it holds on a pane that left a tab moves to the pane that inherits it
//! from its own focus history. No commit changes a stack's active member.

use std::collections::HashSet;
use std::iter;
use std::num::NonZeroU16;

use koshi_core::command::TiledPlacementTarget;
use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabFocused};
use koshi_core::geometry::{
    FloatingPaneDimension, FloatingPanePosition, FloatingPaneSize, Point, Size,
};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::placement::{
    find_insertion_at_point, list_shown_placement_destinations, solve_placed_tab,
    CrossTabPlacement, PlacementTabSizes,
};
use koshi_layout::tree::LayoutNode;
use thiserror::Error;

use crate::client::Client;
use crate::error::FloatingSetError;
use crate::session::cascade::{
    list_focused_client_rects, remove_pane_from_tab, repair_focus_after_pane_left,
};
use crate::session::focus::build_pane_minimized_changed_events;
use crate::session::pane_ops::show_new_floating_pane;
use crate::session::state::{FloatingMember, Session, Tab};
use crate::session::tab_ops::{close_and_refocus_tab, find_nearest_surviving_tab};

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
/// out of the tab, and show the pane for the acting client and for every
/// client whose input is on the pane ([`Client::get_active_focused_pane_id`]).
/// Returns the ordered events.
///
/// The pane's registry record, PTY and every per-pane view stay attached to
/// its id. The pane is never closed: no [`Event::PaneClosing`] and no
/// [`Event::PaneRemoved`] is emitted.
///
/// 1. The member joins the floating set as its newest member, with a
///    minimized view of it for every attached client
///    ([`Session::add_floating_member`]): at `acting_view`'s position for
///    `acting_view`'s client, at the top-left cell of the rect the pane holds
///    in its own view of the tab ([`find_lifted_floating_pane_rect`]) for a
///    client whose input is on the pane, else at the default placement.
/// 2. The leaf leaves the tab as a closed pane's leaf does in
///    [`remove_pane_cascade`](crate::session::cascade::remove_pane_cascade):
///    the tab's tree collapses, normalizes and is solved again, every zoom on
///    the pane ends, and every client focused on the pane in the tab takes the
///    inheriting focus from its own focus history. A tab left with no pane
///    closes, and its viewers move to the nearest surviving tab.
/// 3. The acting client, then each client whose input was on the pane, in
///    client id order, shows the pane above its other floating panes and
///    focuses it, when it can draw floating panes
///    ([`Session::can_client_draw_floating_panes`]). A client that cannot
///    keeps the pane minimized and the focus step 2 gave it. No other client
///    shows or focuses the pane.
///
/// Events: step 2's [`Event::LayoutChanged`] and focus events, or its
/// [`Event::TabClosed`] and the events of the viewers that move, then one
/// [`Event::PaneFocused`] with `tab_id: None` per client of step 3 that shows
/// the pane, naming the pane that took its input after step 2 as
/// `previous_pane_id`.
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
    let mut client_positions = vec![acting_view];
    for following_client_id in list_following_client_ids(session, source_pane_id, acting_client_id)
    {
        let lifted_position = find_lifted_floating_pane_rect(
            session,
            following_client_id,
            source_tab_id,
            source_pane_id,
        )
        .map_or(FloatingPanePosition::Default, |(_, top_left_cell)| {
            FloatingPanePosition::Moved(top_left_cell)
        });
        client_positions.push((following_client_id, lifted_position));
    }
    session.add_floating_member(floating_member, &client_positions)?;

    let mut emitted_events = remove_pane_from_tab(session, source_tab_id, source_pane_id, None);
    for &(client_id, _) in &client_positions {
        emitted_events.extend(show_new_floating_pane(session, client_id, source_pane_id));
    }
    Ok(emitted_events)
}

/// Where `client_id` draws the tiled pane `pane_id` of `tab_id`: the size a
/// floating pane lifted from that spot asks for, in cells, and its top-left
/// cell, counted from the client's pane-area origin. The rect is the pane's
/// rect in the client's own view of the tab
/// ([`Session::compute_client_solve`]), moved by the offset that centers the
/// tab in the client's pane area ([`Size::compute_centered_origin`]). A stack
/// member the client draws as its header strip gives the rect it takes once
/// the client opens it: the tab solved with the pane first in the client's
/// focus history ([`solve_placed_tab`]).
///
/// A `40x22` rect at `(40, 0)` in an `80x22` tab, drawn in a `100x30` pane
/// area → `Cells(40)` by `Cells(22)` at `(50, 4)`. In an `80x22` tab that is
/// the stack `[vim open, htop]`, `htop` on its header on row 21 gives
/// `Cells(80)` by `Cells(21)` at `(0, 1)`.
///
/// `None` when the client is not attached, views another tab, reports
/// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving), or draws
/// the pane at zero area, and when the session holds no `tab_id`.
#[must_use]
pub fn find_lifted_floating_pane_rect(
    session: &Session,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
) -> Option<(FloatingPaneSize, Point)> {
    let client = session.clients.get_client_by_id(client_id)?;
    if client.get_active_tab_id() != tab_id {
        return None;
    }
    let pane_area = client.get_pane_area()?;
    let tab = session.tabs.get(&tab_id)?;
    let tab_size = tab.get_solved_tab_size();
    let client_solve = session.compute_client_solve(client, tab_id)?;
    let is_pane_on_stack_header = client_solve
        .stack_headers
        .iter()
        .any(|stack_header| stack_header.pane_id == pane_id);
    let pane_rect = if is_pane_on_stack_header {
        solve_placed_tab(
            tab.get_layout_tree(),
            pane_id,
            client.list_focus_history(tab_id),
            tab_size,
            session.get_pane_sizing(),
        )
        .layout_solve
        .find_pane_rect(pane_id)?
    } else {
        client_solve.find_pane_rect(pane_id)?
    };
    let desired_size = FloatingPaneSize {
        width: FloatingPaneDimension::Cells(NonZeroU16::new(pane_rect.size.column_count)?),
        height: FloatingPaneDimension::Cells(NonZeroU16::new(pane_rect.size.row_count)?),
    };
    let tab_origin = pane_area.compute_centered_origin(tab_size);
    let top_left_cell = Point {
        column: pane_rect.origin.column.saturating_add(tab_origin.column),
        row: pane_rect.origin.row.saturating_add(tab_origin.row),
    };
    Some((desired_size, top_left_cell))
}

/// Where the floating pane `pane_id` docks for `client`: an insertion into
/// `client`'s active tab beside the smallest pane, group or tab span under
/// the center cell of the rect `client` draws the pane in
/// ([`Session::find_client_floating_pane_rect`]), on that span's edge nearest
/// the cell ([`find_insertion_at_point`]). The spans are those of the tab as
/// `client` draws it: centered in its pane area
/// ([`Size::compute_centered_origin`]), each stack showing the member that
/// `client`'s focus history gives ([`list_shown_placement_destinations`]). A
/// center cell outside the tab moves to the tab's nearest cell.
///
/// A `16x10` pane drawn at `(62, 6)`, over the tab `A | B` that fills an
/// `80x22` pane area with `B` at `(40, 0)`, has the center cell `(70, 11)`:
/// the insertion right of `B`.
///
/// `None` when `client` draws no rect for `pane_id`, such as with a pane area
/// smaller than `5x6`, or views a tab with no cell.
#[must_use]
pub fn find_nearest_dock_target(
    session: &Session,
    client: &Client,
    pane_id: PaneId,
) -> Option<TiledPlacementTarget> {
    let floating_pane_rect = session.find_client_floating_pane_rect(client, pane_id)?;
    let pane_area = client.get_pane_area()?;
    let destination_tab_id = client.get_active_tab_id();
    let tab = session.tabs.get(&destination_tab_id)?;
    let tab_size = tab.get_solved_tab_size();
    if tab_size.column_count == 0 || tab_size.row_count == 0 {
        return None;
    }
    let tab_origin = pane_area.compute_centered_origin(tab_size);
    let center_cell = Point {
        column: floating_pane_rect
            .origin
            .column
            .saturating_add(floating_pane_rect.size.column_count / 2)
            .saturating_sub(tab_origin.column)
            .min(tab_size.column_count - 1),
        row: floating_pane_rect
            .origin
            .row
            .saturating_add(floating_pane_rect.size.row_count / 2)
            .saturating_sub(tab_origin.row)
            .min(tab_size.row_count - 1),
    };
    let placement_destinations = list_shown_placement_destinations(
        tab.get_layout_tree(),
        client.list_focus_history(destination_tab_id),
        tab_size,
        session.get_pane_sizing(),
        pane_id,
    );
    let span_rects = placement_destinations
        .insertion_spans
        .iter()
        .map(|insertion_span| (&insertion_span.anchor, insertion_span.span_rect));
    let (anchor, direction) = find_insertion_at_point(span_rects, center_cell)?;
    Some(TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: anchor.clone(),
        direction,
    })
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
///    ([`Session::remove_floating_member`]).
/// 2. `destination_tree` becomes the destination tab's tree, solved again
///    ([`Session::update_tab_layout`]).
/// 3. The acting client, then each client whose input was on the pane, in
///    client id order, follows it: it leaves any zoom in the destination tab,
///    makes that tab its active tab and focuses the pane there, and every
///    floating pane it shows is minimized.
/// 4. The destination tab, then each tab a client of step 3 left, is solved
///    again at its tab size ([`Session::update_tab_solve`]).
///
/// No other client's input, active tab or shown floating panes change, and
/// a stack's active member stays the same: a client focused on `vim` in the
/// stack `[vim]` keeps `vim` open when `htop` is docked into that stack.
///
/// Events: [`Event::LayoutChanged`] for the destination tab and the events of
/// its solve, then the events of each client of step 3, then the events of
/// the solves of step 4.
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
    if session.clients.get_client_by_id(acting_client_id).is_none() {
        return Err(PlacementCommitError::ActingClientNotFound);
    }
    let moving_clients: Vec<(ClientId, TabId, Option<PaneId>)> =
        list_moving_client_ids(session, source_pane_id, acting_client_id)
            .into_iter()
            .filter_map(|client_id| {
                let client = session.clients.get_client_by_id(client_id)?;
                Some((
                    client_id,
                    client.get_active_tab_id(),
                    client.get_focused_floating_pane_id(),
                ))
            })
            .collect();

    let _ = session.remove_floating_member(source_pane_id);
    let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    })];
    emitted_events.extend(session.update_tab_layout(destination_tab_id, destination_tree));
    for &(client_id, _, previous_floating_pane_id) in &moving_clients {
        emitted_events.extend(move_client_onto_tiled_pane(
            session,
            client_id,
            destination_tab_id,
            source_pane_id,
            previous_floating_pane_id,
        ));
    }
    let left_tab_ids = moving_clients
        .iter()
        .map(|&(_, previous_tab_id, _)| previous_tab_id);
    emitted_events.extend(update_tab_solves(
        session,
        iter::once(destination_tab_id).chain(left_tab_ids),
    ));
    Ok(emitted_events)
}

/// Install a prepared cross-tab placement and return its ordered events.
///
/// `source_tree` is `None` only when the source pane was the source tab's sole
/// leaf. In that case the source tab is removed after the destination tree is
/// installed. The commit verifies unique ownership, exact pane-set conservation,
/// and that only the source pane leaves the source tree, and at most one pane,
/// the swapped pane, leaves the destination tree. The pane registry and every
/// per-pane view remain attached to the pane id.
///
/// 1. Both trees are installed and solved again
///    ([`Session::update_tab_layout`]). Every zoom on a moved pane ends.
/// 2. The acting client, then each client whose input is on the source pane,
///    in client id order, follows it to the destination tab: it leaves any
///    zoom there, makes that tab its active tab and focuses the pane there,
///    and every floating pane it shows is minimized. Each client whose input
///    is on the swapped pane follows it to the source tab in the same way.
/// 3. The destination tab, the source tab, and each tab a client of step 2
///    left, are solved again at their tab sizes
///    ([`Session::update_tab_solve`]).
/// 4. Every client whose focus in a tab was on a pane that left it, and that
///    does not follow another pane into that tab, takes the inheriting focus
///    from its own focus history, ranked against the rect the departed pane
///    held in its own view of the tab before the placement.
///
/// No other client's input or active tab changes, and a stack's active
/// member stays the same.
///
/// Events: [`Event::LayoutChanged`] and the solve events of the destination
/// tab, then of the source tab, then the events of each client of step 2,
/// then of the solves of step 3, then of the repairs of step 4, then, when
/// the source tab closes, its [`Event::TabClosed`] and the events of its
/// viewers.
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
    if session.clients.get_client_by_id(acting_client_id).is_none() {
        return Err(PlacementCommitError::ActingClientNotFound);
    }

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
    let source_moved_pane_ids: Vec<PaneId> = source_pane_ids_before
        .difference(&source_pane_ids_after)
        .copied()
        .collect();
    let destination_moved_pane_ids: Vec<PaneId> = destination_pane_ids_before
        .difference(&destination_pane_ids_after)
        .copied()
        .collect();
    if source_moved_pane_ids != [source_pane_id] || destination_moved_pane_ids.len() > 1 {
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
    let swapped_pane_id = destination_moved_pane_ids.first().copied();
    let is_source_tab_closing = prepared_placement.source_tree.is_none();

    let source_moving_client_ids =
        list_moving_client_ids(session, source_pane_id, acting_client_id);
    let swapped_moving_client_ids = swapped_pane_id
        .map(|swapped_pane_id| {
            list_following_client_ids(session, swapped_pane_id, acting_client_id)
        })
        .unwrap_or_default();
    let left_tab_ids: Vec<TabId> = source_moving_client_ids
        .iter()
        .chain(&swapped_moving_client_ids)
        .filter_map(|&client_id| session.clients.get_client_by_id(client_id))
        .map(Client::get_active_tab_id)
        .collect();
    let mut source_focused_client_rects =
        list_focused_client_rects(session, source_tab_id, source_pane_id);
    source_focused_client_rects
        .retain(|(client_id, _)| !swapped_moving_client_ids.contains(client_id));
    let mut destination_focused_client_rects = swapped_pane_id
        .map(|swapped_pane_id| {
            list_focused_client_rects(session, destination_tab_id, swapped_pane_id)
        })
        .unwrap_or_default();
    destination_focused_client_rects
        .retain(|(client_id, _)| !source_moving_client_ids.contains(client_id));

    // Destination installation is the first live tree write. All fallible
    // checks happened before this point.
    let mut emitted_events = vec![Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    })];
    emitted_events
        .extend(session.update_tab_layout(destination_tab_id, prepared_placement.destination_tree));
    if let Some(destination_tab) = session.tabs.get_mut(&destination_tab_id) {
        if let Some(swapped_pane_id) = swapped_pane_id {
            destination_tab.remove_focus_mru(swapped_pane_id);
        }
    }
    if let Some(source_tree) = prepared_placement.source_tree {
        emitted_events.push(Event::LayoutChanged(LayoutChanged {
            tab_id: source_tab_id,
        }));
        emitted_events.extend(session.update_tab_layout(source_tab_id, source_tree));
        if let Some(source_tab) = session.tabs.get_mut(&source_tab_id) {
            source_tab.remove_focus_mru(source_pane_id);
            if let Some(swapped_pane_id) = swapped_pane_id {
                source_tab.record_focus_mru(swapped_pane_id);
            }
        }
    }
    for client in session.clients.list_attached_clients_mut() {
        client.clear_zoom_of_pane(source_pane_id);
        if let Some(swapped_pane_id) = swapped_pane_id {
            client.clear_zoom_of_pane(swapped_pane_id);
        }
    }

    for &client_id in &source_moving_client_ids {
        emitted_events.extend(move_client_onto_tiled_pane(
            session,
            client_id,
            destination_tab_id,
            source_pane_id,
            None,
        ));
    }
    if let Some(swapped_pane_id) = swapped_pane_id {
        for &client_id in &swapped_moving_client_ids {
            emitted_events.extend(move_client_onto_tiled_pane(
                session,
                client_id,
                source_tab_id,
                swapped_pane_id,
                None,
            ));
        }
    }

    let solved_tab_ids = iter::once(destination_tab_id)
        .chain(iter::once(source_tab_id))
        .chain(left_tab_ids)
        .filter(|&tab_id| !(is_source_tab_closing && tab_id == source_tab_id));
    emitted_events.extend(update_tab_solves(session, solved_tab_ids));
    if !is_source_tab_closing {
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            source_tab_id,
            source_pane_id,
            &source_focused_client_rects,
        ));
    }
    if let Some(swapped_pane_id) = swapped_pane_id {
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            destination_tab_id,
            swapped_pane_id,
            &destination_focused_client_rects,
        ));
    }

    if is_source_tab_closing {
        emitted_events.extend(close_and_refocus_tab(session, source_tab_id, None));
    }

    Ok(emitted_events)
}

/// The clients that follow `pane_id` when `acting_client_id` moves it:
/// `acting_client_id` first, then every attached client whose input is on
/// `pane_id` ([`list_following_client_ids`]).
#[must_use]
pub fn list_moving_client_ids(
    session: &Session,
    pane_id: PaneId,
    acting_client_id: ClientId,
) -> Vec<ClientId> {
    let mut moving_client_ids = vec![acting_client_id];
    moving_client_ids.extend(list_following_client_ids(
        session,
        pane_id,
        acting_client_id,
    ));
    moving_client_ids
}

/// Every attached client other than `acting_client_id` whose input is on
/// `pane_id` ([`Client::get_active_focused_pane_id`]), in client id order.
#[must_use]
pub fn list_following_client_ids(
    session: &Session,
    pane_id: PaneId,
    acting_client_id: ClientId,
) -> Vec<ClientId> {
    session
        .clients
        .list_attached_clients()
        .filter(|client| {
            client.get_client_id() != acting_client_id
                && client.get_active_focused_pane_id() == Some(pane_id)
        })
        .map(Client::get_client_id)
        .collect()
}

/// The tab size `tab_id` takes once the clients of `arriving_client_ids`
/// switch to it and the clients of `leaving_client_ids` switch away: the
/// per-axis minimum pane area ([`Client::get_pane_area`]) of every attached
/// client of `arriving_client_ids`, and of every other attached client, not in
/// `leaving_client_ids`, whose active tab is `tab_id` or
/// `landing_viewer_tab_id`. `None` when none of those clients has a pane area.
///
/// `tab_id` viewed by an `80x22` client that leaves and a `100x30` client
/// that stays, with a `90x20` client arriving, gives `90x20`.
#[must_use]
pub fn compute_tab_size_after_clients_move(
    session: &Session,
    tab_id: TabId,
    landing_viewer_tab_id: Option<TabId>,
    arriving_client_ids: &[ClientId],
    leaving_client_ids: &[ClientId],
) -> Option<Size> {
    session
        .clients
        .list_attached_clients()
        .filter(|client| {
            let client_id = client.get_client_id();
            let active_tab_id = client.get_active_tab_id();
            arriving_client_ids.contains(&client_id)
                || (!leaving_client_ids.contains(&client_id)
                    && (active_tab_id == tab_id || landing_viewer_tab_id == Some(active_tab_id)))
        })
        .filter_map(Client::get_pane_area)
        .reduce(Size::compute_minimum_axes)
}

/// The tab sizes a placement of `source_pane_id` into `destination_tab_id`
/// is checked and solved at, once the clients it moves have switched tabs
/// ([`compute_tab_size_after_clients_move`]). `swapped_pane_id` is the
/// destination pane a swap sends to the source tab, and `None` for a split or
/// a stack.
///
/// - Within the source pane's own tab no client moves: the destination tab
///   size is [`Session::get_tab_size`], and the source tab size is `None`.
/// - Into another tab, `acting_client_id` and every client whose input is on
///   the source pane ([`list_moving_client_ids`]) arrive at the destination
///   tab. A swap sends every client other than `acting_client_id` whose input
///   is on `swapped_pane_id` ([`list_following_client_ids`]) from the
///   destination tab to the source tab. A split or a stack of the source
///   tab's only pane closes the source tab. When the destination tab is the
///   nearest surviving tab ([`find_nearest_surviving_tab`]), the closed tab's
///   other viewers arrive at the destination tab too. The source tab size is
///   `None` for a floating source pane and for a source tab that closes.
///
/// Tabs `[A, B]`: `A` holds only `S`, Alice (`100x30`) and Bob (`60x18`) both
/// type in `S`, and no client views `B`. A split of `S` into `B` gives the
/// destination tab size `60x18` and no source tab size.
#[must_use]
pub fn compute_placement_tab_sizes(
    session: &Session,
    source_pane_id: PaneId,
    destination_tab_id: TabId,
    swapped_pane_id: Option<PaneId>,
    acting_client_id: ClientId,
) -> PlacementTabSizes {
    let source_tab_id = session
        .find_tab_by_pane_id(source_pane_id)
        .map(Tab::get_tab_id);
    if source_tab_id == Some(destination_tab_id) {
        return PlacementTabSizes {
            destination_tab_size: session.get_tab_size(destination_tab_id),
            source_tab_size: None,
        };
    }
    let source_moving_client_ids =
        list_moving_client_ids(session, source_pane_id, acting_client_id);
    let swapped_moving_client_ids = match swapped_pane_id {
        Some(swapped_pane_id) => {
            list_following_client_ids(session, swapped_pane_id, acting_client_id)
        }
        None => Vec::new(),
    };
    let is_source_tab_closing = swapped_pane_id.is_none()
        && source_tab_id.is_some_and(|source_tab_id| {
            session.tabs.get(&source_tab_id).is_some_and(|source_tab| {
                source_tab.get_layout_tree().list_leaf_pane_ids() == [source_pane_id]
            })
        });
    let landing_viewer_tab_id = match source_tab_id {
        Some(source_tab_id)
            if is_source_tab_closing
                && find_nearest_surviving_tab(session, source_tab_id)
                    == Some(destination_tab_id) =>
        {
            Some(source_tab_id)
        }
        _ => None,
    };
    let destination_tab_size = compute_tab_size_after_clients_move(
        session,
        destination_tab_id,
        landing_viewer_tab_id,
        &source_moving_client_ids,
        &swapped_moving_client_ids,
    );
    let source_tab_size = match source_tab_id {
        Some(source_tab_id) if !is_source_tab_closing => compute_tab_size_after_clients_move(
            session,
            source_tab_id,
            None,
            &swapped_moving_client_ids,
            &source_moving_client_ids,
        ),
        _ => None,
    };
    PlacementTabSizes {
        destination_tab_size,
        source_tab_size,
    }
}

/// Move `client_id` onto `pane_id`, a leaf of `tab_id`: the client leaves any
/// zoom in `tab_id`, makes `tab_id` its active tab, and focuses `pane_id`
/// there ([`Client::focus_tiled_pane`]). `pane_id` takes its input, and every
/// floating pane it shows is minimized. The tab records `pane_id` as its
/// most-recent focus.
///
/// Returns [`Event::TabFocused`] when the client's active tab changed, one
/// [`Event::PaneMinimizedChanged`] per floating pane minimized, oldest shown
/// first, then [`Event::PaneFocused`] with `tab_id: Some(tab_id)`. Its
/// `previous_pane_id` is `previous_floating_pane_id`, the floating pane that
/// held the client's input before the placement, else the floating pane that
/// held it now, else the pane the client focused in `tab_id` before. A client
/// whose input was on the docked pane `htop` gets `PaneFocused { pane_id:
/// htop, previous_pane_id: Some(htop) }`.
///
/// A client that is not attached changes nothing and returns no events.
fn move_client_onto_tiled_pane(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
    previous_floating_pane_id: Option<PaneId>,
) -> Vec<Event> {
    let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
        return Vec::new();
    };
    let previous_tab_id = client.get_active_tab_id();
    client.clear_zoom(tab_id);
    client.update_active_tab_id(tab_id);
    let tiled_pane_focus_change = client.focus_tiled_pane(tab_id, pane_id);
    let mut emitted_events = Vec::new();
    if previous_tab_id != tab_id {
        emitted_events.push(Event::TabFocused(TabFocused {
            client_id,
            tab_id,
            previous_tab_id,
        }));
    }
    emitted_events.extend(build_pane_minimized_changed_events(
        client_id,
        &tiled_pane_focus_change.minimized_floating_pane_ids,
        true,
    ));
    emitted_events.push(Event::PaneFocused(PaneFocused {
        client_id,
        tab_id: Some(tab_id),
        pane_id,
        previous_pane_id: previous_floating_pane_id.or(tiled_pane_focus_change.previous_pane_id),
    }));
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.record_focus_mru(pane_id);
    }
    emitted_events
}

/// Solve each tab of `tab_ids` again at its tab size
/// ([`Session::update_tab_solve`]), in the order listed, each tab once.
/// Returns the events of the solves.
fn update_tab_solves(
    session: &mut Session,
    tab_ids: impl IntoIterator<Item = TabId>,
) -> Vec<Event> {
    let mut solved_tab_ids: Vec<TabId> = Vec::new();
    let mut emitted_events = Vec::new();
    for tab_id in tab_ids {
        if solved_tab_ids.contains(&tab_id) {
            continue;
        }
        solved_tab_ids.push(tab_id);
        emitted_events.extend(session.update_tab_solve(tab_id));
    }
    emitted_events
}

#[cfg(test)]
mod tests;
