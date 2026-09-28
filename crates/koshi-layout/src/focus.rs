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
/// `removed_pane_rect`.
///
/// `surviving_pane_rects` is the solved placement of the layout after the
/// removal, in layout order, and `stack_headers` the collapsed members of
/// that same solve (exactly what the solver returns). A pane with a
/// zero-area rect, or one listed in `stack_headers`, is not visible and is
/// excluded from every ranking.
#[must_use]
pub fn compute_focus_candidates(
    removed_pane_rect: Rect,
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
        .min_by_key(|&&(_, pane_rect)| compute_center_distance(removed_pane_rect, pane_rect))
        .map(|&(pane_id, _)| pane_id);

    // Largest absorbed area wins; on a tie the earlier pane in layout order
    // keeps it.
    let mut largest_absorbed_pane: Option<(PaneId, u64)> = None;
    for &(pane_id, pane_rect) in &visible_pane_rects {
        let Some(overlap_rect) = pane_rect.compute_intersection(removed_pane_rect) else {
            continue;
        };
        let absorbed_cell_area = compute_cell_area(overlap_rect);
        if largest_absorbed_pane.is_none_or(|(_, largest_absorbed_cell_area)| {
            absorbed_cell_area > largest_absorbed_cell_area
        }) {
            largest_absorbed_pane = Some((pane_id, absorbed_cell_area));
        }
    }
    let absorbed_space_pane_id = largest_absorbed_pane.map(|(pane_id, _)| pane_id);

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
fn compute_center_distance(removed_pane_rect: Rect, candidate_pane_rect: Rect) -> u64 {
    let (removed_doubled_column, removed_doubled_row) = compute_doubled_center(removed_pane_rect);
    let (candidate_doubled_column, candidate_doubled_row) =
        compute_doubled_center(candidate_pane_rect);
    let column_distance = i64::from(removed_doubled_column) - i64::from(candidate_doubled_column);
    let row_distance = i64::from(removed_doubled_row) - i64::from(candidate_doubled_row);
    (column_distance * column_distance + row_distance * row_distance) as u64
}

/// The center of `pane_rect` with both components doubled: `2·origin + size`
/// on each axis. A rect at column 0 spanning 5 columns yields column 5, an
/// odd half-cell center held as an exact integer.
fn compute_doubled_center(pane_rect: Rect) -> (u32, u32) {
    (
        2 * u32::from(pane_rect.origin.column) + u32::from(pane_rect.size.column_count),
        2 * u32::from(pane_rect.origin.row) + u32::from(pane_rect.size.row_count),
    )
}

/// Expand the stack member holding `pane_id`, which collapses every other
/// member. A collapsed member is a valid target, and the member may be a
/// subtree.
///
/// Returns `true` when the active member changed. Returns `false`, with
/// `stack_node` unchanged, when `stack_node` is not a stack, when `pane_id` is not in
/// it, or when the member holding `pane_id` is already the active member.
pub fn activate_stack_member(stack_node: &mut SplitNode, pane_id: PaneId) -> bool {
    if stack_node.direction != SplitDirection::Stacked {
        return false;
    }
    let Some(target_stack_member_index) = stack_node
        .children
        .iter()
        .position(|child| child.has_pane(pane_id))
    else {
        return false;
    };
    if target_stack_member_index == stack_node.get_active_child_index() {
        return false;
    }
    stack_node.active_child_index = target_stack_member_index;
    true
}

#[cfg(test)]
mod tests;
