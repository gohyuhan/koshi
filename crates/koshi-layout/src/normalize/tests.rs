//! Tests for layout normalization: cleanup after edits and snapshot restore.

use koshi_core::geometry::{Rect, Size};

use super::*;
use crate::solver::solve_layout;

/// Wraps a pane id in a leaf node.
fn build_pane_leaf(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
}

/// Converts a pane ID slice into the set of live panes.
fn build_live_pane_set(pane_ids: &[PaneId]) -> HashSet<PaneId> {
    pane_ids.iter().copied().collect()
}

/// A plain flex weight of `share` with no overlays and no resize offset.
fn build_flex_weight(weight_share: FlexWeight) -> SizeWeight {
    SizeWeight::from_primary_constraint(SizeConstraint::Flex(weight_share))
}

/// A horizontal split of `children` with the given `weights`.
fn build_horizontal_split(children: Vec<LayoutNode>, weights: Vec<SizeWeight>) -> LayoutNode {
    LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children,
        weights,
        active_child_index: 0,
    })
}

/// Returns a standard 80×24 tab rectangle for test layouts.
fn build_test_tab_rect() -> Rect {
    Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 24,
    })
}

#[test]
fn dead_leaves_are_dropped_and_the_split_collapses() {
    let (live_pane_id, dead_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(live_pane_id), build_pane_leaf(dead_pane_id)],
    ));

    let normalized_layout_tree =
        normalize_layout_tree(&layout_tree, &build_live_pane_set(&[live_pane_id])).unwrap();
    assert_eq!(normalized_layout_tree, LayoutNode::Pane(live_pane_id));
}

#[test]
fn nested_unary_splits_collapse_to_the_leaf() {
    let pane_id = PaneId::new();
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![build_pane_leaf(pane_id)],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![inner_split],
    ));

    let normalized_layout_tree =
        normalize_layout_tree(&layout_tree, &build_live_pane_set(&[pane_id])).unwrap();
    assert_eq!(normalized_layout_tree, LayoutNode::Pane(pane_id));
}

#[test]
fn same_direction_splits_merge_and_preserve_solved_shares() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), inner_split],
    ));

    let original_layout_solution = solve_layout(&layout_tree, build_test_tab_rect());
    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    let normalized_layout_solution = solve_layout(&normalized_layout_tree, build_test_tab_rect());
    assert_eq!(
        original_layout_solution.pane_rects,
        normalized_layout_solution.pane_rects
    );

    let LayoutNode::Split(flattened_split) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(flattened_split.children.len(), 3);
    assert_eq!(
        normalized_layout_tree.list_leaf_pane_ids(),
        [first_pane_id, second_pane_id, third_pane_id]
    );
    // The first pane keeps its half; the second and third panes each keep a quarter.
    assert_eq!(
        normalized_layout_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id),
                build_pane_leaf(third_pane_id),
            ],
            vec![
                build_flex_weight(2),
                build_flex_weight(1),
                build_flex_weight(1)
            ]
        )
    );
}

#[test]
fn merge_is_skipped_when_a_resize_offset_is_present() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut inner_split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    );
    inner_split.weights[0].resize_delta = 4;
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            LayoutNode::Split(inner_split),
        ],
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(outer_split) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    // The presence of resize_delta prevents merging; the nested split survives with its offset.
    assert_eq!(outer_split.children.len(), 2);
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn cross_direction_splits_do_not_merge() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), inner_split],
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn collapsing_a_unary_split_exposes_a_mergeable_child() {
    // A horizontal split containing a vertical wrapper exposes the
    // inner horizontal pair, which must then merge into the root.
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_horizontal_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let wrapper = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![inner_horizontal_split],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), wrapper],
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(flattened_split) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(flattened_split.children.len(), 3);
    assert_eq!(
        normalized_layout_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id),
                build_pane_leaf(third_pane_id),
            ],
            vec![
                build_flex_weight(2),
                build_flex_weight(1),
                build_flex_weight(1)
            ]
        )
    );
}

