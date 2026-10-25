//! Rewrites the resize deltas of a layout tree laid out by a solve that
//! applies each `resize_delta` before the `preferred` targets, for
//! [`super::solve_layout_with_sizing`], which applies each `resize_delta`
//! after the targets.

use std::collections::HashMap;

use koshi_core::geometry::{Rect, Size, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_core::process::PtySize;

use super::{
    apply_preferred_cell_counts, apply_resize_deltas, clamp_cell_counts_to_floors,
    compute_directional_child_rects, compute_share_cell_counts, compute_stacked_child_rects,
    distribute_axis_cells, get_preferred_cell_count, is_flexible, list_kept_children,
    repair_cell_count_sum, solve_layout_with_sizing, split_axis_and_cross_cell_counts, PaneSizing,
};
use crate::content::list_content_rects;
use crate::size::SizeWeight;
use crate::tree::{LayoutNode, SplitNode};

/// Rewrite the `resize_delta` values in `layout_tree`, a tab's tree that the
/// earlier solve laid out: the solve that applies each `resize_delta` before
/// the `preferred` targets. `pty_size_by_pane_id` holds the terminal size each
/// pane was last drawn at.
///
/// 1. The tab rect is measured from those sizes. A pane is its carried size
///    plus its one-cell border on each side. A horizontal or vertical split is
///    its children side by side along its axis, with the gap between each
///    pair. A stack is its shown member plus one header row for every other
///    member.
/// 2. Each split is rewritten at the rect the earlier solve gives it inside
///    the tab rect: each kept child's `resize_delta` becomes the size the
///    earlier solve gave minus the size its share and its `preferred` target
///    give.
/// 3. The rewritten tree is solved at the tab rect. When every pane the
///    measurement counted shows content of exactly its carried size, the
///    rewrite is kept: the tab solves to the sizes it was last drawn at.
///
/// Otherwise, and when the measurement finds no rect, the tree keeps its
/// values except one fold: in each horizontal and vertical split, the
/// `resize_delta` of each child with a `preferred` target moves onto the
/// trailing-most flexible sibling. Every split then solves to the sizes the
/// earlier solve gives at any rect where no floor and no split edge limits a
/// size.
///
/// 80 by 24 cells, a `preferred 20` pane beside a flexible one, resize
/// deltas `+10` and `-10`, carried sizes 18 by 22 and 58 by 22: the measured
/// rect is 80 by 24, the earlier solve gave 20 and 60 there, and both deltas
/// become `0`.
pub fn convert_resize_deltas_to_apply_after_preferred(
    layout_tree: &mut LayoutNode,
    pty_size_by_pane_id: &HashMap<PaneId, PtySize>,
    pane_sizing: PaneSizing,
) {
    let mut measured_pane_count: usize = 0;
    if let Some(tab_size) = measure_layout_size(
        layout_tree,
        pty_size_by_pane_id,
        pane_sizing.gap_cell_count,
        &mut measured_pane_count,
    ) {
        let tab_rect = Rect::from_size_at_origin(tab_size);
        let mut converted_layout_tree = layout_tree.clone();
        convert_resize_deltas_at_rect(&mut converted_layout_tree, tab_rect, pane_sizing);
        if does_solve_give_every_pane_its_carried_size(
            &converted_layout_tree,
            tab_rect,
            pty_size_by_pane_id,
            measured_pane_count,
            pane_sizing,
        ) {
            *layout_tree = converted_layout_tree;
            return;
        }
    }
    fold_preferred_resize_deltas(layout_tree);
}

/// The size `layout_node` was last drawn at, measured from the carried size
/// of each pane it shows: a pane is its carried size plus its one-cell border
/// on each side; a horizontal or vertical split is its children placed side by
/// side along its axis with `gap_cell_count` cells between each pair, and its
/// last child's size across it; a stack is its shown member plus one header
/// row for every other member. A collapsed stack member is not measured. Adds
/// one to `measured_pane_count` for each pane measured.
///
/// `None` when a measured pane has no carried size, or a size passes
/// `u16::MAX`. A split with no child measures `0` by `0`.
///
/// A vertical split of carried sizes 9 by 1 and 9 by 2, beside a carried 9 by
/// 5 → 22 by 7.
fn measure_layout_size(
    layout_node: &LayoutNode,
    pty_size_by_pane_id: &HashMap<PaneId, PtySize>,
    gap_cell_count: u16,
    measured_pane_count: &mut usize,
) -> Option<Size> {
    let split = match layout_node {
        LayoutNode::Pane(pane_id) => {
            let pty_size = pty_size_by_pane_id.get(pane_id)?;
            *measured_pane_count += 1;
            return Some(Size {
                column_count: pty_size.column_count.checked_add(2)?,
                row_count: pty_size.row_count.checked_add(2)?,
            });
        }
        LayoutNode::Split(split) => split,
    };
    if split.direction == SplitDirection::Stacked {
        let shown_member = split.children.get(split.get_active_child_index())?;
        let shown_member_size = measure_layout_size(
            shown_member,
            pty_size_by_pane_id,
            gap_cell_count,
            measured_pane_count,
        )?;
        let header_row_count = u16::try_from(split.children.len() - 1).ok()?;
        return Some(Size {
            column_count: shown_member_size.column_count,
            row_count: shown_member_size.row_count.checked_add(header_row_count)?,
        });
    }
    let is_horizontal_split = split.direction == SplitDirection::Horizontal;
    let mut axis_cell_count: u16 = 0;
    let mut cross_cell_count: u16 = 0;
    for (child_index, child) in split.children.iter().enumerate() {
        let child_size = measure_layout_size(
            child,
            pty_size_by_pane_id,
            gap_cell_count,
            measured_pane_count,
        )?;
        let (child_axis_cell_count, child_cross_cell_count) = split_axis_and_cross_cell_counts(
            child_size.column_count,
            child_size.row_count,
            is_horizontal_split,
        );
        let leading_gap_cell_count = if child_index == 0 { 0 } else { gap_cell_count };
        axis_cell_count = axis_cell_count
            .checked_add(leading_gap_cell_count)?
            .checked_add(child_axis_cell_count)?;
        cross_cell_count = child_cross_cell_count;
    }
    let (column_count, row_count) =
        split_axis_and_cross_cell_counts(axis_cell_count, cross_cell_count, is_horizontal_split);
    Some(Size {
        column_count,
        row_count,
    })
}

/// `true` when the solve of `layout_tree` over `tab_rect` with `pane_sizing`
/// shows content for exactly `measured_pane_count` panes, and each of them
/// at exactly its size in `pty_size_by_pane_id`.
fn does_solve_give_every_pane_its_carried_size(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pty_size_by_pane_id: &HashMap<PaneId, PtySize>,
    measured_pane_count: usize,
    pane_sizing: PaneSizing,
) -> bool {
    let layout_solve = solve_layout_with_sizing(layout_tree, tab_rect, pane_sizing);
    let mut shown_pane_count: usize = 0;
    for (pane_id, content_rect) in list_content_rects(&layout_solve) {
        let Some(content_rect) = content_rect else {
            continue;
        };
        shown_pane_count += 1;
        let carried_size = pty_size_by_pane_id.get(&pane_id).map(|pty_size| Size {
            column_count: pty_size.column_count,
            row_count: pty_size.row_count,
        });
        if carried_size != Some(content_rect.size) {
            return false;
        }
    }
    shown_pane_count == measured_pane_count
}

/// Rewrite the `resize_delta` values in `layout_tree`. After the rewrite, the
/// solve over `tab_rect` with `pane_sizing` gives each pane the size it got
/// from the solve that applied each `resize_delta` before the `preferred`
/// targets.
///
/// Each horizontal or vertical split solves its kept children both ways. When
/// the two solves give the same sizes, the split keeps its values. When they
/// differ, each kept child's `resize_delta` becomes the size the earlier solve
/// gave minus the size its share and its `preferred` target give, and a
/// missing weight is added with the default share first. A child that solves
/// to no cells keeps every value in its subtree. Every member of a stack is
/// rewritten at the size of the band the shown member holds. A pane or a
/// split with no kept child changes nothing.
///
/// 80 columns, a `preferred 20` pane beside a flexible one, resize deltas
/// `+10` and `-10`: the earlier solve gave 20 and 60, the share and the
/// target give 20 and 60, and both deltas become `0`.
fn convert_resize_deltas_at_rect(
    layout_tree: &mut LayoutNode,
    tab_rect: Rect,
    pane_sizing: PaneSizing,
) {
    let LayoutNode::Split(split) = layout_tree else {
        return;
    };
    match split.direction {
        SplitDirection::Horizontal | SplitDirection::Vertical => {
            convert_split_resize_deltas(split, tab_rect, pane_sizing);
            let child_rects = compute_directional_child_rects(split, tab_rect, pane_sizing);
            for (child, child_rect) in split.children.iter_mut().zip(child_rects) {
                if !child_rect.is_empty() {
                    convert_resize_deltas_at_rect(child, child_rect, pane_sizing);
                }
            }
        }
        SplitDirection::Stacked => {
            let member_rects = compute_stacked_child_rects(split, tab_rect, pane_sizing);
            let Some(&shown_member_rect) = member_rects.get(split.get_active_child_index()) else {
                return;
            };
            if shown_member_rect.is_empty() {
                return;
            }
            for member in &mut split.children {
                convert_resize_deltas_at_rect(member, shown_member_rect, pane_sizing);
            }
        }
    }
}

/// Rewrite the `resize_delta` of each child `split` keeps inside `split_rect`,
/// as [`convert_resize_deltas_at_rect`] describes for one split.
fn convert_split_resize_deltas(split: &mut SplitNode, split_rect: Rect, pane_sizing: PaneSizing) {
    let kept_children = list_kept_children(split, split_rect, pane_sizing);
    let kept_weights = &kept_children.kept_weights;
    let kept_floors = &kept_children.kept_floors;
    let shared_cell_count = kept_children.shared_cell_count;

    let mut earlier_cell_counts = compute_share_cell_counts(kept_weights, shared_cell_count);
    apply_resize_deltas(&mut earlier_cell_counts, kept_weights, shared_cell_count);
    apply_preferred_cell_counts(&mut earlier_cell_counts, kept_weights, kept_floors);
    clamp_cell_counts_to_floors(
        &mut earlier_cell_counts,
        kept_weights,
        kept_floors,
        shared_cell_count,
    );
    if earlier_cell_counts == distribute_axis_cells(kept_weights, kept_floors, shared_cell_count) {
        return;
    }

    let mut target_cell_counts = compute_share_cell_counts(kept_weights, shared_cell_count);
    repair_cell_count_sum(&mut target_cell_counts, shared_cell_count);
    apply_preferred_cell_counts(&mut target_cell_counts, kept_weights, kept_floors);

    if split.weights.len() < split.children.len() {
        split
            .weights
            .resize(split.children.len(), SizeWeight::default());
    }
    let kept_child_indices = kept_children
        .is_child_kept
        .iter()
        .enumerate()
        .filter_map(|(child_index, &is_kept)| is_kept.then_some(child_index));
    for ((child_index, earlier_cell_count), target_cell_count) in kept_child_indices
        .zip(earlier_cell_counts)
        .zip(target_cell_counts)
    {
        split.weights[child_index].resize_delta =
            i32::from(earlier_cell_count) - i32::from(target_cell_count);
    }
}

/// Move the `resize_delta` of each child with a `preferred` target onto the
/// sibling that takes that child's cells, in every horizontal and vertical
/// split of `layout_tree`, stack members included. A stacked split's own
/// weights keep their values.
fn fold_preferred_resize_deltas(layout_tree: &mut LayoutNode) {
    let LayoutNode::Split(split) = layout_tree else {
        return;
    };
    if split.direction != SplitDirection::Stacked {
        fold_split_preferred_resize_deltas(split);
    }
    for child in &mut split.children {
        fold_preferred_resize_deltas(child);
    }
}

/// Move the `resize_delta` of each child of `split` with a `preferred` target,
/// in child order, onto its receiver: the trailing-most flexible sibling, the
/// child that gains a surplus above the target and gives a deficit below it
/// first. The child's own `resize_delta` becomes `0`. A child with no
/// flexible sibling keeps its `resize_delta`. A missing weight is added with
/// the default share before a `resize_delta` is written to it.
///
/// `Preferred(20)`, `Flex(1)` with resize deltas `+10` and `-10` → `0` and
/// `0`. `Flex(1)`, `Preferred(20)`, `Flex(1)` with `+5`, `+3` and `-8` → `+5`,
/// `0` and `-5`.
fn fold_split_preferred_resize_deltas(split: &mut SplitNode) {
    for child_index in 0..split.children.len() {
        let Some(child_weight) = split.weights.get(child_index) else {
            continue;
        };
        if get_preferred_cell_count(child_weight).is_none() {
            continue;
        }
        let receiver_child_index = (0..split.children.len()).rev().find(|&sibling_index| {
            let sibling_weight = split
                .weights
                .get(sibling_index)
                .copied()
                .unwrap_or_default();
            sibling_index != child_index && is_flexible(&sibling_weight)
        });
        let Some(receiver_child_index) = receiver_child_index else {
            continue;
        };
        if split.weights.len() <= receiver_child_index {
            split
                .weights
                .resize(split.children.len(), SizeWeight::default());
        }
        let folded_resize_delta = std::mem::take(&mut split.weights[child_index].resize_delta);
        let receiver_weight = &mut split.weights[receiver_child_index];
        receiver_weight.resize_delta = receiver_weight
            .resize_delta
            .saturating_add(folded_resize_delta);
    }
}

#[cfg(test)]
mod tests;
