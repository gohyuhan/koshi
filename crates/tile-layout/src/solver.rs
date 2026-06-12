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
//! their floor/target) in proportion to their weights. User resizes apply
//! next as exact cell deltas; then preferred targets are honored within
//! whatever slack flexible siblings can give, and finally every child is
//! clamped up to its floor whenever the floors fit at all.
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

/// The smallest useful terminal pane: two columns by one row. A pane PTY is
/// never sized below this floor.
pub const MIN_PANE_SIZE: Size = Size { cols: 2, rows: 1 };

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

/// `true` when every pane in `tree` can be placed inside `rect` at minimum
/// size. Command handlers call this before mutating a layout: a split that
/// cannot fit is rejected up front instead of producing a broken solve.
///
/// `default_min` is the floor for panes without an explicit one; terminal
/// panes use [`MIN_PANE_SIZE`].
#[must_use]
pub fn fits(tree: &LayoutNode, rect: Rect, default_min: Size) -> bool {
    let needed = min_size(tree, default_min);
    needed.cols <= rect.size.cols && needed.rows <= rect.size.rows
}

/// The minimum a pane needs once borders are drawn around its content:
/// bordered panes spend one cell per side on each axis.
#[must_use]
pub fn border_inclusive_min(content_min: Size, has_borders: bool) -> Size {
    if has_borders {
        Size {
            cols: content_min.cols.saturating_add(2),
            rows: content_min.rows.saturating_add(2),
        }
    } else {
        content_min
    }
}

/// The smallest rectangle this subtree can be solved into.
///
/// Directional splits sum their children's floors along the split axis and
/// take the largest across it. A stack needs its widest child, one header row
/// per collapsed child, plus the active child's rows.
#[must_use]
pub fn min_size(node: &LayoutNode, default_min: Size) -> Size {
    match node {
        LayoutNode::Pane(_) => default_min,
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal => {
                let mut cols: u16 = 0;
                let mut rows: u16 = 0;
                for (index, child) in split.children.iter().enumerate() {
                    let child_min = min_size(&child.node, default_min);
                    let floor = child_floor(split, index, child_min.cols);
                    cols = cols.saturating_add(floor);
                    rows = rows.max(child_min.rows);
                }
                Size { cols, rows }
            }
            SplitDirection::Vertical => {
                let mut cols: u16 = 0;
                let mut rows: u16 = 0;
                for (index, child) in split.children.iter().enumerate() {
                    let child_min = min_size(&child.node, default_min);
                    let floor = child_floor(split, index, child_min.rows);
                    rows = rows.saturating_add(floor);
                    cols = cols.max(child_min.cols);
                }
                Size { cols, rows }
            }
            SplitDirection::Stacked => {
                let mut cols: u16 = 0;
                for child in &split.children {
                    cols = cols.max(min_size(&child.node, default_min).cols);
                }
                let header_rows = split.children.len().saturating_sub(1) as u16;
                let active_rows = split
                    .children
                    .get(split.active)
                    .map_or(0, |child| min_size(&child.node, default_min).rows);
                Size {
                    cols,
                    rows: header_rows.saturating_add(active_rows),
                }
            }
        },
    }
}

/// The floor for one child slot along the split axis: the larger of the
/// subtree's own minimum and any floor its weight declares.
fn child_floor(split: &SplitNode, index: usize, subtree_axis_min: u16) -> u16 {
    let weight_floor = split.weights.get(index).map_or(0, |weight| {
        let primary_floor = match weight.primary {
            SizeConstraint::Min(cells) => cells,
            _ => 0,
        };
        primary_floor.max(weight.min.unwrap_or(0))
    });
    subtree_axis_min.max(weight_floor)
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
    let floors: Vec<u16> = split
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            let child_min = min_size(&child.node, MIN_PANE_SIZE);
            let axis_min = if horizontal {
                child_min.cols
            } else {
                child_min.rows
            };
            child_floor(split, index, axis_min)
        })
        .collect();
    let sizes = distribute(&split.weights, &floors, available);

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
/// leaves cells unassigned and never assigns more than it has. When the
/// floors fit, every child also ends at or above its floor.
fn distribute(weights: &[SizeWeight], floors: &[u16], available: u16) -> Vec<u16> {
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
    honor_preferred(&mut sizes, weights, floors);
    clamp_to_floors(&mut sizes, weights, floors, available);
    sizes
}

