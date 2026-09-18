//! The close/quit cascade: removing a pane and following the consequences up
//! through the tab and the session.
//!
//! A pane leaves for one of two reasons — its shell exited, or a client asked
//! to close it — and both run the *same* removal routine. [`on_child_exit`] is
//! the shell-exit entry: it emits the exit event, consults the pane's
//! [`PaneExitPolicy`], and hands off to [`remove_pane_cascade`]. A user close
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
use koshi_pane::pane::policy::PaneExitPolicy;

use crate::client::pane_viewport;
use crate::session::focus::{repair_focus, FocusRepairResult};
use crate::session::policy::EmptyTabPolicy;
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
/// 5. if the tab is now empty, apply `empty_tab_policy` —
///    [`EmptyTabPolicy::CloseTab`] closes the tab, and closing the last tab
///    quits the session with [`QuitCause::LastTabClosed`](koshi_core::event::QuitCause::LastTabClosed) carrying `pane_exit`.
///
/// `tab_rect` is the viewport the tab is solved against, needed to rank focus
/// candidates geometrically. `sizing` carries the per-pane content minimum and
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
    sizing: PaneSizing,
    empty_tab_policy: EmptyTabPolicy,
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

    let mut events = vec![
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
    let removal = match remove_pane(tab.get_layout_tree(), tab_rect, pane_id, sizing) {
        Ok((new_tree, removal_info)) => {
            let live: HashSet<PaneId> = new_tree.list_leaf_pane_ids().into_iter().collect();
            let canonical = normalize_layout_tree(&new_tree, &live).unwrap_or(new_tree);
            tab.update_layout(canonical);
            // The layout collapsed a leaf: the tab's geometry changed. This
            // event lands ahead of every focus event.
            events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
            Some(removal_info.removed_pane_rect)
        }
        Err(RemoveError::LastPane { .. }) => None,
        // The pane was in the registry but not the layout: a registry/layout
        // desync. The layout stands unchanged, so no rect was vacated and no
        // `LayoutChanged` is emitted; zoom and focus still move off the gone
        // pane below, ranked by focus history and layout order alone.
        Err(RemoveError::PaneNotFound { .. }) => Some(Rect::empty_at_origin()),
    };

    // Every client zoomed on the removed pane returns to its tiled view. A
    // client zoomed on a pane that survives keeps its zoom.
    for client in session.clients.list_attached_clients_mut() {
        client.clear_zoom_of_pane(pane_id);
    }

    match removal {
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
                    sizing,
                );
                let candidates = compute_focus_candidates(
                    removed_pane_rect,
                    &solved.pane_rects,
                    &solved.stack_headers,
                );
                // The verdict reads the tab, the registry and the candidates,
                // nothing client-specific: every repaired client inherits the
                // same pane.
                let verdict = repair_focus(tab, &session.panes, candidates, empty_tab_policy);
                session
                    .clients
                    .list_attached_clients()
                    .filter(|client| client.get_focused_pane(tab_id) == Some(pane_id))
                    .map(|client| (client.get_client_id(), verdict))
                    .collect()
            };

            for (client_id, verdict) in verdicts {
                match verdict {
                    FocusRepairResult::Focused(new_pane) => {
                        let previous_pane_id = session
                            .clients
                            .get_client_mut_by_id(client_id)
                            .and_then(|client| client.update_focused_pane(tab_id, new_pane));
                        if let Some(tab) = session.tabs.get_mut(&tab_id) {
                            tab.record_focus_mru(new_pane);
                        }
                        events.push(Event::PaneFocused(PaneFocused {
                            client_id,
                            tab_id,
                            pane_id: new_pane,
                            previous_pane_id,
                        }));
                    }
                    FocusRepairResult::TerminalTooSmall => {
                        let cause = terminal_too_small_cause(session, tab_id, client_id, tab_rect);
                        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
                            client.remove_focused_pane(tab_id);
                            events.push(Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                                client_id,
                                viewport_size: client.get_viewport_size(),
                                pane_area: client.get_reported_pane_area(),
                                cause,
                            }));
                        }
                    }
                    // The tab remains nonempty here, making this verdict unreachable.
                    FocusRepairResult::EmptyTab(_) => {}
                }
            }
        }
        // The tab is empty: its policy decides its fate.
        None => match empty_tab_policy {
            EmptyTabPolicy::CloseTab => {
                events.extend(close_and_refocus_tab(session, tab_id, pane_exit));
            }
        },
    }

    events
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
fn terminal_too_small_cause(
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
        Some(PaneArea::Reported(reported)) => {
            let fallback = pane_viewport(client.get_viewport_size());
            let resolved = reported.compute_minimum_axes(client.get_viewport_size());
            if resolved.column_count < fallback.column_count
                || resolved.row_count < fallback.row_count
            {
                return TerminalTooSmallCause::Regions;
            }
        }
        None => {}
    }

    let Some(own_area) = client.get_pane_area() else {
        return TerminalTooSmallCause::Regions;
    };
    let Some(effective) = session.get_tab_viewport(tab_id) else {
        return TerminalTooSmallCause::Terminal;
    };

    if tab_rect.cell_size != effective {
        return TerminalTooSmallCause::Terminal;
    }

    if let Some(other_client) = session
        .clients
        .list_attached_clients()
        .filter(|other| other.get_client_id() != client_id && other.get_active_tab() == tab_id)
        .find(|other| {
            let Some(other_area) = other.get_pane_area() else {
                return false;
            };
            let sets_columns = effective.column_count < own_area.column_count
                && other_area.column_count == effective.column_count;
            let sets_rows = effective.row_count < own_area.row_count
                && other_area.row_count == effective.row_count;
            sets_columns || sets_rows
        })
    {
        return TerminalTooSmallCause::OtherClient(other_client.get_client_id());
    }

    TerminalTooSmallCause::Terminal
}

/// Handle a pane's child process exiting, applying its [`PaneExitPolicy`].
///
/// Emits a process-exited event unconditionally — the exit is a fact whatever
/// the policy — then applies [`PaneExitPolicy::CloseOnExit`]: the pane is
/// removed through [`remove_pane_cascade`], so a self-exiting shell tears down
/// exactly like an explicit close.
///
/// `sizing` carries the per-pane content minimum and the gap between split
/// children. `pane_exit` is the exit fact for `pane_id`; a quit the removal
/// reaches carries it as [`QuitCause::LastTabClosed`](koshi_core::event::QuitCause::LastTabClosed)'s `pane_exit`. An
/// unknown `pane_id` emits only the exit event.
#[must_use]
pub fn on_child_exit(
    session: &mut Session,
    tab_id: TabId,
    pane_exit: PaneProcessExited,
    tab_rect: Rect,
    sizing: PaneSizing,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    let pane_id = pane_exit.pane_id;
    let mut events = vec![Event::PaneProcessExited(pane_exit)];

    let Some(policy) = session
        .panes
        .get_pane_record_by_id(pane_id)
        .map(|pane| pane.exit_policy)
    else {
        return events;
    };

    match policy {
        // A self-exiting shell removes its pane through the shared cascade.
        PaneExitPolicy::CloseOnExit => {
            events.extend(remove_pane_cascade(
                session,
                tab_id,
                pane_id,
                tab_rect,
                sizing,
                empty_tab_policy,
                Some(pane_exit),
            ));
        }
    }

    events
}

#[cfg(test)]
mod tests;
