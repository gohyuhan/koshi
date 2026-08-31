//! Tests for layout tree structure and navigation.

use koshi_core::geometry::{Direction, SplitDirection};
use koshi_core::ids::PaneId;
use serde_json::json;

use super::*;

/// Wraps a pane id in a leaf node.
fn leaf(pane: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane)
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
        vec![leaf(a), right],
    ))
}

/// A stack of `a` and `b` inside the second child of a horizontal split:
/// `horizontal(outside, stack(a, b))`, with `a` expanded.
fn stack_beside_a_pane(outside: PaneId, a: PaneId, b: PaneId) -> LayoutNode {
    let stack = LayoutNode::Split(SplitNode::stack(vec![a, b], 0));
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(outside), stack],
    ))
}

/// A stack whose expanded second member is itself a stack:
/// `stack(a collapsed, stack(b expanded, c))`.
fn nested_stacks(a: PaneId, b: PaneId, c: PaneId) -> SplitNode {
    SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(a),
            LayoutNode::Split(SplitNode::stack(vec![b, c], 0)),
        ],
        weights: vec![SizeWeight::default(); 2],
        active: 1,
    }
}

/// The pane id whose UUID is `uuid`.
fn fixed_pane(uuid: &str) -> PaneId {
    serde_json::from_value(json!(uuid)).expect("a valid UUID")
}

#[test]
fn split_axis_maps_left_right_to_horizontal_and_up_down_to_vertical() {
    assert_eq!(split_axis(Direction::Left), SplitDirection::Horizontal);
    assert_eq!(split_axis(Direction::Right), SplitDirection::Horizontal);
    assert_eq!(split_axis(Direction::Up), SplitDirection::Vertical);
    assert_eq!(split_axis(Direction::Down), SplitDirection::Vertical);
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
fn a_bare_pane_is_its_own_only_leaf() {
    let pane = PaneId::new();
    let tree = LayoutNode::Pane(pane);
    assert_eq!(tree.leaf_panes(), [pane]);
    assert!(tree.contains_pane(pane));
    assert!(!tree.contains_pane(PaneId::new()));
}

#[test]
fn an_empty_split_has_no_leaves() {
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    assert_eq!(empty.leaf_panes(), []);
    assert!(!empty.contains_pane(PaneId::new()));
}

#[test]
fn first_leaf_of_a_bare_pane_is_that_pane() {
    let pane = PaneId::new();
    assert_eq!(LayoutNode::Pane(pane).first_leaf(), Some(pane));
}

#[test]
fn first_leaf_of_a_nested_tree_is_the_first_leaf_in_layout_order() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested_tree(a, b, c);
    assert_eq!(tree.first_leaf(), Some(a));
    assert_eq!(tree.node_at(&[1]).first_leaf(), Some(b));
}

#[test]
fn first_leaf_of_an_empty_split_is_none() {
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));
    assert_eq!(empty.first_leaf(), None);
}

#[test]
fn first_leaf_skips_an_empty_split_before_a_pane() {
    let pane = PaneId::new();
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![empty, leaf(pane)],
    ));
    assert_eq!(tree.first_leaf(), Some(pane));
}

#[test]
fn path_to_a_bare_pane_is_empty() {
    let pane = PaneId::new();
    assert_eq!(LayoutNode::Pane(pane).path_to(pane), Some(Vec::new()));
}

#[test]
fn path_to_lists_the_child_index_taken_at_each_split() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested_tree(a, b, c);
    assert_eq!(tree.path_to(a), Some(vec![0]));
    assert_eq!(tree.path_to(b), Some(vec![1, 0]));
    assert_eq!(tree.path_to(c), Some(vec![1, 1]));
}

#[test]
fn path_to_a_missing_pane_is_none() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    assert_eq!(nested_tree(a, b, c).path_to(PaneId::new()), None);
    assert_eq!(LayoutNode::Pane(a).path_to(b), None);
}

#[test]
fn node_at_walks_a_path_from_the_root_to_the_leaf() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested_tree(a, b, c);
    assert_eq!(tree.node_at(&[]), &tree);
    assert_eq!(tree.node_at(&[0]), &LayoutNode::Pane(a));
    assert_eq!(tree.node_at(&[1, 0]), &LayoutNode::Pane(b));
    assert_eq!(tree.node_at(&[1, 1]), &LayoutNode::Pane(c));
}

