//! Geometry solver: a layout tree plus a tab rectangle in, exact pane
//! rectangles out.
//!
//! The tree holds structure and relative sizes only; this module is the one
//! place that computes geometry from it. A terminal resize changes no tree;
//! the solver runs again over the new rectangle.
//!
//! Solving is pure and deterministic: the same tree over the same rect always
//! yields the same placement. Every leaf appears in the result exactly once, in
//! layout order, and at [`PaneSizing::gap`] `0` the placed rects tile the tab
//! exactly — a split's children account for every cell of the split's own
//! rectangle. A larger gap leaves that many blank cells between each pair of
//! kept children of a directional split.
//!
//! ## Distribution order
//!
//! Along a split axis, children claim cells in constraint order: `Fixed`
//! sizes first, then `Percent` of the axis, then the remainder is shared by
//! the flexible children (`Flex`, and `Min`/`Preferred`, which flex around
//! their floor/target) in proportion to their weights. User resizes apply
//! next as exact cell deltas, and the sizes are repaired to sum to the axis;
//! then preferred targets are honored within whatever slack flexible
//! siblings can give, and finally every child is clamped up to its floor
//! whenever the floors fit at all.
//!
//! Cells that integer division leaves over go to the *trailing* children, one
//! each: a 101-column 50/50 split solves to 50 and 51. When no flexible child
//! exists to absorb slack, the last child takes it.

use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use serde::{Deserialize, Serialize};

use crate::mode::LayoutMode;
use crate::size::SizeConstraint;
use crate::size::SizeWeight;
use crate::tree::{LayoutChild, LayoutNode, SplitNode};

/// The smallest content size of a pane: two columns by one row. A pane's PTY
/// (the pseudo-terminal process feeding its content) is never sized below
/// it.
pub const MIN_PANE_SIZE: Size = Size { cols: 2, rows: 1 };

/// The per-pane sizing every solve, resize and removal takes: the smallest
/// content size of a leaf pane and the blank cells between two children of
/// one horizontal or vertical split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSizing {
    /// Smallest content size of a leaf pane: the configured pane minimum,
    /// floored at [`MIN_PANE_SIZE`] by the caller.
    pub min: Size,
    /// Blank cells between two consecutive kept children of a horizontal or
    /// vertical split. The gaps come off the split axis before any child is
    /// sized, and a child that is suppressed reserves none. A stacked split
    /// places no gap between its members. `0` places children edge to edge.
    ///
    /// `A | B` over 120 columns with `gap: 2` solves to `A` at columns 0–58
    /// and `B` at 61–119; columns 59 and 60 belong to no pane.
    pub gap: u16,
}

impl Default for PaneSizing {
    /// [`MIN_PANE_SIZE`] as the floor and no gap.
    fn default() -> Self {
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        }
    }
}

/// The solved placement for one tree over one tab rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveResult {
    /// Every leaf pane exactly once, in layout order, with its solved
    /// rectangle. A collapsed stack member's rect is its one-row header
    /// strip; a zero-area rect means the pane is not visible at all.
    pub panes: Vec<(PaneId, Rect)>,
    /// Panes clipped to zero area when the layout no longer fits. A pane
    /// that is zero-area for another reason is not listed here: one hidden
    /// behind a fullscreen pane, or a collapsed stack member's non-header
    /// leaves. Trailing order is stable: the same panes suppress and restore
    /// as space changes.
    pub suppressed: Vec<PaneId>,
    /// `true` when `suppressed` is non-empty and every rect in `panes` is
    /// zero-area; the caller shows a terminal-too-small overlay instead of a
    /// pane grid.
    pub all_suppressed: bool,
    /// One entry per collapsed stack member, in layout order.
    pub stack_headers: Vec<StackHeader>,
}

/// The one-row strip standing in for a collapsed stack member.
///
/// A member is collapsed when it is not the stack's active member (the
/// stack's `active` index, clamped into bounds); the solver does not read
/// [`LayoutChild::collapsed`]. The strip is a Koshi-owned region: the
/// renderer draws it and mouse routing hit-tests it, and a click on it
/// activates the member instead of reaching a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackHeader {
    /// The collapsed pane this header represents; clicking the strip
    /// activates it.
    pub pane: PaneId,
    /// The strip itself: one row spanning the stack's width.
    pub rect: Rect,
    /// Zero-based position of this member within its stack.
    pub position: usize,
    /// Total members in the stack, the active one included.
    pub total: usize,
}

