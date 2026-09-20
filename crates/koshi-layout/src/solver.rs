//! Geometry solver: a layout tree plus a tab rectangle in, exact pane
//! rectangles out.
//!
//! The tree holds structure and relative sizes only; this module is the one
//! place that computes geometry from it. A terminal resize changes no tree;
//! the solver runs again over the new rectangle.
//!
//! Solving is pure and deterministic: the same tree over the same rect always
//! yields the same placement. Every leaf appears in the result exactly once, in
//! layout order, and at [`PaneSizing::gap_cell_count`] `0` the placed rects tile the tab
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
use crate::tree::{LayoutNode, SplitNode};

/// The smallest content size of a pane: two columns by one row. A pane's PTY
/// (the pseudo-terminal process feeding its content) is never sized below
/// it.
pub const MIN_PANE_SIZE: Size = Size {
    column_count: 2,
    row_count: 1,
};

/// The per-pane sizing every solve, resize and removal takes: the smallest
/// content size of a leaf pane and the blank cells between two children of
/// one horizontal or vertical split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneSizing {
    /// Smallest content size of a leaf pane: the configured pane minimum,
    /// floored at [`MIN_PANE_SIZE`] by the caller.
    pub minimum_size: Size,
    /// Blank cells between two consecutive kept children of a horizontal or
    /// vertical split. The gaps come off the split axis before any child is
    /// sized, and a child that is suppressed reserves none. A stacked split
    /// places no gap between its members. `0` places children edge to edge.
    ///
    /// `A | B` over 120 columns with `gap: 2` solves to `A` at columns 0–58
    /// and `B` at 61–119; columns 59 and 60 belong to no pane.
    pub gap_cell_count: u16,
}

impl Default for PaneSizing {
    /// [`MIN_PANE_SIZE`] as the floor and no gap.
    fn default() -> Self {
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        }
    }
}

/// The solved placement for one tree over one tab rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutSolve {
    /// Every leaf pane exactly once, in layout order, with its solved
    /// rectangle. A collapsed stack member's rect is its one-row header
    /// strip; a zero-area rect means the pane is not visible at all.
    pub pane_rects: Vec<(PaneId, Rect)>,
    /// Panes clipped to zero area when the layout no longer fits. A pane
    /// that is zero-area for another reason is not listed here: one hidden
    /// behind a fullscreen pane, or a collapsed stack member's non-header
    /// leaves. Trailing order is stable: the same panes suppress and restore
    /// as space changes.
    pub suppressed_pane_ids: Vec<PaneId>,
    /// `true` when `suppressed` is non-empty and every rect in `panes` is
    /// zero-area; the caller shows a terminal-too-small overlay instead of a
    /// pane grid.
    pub is_all_panes_suppressed: bool,
    /// One entry per collapsed stack member, in layout order.
    pub stack_headers: Vec<StackHeader>,
}

/// The one-row strip standing in for a collapsed stack member.
///
/// A member is collapsed when it is not the stack's active member (the
/// stack's `active` index, clamped into bounds). The strip is a Koshi-owned
/// region: the
/// renderer draws it and mouse routing hit-tests it, and a click on it
/// activates the member instead of reaching a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackHeader {
    /// The collapsed pane this header represents; clicking the strip
    /// activates it.
    pub pane_id: PaneId,
    /// The strip itself: one row spanning the stack's width.
    pub header_rect: Rect,
    /// Zero-based position of this member within its stack.
    pub member_index: usize,
    /// Total members in the stack, the active one included.
    pub member_count: usize,
}

/// Accumulators threaded through the solve recursion.
struct LayoutSolveState {
    /// The [`PaneSizing`] for the whole solve.
    pane_sizing: PaneSizing,
    pane_rects: Vec<(PaneId, Rect)>,
    suppressed_pane_ids: Vec<PaneId>,
    stack_headers: Vec<StackHeader>,
}

impl LayoutSolveState {
    fn from_pane_sizing(pane_sizing: PaneSizing) -> Self {
        LayoutSolveState {
            pane_sizing,
            pane_rects: Vec::new(),
            suppressed_pane_ids: Vec::new(),
            stack_headers: Vec::new(),
        }
    }

