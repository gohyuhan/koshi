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
//! through [`activate_stack_member`], not through focus repair.

use koshi_core::geometry::{Rect, SplitDirection};
use koshi_core::ids::PaneId;

use crate::solver::{compute_cell_area, is_content_visible, StackHeader};
use crate::tree::SplitNode;

/// Focus targets after a removal, for the caller to rank against its own
/// focus history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusCandidates {
    /// The visible pane whose center is closest to the removed pane's
    /// center. Ties go to the earlier pane in layout order.
    pub spatial_neighbor_pane_id: Option<PaneId>,
    /// The visible pane that took over the largest share of the removed
    /// pane's cells. `None` when nothing overlaps the old rect.
    pub absorbed_space_pane_id: Option<PaneId>,
    /// Every visible pane, in layout order; the caller's last-resort
    /// fallback.
    pub layout_order_pane_ids: Vec<PaneId>,
}

/// Rank the surviving panes as focus targets for a pane that occupied
/// `removed_rect`.
///
/// `surviving_pane_rects` is the solved placement of the layout after the
/// removal, in layout order, and `stack_headers` the collapsed members of
/// that same solve (exactly what the solver returns). A pane with a
/// zero-area rect, or one listed in `stack_headers`, is not visible and is
/// excluded from every ranking.
#[must_use]
pub fn compute_focus_candidates(
    removed_rect: Rect,
    surviving_pane_rects: &[(PaneId, Rect)],
    stack_headers: &[StackHeader],
) -> FocusCandidates {
    let visible_pane_rects: Vec<(PaneId, Rect)> = surviving_pane_rects
        .iter()
        .copied()
        .filter(|&(pane_id, pane_rect)| is_content_visible(pane_id, pane_rect, stack_headers))
        .collect();

    let spatial_neighbor_pane_id = visible_pane_rects
        .iter()
        .min_by_key(|&&(_, pane_rect)| compute_center_distance(removed_rect, pane_rect))
        .map(|&(pane_id, _)| pane_id);

    // Largest absorbed area wins; on a tie the earlier pane in layout order
    // keeps it.
    let mut absorbed_pane_area: Option<(PaneId, u64)> = None;
    for &(pane_id, pane_rect) in &visible_pane_rects {
        let Some(overlap_rect) = pane_rect.compute_intersection(removed_rect) else {
            continue;
        };
        let overlap_area = compute_cell_area(overlap_rect);
        if absorbed_pane_area
            .is_none_or(|(_, largest_overlap_area)| overlap_area > largest_overlap_area)
        {
            absorbed_pane_area = Some((pane_id, overlap_area));
        }
    }
    let absorbed_space_pane_id = absorbed_pane_area.map(|(pane_id, _)| pane_id);

    let layout_order_pane_ids = visible_pane_rects
        .into_iter()
        .map(|(pane_id, _)| pane_id)
        .collect();

    FocusCandidates {
        spatial_neighbor_pane_id,
        absorbed_space_pane_id,
        layout_order_pane_ids,
    }
}

/// Squared distance between two rect centers, in the doubled coordinates
/// [`compute_doubled_center`] returns.
fn compute_center_distance(first_rect: Rect, second_rect: Rect) -> u64 {
    let (first_doubled_column, first_doubled_row) = compute_doubled_center(first_rect);
    let (second_doubled_column, second_doubled_row) = compute_doubled_center(second_rect);
    let column_distance = i64::from(first_doubled_column) - i64::from(second_doubled_column);
    let row_distance = i64::from(first_doubled_row) - i64::from(second_doubled_row);
    (column_distance * column_distance + row_distance * row_distance) as u64
}

/// The center of `rect` with both components doubled: `2·origin + cell_size`
/// on each axis. A rect at column 0 spanning 5 columns yields column 5, an
/// odd half-cell center held as an exact integer.
fn compute_doubled_center(rect: Rect) -> (u32, u32) {
    (
        2 * u32::from(rect.origin.column) + u32::from(rect.cell_size.column_count),
        2 * u32::from(rect.origin.row) + u32::from(rect.cell_size.row_count),
    )
}

/// A completed stack-local focus move: which member expanded and which
/// collapsed. The caller forwards these to its focus and render state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackFocusChange {
    /// The member that just expanded.
    pub newly_active_pane_id: PaneId,
    /// The member that collapsed to a header, when the previously active
    /// slot held one.
    pub deactivated_pane_id: Option<PaneId>,
}

/// Expand the stack member holding `pane_id`. A collapsed member is a valid
/// target. The member may be a subtree; [`StackFocusChange::newly_active_pane_id`]
/// is then its first leaf, which can differ from `pane_id`.
///
/// Returns `None` when `stack` is not a stack, when `pane_id` is not in it, or
/// when the member holding `pane_id` is already the active member; the stack is
/// unchanged in that case.
pub fn activate_stack_member(stack: &mut SplitNode, pane_id: PaneId) -> Option<StackFocusChange> {
    if stack.direction != SplitDirection::Stacked {
        return None;
    }
    let target_child_index = stack
        .children
        .iter()
        .position(|child| child.contains_pane(pane_id))?;
    if target_child_index == stack.get_active_child_index() {
        return None;
    }
    Some(set_active_child(stack, target_child_index))
}

/// Set the stack's active member to `target_child_index`, which collapses every
/// other member. `deactivated_pane_id` is the first leaf of the member that was
/// in the active slot. Panics when the member at `target_child_index` holds no
/// pane.
fn set_active_child(stack: &mut SplitNode, target_child_index: usize) -> StackFocusChange {
    let deactivated_pane_id = stack
        .children
        .get(stack.get_active_child_index())
        .and_then(|child| child.find_first_leaf_pane_id());
    stack.active_child_index = target_child_index;
    StackFocusChange {
        newly_active_pane_id: stack.children[target_child_index]
            .find_first_leaf_pane_id()
            .expect("callers only activate members that hold a pane"),
        deactivated_pane_id,
    }
}

#[cfg(test)]
mod tests;
