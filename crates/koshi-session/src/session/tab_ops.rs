//! Tab operations: the pure state transitions for creating, closing, renaming,
//! focusing, and reordering a session's tabs.
//!
//! Each operation mutates the session and returns the [`Event`]s describing
//! what changed, for the caller to emit. They draft
//! events and edit state only — never spawning or killing a process or touching
//! a terminal. That work is the runtime's, driven by the events returned here:
//! a [`close_tab`] emits [`Event::PaneClosing`]/[`Event::PaneRemoved`] and the
//! runtime tears down the real PTYs (pseudo-terminals — the OS handles each
//! pane's shell process runs through) off them.
//!
//! Tab display order is a dense `0..len` index on each [`Tab`]: a tab's index
//! *is* its position. Every operation that changes the tab set keeps it dense —
//! [`commit_new_tab`] appends, [`close_tab`] removes and renumbers,
//! [`move_tab`] reorders — so consumers can treat index and display position as
//! one. Closing a tab funnels through `close_and_refocus_tab`, shared with the
//! close/quit cascade, so a user-closed tab and a tab emptied by a
//! self-exiting shell tear down identically.

use std::time::SystemTime;

use koshi_core::event::{
    Event, PaneClosing, PaneCreated, PaneFocused, PaneRemoved, TabClosed, TabCreated, TabFocused,
    TabMoved, TabRenamed,
};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;

