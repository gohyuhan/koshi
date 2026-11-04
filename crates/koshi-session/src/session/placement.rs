//! Session-state commit for checked cross-tab pane placement, for undock, and
//! for dock.
//!
//! The layout crate builds the proposed trees without touching live state.
//! This module installs those trees, repairs per-client references, and removes
//! an empty source tab without removing the transferred pane record. An undock
//! takes the pane's leaf out of its tab, or out of a floating pane of 2 or
//! more panes, and adds a new floating pane that holds it. A dock takes one
//! pane, or every pane of a floating pane, out of the floating panes and
//! installs a tab tree that holds them.
//!
//! The acting client, and every client whose input is on a pane that moves,
//! follow that pane to its new place, where it takes their input. Every other
//! client keeps its input, its active tab and its shown floating panes. A
//! focus it holds on a pane that left a tab or a floating pane moves to the
//! pane that inherits it from its own focus history. No commit changes a
//! stack's active member.

use std::collections::HashSet;
use std::iter;
use std::num::NonZeroU16;

use koshi_core::command::TiledPlacementTarget;
use koshi_core::constant::FLOATING_TITLEBAR_STRIP_ROW_COUNT;
use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabFocused};
use koshi_core::geometry::{
    FloatingPaneDimension, FloatingPanePosition, FloatingPaneSize, Point, Rect, Size,
};
use koshi_core::ids::{ClientId, FloatingPaneId, PaneId, TabId};
use koshi_layout::placement::{
    find_insertion_at_point, list_placement_destinations, solve_placed_tab, CrossTabPlacement,
    PlacementTabSizes,
};
use koshi_layout::tree::LayoutNode;
use thiserror::Error;

use crate::client::Client;
use crate::error::FloatingSetError;
use crate::session::cascade::{
    list_floating_pane_focused_client_rects, list_focused_client_rects, remove_pane_from_tab,
    remove_pane_leaf_from_floating_pane, repair_floating_pane_focus_after_pane_left,
    repair_focus_after_pane_left,
};
use crate::session::focus::build_floating_pane_minimized_changed_events;
use crate::session::pane_ops::show_new_floating_pane;
use crate::session::state::{FloatingPane, LayoutAreaId, Session, Tab};
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
    /// No floating pane of the session holds the source pane of a dock or an
    /// undock, or the session holds no floating pane a whole dock names.
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
    /// An undock source is the only pane of its floating pane.
    #[error("{pane_id} is already alone in its floating pane")]
    AloneInFloatingPane {
        /// The undock source.
        pane_id: PaneId,
    },
    /// The floating set refused the undocked pane.
    #[error(transparent)]
    FloatingPaneRefused(#[from] FloatingSetError),
}

/// The panes a dock takes out of a floating pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockSource {
    /// One pane of a floating pane. The floating pane keeps its other panes,
    /// and leaves the floating set when it holds no other pane.
    Pane(PaneId),
    /// Every pane of the floating pane, as one subtree. The floating pane
    /// leaves the floating set.
    WholeFloatingPane(FloatingPaneId),
}

