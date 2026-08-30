//! The close/quit cascade: removing a pane and following the consequences up
//! through the tab and the session.
//!
//! A pane leaves for one of two reasons — its shell exited, or a client asked
//! to close it — and both run the *same* removal routine. [`on_child_exit`] is
//! the shell-exit entry: it consults the pane's [`PaneExitPolicy`] and, only
//! when that policy says to remove, hands off to [`remove_pane_cascade`]. A
//! user close enters [`remove_pane_cascade`] directly, so a self-exiting shell
//! and an explicit close converge on identical behaviour.
//!
//! [`remove_pane_cascade`] is the cascade proper: drop the pane, collapse the
//! layout, repair each affected client's focus, and — if that empties the tab —
//! close the tab, and if that empties the session, quit. Each function returns
//! the events describing what it did, for the caller to emit; neither touches
//! the terminal or spawns a process.

use std::collections::HashSet;
use std::time::SystemTime;

use koshi_core::event::{
    Event, LayoutChanged, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved,
    TerminalTooSmallCause, TerminalTooSmallEntered,
};
use koshi_core::geometry::{PaneArea, Rect};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::edit::{remove_pane, RemoveError};
use koshi_layout::focus::focus_candidates;
use koshi_layout::mode::LayoutMode;
use koshi_layout::normalize::normalize;
use koshi_layout::solver::{solve_with_mode_min, PaneSizing};
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
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
///    quits the session.
///
/// `tab_rect` is the viewport the tab is solved against, needed to rank focus
/// candidates geometrically. `sizing` carries the per-pane content minimum and
/// the gap between split children. Returns the events for the caller to emit.
/// An unknown pane changes nothing and emits no events; an unknown tab emits no
/// events after the pane's registry record is dropped.
#[must_use]
pub fn remove_pane_cascade(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    tab_rect: Rect,
    sizing: PaneSizing,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    // An unknown pane id changes nothing and emits no events.
    if session.panes.remove(pane_id).is_none() {
        return Vec::new();
    }
    // A tab id the session does not hold ends the cascade here, with no events;
    // the pane's registry record is already gone.
    let Some(tab) = session.tabs.get_mut(&tab_id) else {
        return Vec::new();
    };

    let mut events = vec![
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
    ];

    tab.remove_focus_mru(pane_id);

    // Collapses the layout *before* focus repair: the tree names no removed
    // pane while candidates are computed. Removing the only pane yields
    // `LastPane` — the signal that the tab is now empty. The removal edit
    // leaves canonicalization to `normalize`, which collapses the unary split
    // the removed leaf leaves behind; every surviving leaf is live, so the pass
    // canonicalizes shape only and drops nothing.
    // `Some` carries the rect the pane vacated, which ranks the spatial focus
    // candidates; `None` means the tab is now empty.
    let removal = match remove_pane(tab.layout(), tab_rect, pane_id, sizing) {
        Ok((new_tree, info)) => {
            let live: HashSet<PaneId> = new_tree.leaf_panes().into_iter().collect();
            let canonical = normalize(&new_tree, &live).unwrap_or(new_tree);
            tab.update_layout(canonical);
            // The layout collapsed a leaf: the tab's geometry changed. This
            // event lands ahead of every focus event.
            events.push(Event::LayoutChanged(LayoutChanged { tab_id }));
            Some(info.old_rect)
        }
        Err(RemoveError::LastPane { .. }) => None,
        // The pane was in the registry but not the layout: a registry/layout
        // desync. The layout stands unchanged, so no rect was vacated and no
        // `LayoutChanged` is emitted; zoom and focus still move off the gone
        // pane below, ranked by focus history and layout order alone.
        Err(RemoveError::PaneNotFound { .. }) => Some(Rect::zero()),
    };

    // Every client zoomed on the removed pane returns to its tiled view. A
    // client zoomed on a pane that survives keeps its zoom.
    for client in session.clients.list_attached_mut() {
        client.clear_zoom_of_pane(pane_id);
    }

    match removal {
        // The tab still has panes: repair focus for every client that was
        // looking at the removed pane.
        Some(old_rect) => {
            let verdicts: Vec<(ClientId, FocusRepairResult)> = {
                let tab = &session.tabs[&tab_id];
                // Candidates are ranked against the tiled solve. Every client
                // repaired here was focused on the removed pane; zoom follows
                // focus, and the loop above dropped every zoom on that pane.
                let solved = solve_with_mode_min(tab.layout(), LayoutMode::Tiled, tab_rect, sizing);
                let candidates = focus_candidates(old_rect, &solved.panes, &solved.stack_headers);
                // The verdict reads the tab, the registry and the candidates,
                // nothing client-specific: every repaired client inherits the
                // same pane.
                let verdict = repair_focus(tab, &session.panes, candidates, empty_tab_policy);
                session
                    .clients
                    .list_attached()
                    .filter(|client| client.focused_pane(tab_id) == Some(pane_id))
                    .map(|client| (client.id(), verdict))
                    .collect()
            };

            for (client_id, verdict) in verdicts {
                match verdict {
                    FocusRepairResult::Focused(new_pane) => {
                        let prior_pane = session
                            .clients
                            .get_mut(client_id)
                            .and_then(|client| client.update_focused_pane(tab_id, new_pane));
                        if let Some(tab) = session.tabs.get_mut(&tab_id) {
                            tab.record_focus_mru(new_pane);
                        }
                        events.push(Event::PaneFocused(PaneFocused {
                            client_id,
                            tab_id,
                            pane_id: new_pane,
                            prior_pane,
                        }));
                    }
                    FocusRepairResult::TerminalTooSmall => {
                        let cause = terminal_too_small_cause(session, tab_id, client_id, tab_rect);
                        if let Some(client) = session.clients.get_mut(client_id) {
                            client.remove_focused_pane(tab_id);
                            events.push(Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                                client_id,
                                size: client.viewport(),
                                pane_area: client.reported_pane_area(),
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
                events.extend(close_and_refocus_tab(session, tab_id));
            }
            // `RespawnShell` leaves the empty tab in place and emits no events.
            EmptyTabPolicy::RespawnShell => {}
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
    let Some(client) = session.clients.get(client_id) else {
        return TerminalTooSmallCause::Terminal;
    };

    match client.reported_pane_area() {
        Some(PaneArea::Starving) => return TerminalTooSmallCause::Regions,
        Some(PaneArea::Reported(reported)) => {
            let fallback = pane_viewport(client.viewport());
            let resolved = reported.min_axes(client.viewport());
            if resolved.cols < fallback.cols || resolved.rows < fallback.rows {
                return TerminalTooSmallCause::Regions;
            }
        }
        None => {}
    }

    let Some(own_area) = client.pane_area() else {
        return TerminalTooSmallCause::Regions;
    };
    let Some(effective) = session.tab_viewport(tab_id) else {
        return TerminalTooSmallCause::Terminal;
    };

    if tab_rect.size != effective {
        return TerminalTooSmallCause::Terminal;
    }

    if let Some(other_client) = session
        .clients
        .list_attached()
        .filter(|other| other.id() != client_id && other.active_tab() == tab_id)
        .find(|other| {
            let Some(other_area) = other.pane_area() else {
                return false;
            };
            let sets_columns = effective.cols < own_area.cols && other_area.cols == effective.cols;
            let sets_rows = effective.rows < own_area.rows && other_area.rows == effective.rows;
            sets_columns || sets_rows
        })
    {
        return TerminalTooSmallCause::OtherClient(other_client.id());
    }

    TerminalTooSmallCause::Terminal
}

/// Handle a pane's child process exiting, applying its [`PaneExitPolicy`].
///
/// Emits a process-exited event unconditionally — the exit is a fact whatever
/// the policy — then:
/// - [`PaneExitPolicy::RespawnShell`]: advance the pane `Exited` then back to
///   `Spawning`; the runtime spawns the replacement process.
/// - [`PaneExitPolicy::CloseOnExit`]: remove the pane through
///   [`remove_pane_cascade`], so a self-exiting shell tears down exactly like an
///   explicit close.
///
/// `exited_at` is the time the caller observed the exit; this never reads the
/// clock. `sizing` carries the per-pane content minimum and the gap between
/// split children. An unknown `pane_id` emits only the exit event.
// Carries a child-exit's full context to the shared cascade: the exit fact
// (`exit_code`, `exited_at`), the reflow geometry (`tab_rect`, `sizing`), and
// the empty-tab policy.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn on_child_exit(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    exit_code: Option<i32>,
    exited_at: SystemTime,
    tab_rect: Rect,
    sizing: PaneSizing,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    let mut events = vec![Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code,
    })];

    let Some(policy) = session.panes.get(pane_id).map(|pane| pane.exit_policy) else {
        return events;
    };

    match policy {
        // Respawn in place: Running -> Exited -> Spawning. Only the lifecycle
        // advances here; the runtime spawns the process. A pane that was not
        // `Running` rejects the step and keeps the state it had.
        PaneExitPolicy::RespawnShell => {
            if let Some(pane) = session.panes.get_mut(pane_id) {
                let _ = pane.update_lifecycle(PaneLifecycleEvent::ProcessExited {
                    code: exit_code,
                    at: exited_at,
                });
                let _ = pane.update_lifecycle(PaneLifecycleEvent::Respawn);
            }
        }
        // A self-exiting shell removes its pane through the shared cascade.
        PaneExitPolicy::CloseOnExit => {
            events.extend(remove_pane_cascade(
                session,
                tab_id,
                pane_id,
                tab_rect,
                sizing,
                empty_tab_policy,
            ));
        }
    }

    events
}

#[cfg(test)]
mod tests;
