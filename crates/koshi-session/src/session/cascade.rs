//! The close/quit cascade: removing a pane and following the consequences up
//! through the tab and the session.
//!
//! A pane leaves for one of two reasons — its shell exited, or a client asked
//! to close it — and both run the *same* removal routine. [`apply_child_exit`] is
//! the shell-exit entry: it emits the exit event and hands off to
//! [`remove_pane_cascade`]. A user close
//! enters [`remove_pane_cascade`] directly, so a self-exiting shell and an
//! explicit close converge on identical behaviour.
//!
//! [`remove_pane_cascade`] is the cascade proper: drop the pane, collapse the
//! layout, repair each affected client's focus, and — if that empties the tab —
//! close the tab, and if that empties the session, quit. A floating pane holds
//! no leaf: [`remove_floating_pane`] drops it from the registry, the floating
//! set and every client's view, moves the input of each client it held, and
//! closes nothing above it. Each function
//! returns the events describing what it did, for the caller to emit; none
//! touches the terminal or spawns a process.

use std::collections::HashSet;

use koshi_core::event::{
    Event, LayoutChanged, PaneClosing, PaneProcessExited, PaneRemoved, TerminalTooSmallCause,
    TerminalTooSmallEntered,
};
use koshi_core::geometry::{PaneArea, Rect};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::edit::{remove_pane, RemoveError};
use koshi_layout::focus::compute_focus_candidates;
use koshi_layout::mode::LayoutMode;
use koshi_layout::normalize::normalize_layout_tree;
use koshi_layout::solver::LayoutSolve;

use crate::client::{compute_default_pane_area_size, Client};
use crate::session::focus::{
    build_pane_focused_event, repair_focus, update_client_tab_focus, FocusRepairResult,
};
use crate::session::state::Session;
use crate::session::tab_ops::{close_and_refocus_tab, find_landing_pane};

/// Remove `pane_id` from `tab_id` and follow the consequences up the tree.
///
/// The shared removal routine behind both a closed pane and a self-exiting
/// shell:
/// 1. drop the pane from the registry, emit [`Event::PaneClosing`] and
///    [`Event::PaneRemoved`], and drop the pane from the tab's focus history;
/// 2. collapse its leaf out of the layout and solve the tab again
///    ([`Session::update_tab_layout`]), before focus repair ranks its
///    candidates, and emit [`Event::LayoutChanged`];
/// 3. drop the zoom of every client zoomed on the removed pane, returning
///    those clients to their tiled view; a client zoomed on a surviving pane
///    keeps its zoom;
/// 4. for every client focused on it, pick the inheriting focus with
///    [`repair_focus`] and apply the verdict;
/// 5. if the tab is now empty, close the tab; closing the last tab quits the session with
///    [`QuitCause::LastTabClosed`](koshi_core::event::QuitCause::LastTabClosed) carrying
///    `pane_exit`.
///
/// `pane_exit` is the child exit that removes the pane, and `None` when a
/// command removes it. Returns the events for the caller to emit. An unknown
/// pane, and a tab id the session does not hold, each change nothing and emit
/// no events.
#[must_use]
pub fn remove_pane_cascade(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    pane_exit: Option<PaneProcessExited>,
) -> Vec<Event> {
    // Both checks run before anything is removed: an unknown pane and an
    // unknown tab each leave the session as it was.
    if !session.tabs.contains_key(&tab_id) || session.panes.remove_pane_record(pane_id).is_none() {
        return Vec::new();
    }
    let mut emitted_events = vec![
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved {
            pane_id,
            tab_id: Some(tab_id),
        }),
    ];
    emitted_events.extend(remove_pane_from_tab(session, tab_id, pane_id, pane_exit));
    emitted_events
}

