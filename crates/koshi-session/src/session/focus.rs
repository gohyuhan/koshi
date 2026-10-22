//! Focus recovery and focus events.
//!
//! When a client's focused pane disappears — closed, its shell exited, or it was
//! suppressed out of view — focus has to land somewhere deterministic.
//! [`repair_focus`] is the pure decision that picks it: given the tab and the
//! layout's ranked survivors, it walks a fixed recovery order and returns the
//! pane to focus, or a defined fallback when nothing is focusable.
//!
//! It chooses, it does not mutate; the caller applies the verdict. The pane
//! that left must have been the client's focus: the callers run it only for
//! the clients whose focused pane left the tab or is suppressed.
//!
//! [`build_pane_focused_event`] reports a change of the pane that takes a
//! client's input, floating or tiled, and
//! [`build_pane_minimized_changed_events`] a change of the floating panes one
//! client shows. [`apply_tiled_pane_focus`] focuses a tiled pane for one
//! client and reports both.

use koshi_core::{
    event::{Event, PaneFocused, PaneMinimizedChanged},
    ids::{ClientId, PaneId, TabId},
};
use koshi_layout::focus::FocusCandidates;
use koshi_pane::registry::PaneRegistry;

use crate::{client::Client, session::state::Tab};

/// The outcome of focus recovery: where focus should go now, or why it cannot
/// go to a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusRepairResult {
    /// Focus this pane — the first eligible one found walking the recovery
    /// order (focus history, then spatial neighbor, absorbed space, and finally
    /// the first eligible pane in layout order).
    Focused(PaneId),
    /// The tab's layout still holds panes, but none is eligible: every pane is
    /// suppressed (zero-area, too little room to draw), or every visible pane
    /// is missing from the registry. The caller focuses the tab's landing pane
    /// and reports the terminal-too-small overlay.
    TerminalTooSmall,
}

/// Pick the pane that inherits focus after the focused pane in `tab` left the
/// tab or was suppressed.
///
/// The recovery order is fixed, and the first eligible pane wins:
/// 1. the tab's focus history, newest first ([`Tab::list_focus_mru`]);
/// 2. the spatial neighbor of the rect the pane that left held;
/// 3. the pane that absorbed the most of that rect;
/// 4. the first eligible pane in layout order, as a last resort.
///
/// `ranked_focus_candidates` is the layout's ranked survivors after the removal
/// (from `koshi_layout::focus::compute_focus_candidates`); its
/// `layout_order_pane_ids` is exactly the
/// visible panes, so suppressed panes are already excluded. A pane is
/// *eligible* when it appears in `layout_order_pane_ids` and has a record in
/// `pane_registry`.
///
/// When no pane is eligible, the verdict is
/// [`FocusRepairResult::TerminalTooSmall`].
#[must_use]
pub fn repair_focus(
    tab: &Tab,
    pane_registry: &PaneRegistry,
    ranked_focus_candidates: FocusCandidates,
) -> FocusRepairResult {
    let is_eligible_pane = |pane_id: PaneId| {
        ranked_focus_candidates
            .layout_order_pane_ids
            .contains(&pane_id)
            && pane_registry.get_pane_record_by_id(pane_id).is_some()
    };

    // The recovery order in one pass, focus history newest-first.
    let focus_pane_id = tab
        .list_focus_mru()
        .iter()
        .copied()
        .chain(ranked_focus_candidates.spatial_neighbor_pane_id)
        .chain(ranked_focus_candidates.absorbed_space_pane_id)
        .chain(
            ranked_focus_candidates
                .layout_order_pane_ids
                .iter()
                .copied(),
        )
        .find(|&pane_id| is_eligible_pane(pane_id));

    match focus_pane_id {
        Some(pane_id) => FocusRepairResult::Focused(pane_id),
        None => FocusRepairResult::TerminalTooSmall,
    }
}

/// The [`Event::PaneFocused`] that reports the pane taking `client`'s input
/// now ([`Client::get_active_focused_pane_id`]), when it is not
/// `previous_focused_pane_id`, the pane that took the input before.
///
/// Returns `None` when no pane takes `client`'s input now, or when that pane
/// is `previous_focused_pane_id`. The event's `tab_id` is `None` when a
/// floating pane takes the input, else `client`'s active tab, and its
/// `previous_pane_id` is `previous_focused_pane_id`. Float `htop` focused
/// after `vim` in tab `db` → `PaneFocused { tab_id: None, pane_id: htop,
/// previous_pane_id: Some(vim) }`.
#[must_use]
pub fn build_pane_focused_event(
    client: &Client,
    previous_focused_pane_id: Option<PaneId>,
) -> Option<Event> {
    let focused_pane_id = client.get_active_focused_pane_id()?;
    if Some(focused_pane_id) == previous_focused_pane_id {
        return None;
    }
    let tab_id = match client.get_focused_floating_pane_id() {
        Some(_) => None,
        None => Some(client.get_active_tab_id()),
    };
    Some(Event::PaneFocused(PaneFocused {
        client_id: client.get_client_id(),
        tab_id,
        pane_id: focused_pane_id,
        previous_pane_id: previous_focused_pane_id,
    }))
}

/// Focus `pane_id` in `tab_id` for `client` ([`Client::focus_tiled_pane`]):
/// in `client`'s active tab, `pane_id` takes its input and every floating
/// pane it shows is minimized.
///
/// Returns one [`Event::PaneMinimizedChanged`] with `is_minimized: true` per
/// floating pane minimized, oldest shown first, then an
/// [`Event::PaneFocused`] with `tab_id: Some(tab_id)` naming the pane that
/// held the focus before as `previous_pane_id`. With `htop` shown and
/// focused, focusing tiled `a` in the active tab → `PaneMinimizedChanged
/// { pane_id: htop, is_minimized: true }`, then `PaneFocused { pane_id: a,
/// previous_pane_id: Some(htop) }`. No `PaneFocused` when `pane_id` held the
/// focus already.
#[must_use]
pub fn apply_tiled_pane_focus(client: &mut Client, tab_id: TabId, pane_id: PaneId) -> Vec<Event> {
    let client_id = client.get_client_id();
    let tiled_pane_focus_change = client.focus_tiled_pane(tab_id, pane_id);
    let mut emitted_events: Vec<Event> = build_pane_minimized_changed_events(
        client_id,
        &tiled_pane_focus_change.minimized_floating_pane_ids,
        true,
    )
    .collect();
    if tiled_pane_focus_change.previous_pane_id != Some(pane_id) {
        emitted_events.push(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(tab_id),
            pane_id,
            previous_pane_id: tiled_pane_focus_change.previous_pane_id,
        }));
    }
    emitted_events
}

/// One [`Event::PaneMinimizedChanged`] per pane of `pane_ids`, in that order,
/// each naming `client_id` and `is_minimized`.
pub fn build_pane_minimized_changed_events(
    client_id: ClientId,
    pane_ids: &[PaneId],
    is_minimized: bool,
) -> impl Iterator<Item = Event> + '_ {
    pane_ids.iter().map(move |&pane_id| {
        Event::PaneMinimizedChanged(PaneMinimizedChanged {
            client_id,
            pane_id,
            is_minimized,
        })
    })
}

#[cfg(test)]
mod tests;