    fn into_layout_solve(self) -> LayoutSolve {
        // Set when at least one pane was suppressed and every pane's rect is
        // empty. A pane that is zero-area for another reason (hidden behind
        // a fullscreen pane, or a non-header leaf of a collapsed subtree) is
        // not suppressed, but its empty rect counts.
        let is_all_panes_suppressed = !self.suppressed_pane_ids.is_empty()
            && self.pane_rects.iter().all(|&(_, rect)| rect.is_empty());
        LayoutSolve {
            pane_rects: self.pane_rects,
            suppressed_pane_ids: self.suppressed_pane_ids,
            is_all_panes_suppressed,
            stack_headers: self.stack_headers,
        }
    }
}

/// Solve `layout_tree` over `tab_rect` with [`PaneSizing::default`].
///
/// Rects are half-open: `origin` is inclusive, the right and bottom edges
/// are exclusive. At [`PaneSizing::gap_cell_count`] `0` adjacent panes meet without
/// sharing cells.
///
/// When the tree's floors no longer fit, trailing panes are suppressed:
/// solved to zero area and listed in [`LayoutSolve::suppressed_pane_ids`].
/// Suppression is stable: the same panes drop out and return as the rect
/// shrinks and regrows.
#[must_use]
pub fn solve_layout(layout_tree: &LayoutNode, tab_rect: Rect) -> LayoutSolve {
    solve_layout_with_sizing(layout_tree, tab_rect, PaneSizing::default())
}

/// [`solve_layout`] with an explicit [`PaneSizing`]: `pane_sizing.minimum_size`
/// is the content floor of every leaf (the configured pane minimum, floored at
/// [`MIN_PANE_SIZE`] by the caller) and `pane_sizing.gap_cell_count` the blank cells between
/// split children.
#[must_use]
pub fn solve_layout_with_sizing(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_sizing: PaneSizing,
) -> LayoutSolve {
    let mut solve_state = LayoutSolveState::from_pane_sizing(pane_sizing);
    solve_layout_node(layout_tree, tab_rect, &mut solve_state);
    solve_state.into_layout_solve()
}

/// Solve `layout_tree` over `tab_rect` under a layout mode, with an explicit
/// [`PaneSizing`]; see [`solve_layout_with_sizing`] for what `pane_sizing` sets.
///
/// `Tiled` is [`solve_layout_with_sizing`]. `Fullscreen` gives the focused pane the
/// whole tab and zero area to every other pane, changes no tree, and emits
/// no stack headers. A tab smaller than the focused pane's border-inclusive
/// floor suppresses it. A fullscreen mode whose focused pane is not in the
/// tree falls back to the tiled solve.
#[must_use]
pub fn solve_layout_with_mode(
    layout_tree: &LayoutNode,
    layout_mode: LayoutMode,
    tab_rect: Rect,
    pane_sizing: PaneSizing,
) -> LayoutSolve {
    let LayoutMode::Fullscreen { focused_pane_id } = layout_mode else {
        return solve_layout_with_sizing(layout_tree, tab_rect, pane_sizing);
    };
    if !layout_tree.contains_pane(focused_pane_id) {
        return solve_layout_with_sizing(layout_tree, tab_rect, pane_sizing);
    }

    let mut solve_state = LayoutSolveState::from_pane_sizing(pane_sizing);
    // The focused pane is suppressed when the tab is below its
    // border-inclusive floor, the same floor the tiled path uses.
    let is_too_small = !is_leaf_within_minimum(tab_rect, pane_sizing);
    for pane_id in layout_tree.list_leaf_pane_ids() {
        if pane_id != focused_pane_id {
            solve_state
                .pane_rects
                .push((pane_id, Rect::empty_at_origin()));
        } else if is_too_small {
            solve_state
                .pane_rects
                .push((pane_id, Rect::empty_at_origin()));
            solve_state.suppressed_pane_ids.push(pane_id);
        } else {
            solve_state.pane_rects.push((pane_id, tab_rect));
        }
    }
    solve_state.into_layout_solve()
}

/// `true` when every pane in `layout_tree` can be placed inside `layout_rect` at minimum
/// size: [`compute_minimum_size`] of `layout_tree` under `pane_sizing` fits `layout_rect` on
/// both axes.
#[must_use]
pub fn is_layout_within_rect(
    layout_tree: &LayoutNode,
    layout_rect: Rect,
    pane_sizing: PaneSizing,
) -> bool {
    let minimum_size = compute_minimum_size(layout_tree, pane_sizing);
    minimum_size.column_count <= layout_rect.cell_size.column_count
        && minimum_size.row_count <= layout_rect.cell_size.row_count
}