/// Take `pane_id`'s leaf out of `tab_id` and follow the consequences up the
/// tree: steps 1 to 5 of [`remove_pane_cascade`] without the registry drop and
/// its two events. The pane registry keeps every record: a closed pane's
/// record is gone already, and an undocked pane's record stays. Step 4 runs
/// [`repair_focus_after_pane_left`], each client ranked against the rect the
/// pane held in its own view of the tab before the removal
/// ([`list_focused_client_rects`]).
///
/// `pane_exit` means what it means for [`remove_pane_cascade`]. A tab id the
/// session does not hold changes nothing and emits no events.
#[must_use]
pub(crate) fn remove_pane_from_tab(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    pane_exit: Option<PaneProcessExited>,
) -> Vec<Event> {
    let focused_client_rects = list_focused_client_rects(session, tab_id, pane_id);
    let Some(tab) = session.tabs.get_mut(&tab_id) else {
        return Vec::new();
    };
    tab.remove_focus_mru(pane_id);
    let mut emitted_events = Vec::new();

    // Removing the only pane yields `LastPane`: the tab is now empty. The
    // removal edit leaves canonicalization to `normalize_layout_tree`, which
    // collapses the unary split the removed leaf leaves behind; every
    // surviving leaf is live, so the pass canonicalizes shape only and drops
    // nothing. A pane that is no leaf of the tab leaves the layout as it is,
    // emits no `LayoutChanged`, and still moves zoom and focus off the pane.
    let is_tab_empty = match remove_pane(tab.get_layout_tree(), pane_id) {
        Ok(edited_layout_tree) => {
            let live_pane_ids: HashSet<PaneId> = edited_layout_tree
                .list_leaf_pane_ids()
                .into_iter()
                .collect();
            let canonical_layout_tree = normalize_layout_tree(&edited_layout_tree, &live_pane_ids)
                .unwrap_or(edited_layout_tree);
            emitted_events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
            emitted_events.extend(session.update_tab_layout(tab_id, canonical_layout_tree));
            false
        }
        Err(RemoveError::LastPane { .. }) => true,
        Err(RemoveError::PaneNotFound { .. }) => false,
    };

    // Every client zoomed on the removed pane returns to its tiled view. A
    // client zoomed on a pane that survives keeps its zoom.
    for client in session.clients.list_attached_clients_mut() {
        client.clear_zoom_of_pane(pane_id);
    }

    if is_tab_empty {
        emitted_events.extend(close_and_refocus_tab(session, tab_id, pane_exit));
    } else {
        emitted_events.extend(repair_focus_after_pane_left(
            session,
            tab_id,
            pane_id,
            &focused_client_rects,
        ));
    }
    emitted_events
}

/// Every attached client whose focused pane in `tab_id` is `pane_id`, in
/// client id order, each with the rect `pane_id` holds in that client's own
/// view of the tab ([`Session::compute_client_solve`]). The rect is empty at
/// the origin when that view holds none. Called before the pane leaves the
/// tab, to rank the focus repair of [`repair_focus_after_pane_left`].
#[must_use]
pub(crate) fn list_focused_client_rects(
    session: &Session,
    tab_id: TabId,
    pane_id: PaneId,
) -> Vec<(ClientId, Rect)> {
    session
        .clients
        .list_attached_clients()
        .filter(|client| client.get_focused_pane_id(tab_id) == Some(pane_id))
        .map(|client| {
            let pane_rect = session
                .compute_client_solve(client, tab_id)
                .and_then(|client_solve| client_solve.find_pane_rect(pane_id))
                .unwrap_or_else(Rect::build_empty_at_origin);
            (client.get_client_id(), pane_rect)
        })
        .collect()
}

