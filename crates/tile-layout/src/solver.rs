//! Geometry solver: a layout tree plus a tab rectangle in, exact pane
//! rectangles out.
//!
//! Solving is pure and deterministic: the same tree over the same rect always
//! yields the same placement, so nothing flickers across renders. Every leaf
//! appears in the result exactly once, in layout order, and the placed rects
//! tile the tab exactly — a split's children always account for every cell of
//! the split's own rectangle.
//!
//! ## Distribution order
//!
//! Along a split axis, children claim cells in constraint order: `Fixed`
//! sizes first, then `Percent` of the axis, then the remainder is shared by
//! the flexible children (`Flex`, and `Min`/`Preferred`, which flex around
//! their floor/target) in proportion to their weights. User resizes are
//! applied last as exact cell deltas.
//!
//! Cells that integer division leaves over go to the *trailing* children, one
//! each: a 101-column 50/50 split solves to 50 and 51. When no flexible child
//! exists to absorb slack, the last child takes it — no region of the tab may
//! go dead.

use tile_core::geometry::{Point, Rect, Size, SplitDirection};
use tile_core::ids::PaneId;

use crate::size::SizeConstraint;
use crate::size::SizeWeight;
use crate::tree::{LayoutNode, SplitNode};

/// The solved placement for one tree over one tab rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveResult {
    /// Every leaf pane exactly once, in layout order, with its solved
    /// rectangle. A zero-area rect means the pane is not visible.
    pub panes: Vec<(PaneId, Rect)>,
    /// Panes clipped to zero area because the layout no longer fits. Stable
    /// trailing order: the same panes suppress and restore as space changes.
    pub suppressed: Vec<PaneId>,
    /// `true` when every pane is suppressed; the caller shows a
    /// terminal-too-small overlay instead of a pane grid.
    pub all_suppressed: bool,
}

/// Solve `tree` over `tab_rect`.
#[must_use]
pub fn solve(tree: &LayoutNode, tab_rect: Rect) -> SolveResult {
    let mut panes = Vec::new();
    solve_node(tree, tab_rect, &mut panes);
    SolveResult {
        panes,
        suppressed: Vec::new(),
        all_suppressed: false,
    }
}

fn solve_node(node: &LayoutNode, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        LayoutNode::Pane(id) => out.push((*id, rect)),
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal => solve_directional(split, rect, out),
            SplitDirection::Vertical => solve_directional(split, rect, out),
            SplitDirection::Stacked => solve_stacked(split, rect, out),
        },
    }
}

/// Divide `rect` among the split's children along its axis and recurse.
fn solve_directional(split: &SplitNode, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
    let horizontal = split.direction == SplitDirection::Horizontal;
    let available = if horizontal {
        rect.size.cols
    } else {
        rect.size.rows
    };
    let sizes = distribute(&split.weights, available);

    let mut offset: u16 = 0;
    for (child, &cells) in split.children.iter().zip(&sizes) {
        let child_rect = if horizontal {
            Rect::new(
                Point {
                    x: rect.origin.x.saturating_add(offset),
                    y: rect.origin.y,
                },
                Size {
                    cols: cells,
                    rows: rect.size.rows,
                },
            )
        } else {
            Rect::new(
                Point {
                    x: rect.origin.x,
                    y: rect.origin.y.saturating_add(offset),
                },
                Size {
                    cols: rect.size.cols,
                    rows: cells,
                },
            )
        };
        solve_node(&child.node, child_rect, out);
        offset = offset.saturating_add(cells);
    }
}

/// Stacked children share the rect. The active child takes all of it for
/// now; collapsed children solve to zero area. (Header rows are layered in
/// with the rest of the stacked behavior.)
fn solve_stacked(split: &SplitNode, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
    for (index, child) in split.children.iter().enumerate() {
        let child_rect = if index == split.active {
            rect
        } else {
            Rect::zero()
        };
        solve_node(&child.node, child_rect, out);
    }
}

/// Split `available` cells among children according to their weights.
///
/// The returned sizes always sum to exactly `available`: a split never
/// leaves cells unassigned and never assigns more than it has.
fn distribute(weights: &[SizeWeight], available: u16) -> Vec<u16> {
    let mut sizes = vec![0u16; weights.len()];
    let mut remaining = available;

    // Fixed sizes claim cells first, in child order, never more than remain.
    for (index, weight) in weights.iter().enumerate() {
        if let SizeConstraint::Fixed(cells) = weight.primary {
            sizes[index] = cells.min(remaining);
            remaining -= sizes[index];
        }
    }

    // Percentages are shares of the whole axis, floored to cells.
    for (index, weight) in weights.iter().enumerate() {
        if let SizeConstraint::Percent(percent) = weight.primary {
            let want = (u32::from(available) * u32::from(percent) / 100) as u16;
            sizes[index] = want.min(remaining);
            remaining -= sizes[index];
        }
    }

    // Flexible children share the remainder by weight. `Min` and `Preferred`
    // flex with weight 1; their floor and target are overlays on a share.
    let flex: Vec<(usize, u64)> = weights
        .iter()
        .enumerate()
        .filter_map(|(index, weight)| match weight.primary {
            SizeConstraint::Flex(w) => Some((index, u64::from(w))),
            SizeConstraint::Min(_) | SizeConstraint::Preferred(_) => Some((index, 1)),
            SizeConstraint::Fixed(_) | SizeConstraint::Percent(_) => None,
        })
        .collect();
    let total_weight: u64 = flex.iter().map(|&(_, w)| w).sum();
    if total_weight > 0 {
        let pool = u64::from(remaining);
        let mut assigned: u64 = 0;
        for &(index, w) in &flex {
            let share = (pool * w / total_weight) as u16;
            sizes[index] = share;
            assigned += u64::from(share);
        }
        // Leftover cells from flooring go to the trailing flexible children,
        // one each, so remainders stay stable (a 101/2 split is 50 then 51).
        let leftover = (pool - assigned) as usize;
        for &(index, _) in flex.iter().rev().take(leftover) {
            sizes[index] += 1;
        }
    }

    // User resizes: exact cell offsets on top of the distribution.
    for (index, weight) in weights.iter().enumerate() {
        let adjusted = i64::from(sizes[index]) + i64::from(weight.resize_delta);
        sizes[index] = adjusted.clamp(0, i64::from(available)) as u16;
    }

    repair_sum(&mut sizes, available);
    sizes
}

/// Force `sizes` to sum to exactly `available`, adjusting from the end.
///
/// Slack (all-fixed splits that underfill, or resize deltas that drifted)
/// goes to the last child; excess is trimmed from the trailing children
/// toward zero. Trailing-first keeps the leading panes stable.
fn repair_sum(sizes: &mut [u16], available: u16) {
    let sum: u64 = sizes.iter().map(|&cells| u64::from(cells)).sum();
    let available = u64::from(available);

    if sum < available {
        if let Some(last) = sizes.last_mut() {
            *last += (available - sum) as u16;
        }
    } else if sum > available {
        let mut excess = sum - available;
        for cells in sizes.iter_mut().rev() {
            let trim = excess.min(u64::from(*cells));
            *cells -= trim as u16;
            excess -= trim;
            if excess == 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests;
