//! Tests for layout normalization: cleanup after edits and snapshot restore.

use koshi_core::geometry::{Rect, Size};

use super::*;
use crate::solver::solve;

/// Wraps a pane id in a leaf node.
fn leaf(pane: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane)
}

/// Converts a pane ID slice into the set of live panes.
fn live(panes: &[PaneId]) -> HashSet<PaneId> {
    panes.iter().copied().collect()
}

/// A plain flex weight of `share` with no overlays and no resize offset.
fn flex(share: Weight) -> SizeWeight {
    SizeWeight::new(SizeConstraint::Flex(share))
}

/// A horizontal split of `children` with the given `weights`.
fn row(children: Vec<LayoutNode>, weights: Vec<SizeWeight>) -> LayoutNode {
    LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children,
        weights,
        active: 0,
    })
}

/// Returns a standard 80×24 tab rectangle for test layouts.
fn tab() -> Rect {
    Rect::at_origin(Size { cols: 80, rows: 24 })
}

#[test]
fn dead_leaves_are_dropped_and_the_split_collapses() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b)],
    ));

    let normalized = normalize(&tree, &live(&[a])).unwrap();
    assert_eq!(normalized, LayoutNode::Pane(a));
}

#[test]
fn nested_unary_splits_collapse_to_the_leaf() {
    let a = PaneId::new();
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(a)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![inner],
    ));

    let normalized = normalize(&tree, &live(&[a])).unwrap();
    assert_eq!(normalized, LayoutNode::Pane(a));
}

#[test]
fn same_direction_splits_merge_and_preserve_solved_shares() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), inner],
    ));

    let before = solve(&tree, tab());
    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let after = solve(&normalized, tab());
    assert_eq!(before.panes, after.panes);

    let LayoutNode::Split(flat) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(flat.children.len(), 3);
    assert_eq!(normalized.leaf_panes(), [a, b, c]);
    // a keeps its half; b and c each keep a quarter.
    assert_eq!(
        normalized,
        row(
            vec![leaf(a), leaf(b), leaf(c)],
            vec![flex(2), flex(1), flex(1)]
        )
    );
}

#[test]
fn merge_is_skipped_when_a_resize_offset_is_present() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut inner =
        SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(b), leaf(c)]);
    inner.weights[0].resize_delta = 4;
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutNode::Split(inner)],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(outer) = &normalized else {
        panic!("expected a split");
    };
    // The presence of resize_delta prevents merging; the nested split survives with its offset.
    assert_eq!(outer.children.len(), 2);
    assert_eq!(normalized, tree);
}

#[test]
fn cross_direction_splits_do_not_merge() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), inner],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(normalized, tree);
}

#[test]
fn collapsing_a_unary_split_exposes_a_mergeable_child() {
    // h(a, v(h(b, c))): dropping the unary vertical wrapper exposes the
    // inner horizontal pair, which must then merge into the root.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_h = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let wrapper = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![inner_h],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), wrapper],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(flat) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(flat.children.len(), 3);
    assert_eq!(
        normalized,
        row(
            vec![leaf(a), leaf(b), leaf(c)],
            vec![flex(2), flex(1), flex(1)]
        )
    );
}

#[test]
fn stack_reduced_to_one_live_child_becomes_a_plain_leaf() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));

    let normalized = normalize(&tree, &live(&[b])).unwrap();
    assert_eq!(normalized, LayoutNode::Pane(b));
}

#[test]
fn dead_members_before_the_active_one_shift_its_index_down() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 2));

    // a dies; c stays the expanded member and sits at index 1.
    let normalized = normalize(&tree, &live(&[b, c])).unwrap();
    let LayoutNode::Split(stack) = &normalized else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = (0..stack.children.len())
        .map(|index| stack.is_collapsed(index))
        .collect();
    assert_eq!(collapsed, [true, false]);
    assert_eq!(normalized.leaf_panes(), [b, c]);
}

