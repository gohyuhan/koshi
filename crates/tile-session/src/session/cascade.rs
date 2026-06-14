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

use std::time::SystemTime;

use tile_core::event::{
    Event, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved, TerminalTooSmallEntered,
};
use tile_core::geometry::Rect;
use tile_core::ids::{ClientId, PaneId, TabId};
use tile_layout::edit::{remove_pane, RemoveError};
use tile_layout::focus::focus_candidates;
use tile_layout::solver::solve_with_mode;
use tile_pane::pane::lifecycle::PaneLifecycleEvent;
use tile_pane::pane::policy::PaneExitPolicy;

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
/// 3. for every client focused on it, pick the inheriting focus with
///    [`repair_focus`] and apply the verdict;
/// 4. if the tab is now empty, apply `empty_tab_policy` —
///    [`EmptyTabPolicy::CloseTab`] closes the tab, and closing the last tab
///    quits the session.
///
/// `tab_rect` is the viewport the tab is solved against, needed to rank focus
/// candidates geometrically. Returns the events for the caller to emit; an
/// unknown pane or tab is a no-op with no events.
#[must_use]
pub fn remove_pane_cascade(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    tab_rect: Rect,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    // An unknown pane id is a no-op: nothing was removed, so nothing happened.
    if session.panes.remove(pane_id).is_none() {
        return Vec::new();
    }
    // The tab must exist; same guard.
    let Some(tab) = session.tabs.get_mut(&tab_id) else {
        return Vec::new();
    };

    let mut events = vec![
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
    ];

    tab.remove_focus_mru(pane_id);

    // Collapse the layout *before* repairing focus, so the tree never
    // references the removed pane while candidates are computed. Removing the
    // only pane yields `LastPane` — the signal that the tab is now empty.
    let removal = match remove_pane(&tab.layout, tab_rect, pane_id) {
        Ok((new_tree, info)) => {
            tab.layout = new_tree;
            Some(info)
        }
        Err(RemoveError::LastPane { .. }) => None,
        // The pane was in the registry but not the layout: a registry/layout
        // desync that the removal pipeline upholds against. Nothing left to
        // collapse; the removal events already emitted stand.
        Err(RemoveError::PaneNotFound { .. }) => return events,
    };

    match removal {
        // The tab still has panes: repair focus for every client that was
        // looking at the removed pane.
        Some(info) => {
            let verdicts: Vec<(ClientId, FocusRepairResult)> = {
                let tab = &session.tabs[&tab_id];
                let solved = solve_with_mode(&tab.layout, tab.layout_mode, tab_rect);
                let candidates =
                    focus_candidates(info.old_rect, &solved.panes, &solved.stack_headers);
                session
                    .clients
                    .list_attached()
                    .filter(|client| client.focused_pane(tab_id) == Some(pane_id))
                    .map(|client| client.id())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|client_id| {
                        let verdict =
                            repair_focus(tab, &session.panes, candidates.clone(), empty_tab_policy);
                        (client_id, verdict)
                    })
                    .collect()
            };

            for (client_id, verdict) in verdicts {
                match verdict {
                    FocusRepairResult::Focused(new_pane) => {
                        if let Some(client) = session.clients.get_mut(client_id) {
                            client.update_focused_pane(tab_id, new_pane);
                        }
                        if let Some(tab) = session.tabs.get_mut(&tab_id) {
                            tab.record_focus_mru(new_pane);
                        }
                        events.push(Event::PaneFocused(PaneFocused {
                            pane_id: new_pane,
                            tab_id,
                        }));
                    }
                    FocusRepairResult::TerminalTooSmall => {
                        if let Some(client) = session.clients.get_mut(client_id) {
                            client.remove_focused_pane(tab_id);
                            events.push(Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                                client_id,
                                size: client.viewport(),
                            }));
                        }
                    }
                    // The tab still has a leaf here, so the no-pane verdict
                    // cannot occur.
                    FocusRepairResult::EmptyTab(_) => {}
                }
            }
        }
        // The tab is empty: its policy decides its fate.
        None => match empty_tab_policy {
            EmptyTabPolicy::CloseTab => {
                events.extend(close_and_refocus_tab(session, tab_id));
            }
            // Respawn a fresh shell into the now-empty tab instead of closing
            // it. Spawning a replacement pane is the runtime's job and needs a
            // command/spawn path that does not exist in this layer yet, so this
            // arm is intentionally inert until that lands.
            EmptyTabPolicy::RespawnShell => {}
        },
    }

    events
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
/// `exited_at` is supplied by the caller — the runtime that observed the exit —
/// rather than read from the clock here, so the timestamp crosses the IPC
/// boundary intact and tests stay deterministic. An unknown `pane_id` emits
/// only the exit event.
#[must_use]
pub fn on_child_exit(
    session: &mut Session,
    tab_id: TabId,
    pane_id: PaneId,
    exit_code: Option<i32>,
    exited_at: SystemTime,
    tab_rect: Rect,
    empty_tab_policy: EmptyTabPolicy,
) -> Vec<Event> {
    let mut events = vec![Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code,
    })];

    // Read the policy, then drop the borrow before any `&mut` use.
    let Some(pane) = session.panes.get(pane_id) else {
        return events;
    };
    let policy = pane.exit_policy;

    match policy {
        // Respawn in place: Running -> Exited -> Spawning. The actual process
        // spawn is the runtime's job; here we only advance the lifecycle. An
        // illegal step is a no-op (the pane was not Running), so applying the
        // two events in sequence settles on the right state either way.
        PaneExitPolicy::RespawnShell => {
            if let Some(pane) = session.panes.get_mut(pane_id) {
                pane.update_lifecycle(PaneLifecycleEvent::ProcessExited {
                    code: exit_code,
                    at: exited_at,
                });
                pane.update_lifecycle(PaneLifecycleEvent::Respawn);
            }
        }
        // A self-exiting shell removes its pane through the shared cascade.
        PaneExitPolicy::CloseOnExit => {
            events.extend(remove_pane_cascade(
                session,
                tab_id,
                pane_id,
                tab_rect,
                empty_tab_policy,
            ));
        }
    }

    events
}

#[cfg(test)]
mod tests;
