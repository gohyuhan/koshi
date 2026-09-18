//! Tab operations: the state transitions for creating, closing, focusing, and
//! reordering a session's tabs.
//!
//! Each operation edits the session and returns the [`Event`]s describing what
//! changed, for the caller to emit. None spawns or kills a process, and none
//! touches a terminal: [`close_tab`] emits
//! [`Event::PaneClosing`]/[`Event::PaneRemoved`], and the runtime tears down the
//! matching PTYs (pseudo-terminals — the OS handles each pane's shell process
//! runs through) off those events.
//!
//! Tab display order is a dense `0..len` index on each [`Tab`]: a tab's index
//! *is* its position. Every operation that changes the tab set keeps it dense —
//! [`commit_new_tab`] and [`commit_profile_tab`] append, [`close_tab`] removes
//! and renumbers, [`move_tab`] reorders. [`close_tab`] and the close/quit
//! cascade both drop a tab through `close_and_refocus_tab`.

use std::time::SystemTime;

use koshi_core::event::{
    Event, PaneClosing, PaneCreated, PaneFocused, PaneProcessExited, PaneRemoved, QuitCause,
    TabClosed, TabCreated, TabFocused, TabMoved,
};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::tree::LayoutNode;

use crate::client::Client;
use crate::session::lifecycle::SessionLifecycleEvent;
use crate::session::pane_ops::{register_running_pane, NewPaneSpec};
use crate::session::state::{Session, Tab};

/// Which tab a focus request names, resolved against the current display order
/// by [`focus_tab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabTarget {
    /// A specific tab by id.
    Id(TabId),
    /// The tab at a zero-based display position.
    Index(usize),
    /// The previous tab in display order, wrapping past the first to the last.
    Prev,
    /// The next tab in display order, wrapping past the last to the first.
    Next,
}

/// Apply an already-spawned new tab: register its root pane as `Running`,
/// append the tab after the last one, and switch the focused client onto it.
///
/// The caller mints `new_tab_id` and `new_pane_id` and spawns the root pane's
/// child under `new_pane_id` before calling. The tab takes the next dense
/// display index (`len`, the end) and records `new_pane_id` as its own
/// most-recent focus, whether or not a client is attached. The first tab of the
/// session moves it from `Starting` to `Running`; a session in any other state
/// keeps the state it has. `spec` carries the cwd and command recorded on the
/// root pane; `created_at` stamps that record.
///
/// `focus_client_id` — when given and still attached — switches onto the new tab
/// and focuses its root pane; a stale id focuses nothing, exactly like `None`.
/// Other clients never move.
///
/// Returns the focused client's *previous* tab when one was switched, and the
/// events to emit: [`Event::TabCreated`], [`Event::PaneCreated`], then — only
/// when `focus_client_id` applies — [`Event::TabFocused`] and
/// [`Event::PaneFocused`], in that order.
#[must_use]
pub fn commit_new_tab(
    session: &mut Session,
    new_tab_id: TabId,
    new_pane_id: PaneId,
    tab_name: String,
    focus_client_id: Option<ClientId>,
    spec: NewPaneSpec,
    created_at: SystemTime,
) -> (Option<TabId>, Vec<Event>) {
    let mut events = vec![];

    register_running_pane(session, new_pane_id, spec, created_at);

    let mut new_tab = Tab::from_root_pane(new_tab_id, tab_name, session.tabs.len(), new_pane_id);
    new_tab.record_focus_mru(new_pane_id);
    if session.tabs.is_empty() {
        let _ = session.update_lifecycle(SessionLifecycleEvent::FirstTabCreated);
    }
    session.tabs.insert(new_tab_id, new_tab);

    events.push(Event::TabCreated(TabCreated { tab_id: new_tab_id }));
    events.push(Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id: new_tab_id,
    }));

    // A `focus_client_id` that is no longer attached moves no view and reports no
    // previous tab.
    let mut previous_tab = None;

    if let Some(client_id) = focus_client_id {
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            let previous_tab_id = client.get_active_tab();
            previous_tab = Some(previous_tab_id);
            client.update_active_tab(new_tab_id);
            events.push(Event::TabFocused(TabFocused {
                client_id,
                tab_id: new_tab_id,
                previous_tab_id,
            }));
            let previous_pane_id = client.update_focused_pane(new_tab_id, new_pane_id);
            events.push(Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: new_tab_id,
                pane_id: new_pane_id,
                previous_pane_id,
            }));
        }
    }

    (previous_tab, events)
}

