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
    Event, PaneClosing, PaneCreated, PaneFocused, PaneRemoved, TabClosed, TabCreated, TabFocused,
    TabMoved,
};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::tree::LayoutNode;

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
/// `focus_client` — when given and still attached — switches onto the new tab
/// and focuses its root pane; a stale id focuses nothing, exactly like `None`.
/// Other clients never move.
///
/// Returns the focused client's *previous* tab when one was switched, and the
/// events to emit: [`Event::TabCreated`], [`Event::PaneCreated`], then — only
/// when `focus_client` applies — [`Event::TabFocused`] and
/// [`Event::PaneFocused`], in that order.
#[must_use]
pub fn commit_new_tab(
    session: &mut Session,
    new_tab_id: TabId,
    new_pane_id: PaneId,
    name: String,
    focus_client: Option<ClientId>,
    spec: NewPaneSpec,
    created_at: SystemTime,
) -> (Option<TabId>, Vec<Event>) {
    let mut events = vec![];

    register_running_pane(session, new_pane_id, spec, created_at);

    let mut new_tab = Tab::new(new_tab_id, name, session.tabs.len(), new_pane_id);
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

    // A `focus_client` that is no longer attached moves no view and reports no
    // previous tab.
    let mut previous_tab = None;

    if let Some(client_id) = focus_client {
        if let Some(client) = session.clients.get_mut(client_id) {
            let prior_tab = client.active_tab();
            previous_tab = Some(prior_tab);
            client.update_active_tab(new_tab_id);
            events.push(Event::TabFocused(TabFocused {
                client_id,
                tab_id: new_tab_id,
                prior_tab,
            }));
            let prior_pane = client.update_focused_pane(new_tab_id, new_pane_id);
            events.push(Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: new_tab_id,
                pane_id: new_pane_id,
                prior_pane,
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
    pub focus_leaf: usize,
}

/// Commit a whole multi-pane tab from a profile in one shot: register every
/// pane in `tab.pane_ids` as `Running` (each already spawned under its id),
/// append the tab under `name` with `tab.layout` as its tree, and record the
/// pane at `tab.focus_leaf` as the tab's own most-recent focus.
///
/// `pane_ids` and `specs` are parallel and in layout order — the order
/// [`koshi_layout::template::TemplateNode::leaves`] and the tree's leaves agree
/// on — so `pane_ids[i]` fills leaf `i`. `focus_leaf` indexes that same order;
/// an out-of-range value falls back to `pane_ids[0]`. The tab takes the next
/// dense display index (`len`, the end). The first tab of the session moves it
/// from `Starting` to `Running`; a session in any other state keeps the state it
/// has. `created_at` stamps every pane record.
///
/// `focus_client` — when given and still attached — records the focus pane for
/// that client in this tab; a stale id records nothing, exactly like `None`.
/// `active` then decides that client's view: `true` switches it onto the tab and
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
    name: String,
    focus_client: Option<ClientId>,
    active: bool,
    created_at: SystemTime,
) -> Vec<Event> {
    let ProfileTab {
        pane_ids,
        layout,
        specs,
        focus_leaf,
    } = tab;
    let mut events = Vec::new();

    for (pane_id, spec) in pane_ids.iter().zip(specs) {
        register_running_pane(session, *pane_id, spec, created_at);
    }

    let root_pane = pane_ids[0];
    let focus_pane = pane_ids.get(focus_leaf).copied().unwrap_or(root_pane);

    let mut new_tab = Tab::new(tab_id, name, session.tabs.len(), root_pane);
    // Swap the single-root layout for the profile's full tree.
    new_tab.update_layout(layout);
    // The tab's own most-recent focus, recorded whether or not a client is
    // given.
    new_tab.record_focus_mru(focus_pane);
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

    if let Some(client_id) = focus_client {
        if let Some(client) = session.clients.get_mut(client_id) {
            // The pane is recorded on the client whether or not the tab starts
            // active.
            let prior_pane = client.update_focused_pane(tab_id, focus_pane);
            if active {
                let prior_tab = client.active_tab();
                client.update_active_tab(tab_id);
                events.push(Event::TabFocused(TabFocused {
                    client_id,
                    tab_id,
                    prior_tab,
                }));
                events.push(Event::PaneFocused(PaneFocused {
                    client_id,
                    tab_id,
                    pane_id: focus_pane,
                    prior_pane,
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
    let tab_own_panes = tab.layout().leaf_panes();

    let mut events = vec![];
    for pane_id in tab_own_panes {
        let _ = session.panes.remove(pane_id);
        events.push(Event::PaneClosing(PaneClosing { pane_id }));
        events.push(Event::PaneRemoved(PaneRemoved { pane_id, tab_id }));
    }

    events.extend(close_and_refocus_tab(session, tab_id));

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
/// no-ops with no events. Only this client's active tab changes; every per-tab
/// pane focus it holds is left as it is, so switching back restores the pane it
/// was on. Returns one [`Event::TabFocused`].
#[must_use]
pub fn focus_tab(session: &mut Session, client_id: ClientId, target: TabTarget) -> Vec<Event> {
    let Some(client) = session.clients.get(client_id) else {
        return Vec::new();
    };
    let prior_tab = client.active_tab();

    let Some(target_id) = resolve_tab_target(session, prior_tab, target) else {
        return Vec::new();
    };

    if prior_tab == target_id {
        return Vec::new();
    }

    let Some(client) = session.clients.get_mut(client_id) else {
        return Vec::new();
    };
    client.update_active_tab(target_id);

    vec![Event::TabFocused(TabFocused {
        client_id,
        tab_id: target_id,
        prior_tab,
    })]
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
    target: TabTarget,
) -> Option<TabId> {
    match target {
        TabTarget::Id(id) => session.tabs.contains_key(&id).then_some(id),
        TabTarget::Index(index) => tab_at_index(session, index),
        TabTarget::Next => {
            let len = session.tabs.len();
            let current = session.tabs.get(&active_tab)?.index();
            tab_at_index(session, (current + 1) % len)
        }
        TabTarget::Prev => {
            let len = session.tabs.len();
            let current = session.tabs.get(&active_tab)?.index();
            tab_at_index(session, (current + len - 1) % len)
        }
    }
}

/// The tab at display position `index` (dense `0..len`), if one sits there.
fn tab_at_index(session: &Session, index: usize) -> Option<TabId> {
    session
        .tabs
        .values()
        .find(|tab| tab.index() == index)
        .map(|tab| tab.id())
}

/// Move `tab_id` to display position `new_index`, keeping the index dense.
///
/// `new_index` is clamped to `[0, len-1]`. The other tabs close ranks around the
/// moved one so the final order is still `0..len` with the target at
/// `new_index`. A no-op when the tab is unknown or already at that position.
/// Returns a single [`Event::TabMoved`]; the tabs that shift to make room do not
/// emit events of their own.
#[must_use]
pub fn move_tab(session: &mut Session, tab_id: TabId, new_index: usize) -> Vec<Event> {
    let Some(old_index) = session.tabs.get(&tab_id).map(|tab| tab.index()) else {
        return Vec::new();
    };

    // The target exists, so `len` is at least 1.
    let new_index = new_index.min(session.tabs.len() - 1);

    if new_index == old_index {
        return Vec::new();
    }

    // 1. Renumber the other tabs densely, leaving the target's slot free.
    for (position, id) in tab_ids_in_display_order(session)
        .into_iter()
        .filter(|&id| id != tab_id)
        .enumerate()
    {
        let settled_index = if position >= new_index {
            position + 1
        } else {
            position
        };
        if let Some(tab) = session.tabs.get_mut(&id) {
            tab.update_index(settled_index);
        }
    }

    // 2. Drop the target into its new slot.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_index(new_index);
    }

    vec![Event::TabMoved(TabMoved {
        tab_id,
        old_index,
        new_index,
    })]
}

/// Remove an already-emptied `tab_id` and settle the fallout.
///
/// Emits [`Event::TabClosed`], moves every client off the tab — dropping the
/// pane focus and the zoom it held there, and sending any client that was
/// viewing it to the nearest surviving tab with [`Event::TabFocused`] —
/// renumbers the survivors densely, and emits [`Event::Quit`] when no tabs
/// remain. With no surviving tab to move to, a viewer's `active_tab` keeps
/// naming the removed tab. Shared by [`close_tab`] and the close/quit cascade's
/// empty-tab path. The caller removes the tab's panes first (if any); this
/// handles the tab and above.
#[must_use]
pub(crate) fn close_and_refocus_tab(session: &mut Session, tab_id: TabId) -> Vec<Event> {
    let mut events = vec![];

    let closed_index = session.tabs.remove(&tab_id).map(|tab| tab.index());
    events.push(Event::TabClosed(TabClosed { tab_id }));

    // Move every client off the closed tab: drop its focus and zoom for the
    // gone tab, and send whoever was viewing it to the nearest surviving tab.
    let next_tab = closed_index.and_then(|index| nearest_surviving_tab(session, index));
    for client in session.clients.list_attached_mut() {
        client.remove_focused_pane(tab_id);
        if client.active_tab() == tab_id {
            if let Some(next) = next_tab {
                client.update_active_tab(next);
                events.push(Event::TabFocused(TabFocused {
                    client_id: client.id(),
                    tab_id: next,
                    prior_tab: tab_id,
                }));
            }
        }
    }

    reindex_tab_index(session);

    if session.tabs.is_empty() {
        // An already `Stopping` or `Stopped` session keeps the state it has;
        // `Quit` is emitted either way.
        let _ = session.update_lifecycle(SessionLifecycleEvent::StopRequested);
        events.push(Event::Quit);
    }

    events
}

/// Renumber every tab to a dense `0..len` index in current display order,
/// closing any gap a removal left. Reordering only — emits no events.
fn reindex_tab_index(session: &mut Session) {
    for (position, id) in tab_ids_in_display_order(session).into_iter().enumerate() {
        if let Some(tab) = session.tabs.get_mut(&id) {
            tab.update_index(position);
        }
    }
}

/// Every tab of the session in display order, lowest index first. Tabs sharing
/// an index keep their id order.
fn tab_ids_in_display_order(session: &Session) -> Vec<TabId> {
    let mut tab_ids: Vec<TabId> = session.tabs.keys().copied().collect();
    tab_ids.sort_by_key(|tab_id| session.tabs[tab_id].index());
    tab_ids
}

/// The surviving tab nearest `closed_index` in display order: the previous tab
/// (largest index below it) if one exists, otherwise the next (smallest index
/// above it). `None` when no tabs remain.
fn nearest_surviving_tab(session: &Session, closed_index: usize) -> Option<TabId> {
    let previous = session
        .tabs
        .values()
        .filter(|tab| tab.index() < closed_index)
        .max_by_key(|tab| tab.index());
    let next = session
        .tabs
        .values()
        .filter(|tab| tab.index() > closed_index)
        .min_by_key(|tab| tab.index());
    previous.or(next).map(|tab| tab.id())
}

#[cfg(test)]
mod tests;