#[test]
fn node_at_mut_replaces_the_leaf_at_the_path() {
    let (a, b, c, d) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = nested_tree(a, b, c);
    *tree.node_at_mut(&[1, 1]) = LayoutNode::Pane(d);
    assert_eq!(tree.leaf_panes(), [a, b, d]);
}

#[test]
#[should_panic(expected = "path was built over this tree")]
fn node_at_panics_when_the_path_steps_into_a_pane() {
    let pane = LayoutNode::Pane(PaneId::new());
    let _ = pane.node_at(&[0]);
}

#[test]
fn split_at_returns_the_split_a_path_prefix_ends_on() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = nested_tree(a, b, c);
    let inner = SplitNode::with_equal_weights(SplitDirection::Vertical, vec![leaf(b), leaf(c)]);
    assert_eq!(tree.split_at(&[1]), &inner);

    tree.split_at_mut(&[1]).direction = SplitDirection::Horizontal;
    assert_eq!(tree.split_at(&[1]).direction, SplitDirection::Horizontal);
}

#[test]
#[should_panic(expected = "path was built over this tree")]
fn split_at_panics_when_the_path_ends_on_a_pane() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested_tree(a, b, c);
    let _ = tree.split_at(&[0]);
}

#[test]
fn stack_containing_mut_of_a_bare_pane_is_none() {
    let pane = PaneId::new();
    assert_eq!(LayoutNode::Pane(pane).stack_containing_mut(pane), None);
}

#[test]
fn stack_containing_mut_of_a_pane_in_directional_splits_only_is_none() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = nested_tree(a, b, c);
    assert_eq!(tree.stack_containing_mut(c), None);
}

#[test]
fn stack_containing_mut_of_a_missing_pane_is_none() {
    let (outside, a, b) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = stack_beside_a_pane(outside, a, b);
    assert_eq!(tree.stack_containing_mut(PaneId::new()), None);
}

#[test]
fn stack_containing_mut_finds_the_stack_under_a_directional_split() {
    let (outside, a, b) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = stack_beside_a_pane(outside, a, b);
    let mut expected = SplitNode::stack(vec![a, b], 0);
    assert_eq!(tree.stack_containing_mut(b), Some(&mut expected));
    assert_eq!(tree.stack_containing_mut(outside), None);
}

#[test]
fn stack_containing_mut_picks_the_innermost_of_nested_stacks() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let outer = nested_stacks(a, b, c);
    let mut tree = LayoutNode::Split(outer.clone());

    let mut inner = SplitNode::stack(vec![b, c], 0);
    assert_eq!(tree.stack_containing_mut(c), Some(&mut inner));

    let mut expected_outer = outer;
    assert_eq!(tree.stack_containing_mut(a), Some(&mut expected_outer));
}

#[test]
fn stack_containing_mut_edits_the_tree_in_place() {
    let (outside, a, b) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = stack_beside_a_pane(outside, a, b);
    let stack = tree.stack_containing_mut(a).expect("a lives in a stack");
    stack.active = 1;
    assert_eq!(tree.split_at(&[1]).active, 1);
}

#[test]
fn stack_expands_exactly_the_active_child() {
    let panes = vec![PaneId::new(), PaneId::new(), PaneId::new()];
    let stack = SplitNode::stack(panes.clone(), 1);

    assert_eq!(stack.direction, SplitDirection::Stacked);
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = (0..stack.children.len())
        .map(|index| stack.is_collapsed(index))
        .collect();
    assert_eq!(collapsed, [true, false, true]);
    assert_eq!(stack.weights, [SizeWeight::default(); 3]);
}

#[test]
fn stack_with_one_child_is_representable() {
    let pane = PaneId::new();
    let stack = SplitNode::stack(vec![pane], 0);
    assert_eq!(
        stack,
        SplitNode {
            direction: SplitDirection::Stacked,
            children: vec![leaf(pane)],
            weights: vec![SizeWeight::default()],
            active: 0,
        }
    );
    assert_eq!(LayoutNode::Split(stack).leaf_panes(), [pane]);
}