/// `content_minimum_size` plus one cell per side on each axis when
/// `has_borders` is `true`, saturating at `u16::MAX`; `content_minimum_size`
/// unchanged when `false`.
/// A 2 by 1 content minimum with borders is 4 by 3.
fn compute_border_inclusive_minimum(content_minimum_size: Size, has_borders: bool) -> Size {
    if has_borders {
        Size {
            column_count: content_minimum_size.column_count.saturating_add(2),
            row_count: content_minimum_size.row_count.saturating_add(2),
        }
    } else {
        content_minimum_size
    }
}

/// The smallest rectangle this subtree can be solved into.
///
/// A leaf needs `pane_sizing.minimum_size` plus one border cell per side;
/// siblings do not share border cells. A
/// horizontal or vertical split sums its children's floors along the split
/// axis, plus one `pane_sizing.gap_cell_count` between each pair of children, and takes the
/// largest child floor across it; a slot's declared floor (`Min` primary or
/// `min` overlay) raises that child's share of the sum. A stack needs its
/// widest member, one header row per collapsed member, plus the active
/// member's rows, and places no gap. Every sum saturates at `u16::MAX`.
#[must_use]
pub fn compute_minimum_size(layout_node: &LayoutNode, pane_sizing: PaneSizing) -> Size {
    match layout_node {
        LayoutNode::Pane(_) => compute_border_inclusive_minimum(pane_sizing.minimum_size, true),
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                let is_horizontal_split = split.direction == SplitDirection::Horizontal;
                // Floors sum along the split axis; the cross axis takes the
                // largest child minimum.
                let mut split_axis_cell_count: u16 = 0;
                let mut cross_axis_cell_count: u16 = 0;
                for (child_index, child) in split.children.iter().enumerate() {
                    let (axis_minimum, cross_minimum) = split_axis_and_cross_cell_counts(
                        compute_minimum_size(child, pane_sizing),
                        is_horizontal_split,
                    );
                    split_axis_cell_count = split_axis_cell_count
                        .saturating_add(compute_child_floor(split, child_index, axis_minimum));
                    cross_axis_cell_count = cross_axis_cell_count.max(cross_minimum);
                }
                // One gap sits between each pair of children.
                let gap_cell_count = pane_sizing
                    .gap_cell_count
                    .saturating_mul(split.children.len().saturating_sub(1) as u16);
                split_axis_cell_count = split_axis_cell_count.saturating_add(gap_cell_count);
                if is_horizontal_split {
                    Size {
                        column_count: split_axis_cell_count,
                        row_count: cross_axis_cell_count,
                    }
                } else {
                    Size {
                        column_count: cross_axis_cell_count,
                        row_count: split_axis_cell_count,
                    }
                }
            }
            SplitDirection::Stacked => compute_stack_minimum_size(split, pane_sizing),
        },
    }
}

/// The smallest rectangle a stack can be solved into: its widest member by
/// one header row per collapsed member plus the active member's rows. A
/// stack places no gap between its members; an empty stack needs 0 by 0.
pub(crate) fn compute_stack_minimum_size(split: &SplitNode, pane_sizing: PaneSizing) -> Size {
    let active_member_index = split.get_active_child_index();
    let mut maximum_column_count: u16 = 0;
    let mut active_row_count: u16 = 0;
    for (child_index, child) in split.children.iter().enumerate() {
        let child_minimum_size = compute_minimum_size(child, pane_sizing);
        maximum_column_count = maximum_column_count.max(child_minimum_size.column_count);
        if child_index == active_member_index {
            active_row_count = child_minimum_size.row_count;
        }
    }
    let header_row_count = split.children.len().saturating_sub(1) as u16;
    Size {
        column_count: maximum_column_count,
        row_count: header_row_count.saturating_add(active_row_count),
    }
}

/// The floor of the child at `child_index` along the split axis: the larger of
/// that subtree's minimum and the floor its weight declares. A missing
/// child counts as a 0 by 0 subtree.
pub(crate) fn compute_slot_floor(
    split: &SplitNode,
    child_index: usize,
    is_horizontal_split: bool,
    pane_sizing: PaneSizing,
) -> u16 {
    let child_minimum_size = split.children.get(child_index).map_or(
        Size {
            column_count: 0,
            row_count: 0,
        },
        |child| compute_minimum_size(child, pane_sizing),
    );
    compute_child_floor(
        split,
        child_index,
        split_axis_and_cross_cell_counts(child_minimum_size, is_horizontal_split).0,
    )
}

/// The cell count of `rect`: columns × rows. A 40 by 24 rect gives 960.
pub(crate) fn compute_cell_area(rect: Rect) -> u64 {
    u64::from(rect.cell_size.column_count) * u64::from(rect.cell_size.row_count)
}