#[test]
fn stack_reduced_to_one_live_child_becomes_a_plain_leaf() {
    let (dead_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![dead_pane_id, live_pane_id],
        0,
    ));

    let normalized_layout_tree =
        normalize_layout_tree(&layout_tree, &build_live_pane_set(&[live_pane_id])).unwrap();
    assert_eq!(normalized_layout_tree, LayoutNode::Pane(live_pane_id));
}

#[test]
fn dead_members_before_the_active_one_shift_its_index_down() {
    let (dead_pane_id, first_live_pane_id, active_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![dead_pane_id, first_live_pane_id, active_pane_id],
        2,
    ));

    // The dead pane is removed; the active pane stays expanded at index 1.
    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_live_pane_id, active_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(stack) = &normalized_layout_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active_child_index, 1);
    let is_collapsed_by_child_index: Vec<bool> = (0..stack.children.len())
        .map(|child_index| stack.is_child_collapsed(child_index))
        .collect();
    assert_eq!(is_collapsed_by_child_index, [true, false]);
    assert_eq!(
        normalized_layout_tree.list_leaf_pane_ids(),
        [first_live_pane_id, active_pane_id]
    );
}

#[test]
fn dead_active_stack_child_hands_off_to_the_next_member() {
    let (first_pane_id, dead_active_pane_id, next_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, dead_active_pane_id, next_pane_id],
        1,
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, next_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(stack) = &normalized_layout_tree else {
        panic!("stack must survive");
    };
    // The next pane slides into the dead pane's place and becomes expanded.
    assert_eq!(stack.active_child_index, 1);
    let is_collapsed_by_child_index: Vec<bool> = (0..stack.children.len())
        .map(|child_index| stack.is_child_collapsed(child_index))
        .collect();
    assert_eq!(is_collapsed_by_child_index, [true, false]);
    assert_eq!(
        normalized_layout_tree.list_leaf_pane_ids(),
        [first_pane_id, next_pane_id]
    );
}

#[test]
fn invalid_weight_values_are_clamped() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    );
    split_node.weights[0] = SizeWeight {
        primary_constraint: SizeConstraint::Percent(250),
        minimum_cell_count: Some(0),
        preferred_cell_count: Some(0),
        resize_delta: 0,
    };
    split_node.weights[1].primary_constraint = SizeConstraint::Flex(0);
    let layout_tree = LayoutNode::Split(split_node);

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(split_node) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(
        split_node.weights[0].primary_constraint,
        SizeConstraint::Percent(100)
    );
    assert_eq!(split_node.weights[0].minimum_cell_count, None);
    assert_eq!(split_node.weights[0].preferred_cell_count, None);
    assert_eq!(
        split_node.weights[1].primary_constraint,
        SizeConstraint::Flex(1)
    );
}

#[test]
fn missing_weights_are_refilled_with_defaults() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    );
    split_node.weights.pop();
    let layout_tree = LayoutNode::Split(split_node);

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(split_node) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(
        split_node.weights,
        [SizeWeight::default(), SizeWeight::default()]
    );
}

#[test]
fn a_tree_with_no_live_panes_normalizes_to_nothing() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    ));
    assert_eq!(normalize_layout_tree(&layout_tree, &HashSet::new()), None);
}

#[test]
fn normalization_is_idempotent() {
    let (first_pane_id, second_pane_id, third_pane_id, fourth_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![fourth_pane_id, PaneId::new()],
        1,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), inner_split, stack],
    ));
    let live_pane_ids =
        build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id, fourth_pane_id]);

    let normalized_layout_tree = normalize_layout_tree(&layout_tree, &live_pane_ids).unwrap();
    let normalized_again_layout_tree =
        normalize_layout_tree(&normalized_layout_tree, &live_pane_ids).unwrap();
    assert_eq!(normalized_layout_tree, normalized_again_layout_tree);
}

#[test]
fn an_empty_split_normalizes_to_nothing() {
    let empty_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    assert_eq!(normalize_layout_tree(&empty_split, &HashSet::new()), None);
}