/// Undock `source_pane_id`, a pane of the tab or the floating pane
/// `source_layout_area_id` names: add a new floating pane whose tree is
/// `Pane(source_pane_id)`, asking for `desired_size` and holding
/// `solved_size`, its tree solved with [`Session::get_pane_sizing`] over
/// [`compute_floating_layout_size`](crate::session::state::compute_floating_layout_size)
/// of `solved_size`, to the session's floating panes, take the pane's leaf out
/// of its tree, and show the new floating pane for the acting client and for
/// every client whose input is on the pane
/// ([`Client::get_active_focused_pane_id`]). Returns the ordered events.
///
/// The pane's registry record, PTY and every per-pane view stay attached to
/// its id. The pane is never closed: no [`Event::PaneClosing`] and no
/// [`Event::PaneRemoved`] is emitted.
///
/// 1. The new floating pane joins the floating set as its newest floating
///    pane, with a minimized view of it for every attached client
///    ([`Session::add_floating_pane`]): at `acting_view`'s position for
///    `acting_view`'s client, at the top-left cell where it draws the pane
///    ([`find_lifted_floating_pane_rect`]) for a client whose input is on the
///    pane, else at the default placement.
/// 2. From a tab, the leaf leaves the tab as a closed pane's leaf does in
///    [`remove_pane_cascade`](crate::session::cascade::remove_pane_cascade):
///    the tab's tree collapses, normalizes and is solved again, every zoom on
///    the pane ends, and every client focused on the pane in the tab takes the
///    inheriting focus from its own focus history. A tab left with no pane
///    closes, and its viewers move to the nearest surviving tab. From a
///    floating pane, the leaf collapses out of its tree, which is solved
///    again, and every zoom on the pane ends; the floating pane keeps its
///    desired size and its other panes. This step runs before step 1 for a
///    floating pane source, and after it for a tab source.
/// 3. The acting client, then each client whose input was on the pane, in
///    client id order, shows the new floating pane above its other floating
///    panes and focuses it, when it can draw floating panes
///    ([`Session::can_client_draw_floating_panes`]). A client that cannot
///    keeps the floating pane minimized and the focus step 2 gave it. No other
///    client shows or focuses the new floating pane.
/// 4. For a floating pane source, each client whose focused pane inside that
///    floating pane was the pane focuses the pane that inherits it there,
///    ranked against the rect the pane held in that client's own view before
///    the undock. Then the pane leaves every client's focus history inside
///    that floating pane. Only a client whose input that floating pane still
///    takes changes its input.
///
/// Events, from a tab: step 2's [`Event::LayoutChanged`] and focus events, or
/// its [`Event::TabClosed`] and the events of the viewers that move, then one
/// [`Event::PaneFocused`] with `tab_id: None` per client of step 3 that shows
/// the pane, naming the pane that took its input after step 2 as
/// `previous_pane_id`. From a floating pane: [`Event::FloatingPaneLayoutChanged`]
/// with the panes left and the events of its solve, then the
/// [`Event::PaneFocused`] events of step 3, then those of step 4. Moving
/// `htop` out of `[vim | htop]` leaves `[vim]` and adds the floating pane
/// `[htop]`.
///
/// # Errors
///
/// Checked in this order, before any change:
/// - [`PlacementCommitError::SourceTabNotFound`]: the session holds no
///   source tab;
/// - [`PlacementCommitError::SourcePaneNotFound`]: the pane is not a leaf of
///   that tab;
/// - [`PlacementCommitError::FloatingSourceNotFound`]: the session holds no
///   source floating pane, or the pane is not a leaf of it;
/// - [`PlacementCommitError::AloneInFloatingPane`]: the pane is the only pane
///   of its floating pane;
/// - [`PlacementCommitError::LastTiledPane`]: the pane is the only pane of the
///   session's only tab;
/// - [`PlacementCommitError::ActingClientNotFound`]: `acting_view`'s client is
///   not attached;
/// - [`PlacementCommitError::FloatingPaneRefused`]: the floating set
///   refuses the floating pane ([`Session::add_floating_pane`]).
pub fn commit_undock_placement(
    session: &mut Session,
    source_layout_area_id: LayoutAreaId,
    source_pane_id: PaneId,
    (desired_size, solved_size): (FloatingPaneSize, Size),
    acting_view: (ClientId, FloatingPanePosition),
) -> Result<Vec<Event>, PlacementCommitError> {
    let (acting_client_id, _) = acting_view;
    match source_layout_area_id {
        LayoutAreaId::Tab(source_tab_id) => {
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
        }
        LayoutAreaId::FloatingPane(source_floating_pane_id) => {
            let (_, source_floating_pane) = session
                .floating_set
                .find_floating_pane_by_id(source_floating_pane_id)
                .ok_or(PlacementCommitError::FloatingSourceNotFound)?;
            let source_floating_pane_ids = source_floating_pane.list_pane_ids();
            if !source_floating_pane_ids.contains(&source_pane_id) {
                return Err(PlacementCommitError::FloatingSourceNotFound);
            }
            if source_floating_pane_ids.len() == 1 {
                return Err(PlacementCommitError::AloneInFloatingPane {
                    pane_id: source_pane_id,
                });
            }
        }
    }
    if session.clients.get_client_by_id(acting_client_id).is_none() {
        return Err(PlacementCommitError::ActingClientNotFound);
    }
    if session.floating_set.is_full() {
        return Err(PlacementCommitError::FloatingPaneRefused(
            FloatingSetError::TooManyPanes,
        ));
    }
    let mut client_positions = vec![acting_view];
    for following_client_id in
        list_following_client_ids(session, &[source_pane_id], acting_client_id)
    {
        let lifted_position = find_lifted_floating_pane_rect(
            session,
            following_client_id,
            source_layout_area_id,
            source_pane_id,
        )
        .map_or(FloatingPanePosition::Default, |(_, top_left_cell)| {
            FloatingPanePosition::Moved(top_left_cell)
        });
        client_positions.push((following_client_id, lifted_position));
    }
    let floating_pane = FloatingPane::from_pane(
        source_pane_id,
        desired_size,
        solved_size,
        session.get_pane_sizing(),
    );
    let floating_pane_id = floating_pane.get_floating_pane_id();
    let focused_client_rects = match source_layout_area_id {
        LayoutAreaId::Tab(_) => Vec::new(),
        LayoutAreaId::FloatingPane(source_floating_pane_id) => {
            list_floating_pane_focused_client_rects(
                session,
                source_floating_pane_id,
                source_pane_id,
            )
        }
    };

    let mut emitted_events = match source_layout_area_id {
        LayoutAreaId::Tab(source_tab_id) => {
            session.add_floating_pane(floating_pane, &client_positions)?;
            remove_pane_from_tab(session, source_tab_id, source_pane_id, None)
        }
        LayoutAreaId::FloatingPane(source_floating_pane_id) => {
            let emitted_events = remove_pane_leaf_from_floating_pane(
                session,
                source_floating_pane_id,
                source_pane_id,
            );
            session
                .add_floating_pane(floating_pane, &client_positions)
                .expect("the set is not full, and no floating pane holds the pane once it left");
            emitted_events
        }
    };
    for &(client_id, _) in &client_positions {
        emitted_events.extend(show_new_floating_pane(session, client_id, floating_pane_id));
    }
    if let LayoutAreaId::FloatingPane(source_floating_pane_id) = source_layout_area_id {
        emitted_events.extend(repair_floating_pane_focus_after_pane_left(
            session,
            source_floating_pane_id,
            source_pane_id,
            &focused_client_rects,
        ));
    }
    Ok(emitted_events)
}