/// Repair the focus of each client of `focused_client_rects`, whose focused
/// pane `departed_pane_id` is no longer a leaf of `tab_id`, ranked against the
/// rect it names ([`repair_client_focus`]). Then take `departed_pane_id` out
/// of every attached client's focus history in `tab_id`
/// ([`Client::remove_pane_from_focus_history`]).
///
/// A client whose floating pane takes its input keeps that input. A tab id the
/// session does not hold changes no focus and emits no events.
#[must_use]
pub(crate) fn repair_focus_after_pane_left(
    session: &mut Session,
    tab_id: TabId,
    departed_pane_id: PaneId,
    focused_client_rects: &[(ClientId, Rect)],
) -> Vec<Event> {
    let mut emitted_events = Vec::new();
    for &(client_id, departed_pane_rect) in focused_client_rects {
        emitted_events.extend(repair_client_focus(
            session,
            tab_id,
            client_id,
            departed_pane_rect,
        ));
    }
    for client in session.clients.list_attached_clients_mut() {
        client.remove_pane_from_focus_history(tab_id, departed_pane_id);
    }
    emitted_events
}

/// Move the focus of every attached client that views `tab_id` tiled and
/// whose focused pane in it the tab's solve suppresses, to the pane that
/// inherits it ([`repair_client_focus`]), ranked against that pane's rect in
/// `previous_layout_solve`, else in the tab's current solve. Suppressed panes
/// are handled in the solve's suppressed order, and the clients focused on
/// one pane in client id order.
///
/// A client zoomed in the tab, a client that does not view the tab, and a
/// tab whose solve suppresses every pane, keep their focus. A tab id the
/// session does not hold changes nothing and emits no events.
///
/// `A | B | C` with a solve that suppresses `C`: a client focused on `C`
/// whose focus history is `C, A, B` focuses `A`.
#[must_use]
pub(crate) fn hand_off_suppressed_focus(
    session: &mut Session,
    tab_id: TabId,
    previous_layout_solve: Option<&LayoutSolve>,
) -> Vec<Event> {
    let Some(tab) = session.tabs.get(&tab_id) else {
        return Vec::new();
    };
    let layout_solve = tab.get_layout_solve();
    if layout_solve.is_every_pane_suppressed {
        return Vec::new();
    }
    let mut repaired_client_rects: Vec<(ClientId, Rect)> = Vec::new();
    for &suppressed_pane_id in &layout_solve.suppressed_pane_ids {
        let ranking_pane_rect = previous_layout_solve
            .and_then(|previous_layout_solve| {
                previous_layout_solve.find_pane_rect(suppressed_pane_id)
            })
            .or_else(|| layout_solve.find_pane_rect(suppressed_pane_id))
            .unwrap_or_else(Rect::build_empty_at_origin);
        repaired_client_rects.extend(
            session
                .clients
                .list_attached_clients()
                .filter(|client| {
                    client.get_active_tab_id() == tab_id
                        && client.get_layout_mode(tab_id) == LayoutMode::Tiled
                        && client.get_focused_pane_id(tab_id) == Some(suppressed_pane_id)
                })
                .map(|client| (client.get_client_id(), ranking_pane_rect)),
        );
    }
    let mut emitted_events = Vec::new();
    for (client_id, ranking_pane_rect) in repaired_client_rects {
        emitted_events.extend(repair_client_focus(
            session,
            tab_id,
            client_id,
            ranking_pane_rect,
        ));
    }
    emitted_events
}

