//! Tests for the rewrite of resize deltas laid out by the solve that applies
//! each `resize_delta` before the `preferred` targets.

use koshi_core::geometry::{Rect, SplitDirection};
use koshi_core::ids::PaneId;
use koshi_core::process::PtySize;
use koshi_test_support::fixtures::build_cell_rect;
use proptest::prelude::*;

use super::*;
use crate::size::SizeConstraint;
use crate::solver::tests::build_size_weight_strategy;
use crate::solver::{
    apply_preferred_cell_counts, apply_resize_deltas, clamp_cell_counts_to_floors,
    compute_share_cell_counts, distribute_axis_cells, get_preferred_cell_count, is_flexible,
    solve_layout_with_sizing, MIN_PANE_SIZE,
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

    convert_resize_deltas_at_rect(
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

        convert_resize_deltas_at_rect(&mut layout_tree, layout_area, DEFAULT_PANE_SIZING);

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

    convert_resize_deltas_at_rect(&mut layout_tree, layout_area, DEFAULT_PANE_SIZING);

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

    convert_resize_deltas_at_rect(&mut converted_layout_tree, layout_area, DEFAULT_PANE_SIZING);

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

    convert_resize_deltas_at_rect(&mut layout_tree, layout_area, DEFAULT_PANE_SIZING);

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

    convert_resize_deltas_at_rect(&mut layout_tree, layout_area, DEFAULT_PANE_SIZING);

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

    convert_resize_deltas_at_rect(
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
        convert_resize_deltas_at_rect(&mut layout_tree, split_rect, pane_sizing);

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

/// The sizes the `preferred` pass gives `cell_counts` when each child with a
/// target ends at that target and its surplus or deficit moves to or from its
/// receiver alone: the trailing-most flexible sibling. A child with no
/// flexible sibling keeps its cells. `None` when a target is below its child's
/// floor, or a receiver ends below its floor.
fn apply_preferred_targets_through_receivers(
    cell_counts: &[i64],
    weights: &[SizeWeight],
    floors: &[u16],
) -> Option<Vec<i64>> {
    let mut target_cell_counts = cell_counts.to_vec();
    for (child_index, child_weight) in weights.iter().enumerate() {
        let Some(target_cell_count) = get_preferred_cell_count(child_weight) else {
            continue;
        };
        let Some(receiver_child_index) = (0..weights.len()).rev().find(|&sibling_index| {
            sibling_index != child_index && is_flexible(&weights[sibling_index])
        }) else {
            continue;
        };
        if target_cell_count < floors[child_index] {
            return None;
        }
        let moved_cell_count = target_cell_counts[child_index] - i64::from(target_cell_count);
        target_cell_counts[child_index] = i64::from(target_cell_count);
        target_cell_counts[receiver_child_index] += moved_cell_count;
        if target_cell_counts[receiver_child_index] < i64::from(floors[receiver_child_index]) {
            return None;
        }
    }
    Some(target_cell_counts)
}

/// `true` when every one of `cell_counts` lies between its floor and
/// `shared_cell_count`.
fn is_every_cell_count_within_limits(
    cell_counts: &[i64],
    floors: &[u16],
    shared_cell_count: u16,
) -> bool {
    cell_counts
        .iter()
        .zip(floors)
        .all(|(&cell_count, &floor_cell_count)| {
            (i64::from(floor_cell_count)..=i64::from(shared_cell_count)).contains(&cell_count)
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2048,
        max_global_rejects: 1_000_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn a_folded_split_solves_to_the_rewritten_sizes_while_no_limit_binds(
        mut weights in proptest::collection::vec(build_size_weight_strategy(), 2..=4),
        resize_delta_seeds in proptest::collection::vec(-20i32..=20, 3),
        axis_cell_count in 8u16..=200,
        gap_cell_count in 0u16..=2,
    ) {
        let last_child_index = weights.len() - 1;
        for (child_index, size_weight) in weights.iter_mut().enumerate().take(last_child_index) {
            size_weight.resize_delta = resize_delta_seeds[child_index];
        }
        weights[last_child_index].resize_delta =
            -resize_delta_seeds[..last_child_index].iter().sum::<i32>();
        let pane_ids: Vec<PaneId> = weights.iter().map(|_| PaneId::new()).collect();
        let split = build_leaf_split(SplitDirection::Horizontal, &pane_ids, weights.clone());
        let split_rect = build_cell_rect(0, 0, axis_cell_count, 24);
        let pane_sizing = PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count,
        };
        let kept_children = list_kept_children(&split, split_rect, pane_sizing);
        prop_assume!(kept_children.is_child_kept.iter().all(|&is_kept| is_kept));
        let floors = &kept_children.kept_floors;
        let shared_cell_count = kept_children.shared_cell_count;

        let mut folded_split = split.clone();
        fold_split_preferred_resize_deltas(&mut folded_split);
        prop_assume!(folded_split.weights != weights);

        let share_cell_counts: Vec<i64> = compute_share_cell_counts(&weights, shared_cell_count)
            .into_iter()
            .map(i64::from)
            .collect();
        prop_assume!(share_cell_counts.iter().sum::<i64>() == i64::from(shared_cell_count));
        let shifted_cell_counts: Vec<i64> = share_cell_counts
            .iter()
            .zip(&weights)
            .map(|(&share_cell_count, size_weight)| {
                share_cell_count + i64::from(size_weight.resize_delta)
            })
            .collect();
        prop_assume!(is_every_cell_count_within_limits(&shifted_cell_counts, &vec![0; weights.len()], shared_cell_count));
        let earlier_model_cell_counts =
            apply_preferred_targets_through_receivers(&shifted_cell_counts, &weights, floors);
        prop_assume!(earlier_model_cell_counts.as_ref().is_some_and(|cell_counts| {
            is_every_cell_count_within_limits(cell_counts, floors, shared_cell_count)
        }));
        let folded_model_cell_counts: Option<Vec<i64>> =
            apply_preferred_targets_through_receivers(&share_cell_counts, &folded_split.weights, floors)
                .map(|target_cell_counts| {
                    target_cell_counts
                        .iter()
                        .zip(&folded_split.weights)
                        .map(|(&target_cell_count, size_weight)| {
                            target_cell_count + i64::from(size_weight.resize_delta)
                        })
                        .collect()
                });
        prop_assume!(folded_model_cell_counts.as_ref().is_some_and(|cell_counts| {
            is_every_cell_count_within_limits(cell_counts, floors, shared_cell_count)
        }));

        let earlier_cell_counts = compute_earlier_cell_counts(&weights, floors, shared_cell_count);
        let folded_cell_counts =
            distribute_axis_cells(&folded_split.weights, floors, shared_cell_count);
        let mut converted_layout_tree = LayoutNode::Split(split);
        convert_resize_deltas_at_rect(
            &mut converted_layout_tree,
            split_rect,
            pane_sizing,
        );
        let LayoutNode::Split(converted_split) = &converted_layout_tree else {
            panic!("the tree is a split");
        };
        let converted_cell_counts: Vec<u16> =
            compute_directional_child_rects(converted_split, split_rect, pane_sizing)
                .into_iter()
                .map(|child_rect| child_rect.size.column_count)
                .collect();
        let earlier_model_cell_counts: Vec<u16> = earlier_model_cell_counts
            .expect("the earlier run is within its limits")
            .into_iter()
            .map(|cell_count| u16::try_from(cell_count).expect("a size within the axis"))
            .collect();
        prop_assert_eq!(&earlier_cell_counts, &earlier_model_cell_counts);
        prop_assert_eq!(&folded_cell_counts, &converted_cell_counts);
        prop_assert_eq!(&folded_cell_counts, &earlier_cell_counts);
    }
}

/// The carried terminal sizes, one `(pane_id, column_count, row_count)` per
/// pane.
fn build_pty_size_by_pane_id(carried_sizes: &[(PaneId, u16, u16)]) -> HashMap<PaneId, PtySize> {
    carried_sizes
        .iter()
        .map(|&(pane_id, column_count, row_count)| {
            (
                pane_id,
                PtySize {
                    column_count,
                    row_count,
                },
            )
        })
        .collect()
}

#[test]
fn resize_deltas_that_moved_nothing_become_zero_at_the_carried_sizes() {
    // 80 columns, the left pane aiming for 20, ten presses stored `+10` and
    // `-10`, and the panes were drawn at 20 and 60 columns: 18 and 58 inside
    // their borders. The measured tab is 80 by 24.
    let pane_ids = [PaneId::new(), PaneId::new()];
    let mut layout_tree = LayoutNode::Split(build_leaf_split(
        SplitDirection::Horizontal,
        &pane_ids,
        vec![build_preferred_weight(20, 10), build_offset_weight(-10)],
    ));
    let pty_size_by_pane_id =
        build_pty_size_by_pane_id(&[(pane_ids[0], 18, 22), (pane_ids[1], 58, 22)]);

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &pty_size_by_pane_id,
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(list_root_resize_deltas(&layout_tree), [0, 0]);
    assert_eq!(
        list_column_counts(&layout_tree, build_cell_rect(0, 0, 80, 24), &pane_ids),
        [20, 60]
    );
}

#[test]
fn three_panes_keep_the_sizes_they_were_drawn_at() {
    // The earlier solve gave 80, 6 and 4 of 90 columns: 78, 4 and 2 inside
    // the borders. The share and the target give 30, 10 and 50.
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut layout_tree = build_three_pane_tree(&pane_ids);
    let pty_size_by_pane_id = build_pty_size_by_pane_id(&[
        (pane_ids[0], 78, 22),
        (pane_ids[1], 4, 22),
        (pane_ids[2], 2, 22),
    ]);

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &pty_size_by_pane_id,
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(list_root_resize_deltas(&layout_tree), [50, -4, -46]);
    assert_eq!(
        list_column_counts(&layout_tree, build_cell_rect(0, 0, 90, 24), &pane_ids),
        [80, 6, 4]
    );
}

#[test]
fn a_gap_between_panes_is_part_of_the_measured_tab() {
    // 94 columns with a 2-cell gap between each pair leave 90, where the
    // earlier solve gave 80, 6 and 4: 78, 4 and 2 inside the borders.
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut layout_tree = build_three_pane_tree(&pane_ids);
    let pty_size_by_pane_id = build_pty_size_by_pane_id(&[
        (pane_ids[0], 78, 22),
        (pane_ids[1], 4, 22),
        (pane_ids[2], 2, 22),
    ]);
    let pane_sizing = PaneSizing {
        minimum_size: MIN_PANE_SIZE,
        gap_cell_count: 2,
    };

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &pty_size_by_pane_id,
        pane_sizing,
    );

    assert_eq!(list_root_resize_deltas(&layout_tree), [50, -4, -46]);
    let layout_solve =
        solve_layout_with_sizing(&layout_tree, build_cell_rect(0, 0, 94, 24), pane_sizing);
    assert_eq!(
        pane_ids.map(|pane_id| layout_solve
            .find_pane_rect(pane_id)
            .expect("every pane is solved")
            .size
            .column_count),
        [80, 6, 4]
    );
}

#[test]
fn a_stack_is_measured_from_its_shown_member_and_one_header_row_per_other_member() {
    // The shown member is drawn at 88 by 21 inside its border, and the
    // collapsed three panes below it carry sizes from before they collapsed.
    // The measured tab is 90 by 24, and the three panes are rewritten at the
    // 90 by 23 band, where the earlier solve gave them 80, 6 and 4.
    let shown_pane_id = PaneId::new();
    let collapsed_pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Stacked,
        vec![
            LayoutNode::Pane(shown_pane_id),
            build_three_pane_tree(&collapsed_pane_ids),
        ],
    ));
    let pty_size_by_pane_id = build_pty_size_by_pane_id(&[
        (shown_pane_id, 88, 21),
        (collapsed_pane_ids[0], 5, 5),
        (collapsed_pane_ids[1], 5, 5),
        (collapsed_pane_ids[2], 5, 5),
    ]);

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &pty_size_by_pane_id,
        DEFAULT_PANE_SIZING,
    );

    let LayoutNode::Split(stack_split) = &layout_tree else {
        panic!("the tree is a split");
    };
    assert_eq!(
        list_root_resize_deltas(&stack_split.children[1]),
        [50, -4, -46]
    );
}