/// Where `client_id` draws `pane_id`, a pane of the tab or the floating pane
/// `layout_area_id` names: the size a floating pane lifted from that spot
/// asks for, in cells, and its top-left cell, counted from the client's
/// pane-area origin.
///
/// - In a tab, the rect is the pane's rect in the client's own view of the
///   tab ([`Session::compute_client_solve`]), moved by the offset that
///   centers the tab in the client's pane area
///   ([`Size::compute_centered_origin`]).
/// - In a floating pane, the rect is the box the client draws the pane in,
///   grown up by the titlebar strip row, from the client's own view of the
///   floating pane ([`Session::compute_client_floating_pane_solve`]).
///
/// A stack member the client draws as its header strip gives the rect it
/// takes once the client opens it: the tree solved with the pane first in the
/// client's focus history there ([`solve_placed_tab`]).
///
/// A `40x22` rect at `(40, 0)` in an `80x22` tab, drawn in a `100x30` pane
/// area → `Cells(40)` by `Cells(22)` at `(50, 4)`. In an `80x22` tab that is
/// the stack `[vim open, htop]`, `htop` on its header on row 21 gives
/// `Cells(80)` by `Cells(21)` at `(0, 1)`. In the floating pane `[vim | htop]`
/// drawn `48x13` at `(16, 4)`, `htop` gives `Cells(24)` by `Cells(13)` at
/// `(40, 4)`.
///
/// `None` when the client is not attached, reports
/// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving), or draws
/// the pane at zero area, such as behind its own zoom on another pane; for a
/// tab, when the client views another tab or the session holds no such tab;
/// for a floating pane, when the client does not show it
/// ([`Client::is_floating_pane_shown`]), draws no rect for it, or the session
/// holds no such floating pane.
#[must_use]
pub fn find_lifted_floating_pane_rect(
    session: &Session,
    client_id: ClientId,
    layout_area_id: LayoutAreaId,
    pane_id: PaneId,
) -> Option<(FloatingPaneSize, Point)> {
    let client = session.clients.get_client_by_id(client_id)?;
    let lifted_rect = match layout_area_id {
        LayoutAreaId::Tab(tab_id) => find_lifted_tab_pane_rect(session, client, tab_id, pane_id)?,
        LayoutAreaId::FloatingPane(floating_pane_id) => {
            find_lifted_floating_pane_member_rect(session, client, floating_pane_id, pane_id)?
        }
    };
    let desired_size = FloatingPaneSize {
        width: FloatingPaneDimension::Cells(NonZeroU16::new(lifted_rect.size.column_count)?),
        height: FloatingPaneDimension::Cells(NonZeroU16::new(lifted_rect.size.row_count)?),
    };
    Some((desired_size, lifted_rect.origin))
}