#[test]
fn dead_active_stack_child_hands_off_to_the_next_member() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 1));

    let normalized = normalize(&tree, &live(&[a, c])).unwrap();
    let LayoutNode::Split(stack) = &normalized else {
        panic!("stack must survive");
    };
    // c slid into b's place and becomes the expanded child.
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = (0..stack.children.len())
        .map(|index| stack.is_collapsed(index))
        .collect();
    assert_eq!(collapsed, [true, false]);
    assert_eq!(normalized.leaf_panes(), [a, c]);
}

#[test]
fn invalid_weight_values_are_clamped() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut split =
        SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(a), leaf(b)]);
    split.weights[0] = SizeWeight {
        primary: SizeConstraint::Percent(250),
        min: Some(0),
        preferred: Some(0),
        resize_delta: 0,
    };
    split.weights[1].primary = SizeConstraint::Flex(0);
    let tree = LayoutNode::Split(split);

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    let LayoutNode::Split(split) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(split.weights[0].primary, SizeConstraint::Percent(100));
    assert_eq!(split.weights[0].min, None);
    assert_eq!(split.weights[0].preferred, None);
    assert_eq!(split.weights[1].primary, SizeConstraint::Flex(1));
}

#[test]
fn missing_weights_are_refilled_with_defaults() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut split =
        SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(a), leaf(b)]);
    split.weights.pop();
    let tree = LayoutNode::Split(split);

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    let LayoutNode::Split(split) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(
        split.weights,
        [SizeWeight::default(), SizeWeight::default()]
    );
}

#[test]
fn a_tree_with_no_live_panes_normalizes_to_nothing() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b)],
    ));
    assert_eq!(normalize(&tree, &HashSet::new()), None);
}

#[test]
fn normalization_is_idempotent() {
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let stack = LayoutNode::Split(SplitNode::stack(vec![d, PaneId::new()], 1));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), inner, stack],
    ));
    let alive = live(&[a, b, c, d]);

    let once = normalize(&tree, &alive).unwrap();
    let twice = normalize(&once, &alive).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn an_empty_split_normalizes_to_nothing() {
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    assert_eq!(normalize(&empty, &HashSet::new()), None);
}

#[test]
fn merge_is_skipped_when_inner_flex_weights_would_overflow_their_sum() {
    // Hand-built: the inner split's own flex weights sum past u32::MAX by
    // 4 (not a round wrap to zero, so a naive wrapping add would produce a
    // nonzero — and wrong — factor instead of catching the overflow). The
    // merge factor cannot be computed, so the merge aborts instead of
    // panicking, leaving the nested split intact.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut inner =
        SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(b), leaf(c)]);
    inner.weights = vec![
        SizeWeight::new(SizeConstraint::Flex(u32::MAX)),
        SizeWeight::new(SizeConstraint::Flex(5)),
    ];
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutNode::Split(inner)],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(outer) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(outer.children.len(), 2);
    assert_eq!(normalized, tree);
    assert_eq!(normalized.leaf_panes(), [a, b, c]);
}

/// Four horizontal child splits whose inner weight sums each reach
/// `u32::MAX`, so the product of the four factors fills most of a `u128`.
/// `extra` names the plain sibling placed before them, or `None` for no
/// sibling.
fn near_u128_product_tree(extra: Option<(PaneId, Weight)>) -> (LayoutNode, Vec<PaneId>) {
    let mut children = Vec::new();
    let mut weights = Vec::new();
    let mut panes = Vec::new();
    if let Some((pane, share)) = extra {
        panes.push(pane);
        children.push(leaf(pane));
        weights.push(flex(share));
    }
    for _ in 0..4 {
        let (x, y) = (PaneId::new(), PaneId::new());
        panes.extend([x, y]);
        let mut inner =
            SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(x), leaf(y)]);
        inner.weights = vec![flex(u32::MAX - 1), flex(1)];
        children.push(LayoutNode::Split(inner));
        weights.push(flex(u32::MAX));
    }
    (row(children, weights), panes)
}