/// The panes and tree of one profile tab, bundled for [`commit_profile_tab`].
pub struct ProfileTab {
    /// One pane id per leaf, in layout order.
    pub pane_ids: Vec<PaneId>,
    /// The live tree the ids fill.
    pub layout: LayoutNode,
    /// The record spec for each pane, parallel to `pane_ids`.
    pub specs: Vec<NewPaneSpec>,
    /// Index into `pane_ids` of the pane that starts focused.
    pub focused_leaf_index: usize,
}

/// Commit a whole multi-pane tab from a profile in one shot: register every
/// pane in `tab.pane_ids` as `Running` (each already spawned under its id),
/// append the tab under `tab_name` with `tab.layout` as its tree, and record the
/// pane at `tab.focused_leaf_index` as the tab's own most-recent focus.
///
/// `pane_ids` and `specs` are parallel and in layout order — the order
/// [`koshi_layout::template::TemplateNode::list_leaf_templates`] and the tree's leaves agree
/// on — so `pane_ids[i]` fills leaf `i`. `focused_leaf_index` indexes that same order;
/// an out-of-range value falls back to `pane_ids[0]`. The tab takes the next
/// dense display index (`len`, the end). The first tab of the session moves it
/// from `Starting` to `Running`; a session in any other state keeps the state it
/// has. `created_at` stamps every pane record.
///
/// `focus_client_id` — when given and still attached — records the focus pane for
/// that client in this tab; a stale id records nothing, exactly like `None`.
/// `is_active` then decides that client's view: `true` switches it onto the tab and
/// emits [`Event::TabFocused`] and [`Event::PaneFocused`]; `false` leaves the
/// client viewing the tab it was on and emits neither.
///
/// Returns the events to emit: [`Event::TabCreated`], one
/// [`Event::PaneCreated`] per pane in layout order, then the focus pair when it
/// applies.
///
/// # Panics
///
/// Panics when `tab.pane_ids` is empty.
#[must_use]
pub fn commit_profile_tab(
    session: &mut Session,
    tab_id: TabId,
    tab: ProfileTab,
    tab_name: String,
    focus_client_id: Option<ClientId>,
    is_active: bool,
    created_at: SystemTime,
) -> Vec<Event> {
    let ProfileTab {
        pane_ids,
        layout,
        specs,
        focused_leaf_index,
    } = tab;
    let mut events = Vec::new();

    for (pane_id, spec) in pane_ids.iter().zip(specs) {
        register_running_pane(session, *pane_id, spec, created_at);
    }

    let root_pane_id = pane_ids[0];
    let focused_pane_id = pane_ids
        .get(focused_leaf_index)
        .copied()
        .unwrap_or(root_pane_id);

    let mut new_tab = Tab::from_root_pane(tab_id, tab_name, session.tabs.len(), root_pane_id);
    // Swap the single-root layout for the profile's full tree.
    new_tab.update_layout(layout);
    // The tab's own most-recent focus, recorded whether or not a client is
    // given.
    new_tab.record_focus_mru(focused_pane_id);
    if session.tabs.is_empty() {
        let _ = session.update_lifecycle(SessionLifecycleEvent::FirstTabCreated);
    }
    session.tabs.insert(tab_id, new_tab);

    events.push(Event::TabCreated(TabCreated { tab_id }));
    for pane_id in &pane_ids {
        events.push(Event::PaneCreated(PaneCreated {
            pane_id: *pane_id,
            tab_id,
        }));
    }

    if let Some(client_id) = focus_client_id {
        if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
            // The pane is recorded on the client whether or not the tab starts
            // active.
            let previous_pane_id = client.update_focused_pane(tab_id, focused_pane_id);
            if is_active {
                let previous_tab_id = client.get_active_tab();
                client.update_active_tab(tab_id);
                events.push(Event::TabFocused(TabFocused {
                    client_id,
                    tab_id,
                    previous_tab_id,
                }));
                events.push(Event::PaneFocused(PaneFocused {
                    client_id,
                    tab_id,
                    pane_id: focused_pane_id,
                    previous_pane_id,
                }));
            }
        }
    }

    events
}