/// The rect `client` draws `pane_id`, a leaf of `tab_id`, in, counted from
/// its pane-area origin, as [`find_lifted_floating_pane_rect`] states for a
/// tab.
fn find_lifted_tab_pane_rect(
    session: &Session,
    client: &Client,
    tab_id: TabId,
    pane_id: PaneId,
) -> Option<Rect> {
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
    let tab_origin = pane_area.compute_centered_origin(tab_size);
    Some(Rect::from_origin_and_size(
        Point {
            column: pane_rect.origin.column.saturating_add(tab_origin.column),
            row: pane_rect.origin.row.saturating_add(tab_origin.row),
        },
        pane_rect.size,
    ))
}

/// The box `client` draws `pane_id`, a leaf of `floating_pane_id`, in, grown
/// up by the titlebar strip row and counted from its pane-area origin, as
/// [`find_lifted_floating_pane_rect`] states for a floating pane.
fn find_lifted_floating_pane_member_rect(
    session: &Session,
    client: &Client,
    floating_pane_id: FloatingPaneId,
    pane_id: PaneId,
) -> Option<Rect> {
    if !client.is_floating_pane_shown(floating_pane_id) {
        return None;
    }
    let floating_pane_rect = session.find_client_floating_pane_rect(client, floating_pane_id)?;
    let client_solve = session.compute_client_floating_pane_solve(client, floating_pane_id)?;
    let is_pane_on_stack_header = client_solve
        .stack_headers
        .iter()
        .any(|stack_header| stack_header.pane_id == pane_id);
    let pane_rect = if is_pane_on_stack_header {
        let (_, floating_pane) = session
            .floating_set
            .find_floating_pane_by_id(floating_pane_id)?;
        solve_placed_tab(
            floating_pane.get_layout_tree(),
            pane_id,
            &client
                .get_floating_pane_view(floating_pane_id)?
                .focus_history,
            floating_pane.get_solved_layout_size(),
            session.get_pane_sizing(),
        )
        .layout_solve
        .find_pane_rect(pane_id)?
    } else {
        client_solve.find_pane_rect(pane_id)?
    };
    Some(compute_floating_pane_member_box(
        floating_pane_rect,
        pane_rect,
    ))
}

/// The box a client draws a pane of a floating pane in, grown up by the
/// titlebar strip row: `pane_rect`, counted from the first cell below the
/// strip of `floating_pane_rect`, moved onto the client's pane area and made
/// [`FLOATING_TITLEBAR_STRIP_ROW_COUNT`] rows taller at its top. In the
/// floating pane drawn `48x13` at `(16, 4)`, the box `24x12` at `(24, 0)`
/// gives `24x13` at `(40, 4)`.
fn compute_floating_pane_member_box(floating_pane_rect: Rect, pane_rect: Rect) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: floating_pane_rect
                .origin
                .column
                .saturating_add(pane_rect.origin.column),
            row: floating_pane_rect
                .origin
                .row
                .saturating_add(pane_rect.origin.row),
        },
        Size {
            column_count: pane_rect.size.column_count,
            row_count: pane_rect
                .size
                .row_count
                .saturating_add(FLOATING_TITLEBAR_STRIP_ROW_COUNT),
        },
    )
}

