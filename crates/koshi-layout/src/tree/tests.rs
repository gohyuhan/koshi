//! Tests for layout tree structure and navigation.

use koshi_core::geometry::{Direction, SplitDirection};
use koshi_core::ids::PaneId;
use serde_json::json;

use super::*;

/// Wraps a pane id in a leaf node.
fn build_leaf_node(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
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
fn build_nested_layout_tree(
    left_pane_id: PaneId,
    upper_right_pane_id: PaneId,
    lower_right_pane_id: PaneId,
) -> LayoutNode {
    let right_subtree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(upper_right_pane_id),
            build_leaf_node(lower_right_pane_id),
        ],
    ));
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), right_subtree],
    ))
}

/// A stack of `a` and `b` inside the second child of a horizontal split:
/// `horizontal(outside, stack(a, b))`, with `a` expanded.
fn build_stack_beside_pane(
    outside_pane_id: PaneId,
    first_stack_pane_id: PaneId,
    second_stack_pane_id: PaneId,
) -> LayoutNode {
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_stack_pane_id, second_stack_pane_id],
        0,
    ));
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(outside_pane_id), stack],
    ))
}

/// A stack whose expanded second member is itself a stack:
/// `stack(a collapsed, stack(b expanded, c))`.
fn build_nested_stack_layout(
    first_pane_id: PaneId,
    nested_first_pane_id: PaneId,
    nested_second_pane_id: PaneId,
) -> SplitNode {
    SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                vec![nested_first_pane_id, nested_second_pane_id],
                0,
            )),
        ],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 1,
    }
}

/// The pane id whose UUID is `uuid_text`.
fn build_fixed_pane_id(uuid_text: &str) -> PaneId {
    serde_json::from_value(json!(uuid_text)).expect("a valid UUID")
}

#[test]
fn compute_split_direction_maps_each_cardinal_direction_to_its_axis() {
    assert_eq!(
        compute_split_direction(Direction::Left),
        SplitDirection::Horizontal
    );
    assert_eq!(
        compute_split_direction(Direction::Right),
        SplitDirection::Horizontal
    );
    assert_eq!(
        compute_split_direction(Direction::Up),
        SplitDirection::Vertical
    );
    assert_eq!(
        compute_split_direction(Direction::Down),
        SplitDirection::Vertical
    );
}

#[test]
fn three_way_tile_holds_children_in_order() {
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        pane_ids
            .iter()
            .map(|&pane_id| build_leaf_node(pane_id))
            .collect(),
    ));
    assert_eq!(layout_tree.list_leaf_pane_ids(), pane_ids);
}

#[test]
fn nested_layout_tree_lists_leaves_depth_first() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    assert_eq!(
        layout_tree.list_leaf_pane_ids(),
        [left_pane_id, upper_right_pane_id, lower_right_pane_id]
    );
    assert!(layout_tree.contains_pane(upper_right_pane_id));
    assert!(!layout_tree.contains_pane(PaneId::new()));
}

#[test]
fn a_bare_pane_is_its_own_only_leaf() {
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Pane(pane_id);
    assert_eq!(layout_tree.list_leaf_pane_ids(), [pane_id]);
    assert!(layout_tree.contains_pane(pane_id));
    assert!(!layout_tree.contains_pane(PaneId::new()));
}

#[test]
fn an_empty_split_has_no_leaves() {
    let empty_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    assert_eq!(empty_split.list_leaf_pane_ids(), []);
    assert!(!empty_split.contains_pane(PaneId::new()));
}

#[test]
fn first_leaf_of_a_bare_pane_is_that_pane() {
    let pane_id = PaneId::new();
    assert_eq!(
        LayoutNode::Pane(pane_id).find_first_leaf_pane_id(),
        Some(pane_id)
    );
}

#[test]
fn first_leaf_of_a_nested_layout_tree_is_the_first_leaf_in_layout_order() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    assert_eq!(layout_tree.find_first_leaf_pane_id(), Some(left_pane_id));
    assert_eq!(
        layout_tree.get_node_at_path(&[1]).find_first_leaf_pane_id(),
        Some(upper_right_pane_id)
    );
}