/// Accumulators threaded through the solve recursion.
struct SolveState {
    /// The [`PaneSizing`] for the whole solve.
    sizing: PaneSizing,
    panes: Vec<(PaneId, Rect)>,
    suppressed: Vec<PaneId>,
    headers: Vec<StackHeader>,
}

impl SolveState {
    fn new(sizing: PaneSizing) -> Self {
        SolveState {
            sizing,
            panes: Vec::new(),
            suppressed: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn into_result(self) -> SolveResult {
        // Set when at least one pane was suppressed and every pane's rect is
        // empty. A pane that is zero-area for another reason (hidden behind
        // a fullscreen pane, or a non-header leaf of a collapsed subtree) is
        // not suppressed, but its empty rect counts.
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

/// Solve `tree` over `tab_rect` with [`PaneSizing::default`].
///
/// Rects are half-open: `origin` is inclusive, the right and bottom edges
/// are exclusive. At [`PaneSizing::gap`] `0` adjacent panes meet without
/// sharing cells.
///
/// When the tree's floors no longer fit, trailing panes are suppressed:
/// solved to zero area and listed in [`SolveResult::suppressed`].
/// Suppression is stable: the same panes drop out and return as the rect
/// shrinks and regrows.
#[must_use]
pub fn solve(tree: &LayoutNode, tab_rect: Rect) -> SolveResult {
    solve_with_min(tree, tab_rect, PaneSizing::default())
}

/// [`solve`] with an explicit [`PaneSizing`]: `sizing.min` is the content
/// floor of every leaf (the configured pane minimum, floored at
/// [`MIN_PANE_SIZE`] by the caller) and `sizing.gap` the blank cells between
/// split children.
#[must_use]
pub fn solve_with_min(tree: &LayoutNode, tab_rect: Rect, sizing: PaneSizing) -> SolveResult {
    let mut state = SolveState::new(sizing);
    solve_node(tree, tab_rect, &mut state);
    state.into_result()
}

/// Solve `tree` over `tab_rect` under a layout mode.
///
/// `Tiled` is [`solve`]. `Fullscreen` gives the focused pane the whole tab
/// and zero area to every other pane, changes no tree, and emits no stack
/// headers. A tab smaller than the focused pane's border-inclusive floor
/// suppresses it. A fullscreen mode whose focused pane is not in the tree
/// falls back to the tiled solve.
#[must_use]
pub fn solve_with_mode(tree: &LayoutNode, mode: LayoutMode, tab_rect: Rect) -> SolveResult {
    solve_with_mode_min(tree, mode, tab_rect, PaneSizing::default())
}

/// [`solve_with_mode`] with an explicit [`PaneSizing`]; see
/// [`solve_with_min`] for what `sizing` sets.
#[must_use]
pub fn solve_with_mode_min(
    tree: &LayoutNode,
    mode: LayoutMode,
    tab_rect: Rect,
    sizing: PaneSizing,
) -> SolveResult {
    let LayoutMode::Fullscreen { focused } = mode else {
        return solve_with_min(tree, tab_rect, sizing);
    };
    if !tree.contains_pane(focused) {
        return solve_with_min(tree, tab_rect, sizing);
    }

    let mut state = SolveState::new(sizing);
    // The focused pane is suppressed when the tab is below its
    // border-inclusive floor, the same floor the tiled path uses.
    let floor = border_inclusive_min(sizing.min, true);
    let too_small = tab_rect.size.cols < floor.cols || tab_rect.size.rows < floor.rows;
    for pane in tree.leaf_panes() {
        if pane != focused {
            state.panes.push((pane, Rect::zero()));
        } else if too_small {
            state.panes.push((pane, Rect::zero()));
            state.suppressed.push(pane);
        } else {
            state.panes.push((pane, tab_rect));
        }
    }
    state.into_result()
}

/// `true` when every pane in `tree` can be placed inside `rect` at minimum
/// size: the [`min_size`] of `tree` under `sizing` fits `rect` on both axes.
#[must_use]
pub fn fits(tree: &LayoutNode, rect: Rect, sizing: PaneSizing) -> bool {
    let needed = min_size(tree, sizing);
    needed.cols <= rect.size.cols && needed.rows <= rect.size.rows
}

/// `content_min` plus one cell per side on each axis when `has_borders` is
/// `true`, saturating at `u16::MAX`; `content_min` unchanged when `false`.
/// A 2 by 1 content minimum with borders is 4 by 3.
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
/// A leaf needs `sizing.min` plus one border cell per side
/// ([`border_inclusive_min`]); siblings do not share border cells. A
/// horizontal or vertical split sums its children's floors along the split
/// axis, plus one `sizing.gap` between each pair of children, and takes the
/// largest child floor across it; a slot's declared floor (`Min` primary or
/// `min` overlay) raises that child's share of the sum. A stack needs its
/// widest member, one header row per collapsed member, plus the active
/// member's rows, and places no gap. Every sum saturates at `u16::MAX`.
#[must_use]
pub fn min_size(node: &LayoutNode, sizing: PaneSizing) -> Size {
    match node {
        LayoutNode::Pane(_) => border_inclusive_min(sizing.min, true),
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                let horizontal = split.direction == SplitDirection::Horizontal;
                // Floors sum along the split axis; the cross axis takes the
                // largest child minimum.
                let mut along: u16 = 0;
                let mut across: u16 = 0;
                for (index, child) in split.children.iter().enumerate() {
                    let (axis_min, cross_min) =
                        axis_and_cross(min_size(&child.node, sizing), horizontal);
                    along = along.saturating_add(child_floor(split, index, axis_min));
                    across = across.max(cross_min);
                }
                // One gap sits between each pair of children.
                let gaps = sizing
                    .gap
                    .saturating_mul(split.children.len().saturating_sub(1) as u16);
                along = along.saturating_add(gaps);
                if horizontal {
                    Size {
                        cols: along,
                        rows: across,
                    }
                } else {
                    Size {
                        cols: across,
                        rows: along,
                    }
                }
            }
            SplitDirection::Stacked => stack_min_size(split, sizing),
        },
    }
}

/// The smallest rectangle a stack can be solved into: its widest member by
/// one header row per collapsed member plus the active member's rows. A
/// stack places no gap between its members; an empty stack needs 0 by 0.
pub(crate) fn stack_min_size(split: &SplitNode, sizing: PaneSizing) -> Size {
    let active = split.active_index();
    let mut cols: u16 = 0;
    let mut active_rows: u16 = 0;
    for (index, child) in split.children.iter().enumerate() {
        let child_min = min_size(&child.node, sizing);
        cols = cols.max(child_min.cols);
        if index == active {
            active_rows = child_min.rows;
        }
    }
    let header_rows = split.children.len().saturating_sub(1) as u16;
    Size {
        cols,
        rows: header_rows.saturating_add(active_rows),
    }
}

/// The floor of the child at `index` along the split axis: the larger of
/// that subtree's minimum and the floor its weight declares. A missing
/// child counts as a 0 by 0 subtree.
pub(crate) fn slot_floor(
    split: &SplitNode,
    index: usize,
    horizontal: bool,
    sizing: PaneSizing,
) -> u16 {
    let child_min = split
        .children
        .get(index)
        .map_or(Size { cols: 0, rows: 0 }, |child| {
            min_size(&child.node, sizing)
        });
    child_floor(split, index, axis_and_cross(child_min, horizontal).0)
}

/// The cell count of `rect`: columns × rows. A 40 by 24 rect gives 960.
pub(crate) fn cell_area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
}

/// The `size` measures along the split axis and across it: columns then rows
/// for a horizontal split, rows then columns for a vertical one.
fn axis_and_cross(size: Size, horizontal: bool) -> (u16, u16) {
    if horizontal {
        (size.cols, size.rows)
    } else {
        (size.rows, size.cols)
    }
}

/// The floor for one child slot along the split axis: the larger of the
/// subtree's own minimum and any floor its weight declares (`Min` primary
/// or `min` overlay). A missing weight declares no floor.
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
            // A leaf whose rect is below its border-inclusive floor on
            // either axis is suppressed.
            let floor = border_inclusive_min(state.sizing.min, true);
            if rect.size.cols < floor.cols || rect.size.rows < floor.rows {
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
/// whose cross-axis minimum exceeds the rect is dropped on its own, and
/// once the running sum of axis floors overflows the rect, that child and
/// every child after it drop too. The children that remain always fit at
/// their floor.
fn solve_directional(split: &SplitNode, rect: Rect, state: &mut SolveState) {
    let rects = directional_child_rects(split, rect, state.sizing);
    for (child, child_rect) in split.children.iter().zip(rects) {
        if child_rect.is_empty() {
            suppress_subtree(&child.node, state);
        } else {
            solve_node(&child.node, child_rect, state);
        }
    }
}

/// The rectangle each child of a directional split receives inside `rect`,
/// in child order. A suppressed child gets a zero rect at its position; a
/// kept child's rect meets the child's floor, and an empty rect always means
/// "suppressed".
///
/// One `sizing.gap` sits between each pair of kept children and comes off the
/// axis before any child is sized. A suppressed child reserves no gap; the
/// survivors share those cells.
pub(crate) fn directional_child_rects(
    split: &SplitNode,
    rect: Rect,
    sizing: PaneSizing,
) -> Vec<Rect> {
    let horizontal = split.direction == SplitDirection::Horizontal;
    let (available, available_cross) = axis_and_cross(rect.size, horizontal);
    let gap = sizing.gap;

    // Decide who fits: per-child cross-axis check, then trailing suppression
    // along the split axis. The kept children's weights and floors are
    // collected in the same pass, in child order. A child without a weight
    // takes the default share.
    let mut kept = vec![false; split.children.len()];
    let mut kept_weights: Vec<SizeWeight> = Vec::with_capacity(split.children.len());
    let mut kept_floors: Vec<u16> = Vec::with_capacity(split.children.len());
    let mut floors_fit = true;
    let mut claimed: u32 = 0;
    for (index, child) in split.children.iter().enumerate() {
        let (axis_min, cross_min) = axis_and_cross(min_size(&child.node, sizing), horizontal);
        let floor = child_floor(split, index, axis_min);
        if cross_min > available_cross {
            continue;
        }
        // Every kept child after the first is preceded by one gap.
        let lead: u32 = if kept_weights.is_empty() {
            0
        } else {
            u32::from(gap)
        };
        if floors_fit && claimed + lead + u32::from(floor) <= u32::from(available) {
            kept[index] = true;
            claimed += lead + u32::from(floor);
            kept_weights.push(split.weights.get(index).copied().unwrap_or_default());
            kept_floors.push(floor);
        } else {
            floors_fit = false;
        }
    }

    // The gaps between kept children come off the axis before any child is
    // sized. Distribute over the kept children only, then lay rects in child
    // order; suppressed children sit at their position with zero area.
    let gaps = gap.saturating_mul(kept_weights.len().saturating_sub(1) as u16);
    let available_for_children = available.saturating_sub(gaps);
    let sizes = distribute(&kept_weights, &kept_floors, available_for_children);

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
        offset = offset.saturating_add(cells).saturating_add(gap);
    }
    rects
}

/// Stacked children share the rect: the active child expands into whatever
/// remains after every collapsed member takes a one-row header strip.
///
/// Headers stay in layout order: members before the active child sit above
/// it, members after it sit below. A collapsed member's pane rect *is* its
/// header strip; the matching [`StackHeader`] entry carries the indicator
/// metadata.
///
/// If the rect cannot hold every header plus the active child at minimum
/// size, or is narrower than the widest member needs, the whole stack
/// suppresses as one unit: no headers, every member zero-area.
fn solve_stacked(split: &SplitNode, rect: Rect, state: &mut SolveState) {
    if split.children.is_empty() {
        return;
    }
    let active = split.active_index();
    let total = split.children.len();
    let header_count = (total - 1) as u16;

    let needed = stack_min_size(split, state.sizing);
    if rect.size.rows < needed.rows || rect.size.cols < needed.cols {
        for child in &split.children {
            suppress_subtree(&child.node, state);
        }
        return;
    }

    let active_rows = rect.size.rows - header_count;
    // A band of `rows` rows starting at `y`, spanning the stack's width.
    let rows_at = |y: u16, rows: u16| {
        Rect::new(
            Point {
                x: rect.origin.x,
                y,
            },
            Size {
                cols: rect.size.cols,
                rows,
            },
        )
    };
    let mut y = rect.origin.y;
    for (index, child) in split.children.iter().enumerate() {
        if index == active {
            solve_node(&child.node, rows_at(y, active_rows), state);
            y = y.saturating_add(active_rows);
        } else {
            emit_header(child, rows_at(y, 1), index, total, state);
            y = y.saturating_add(1);
        }
    }
}

/// Place one collapsed stack member on its header strip.
///
/// A member that is a subtree puts its first leaf on the strip; its other
/// leaves solve to zero area and are not listed as suppressed. A member with
/// no leaf gets no header and no pane entry.
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
/// The returned sizes sum to exactly `available`. When the floors fit,
/// every child also ends at or above its floor.
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