#[test]
fn stack_of_no_panes_is_empty_with_active_zero() {
    let empty = SplitNode {
        direction: SplitDirection::Stacked,
        children: Vec::new(),
        weights: Vec::new(),
        active: 0,
    };
    assert_eq!(SplitNode::stack(Vec::new(), 0), empty);
    assert_eq!(SplitNode::stack(Vec::new(), 5), empty);
}

#[test]
fn stack_clamps_out_of_bounds_active() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let stack = SplitNode::stack(vec![a, b], 9);
    assert_eq!(stack.active, 1);
    assert_eq!(stack, SplitNode::stack(vec![a, b], 1));
}

#[test]
fn with_equal_weights_of_no_children_has_no_weights() {
    let split = SplitNode::with_equal_weights(SplitDirection::Vertical, Vec::new());
    assert_eq!(
        split,
        SplitNode {
            direction: SplitDirection::Vertical,
            children: Vec::new(),
            weights: Vec::new(),
            active: 0,
        }
    );
}

#[test]
fn with_equal_weights_activates_the_first_child_and_collapses_the_rest() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let split = SplitNode::with_equal_weights(SplitDirection::Stacked, vec![leaf(a), leaf(b)]);
    let flags: Vec<bool> = (0..split.children.len())
        .map(|index| split.is_collapsed(index))
        .collect();
    assert_eq!(flags, [false, true]);
    assert_eq!(split.weights, [SizeWeight::default(); 2]);
    assert_eq!(split.active, 0);
}

#[test]
fn a_directional_split_collapses_no_child() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let split = SplitNode::with_equal_weights(SplitDirection::Horizontal, vec![leaf(a), leaf(b)]);
    assert!(!split.is_collapsed(0));
    assert!(!split.is_collapsed(1));
}

#[test]
fn an_out_of_range_active_index_collapses_every_child_but_the_last() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b], 0);
    stack.active = 9;
    assert!(stack.is_collapsed(0));
    assert!(!stack.is_collapsed(1));
}

#[test]
fn a_child_index_past_the_last_child_is_not_collapsed() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let stack = SplitNode::stack(vec![a, b], 0);
    assert!(!stack.is_collapsed(2));

    let empty = SplitNode::stack(Vec::new(), 0);
    assert!(!empty.is_collapsed(0));
}

#[test]
fn active_index_clamps_past_the_last_child_and_is_zero_for_an_empty_split() {
    let mut stack = SplitNode::stack(vec![PaneId::new(), PaneId::new()], 0);
    assert_eq!(stack.active_index(), 0);
    stack.active = 1;
    assert_eq!(stack.active_index(), 1);
    stack.active = 2;
    assert_eq!(stack.active_index(), 1);
    stack.active = usize::MAX;
    assert_eq!(stack.active_index(), 1);

    let empty = SplitNode::stack(Vec::new(), 0);
    assert_eq!(empty.active_index(), 0);
}

#[test]
fn mixed_tree_roundtrips_through_serde() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    // Nested splits with a stack on one side, exercising every node kind.
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), stack],
    ));

    let json = serde_json::to_string(&tree).expect("serialize");
    let back: LayoutNode = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(tree, back);
}

#[test]
fn a_pane_serializes_as_a_tagged_uuid() {
    let pane = fixed_pane("00000000-0000-0000-0000-000000000001");
    assert_eq!(
        serde_json::to_value(LayoutNode::Pane(pane)).expect("serialize"),
        json!({ "Pane": "00000000-0000-0000-0000-000000000001" })
    );
}

#[test]
fn a_stack_serializes_field_by_field() {
    let a = fixed_pane("00000000-0000-0000-0000-000000000001");
    let b = fixed_pane("00000000-0000-0000-0000-000000000002");
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 1));
    let default_weight = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    assert_eq!(
        serde_json::to_value(&tree).expect("serialize"),
        json!({
            "Split": {
                "direction": "Stacked",
                "children": [
                    { "Pane": "00000000-0000-0000-0000-000000000001" },
                    { "Pane": "00000000-0000-0000-0000-000000000002" }
                ],
                "weights": [default_weight.clone(), default_weight],
                "active": 1
            }
        })
    );
}