#[test]
fn first_leaf_of_an_empty_split_is_none() {
    let empty_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));
    assert_eq!(empty_split.find_first_leaf_pane_id(), None);
}

#[test]
fn first_leaf_skips_an_empty_split_before_a_pane() {
    let pane_id = PaneId::new();
    let empty_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![empty_split, build_leaf_node(pane_id)],
    ));
    assert_eq!(layout_tree.find_first_leaf_pane_id(), Some(pane_id));
}

#[test]
fn path_to_a_bare_pane_is_empty() {
    let pane_id = PaneId::new();
    assert_eq!(
        LayoutNode::Pane(pane_id).find_pane_path(pane_id),
        Some(Vec::new())
    );
}

#[test]
fn find_pane_path_lists_each_child_index_taken_at_each_layout_split() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    assert_eq!(layout_tree.find_pane_path(left_pane_id), Some(vec![0]));
    assert_eq!(
        layout_tree.find_pane_path(upper_right_pane_id),
        Some(vec![1, 0])
    );
    assert_eq!(
        layout_tree.find_pane_path(lower_right_pane_id),
        Some(vec![1, 1])
    );
}

#[test]
fn find_pane_path_returns_none_for_a_missing_pane() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    assert_eq!(
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id)
            .find_pane_path(PaneId::new()),
        None
    );
    assert_eq!(
        LayoutNode::Pane(left_pane_id).find_pane_path(upper_right_pane_id),
        None
    );
}

#[test]
fn get_node_at_path_walks_from_the_root_to_the_leaf() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    assert_eq!(layout_tree.get_node_at_path(&[]), &layout_tree);
    assert_eq!(
        layout_tree.get_node_at_path(&[0]),
        &LayoutNode::Pane(left_pane_id)
    );
    assert_eq!(
        layout_tree.get_node_at_path(&[1, 0]),
        &LayoutNode::Pane(upper_right_pane_id)
    );
    assert_eq!(
        layout_tree.get_node_at_path(&[1, 1]),
        &LayoutNode::Pane(lower_right_pane_id)
    );
}

#[test]
fn get_node_at_path_mut_replaces_the_leaf_at_the_path() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id, replacement_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    *layout_tree.get_node_at_path_mut(&[1, 1]) = LayoutNode::Pane(replacement_pane_id);
    assert_eq!(
        layout_tree.list_leaf_pane_ids(),
        [left_pane_id, upper_right_pane_id, replacement_pane_id]
    );
}

#[test]
#[should_panic(expected = "path was built over this tree")]
fn get_node_at_path_panics_when_the_path_steps_into_a_pane() {
    let pane_node = LayoutNode::Pane(PaneId::new());
    let _ = pane_node.get_node_at_path(&[0]);
}

#[test]
fn get_split_at_path_returns_the_split_at_a_path_prefix() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    let expected_inner_split_node = SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(upper_right_pane_id),
            build_leaf_node(lower_right_pane_id),
        ],
    );
    assert_eq!(
        layout_tree.get_split_at_path(&[1]),
        &expected_inner_split_node
    );

    layout_tree.get_split_at_path_mut(&[1]).direction = SplitDirection::Horizontal;
    assert_eq!(
        layout_tree.get_split_at_path(&[1]).direction,
        SplitDirection::Horizontal
    );
}

#[test]
#[should_panic(expected = "path was built over this tree")]
fn get_split_at_path_panics_when_the_path_ends_on_a_pane() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    let _ = layout_tree.get_split_at_path(&[0]);
}

#[test]
fn find_containing_stack_mut_returns_none_for_a_bare_pane() {
    let pane_id = PaneId::new();
    assert_eq!(
        LayoutNode::Pane(pane_id).find_containing_stack_mut(pane_id),
        None
    );
}

#[test]
fn find_containing_stack_mut_returns_none_for_directional_splits() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_nested_layout_tree(left_pane_id, upper_right_pane_id, lower_right_pane_id);
    assert_eq!(
        layout_tree.find_containing_stack_mut(lower_right_pane_id),
        None
    );
}

