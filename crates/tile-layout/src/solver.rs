//! Geometry solver: a layout tree plus a tab rectangle in, exact pane
//! rectangles out.
//!
//! The tree is pure intent — structure and relative sizes — and this module
//! is the only place geometry is computed from it. That split of roles is
//! what makes terminal resizes cheap and safe: the tree never changes, the
//! solver just runs again over the new rectangle.
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

use crate::mode::LayoutMode;
use crate::size::SizeConstraint;
use crate::size::SizeWeight;
use crate::tree::{LayoutChild, LayoutNode, SplitNode};

/// The smallest useful terminal pane: two columns by one row. A pane PTY is
/// never sized below this floor.
pub const MIN_PANE_SIZE: Size = Size { cols: 2, rows: 1 };

/// The solved placement for one tree over one tab rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveResult {
    /// Every leaf pane exactly once, in layout order, with its solved
    /// rectangle. A collapsed stack member's rect is its one-row header
    /// strip; a zero-area rect means the pane is not visible at all.
    pub panes: Vec<(PaneId, Rect)>,
    /// Panes clipped to zero area because the layout no longer fits. Stable
    /// trailing order: the same panes suppress and restore as space changes.
    pub suppressed: Vec<PaneId>,
    /// `true` when suppression left nothing visible at all; the caller shows
    /// a terminal-too-small overlay instead of a pane grid.
    pub all_suppressed: bool,
    /// One entry per collapsed stack member, in layout order. The renderer
    /// draws these strips and mouse routing hit-tests them; both are
    /// Tile-owned regions, never forwarded to a PTY.
    pub stack_headers: Vec<StackHeader>,
}

/// The one-row strip standing in for a collapsed stack member.
///
/// Only collapsed members get headers — the active member shows its content
/// instead. The strip is a Tile-owned region: the renderer draws it and
/// mouse routing hit-tests it like a border, so a click on it activates the
/// member and is never forwarded to a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackHeader {
    /// The collapsed pane this header represents; clicking the strip
    /// activates it.
    pub pane: PaneId,
    /// The strip itself: one row spanning the stack's width.
    pub rect: Rect,
    /// Zero-based position of this member within its stack.
    pub position: usize,
    /// Total members in the stack, for indicators like `[2/5]`.
    pub total: usize,
}

/// Accumulators threaded through the solve recursion.
#[derive(Default)]
struct SolveState {
    panes: Vec<(PaneId, Rect)>,
    suppressed: Vec<PaneId>,
    headers: Vec<StackHeader>,
}

impl SolveState {
    fn into_result(self) -> SolveResult {
        // The overlay condition: space ran out (something was suppressed)
        // and no pane kept a visible rect. Panes that are zero-area for
        // other reasons (collapsed stack members, fullscreen hiding) do not
        // count as suppressed, but they cannot keep the overlay away either.
        let all_suppressed =
            !self.suppressed.is_empty() && self.panes.iter().all(|&(_, rect)| rect.is_empty());
        SolveResult {
            panes: self.panes,
            suppressed: self.suppressed,
            all_suppressed,
            stack_headers: self.headers,
        }
    }
}

/// Solve `tree` over `tab_rect`.
///
/// When the tree's floors no longer fit, trailing panes are suppressed —
/// solved to zero area and listed in [`SolveResult::suppressed`] — rather
/// than overlapped or shrunk below minimum. Suppression is stable: the same
/// panes drop out and return as the rect shrinks and regrows.
#[must_use]
pub fn solve(tree: &LayoutNode, tab_rect: Rect) -> SolveResult {
    let mut state = SolveState::default();
    solve_node(tree, tab_rect, &mut state);
    state.into_result()
}

/// Solve `tree` over `tab_rect` under a layout mode.
///
/// `Tiled` is [`solve`]. `Fullscreen` gives the focused pane the whole tab
/// and zero area to everyone else — without touching the tree, so leaving
/// fullscreen restores the prior layout exactly. No stack headers are drawn
/// over a fullscreen pane. A fullscreen mode pointing at a pane that is no
/// longer in the tree is treated as stale and falls back to the tiled
/// solve: the session stays visible with its normal grid instead of an
/// empty screen, and the user can simply toggle fullscreen again.
#[must_use]
pub fn solve_with_mode(tree: &LayoutNode, mode: LayoutMode, tab_rect: Rect) -> SolveResult {
    let LayoutMode::Fullscreen { focused } = mode else {
        return solve(tree, tab_rect);
    };
    if !tree.contains_pane(focused) {
        return solve(tree, tab_rect);
    }

    let mut state = SolveState::default();
    for pane in tree.leaf_panes() {
        if pane != focused {
            state.panes.push((pane, Rect::zero()));
        } else if tab_rect.size.cols < MIN_PANE_SIZE.cols || tab_rect.size.rows < MIN_PANE_SIZE.rows
        {
            state.panes.push((pane, Rect::zero()));
            state.suppressed.push(pane);
        } else {
            state.panes.push((pane, tab_rect));
        }
    }
    state.into_result()
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
            SplitDirection::Stacked => stack_min_size(split, default_min),
        },
    }
}

