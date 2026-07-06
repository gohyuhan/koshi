//! Tests for layout tree structure and navigation.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::PaneId;

use super::*;

/// Helper to create a layout child wrapping a single pane.
fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

/// One pane beside a vertical pair:
///
/// ```text
/// ┌─────┬─────┐
/// │  a  │  b  │
/// │     ├─────┤
/// │     │  c  │
/// └─────┴─────┘
/// ```
fn nested_tree(a: PaneId, b: PaneId, c: PaneId) -> LayoutNode {
    let right = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(right)],
    ))
}

#[test]
fn three_way_tile_holds_children_in_order() {
    let panes = [PaneId::new(), PaneId::new(), PaneId::new()];
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        panes.iter().map(|&pane| leaf(pane)).collect(),
    ));
    assert_eq!(tree.leaf_panes(), panes);
}

#[test]
fn nested_tree_lists_leaves_depth_first() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested_tree(a, b, c);
    assert_eq!(tree.leaf_panes(), [a, b, c]);
    assert!(tree.contains_pane(b));
    assert!(!tree.contains_pane(PaneId::new()));
}

#[test]
fn stack_expands_exactly_the_active_child() {
    let panes = vec![PaneId::new(), PaneId::new(), PaneId::new()];
    let stack = SplitNode::stack(panes.clone(), 1);

    assert_eq!(stack.direction, SplitDirection::Stacked);
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = stack.children.iter().map(|c| c.collapsed).collect();
    assert_eq!(collapsed, [true, false, true]);
    assert_eq!(stack.children.len(), stack.weights.len());
}

#[test]
fn stack_with_one_child_is_representable() {
    let pane = PaneId::new();
    let stack = SplitNode::stack(vec![pane], 0);
    assert_eq!(stack.children.len(), 1);
    assert_eq!(stack.active, 0);
    assert!(!stack.children[0].collapsed);
    assert_eq!(LayoutNode::Split(stack).leaf_panes(), [pane]);
}

#[test]
fn stack_clamps_out_of_bounds_active() {
    let stack = SplitNode::stack(vec![PaneId::new(), PaneId::new()], 9);
    assert_eq!(stack.active, 1);
}

#[test]
fn mixed_tree_roundtrips_through_serde() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    // Nested splits with a stack on one side, exercising every node kind.
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(stack)],
    ));

    let json = serde_json::to_string(&tree).expect("serialize");
    let back: LayoutNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(tree, back);
}

#[test]
fn clone_is_independent_of_the_original() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = nested_tree(a, b, c);
    let snapshot = tree.clone();

    // Mutate the original deeply: flip the inner split's active child and
    // push another child onto the outer split.
    if let LayoutNode::Split(split) = &mut tree {
        split.active = 1;
        split.children.push(leaf(PaneId::new()));
        split.weights.push(SizeWeight::default());
    }

    assert_ne!(tree, snapshot);
    assert_eq!(snapshot, nested_tree(a, b, c));
}