#[test]
fn find_containing_stack_mut_returns_none_for_a_missing_pane() {
    let (outside_pane_id, first_stack_pane_id, second_stack_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_stack_beside_pane(outside_pane_id, first_stack_pane_id, second_stack_pane_id);
    assert_eq!(layout_tree.find_containing_stack_mut(PaneId::new()), None);
}

#[test]
fn find_containing_stack_mut_finds_a_stack_under_a_directional_split() {
    let (outside_pane_id, first_stack_pane_id, second_stack_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_stack_beside_pane(outside_pane_id, first_stack_pane_id, second_stack_pane_id);
    let mut expected_stack =
        SplitNode::from_stacked_pane_ids(vec![first_stack_pane_id, second_stack_pane_id], 0);
    assert_eq!(
        layout_tree.find_containing_stack_mut(second_stack_pane_id),
        Some(&mut expected_stack)
    );
    assert_eq!(layout_tree.find_containing_stack_mut(outside_pane_id), None);
}

#[test]
fn find_containing_stack_mut_picks_the_innermost_nested_stack() {
    let (outer_pane_id, inner_first_pane_id, inner_second_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let outer_stack =
        build_nested_stack_layout(outer_pane_id, inner_first_pane_id, inner_second_pane_id);
    let mut layout_tree = LayoutNode::Split(outer_stack.clone());

    let mut expected_inner_stack =
        SplitNode::from_stacked_pane_ids(vec![inner_first_pane_id, inner_second_pane_id], 0);
    assert_eq!(
        layout_tree.find_containing_stack_mut(inner_second_pane_id),
        Some(&mut expected_inner_stack)
    );

    let mut expected_outer_stack = outer_stack;
    assert_eq!(
        layout_tree.find_containing_stack_mut(outer_pane_id),
        Some(&mut expected_outer_stack)
    );
}

#[test]
fn find_containing_stack_mut_edits_the_tree_in_place() {
    let (outside_pane_id, first_stack_pane_id, second_stack_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_stack_beside_pane(outside_pane_id, first_stack_pane_id, second_stack_pane_id);
    let stack_node = layout_tree
        .find_containing_stack_mut(first_stack_pane_id)
        .expect("the pane lives in a stack");
    stack_node.active_child_index = 1;
    assert_eq!(layout_tree.get_split_at_path(&[1]).active_child_index, 1);
}

#[test]
fn stack_expands_exactly_the_active_child() {
    let pane_ids = vec![PaneId::new(), PaneId::new(), PaneId::new()];
    let stack_node = SplitNode::from_stacked_pane_ids(pane_ids.clone(), 1);

    assert_eq!(stack_node.direction, SplitDirection::Stacked);
    assert_eq!(stack_node.active_child_index, 1);
    let collapsed_child_flags: Vec<bool> = (0..stack_node.children.len())
        .map(|child_index| stack_node.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, false, true]);
    assert_eq!(stack_node.weights, [SizeWeight::default(); 3]);
}

#[test]
fn stack_with_one_child_is_representable() {
    let pane_id = PaneId::new();
    let stack = SplitNode::from_stacked_pane_ids(vec![pane_id], 0);
    assert_eq!(
        stack,
        SplitNode {
            direction: SplitDirection::Stacked,
            children: vec![build_leaf_node(pane_id)],
            weights: vec![SizeWeight::default()],
            active_child_index: 0,
        }
    );
    assert_eq!(LayoutNode::Split(stack).list_leaf_pane_ids(), [pane_id]);
}

#[test]
fn stack_of_no_panes_is_empty_with_active_zero() {
    let empty_stack = SplitNode {
        direction: SplitDirection::Stacked,
        children: Vec::new(),
        weights: Vec::new(),
        active_child_index: 0,
    };
    assert_eq!(SplitNode::from_stacked_pane_ids(Vec::new(), 0), empty_stack);
    assert_eq!(SplitNode::from_stacked_pane_ids(Vec::new(), 5), empty_stack);
}

#[test]
fn stack_clamps_out_of_bounds_active() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 9);
    assert_eq!(stack.active_child_index, 1);
    assert_eq!(
        stack,
        SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 1)
    );
}

#[test]
fn with_equal_weights_of_no_children_has_no_weights() {
    let split_node = SplitNode::with_equal_weights(SplitDirection::Vertical, Vec::new());
    assert_eq!(
        split_node,
        SplitNode {
            direction: SplitDirection::Vertical,
            children: Vec::new(),
            weights: Vec::new(),
            active_child_index: 0,
        }
    );
}