/// The smallest rectangle a stack can be solved into: its widest member by
/// one header row per collapsed member plus the active member's rows.
fn stack_min_size(split: &SplitNode, default_min: Size) -> Size {
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

/// The floor of one child slot along the split axis, for callers that only
/// know the slot index (resize uses this to bound what a donor can give).
pub(crate) fn slot_floor(split: &SplitNode, index: usize, horizontal: bool) -> u16 {
    let child_min = split
        .children
        .get(index)
        .map_or(Size { cols: 0, rows: 0 }, |child| {
            min_size(&child.node, MIN_PANE_SIZE)
        });
    let axis_min = if horizontal {
        child_min.cols
    } else {
        child_min.rows
    };
    child_floor(split, index, axis_min)
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

fn solve_node(node: &LayoutNode, rect: Rect, state: &mut SolveState) {
    match node {
        LayoutNode::Pane(id) => {
            // Backstop: a leaf that cannot show a usable pane is suppressed,
            // never rendered as a sliver.
            if rect.size.cols < MIN_PANE_SIZE.cols || rect.size.rows < MIN_PANE_SIZE.rows {
                state.panes.push((*id, Rect::zero()));
                state.suppressed.push(*id);
            } else {
                state.panes.push((*id, rect));
            }
        }
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                solve_directional(split, rect, state);
            }
            SplitDirection::Stacked => solve_stacked(split, rect, state),
        },
    }
}

/// Zero out a whole subtree and record every leaf as suppressed.
fn suppress_subtree(node: &LayoutNode, state: &mut SolveState) {
    for pane in node.leaf_panes() {
        state.panes.push((pane, Rect::zero()));
        state.suppressed.push(pane);
    }
}

/// Divide `rect` among the split's children along its axis and recurse.
///
/// Children that cannot fit are suppressed before distribution: a child
/// whose cross-axis minimum exceeds the rect is dropped individually, and
/// once the running sum of axis floors overflows the rect, that child and
/// everything after it drop too (trailing suppression). The children that
/// remain always fit at floor, so the recursion below never overlaps.
fn solve_directional(split: &SplitNode, rect: Rect, state: &mut SolveState) {
    let rects = directional_child_rects(split, rect);
    for (child, child_rect) in split.children.iter().zip(rects) {
        if child_rect.is_empty() {
            suppress_subtree(&child.node, state);
        } else {
            solve_node(&child.node, child_rect, state);
        }
    }
}

/// The rectangle each child of a directional split receives inside `rect`,
/// in child order. Suppressed children get a zero rect at their position.
///
/// This is the one place a directional split's geometry is decided; both
/// solving and resize preflighting read it. A kept child's rect always meets
/// the child's floor, so an empty rect here always means "suppressed".
pub(crate) fn directional_child_rects(split: &SplitNode, rect: Rect) -> Vec<Rect> {
    let horizontal = split.direction == SplitDirection::Horizontal;
    let (available, available_cross) = if horizontal {
        (rect.size.cols, rect.size.rows)
    } else {
        (rect.size.rows, rect.size.cols)
    };

    // Decide who fits: per-child cross-axis check, then trailing suppression
    // along the split axis.
    let mut kept = vec![false; split.children.len()];
    let mut floors_fit = true;
    let mut claimed: u32 = 0;
    let mut floors = vec![0u16; split.children.len()];
    for (index, child) in split.children.iter().enumerate() {
        let child_min = min_size(&child.node, MIN_PANE_SIZE);
        let (axis_min, cross_min) = if horizontal {
            (child_min.cols, child_min.rows)
        } else {
            (child_min.rows, child_min.cols)
        };
        floors[index] = child_floor(split, index, axis_min);
        if cross_min > available_cross {
            continue;
        }
        if floors_fit && claimed + u32::from(floors[index]) <= u32::from(available) {
            kept[index] = true;
            claimed += u32::from(floors[index]);
        } else {
            floors_fit = false;
        }
    }

    // Distribute over the kept children only, then lay rects in child order;
    // suppressed children sit at their position with zero area.
    let kept_weights: Vec<SizeWeight> = filter_kept(&split.weights, &kept);
    let kept_floors: Vec<u16> = filter_kept(&floors, &kept);
    let sizes = distribute(&kept_weights, &kept_floors, available);

    let mut rects = Vec::with_capacity(split.children.len());
    let mut offset: u16 = 0;
    let mut kept_index = 0;
    for &keep in &kept {
        if !keep {
            rects.push(Rect::zero());
            continue;
        }
        let cells = sizes[kept_index];
        kept_index += 1;
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
        rects.push(child_rect);
        offset = offset.saturating_add(cells);
    }
    rects
}

