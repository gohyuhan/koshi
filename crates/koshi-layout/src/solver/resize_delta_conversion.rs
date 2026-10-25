//! Rewrites the resize deltas of a layout tree laid out by a solve that
//! applies each `resize_delta` before the `preferred` targets. After the
//! rewrite, [`super::solve_layout_with_sizing`], which applies each
//! `resize_delta` after the targets, gives every pane the size the earlier
//! solve gave at the rect the tree is rewritten at.

use koshi_core::geometry::{Rect, SplitDirection};

use super::{
    apply_preferred_cell_counts, apply_resize_deltas, clamp_cell_counts_to_floors,
    compute_directional_child_rects, compute_share_cell_counts, compute_stacked_child_rects,
    distribute_axis_cells, list_kept_children, repair_cell_count_sum, PaneSizing,
};
use crate::size::SizeWeight;
use crate::tree::{LayoutNode, SplitNode};

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
pub fn convert_resize_deltas_to_apply_after_preferred(
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
                    convert_resize_deltas_to_apply_after_preferred(child, child_rect, pane_sizing);
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
                convert_resize_deltas_to_apply_after_preferred(
                    member,
                    shown_member_rect,
                    pane_sizing,
                );
            }
        }
    }
}

/// Rewrite the `resize_delta` of each child `split` keeps inside `split_rect`,
/// as [`convert_resize_deltas_to_apply_after_preferred`] describes for one
/// split.
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

#[cfg(test)]
mod tests;
