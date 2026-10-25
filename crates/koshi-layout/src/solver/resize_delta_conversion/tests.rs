//! Tests for the rewrite of resize deltas laid out by the solve that applies
//! each `resize_delta` before the `preferred` targets.

use koshi_core::geometry::{Rect, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_test_support::fixtures::build_cell_rect;
use proptest::prelude::*;

use super::*;
use crate::solver::tests::build_size_weight_strategy;
use crate::solver::{
    apply_preferred_cell_counts, apply_resize_deltas, clamp_cell_counts_to_floors,
    compute_share_cell_counts, solve_layout_with_sizing, MIN_PANE_SIZE,
};

/// The default content floor and no gap.
const DEFAULT_PANE_SIZING: PaneSizing = PaneSizing {
    minimum_size: MIN_PANE_SIZE,
    gap_cell_count: 0,
};

/// A split of one leaf per weight, in `direction`.
fn build_leaf_split(
    direction: SplitDirection,
    pane_ids: &[PaneId],
    weights: Vec<SizeWeight>,
) -> SplitNode {
    let mut split = SplitNode::with_equal_weights(
        direction,
        pane_ids
            .iter()
            .map(|&pane_id| LayoutNode::Pane(pane_id))
            .collect(),
    );
    split.weights = weights;
    split
}

/// A weight with the default share, the `preferred_cell_count` overlay
/// `preferred_cell_count` and the `resize_delta` `resize_delta`.
fn build_preferred_weight(preferred_cell_count: u16, resize_delta: i32) -> SizeWeight {
    SizeWeight {
        preferred_cell_count: Some(preferred_cell_count),
        resize_delta,
        ..SizeWeight::default()
    }
}

/// A weight with the default share and the `resize_delta` `resize_delta`.
fn build_offset_weight(resize_delta: i32) -> SizeWeight {
    SizeWeight {
        resize_delta,
        ..SizeWeight::default()
    }
}

/// The column count each of `pane_ids` solves to in `layout_tree` over `layout_area`.
fn list_column_counts(
    layout_tree: &LayoutNode,
    layout_area: Rect,
    pane_ids: &[PaneId],
) -> Vec<u16> {
    let layout_solve = solve_layout_with_sizing(layout_tree, layout_area, DEFAULT_PANE_SIZING);
    pane_ids
        .iter()
        .map(|&pane_id| {
            layout_solve
                .find_pane_rect(pane_id)
                .expect("every pane is solved")
                .size
                .column_count
        })
        .collect()
}

/// Each `resize_delta` of the split `layout_tree` holds at its root.
fn list_root_resize_deltas(layout_tree: &LayoutNode) -> Vec<i32> {
    let LayoutNode::Split(split) = layout_tree else {
        panic!("the tree is a split");
    };
    split
        .weights
        .iter()
        .map(|size_weight| size_weight.resize_delta)
        .collect()
}

/// The sizes the solve that applies each `resize_delta` before the
/// `preferred` targets gives the kept children `weights` and `floors` describe
/// over `shared_cell_count` cells.
fn compute_earlier_cell_counts(
    weights: &[SizeWeight],
    floors: &[u16],
    shared_cell_count: u16,
) -> Vec<u16> {
    let mut earlier_cell_counts = compute_share_cell_counts(weights, shared_cell_count);
    apply_resize_deltas(&mut earlier_cell_counts, weights, shared_cell_count);
    apply_preferred_cell_counts(&mut earlier_cell_counts, weights, floors);
    clamp_cell_counts_to_floors(&mut earlier_cell_counts, weights, floors, shared_cell_count);
    earlier_cell_counts
}

#[test]
fn a_split_whose_two_solves_agree_keeps_its_resize_deltas() {
    // No pane aims for a size, so both solves give 76 and 4 over 80 columns:
    // the `+60` and `-60` clamp to 80 and 0, and the floor lifts the right
    // pane to 4. Rewritten, the deltas would read `+36` and `-36`.
    let pane_ids = [PaneId::new(), PaneId::new()];
    let layout_tree = LayoutNode::Split(build_leaf_split(
        SplitDirection::Horizontal,
        &pane_ids,
        vec![build_offset_weight(60), build_offset_weight(-60)],
    ));
    let mut converted_layout_tree = layout_tree.clone();

    convert_resize_deltas_to_apply_after_preferred(
        &mut converted_layout_tree,
        build_cell_rect(0, 0, 80, 24),
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(converted_layout_tree, layout_tree);
}

#[test]
fn a_pane_held_at_its_preferred_size_keeps_that_size() {
    // 80 columns share as 40 and 40. The earlier solve applied the deltas
    // first, then cut the left pane to its preferred 20: 20 and 60 either way.
    let pane_ids = [PaneId::new(), PaneId::new()];
    for resize_delta in [10, -10] {
        let mut layout_tree = LayoutNode::Split(build_leaf_split(
            SplitDirection::Horizontal,
            &pane_ids,
            vec![
                build_preferred_weight(20, resize_delta),
                build_offset_weight(-resize_delta),
            ],
        ));
        let layout_area = build_cell_rect(0, 0, 80, 24);

        convert_resize_deltas_to_apply_after_preferred(
            &mut layout_tree,
            layout_area,
            DEFAULT_PANE_SIZING,
        );

        assert_eq!(list_root_resize_deltas(&layout_tree), [0, 0]);
        assert_eq!(
            list_column_counts(&layout_tree, layout_area, &pane_ids),
            [20, 60]
        );
    }
}

#[test]
fn three_panes_keep_the_sizes_the_earlier_solve_gave() {
    // 90 columns in three equal shares, the middle pane aiming for 10, and
    // resize deltas `+60`, `-60`, `0`: the earlier solve gave 80, 6 and 4.
    // The share and the target give 30, 10 and 50.
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut layout_tree = LayoutNode::Split(build_leaf_split(
        SplitDirection::Horizontal,
        &pane_ids,
        vec![
            build_offset_weight(60),
            build_preferred_weight(10, -60),
            build_offset_weight(0),
        ],
    ));
    let layout_area = build_cell_rect(0, 0, 90, 24);

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        layout_area,
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(list_root_resize_deltas(&layout_tree), [50, -4, -46]);
    assert_eq!(
        list_column_counts(&layout_tree, layout_area, &pane_ids),
        [80, 6, 4]
    );
}

#[test]
fn a_nested_split_is_rewritten_at_the_rect_its_parent_gives() {
    // A top pane over a pair that spans all 80 columns. The pair's left pane
    // aims for 20, and the earlier solve gave the pair 20 and 60.
    let top_pane_id = PaneId::new();
    let pair_pane_ids = [PaneId::new(), PaneId::new()];
    let pair_split = build_leaf_split(
        SplitDirection::Horizontal,
        &pair_pane_ids,
        vec![build_preferred_weight(20, 10), build_offset_weight(-10)],
    );
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![LayoutNode::Pane(top_pane_id), LayoutNode::Split(pair_split)],
    ));
    let mut converted_layout_tree = layout_tree.clone();
    let layout_area = build_cell_rect(0, 0, 80, 24);

    convert_resize_deltas_to_apply_after_preferred(
        &mut converted_layout_tree,
        layout_area,
        DEFAULT_PANE_SIZING,
    );

    let LayoutNode::Split(converted_root_split) = &converted_layout_tree else {
        panic!("the tree is a split");
    };
    assert_eq!(list_root_resize_deltas(&converted_layout_tree), [0, 0]);
    assert_eq!(
        list_root_resize_deltas(&converted_root_split.children[1]),
        [0, 0]
    );
    assert_eq!(
        list_column_counts(&converted_layout_tree, layout_area, &pair_pane_ids),
        [20, 60]
    );
}