/// Move `client_id`'s focus in `tab_id` to the pane that inherits it from a
/// pane that held `ranking_pane_rect`.
///
/// The candidates come from the client's own view of the tab
/// ([`Session::compute_client_solve`], [`compute_focus_candidates`]), ranked
/// against `ranking_pane_rect`, and [`repair_focus`] picks the verdict from
/// the client's focus history ([`Client::list_focus_history`]). The
/// inheriting pane is the verdict's pane for [`FocusRepairResult::Focused`],
/// and the client's landing pane ([`find_landing_pane`]) for
/// [`FocusRepairResult::TerminalTooSmall`]. No inheriting pane is a stack
/// member collapsed to its header in the client's view. The tab records the
/// inheriting pane as its most-recent focus. Then the client:
/// 1. focuses the inheriting pane and emits [`Event::PaneFocused`] naming the
///    pane it focused before as `previous_pane_id`. With no inheriting pane,
///    its focus history and zoom in the tab are removed and no
///    [`Event::PaneFocused`] is emitted;
/// 2. for [`FocusRepairResult::TerminalTooSmall`], emits
///    [`Event::TerminalTooSmallEntered`] when `tab_id` is its active tab.
///
/// A tab `A | B` solved at `3x1` has no room for `A`: a client focused on `B`
/// when `B` leaves focuses `A`, and its keys reach `A` once the tab has room
/// for it. A stack `[A, B, C]` with `C` active, solved at `3x1`, when `C`
/// leaves: a client with focus history `C, A, B` sees `A` open and focuses
/// `A`, and a client with focus history `C` sees `B`, the member the removal
/// makes active, open and focuses `B`.
///
/// A client that is not attached, and a tab id the session does not hold,
/// change nothing and emit no events.
fn repair_client_focus(
    session: &mut Session,
    tab_id: TabId,
    client_id: ClientId,
    ranking_pane_rect: Rect,
) -> Vec<Event> {
    let Some(client) = session.clients.get_client_by_id(client_id) else {
        return Vec::new();
    };
    let Some(client_solve) = session.compute_client_solve(client, tab_id) else {
        return Vec::new();
    };
    let candidates = compute_focus_candidates(
        ranking_pane_rect,
        &client_solve.pane_rects,
        &client_solve.stack_headers,
    );
    let verdict = repair_focus(
        client.list_focus_history(tab_id),
        &session.panes,
        candidates,
    );
    let inheriting_pane_id = match verdict {
        FocusRepairResult::Focused(focused_pane_id) => Some(focused_pane_id),
        FocusRepairResult::TerminalTooSmall => find_landing_pane(session, client, tab_id),
    };

    let mut emitted_events = Vec::new();
    match inheriting_pane_id {
        Some(inheriting_pane_id) => {
            emitted_events.extend(update_client_tab_focus(
                session,
                client_id,
                tab_id,
                inheriting_pane_id,
            ));
        }
        None => {
            if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
                client.clear_focus_history(tab_id);
            }
        }
    }
    if verdict == FocusRepairResult::TerminalTooSmall {
        if let Some(client) = session
            .clients
            .get_client_by_id(client_id)
            .filter(|client| client.get_active_tab_id() == tab_id)
        {
            emitted_events.push(Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                client_id,
                viewport_size: client.get_viewport_size(),
                pane_area: client.get_reported_pane_area(),
                cause: resolve_terminal_too_small_cause(session, tab_id, client_id),
            }));
        }
    }
    emitted_events
}

/// Classify why `client_id` has no visible pane area in `tab_id`.
///
/// Returns [`TerminalTooSmallCause::Regions`] when the client reported
/// [`PaneArea::Starving`], or reported an area smaller on either axis than its
/// viewport minus the two chrome rows. Returns
/// [`TerminalTooSmallCause::OtherClient`] naming another viewer of `tab_id`
/// whose own pane area sets the constraining axis of the tab's pane region.
/// Returns [`TerminalTooSmallCause::Terminal`] in every other case, including
/// an unattached `client_id` and a tab no client contributes a size to.
fn resolve_terminal_too_small_cause(
    session: &Session,
    tab_id: TabId,
    client_id: ClientId,
) -> TerminalTooSmallCause {
    let Some(client) = session.clients.get_client_by_id(client_id) else {
        return TerminalTooSmallCause::Terminal;
    };

    match client.get_reported_pane_area() {
        Some(PaneArea::Starving) => return TerminalTooSmallCause::Regions,
        Some(PaneArea::Reported(reported_pane_area)) => {
            let default_pane_area = compute_default_pane_area_size(client.get_viewport_size());
            let clamped_pane_area =
                reported_pane_area.compute_minimum_axes(client.get_viewport_size());
            if !default_pane_area.can_fit_inside(clamped_pane_area) {
                return TerminalTooSmallCause::Regions;
            }
        }
        None => {}
    }

    let Some(own_pane_area) = client.get_pane_area() else {
        return TerminalTooSmallCause::Regions;
    };
    let Some(tab_size) = session.get_tab_size(tab_id) else {
        return TerminalTooSmallCause::Terminal;
    };

    if let Some(other_client) = session
        .clients
        .list_attached_clients()
        .filter(|other_viewer| {
            other_viewer.get_client_id() != client_id && other_viewer.get_active_tab_id() == tab_id
        })
        .find(|other_viewer| {
            let Some(other_pane_area) = other_viewer.get_pane_area() else {
                return false;
            };
            let is_column_constraint = tab_size.column_count < own_pane_area.column_count
                && other_pane_area.column_count == tab_size.column_count;
            let is_row_constraint = tab_size.row_count < own_pane_area.row_count
                && other_pane_area.row_count == tab_size.row_count;
            is_column_constraint || is_row_constraint
        })
    {
        return TerminalTooSmallCause::OtherClient(other_client.get_client_id());
    }

    TerminalTooSmallCause::Terminal
}

