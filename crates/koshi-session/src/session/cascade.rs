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
//! close the tab, and if that empties the session, quit. Each function returns
//! the events describing what it did, for the caller to emit; neither touches
//! the terminal or spawns a process.

use std::collections::HashSet;

use koshi_core::event::{
    Event, LayoutChanged, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved,
    TerminalTooSmallCause, TerminalTooSmallEntered,
};
use koshi_core::geometry::{PaneArea, Rect};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::edit::{remove_pane, RemoveError};
use koshi_layout::focus::compute_focus_candidates;
use koshi_layout::mode::LayoutMode;
use koshi_layout::normalize::normalize_layout_tree;
use koshi_layout::solver::{solve_layout_with_mode, PaneSizing};

use crate::client::compute_default_pane_area_size;
use crate::session::focus::{repair_focus, FocusRepairResult};
use crate::session::state::Session;
use crate::session::tab_ops::close_and_refocus_tab;

/// Remove `pane_id` from `tab_id` and follow the consequences up the tree.
///
/// The shared removal routine behind both a closed pane and a self-exiting
/// shell:
/// 1. drop the pane from the registry and the tab's focus history;
/// 2. collapse its leaf out of the layout — *before* focus repair, so the tree
///    never names a gone pane while candidates are computed;
/// 3. drop the zoom of every client zoomed on the removed pane, returning
///    those clients to their tiled view; a client zoomed on a surviving pane
///    keeps its zoom;
/// 4. for every client focused on it, pick the inheriting focus with
///    [`repair_focus`] and apply the verdict;
/// 5. if the tab is now empty, close the tab; closing the last tab quits the session with
///    [`QuitCause::LastTabClosed`](koshi_core::event::QuitCause::LastTabClosed) carrying
///    `pane_exit`.
///
/// `tab_rect` is the rect the tab is solved against, needed to rank focus
/// candidates geometrically. `pane_sizing` carries the per-pane content minimum and
/// the gap between split children. `pane_exit` is the child exit that removes
/// the pane, and `None` when a command removes it. Returns the events for the
/// caller to emit. An unknown pane, and a tab id the session does not hold,
/// each change nothing and emit no events.
#[must_use]
pub fn remove_pane_cascade(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    tab_rect: Rect,
    pane_sizing: PaneSizing,
    pane_exit: Option<PaneProcessExited>,
) -> Vec<Event> {
    // Both checks run before anything is removed, so an unknown pane and an
    // unknown tab each leave the session as it was.
    if !session.tabs.contains_key(&tab_id) || session.panes.remove_pane_record(pane_id).is_none() {
        return Vec::new();
    }
    let tab = session
        .tabs
        .get_mut(&tab_id)
        .expect("the tab was checked above");

    let mut emitted_events = vec![
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
    ];

    tab.remove_focus_mru(pane_id);

    // Collapses the layout *before* focus repair: the tree names no removed
    // pane while candidates are computed. Removing the only pane yields
    // `LastPane` — the signal that the tab is now empty. The removal edit
    // leaves canonicalization to `normalize_layout_tree`, which collapses the unary split
    // the removed leaf leaves behind; every surviving leaf is live, so the pass
    // canonicalizes shape only and drops nothing.
    // `Some` carries the rect the pane vacated, which ranks the spatial focus
    // candidates; `None` means the tab is now empty.
    let removed_pane_rect = match remove_pane(tab.get_layout_tree(), tab_rect, pane_id, pane_sizing)
    {
        Ok((new_tree, pane_removal)) => {
            let live_pane_ids: HashSet<PaneId> =
                new_tree.list_leaf_pane_ids().into_iter().collect();
            let canonical_tree =
                normalize_layout_tree(&new_tree, &live_pane_ids).unwrap_or(new_tree);
            tab.update_layout(canonical_tree);
            // The layout collapsed a leaf: the tab's geometry changed. This
            // event lands ahead of every focus event.
            emitted_events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
            Some(pane_removal.removed_pane_rect)
        }
        Err(RemoveError::LastPane { .. }) => None,
        // The pane was in the registry but not the layout: a registry/layout
        // desync. The layout stands unchanged, so no rect was vacated and no
        // `LayoutChanged` is emitted; zoom and focus still move off the gone
        // pane below, ranked by focus history and layout order alone.
        Err(RemoveError::PaneNotFound { .. }) => Some(Rect::build_empty_at_origin()),
    };

    // Every client zoomed on the removed pane returns to its tiled view. A
    // client zoomed on a pane that survives keeps its zoom.
    for client in session.clients.list_attached_clients_mut() {
        client.clear_zoom_of_pane(pane_id);
    }

    match removed_pane_rect {
        // The tab still has panes: repair focus for every client that was
        // looking at the removed pane.
        Some(removed_pane_rect) => {
            let verdicts: Vec<(ClientId, FocusRepairResult)> = {
                let tab = &session.tabs[&tab_id];
                // Candidates are ranked against the tiled solve. Every client
                // repaired here was focused on the removed pane; zoom follows
                // focus, and the loop above dropped every zoom on that pane.
                let solved = solve_layout_with_mode(
                    tab.get_layout_tree(),
                    LayoutMode::Tiled,
                    tab_rect,
                    pane_sizing,
                );
                let candidates = compute_focus_candidates(
                    removed_pane_rect,
                    &solved.pane_rects,
                    &solved.stack_headers,
                );
                // The verdict reads the tab, the registry and the candidates,
                // nothing client-specific: every repaired client inherits the
                // same pane.
                let verdict = repair_focus(tab, &session.panes, candidates);
                session
                    .clients
                    .list_attached_clients()
                    .filter(|client| client.get_focused_pane_id(tab_id) == Some(pane_id))
                    .map(|client| (client.get_client_id(), verdict))
                    .collect()
            };

            for (client_id, verdict) in verdicts {
                match verdict {
                    FocusRepairResult::Focused(new_pane_id) => {
                        let previous_pane_id = session
                            .clients
                            .get_client_mut_by_id(client_id)
                            .and_then(|client| client.update_focused_pane(tab_id, new_pane_id));
                        if let Some(tab) = session.tabs.get_mut(&tab_id) {
                            tab.record_focus_mru(new_pane_id);
                        }
                        emitted_events.push(Event::PaneFocused(PaneFocused {
                            client_id,
                            tab_id,
                            pane_id: new_pane_id,
                            previous_pane_id,
                        }));
                    }
                    FocusRepairResult::TerminalTooSmall => {
                        let cause =
                            resolve_terminal_too_small_cause(session, tab_id, client_id, tab_rect);
                        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
                            client.remove_focused_pane(tab_id);
                            emitted_events.push(Event::TerminalTooSmallEntered(
                                TerminalTooSmallEntered {
                                    client_id,
                                    viewport_size: client.get_viewport_size(),
                                    pane_area: client.get_reported_pane_area(),
                                    cause,
                                },
                            ));
                        }
                    }
                }
            }
        }
        // The tab is empty: close it.
        None => emitted_events.extend(close_and_refocus_tab(session, tab_id, pane_exit)),
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
/// an unattached `client_id`, a tab no client contributes a size to, and a
/// `tab_rect` that differs from the tab's pane region.
fn resolve_terminal_too_small_cause(
    session: &Session,
    tab_id: TabId,
    client_id: ClientId,
    tab_rect: Rect,
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
            if clamped_pane_area.column_count < default_pane_area.column_count
                || clamped_pane_area.row_count < default_pane_area.row_count
            {
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

    if tab_rect.size != tab_size {
        return TerminalTooSmallCause::Terminal;
    }

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

/// Handle a pane's child process exiting.
///
/// Emits a process-exited event unconditionally, then removes the pane through
/// [`remove_pane_cascade`], so a self-exiting shell tears down exactly like an
/// explicit close.
///
/// `pane_sizing` carries the per-pane content minimum and the gap between split children. `pane_exit` is
/// the exit fact for `pane_id`; a quit the removal reaches carries it as
/// [`QuitCause::LastTabClosed`](koshi_core::event::QuitCause::LastTabClosed)'s `pane_exit`. An
/// unknown `pane_id` emits only the exit event.
#[must_use]
pub fn apply_child_exit(
    session: &mut Session,
    tab_id: TabId,
    pane_exit: PaneProcessExited,
    tab_rect: Rect,
    pane_sizing: PaneSizing,
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
        tab_rect,
        pane_sizing,
        Some(pane_exit),
    ));
    emitted_events
}

#[cfg(test)]
mod tests;
