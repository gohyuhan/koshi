//! Focus candidates after a pane disappears.
//!
//! This module answers the geometric part only: given where the removed pane
//! was and where the survivors now sit, it returns the candidate panes ranked
//! three ways, and chooses nothing. The session owns focus history and
//! per-client state.
//!
//! Two kinds of panes are never candidates: zero-area panes (suppressed, or
//! hidden under a fullscreen pane), and collapsed stack members, whose only
//! visible rect is their one-row header strip. A collapsed member expands
//! through [`stack_activate`], not through focus repair.

use koshi_core::geometry::{Rect, SplitDirection};
use koshi_core::ids::PaneId;

use crate::solver::{cell_area, shows_content, StackHeader};
use crate::tree::SplitNode;

/// Focus targets after a removal, for the caller to rank against its own
/// focus history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusCandidates {
    /// The visible pane whose center is closest to the removed pane's
    /// center. Ties go to the earlier pane in layout order.
    pub spatial_neighbor: Option<PaneId>,
    /// The visible pane that took over the largest share of the removed
    /// pane's cells. `None` when nothing overlaps the old rect.
    pub absorbed_space: Option<PaneId>,
    /// Every visible pane, in layout order; the caller's last-resort
    /// fallback.
    pub layout_order: Vec<PaneId>,
}

/// Rank the surviving panes as focus targets for a pane that occupied
/// `removed_rect`.
///
/// `surviving_panes` is the solved placement of the layout after the
/// removal, in layout order, and `stack_headers` the collapsed members of
/// that same solve (exactly what the solver returns). A pane with a
/// zero-area rect, or one listed in `stack_headers`, is not visible and is
/// excluded from every ranking.
#[must_use]
pub fn focus_candidates(
    removed_rect: Rect,
    surviving_panes: &[(PaneId, Rect)],
    stack_headers: &[StackHeader],
) -> FocusCandidates {
    let visible: Vec<(PaneId, Rect)> = surviving_panes
        .iter()
        .copied()
        .filter(|&(id, rect)| shows_content(id, rect, stack_headers))
        .collect();

    let spatial_neighbor = visible
        .iter()
        .min_by_key(|&&(_, rect)| center_distance(removed_rect, rect))
        .map(|&(pane, _)| pane);

    // Largest absorbed area wins; on a tie the earlier pane in layout order
    // keeps it.
    let mut absorbed: Option<(PaneId, u64)> = None;
    for &(pane, rect) in &visible {
        let Some(overlap) = rect.intersection(removed_rect) else {
            continue;
        };
        let area = cell_area(overlap);
        if absorbed.is_none_or(|(_, best)| area > best) {
            absorbed = Some((pane, area));
        }
    }
    let absorbed_space = absorbed.map(|(pane, _)| pane);

    let layout_order = visible.into_iter().map(|(pane, _)| pane).collect();

    FocusCandidates {
        spatial_neighbor,
        absorbed_space,
        layout_order,
    }
}

/// Squared distance between two rect centers, in the doubled coordinates
/// [`doubled_center`] returns.
fn center_distance(a: Rect, b: Rect) -> u64 {
    let (ax, ay) = doubled_center(a);
    let (bx, by) = doubled_center(b);
    let dx = i64::from(ax) - i64::from(bx);
    let dy = i64::from(ay) - i64::from(by);
    (dx * dx + dy * dy) as u64
}

/// The center of `rect` with both components doubled: `2·origin + size` on
/// each axis. A rect at x 0 spanning 5 columns yields x 5, an odd half-cell
/// center held as an exact integer.
fn doubled_center(rect: Rect) -> (u32, u32) {
    (
        2 * u32::from(rect.origin.x) + u32::from(rect.size.cols),
        2 * u32::from(rect.origin.y) + u32::from(rect.size.rows),
    )
}

/// A completed stack-local focus move: which member expanded and which
/// collapsed. The caller forwards these to its focus and render state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackFocusChange {
    /// The member that just expanded.
    pub newly_active: PaneId,
    /// The member that collapsed to a header, when the previously active
    /// slot held one.
    pub deactivated: Option<PaneId>,
}

/// Expand the stack member holding `pane`. A collapsed member is a valid
/// target. The member may be a subtree; [`StackFocusChange::newly_active`]
/// is then its first leaf, which can differ from `pane`.
///
/// Returns `None` when `stack` is not a stack, when `pane` is not in it, or
/// when the member holding `pane` is already the active member; the stack is
/// unchanged in that case.
pub fn stack_activate(stack: &mut SplitNode, pane: PaneId) -> Option<StackFocusChange> {
    if stack.direction != SplitDirection::Stacked {
        return None;
    }
    let target = stack
        .children
        .iter()
        .position(|child| child.contains_pane(pane))?;
    if target == stack.active_index() {
        return None;
    }
    Some(set_active(stack, target))
}

/// Set the stack's active member to `target`, which collapses every other
/// member. `deactivated` is the first leaf of the member that was in the
/// active slot. Panics when the member at `target` holds no pane.
fn set_active(stack: &mut SplitNode, target: usize) -> StackFocusChange {
    let deactivated = stack
        .children
        .get(stack.active_index())
        .and_then(|child| child.first_leaf());
    stack.active = target;
    StackFocusChange {
        newly_active: stack.children[target]
            .first_leaf()
            .expect("callers only activate members that hold a pane"),
        deactivated,
    }
}

#[cfg(test)]
mod tests;