/// The `cell_size` measures along the split axis and across it: columns then rows
/// for a horizontal split, rows then columns for a vertical one.
fn split_axis_and_cross_cell_counts(cell_size: Size, is_horizontal_split: bool) -> (u16, u16) {
    if is_horizontal_split {
        (cell_size.column_count, cell_size.row_count)
    } else {
        (cell_size.row_count, cell_size.column_count)
    }
}

/// The floor for one child slot along the split axis: the larger of the
/// subtree's own minimum and any floor its weight declares (`Min` primary
/// or `min` overlay). A missing weight declares no floor.
fn compute_child_floor(split: &SplitNode, child_index: usize, subtree_axis_minimum: u16) -> u16 {
    let weight_floor = split.weights.get(child_index).map_or(0, |size_weight| {
        let primary_floor = match size_weight.primary_constraint {
            SizeConstraint::Minimum(minimum_cell_count) => minimum_cell_count,
            _ => 0,
        };
        primary_floor.max(size_weight.minimum_cell_count.unwrap_or(0))
    });
    subtree_axis_minimum.max(weight_floor)
}

/// `true` when `rect` holds one leaf pane: it meets the border-inclusive
/// floor of `pane_sizing.minimum_size` on both axes. A 3 by 3
/// rect holds a leaf at the 2 by 1 default minimum; a 3 by 2 rect does not.
pub(crate) fn is_leaf_within_minimum(rect: Rect, pane_sizing: PaneSizing) -> bool {
    let minimum_size = compute_border_inclusive_minimum(pane_sizing.minimum_size, true);
    rect.cell_size.column_count >= minimum_size.column_count
        && rect.cell_size.row_count >= minimum_size.row_count
}

/// `true` when `pane` shows its own content at `rect`: the rect covers cells
/// and `pane` does not stand on a collapsed stack member's header strip in
/// `stack_headers`.
pub(crate) fn is_content_visible(
    pane_id: PaneId,
    pane_rect: Rect,
    stack_headers: &[StackHeader],
) -> bool {
    !pane_rect.is_empty() && !stack_headers.iter().any(|header| header.pane_id == pane_id)
}

fn solve_layout_node(
    layout_node: &LayoutNode,
    layout_rect: Rect,
    solve_state: &mut LayoutSolveState,
) {
    match layout_node {
        LayoutNode::Pane(pane_id) => {
            // A leaf whose rect is below its border-inclusive floor on
            // either axis is suppressed.
            if is_leaf_within_minimum(layout_rect, solve_state.pane_sizing) {
                solve_state.pane_rects.push((*pane_id, layout_rect));
            } else {
                solve_state
                    .pane_rects
                    .push((*pane_id, Rect::empty_at_origin()));
                solve_state.suppressed_pane_ids.push(*pane_id);
            }
        }
        LayoutNode::Split(split) => match split.direction {
            SplitDirection::Horizontal | SplitDirection::Vertical => {
                solve_directional_split(split, layout_rect, solve_state);
            }
            SplitDirection::Stacked => solve_stacked_split(split, layout_rect, solve_state),
        },
    }
}

/// Zero out a whole subtree and record every leaf as suppressed.
fn suppress_layout_subtree(layout_node: &LayoutNode, solve_state: &mut LayoutSolveState) {
    for pane_id in layout_node.list_leaf_pane_ids() {
        solve_state
            .pane_rects
            .push((pane_id, Rect::empty_at_origin()));
        solve_state.suppressed_pane_ids.push(pane_id);
    }
}

/// Divide `rect` among the split's children along its axis and recurse.
///
/// Children that cannot fit are suppressed before distribution: a child
/// whose cross-axis minimum exceeds the rect is dropped on its own, and
/// once the running sum of axis floors overflows the rect, that child and
/// every child after it drop too. The children that remain always fit at
/// their floor.
fn solve_directional_split(split: &SplitNode, rect: Rect, solve_state: &mut LayoutSolveState) {
    let child_rects = compute_directional_child_rects(split, rect, solve_state.pane_sizing);
    for (child, child_rect) in split.children.iter().zip(child_rects) {
        if child_rect.is_empty() {
            suppress_layout_subtree(child, solve_state);
        } else {
            solve_layout_node(child, child_rect, solve_state);
        }
    }
}