#[test]
fn merge_is_skipped_when_a_rescaled_inner_share_overflows_u128() {
    // The four factors multiply to (u32::MAX)^4, which fits u128, but an
    // inner share rescaled by its slot weight and that product does not.
    let (tree, panes) = near_u128_product_tree(None);

    let normalized = normalize(&tree, &live(&panes)).unwrap();

    assert_eq!(normalized, tree, "the split stays nested");
}

#[test]
fn merge_is_skipped_when_a_kept_siblings_rescale_overflows_u128() {
    // The kept sibling's own share is multiplied by the whole product, which
    // is already past u128::MAX / 2.
    let a = PaneId::new();
    let (tree, panes) = near_u128_product_tree(Some((a, 2)));

    let normalized = normalize(&tree, &live(&panes)).unwrap();

    assert_eq!(normalized, tree, "the split stays nested");
}

#[test]
fn merge_is_skipped_when_the_slot_weight_carries_a_min_overlay() {
    // The nested split's own weights are plain flex, but the slot that
    // holds the split in the outer split carries a min overlay — not a
    // plain flex share — so `plain_flex` rejects it and the merge aborts.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let mut outer = SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(a), inner]);
    outer.weights[1].min = Some(10);
    let tree = LayoutNode::Split(outer);

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(result) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(result.children.len(), 2);
    assert_eq!(normalized, tree);
}

#[test]
fn canonical_weight_clamps_every_zero_variant_up_to_one() {
    let panes: Vec<PaneId> = (0..4).map(|_| PaneId::new()).collect();
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        panes.iter().map(|&p| leaf(p)).collect(),
    );
    split.weights[0].primary = SizeConstraint::Percent(0);
    split.weights[1].primary = SizeConstraint::Fixed(0);
    split.weights[2].primary = SizeConstraint::Min(0);
    split.weights[3].primary = SizeConstraint::Preferred(0);
    let tree = LayoutNode::Split(split);

    let normalized = normalize(&tree, &live(&panes)).unwrap();
    let LayoutNode::Split(result) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(result.weights[0].primary, SizeConstraint::Percent(1));
    assert_eq!(result.weights[1].primary, SizeConstraint::Fixed(1));
    assert_eq!(result.weights[2].primary, SizeConstraint::Min(1));
    assert_eq!(result.weights[3].primary, SizeConstraint::Preferred(1));
}

#[test]
fn an_already_canonical_tree_is_returned_unchanged() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), column],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(normalized, tree);
}

#[test]
fn a_dead_last_active_stack_member_hands_off_to_the_new_last_member() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 2));

    // The expanded member was the last one and it died. No member sits at
    // or after its index any more, so the new last member expands.
    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    let LayoutNode::Split(stack) = &normalized else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = (0..stack.children.len())
        .map(|index| stack.is_collapsed(index))
        .collect();
    assert_eq!(collapsed, [true, false]);
    assert_eq!(normalized.leaf_panes(), [a, b]);
}

#[test]
fn a_live_lone_pane_is_returned_as_is() {
    let a = PaneId::new();
    assert_eq!(
        normalize(&LayoutNode::Pane(a), &live(&[a])),
        Some(LayoutNode::Pane(a))
    );
}

#[test]
fn a_dead_lone_pane_normalizes_to_nothing() {
    let a = PaneId::new();
    assert_eq!(normalize(&LayoutNode::Pane(a), &HashSet::new()), None);
}

#[test]
fn an_out_of_range_stack_active_index_expands_the_last_member() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b], 0);
    stack.active = 9;
    let tree = LayoutNode::Split(stack);

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    assert_eq!(
        normalized,
        LayoutNode::Split(SplitNode::stack(vec![a, b], 1))
    );
}

#[test]
fn extra_weights_past_the_children_are_dropped() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut split =
        SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(a), leaf(b)]);
    split.weights.push(flex(7));
    let tree = LayoutNode::Split(split);

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    assert_eq!(
        normalized,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![leaf(a), leaf(b)],
        ))
    );
}