/// Three panes over 90 columns: a flexible one with resize delta `+60`, one
/// aiming for 10 columns with `-60`, and a flexible one with `0`. The earlier
/// solve gives 80, 6 and 4 columns.
fn build_three_pane_tree(pane_ids: &[PaneId; 3]) -> LayoutNode {
    LayoutNode::Split(build_leaf_split(
        SplitDirection::Horizontal,
        pane_ids,
        vec![
            build_offset_weight(60),
            build_preferred_weight(10, -60),
            build_offset_weight(0),
        ],
    ))
}

/// Convert the three-pane tree of [`build_three_pane_tree`] with `pane_sizing`
/// at the carried `(column_count, row_count)` of each pane, `None` for a pane
/// that carries no size, and return the root's resize deltas.
fn convert_three_pane_tree(
    carried_sizes: [Option<(u16, u16)>; 3],
    pane_sizing: PaneSizing,
) -> Vec<i32> {
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut layout_tree = build_three_pane_tree(&pane_ids);
    let pty_size_by_pane_id: HashMap<PaneId, PtySize> = pane_ids
        .iter()
        .zip(carried_sizes)
        .filter_map(|(&pane_id, carried_size)| {
            carried_size.map(|(column_count, row_count)| {
                (
                    pane_id,
                    PtySize {
                        column_count,
                        row_count,
                    },
                )
            })
        })
        .collect();

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &pty_size_by_pane_id,
        pane_sizing,
    );

    list_root_resize_deltas(&layout_tree)
}