/// `true` when this weight may give up or take cells during adjustment.
/// `Fixed` and `Percent` children hold their computed size unless a floor
/// elsewhere forces the issue.
fn is_flexible(weight: &SizeWeight) -> bool {
    matches!(
        weight.primary,
        SizeConstraint::Flex(_) | SizeConstraint::Min(_) | SizeConstraint::Preferred(_)
    )
}

/// The target a child aims for when space allows, if it declared one.
/// The overlay wins over a `Preferred` primary, since overlays sit on top.
fn preferred_target(weight: &SizeWeight) -> Option<u16> {
    weight.preferred.or(match weight.primary {
        SizeConstraint::Preferred(cells) => Some(cells),
        _ => None,
    })
}

/// Pull each preferred child toward its target using only slack: donors give
/// cells down to their floor, receivers are the trailing flexible siblings.
/// Children are visited in order, so an earlier target wins contested slack.
fn honor_preferred(sizes: &mut [u16], weights: &[SizeWeight], floors: &[u16]) {
    for index in 0..weights.len() {
        let Some(target) = preferred_target(&weights[index]) else {
            continue;
        };
        let current = sizes[index];
        if current > target {
            // Surplus above the target flows to the trailing-most flexible
            // sibling; without one there is no slack to rebalance into.
            let floor = floors.get(index).copied().unwrap_or(0);
            let surplus = current.saturating_sub(target.max(floor));
            let receiver = (0..weights.len())
                .rev()
                .find(|&i| i != index && is_flexible(&weights[i]));
            if let Some(receiver) = receiver {
                sizes[index] -= surplus;
                sizes[receiver] = sizes[receiver].saturating_add(surplus);
            }
        } else if current < target {
            let taken = take_cells(sizes, weights, floors, target - current, index);
            sizes[index] = sizes[index].saturating_add(taken);
        }
    }
}

/// Raise every child to its floor, funding the deficit from siblings above
/// theirs. Skipped entirely when the floors cannot fit — that is the
/// suppression path, not a clamping problem.
fn clamp_to_floors(sizes: &mut [u16], weights: &[SizeWeight], floors: &[u16], available: u16) {
    let total_floor: u64 = floors.iter().map(|&cells| u64::from(cells)).sum();
    if total_floor > u64::from(available) {
        return;
    }
    for index in 0..sizes.len() {
        let floor = floors.get(index).copied().unwrap_or(0);
        if sizes[index] < floor {
            let need = floor - sizes[index];
            let taken = take_cells(sizes, weights, floors, need, index);
            sizes[index] += taken;
        }
    }
}

/// Take up to `need` cells from siblings, trailing-first, leaving every donor
/// at or above its floor. Flexible donors give first; `Fixed`/`Percent`
/// children are only tapped when the flexible ones are exhausted.
fn take_cells(
    sizes: &mut [u16],
    weights: &[SizeWeight],
    floors: &[u16],
    need: u16,
    skip: usize,
) -> u16 {
    let mut taken: u16 = 0;
    for flexible_pass in [true, false] {
        for index in (0..sizes.len()).rev() {
            if taken == need {
                return taken;
            }
            if index == skip || is_flexible(&weights[index]) != flexible_pass {
                continue;
            }
            let floor = floors.get(index).copied().unwrap_or(0);
            let spare = sizes[index].saturating_sub(floor);
            let give = spare.min(need - taken);
            sizes[index] -= give;
            taken += give;
        }
    }
    taken
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