#[test]
fn a_deserialized_active_index_past_the_last_child_is_kept_and_clamped_on_read() {
    let default_weight = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let split: SplitNode = serde_json::from_value(json!({
        "direction": "Stacked",
        "children": [
            { "Pane": "00000000-0000-0000-0000-000000000001" },
            { "Pane": "00000000-0000-0000-0000-000000000002" }
        ],
        "weights": [default_weight.clone(), default_weight],
        "active": 9
    }))
    .expect("deserialize");
    assert_eq!(split.active, 9);
    assert_eq!(split.active_index(), 1);
}

#[test]
fn a_stored_child_wrapped_in_a_node_record_reads_as_the_node_itself() {
    let default_weight = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let wrapped: SplitNode = serde_json::from_value(json!({
        "direction": "Horizontal",
        "children": [
            { "node": { "Pane": "00000000-0000-0000-0000-000000000001" } },
            { "node": { "Split": {
                "direction": "Vertical",
                "children": [{ "node": { "Pane": "00000000-0000-0000-0000-000000000002" } }],
                "weights": [default_weight.clone()],
                "active": 0
            } } }
        ],
        "weights": [default_weight.clone(), default_weight.clone()],
        "active": 0
    }))
    .expect("deserialize");
    let bare: SplitNode = serde_json::from_value(json!({
        "direction": "Horizontal",
        "children": [
            { "Pane": "00000000-0000-0000-0000-000000000001" },
            { "Split": {
                "direction": "Vertical",
                "children": [{ "Pane": "00000000-0000-0000-0000-000000000002" }],
                "weights": [default_weight.clone()],
                "active": 0
            } }
        ],
        "weights": [default_weight.clone(), default_weight],
        "active": 0
    }))
    .expect("deserialize");
    assert_eq!(wrapped, bare);
    assert_eq!(
        wrapped.children,
        [
            LayoutNode::Pane(fixed_pane("00000000-0000-0000-0000-000000000001")),
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![LayoutNode::Pane(fixed_pane(
                    "00000000-0000-0000-0000-000000000002"
                ))],
            )),
        ]
    );
}

#[test]
fn a_stored_child_that_is_neither_a_node_nor_a_node_record_is_refused() {
    let default_weight = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let refused = serde_json::from_value::<SplitNode>(json!({
        "direction": "Horizontal",
        "children": [{ "slot": { "Pane": "00000000-0000-0000-0000-000000000001" } }],
        "weights": [default_weight],
        "active": 0
    }));
    assert!(refused.is_err());
}

#[test]
fn a_stored_child_carrying_a_collapsed_flag_still_reads_and_drops_it() {
    let default_weight = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let split: SplitNode = serde_json::from_value(json!({
        "direction": "Stacked",
        "children": [
            {
                "node": { "Pane": "00000000-0000-0000-0000-000000000001" },
                "collapsed": false
            },
            {
                "node": { "Pane": "00000000-0000-0000-0000-000000000002" },
                "collapsed": true
            }
        ],
        "weights": [default_weight.clone(), default_weight],
        "active": 0
    }))
    .expect("deserialize");
    // `active` alone decides the collapse, so the stored flags are ignored:
    // the stored file says child 0 is expanded and child 1 collapsed, and
    // `active` 0 says the same.
    assert!(!split.is_collapsed(0));
    assert!(split.is_collapsed(1));
    assert_eq!(
        split.children,
        [
            LayoutNode::Pane(fixed_pane("00000000-0000-0000-0000-000000000001")),
            LayoutNode::Pane(fixed_pane("00000000-0000-0000-0000-000000000002")),
        ]
    );
}

#[test]
fn clone_is_independent_of_the_original() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tree = nested_tree(a, b, c);
    let snapshot = tree.clone();

    // Mutate the original: set the root split's active child and push
    // another child onto it.
    if let LayoutNode::Split(split) = &mut tree {
        split.active = 1;
        split.children.push(leaf(PaneId::new()));
        split.weights.push(SizeWeight::default());
    }

    assert_ne!(tree, snapshot);
    assert_eq!(snapshot, nested_tree(a, b, c));
}