/// The rectangle each child of a directional split receives inside `rect`,
/// in child order. A suppressed child gets a zero rect at its position; a
/// kept child's rect meets the child's floor, and an empty rect always means
/// "suppressed".
///
/// One `pane_sizing.gap_cell_count` sits between each pair of kept children and comes off the
/// axis before any child is sized. A suppressed child reserves no gap; the
/// survivors share those cells.
pub(crate) fn compute_directional_child_rects(
    split: &SplitNode,
    split_rect: Rect,
    pane_sizing: PaneSizing,
) -> Vec<Rect> {
    let is_horizontal_split = split.direction == SplitDirection::Horizontal;
    let (available_cell_count, available_cross_axis_cell_count) =
        split_axis_and_cross_cell_counts(split_rect.cell_size, is_horizontal_split);
    let gap_cell_count = pane_sizing.gap_cell_count;

    // Decide who fits: per-child cross-axis check, then trailing suppression
    // along the split axis. The kept children's weights and floors are
    // collected in the same pass, in child order. A child without a weight
    // takes the default share.
    let mut is_child_kept = vec![false; split.children.len()];
    let mut kept_weights: Vec<SizeWeight> = Vec::with_capacity(split.children.len());
    let mut kept_floors: Vec<u16> = Vec::with_capacity(split.children.len());
    let mut can_keep_children = true;
    let mut claimed_cell_count: u32 = 0;
    for (child_index, child) in split.children.iter().enumerate() {
        let (axis_minimum, cross_minimum) = split_axis_and_cross_cell_counts(
            compute_minimum_size(child, pane_sizing),
            is_horizontal_split,
        );
        let child_floor = compute_child_floor(split, child_index, axis_minimum);
        if cross_minimum > available_cross_axis_cell_count {
            continue;
        }
        // Every kept child after the first is preceded by one gap.
        let leading_gap_cell_count: u32 = if kept_weights.is_empty() {
            0
        } else {
            u32::from(gap_cell_count)
        };
        if can_keep_children
            && claimed_cell_count + leading_gap_cell_count + u32::from(child_floor)
                <= u32::from(available_cell_count)
        {
            is_child_kept[child_index] = true;
            claimed_cell_count += leading_gap_cell_count + u32::from(child_floor);
            kept_weights.push(split.weights.get(child_index).copied().unwrap_or_default());
            kept_floors.push(child_floor);
        } else {
            can_keep_children = false;
        }
    }

    // The gaps between kept children come off the axis before any child is
    // sized. Distribute over the kept children only, then lay rects in child
    // order; suppressed children sit at their position with zero area.
    let gap_cell_total = gap_cell_count.saturating_mul(kept_weights.len().saturating_sub(1) as u16);
    let available_for_children = available_cell_count.saturating_sub(gap_cell_total);
    let child_cell_counts =
        distribute_axis_cells(&kept_weights, &kept_floors, available_for_children);

    let mut child_rects = Vec::with_capacity(split.children.len());
    let mut axis_offset: u16 = 0;
    let mut kept_child_index = 0;
    for &is_kept in &is_child_kept {
        if !is_kept {
            child_rects.push(Rect::empty_at_origin());
            continue;
        }
        let child_cell_count = child_cell_counts[kept_child_index];
        kept_child_index += 1;
        let child_rect = if is_horizontal_split {
            Rect::from_origin_and_size(
                Point {
                    column: split_rect.origin.column.saturating_add(axis_offset),
                    row: split_rect.origin.row,
                },
                Size {
                    column_count: child_cell_count,
                    row_count: split_rect.cell_size.row_count,
                },
            )
        } else {
            Rect::from_origin_and_size(
                Point {
                    column: split_rect.origin.column,
                    row: split_rect.origin.row.saturating_add(axis_offset),
                },
                Size {
                    column_count: split_rect.cell_size.column_count,
                    row_count: child_cell_count,
                },
            )
        };
        child_rects.push(child_rect);
        axis_offset = axis_offset
            .saturating_add(child_cell_count)
            .saturating_add(gap_cell_count);
    }
    child_rects
}