/// Close `tab_id` and everything in it.
///
/// Drops the record of every pane the tab holds and emits
/// [`Event::PaneClosing`] + [`Event::PaneRemoved`] for each, in layout order —
/// the runtime kills the real processes off these events — then hands off to
/// `close_and_refocus_tab` to remove the tab, move any client viewing it to the
/// nearest surviving tab, renumber the remaining tabs densely, and quit the
/// session if no tabs remain. An unknown `tab_id` is a no-op with no events.
#[must_use]
pub fn close_tab(session: &mut Session, tab_id: TabId) -> Vec<Event> {
    let Some(tab) = session.tabs.get(&tab_id) else {
        return Vec::new();
    };
    let tab_own_panes = tab.get_layout_tree().list_leaf_pane_ids();

    let mut events = vec![];
    for pane_id in tab_own_panes {
        let _ = session.panes.remove_pane_record(pane_id);
        events.push(Event::PaneClosing(PaneClosing { pane_id }));
        events.push(Event::PaneRemoved(PaneRemoved { pane_id, tab_id }));
    }

    events.extend(close_and_refocus_tab(session, tab_id, None));

    events
}

/// Point the client `client_id` at the tab named by `target`, resolved
/// against the current display order.
///
/// [`TabTarget::Id`] focuses that tab if it exists; [`TabTarget::Index`] the tab
/// at that display position; [`TabTarget::Next`]/[`TabTarget::Prev`] step one
/// position, wrapping at the ends. An unresolvable target — unknown id,
/// out-of-range index, unattached client, a `Next`/`Prev` step from an active
/// tab the session no longer holds — and re-focusing the already-active tab are
/// no-ops with no events. A pane focus this client already holds in the target
/// tab is left as it is, so switching back restores the pane it was on. Holding
/// none there, it lands on the target tab's focus history head, else on the
/// first leaf in layout order, taking the first that has a registry record.
/// Returns one [`Event::TabFocused`], followed by one [`Event::PaneFocused`]
/// when the client lands on a pane.
#[must_use]
pub fn focus_tab(session: &mut Session, client_id: ClientId, tab_target: TabTarget) -> Vec<Event> {
    let Some(client) = session.clients.get_client_by_id(client_id) else {
        return Vec::new();
    };
    let previous_tab_id = client.get_active_tab();

    let Some(target_tab_id) = resolve_tab_target(session, previous_tab_id, tab_target) else {
        return Vec::new();
    };

    if previous_tab_id == target_tab_id {
        return Vec::new();
    }

    let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
        return Vec::new();
    };
    client.update_active_tab(target_tab_id);

    let mut events = vec![Event::TabFocused(TabFocused {
        client_id,
        tab_id: target_tab_id,
        previous_tab_id,
    })];
    land_focus(session, client_id, target_tab_id, &mut events);
    events
}

/// The pane a client landing on `tab_id` focuses: the tab's focus history
/// newest-first, then its leaves in layout order, taking the first entry that
/// is a leaf of `tab_id` and has a record in the pane registry.
///
/// `None` when the session does not hold `tab_id`, and when no leaf of the tab
/// has a registry record.
fn landing_pane(session: &Session, tab_id: TabId) -> Option<PaneId> {
    let tab = session.tabs.get(&tab_id)?;
    let leaves = tab.get_layout_tree().list_leaf_pane_ids();
    tab.list_focus_mru()
        .iter()
        .copied()
        .chain(leaves.iter().copied())
        .find(|pane_id| {
            leaves.contains(pane_id) && session.panes.get_pane_record_by_id(*pane_id).is_some()
        })
}