/// Where `pane_id`, a pane of a floating pane, docks for `client`: an
/// insertion into `client`'s active tab beside the smallest pane, group or tab
/// span under the dock cell, on that span's edge nearest the cell
/// ([`find_insertion_at_point`]). The dock cell is the center cell of the box
/// `client` draws `pane_id` in, grown up by the titlebar strip's row. The box
/// is the pane's rect in `client`'s own view of the floating pane
/// ([`Session::compute_client_floating_pane_solve`]), counted from the first
/// cell below the titlebar strip of the rect `client` draws the floating pane
/// in ([`Session::find_client_floating_pane_rect`]). A floating pane of one
/// pane gives its own outer rect, and a collapsed stack member's box is its
/// one-row header strip. The spans are those of the solve `client` draws the
/// tab with ([`Session::compute_client_solve`]), centered in its pane area
/// ([`Size::compute_centered_origin`]): each stack shows the member that
/// `client`'s focus history gives, and a pane `client` zooms fills the tab
/// while every other pane has no span. A dock cell outside the tab moves to
/// the tab's nearest cell.
///
/// A `16x10` floating pane of one pane drawn at `(62, 6)`, over the tab
/// `A | B` that fills an `80x22` pane area with `B` at `(40, 0)`, has the dock
/// cell `(70, 11)`: the insertion right of `B`. With `A` zoomed, it is the
/// insertion right of `A`. In the floating pane `[vim | htop]` drawn `48x13`
/// at `(16, 4)`, `htop`'s box is `24x12` at `(40, 5)`, the grown box `24x13`
/// at `(40, 4)`, and the dock cell `(52, 10)`.
///
/// `None` when no floating pane holds `pane_id`, when `client` does not show
/// the floating pane that holds it ([`Client::is_floating_pane_shown`]), such
/// as one it minimized, draws no rect for it, such as with a pane area smaller
/// than `5x5`, draws `pane_id` at zero area, such as behind its own zoom on
/// another pane of the floating pane, or views a tab with no cell.
#[must_use]
pub fn find_nearest_dock_target(
    session: &Session,
    client: &Client,
    pane_id: PaneId,
) -> Option<TiledPlacementTarget> {
    let (_, floating_pane) = session
        .floating_set
        .find_floating_pane_by_pane_id(pane_id)?;
    let floating_pane_id = floating_pane.get_floating_pane_id();
    if !client.is_floating_pane_shown(floating_pane_id) {
        return None;
    }
    let floating_pane_rect = session.find_client_floating_pane_rect(client, floating_pane_id)?;
    let pane_rect = session
        .compute_client_floating_pane_solve(client, floating_pane_id)?
        .find_pane_rect(pane_id)?;
    if pane_rect.is_empty() {
        return None;
    }
    find_insertion_under_rect_center(
        session,
        client,
        compute_floating_pane_member_box(floating_pane_rect, pane_rect),
        pane_id,
    )
}

/// Where every pane of the floating pane `floating_pane_id` docks for
/// `client`, as one subtree: the insertion that [`find_nearest_dock_target`]
/// gives, with the center cell of the rect `client` draws the floating pane
/// in ([`Session::find_client_floating_pane_rect`]) as the dock cell. For a
/// floating pane of one pane, this is [`find_nearest_dock_target`]'s
/// insertion.
///
/// The floating pane `[vim | htop]` drawn `16x10` at `(62, 6)`, over the tab
/// `A | B` that fills an `80x22` pane area with `B` at `(40, 0)`, has the dock
/// cell `(70, 11)`: the insertion right of `B`.
///
/// `None` when the session holds no `floating_pane_id`, when `client` does
/// not show it ([`Client::is_floating_pane_shown`]), draws no rect for it, or
/// views a tab with no cell.
#[must_use]
pub fn find_nearest_floating_pane_dock_target(
    session: &Session,
    client: &Client,
    floating_pane_id: FloatingPaneId,
) -> Option<TiledPlacementTarget> {
    let (_, floating_pane) = session
        .floating_set
        .find_floating_pane_by_id(floating_pane_id)?;
    if !client.is_floating_pane_shown(floating_pane_id) {
        return None;
    }
    let floating_pane_rect = session.find_client_floating_pane_rect(client, floating_pane_id)?;
    let first_pane_id = *floating_pane.list_pane_ids().first()?;
    find_insertion_under_rect_center(session, client, floating_pane_rect, first_pane_id)
}

