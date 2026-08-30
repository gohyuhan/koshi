//! Focus recovery: choosing the next focused pane after the focused one is gone.
//!
//! When a client's focused pane disappears — closed, its shell exited, or it was
//! suppressed out of view — focus has to land somewhere deterministic.
//! [`repair_focus`] is the pure decision that picks it: given the tab and the
//! layout's ranked survivors, it walks a fixed recovery order and returns the
//! pane to focus, or a defined fallback when nothing is focusable.
//!
//! It chooses, it does not mutate; the caller applies the verdict. The removed
//! pane must have been the client's focus: the removal pipeline runs it only
//! for the clients whose focused pane vanished.

use koshi_core::ids::PaneId;
use koshi_layout::focus::FocusCandidates;
use koshi_pane::{pane::lifecycle::PaneLifecycle, registry::PaneRegistry};

use crate::session::{policy::EmptyTabPolicy, state::Tab};

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
    /// is [`PaneLifecycle::Removed`] or missing from the registry. The caller
    /// shows the terminal-too-small overlay.
    TerminalTooSmall,
    /// The tab's layout holds no panes at all. Carries `empty_tab_policy` for
    /// the caller to apply — close the tab.
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
/// `koshi_layout::focus::focus_candidates`); its `layout_order` is exactly the
/// visible panes, so suppressed panes are already excluded. A pane is
/// *eligible* when it appears in `layout_order`, has a record in
/// `pane_registry`, and that record is not [`PaneLifecycle::Removed`]. A
/// `Spawning`, `Running`, dead (`Exited`) or `Closing` pane all stay eligible:
/// each is a visible, focusable placeholder until it is removed.
///
/// When no pane is eligible, the tab's layout picks the verdict: a layout that
/// still holds panes yields [`FocusRepairResult::TerminalTooSmall`]; a layout
/// with no panes left yields [`FocusRepairResult::EmptyTab`] carrying
/// `empty_tab_policy`.
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

    // The recovery order in one pass, focus history newest-first.
    let inheritor = tab
        .focus_mru()
        .iter()
        .copied()
        .chain(candidate.spatial_neighbor)
        .chain(candidate.absorbed_space)
        .chain(candidate.layout_order.iter().copied())
        .find(|&pane_id| is_eligible(pane_id));

    match inheritor {
        Some(pane_id) => FocusRepairResult::Focused(pane_id),
        None if tab.layout().leaf_panes().is_empty() => {
            FocusRepairResult::EmptyTab(empty_tab_policy)
        }
        None => FocusRepairResult::TerminalTooSmall,
    }
}

#[cfg(test)]
mod tests;