/// Remove the floating pane `pane_id`: drop its registry record, its member
/// entry in the floating set, and every attached client's view of it
/// ([`Session::remove_floating_member`]). No layout changes and no tab closes.
/// A client whose floating focus was on `pane_id` focuses the newest floating
/// pane it still shows, else its input returns to the pane it focused in its
/// active tab.
///
/// Returns [`Event::PaneClosing`] then [`Event::PaneRemoved`] with
/// `tab_id: None`, then, in client id order, one [`Event::PaneFocused`] with
/// `previous_pane_id: Some(pane_id)` for each client whose input `pane_id`
/// took and that has a pane to take its input now
/// ([`build_pane_focused_event`]). A `pane_id` that is not a floating member
/// changes nothing and returns no events.
#[must_use]
pub fn remove_floating_pane(session: &mut Session, pane_id: PaneId) -> Vec<Event> {
    let focusing_client_ids: Vec<ClientId> = session
        .clients
        .list_attached_clients()
        .filter(|client| client.get_focused_floating_pane_id() == Some(pane_id))
        .map(Client::get_client_id)
        .collect();
    if session.remove_floating_member(pane_id).is_none() {
        return Vec::new();
    }
    session.panes.remove_pane_record(pane_id);
    let mut emitted_events = vec![
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved {
            pane_id,
            tab_id: None,
        }),
    ];
    for client_id in focusing_client_ids {
        if let Some(client) = session.clients.get_client_by_id(client_id) {
            emitted_events.extend(build_pane_focused_event(client, Some(pane_id)));
        }
    }
    emitted_events
}

/// Handle a pane's child process exiting.
///
/// Emits a process-exited event unconditionally, then removes the pane through
/// [`remove_pane_cascade`], so a self-exiting shell tears down exactly like an
/// explicit close.
///
/// `pane_exit` is the exit fact for its pane; a quit the removal reaches
/// carries it as
/// [`QuitCause::LastTabClosed`](koshi_core::event::QuitCause::LastTabClosed)'s
/// `pane_exit`. An unknown pane emits only the exit event.
#[must_use]
pub fn apply_child_exit(
    session: &mut Session,
    tab_id: TabId,
    pane_exit: PaneProcessExited,
) -> Vec<Event> {
    let pane_id = pane_exit.pane_id;
    let mut emitted_events = vec![Event::PaneProcessExited(pane_exit)];
    if session.panes.get_pane_record_by_id(pane_id).is_none() {
        return emitted_events;
    }
    emitted_events.extend(remove_pane_cascade(
        session,
        tab_id,
        pane_id,
        Some(pane_exit),
    ));
    emitted_events
}

#[cfg(test)]
mod tests;
