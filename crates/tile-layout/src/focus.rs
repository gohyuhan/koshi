//! Focus candidates after a pane disappears.
//!
//! Picking the next focused pane is session policy, not layout's job — the
//! session owns focus history and per-client state. What layout can answer
//! is the geometric part: given where the removed pane was and where the
//! survivors now sit, which panes are sensible focus targets? This module
//! returns those candidates, ranked three ways, and chooses nothing.
//!
//! Two kinds of panes are never candidates. Zero-area panes — suppressed
//! or hidden by a fullscreen overlay — have no visible cells to focus.
//! Collapsed stack members do have a visible rect (their one-row header
//! strip), but that strip is Tile-owned chrome: such a member must not
//! silently receive focus either. Reaching one of those goes through an
//! explicit activation, not through focus repair.

use tile_core::geometry::{Rect, SplitDirection};
use tile_core::ids::PaneId;

use crate::solver::StackHeader;
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
    /// Every visible pane, in layout order — the last-resort fallback.
    pub layout_order: Vec<PaneId>,
}

/// Rank the surviving panes as focus targets for a pane that occupied
/// `removed_rect`.
///
/// `surviving_panes` is the solved placement of the layout after the
/// removal, in layout order, and `stack_headers` the collapsed members of
/// that same solve (exactly what the solver returns). Panes listed in
/// `stack_headers` are excluded: their non-empty rect is the header strip,
/// not focusable content.
#[must_use]
pub fn focus_candidates(
    removed_rect: Rect,
    surviving_panes: &[(PaneId, Rect)],
    stack_headers: &[StackHeader],
) -> FocusCandidates {
    let visible: Vec<(PaneId, Rect)> = surviving_panes
        .iter()
        .copied()
        .filter(|&(id, rect)| {
            !rect.is_empty() && !stack_headers.iter().any(|header| header.pane == id)
        })
        .collect();

    let spatial_neighbor = visible
        .iter()
        .min_by_key(|&&(_, rect)| center_distance(removed_rect, rect))
        .map(|&(pane, _)| pane);

    // Largest absorbed area wins; on a tie the earlier pane in layout order
    // keeps it, because only strictly larger areas displace the holder.
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

/// Squared distance between two rect centers, on doubled coordinates so
/// half-cell centers stay exact integers.
fn center_distance(a: Rect, b: Rect) -> u64 {
    let (ax, ay) = doubled_center(a);
    let (bx, by) = doubled_center(b);
    let dx = i64::from(ax) - i64::from(bx);
    let dy = i64::from(ay) - i64::from(by);
    (dx * dx + dy * dy) as u64
}

fn doubled_center(rect: Rect) -> (u32, u32) {
    (
        2 * u32::from(rect.origin.x) + u32::from(rect.size.cols),
        2 * u32::from(rect.origin.y) + u32::from(rect.size.rows),
    )
}

fn cell_area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
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

/// Expand the next stack member, wrapping at the end.
///
/// Returns `None` when nothing can change: the node is not a stack, or no
/// other member can take focus. The stack is unchanged in that case.
pub fn stack_focus_next(stack: &mut SplitNode) -> Option<StackFocusChange> {
    stack_focus_step(stack, 1)
}

/// Expand the previous stack member, wrapping at the start.
///
/// Returns `None` when nothing can change, leaving the stack unchanged.
pub fn stack_focus_prev(stack: &mut SplitNode) -> Option<StackFocusChange> {
    stack_focus_step(stack, -1)
}

/// Expand the stack member holding `pane` (a collapsed member is a valid
/// target — that is exactly what header clicks and remote focus do).
///
/// Returns `None` when `pane` is not in this stack or is already the
/// active member; the stack is unchanged in that case.
pub fn stack_activate(stack: &mut SplitNode, pane: PaneId) -> Option<StackFocusChange> {
    if stack.direction != SplitDirection::Stacked {
        return None;
    }
    let target = stack
        .children
        .iter()
        .position(|child| child.node.contains_pane(pane))?;
    if target == current_active(stack) {
        return None;
    }
    Some(set_active(stack, target))
}

/// The pane focus lands on when a client enters this stack from outside:
/// its active member.
#[must_use]
pub fn stack_entry_target(stack: &SplitNode) -> Option<PaneId> {
    let child = stack.children.get(current_active(stack))?;
    child.node.leaf_panes().first().copied()
}

/// Walk `step` through the members (wrapping) to the first one that holds a
/// pane, and expand it.
fn stack_focus_step(stack: &mut SplitNode, step: i64) -> Option<StackFocusChange> {
    if stack.direction != SplitDirection::Stacked || stack.children.len() < 2 {
        return None;
    }
    let count = stack.children.len() as i64;
    let active = current_active(stack) as i64;
    for offset in 1..count {
        let candidate = (active + step * offset).rem_euclid(count) as usize;
        if !stack.children[candidate].node.leaf_panes().is_empty() {
            return Some(set_active(stack, candidate));
        }
    }
    None
}

/// The in-bounds active index (constructors clamp it, but a deserialized
/// stack might not have).
fn current_active(stack: &SplitNode) -> usize {
    stack.active.min(stack.children.len().saturating_sub(1))
}

/// Point the stack at `target`: expand it, collapse everyone else.
fn set_active(stack: &mut SplitNode, target: usize) -> StackFocusChange {
    let deactivated = stack
        .children
        .get(current_active(stack))
        .and_then(|child| child.node.leaf_panes().first().copied());
    stack.active = target;
    for (index, child) in stack.children.iter_mut().enumerate() {
        child.collapsed = index != target;
    }
    StackFocusChange {
        newly_active: stack.children[target]
            .node
            .leaf_panes()
            .first()
            .copied()
            .expect("callers only activate members that hold a pane"),
        deactivated,
    }
}

#[cfg(test)]
mod tests;