#[test]
fn every_stack_member_is_rewritten_at_the_shown_members_size() {
    // The stack's second member is collapsed. Shown, it holds the 80-column
    // band, where the earlier solve gave its pair 20 and 60.
    let shown_pane_id = PaneId::new();
    let pair_pane_ids = [PaneId::new(), PaneId::new()];
    let pair_split = build_leaf_split(
        SplitDirection::Horizontal,
        &pair_pane_ids,
        vec![build_preferred_weight(20, 10), build_offset_weight(-10)],
    );
    let mut layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Stacked,
        vec![
            LayoutNode::Pane(shown_pane_id),
            LayoutNode::Split(pair_split),
        ],
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        layout_area,
        DEFAULT_PANE_SIZING,
    );

    let LayoutNode::Split(stack_split) = &mut layout_tree else {
        panic!("the tree is a split");
    };
    assert_eq!(list_root_resize_deltas(&stack_split.children[1]), [0, 0]);
    stack_split.active_child_index = 1;
    assert_eq!(
        list_column_counts(&layout_tree, layout_area, &pair_pane_ids),
        [20, 60]
    );
}

#[test]
fn a_missing_weight_is_added_before_its_delta_is_written() {
    // One weight for two children: the second takes the default share. The
    // earlier solve added the `+10` to 40 and 40, trimmed the second pane so
    // the sum is 80 (50 and 30), then cut the first to its preferred 20: 20
    // and 60.
    let pane_ids = [PaneId::new(), PaneId::new()];
    let mut layout_tree = LayoutNode::Split(build_leaf_split(
        SplitDirection::Horizontal,
        &pane_ids,
        vec![build_preferred_weight(20, 10)],
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        layout_area,
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(list_root_resize_deltas(&layout_tree), [0, 0]);
    assert_eq!(
        list_column_counts(&layout_tree, layout_area, &pane_ids),
        [20, 60]
    );
}

#[test]
fn a_child_solved_to_no_cells_keeps_every_value_in_its_subtree() {
    // Ten columns hold the left pane's 4 and not the pair's 8: the pair is
    // suppressed and keeps its deltas.
    let left_pane_id = PaneId::new();
    let pair_split = build_leaf_split(
        SplitDirection::Horizontal,
        &[PaneId::new(), PaneId::new()],
        vec![build_preferred_weight(20, 10), build_offset_weight(-10)],
    );
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Split(pair_split),
        ],
    ));
    let mut converted_layout_tree = layout_tree.clone();

    convert_resize_deltas_to_apply_after_preferred(
        &mut converted_layout_tree,
        build_cell_rect(0, 0, 10, 24),
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(converted_layout_tree, layout_tree);
}