use crate::session::lifecycle::SessionLifecycleEvent;
use crate::session::pane_ops::NewPaneSpec;
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
/// The caller (the runtime) has minted `new_tab_id` and `new_pane_id` and
/// spawned the root pane's child under `new_pane_id` — so this only commits
/// state, and a failed spawn never creates a tab. The tab takes the next dense
/// display index (`len`, i.e. the end). The first tab transitions the session
/// from `Starting` to `Running`; subsequent tabs leave the lifecycle alone.
/// `spec` carries the title, cwd, and command recorded on the root pane so a
/// later restore or respawn can recover the request; `created_at` is supplied
/// by the caller so the timestamp crosses the IPC boundary intact and tests
/// stay deterministic.
///
/// `focus_client` — when given and still attached — switches onto the new tab
/// and focuses its root pane; a stale id focuses nothing, exactly like `None`.
/// Other clients never move.
///
/// Returns the focused client's *previous* tab when one was switched (so the
/// caller can reflow the tab it left), and the events to emit:
/// [`Event::TabCreated`], [`Event::PaneCreated`], then — only when
/// `focus_client` applies — [`Event::TabFocused`] and [`Event::PaneFocused`],
/// in that order.
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
    // Only a still-attached client can be focused.
    let focus = focus_client.filter(|client_id| session.clients.get(*client_id).is_some());

    let mut events = vec![];

    // Register the root pane. Its process is already live, so it enters
    // `Running` straight away; the spawn request is recorded so a later
    // restore or respawn can recover it.
    let mut record = PaneRecord::new(new_pane_id, created_at);
    record.cwd = spec.cwd;
    record.command = spec.command;
    let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    let _ = session.panes.insert(record);

    let new_tab: Tab = Tab::new(new_tab_id, name, session.tabs.len(), new_pane_id);
    // The first tab transitions the session from Starting to Running; subsequent
    // tabs are a no-op at this layer. The runtime pre-checks admission
    // (`session.lifecycle()`) before routing commands here, so this layer handles
    // state transitions only.
    if session.tabs.is_empty() {
        let _ = session.update_lifecycle(SessionLifecycleEvent::FirstTabCreated);
    }
    session.tabs.insert(new_tab_id, new_tab);

    events.push(Event::TabCreated(TabCreated { tab_id: new_tab_id }));
    events.push(Event::PaneCreated(PaneCreated {
        pane_id: new_pane_id,
        tab_id: new_tab_id,
    }));

    // Switch the focused client onto the new tab and focus its root pane,
    // remembering the tab it left for the caller to reflow.
    let mut previous_tab = None;
    if let Some(client_id) = focus {
        // `focus` was already filtered to attached clients above, so this
        // lookup should always succeed; the early return is a defensive
        // fallback rather than an expected path.
        let Some(client) = session.clients.get_mut(client_id) else {
            return (previous_tab, events);
        };
        let prior_tab = client.active_tab();
        previous_tab = Some(prior_tab);
        client.update_active_tab(new_tab_id);
        events.push(Event::TabFocused(TabFocused {
            client_id,
            tab_id: new_tab_id,
            prior_tab,
        }));
        let prior_pane = client.update_focused_pane(new_tab_id, new_pane_id);
        if let Some(tab) = session.tabs.get_mut(&new_tab_id) {
            tab.record_focus_mru(new_pane_id);
        }
        events.push(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: new_tab_id,
            pane_id: new_pane_id,
            prior_pane,
        }));
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
/// pane in `tab.pane_ids` (each already spawned under its id), create the tab
/// with `tab.layout` as its tree, and — when `focus_client` is given and still
/// attached — switch that client onto the tab and focus the pane at
/// `tab.focus_leaf`.
///
/// `pane_ids` and `specs` are parallel and in layout order — the order
/// [`koshi_layout::template::TemplateNode::leaves`] and the tree's leaves agree
/// on — so `pane_ids[i]` fills leaf `i`. `focus_leaf` indexes that same order;
/// an out-of-range value focuses the root. Genesis only, so it never moves a
/// client off a prior tab. Returns the events to emit: [`Event::TabCreated`],
/// one [`Event::PaneCreated`] per pane, then — when a client was focused —
/// [`Event::TabFocused`] and [`Event::PaneFocused`].
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
    let focus = focus_client.filter(|client_id| session.clients.get(*client_id).is_some());
    let mut events = Vec::new();

    // Register every pane. Each child is already live, so it enters `Running`
    // straight away; its spawn request is recorded for a later restore.
    for (pane_id, spec) in pane_ids.iter().zip(specs) {
        let mut record = PaneRecord::new(*pane_id, created_at);
        record.cwd = spec.cwd;
        record.command = spec.command;
        let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
        let _ = session.panes.insert(record);
    }

    let root_pane = pane_ids[0];
    let new_tab = Tab::new(tab_id, name, session.tabs.len(), root_pane);
    // The first tab transitions the session from Starting to Running.
    if session.tabs.is_empty() {
        let _ = session.update_lifecycle(SessionLifecycleEvent::FirstTabCreated);
    }
    session.tabs.insert(tab_id, new_tab);
    // Swap the single-root layout for the profile's full tree.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_layout(layout);
    }

    events.push(Event::TabCreated(TabCreated { tab_id }));
    for pane_id in &pane_ids {
        events.push(Event::PaneCreated(PaneCreated {
            pane_id: *pane_id,
            tab_id,
        }));
    }

    if let Some(client_id) = focus {
        let focus_pane = pane_ids.get(focus_leaf).copied().unwrap_or(root_pane);
        // Record this tab's starting pane on the client whether or not the tab
        // starts active, so keyboard input and focused-pane commands resolve the
        // moment the client later switches to it.
        if let Some(tab) = session.tabs.get_mut(&tab_id) {
            tab.record_focus_mru(focus_pane);
        }
        if let Some(client) = session.clients.get_mut(client_id) {
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
/// Emits [`Event::PaneClosing`] + [`Event::PaneRemoved`] for every pane the tab
/// holds — the runtime kills the real processes off these events; this layer
/// only drops the records — then hands off to `close_and_refocus_tab` to
/// remove the tab, move any client viewing it to the nearest surviving tab,
/// renumber the remaining tabs densely, and quit the session if no tabs remain.
/// An unknown `tab_id` is a no-op with no events.
#[must_use]
pub fn close_tab(session: &mut Session, tab_id: TabId) -> Vec<Event> {
    let mut events = vec![];

    let Some(tab) = session.tabs.get(&tab_id) else {
        return events;
    };

    let tab_own_panes = tab.layout().leaf_panes();

    for pane_id in tab_own_panes {
        let _ = session.panes.remove(pane_id);
        events.push(Event::PaneClosing(PaneClosing { pane_id }));
        events.push(Event::PaneRemoved(PaneRemoved { pane_id, tab_id }));
    }

    events.extend(close_and_refocus_tab(session, tab_id));

    events
}

/// Rename `tab_id`.
///
/// A no-op (no event) when the tab is unknown or the name is unchanged; tab
/// names need not be unique — nothing resolves a tab by name. Layout and focus
/// are untouched. Returns [`Event::TabRenamed`].
#[must_use]
pub fn rename_tab(session: &mut Session, tab_id: TabId, new_name: String) -> Vec<Event> {
    let Some(tab) = session.tabs.get_mut(&tab_id) else {
        return Vec::new();
    };
    if tab.name() == new_name {
        return Vec::new(); // unchanged, nothing to emit
    }
    tab.update_name(new_name.clone());
    vec![Event::TabRenamed(TabRenamed {
        tab_id,
        name: new_name,
    })]
}

/// Point the client `client_id` at the tab named by `target`, resolved
/// against the current display order.
///
/// [`TabTarget::Id`] focuses that tab if it exists; [`TabTarget::Index`] the tab
/// at that display position; [`TabTarget::Next`]/[`TabTarget::Prev`] step one
/// position, wrapping at the ends. An unresolvable target — unknown id,
/// out-of-range index, unattached client — and re-focusing the already-active
/// tab are no-ops. Only this client's active tab changes; per-tab focus is
/// preserved, so switching back restores the pane it was on. Returns
/// [`Event::TabFocused`].
#[must_use]
pub fn focus_tab(session: &mut Session, client_id: ClientId, target: TabTarget) -> Vec<Event> {
    let Some(client) = session.clients.get(client_id) else {
        return Vec::new();
    };
    let prior_tab = client.active_tab();

    let Some(target_id) = resolve_tab_target(session, prior_tab, target) else {
        return Vec::new();
    };

    // Already viewing it — nothing to do.
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
/// order. A missing `Id` and an out-of-range `Index` resolve to `None` (the
/// caller treats that as a no-op); `Next`/`Prev` wrap around the ends,
/// stepping from `active_tab`.
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

    // Clamp to a valid slot; len >= 1 since the target exists (no underflow).
    let new_index = new_index.min(session.tabs.len() - 1);

    // No action needed if the new and old indices are identical.
    if new_index == old_index {
        return Vec::new();
    }

    // 1. Others in display order, excluding the target. (usize, TabId) are Copy,
    //    so this owns its data — the borrow of session.tabs ends at collect().
    let mut others: Vec<(usize, TabId)> = session
        .tabs
        .values()
        .filter(|tab| tab.id() != tab_id)
        .map(|tab| (tab.index(), tab.id()))
        .collect();
    others.sort_by_key(|&(index, _)| index);

    // 2. Renumber others densely 0..len-2 (closes the gap the target leaves).
    for (position, &(_, id)) in others.iter().enumerate() {
        if let Some(tab) = session.tabs.get_mut(&id) {
            tab.update_index(position);
        }
    }

    // 3. Drop the target into its new slot.
    if let Some(tab) = session.tabs.get_mut(&tab_id) {
        tab.update_index(new_index);
    }

    // 4. Shift everyone at/after the new slot up by one to make room.
    for &(_, id) in &others {
        if let Some(tab) = session.tabs.get_mut(&id) {
            if tab.index() >= new_index {
                tab.update_index(tab.index() + 1);
            }
        }
    }

    vec![Event::TabMoved(TabMoved {
        tab_id,
        old_index,
        new_index,
    })]
}

/// Remove an already-emptied `tab_id` and settle the fallout.
///
/// Emits [`Event::TabClosed`], moves every client off the tab — dropping its
/// stale per-tab focus, and sending any client that was viewing it to the
/// nearest surviving tab with [`Event::TabFocused`] — renumbers the survivors
/// densely, and emits [`Event::Quit`] when no tabs remain. Shared by
/// [`close_tab`] and the close/quit cascade's empty-tab path, so an explicit
/// close and a shell-exit that empties a tab end the same way. The caller
/// removes the tab's panes first (if any); this handles the tab and above.
#[must_use]
pub(crate) fn close_and_refocus_tab(session: &mut Session, tab_id: TabId) -> Vec<Event> {
    let mut events = vec![];

    let closed_index = session.tabs.get(&tab_id).map(|tab| tab.index());
    session.tabs.remove(&tab_id);
    events.push(Event::TabClosed(TabClosed { tab_id }));

    // Move every client off the closed tab: drop its focus entry for
    // the gone tab, and if it was viewing that tab, send it to the
    // nearest surviving tab.
    let next_tab = closed_index.and_then(|index| nearest_surviving_tab(session, index));
    let client_ids: Vec<ClientId> = session
        .clients
        .list_attached()
        .map(|client| client.id())
        .collect();
    for client_id in client_ids {
        if let Some(client) = session.clients.get_mut(client_id) {
            client.remove_focused_pane(tab_id);
            if client.active_tab() == tab_id {
                if let Some(next) = next_tab {
                    client.update_active_tab(next);
                    events.push(Event::TabFocused(TabFocused {
                        client_id,
                        tab_id: next,
                        prior_tab: tab_id,
                    }));
                }
            }
        }
    }

    reindex_tab_index(session);

    if session.tabs.is_empty() {
        // Idempotent if the session is already winding down; the quit signal
        // stands regardless.
        let _ = session.update_lifecycle(SessionLifecycleEvent::StopRequested);
        events.push(Event::Quit);
    }

    events
}

/// Renumber every tab to a dense `0..len` index in current display order,
/// closing any gap a removal left. Reordering only — emits no events.
fn reindex_tab_index(session: &mut Session) {
    let mut existing_tabs: Vec<(usize, TabId)> = session
        .tabs
        .values()
        .map(|tab| (tab.index(), tab.id()))
        .collect();
    existing_tabs.sort_by_key(|&(index, _)| index);

    for (position, &(_, id)) in existing_tabs.iter().enumerate() {
        if let Some(tab) = session.tabs.get_mut(&id) {
            tab.update_index(position);
        }
    }
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