/// The rectangle each child of a stacked split receives inside `rect`, in
/// child order. Members stay in layout order: each collapsed member takes a
/// one-row header strip spanning the stack's width, and the active member
/// takes the band left over between them.
///
/// Every rect is `Rect::empty_at_origin()` when `rect` cannot hold every header plus the
/// active member at minimum size, or is narrower than the widest member needs
/// ([`compute_stack_minimum_size`]). A stack with no children yields no rects.
pub(crate) fn compute_stacked_child_rects(
    split: &SplitNode,
    split_rect: Rect,
    pane_sizing: PaneSizing,
) -> Vec<Rect> {
    let member_count = split.children.len();
    if member_count == 0 {
        return Vec::new();
    }
    let minimum_size = compute_stack_minimum_size(split, pane_sizing);
    if split_rect.cell_size.row_count < minimum_size.row_count
        || split_rect.cell_size.column_count < minimum_size.column_count
    {
        return vec![Rect::empty_at_origin(); member_count];
    }

    let active_member_index = split.get_active_child_index();
    let header_row_count = (member_count - 1) as u16;
    let active_row_count = split_rect.cell_size.row_count - header_row_count;
    let mut member_rects = Vec::with_capacity(member_count);
    let mut row_offset = split_rect.origin.row;
    for member_index in 0..member_count {
        let member_row_count = if member_index == active_member_index {
            active_row_count
        } else {
            1
        };
        member_rects.push(Rect::from_origin_and_size(
            Point {
                column: split_rect.origin.column,
                row: row_offset,
            },
            Size {
                column_count: split_rect.cell_size.column_count,
                row_count: member_row_count,
            },
        ));
        row_offset = row_offset.saturating_add(member_row_count);
    }
    member_rects
}

/// Stacked children share the rect: the active child expands into whatever
/// remains after every collapsed member takes a one-row header strip.
///
/// A collapsed member's pane rect *is* its header strip; the matching
/// [`StackHeader`] entry carries the indicator metadata.
///
/// If [`compute_stacked_child_rects`] gives the stack no room, it suppresses as one
/// unit: no headers, every member zero-area.
fn solve_stacked_split(split: &SplitNode, rect: Rect, solve_state: &mut LayoutSolveState) {
    let member_count = split.children.len();
    let active_member_index = split.get_active_child_index();
    let member_rects = compute_stacked_child_rects(split, rect, solve_state.pane_sizing);
    for (member_index, (child, child_rect)) in split.children.iter().zip(member_rects).enumerate() {
        if child_rect.is_empty() {
            suppress_layout_subtree(child, solve_state);
        } else if member_index == active_member_index {
            solve_layout_node(child, child_rect, solve_state);
        } else {
            emit_stack_header(child, child_rect, member_index, member_count, solve_state);
        }
    }
}

/// Place one collapsed stack member on its header strip.
///
/// A member that is a subtree puts its first leaf on the strip; its other
/// leaves solve to zero area and are not listed as suppressed. A member with
/// no leaf gets no header and no pane entry.
fn emit_stack_header(
    child: &LayoutNode,
    header_rect: Rect,
    member_index: usize,
    member_count: usize,
    solve_state: &mut LayoutSolveState,
) {
    let pane_ids = child.list_leaf_pane_ids();
    let Some((&first_pane_id, remaining_pane_ids)) = pane_ids.split_first() else {
        return;
    };
    solve_state.pane_rects.push((first_pane_id, header_rect));
    solve_state.stack_headers.push(StackHeader {
        pane_id: first_pane_id,
        header_rect,
        member_index,
        member_count,
    });
    for &pane_id in remaining_pane_ids {
        solve_state
            .pane_rects
            .push((pane_id, Rect::empty_at_origin()));
    }
}