#[test]
fn a_tab_that_lost_a_pane_with_no_viewer_has_its_preferred_resize_delta_folded() {
    // The last pane shared its column with a pane that closed while no client
    // viewed the tab: it still carries 10 of the 22 rows. No rect gives the
    // three panes these sizes, and the middle pane's `-60` moves onto the
    // last pane. Rewritten at the measured 90 by 12 tab, the deltas would
    // read `+50`, `-4` and `-46`.
    assert_eq!(
        convert_three_pane_tree(
            [Some((78, 22)), Some((4, 22)), Some((2, 10))],
            DEFAULT_PANE_SIZING
        ),
        [60, 0, -60]
    );
}

#[test]
fn a_tab_whose_last_viewer_zoomed_a_pane_has_its_preferred_resize_delta_folded() {
    // The zoomed first pane carries the whole 90 by 24 tab, 88 by 22, and the
    // other two their sizes from before the zoom. The measured 100 by 24 tab
    // solves the first pane to 86 columns, not 90. Rewritten there, the deltas
    // would read `+53`, `0` and `-53`.
    assert_eq!(
        convert_three_pane_tree(
            [Some((88, 22)), Some((4, 22)), Some((2, 22))],
            DEFAULT_PANE_SIZING
        ),
        [60, 0, -60]
    );
}