/// Focus `tab_id`'s landing pane for `client_id` and record it as the tab's
/// most-recent focus, appending one [`Event::PaneFocused`] carrying no prior
/// pane to `events`.
///
/// Changes nothing and appends nothing when the client already focuses a pane
/// in `tab_id`, when it is not attached, or when the tab has no landing pane.
fn land_focus(session: &mut Session, client_id: ClientId, tab_id: TabId, events: &mut Vec<Event>) {
    let holds_focus = session
        .clients
        .get_client_by_id(client_id)
        .is_some_and(|client| client.get_focused_pane(tab_id).is_some());
    if holds_focus {
        return;
    }

    let Some(pane_id) = landing_pane(session, tab_id) else {
        return;
    };
    let Some(client) = session.clients.get_client_mut_by_id(client_id) else {
        return;
    };
    client.update_focused_pane(tab_id, pane_id);
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.record_focus_mru(pane_id);
    }
    events.push(Event::PaneFocused(PaneFocused {
        client_id,
        tab_id,
        pane_id,
        previous_pane_id: None,
    }));
}

/// Resolve a [`TabTarget`] to a concrete tab id against the current display
/// order.
///
/// `Next`/`Prev` step one position from `active_tab`, wrapping around the ends.
/// Resolves to `None` for an `Id` the session does not hold, for an `Index`
/// outside `0..len`, and for `Next`/`Prev` when `active_tab` itself is not in
/// the session.
#[must_use]
pub fn resolve_tab_target(
    session: &Session,
    active_tab: TabId,
    tab_target: TabTarget,
) -> Option<TabId> {
    match tab_target {
        TabTarget::Id(tab_id) => session.tabs.contains_key(&tab_id).then_some(tab_id),
        TabTarget::Index(tab_index) => tab_at_index(session, tab_index),
        TabTarget::Next => {
            let tab_count = session.tabs.len();
            let current_tab_index = session.tabs.get(&active_tab)?.get_tab_index();
            tab_at_index(session, (current_tab_index + 1) % tab_count)
        }
        TabTarget::Prev => {
            let tab_count = session.tabs.len();
            let current_tab_index = session.tabs.get(&active_tab)?.get_tab_index();
            tab_at_index(session, (current_tab_index + tab_count - 1) % tab_count)
        }
    }
}

/// The tab at display position `index` (dense `0..len`), if one sits there.
fn tab_at_index(session: &Session, tab_index: usize) -> Option<TabId> {
    session
        .tabs
        .values()
        .find(|tab| tab.get_tab_index() == tab_index)
        .map(Tab::get_tab_id)
}

/// Move `tab_id` to display position `new_index`, keeping the index dense.
///
/// `new_index` is clamped to `[0, len-1]`. The other tabs close ranks around the
/// moved one so the final order is still `0..len` with the target at
/// `new_index`. A no-op when the tab is unknown or already at that position.
/// Returns a single [`Event::TabMoved`]; the tabs that shift to make room do not
/// emit events of their own.
#[must_use]
pub fn move_tab(session: &mut Session, target_tab_id: TabId, new_tab_index: usize) -> Vec<Event> {
    let Some(previous_tab_index) = session
        .tabs
        .get(&target_tab_id)
        .map(|tab| tab.get_tab_index())
    else {
        return Vec::new();
    };

    // The target exists, so `len` is at least 1.
    let new_tab_index = new_tab_index.min(session.tabs.len() - 1);

    if new_tab_index == previous_tab_index {
        return Vec::new();
    }

    // 1. Renumber the other tabs densely, leaving the target's slot free.
    for (tab_position, current_tab_id) in tab_ids_in_display_order(session)
        .into_iter()
        .filter(|current_tab_id| *current_tab_id != target_tab_id)
        .enumerate()
    {
        let settled_tab_index = if tab_position >= new_tab_index {
            tab_position + 1
        } else {
            tab_position
        };
        if let Some(tab) = session.tabs.get_mut(&current_tab_id) {
            tab.update_tab_index(settled_tab_index);
        }
    }

    // 2. Drop the target into its new slot.
    if let Some(tab) = session.tabs.get_mut(&target_tab_id) {
        tab.update_tab_index(new_tab_index);
    }

    vec![Event::TabMoved(TabMoved {
        tab_id: target_tab_id,
        previous_tab_index,
        new_tab_index,
    })]
}