/// Keep only the elements whose flag is set, preserving order.
fn filter_kept<T: Copy>(items: &[T], kept: &[bool]) -> Vec<T> {
    items
        .iter()
        .zip(kept)
        .filter_map(|(&item, &keep)| keep.then_some(item))
        .collect()
}

/// Stacked children share the rect: the active child expands into whatever
/// remains after every collapsed member takes a one-row header strip.
///
/// Headers stay in layout order — members above the active child sit on
/// top, members below sit underneath — so the visual stack matches the
/// tree. A collapsed member's pane rect *is* its header strip; the matching
/// [`StackHeader`] entry carries the indicator metadata.
///
/// If the rect cannot hold every header plus the active child at minimum
/// size (or is narrower than the widest member needs), the whole stack
/// suppresses as one unit: no headers, every member zero-area. A stack
/// never shows a partial subset of itself.
fn solve_stacked(split: &SplitNode, rect: Rect, state: &mut SolveState) {
    if split.children.is_empty() {
        return;
    }
    let active = split.active.min(split.children.len() - 1);
    let total = split.children.len();
    let header_count = (total - 1) as u16;

    let needed = stack_min_size(split, MIN_PANE_SIZE);
    if rect.size.rows < needed.rows || rect.size.cols < needed.cols {
        for child in &split.children {
            suppress_subtree(&child.node, state);
        }
        return;
    }

    let active_rows = rect.size.rows - header_count;
    let mut y = rect.origin.y;
    for (index, child) in split.children.iter().enumerate() {
        if index == active {
            let active_rect = Rect::new(
                Point {
                    x: rect.origin.x,
                    y,
                },
                Size {
                    cols: rect.size.cols,
                    rows: active_rows,
                },
            );
            solve_node(&child.node, active_rect, state);
            y = y.saturating_add(active_rows);
            continue;
        }
        let header_rect = Rect::new(
            Point {
                x: rect.origin.x,
                y,
            },
            Size {
                cols: rect.size.cols,
                rows: 1,
            },
        );
        emit_header(child, header_rect, index, total, state);
        y = y.saturating_add(1);
    }
}

/// Place one collapsed stack member on its header strip.
///
/// Members are panes by construction (the stack edits only ever add
/// leaves). If a subtree somehow ends up collapsed in a stack, its first
/// leaf stands in on the strip and the rest solve to zero — deterministic
/// and unreachable through the public edits.
fn emit_header(
    child: &LayoutChild,
    header_rect: Rect,
    index: usize,
    total: usize,
    state: &mut SolveState,
) {
    let leaves = child.node.leaf_panes();
    let Some((&first, rest)) = leaves.split_first() else {
        return;
    };
    state.panes.push((first, header_rect));
    state.headers.push(StackHeader {
        pane: first,
        rect: header_rect,
        position: index,
        total,
    });
    for &pane in rest {
        state.panes.push((pane, Rect::zero()));
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

/// Pull each preferred child toward its target using only slack: donors are
/// flexible siblings with cells above their floor, never `Fixed`/`Percent`
/// children — a preference is a hint and must not override an exact size.
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
            let need = target - current;
            let taken = take_cells(sizes, weights, floors, need, index, DonorPool::FlexibleOnly);
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
            let taken = take_cells(sizes, weights, floors, need, index, DonorPool::Anyone);
            sizes[index] += taken;
        }
    }
}

/// Who may give up cells in [`take_cells`]. A minimum-size clamp may tap
/// anyone — floors outrank exact sizes — but a preferred target is only a
/// hint and must stay within flexible slack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DonorPool {
    FlexibleOnly,
    Anyone,
}

/// Take up to `need` cells from siblings, trailing-first, leaving every donor
/// at or above its floor. Flexible donors give first; `Fixed`/`Percent`
/// children are tapped only when the pool allows it and the flexible donors
/// are exhausted.
fn take_cells(
    sizes: &mut [u16],
    weights: &[SizeWeight],
    floors: &[u16],
    need: u16,
    skip: usize,
    pool: DonorPool,
) -> u16 {
    let mut taken: u16 = 0;
    for flexible_pass in [true, false] {
        if !flexible_pass && pool == DonorPool::FlexibleOnly {
            break;
        }
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