#[test]
fn a_tab_with_a_pane_hidden_for_lack_of_room_has_its_preferred_resize_delta_folded() {
    // A 10-column tab held the first two panes at 4 and 6 columns, 2 and 4
    // inside their borders, and hid the last pane, which still carries 48
    // columns from a wider tab. The measured 60 by 24 tab solves the first
    // pane to 50 columns, not 4. Rewritten there, the deltas would read
    // `+30`, `-4` and `-26`.
    assert_eq!(
        convert_three_pane_tree(
            [Some((2, 22)), Some((4, 22)), Some((48, 22))],
            DEFAULT_PANE_SIZING
        ),
        [60, 0, -60]
    );
}

#[test]
fn a_tab_with_a_pane_that_carries_no_size_has_its_preferred_resize_delta_folded() {
    assert_eq!(
        convert_three_pane_tree([Some((78, 22)), None, Some((2, 22))], DEFAULT_PANE_SIZING),
        [60, 0, -60]
    );
}

#[test]
fn a_tab_whose_panes_are_all_below_the_pane_minimum_has_its_preferred_resize_delta_folded() {
    // The panes were drawn one row high. With a 5-row pane minimum, the
    // measured 90 by 3 tab shows no pane, and the tree would keep `+60`,
    // `-60` and `0`.
    let pane_sizing = PaneSizing {
        minimum_size: Size {
            column_count: 2,
            row_count: 5,
        },
        gap_cell_count: 0,
    };
    assert_eq!(
        convert_three_pane_tree([Some((78, 1)), Some((4, 1)), Some((2, 1))], pane_sizing),
        [60, 0, -60]
    );
}

#[test]
fn the_fold_reaches_a_nested_split_and_every_stack_member_and_skips_a_pane_with_no_flexible_sibling(
) {
    // A stack whose collapsed member is a pair aiming its left pane at 20,
    // beside a fixed pane that aims for 10 with no flexible sibling. No pane
    // carries a size.
    let fixed_pane_id = PaneId::new();
    let pair_split = build_leaf_split(
        SplitDirection::Horizontal,
        &[PaneId::new(), PaneId::new()],
        vec![build_preferred_weight(20, 10), build_offset_weight(-10)],
    );
    let stack_split = SplitNode::with_equal_weights(
        SplitDirection::Stacked,
        vec![
            LayoutNode::Pane(PaneId::new()),
            LayoutNode::Split(pair_split),
        ],
    );
    let fixed_pair_split = build_leaf_split(
        SplitDirection::Horizontal,
        &[fixed_pane_id, PaneId::new()],
        vec![
            SizeWeight {
                preferred_cell_count: Some(10),
                resize_delta: 4,
                ..SizeWeight::from_primary_constraint(SizeConstraint::Fixed(30))
            },
            SizeWeight {
                resize_delta: -4,
                ..SizeWeight::from_primary_constraint(SizeConstraint::Fixed(30))
            },
        ],
    );
    let mut layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutNode::Split(stack_split),
            LayoutNode::Split(fixed_pair_split),
        ],
    ));

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &HashMap::new(),
        DEFAULT_PANE_SIZING,
    );

    let LayoutNode::Split(root_split) = &layout_tree else {
        panic!("the tree is a split");
    };
    let LayoutNode::Split(folded_stack_split) = &root_split.children[0] else {
        panic!("the first child is the stack");
    };
    assert_eq!(
        list_root_resize_deltas(&folded_stack_split.children[1]),
        [0, 0]
    );
    assert_eq!(list_root_resize_deltas(&root_split.children[1]), [4, -4]);
}