#[test]
fn merge_is_skipped_when_inner_flex_weights_would_overflow_their_sum() {
    // Hand-built: the inner split's own flex weights sum past u32::MAX by
    // 4 (not a round wrap to zero, so a naive wrapping add would produce a
    // nonzero — and wrong — factor instead of catching the overflow). The
    // merge factor cannot be computed, so the merge aborts instead of
    // panicking, leaving the nested split intact.
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut inner_split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    );
    inner_split.weights = vec![
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(u32::MAX)),
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(5)),
    ];
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            LayoutNode::Split(inner_split),
        ],
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(outer_split) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(outer_split.children.len(), 2);
    assert_eq!(normalized_layout_tree, layout_tree);
    assert_eq!(
        normalized_layout_tree.list_leaf_pane_ids(),
        [first_pane_id, second_pane_id, third_pane_id]
    );
}

/// Four horizontal child splits whose inner weight sums each reach
/// `u32::MAX`, so the product of the four factors fills most of a `u128`.
/// `extra` names the plain sibling placed before them, or `None` for no
/// sibling.
fn build_near_u128_product_tree(
    extra_sibling: Option<(PaneId, FlexWeight)>,
) -> (LayoutNode, Vec<PaneId>) {
    let mut child_nodes = Vec::new();
    let mut weights = Vec::new();
    let mut pane_ids = Vec::new();
    if let Some((extra_pane_id, extra_weight_share)) = extra_sibling {
        pane_ids.push(extra_pane_id);
        child_nodes.push(build_pane_leaf(extra_pane_id));
        weights.push(build_flex_weight(extra_weight_share));
    }
    for _ in 0..4 {
        let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
        pane_ids.extend([first_pane_id, second_pane_id]);
        let mut inner_split = SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id),
            ],
        );
        inner_split.weights = vec![build_flex_weight(u32::MAX - 1), build_flex_weight(1)];
        child_nodes.push(LayoutNode::Split(inner_split));
        weights.push(build_flex_weight(u32::MAX));
    }
    (build_horizontal_split(child_nodes, weights), pane_ids)
}

#[test]
fn merge_is_skipped_when_a_rescaled_inner_share_overflows_u128() {
    // The four factors multiply to (u32::MAX)^4, which fits u128, but an
    // inner share rescaled by its slot weight and that product does not.
    let (layout_tree, pane_ids) = build_near_u128_product_tree(None);

    let normalized_layout_tree =
        normalize_layout_tree(&layout_tree, &build_live_pane_set(&pane_ids)).unwrap();

    assert_eq!(
        normalized_layout_tree, layout_tree,
        "the split stays nested"
    );
}

#[test]
fn merge_is_skipped_when_a_kept_siblings_rescale_overflows_u128() {
    // The kept sibling's own share is multiplied by the whole product, which
    // is already past u128::MAX / 2.
    let sibling_pane_id = PaneId::new();
    let (layout_tree, pane_ids) = build_near_u128_product_tree(Some((sibling_pane_id, 2)));

    let normalized_layout_tree =
        normalize_layout_tree(&layout_tree, &build_live_pane_set(&pane_ids)).unwrap();

    assert_eq!(
        normalized_layout_tree, layout_tree,
        "the split stays nested"
    );
}

#[test]
fn merge_is_skipped_when_the_slot_weight_carries_a_min_overlay() {
    // The nested split's own weights are plain build_flex_weight, but the slot that
    // holds the split in the outer split carries a min overlay — not a
    // plain build_flex_weight share — so `plain_flex` rejects it and the merge aborts.
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let mut outer_split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), inner_split],
    );
    outer_split.weights[1].minimum_cell_count = Some(10);
    let layout_tree = LayoutNode::Split(outer_split);

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(normalized_split) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(normalized_split.children.len(), 2);
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn canonical_weight_clamps_every_zero_variant_up_to_one() {
    let pane_ids: Vec<PaneId> = (0..4).map(|_| PaneId::new()).collect();
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        pane_ids
            .iter()
            .map(|&pane_id| build_pane_leaf(pane_id))
            .collect(),
    );
    split_node.weights[0].primary_constraint = SizeConstraint::Percent(0);
    split_node.weights[1].primary_constraint = SizeConstraint::Fixed(0);
    split_node.weights[2].primary_constraint = SizeConstraint::Minimum(0);
    split_node.weights[3].primary_constraint = SizeConstraint::Preferred(0);
    let layout_tree = LayoutNode::Split(split_node);

    let normalized_layout_tree =
        normalize_layout_tree(&layout_tree, &build_live_pane_set(&pane_ids)).unwrap();
    let LayoutNode::Split(normalized_split) = &normalized_layout_tree else {
        panic!("expected a split");
    };
    assert_eq!(
        normalized_split.weights[0].primary_constraint,
        SizeConstraint::Percent(1)
    );
    assert_eq!(
        normalized_split.weights[1].primary_constraint,
        SizeConstraint::Fixed(1)
    );
    assert_eq!(
        normalized_split.weights[2].primary_constraint,
        SizeConstraint::Minimum(1)
    );
    assert_eq!(
        normalized_split.weights[3].primary_constraint,
        SizeConstraint::Preferred(1)
    );
}

