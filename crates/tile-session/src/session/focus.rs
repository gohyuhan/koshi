//! Focus recovery: choosing the next focused pane after the focused one is gone.
//!
//! When a client's focused pane disappears — closed, its shell exited, or it was
//! suppressed out of view — focus has to land somewhere deterministic.
//! [`repair_focus`] is the pure decision that picks it: given the tab and the
//! layout's ranked survivors, it walks a fixed recovery order and returns the
//! pane to focus, or a defined fallback when nothing is focusable.
//!
//! It chooses, it does not mutate. The caller applies the verdict, and the
//! caller is also the gate: `repair_focus` assumes the removed pane *was* the
//! client's focus. Removing a pane the client was not looking at leaves focus
//! untouched and never reaches here — that check belongs to the removal
//! pipeline, which knows each client's focus and runs recovery only for the
//! clients whose focused pane actually vanished.

use tile_core::ids::PaneId;
use tile_layout::focus::FocusCandidates;
use tile_pane::{pane::lifecycle::PaneLifecycle, registry::PaneRegistry};

use crate::session::{policy::EmptyTabPolicy, state::Tab};

/// The outcome of focus recovery: where focus should go now, or why it cannot
/// go to a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusRepairResult {
    /// Focus this pane — the first eligible one found walking the recovery
    /// order (focus history, then spatial neighbor, absorbed space, and finally
    /// the first eligible pane in layout order).
    Focused(PaneId),
    /// The tab still holds panes but every one is suppressed (zero-area, too
    /// little room to draw). No pane can take focus: the caller shows the
    /// terminal-too-small overlay and blocks pane input until the window grows
    /// back enough to un-suppress a pane.
    TerminalTooSmall,
    /// The tab has no panes left at all. There is nothing to focus, so the
    /// caller carries out the tab's empty-tab policy — close the tab or respawn
    /// a shell.
    EmptyTab(EmptyTabPolicy),
}

/// Pick the pane that inherits focus after the focused pane in `tab` is gone.
///
/// The recovery order is fixed, and the first eligible pane wins:
/// 1. the tab's focus history, newest first ([`Tab::focus_mru`]);
/// 2. the spatial neighbor of the removed pane's old rect;
/// 3. the pane that absorbed the most of the removed pane's space;
/// 4. the first eligible pane in layout order, as a last resort.
///
/// `candidate` is the layout's ranked survivors after the removal (from
/// `tile_layout::focus::focus_candidates`); its `layout_order` is exactly the
/// visible panes, so suppressed panes are already excluded and a pane is
/// *eligible* when it appears there and is not [`PaneLifecycle::Removed`]. A
/// dead (`Exited`) pane stays eligible on purpose: it is a visible, focusable
/// placeholder, so focus may rest on it until it is actually removed.
///
/// When no pane is eligible, the result distinguishes the two empty cases by
/// the tab's layout: panes still present but all suppressed yield
/// [`FocusRepairResult::TerminalTooSmall`]; a tab with no panes left yields
/// [`FocusRepairResult::EmptyTab`] carrying `empty_tab_policy` for the caller to
/// apply.
#[must_use]
pub fn repair_focus(
    tab: &Tab,
    pane_registry: &PaneRegistry,
    candidate: FocusCandidates,
    empty_tab_policy: EmptyTabPolicy,
) -> FocusRepairResult {
    let is_eligible = |pane_id: PaneId| {
        candidate.layout_order.contains(&pane_id)
            && pane_registry
                .get(pane_id)
                .is_some_and(|pane| *pane.lifecycle() != PaneLifecycle::Removed)
    };

    for &pane_id in tab.focus_mru() {
        if is_eligible(pane_id) {
            return FocusRepairResult::Focused(pane_id);
        }
    }

    if let Some(pane_id) = candidate.spatial_neighbor {
        if is_eligible(pane_id) {
            return FocusRepairResult::Focused(pane_id);
        }
    }

    if let Some(pane_id) = candidate.absorbed_space {
        if is_eligible(pane_id) {
            return FocusRepairResult::Focused(pane_id);
        }
    }

    if let Some(&pane_id) = candidate
        .layout_order
        .iter()
        .find(|&&pane_id| is_eligible(pane_id))
    {
        return FocusRepairResult::Focused(pane_id);
    }

    if tab.layout().leaf_panes().is_empty() {
        FocusRepairResult::EmptyTab(empty_tab_policy)
    } else {
        FocusRepairResult::TerminalTooSmall
    }
}

#[cfg(test)]
mod tests;