/// The insertion into `client`'s active tab beside the smallest pane, group
/// or tab span under the center cell of `dock_rect`, on that span's edge
/// nearest the cell ([`find_insertion_at_point`]). `dock_rect` is counted
/// from `client`'s pane-area origin, and `docking_pane_id` is a pane the tab
/// does not hold. The spans are those of the solve `client` draws the tab
/// with ([`Session::compute_client_solve`]), centered in its pane area
/// ([`Size::compute_centered_origin`]): each stack shows the member that
/// `client`'s focus history gives, and a pane `client` zooms fills the tab
/// while every other pane has no span. A center cell outside the tab moves
/// to the tab's nearest cell.
///
/// `None` when `client` has no pane area or views a tab with no cell.
fn find_insertion_under_rect_center(
    session: &Session,
    client: &Client,
    dock_rect: Rect,
    docking_pane_id: PaneId,
) -> Option<TiledPlacementTarget> {
    let pane_area = client.get_pane_area()?;
    let destination_tab_id = client.get_active_tab_id();
    let tab = session.tabs.get(&destination_tab_id)?;
    let tab_size = tab.get_solved_tab_size();
    if tab_size.column_count == 0 || tab_size.row_count == 0 {
        return None;
    }
    let tab_origin = pane_area.compute_centered_origin(tab_size);
    let dock_cell = Point {
        column: dock_rect
            .origin
            .column
            .saturating_add(dock_rect.size.column_count / 2)
            .saturating_sub(tab_origin.column)
            .min(tab_size.column_count - 1),
        row: dock_rect
            .origin
            .row
            .saturating_add(dock_rect.size.row_count / 2)
            .saturating_sub(tab_origin.row)
            .min(tab_size.row_count - 1),
    };
    let client_solve = session.compute_client_solve(client, destination_tab_id)?;
    let placement_destinations = list_placement_destinations(
        tab.get_layout_tree(),
        &client_solve,
        Rect::from_size_at_origin(tab_size),
        docking_pane_id,
    );
    let span_rects = placement_destinations
        .insertion_spans
        .iter()
        .map(|insertion_span| (&insertion_span.anchor, insertion_span.span_rect));
    let (anchor, direction) = find_insertion_at_point(span_rects, dock_cell)?;
    Some(TiledPlacementTarget::Split {
        destination_tab_id,
        anchor: anchor.clone(),
        direction,
    })
}