/// Split `available` cells among children according to their weights.
///
/// The returned sizes sum to exactly `available`. When the floors fit,
/// every child also ends at or above its floor.
fn distribute_axis_cells(
    weights: &[SizeWeight],
    floors: &[u16],
    available_cell_count: u16,
) -> Vec<u16> {
    let mut child_cell_counts = vec![0u16; weights.len()];
    let mut remaining_cell_count = available_cell_count;

    // Fixed sizes claim cells first, in child order, never more than remain.
    for (child_index, weight) in weights.iter().enumerate() {
        if let SizeConstraint::Fixed(fixed_cell_count) = weight.primary_constraint {
            child_cell_counts[child_index] = fixed_cell_count.min(remaining_cell_count);
            remaining_cell_count -= child_cell_counts[child_index];
        }
    }

    // Percentages are shares of the whole axis, floored to cells; a value
    // above 100 counts as 100.
    for (child_index, weight) in weights.iter().enumerate() {
        if let SizeConstraint::Percent(percent_value) = weight.primary_constraint {
            let requested_cell_count =
                (u32::from(available_cell_count) * u32::from(percent_value.min(100)) / 100) as u16;
            child_cell_counts[child_index] = requested_cell_count.min(remaining_cell_count);
            remaining_cell_count -= child_cell_counts[child_index];
        }
    }

    // Flexible children share the remainder by weight. `Min` and `Preferred`
    // flex with weight 1; their floor and target are overlays on a share.
    let flexible_child_weight_pairs: Vec<(usize, u64)> = weights
        .iter()
        .enumerate()
        .filter_map(
            |(child_index, size_weight)| match size_weight.primary_constraint {
                SizeConstraint::Flex(flex_weight) => Some((child_index, u64::from(flex_weight))),
                SizeConstraint::Minimum(_) | SizeConstraint::Preferred(_) => Some((child_index, 1)),
                SizeConstraint::Fixed(_) | SizeConstraint::Percent(_) => None,
            },
        )
        .collect();
    // A zero total weight gives every share `0`; the leftover pass then adds
    // one cell to each trailing flexible child, up to the pool, and
    // `repair_cell_count_sum` hands the rest to the last child.
    let total_flexible_weight: u64 = flexible_child_weight_pairs
        .iter()
        .map(|&(_, flex_weight)| flex_weight)
        .sum();
    if !flexible_child_weight_pairs.is_empty() {
        let flexible_cell_pool = u64::from(remaining_cell_count);
        let mut assigned_cell_count: u64 = 0;
        for &(child_index, flex_weight) in &flexible_child_weight_pairs {
            let share_cell_count = (flexible_cell_pool * flex_weight)
                .checked_div(total_flexible_weight)
                .unwrap_or(0) as u16;
            child_cell_counts[child_index] = share_cell_count;
            assigned_cell_count += u64::from(share_cell_count);
        }
        // Leftover cells from flooring go to the trailing flexible children,
        // one each: a 101/2 split is 50 then 51.
        let leftover_cell_count = (flexible_cell_pool - assigned_cell_count) as usize;
        for &(child_index, _) in flexible_child_weight_pairs
            .iter()
            .rev()
            .take(leftover_cell_count)
        {
            child_cell_counts[child_index] += 1;
        }
    }

    // User resizes: exact cell offsets on top of the distribution, each
    // result clamped to `0..=available`.
    for (child_index, weight) in weights.iter().enumerate() {
        let adjusted_cell_count =
            i64::from(child_cell_counts[child_index]) + i64::from(weight.resize_delta);
        child_cell_counts[child_index] =
            adjusted_cell_count.clamp(0, i64::from(available_cell_count)) as u16;
    }

    repair_cell_count_sum(&mut child_cell_counts, available_cell_count);
    apply_preferred_cell_counts(&mut child_cell_counts, weights, floors);
    clamp_cell_counts_to_floors(
        &mut child_cell_counts,
        weights,
        floors,
        available_cell_count,
    );
    child_cell_counts
}

/// `true` when this weight may give up or take cells during adjustment.
/// `Fixed` and `Percent` children give cells only in
/// [`clamp_cell_counts_to_floors`].
fn is_flexible(weight: &SizeWeight) -> bool {
    matches!(
        weight.primary_constraint,
        SizeConstraint::Flex(_) | SizeConstraint::Minimum(_) | SizeConstraint::Preferred(_)
    )
}