    // Percentages are shares of the whole axis, floored to cells; a value
    // above 100 counts as 100.
    for (index, weight) in weights.iter().enumerate() {
        if let SizeConstraint::Percent(percent) = weight.primary {
            let want = (u32::from(available) * u32::from(percent.min(100)) / 100) as u16;
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
    // A zero total weight gives every share `0`; the leftover pass then adds
    // one cell to each trailing flexible child, up to the pool, and
    // `repair_sum` hands the rest to the last child.
    let total_weight: u64 = flex.iter().map(|&(_, w)| w).sum();
    if !flex.is_empty() {
        let pool = u64::from(remaining);
        let mut assigned: u64 = 0;
        for &(index, w) in &flex {
            let share = (pool * w).checked_div(total_weight).unwrap_or(0) as u16;
            sizes[index] = share;
            assigned += u64::from(share);
        }
        // Leftover cells from flooring go to the trailing flexible children,
        // one each: a 101/2 split is 50 then 51.
        let leftover = (pool - assigned) as usize;
        for &(index, _) in flex.iter().rev().take(leftover) {
            sizes[index] += 1;
        }
    }

    // User resizes: exact cell offsets on top of the distribution, each
    // result clamped to `0..=available`.
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
/// `Fixed` and `Percent` children give cells only in [`clamp_to_floors`].
fn is_flexible(weight: &SizeWeight) -> bool {
    matches!(
        weight.primary,
        SizeConstraint::Flex(_) | SizeConstraint::Min(_) | SizeConstraint::Preferred(_)
    )
}

/// The target a child aims for when slack allows: the `preferred` overlay
/// when set, else a `Preferred` primary's cells, else `None`.
fn preferred_target(weight: &SizeWeight) -> Option<u16> {
    weight.preferred.or(match weight.primary {
        SizeConstraint::Preferred(cells) => Some(cells),
        _ => None,
    })
}

/// Pull each preferred child toward its target using only slack: donors are
/// flexible siblings with cells above their floor, never `Fixed`/`Percent`
/// children. Children are visited in order, and each preferred child can
/// take back cells an earlier one gained: a surplus goes to the trailing-most
/// flexible sibling, and a deficit is taken from flexible siblings
/// trailing-first, an earlier preferred child above its floor included. Two
/// `Preferred(20)` children over 100 cells end at 80 and 20.
fn honor_preferred(sizes: &mut [u16], weights: &[SizeWeight], floors: &[u16]) {
    for index in 0..weights.len() {
        let Some(target) = preferred_target(&weights[index]) else {
            continue;
        };
        let current = sizes[index];
        if current > target {
            // Surplus above the larger of the target and the floor flows to
            // the trailing-most flexible sibling; without one the surplus
            // stays where it is.
            let floor = floors[index];
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
/// theirs. Does nothing when the floors do not fit in `available`.
fn clamp_to_floors(sizes: &mut [u16], weights: &[SizeWeight], floors: &[u16], available: u16) {
    let total_floor: u64 = floors.iter().map(|&cells| u64::from(cells)).sum();
    if total_floor > u64::from(available) {
        return;
    }
    for index in 0..sizes.len() {
        let floor = floors[index];
        if sizes[index] < floor {
            let need = floor - sizes[index];
            let taken = take_cells(sizes, weights, floors, need, index, DonorPool::Anyone);
            sizes[index] += taken;
        }
    }
}

/// Who may give up cells in [`take_cells`]: `FlexibleOnly` limits donors to
/// flexible children, `Anyone` also taps `Fixed` and `Percent` ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DonorPool {
    FlexibleOnly,
    Anyone,
}

/// Take up to `need` cells from siblings other than `skip`, trailing-first,
/// leaving every donor at or above its floor. Flexible donors give first;
/// `Fixed`/`Percent` children are tapped only when `pool` allows it and the
/// flexible donors are exhausted. Returns the cells taken, at most `need`.
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
            let floor = floors[index];
            let spare = sizes[index].saturating_sub(floor);
            let give = spare.min(need - taken);
            sizes[index] -= give;
            taken += give;
        }
    }
    taken
}

/// Force `sizes` to sum to exactly `available`, adjusting from the end: a
/// shortfall goes to the last child; an excess is trimmed from the trailing
/// children toward zero, leaving the leading ones untouched.
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