/// Remove an already-emptied `tab_id` and settle the fallout.
///
/// Emits [`Event::TabClosed`], moves every client off the tab — dropping the
/// pane focus and the zoom it held there, and sending any client that was
/// viewing it to the nearest surviving tab with [`Event::TabFocused`] and, for
/// a client holding no pane focus there, [`Event::PaneFocused`] on that tab's
/// landing pane — renumbers the survivors densely, and emits [`Event::Quit`]
/// with [`QuitCause::LastTabClosed`] naming `tab_id` and `pane_exit` when no
/// tabs remain. `pane_exit` is the child exit that emptied the tab, and `None`
/// when a command closed the pane or the tab. With no surviving tab to move
/// to, a viewer's `active_tab` keeps naming the removed tab. Shared by
/// [`close_tab`] and the close/quit cascade's empty-tab path. The caller
/// removes the tab's panes first (if any); this handles the tab and above.
#[must_use]
pub(crate) fn close_and_refocus_tab(
    session: &mut Session,
    tab_id: TabId,
    pane_exit: Option<PaneProcessExited>,
) -> Vec<Event> {
    let mut events = vec![];

    let closed_index = session.tabs.remove(&tab_id).map(|tab| tab.get_tab_index());
    events.push(Event::TabClosed(TabClosed { tab_id }));

    // Move every client off the closed tab: drop its focus and zoom for the
    // gone tab, and send whoever was viewing it to the nearest surviving tab.
    let next_tab =
        closed_index.and_then(|closed_tab_index| nearest_surviving_tab(session, closed_tab_index));
    let viewers: Vec<ClientId> = session
        .clients
        .list_attached_clients()
        .filter(|client| client.get_active_tab() == tab_id)
        .map(Client::get_client_id)
        .collect();
    for client in session.clients.list_attached_clients_mut() {
        client.remove_focused_pane(tab_id);
    }
    if let Some(next) = next_tab {
        for client_id in viewers {
            if let Some(client) = session.clients.get_client_mut_by_id(client_id) {
                client.update_active_tab(next);
            }
            events.push(Event::TabFocused(TabFocused {
                client_id,
                tab_id: next,
                previous_tab_id: tab_id,
            }));
            land_focus(session, client_id, next, &mut events);
        }
    }

    reindex_tab_index(session);

    if session.tabs.is_empty() {
        // An already `Stopping` or `Stopped` session keeps the state it has;
        // `Quit` is emitted either way.
        let _ = session.update_lifecycle(SessionLifecycleEvent::StopRequested);
        events.push(Event::Quit(QuitCause::LastTabClosed { tab_id, pane_exit }));
    }

    events
}

/// Renumber every tab to a dense `0..len` index in current display order,
/// closing any gap a removal left. Reordering only — emits no events.
fn reindex_tab_index(session: &mut Session) {
    for (tab_position, tab_id) in tab_ids_in_display_order(session).into_iter().enumerate() {
        if let Some(tab) = session.tabs.get_mut(&tab_id) {
            tab.update_tab_index(tab_position);
        }
    }
}

/// Every tab of the session in display order, lowest index first. Tabs sharing
/// an index keep their id order.
fn tab_ids_in_display_order(session: &Session) -> Vec<TabId> {
    let mut tab_ids: Vec<TabId> = session.tabs.keys().copied().collect();
    tab_ids.sort_by_key(|tab_id| session.tabs[tab_id].get_tab_index());
    tab_ids
}

/// The surviving tab nearest `closed_index` in display order: the previous tab
/// (largest index below it) if one exists, otherwise the next (smallest index
/// above it). `None` when no tabs remain.
fn nearest_surviving_tab(session: &Session, closed_index: usize) -> Option<TabId> {
    let previous = session
        .tabs
        .values()
        .filter(|tab| tab.get_tab_index() < closed_index)
        .max_by_key(|tab| tab.get_tab_index());
    let next_tab = session
        .tabs
        .values()
        .filter(|tab| tab.get_tab_index() > closed_index)
        .min_by_key(|tab| tab.get_tab_index());
    previous.or(next_tab).map(Tab::get_tab_id)
}

#[cfg(test)]
mod tests;