#[test]
fn with_equal_weights_activates_the_first_child_and_collapses_the_rest() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let split_node = SplitNode::with_equal_weights(
        SplitDirection::Stacked,
        vec![
            build_leaf_node(first_pane_id),
            build_leaf_node(second_pane_id),
        ],
    );
    let collapsed_children: Vec<bool> = (0..split_node.children.len())
        .map(|child_index| split_node.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_children, [false, true]);
    assert_eq!(split_node.weights, [SizeWeight::default(); 2]);
    assert_eq!(split_node.active_child_index, 0);
}

#[test]
fn a_directional_split_collapses_no_child() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(first_pane_id),
            build_leaf_node(second_pane_id),
        ],
    );
    assert!(!split_node.is_child_collapsed(0));
    assert!(!split_node.is_child_collapsed(1));
}

#[test]
fn an_out_of_range_active_index_collapses_every_child_but_the_last() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 0);
    stack.active_child_index = 9;
    assert!(stack.is_child_collapsed(0));
    assert!(!stack.is_child_collapsed(1));
}

#[test]
fn a_child_index_past_the_last_child_is_not_collapsed() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 0);
    assert!(!stack.is_child_collapsed(2));

    let empty_stack = SplitNode::from_stacked_pane_ids(Vec::new(), 0);
    assert!(!empty_stack.is_child_collapsed(0));
}

#[test]
fn active_index_clamps_past_the_last_child_and_is_zero_for_an_empty_split() {
    let mut stack = SplitNode::from_stacked_pane_ids(vec![PaneId::new(), PaneId::new()], 0);
    assert_eq!(stack.get_active_child_index(), 0);
    stack.active_child_index = 1;
    assert_eq!(stack.get_active_child_index(), 1);
    stack.active_child_index = 2;
    assert_eq!(stack.get_active_child_index(), 1);
    stack.active_child_index = usize::MAX;
    assert_eq!(stack.get_active_child_index(), 1);

    let empty_stack = SplitNode::from_stacked_pane_ids(Vec::new(), 0);
    assert_eq!(empty_stack.get_active_child_index(), 0);
}

#[test]
fn mixed_layout_tree_assert_size_weight_round_trips_through_serde() {
    let (left_pane_id, upper_right_pane_id, lower_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    // Nested splits with a stack on one side, exercising every node kind.
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![upper_right_pane_id, lower_right_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack],
    ));

    let serialized_tree_json = serde_json::to_string(&layout_tree).expect("serialize");
    let deserialized_tree: LayoutNode =
        serde_json::from_str(&serialized_tree_json).expect("deserialize");
    assert_eq!(layout_tree, deserialized_tree);
}

#[test]
fn a_pane_serializes_as_a_tagged_uuid() {
    let pane_id = build_fixed_pane_id("00000000-0000-0000-0000-000000000001");
    assert_eq!(
        serde_json::to_value(LayoutNode::Pane(pane_id)).expect("serialize"),
        json!({ "Pane": "00000000-0000-0000-0000-000000000001" })
    );
}

#[test]
fn a_stack_serializes_field_by_field() {
    let first_pane_id = build_fixed_pane_id("00000000-0000-0000-0000-000000000001");
    let second_pane_id = build_fixed_pane_id("00000000-0000-0000-0000-000000000002");
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, second_pane_id],
        1,
    ));
    let default_size_weight_json = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    assert_eq!(
        serde_json::to_value(&layout_tree).expect("serialize"),
        json!({
            "Split": {
                "direction": "Stacked",
                "children": [
                    { "Pane": "00000000-0000-0000-0000-000000000001" },
                    { "Pane": "00000000-0000-0000-0000-000000000002" }
                ],
                "weights": [default_size_weight_json.clone(), default_size_weight_json],
                "active": 1
            }
        })
    );
}