#[test]
fn an_already_canonical_tree_is_returned_unchanged() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), vertical_split],
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn a_dead_last_active_stack_member_hands_off_to_the_new_last_member() {
    let (first_pane_id, second_pane_id, dead_active_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, second_pane_id, dead_active_pane_id],
        2,
    ));

    // The expanded member was the last one and it died. No member sits at
    // or after its index any more, so the new last member expands.
    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    let LayoutNode::Split(stack) = &normalized_layout_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active_child_index, 1);
    let collapsed_child_flags: Vec<bool> = (0..stack.children.len())
        .map(|child_index| stack.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, false]);
    assert_eq!(
        normalized_layout_tree.list_leaf_pane_ids(),
        [first_pane_id, second_pane_id]
    );
}

#[test]
fn a_live_lone_pane_is_returned_as_is() {
    let pane_id = PaneId::new();
    assert_eq!(
        normalize_layout_tree(&LayoutNode::Pane(pane_id), &build_live_pane_set(&[pane_id])),
        Some(LayoutNode::Pane(pane_id))
    );
}

#[test]
fn a_dead_lone_pane_normalizes_to_nothing() {
    let pane_id = PaneId::new();
    assert_eq!(
        normalize_layout_tree(&LayoutNode::Pane(pane_id), &HashSet::new()),
        None
    );
}

#[test]
fn an_out_of_range_stack_active_index_expands_the_last_member() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 0);
    stack.active_child_index = 9;
    let layout_tree = LayoutNode::Split(stack);

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    assert_eq!(
        normalized_layout_tree,
        LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![first_pane_id, second_pane_id],
            1,
        ))
    );
}

#[test]
fn extra_weights_past_the_children_are_dropped() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    );
    split_node.weights.push(build_flex_weight(7));
    let layout_tree = LayoutNode::Split(split_node);

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    assert_eq!(
        normalized_layout_tree,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id)
            ],
        ))
    );
}

#[test]
fn a_directional_split_resets_its_active_index() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
    );
    split_node.active_child_index = 1;
    let layout_tree = LayoutNode::Split(split_node);

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    assert_eq!(
        normalized_layout_tree,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id)
            ],
        ))
    );
}

#[test]
fn three_nested_same_direction_splits_flatten_into_one() {
    // Three nested horizontal splits flatten into one. The first pane keeps its
    // half, the second its quarter, and the last two an eighth each.
    let (first_pane_id, second_pane_id, third_pane_id, fourth_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let innermost_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(third_pane_id),
            build_pane_leaf(fourth_pane_id),
        ],
    ));
    let middle_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(second_pane_id), innermost_split],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), middle_split],
    ));

    let original_layout_solution = solve_layout(&layout_tree, build_test_tab_rect());
    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id, fourth_pane_id]),
    )
    .unwrap();
    assert_eq!(
        solve_layout(&normalized_layout_tree, build_test_tab_rect()).pane_rects,
        original_layout_solution.pane_rects
    );
    assert_eq!(
        normalized_layout_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id),
                build_pane_leaf(third_pane_id),
                build_pane_leaf(fourth_pane_id),
            ],
            vec![
                build_flex_weight(4),
                build_flex_weight(2),
                build_flex_weight(1),
                build_flex_weight(1)
            ]
        )
    );
}