/// Dock the panes `dock_source` names into `destination_tab_id`: take them out
/// of their floating pane and install `destination_tree`, the destination
/// tab's tree with those panes placed in it
/// ([`place_docking_tree_in_tab`](koshi_layout::placement::place_docking_tree_in_tab)).
/// Returns the ordered events.
///
/// Each pane's registry record, PTY and every per-pane view stay attached to
/// its id. No pane is closed: no [`Event::PaneClosing`] and no
/// [`Event::PaneRemoved`] is emitted.
///
/// 1. The panes leave their floating pane. A [`DockSource::WholeFloatingPane`],
///    and a [`DockSource::Pane`] that is its floating pane's only pane, take
///    the floating pane out of the floating set and every client's floating
///    view ([`Session::remove_floating_pane`]), with every zoom inside it. A
///    [`DockSource::Pane`] of a floating pane of 2 or more panes leaves the
///    floating pane its desired size and its other panes: the pane's leaf
///    collapses out of its tree, the tree is solved again
///    ([`Session::update_floating_pane_layout`]), and every client zoomed on
///    the pane leaves that zoom.
/// 2. `destination_tree` becomes the destination tab's tree, solved again
///    ([`Session::update_tab_layout`]).
/// 3. The acting client, then each client whose input was on a docked pane,
///    in client id order, follows the panes: it leaves any zoom in the
///    destination tab, makes that tab its active tab and focuses its pane
///    there, and every floating pane it shows is minimized. Its pane there is
///    the first pane of its focus history inside the floating pane when that
///    pane docks, else the first docked pane. A client whose input was on a
///    docked pane focuses that pane, and the acting client of a
///    [`DockSource::Pane`] focuses the docked pane.
/// 4. The destination tab, then each tab a client of step 3 left, is solved
///    again at its tab size ([`Session::update_tab_solve`]).
/// 5. For a [`DockSource::Pane`], each client whose focused pane inside the
///    floating pane of step 1 was the pane focuses the pane that inherits it
///    there, ranked against the rect the pane held in that client's own view
///    of the floating pane before the dock. Then the pane leaves every
///    client's focus history inside that floating pane. No input moves in
///    this step: a client whose input was on the pane follows it in step 3.
///
/// No other client's input, active tab or shown floating panes change, and
/// a stack's active member stays the same: a client focused on `vim` in the
/// stack `[vim]` keeps `vim` open when `htop` is docked into that stack.
/// Docking `htop` out of the floating pane `[vim | htop]` leaves the floating
/// pane `[vim]` at its size; docking the whole of it leaves no floating pane.
///
/// Events: for a [`DockSource::Pane`] of a floating pane of 2 or more panes,
/// [`Event::FloatingPaneLayoutChanged`] with the panes left and the events of
/// its solve; then [`Event::LayoutChanged`] for the destination tab and the
/// events of its solve, then the events of each client of step 3, then the
/// events of the solves of step 4.
///
/// # Errors
///
/// Checked in this order, before any change:
/// - [`PlacementCommitError::FloatingSourceNotFound`]: no floating pane of
///   the session holds the pane of a [`DockSource::Pane`], or the session
///   holds no floating pane of a [`DockSource::WholeFloatingPane`];
/// - [`PlacementCommitError::DestinationTabNotFound`]: the session holds no
///   `destination_tab_id`;
/// - [`PlacementCommitError::PaneOwnershipConflict`]: the tree
///   `destination_tree` repeats a pane, or does not hold exactly the
///   destination tab's leaves and the docked panes;
/// - [`PlacementCommitError::ActingClientNotFound`]: `acting_client_id` is not
///   attached.
pub fn commit_dock_placement(
    session: &mut Session,
    dock_source: DockSource,
    destination_tab_id: TabId,
    destination_tree: LayoutNode,
    acting_client_id: ClientId,
) -> Result<Vec<Event>, PlacementCommitError> {
    let (source_floating_pane_id, docked_pane_ids) = match dock_source {
        DockSource::Pane(source_pane_id) => {
            let (_, source_floating_pane) = session
                .floating_set
                .find_floating_pane_by_pane_id(source_pane_id)
                .ok_or(PlacementCommitError::FloatingSourceNotFound)?;
            (
                source_floating_pane.get_floating_pane_id(),
                vec![source_pane_id],
            )
        }
        DockSource::WholeFloatingPane(source_floating_pane_id) => {
            let (_, source_floating_pane) = session
                .floating_set
                .find_floating_pane_by_id(source_floating_pane_id)
                .ok_or(PlacementCommitError::FloatingSourceNotFound)?;
            (
                source_floating_pane_id,
                source_floating_pane.list_pane_ids(),
            )
        }
    };
    let mut expected_pane_ids: HashSet<PaneId> = session
        .tabs
        .get(&destination_tab_id)
        .ok_or(PlacementCommitError::DestinationTabNotFound)?
        .get_layout_tree()
        .list_leaf_pane_ids()
        .into_iter()
        .collect();
    expected_pane_ids.extend(docked_pane_ids.iter().copied());
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
    let focused_client_rects = match dock_source {
        DockSource::Pane(source_pane_id) => list_floating_pane_focused_client_rects(
            session,
            source_floating_pane_id,
            source_pane_id,
        ),
        DockSource::WholeFloatingPane(_) => Vec::new(),
    };
    let moving_clients: Vec<(ClientId, TabId, Option<PaneId>, PaneId)> =
        list_moving_client_ids(session, &docked_pane_ids, acting_client_id)
            .into_iter()
            .filter_map(|client_id| {
                let client = session.clients.get_client_by_id(client_id)?;
                let landing_pane_id = client
                    .find_floating_pane_input_pane_id(source_floating_pane_id)
                    .filter(|focused_pane_id| docked_pane_ids.contains(focused_pane_id))
                    .unwrap_or(docked_pane_ids[0]);
                Some((
                    client_id,
                    client.get_active_tab_id(),
                    client.find_focused_floating_pane_input_pane_id(),
                    landing_pane_id,
                ))
            })
            .collect();

    let mut emitted_events = match dock_source {
        DockSource::Pane(source_pane_id) => {
            remove_pane_leaf_from_floating_pane(session, source_floating_pane_id, source_pane_id)
        }
        DockSource::WholeFloatingPane(_) => {
            let _ = session.remove_floating_pane(source_floating_pane_id);
            Vec::new()
        }
    };
    emitted_events.push(Event::LayoutChanged(LayoutChanged {
        tab_id: destination_tab_id,
    }));
    emitted_events.extend(session.update_tab_layout(destination_tab_id, destination_tree));
    for &(client_id, _, previous_floating_focused_pane_id, landing_pane_id) in &moving_clients {
        emitted_events.extend(move_client_onto_tiled_pane(
            session,
            client_id,
            destination_tab_id,
            landing_pane_id,
            previous_floating_focused_pane_id,
        ));
    }
    let left_tab_ids = moving_clients
        .iter()
        .map(|&(_, previous_tab_id, _, _)| previous_tab_id);
    emitted_events.extend(update_tab_solves(
        session,
        iter::once(destination_tab_id).chain(left_tab_ids),
    ));
    if let DockSource::Pane(source_pane_id) = dock_source {
        emitted_events.extend(repair_floating_pane_focus_after_pane_left(
            session,
            source_floating_pane_id,
            source_pane_id,
            &focused_client_rects,
        ));
    }
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
        list_moving_client_ids(session, &[source_pane_id], acting_client_id);
    let swapped_moving_client_ids = swapped_pane_id
        .map(|swapped_pane_id| {
            list_following_client_ids(session, &[swapped_pane_id], acting_client_id)
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

/// The clients that follow `moved_pane_ids` when `acting_client_id` moves
/// them: `acting_client_id` first, then every attached client whose input is
/// on a pane of `moved_pane_ids` ([`list_following_client_ids`]).
#[must_use]
pub fn list_moving_client_ids(
    session: &Session,
    moved_pane_ids: &[PaneId],
    acting_client_id: ClientId,
) -> Vec<ClientId> {
    let mut moving_client_ids = vec![acting_client_id];
    moving_client_ids.extend(list_following_client_ids(
        session,
        moved_pane_ids,
        acting_client_id,
    ));
    moving_client_ids
}

/// Every attached client other than `acting_client_id` whose input is on a
/// pane of `moved_pane_ids` ([`Client::get_active_focused_pane_id`]), in
/// client id order.
#[must_use]
pub fn list_following_client_ids(
    session: &Session,
    moved_pane_ids: &[PaneId],
    acting_client_id: ClientId,
) -> Vec<ClientId> {
    session
        .clients
        .list_attached_clients()
        .filter(|client| {
            client.get_client_id() != acting_client_id
                && client
                    .get_active_focused_pane_id()
                    .is_some_and(|input_pane_id| moved_pane_ids.contains(&input_pane_id))
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
        list_moving_client_ids(session, &[source_pane_id], acting_client_id);
    let swapped_moving_client_ids = match swapped_pane_id {
        Some(swapped_pane_id) => {
            list_following_client_ids(session, &[swapped_pane_id], acting_client_id)
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
/// [`Event::FloatingPaneMinimizedChanged`] per floating pane minimized, oldest shown
/// first, then [`Event::PaneFocused`] with `tab_id: Some(tab_id)`. Its
/// `previous_pane_id` is `previous_floating_focused_pane_id`, the pane that
/// held the client's input inside its focused floating pane before the
/// placement, else the pane that holds it there now, else the pane the client
/// focused in `tab_id` before. A client whose input was on the docked pane
/// `htop` gets `PaneFocused { pane_id: htop, previous_pane_id: Some(htop) }`.
///
/// A client that is not attached changes nothing and returns no events.
fn move_client_onto_tiled_pane(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
    previous_floating_focused_pane_id: Option<PaneId>,
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
    emitted_events.extend(build_floating_pane_minimized_changed_events(
        client_id,
        &session.floating_set,
        &tiled_pane_focus_change.minimized_floating_pane_ids,
        true,
    ));
    emitted_events.push(Event::PaneFocused(PaneFocused {
        client_id,
        tab_id: Some(tab_id),
        pane_id,
        previous_pane_id: previous_floating_focused_pane_id
            .or(tiled_pane_focus_change.previous_pane_id),
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