#[test]
fn the_fold_adds_the_missing_weight_of_the_receiver() {
    // One weight for two children: the second takes the default share and
    // receives the first pane's `+7`.
    let pane_ids = [PaneId::new(), PaneId::new()];
    let mut layout_tree = LayoutNode::Split(build_leaf_split(
        SplitDirection::Horizontal,
        &pane_ids,
        vec![build_preferred_weight(20, 7)],
    ));

    convert_resize_deltas_to_apply_after_preferred(
        &mut layout_tree,
        &HashMap::new(),
        DEFAULT_PANE_SIZING,
    );

    assert_eq!(list_root_resize_deltas(&layout_tree), [0, 7]);
}

/// Any layout tree of up to three levels, two or three children per split,
/// each split horizontal, vertical or stacked, with any weights.
fn build_layout_tree_strategy() -> impl Strategy<Value = LayoutNode> {
    Just(())
        .prop_map(|()| LayoutNode::Pane(PaneId::new()))
        .prop_recursive(3, 16, 3, |child_strategy| {
            (
                prop_oneof![
                    Just(SplitDirection::Horizontal),
                    Just(SplitDirection::Vertical),
                    Just(SplitDirection::Stacked),
                ],
                proptest::collection::vec(child_strategy, 2..=3),
                proptest::collection::vec(build_size_weight_strategy(), 3),
                any::<proptest::sample::Index>(),
            )
                .prop_map(|(direction, children, weights, active_child_choice)| {
                    let child_count = children.len();
                    let mut split = SplitNode::with_equal_weights(direction, children);
                    split.weights = weights[..child_count].to_vec();
                    split.active_child_index = active_child_choice.index(child_count);
                    LayoutNode::Split(split)
                })
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2048,
        max_global_rejects: 1_000_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn a_tab_drawn_with_every_pane_shown_is_measured_at_its_rect_and_keeps_every_size(
        layout_tree in build_layout_tree_strategy(),
        column_count in 8u16..=200,
        row_count in 3u16..=60,
        gap_cell_count in 0u16..=2,
    ) {
        let tab_rect = build_cell_rect(0, 0, column_count, row_count);
        let pane_sizing = PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count,
        };
        let mut rect_converted_layout_tree = layout_tree.clone();
        convert_resize_deltas_at_rect(&mut rect_converted_layout_tree, tab_rect, pane_sizing);
        let earlier_layout_solve =
            solve_layout_with_sizing(&rect_converted_layout_tree, tab_rect, pane_sizing);
        let pty_size_by_pane_id: HashMap<PaneId, PtySize> = list_content_rects(&earlier_layout_solve)
            .into_iter()
            .filter_map(|(pane_id, content_rect)| {
                content_rect.map(|content_rect| {
                    (
                        pane_id,
                        PtySize {
                            column_count: content_rect.size.column_count,
                            row_count: content_rect.size.row_count,
                        },
                    )
                })
            })
            .collect();
        let mut measured_pane_count: usize = 0;
        let measured_tab_size = measure_layout_size(
            &layout_tree,
            &pty_size_by_pane_id,
            gap_cell_count,
            &mut measured_pane_count,
        );
        prop_assume!(measured_tab_size.is_some());

        let mut converted_layout_tree = layout_tree.clone();
        convert_resize_deltas_to_apply_after_preferred(
            &mut converted_layout_tree,
            &pty_size_by_pane_id,
            pane_sizing,
        );

        prop_assert_eq!(measured_tab_size, Some(tab_rect.size));
        prop_assert_eq!(measured_pane_count, pty_size_by_pane_id.len());
        prop_assert_eq!(&converted_layout_tree, &rect_converted_layout_tree);
        let converted_layout_solve =
            solve_layout_with_sizing(&converted_layout_tree, tab_rect, pane_sizing);
        for (pane_id, pty_size) in &pty_size_by_pane_id {
            let content_size = crate::content::find_content_rect(&converted_layout_solve, *pane_id)
                .map(|content_rect| content_rect.size);
            prop_assert_eq!(
                content_size,
                Some(Size {
                    column_count: pty_size.column_count,
                    row_count: pty_size.row_count,
                })
            );
        }
    }
}
