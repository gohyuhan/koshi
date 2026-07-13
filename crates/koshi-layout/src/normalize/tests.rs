//! Tests for layout normalization: cleanup after edits and snapshot restore.

use koshi_core::geometry::{Point, Rect, Size};

use super::*;
use crate::solver::solve;

/// Wraps a pane ID in a leaf node child wrapper.
fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

/// Converts a pane ID slice into the set of live panes.
fn live(panes: &[PaneId]) -> HashSet<PaneId> {
    panes.iter().copied().collect()
}

/// Returns a standard 80×24 tab rectangle for test layouts.
fn tab() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 })
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
        vec![LayoutChild::new(inner)],
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
        vec![leaf(a), LayoutChild::new(inner)],
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
}

#[test]
fn merge_is_skipped_when_a_resize_offset_is_present() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut inner =
        SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(b), leaf(c)]);
    inner.weights[0].resize_delta = 4;
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(LayoutNode::Split(inner))],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(outer) = &normalized else {
        panic!("expected a split");
    };
    // The presence of resize_delta prevents merging; the nested split survives with its offset.
    assert_eq!(outer.children.len(), 2);
    assert!(matches!(outer.children[1].node, LayoutNode::Split(_)));
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
        vec![leaf(a), LayoutChild::new(inner)],
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
        vec![LayoutChild::new(inner_h)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(wrapper)],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(flat) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(flat.children.len(), 3);
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

    // a dies; c is still the expanded member but now sits at index 1.
    let normalized = normalize(&tree, &live(&[b, c])).unwrap();
    let LayoutNode::Split(stack) = &normalized else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = stack.children.iter().map(|child| child.collapsed).collect();
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
    let collapsed: Vec<bool> = stack.children.iter().map(|child| child.collapsed).collect();
    assert_eq!(collapsed, [true, false]);
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
    assert_eq!(split.weights.len(), 2);
    assert_eq!(split.weights[1], SizeWeight::default());
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
        vec![leaf(a), LayoutChild::new(inner), LayoutChild::new(stack)],
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
        vec![leaf(a), LayoutChild::new(LayoutNode::Split(inner))],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(outer) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(outer.children.len(), 2);
    assert!(matches!(outer.children[1].node, LayoutNode::Split(_)));
    assert_eq!(normalized.leaf_panes(), [a, b, c]);
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
    let mut outer = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(inner)],
    );
    outer.weights[1] = outer.weights[1].with_min(10).unwrap();
    let tree = LayoutNode::Split(outer);

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    let LayoutNode::Split(result) = &normalized else {
        panic!("expected a split");
    };
    assert_eq!(result.children.len(), 2);
    assert!(matches!(result.children[1].node, LayoutNode::Split(_)));
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
        vec![leaf(a), LayoutChild::new(column)],
    ));

    let normalized = normalize(&tree, &live(&[a, b, c])).unwrap();
    assert_eq!(normalized, tree);
}