/// The target a child aims for when slack allows: the `preferred` overlay
/// when set, else a `Preferred` primary's cells, else `None`.
fn get_preferred_cell_count(weight: &SizeWeight) -> Option<u16> {
    weight
        .preferred_cell_count
        .or(match weight.primary_constraint {
            SizeConstraint::Preferred(preferred_cell_count) => Some(preferred_cell_count),
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
fn apply_preferred_cell_counts(
    child_cell_counts: &mut [u16],
    weights: &[SizeWeight],
    floors: &[u16],
) {
    for child_index in 0..weights.len() {
        let Some(target_cell_count) = get_preferred_cell_count(&weights[child_index]) else {
            continue;
        };
        let current_cell_count = child_cell_counts[child_index];
        if current_cell_count > target_cell_count {
            // Surplus above the larger of the target and the floor flows to
            // the trailing-most flexible sibling; without one the surplus
            // stays where it is.
            let floor_cell_count = floors[child_index];
            let surplus_cell_count =
                current_cell_count.saturating_sub(target_cell_count.max(floor_cell_count));
            let receiver_child_index = (0..weights.len()).rev().find(|&candidate_child_index| {
                candidate_child_index != child_index && is_flexible(&weights[candidate_child_index])
            });
            if let Some(receiver_child_index) = receiver_child_index {
                child_cell_counts[child_index] -= surplus_cell_count;
                child_cell_counts[receiver_child_index] =
                    child_cell_counts[receiver_child_index].saturating_add(surplus_cell_count);
            }
        } else if current_cell_count < target_cell_count {
            let needed_cell_count = target_cell_count - current_cell_count;
            let taken_cell_count = take_donor_cell_count(
                child_cell_counts,
                weights,
                floors,
                needed_cell_count,
                child_index,
                DonorPool::FlexibleOnly,
            );
            child_cell_counts[child_index] =
                child_cell_counts[child_index].saturating_add(taken_cell_count);
        }
    }
}

/// Raise every child to its floor, funding the deficit from siblings above
/// theirs. Does nothing when the floors do not fit in `available_cell_count`.
fn clamp_cell_counts_to_floors(
    child_cell_counts: &mut [u16],
    weights: &[SizeWeight],
    floors: &[u16],
    available_cell_count: u16,
) {
    let total_floor_cell_count: u64 = floors.iter().map(|&cell_count| u64::from(cell_count)).sum();
    if total_floor_cell_count > u64::from(available_cell_count) {
        return;
    }
    for child_index in 0..child_cell_counts.len() {
        let floor_cell_count = floors[child_index];
        if child_cell_counts[child_index] < floor_cell_count {
            let needed_cell_count = floor_cell_count - child_cell_counts[child_index];
            let taken_cell_count = take_donor_cell_count(
                child_cell_counts,
                weights,
                floors,
                needed_cell_count,
                child_index,
                DonorPool::AllChildren,
            );
            child_cell_counts[child_index] += taken_cell_count;
        }
    }
}

/// Who may give up cells in [`take_donor_cell_count`]: `FlexibleOnly` limits donors to
/// flexible children, `AllChildren` also taps `Fixed` and `Percent` ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DonorPool {
    FlexibleOnly,
    AllChildren,
}

/// Take up to `needed_cell_count` cells from siblings other than
/// `skipped_child_index`, trailing-first,
/// leaving every donor at or above its floor. Flexible donors give first;
/// `Fixed`/`Percent` children are tapped only when `donor_pool` allows it and the
/// flexible donors are exhausted. Returns the cells taken, at most
/// `needed_cell_count`.
fn take_donor_cell_count(
    child_cell_counts: &mut [u16],
    weights: &[SizeWeight],
    floors: &[u16],
    needed_cell_count: u16,
    skipped_child_index: usize,
    donor_pool: DonorPool,
) -> u16 {
    let mut taken_cell_count: u16 = 0;
    for is_flexible_donor_pass in [true, false] {
        if !is_flexible_donor_pass && donor_pool == DonorPool::FlexibleOnly {
            break;
        }
        for child_index in (0..child_cell_counts.len()).rev() {
            if taken_cell_count == needed_cell_count {
                return taken_cell_count;
            }
            if child_index == skipped_child_index
                || is_flexible(&weights[child_index]) != is_flexible_donor_pass
            {
                continue;
            }
            let floor_cell_count = floors[child_index];
            let spare_cell_count = child_cell_counts[child_index].saturating_sub(floor_cell_count);
            let given_cell_count = spare_cell_count.min(needed_cell_count - taken_cell_count);
            child_cell_counts[child_index] -= given_cell_count;
            taken_cell_count += given_cell_count;
        }
    }
    taken_cell_count
}

/// Force `child_cell_counts` to sum to exactly `available_cell_count`, adjusting from the end: a
/// shortfall goes to the last child; an excess is trimmed from the trailing
/// children toward zero, leaving the leading ones untouched.
fn repair_cell_count_sum(child_cell_counts: &mut [u16], available_cell_count: u16) {
    let total_cell_count: u64 = child_cell_counts
        .iter()
        .map(|&cell_count| u64::from(cell_count))
        .sum();
    let available_cell_count = u64::from(available_cell_count);

    if total_cell_count < available_cell_count {
        if let Some(last_cell_count) = child_cell_counts.last_mut() {
            *last_cell_count += (available_cell_count - total_cell_count) as u16;
        }
    } else if total_cell_count > available_cell_count {
        let mut excess_cell_count = total_cell_count - available_cell_count;
        for cell_count in child_cell_counts.iter_mut().rev() {
            let trimmed_cell_count = excess_cell_count.min(u64::from(*cell_count));
            *cell_count -= trimmed_cell_count as u16;
            excess_cell_count -= trimmed_cell_count;
            if excess_cell_count == 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests;