#[test]
fn a_deserialized_active_index_past_the_last_child_is_kept_and_clamped_on_read() {
    let default_size_weight_json = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let split_node: SplitNode = serde_json::from_value(json!({
        "direction": "Stacked",
        "children": [
            { "Pane": "00000000-0000-0000-0000-000000000001" },
            { "Pane": "00000000-0000-0000-0000-000000000002" }
        ],
        "weights": [default_size_weight_json.clone(), default_size_weight_json],
        "active": 9
    }))
    .expect("deserialize");
    assert_eq!(split_node.active_child_index, 9);
    assert_eq!(split_node.get_active_child_index(), 1);
}

#[test]
fn a_stored_child_wrapped_in_a_node_record_reads_as_the_node_itself() {
    let default_size_weight_json = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let wrapped_split: SplitNode = serde_json::from_value(json!({
        "direction": "Horizontal",
        "children": [
            { "node": { "Pane": "00000000-0000-0000-0000-000000000001" } },
            { "node": { "Split": {
                "direction": "Vertical",
                "children": [{ "node": { "Pane": "00000000-0000-0000-0000-000000000002" } }],
                "weights": [default_size_weight_json.clone()],
                "active": 0
            } } }
        ],
        "weights": [default_size_weight_json.clone(), default_size_weight_json.clone()],
        "active": 0
    }))
    .expect("deserialize");
    let bare_split: SplitNode = serde_json::from_value(json!({
        "direction": "Horizontal",
        "children": [
            { "Pane": "00000000-0000-0000-0000-000000000001" },
            { "Split": {
                "direction": "Vertical",
                "children": [{ "Pane": "00000000-0000-0000-0000-000000000002" }],
                "weights": [default_size_weight_json.clone()],
                "active": 0
            } }
        ],
        "weights": [default_size_weight_json.clone(), default_size_weight_json],
        "active": 0
    }))
    .expect("deserialize");
    assert_eq!(wrapped_split, bare_split);
    assert_eq!(
        wrapped_split.children,
        [
            LayoutNode::Pane(build_fixed_pane_id("00000000-0000-0000-0000-000000000001")),
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![LayoutNode::Pane(build_fixed_pane_id(
                    "00000000-0000-0000-0000-000000000002"
                ))],
            )),
        ]
    );
}

#[test]
fn a_stored_child_that_is_neither_a_node_nor_a_node_record_is_refused() {
    let default_size_weight_json = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let deserialization_result = serde_json::from_value::<SplitNode>(json!({
        "direction": "Horizontal",
        "children": [{ "slot": { "Pane": "00000000-0000-0000-0000-000000000001" } }],
        "weights": [default_size_weight_json],
        "active": 0
    }));
    assert!(deserialization_result.is_err());
}

#[test]
fn a_stored_child_carrying_a_collapsed_flag_still_reads_and_drops_it() {
    let default_size_weight_json = json!({
        "primary": { "Flex": 1 },
        "min": null,
        "preferred": null,
        "resize_delta": 0
    });
    let split_node: SplitNode = serde_json::from_value(json!({
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
        "weights": [default_size_weight_json.clone(), default_size_weight_json],
        "active": 0
    }))
    .expect("deserialize");
    // `active` alone decides the collapse, so the stored flags are ignored:
    // the stored file says child 0 is expanded and child 1 collapsed, and
    // `active` 0 says the same.
    assert!(!split_node.is_child_collapsed(0));
    assert!(split_node.is_child_collapsed(1));
    assert_eq!(
        split_node.children,
        [
            LayoutNode::Pane(build_fixed_pane_id("00000000-0000-0000-0000-000000000001")),
            LayoutNode::Pane(build_fixed_pane_id("00000000-0000-0000-0000-000000000002")),
        ]
    );
}

#[test]
fn clone_is_independent_of_the_original() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree = build_nested_layout_tree(first_pane_id, second_pane_id, third_pane_id);
    let original_layout_tree = layout_tree.clone();

    // Mutate the original: set the root split's active child and push
    // another child onto it.
    if let LayoutNode::Split(split_node) = &mut layout_tree {
        split_node.active_child_index = 1;
        split_node.children.push(build_leaf_node(PaneId::new()));
        split_node.weights.push(SizeWeight::default());
    }

    assert_ne!(layout_tree, original_layout_tree);
    assert_eq!(
        original_layout_tree,
        build_nested_layout_tree(first_pane_id, second_pane_id, third_pane_id)
    );
}