#[test]
fn merged_shares_keep_unequal_proportions() {
    // The outer share is one third and the nested shares are three quarters and
    // one quarter. Over 96 columns the results are 32, 48, and 16.
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = build_horizontal_split(
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
        vec![build_flex_weight(3), build_flex_weight(1)],
    );
    let layout_tree = build_horizontal_split(
        vec![build_pane_leaf(first_pane_id), inner_split],
        vec![build_flex_weight(1), build_flex_weight(2)],
    );
    let wide_tab_rect = Rect::from_size_at_origin(Size {
        column_count: 96,
        row_count: 24,
    });

    let original_layout_solution = solve_layout(&layout_tree, wide_tab_rect);
    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    assert_eq!(
        solve_layout(&normalized_layout_tree, wide_tab_rect).pane_rects,
        original_layout_solution.pane_rects
    );
    assert_eq!(
        normalized_layout_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id),
                build_pane_leaf(third_pane_id),
            ],
            vec![
                build_flex_weight(4),
                build_flex_weight(6),
                build_flex_weight(2)
            ]
        )
    );
}

#[test]
fn merge_is_skipped_when_a_kept_sibling_is_not_a_plain_flex_share() {
    // The nested pair is plain build_flex_weight, but the first sibling claims a percentage.
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let layout_tree = build_horizontal_split(
        vec![build_pane_leaf(first_pane_id), inner_split],
        vec![
            SizeWeight::from_primary_constraint(SizeConstraint::Percent(50)),
            build_flex_weight(1),
        ],
    );

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn merge_is_skipped_when_a_rescaled_share_would_exceed_the_weight_maximum() {
    // The first pane's share of u32::MAX doubled by the inner pair's factor of 2 does
    // not fit a weight, so the pair stays nested.
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(third_pane_id),
        ],
    ));
    let layout_tree = build_horizontal_split(
        vec![build_pane_leaf(first_pane_id), inner_split],
        vec![build_flex_weight(u32::MAX), build_flex_weight(1)],
    );

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn a_dead_leaf_inside_a_nested_split_collapses_it_into_the_parent() {
    // A dead third pane leaves the inner pair collapsed to the second pane, and
    // that pane takes
    // the pair's slot weight.
    let (first_pane_id, second_pane_id, dead_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_pane_leaf(second_pane_id),
            build_pane_leaf(dead_pane_id),
        ],
    ));
    let layout_tree = build_horizontal_split(
        vec![build_pane_leaf(first_pane_id), inner_split],
        vec![build_flex_weight(1), build_flex_weight(3)],
    );

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    assert_eq!(
        normalized_layout_tree,
        build_horizontal_split(
            vec![
                build_pane_leaf(first_pane_id),
                build_pane_leaf(second_pane_id)
            ],
            vec![build_flex_weight(1), build_flex_weight(3)],
        )
    );
}

#[test]
fn a_stack_inside_a_directional_split_stays_nested() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![second_pane_id, third_pane_id],
        1,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_pane_leaf(first_pane_id), stack],
    ));

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id, third_pane_id]),
    )
    .unwrap();
    assert_eq!(normalized_layout_tree, layout_tree);
}

#[test]
fn valid_overlays_and_the_resize_offset_pass_through_unchanged() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_horizontal_split(
        vec![
            build_pane_leaf(first_pane_id),
            build_pane_leaf(second_pane_id),
        ],
        vec![
            SizeWeight {
                primary_constraint: SizeConstraint::Percent(100),
                minimum_cell_count: Some(5),
                preferred_cell_count: Some(7),
                resize_delta: -3,
            },
            SizeWeight {
                primary_constraint: SizeConstraint::Fixed(u16::MAX),
                minimum_cell_count: None,
                preferred_cell_count: None,
                resize_delta: i32::MAX,
            },
        ],
    );

    let normalized_layout_tree = normalize_layout_tree(
        &layout_tree,
        &build_live_pane_set(&[first_pane_id, second_pane_id]),
    )
    .unwrap();
    assert_eq!(normalized_layout_tree, layout_tree);
}