#[test]
fn a_directional_split_resets_its_active_index() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut split = SplitNode::with_equal_weights(SplitDirection::Vertical, vec![leaf(a), leaf(b)]);
    split.active = 1;
    let tree = LayoutNode::Split(split);

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    assert_eq!(
        normalized,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![leaf(a), leaf(b)],
        ))
    );
}

#[test]
fn three_nested_same_direction_splits_flatten_into_one() {
    // h(a, h(b, h(c, d))) flattens to h(a:4, b:2, c:1, d:1): a keeps its
    // half, b its quarter, c and d their eighth each.
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let innermost = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(c), leaf(d)],
    ));
    let middle = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), innermost],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), middle],
    ));

    let before = solve(&tree, tab());
    let normalized = normalize(&tree, &live(&[a, b, c, d])).unwrap();
    assert_eq!(solve(&normalized, tab()).panes, before.panes);
    assert_eq!(
        normalized,
        row(
            vec![leaf(a), leaf(b), leaf(c), leaf(d)],
            vec![flex(4), flex(2), flex(1), flex(1)]
        )
    );
}

#[test]
fn merged_shares_keep_unequal_proportions() {
    // h(a:1, h(b:3, c:1):2) flattens to h(a:4, b:6, c:2): a keeps a third,
    // b a half, c a sixth. Over 96 columns that is 32, 48 and 16.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = row(vec![leaf(b), leaf(c)], vec![flex(3), flex(1)]);
    let tree = row(vec![leaf(a), inner], vec![flex(1), flex(2)]);
    let wide = Rect::at_origin(Size { cols: 96, rows: 24 });

    let before = solve(&tree, wide);
    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(solve(&normalized, wide).panes, before.panes);
    assert_eq!(
        normalized,
        row(
            vec![leaf(a), leaf(b), leaf(c)],
            vec![flex(4), flex(6), flex(2)]
        )
    );
}

#[test]
fn merge_is_skipped_when_a_kept_sibling_is_not_a_plain_flex_share() {
    // The nested pair is plain flex, but sibling a claims a percentage.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let tree = row(
        vec![leaf(a), inner],
        vec![SizeWeight::new(SizeConstraint::Percent(50)), flex(1)],
    );

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(normalized, tree);
}

#[test]
fn merge_is_skipped_when_a_rescaled_share_would_exceed_the_weight_maximum() {
    // a's share of u32::MAX doubled by the inner pair's factor of 2 does
    // not fit a weight, so the pair stays nested.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let tree = row(vec![leaf(a), inner], vec![flex(u32::MAX), flex(1)]);

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(normalized, tree);
}

#[test]
fn a_dead_leaf_inside_a_nested_split_collapses_it_into_the_parent() {
    // h(a, h(b, c)) with c dead: the inner pair collapses to b, and b takes
    // the pair's slot weight.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(b), leaf(c)],
    ));
    let tree = row(vec![leaf(a), inner], vec![flex(1), flex(3)]);

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    assert_eq!(
        normalized,
        row(vec![leaf(a), leaf(b)], vec![flex(1), flex(3)])
    );
}

#[test]
fn a_stack_inside_a_directional_split_stays_nested() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 1));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), stack],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(normalized, tree);
}

#[test]
fn valid_overlays_and_the_resize_offset_pass_through_unchanged() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = row(
        vec![leaf(a), leaf(b)],
        vec![
            SizeWeight {
                primary: SizeConstraint::Percent(100),
                min: Some(5),
                preferred: Some(7),
                resize_delta: -3,
            },
            SizeWeight {
                primary: SizeConstraint::Fixed(u16::MAX),
                min: None,
                preferred: None,
                resize_delta: i32::MAX,
            },
        ],
    );

    let normalized = normalize(&tree, &live(&[a, b])).unwrap();
    assert_eq!(normalized, tree);
}