proptest! {
    #[test]
    fn a_rewritten_split_gives_each_kept_child_the_earlier_size(
        weights in proptest::collection::vec(build_size_weight_strategy(), 2..=4),
        axis_cell_count in 8u16..=120,
        gap_cell_count in 0u16..=2,
    ) {
        let pane_ids: Vec<PaneId> = weights.iter().map(|_| PaneId::new()).collect();
        let split = build_leaf_split(SplitDirection::Horizontal, &pane_ids, weights);
        let split_rect = build_cell_rect(0, 0, axis_cell_count, 24);
        let pane_sizing = PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count,
        };
        let kept_children = list_kept_children(&split, split_rect, pane_sizing);
        let earlier_cell_counts = compute_earlier_cell_counts(
            &kept_children.kept_weights,
            &kept_children.kept_floors,
            kept_children.shared_cell_count,
        );

        let mut layout_tree = LayoutNode::Split(split);
        convert_resize_deltas_to_apply_after_preferred(&mut layout_tree, split_rect, pane_sizing);

        let LayoutNode::Split(converted_split) = &layout_tree else {
            panic!("the tree is a split");
        };
        let kept_column_counts: Vec<u16> =
            compute_directional_child_rects(converted_split, split_rect, pane_sizing)
                .into_iter()
                .zip(&kept_children.is_child_kept)
                .filter(|(_, &is_kept)| is_kept)
                .map(|(child_rect, _)| child_rect.size.column_count)
                .collect();
        prop_assert_eq!(kept_column_counts, earlier_cell_counts);
    }
}
