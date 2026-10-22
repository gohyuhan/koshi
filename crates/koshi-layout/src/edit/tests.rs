//! Tests for structural edits: split (directional pane splits), stack (tabbed pane groups),
//! and remove (delete pane from the layout tree).
//!
//! Tests verify that edits produce correct layout tree structure, maintain tiling (no
//! gaps/overlaps), keep a stack's active member correct, and handle edge cases (removing last pane,
//! missing targets).

use koshi_core::geometry::{Point, Rect, Size};
use koshi_test_support::fixtures::build_cell_rect;
use koshi_test_support::layout_assert::check_exact_tiling;

use super::*;
use crate::size::SizeWeight;
use crate::solver::tests::build_pane_sizing;
use crate::solver::{solve_layout, solve_layout_with_sizing};
use crate::test_trees::build_deep_alternating_layout;

/// Wraps a single pane ID as a leaf node ready to insert into a layout tree.
fn build_leaf_node(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
}

/// Creates a split node with two equally-weighted pane children in the given direction.
fn build_equal_split_node(
    direction: SplitDirection,
    first_pane_id: PaneId,
    second_pane_id: PaneId,
) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        vec![
            build_leaf_node(first_pane_id),
            build_leaf_node(second_pane_id),
        ],
    ))
}

/// The split node that replaced the target leaf, wherever it ended up.
fn find_parent_split_containing_pane(layout_tree: &LayoutNode, pane_id: PaneId) -> &SplitNode {
    match layout_tree {
        LayoutNode::Pane(_) => panic!("expected a split in {layout_tree:?}"),
        LayoutNode::Split(split) => {
            if split
                .children
                .iter()
                .any(|child| {
                    matches!(child, LayoutNode::Pane(child_pane_id) if *child_pane_id == pane_id)
                })
            {
                split
            } else {
                split
                    .children
                    .iter()
                    .find_map(|child| {
                        child
                            .has_pane(pane_id)
                            .then(|| find_parent_split_containing_pane(child, pane_id))
                    })
                    .expect("pane not found")
            }
        }
    }
}

#[test]
fn split_right_places_the_new_pane_after_the_target() {
    let (target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Pane(target_pane_id);

    let split_tree =
        split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Right).unwrap();
    let split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(split_node.direction, SplitDirection::Horizontal);
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [target_pane_id, new_pane_id]
    );
}

#[test]
fn split_left_places_the_new_pane_before_the_target() {
    let (target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Pane(target_pane_id);

    let split_tree =
        split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Left).unwrap();
    let split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(split_node.direction, SplitDirection::Horizontal);
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [new_pane_id, target_pane_id]
    );
}

#[test]
fn split_down_stacks_the_new_pane_below() {
    let (target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Pane(target_pane_id);

    let split_tree =
        split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Down).unwrap();
    let split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(split_node.direction, SplitDirection::Vertical);
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [target_pane_id, new_pane_id]
    );
}

#[test]
fn split_up_stacks_the_new_pane_above() {
    let (target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Pane(target_pane_id);

    let split_tree = split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Up).unwrap();
    let split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(split_node.direction, SplitDirection::Vertical);
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [new_pane_id, target_pane_id]
    );
}

#[test]
fn new_siblings_share_space_equally() {
    let (target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Pane(target_pane_id);

    let split_tree =
        split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Right).unwrap();
    let split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(
        split_node.weights,
        [SizeWeight::default(), SizeWeight::default()]
    );
}

#[test]
fn splitting_a_nested_leaf_touches_only_that_leaf() {
    let (left_pane_id, target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, target_pane_id);

    let split_tree =
        split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Down).unwrap();
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [left_pane_id, target_pane_id, new_pane_id]
    );
    // The original left side is untouched; only the target pane's slot became a split.
    let inner_split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(inner_split_node.direction, SplitDirection::Vertical);
    assert_eq!(inner_split_node.children.len(), 2);
}

#[test]
fn a_split_keeps_the_parent_weights_and_gives_the_new_split_equal_ones() {
    use crate::size::SizeConstraint;

    let (left_pane_id, target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut row_split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(target_pane_id),
        ],
    );
    row_split_node.weights = vec![
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(3)),
    ];
    let layout_tree = LayoutNode::Split(row_split_node);

    let split_tree =
        split_leaf(&layout_tree, target_pane_id, new_pane_id, Direction::Down).unwrap();
    let LayoutNode::Split(parent_split_node) = &split_tree else {
        panic!("root must stay a split");
    };
    assert_eq!(
        parent_split_node.weights,
        [
            SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            SizeWeight::from_primary_constraint(SizeConstraint::Flex(3)),
        ]
    );
    let inner_split_node = find_parent_split_containing_pane(&split_tree, target_pane_id);
    assert_eq!(
        inner_split_node.weights,
        [SizeWeight::default(), SizeWeight::default()]
    );
    assert_eq!(inner_split_node.active_child_index, 0);
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [left_pane_id, target_pane_id, new_pane_id]
    );
}

#[test]
fn split_result_still_tiles_the_tab() {
    let (left_pane_id, right_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let split_tree = split_leaf(&layout_tree, left_pane_id, new_pane_id, Direction::Down).unwrap();

    let layout_area = Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 24,
    });
    let layout_result = solve_layout(&split_tree, layout_area);
    check_exact_tiling(&layout_result.pane_rects, layout_area).unwrap();
}

#[test]
fn a_successful_split_leaves_the_input_unchanged() {
    let (left_pane_id, right_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let original_tree = layout_tree.clone();

    let split_result =
        split_leaf(&layout_tree, left_pane_id, new_pane_id, Direction::Right).unwrap();

    assert_eq!(layout_tree, original_tree);
    assert_eq!(
        split_result.list_leaf_pane_ids(),
        [left_pane_id, new_pane_id, right_pane_id]
    );
}

#[test]
fn a_successful_stack_addition_leaves_the_input_unchanged() {
    let (left_pane_id, right_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let original_tree = layout_tree.clone();

    let stack_result = add_pane_to_stack(&layout_tree, left_pane_id, new_pane_id).unwrap();

    assert_eq!(layout_tree, original_tree);
    assert_eq!(
        stack_result.list_leaf_pane_ids(),
        [left_pane_id, new_pane_id, right_pane_id]
    );
}

/// Verifies that a solved layout completely tiles the layout area with no gaps, overlaps, or panes
/// outside bounds.
fn assert_tiles(layout_tree: &LayoutNode, layout_area: Rect) {
    let layout_result = solve_layout(layout_tree, layout_area);
    check_exact_tiling(&layout_result.pane_rects, layout_area).unwrap();
}

#[test]
fn removing_a_middle_pane_reflows_with_no_dead_region() {
    let (first_pane_id, removed_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(first_pane_id),
            build_leaf_node(removed_pane_id),
            build_leaf_node(third_pane_id),
        ],
    ));

    let removed_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    assert_eq!(
        removed_layout_tree.list_leaf_pane_ids(),
        [first_pane_id, third_pane_id]
    );
    assert_tiles(&removed_layout_tree, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn removing_a_siblingless_leaf_prunes_the_emptied_split() {
    // A pane beside a column holding only a second pane: removing the second pane must not leave an
    // empty split claiming dead space.
    let (left_pane_id, removed_pane_id) = (PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![build_leaf_node(removed_pane_id)],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), column],
    ));

    let removed_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    assert_eq!(removed_layout_tree.list_leaf_pane_ids(), [left_pane_id]);
    assert_tiles(&removed_layout_tree, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn removing_the_last_pane_in_a_split_leaves_a_unary_split_for_normalization() {
    let (left_pane_id, top_pane_id, removed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_leaf_node(removed_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), column],
    ));

    let removed_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    assert_eq!(
        removed_layout_tree.list_leaf_pane_ids(),
        [left_pane_id, top_pane_id]
    );
    assert_tiles(&removed_layout_tree, build_cell_rect(0, 0, 80, 24));
    // The column still exists with one child; normalization collapses it.
    let LayoutNode::Split(outer) = &removed_layout_tree else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(inner) = &outer.children[1] else {
        panic!("column must survive as a unary split");
    };
    assert_eq!(inner.children.len(), 1);
    assert_eq!(inner.weights.len(), 1);
}

#[test]
fn removing_the_active_member_expands_the_collapsed_member_into_its_place() {
    let (left_pane_id, active_pane_id, collapsed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![active_pane_id, collapsed_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack],
    ));

    // The active pane held columns 40..80 over rows 0..23, with the collapsed pane's header on row
    // 23. Removing the active pane expands the collapsed pane over the whole column; the left pane
    // keeps its size.
    let removed_layout_tree = remove_pane(&layout_tree, active_pane_id).unwrap();
    assert_eq!(
        removed_layout_tree.list_leaf_pane_ids(),
        [left_pane_id, collapsed_pane_id]
    );
    assert_eq!(
        solve_layout(&removed_layout_tree, build_cell_rect(0, 0, 80, 24)).pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 40, 24)),
            (collapsed_pane_id, build_cell_rect(40, 0, 40, 24)),
        ]
    );
}

#[test]
fn removing_the_active_stack_child_activates_the_next_one() {
    let (first_pane_id, removed_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, removed_pane_id, third_pane_id],
        1,
    ));

    let removed_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    let LayoutNode::Split(stack) = &removed_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active_child_index, 1);
    let collapsed_child_flags: Vec<bool> = (0..stack.children.len())
        .map(|child_index| stack.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, false]);
    assert_eq!(
        removed_tree.list_leaf_pane_ids(),
        [first_pane_id, third_pane_id]
    );
}

#[test]
fn removing_the_last_active_stack_child_steps_back() {
    let (first_pane_id, removed_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, removed_pane_id],
        1,
    ));

    let removed_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    let LayoutNode::Split(stack) = &removed_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active_child_index, 0);
    assert!(!stack.is_child_collapsed(0));
}

#[test]
fn removing_before_the_active_stack_child_keeps_it_active() {
    let (removed_pane_id, middle_pane_id, active_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![removed_pane_id, middle_pane_id, active_pane_id],
        2,
    ));

    let removed_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    let LayoutNode::Split(stack) = &removed_tree else {
        panic!("stack must survive");
    };
    // The original active pane is still active, now at index 1.
    assert_eq!(stack.active_child_index, 1);
    assert!(!stack.is_child_collapsed(1));
    assert_eq!(
        removed_tree.list_leaf_pane_ids(),
        [middle_pane_id, active_pane_id]
    );
}

#[test]
fn removing_after_the_active_stack_child_keeps_it_active() {
    let (first_pane_id, active_pane_id, removed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, active_pane_id, removed_pane_id],
        1,
    ));

    let removed_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    let LayoutNode::Split(stack) = &removed_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active_child_index, 1);
    assert!(!stack.is_child_collapsed(1));
    assert_eq!(
        removed_tree.list_leaf_pane_ids(),
        [first_pane_id, active_pane_id]
    );
}

#[test]
fn a_stack_reduced_to_one_member_normalizes_to_a_plain_leaf() {
    use std::collections::HashSet;

    use crate::normalize::normalize_layout_tree;

    let (first_stack_pane_id, second_stack_pane_id, left_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_stack_pane_id, second_stack_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack],
    ));

    let removed_layout_tree = remove_pane(&layout_tree, first_stack_pane_id).unwrap();
    let live_pane_ids: HashSet<PaneId> = [left_pane_id, second_stack_pane_id].into_iter().collect();
    let normalized = normalize_layout_tree(&removed_layout_tree, &live_pane_ids).unwrap();

    let LayoutNode::Split(outer) = &normalized else {
        panic!("root must stay a split");
    };
    // The one-member stack collapsed into the second stack pane's plain leaf.
    assert_eq!(outer.children[1], LayoutNode::Pane(second_stack_pane_id));
    assert_tiles(&normalized, build_cell_rect(0, 0, 80, 24));
    // No header strip remains for a pane that is no longer stacked.
    assert!(solve_layout(&normalized, build_cell_rect(0, 0, 80, 24))
        .stack_headers
        .is_empty());
}

#[test]
fn a_non_active_stack_member_keeps_its_header_and_stays_selectable() {
    use std::collections::HashSet;

    use crate::focus::activate_stack_member;
    use crate::normalize::normalize_layout_tree;

    let (active_pane_id, inactive_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![active_pane_id, inactive_pane_id],
        0,
    ));

    // Both panes are in the live set, so normalization keeps the whole stack.
    let live_pane_ids: HashSet<PaneId> = [active_pane_id, inactive_pane_id].into_iter().collect();
    let mut normalized = normalize_layout_tree(&layout_tree, &live_pane_ids).unwrap();
    assert_eq!(
        normalized.list_leaf_pane_ids(),
        [active_pane_id, inactive_pane_id]
    );

    // The non-active member's header is still drawn, and it can be activated.
    let layout_result = solve_layout(&normalized, build_cell_rect(0, 0, 80, 24));
    assert_eq!(layout_result.stack_headers.len(), 1);
    assert_eq!(layout_result.stack_headers[0].pane_id, inactive_pane_id);

    let stack = normalized
        .find_containing_stack_mut(inactive_pane_id)
        .unwrap();
    assert!(activate_stack_member(stack, inactive_pane_id));
    assert_eq!(stack.active_child_index, 1);
}

#[test]
fn removing_the_last_stack_member_prunes_the_stack() {
    let (left_pane_id, removed_pane_id) = (PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(vec![removed_pane_id], 0));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack],
    ));

    let removed_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    assert_eq!(removed_layout_tree.list_leaf_pane_ids(), [left_pane_id]);
    assert_tiles(&removed_layout_tree, build_cell_rect(0, 0, 80, 24));
}

/// Close every pane except `keep_index`, visiting victims in `order`,
/// normalizing after each removal. A big layout area keeps every survivor fitting, so
/// the layout must tile exactly and solve deterministically at every step, and
/// end as the single kept leaf.
fn close_all_but_one_in_order(removal_order: &[usize], kept_pane_index: usize) {
    use std::collections::HashSet;

    use crate::normalize::normalize_layout_tree;

    let pane_ids: Vec<PaneId> = (0..51).map(|_| PaneId::new()).collect();
    let mut layout_tree = build_deep_alternating_layout(&pane_ids);
    let large_layout_rect = Rect::from_size_at_origin(Size {
        column_count: 1000,
        row_count: 1000,
    });
    let mut live_pane_ids: HashSet<PaneId> = pane_ids.iter().copied().collect();

    for &pane_index in removal_order {
        assert_ne!(
            pane_index, kept_pane_index,
            "the kept pane is never removed"
        );
        let removed_pane_id = pane_ids[pane_index];
        let next_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
        live_pane_ids.remove(&removed_pane_id);
        layout_tree = normalize_layout_tree(&next_tree, &live_pane_ids).unwrap();

        // Every surviving leaf is still live, the layout tiles the large layout area
        // exactly, and solving twice agrees.
        for pane_id in layout_tree.list_leaf_pane_ids() {
            assert!(
                live_pane_ids.contains(&pane_id),
                "dead pane {pane_id} left in the layout_tree"
            );
        }
        assert_tiles(&layout_tree, large_layout_rect);
        assert_eq!(
            solve_layout(&layout_tree, large_layout_rect),
            solve_layout(&layout_tree, large_layout_rect)
        );
    }

    assert_eq!(layout_tree, LayoutNode::Pane(pane_ids[kept_pane_index]));
}

#[test]
fn deep_tree_closed_newest_first_collapses_to_the_outermost_pane() {
    // LIFO: remove the deepest (last-created) leaf first, up to the outermost.
    let removal_order: Vec<usize> = (1..51).rev().collect();
    close_all_but_one_in_order(&removal_order, 0);
}

#[test]
fn deep_tree_closed_oldest_first_collapses_to_the_deepest_pane() {
    // FIFO: remove the outermost leaf first, down to the deepest.
    let removal_order: Vec<usize> = (0..50).collect();
    close_all_but_one_in_order(&removal_order, 50);
}

#[test]
fn deep_tree_closed_in_a_fixed_scrambled_order_stays_consistent() {
    // A fixed permutation: i*20 mod 51 visits each index once. The kept pane
    // is skipped. Same invariants, arbitrary order.
    let kept_pane_index = 25;
    let removal_order: Vec<usize> = (0..51)
        .map(|pane_index| (pane_index * 20) % 51)
        .filter(|&pane_index| pane_index != kept_pane_index)
        .collect();
    assert_eq!(removal_order.len(), 50);
    close_all_but_one_in_order(&removal_order, kept_pane_index);
}

#[test]
fn splitting_a_removed_pane_is_rejected_then_a_live_pane_still_splits() {
    // Remove a pane, then try to split the now-dead id: rejected, layout_tree
    // unchanged. The next split against a live pane still works.
    let (removed_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, removed_pane_id, live_pane_id);
    let tree_after_removal = remove_pane(&layout_tree, removed_pane_id).unwrap();
    assert_eq!(tree_after_removal.list_leaf_pane_ids(), [live_pane_id]);

    let original_tree_after_removal = tree_after_removal.clone();
    let new_pane_id = PaneId::new();
    let split_error = split_leaf(
        &tree_after_removal,
        removed_pane_id,
        new_pane_id,
        Direction::Right,
    )
    .unwrap_err();
    assert_eq!(
        split_error,
        SplitError::PaneNotFound {
            target_pane_id: removed_pane_id
        }
    );
    assert_eq!(tree_after_removal, original_tree_after_removal);

    let split_tree = split_leaf(
        &tree_after_removal,
        live_pane_id,
        new_pane_id,
        Direction::Right,
    )
    .unwrap();
    assert_eq!(split_tree.list_leaf_pane_ids(), [live_pane_id, new_pane_id]);
}

#[test]
fn removing_the_last_pane_is_rejected_then_it_can_still_be_split() {
    // The last pane cannot be removed, but the rejection leaves it intact and
    // a following split succeeds.
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Pane(pane_id);
    let removal_error = remove_pane(&layout_tree, pane_id).unwrap_err();
    assert_eq!(removal_error, RemoveError::LastPane { pane_id });
    assert_eq!(layout_tree, LayoutNode::Pane(pane_id));

    let new_pane_id = PaneId::new();
    let split_tree = split_leaf(&layout_tree, pane_id, new_pane_id, Direction::Down).unwrap();
    assert_eq!(split_tree.list_leaf_pane_ids(), [pane_id, new_pane_id]);
}

#[test]
fn removing_stack_members_until_one_remains_then_normalizing_gives_a_leaf() {
    use std::collections::HashSet;

    use crate::normalize::normalize_layout_tree;

    // A four-member stack, closed one member at a time. The stack keeps
    // exactly one expanded child throughout, and the final survivor
    // normalizes to a plain leaf with no header.
    let (first_pane_id, second_pane_id, third_pane_id, fourth_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, second_pane_id, third_pane_id, fourth_pane_id],
        1,
    ));

    for removed_pane_id in [fourth_pane_id, first_pane_id, second_pane_id] {
        let next_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
        layout_tree = next_layout_tree;
        // After every removal exactly one child stays expanded.
        if let LayoutNode::Split(stack) = &layout_tree {
            let expanded_child_count = (0..stack.children.len())
                .filter(|&child_index| !stack.is_child_collapsed(child_index))
                .count();
            assert_eq!(
                expanded_child_count, 1,
                "a stack always has one expanded member"
            );
        }
    }
    assert_eq!(layout_tree.list_leaf_pane_ids(), [third_pane_id]);

    let live_pane_ids: HashSet<PaneId> = [third_pane_id].into_iter().collect();
    let normalized = normalize_layout_tree(&layout_tree, &live_pane_ids).unwrap();
    assert_eq!(normalized, LayoutNode::Pane(third_pane_id));
    assert!(solve_layout(&normalized, build_cell_rect(0, 0, 80, 24))
        .stack_headers
        .is_empty());
}

#[test]
fn removing_the_only_pane_is_rejected() {
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Pane(pane_id);
    let removal_error = remove_pane(&layout_tree, pane_id).unwrap_err();
    assert_eq!(removal_error, RemoveError::LastPane { pane_id });
}

#[test]
fn removing_a_missing_pane_is_rejected_and_the_input_is_unchanged() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let original_tree = layout_tree.clone();

    let missing_pane_id = PaneId::new();
    let removal_error = remove_pane(&layout_tree, missing_pane_id).unwrap_err();
    assert_eq!(
        removal_error,
        RemoveError::PaneNotFound {
            pane_id: missing_pane_id
        }
    );
    assert_eq!(layout_tree, original_tree);
}

#[test]
fn stacking_onto_a_plain_pane_creates_a_stack_with_the_new_pane_active() {
    let (left_pane_id, target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, target_pane_id);

    let stacked_tree = add_pane_to_stack(&layout_tree, target_pane_id, new_pane_id).unwrap();
    assert_eq!(
        stacked_tree.list_leaf_pane_ids(),
        [left_pane_id, target_pane_id, new_pane_id]
    );
    let LayoutNode::Split(root_split_node) = &stacked_tree else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(stack_split_node) = &root_split_node.children[1] else {
        panic!("the target pane's slot must become a stack");
    };
    assert_eq!(stack_split_node.direction, SplitDirection::Stacked);
    assert_eq!(stack_split_node.active_child_index, 1);
    let collapsed_child_flags: Vec<bool> = (0..stack_split_node.children.len())
        .map(|child_index| stack_split_node.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, false]);
}

#[test]
fn stacking_onto_a_stack_member_appends_to_that_stack() {
    let (first_pane_id, second_pane_id, new_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, second_pane_id],
        0,
    ));

    let stacked_tree = add_pane_to_stack(&layout_tree, first_pane_id, new_pane_id).unwrap();
    let LayoutNode::Split(stack_split_node) = &stacked_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack_split_node.children.len(), 3);
    assert_eq!(stack_split_node.weights.len(), 3);
    assert_eq!(stack_split_node.active_child_index, 2);
    assert_eq!(
        stacked_tree.list_leaf_pane_ids(),
        [first_pane_id, second_pane_id, new_pane_id]
    );
    let collapsed_child_flags: Vec<bool> = (0..stack_split_node.children.len())
        .map(|child_index| stack_split_node.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, true, false]);
}

#[test]
fn stacking_onto_a_pane_inside_a_split_member_appends_to_that_stack() {
    let (active_pane_id, left_nested_pane_id, right_nested_pane_id, new_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(active_pane_id),
            build_equal_split_node(
                SplitDirection::Horizontal,
                left_nested_pane_id,
                right_nested_pane_id,
            ),
        ],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 0,
    });

    // The right nested pane is a leaf of the second member; the new pane becomes a third
    // member of the stack, beside that split, and takes the active slot.
    let stacked_tree = add_pane_to_stack(&layout_tree, right_nested_pane_id, new_pane_id).unwrap();
    let LayoutNode::Split(stack_split_node) = &stacked_tree else {
        panic!("stack must survive");
    };
    assert_eq!(stack_split_node.children.len(), 3);
    assert_eq!(stack_split_node.weights.len(), 3);
    assert_eq!(stack_split_node.active_child_index, 2);
    assert_eq!(
        stack_split_node.children[1],
        build_equal_split_node(
            SplitDirection::Horizontal,
            left_nested_pane_id,
            right_nested_pane_id,
        )
    );
    assert_eq!(stack_split_node.children[2], LayoutNode::Pane(new_pane_id));
    let collapsed_child_flags: Vec<bool> = (0..stack_split_node.children.len())
        .map(|child_index| stack_split_node.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, true, false]);
    assert_eq!(
        stacked_tree.list_leaf_pane_ids(),
        [
            active_pane_id,
            left_nested_pane_id,
            right_nested_pane_id,
            new_pane_id,
        ]
    );
}

#[test]
fn stacking_onto_a_member_of_a_nested_stack_joins_the_innermost_stack() {
    let (outer_pane_id, first_inner_pane_id, second_inner_pane_id, new_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let inner_stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_inner_pane_id, second_inner_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(outer_pane_id), inner_stack],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 1,
    });

    let stacked_tree = add_pane_to_stack(&layout_tree, second_inner_pane_id, new_pane_id).unwrap();
    let LayoutNode::Split(outer_split_node) = &stacked_tree else {
        panic!("the outer stack must survive");
    };
    // The outer stack is untouched: two members, the inner stack active.
    assert_eq!(outer_split_node.children.len(), 2);
    assert_eq!(outer_split_node.active_child_index, 1);
    let LayoutNode::Split(inner_split_node) = &outer_split_node.children[1] else {
        panic!("the inner stack must survive");
    };
    assert_eq!(inner_split_node.children.len(), 3);
    assert_eq!(inner_split_node.active_child_index, 2);
    assert_eq!(inner_split_node.children[2], LayoutNode::Pane(new_pane_id));
    let collapsed_child_flags: Vec<bool> = (0..inner_split_node.children.len())
        .map(|child_index| inner_split_node.is_child_collapsed(child_index))
        .collect();
    assert_eq!(collapsed_child_flags, [true, true, false]);
    assert_eq!(
        stacked_tree.list_leaf_pane_ids(),
        [
            outer_pane_id,
            first_inner_pane_id,
            second_inner_pane_id,
            new_pane_id,
        ]
    );
}

#[test]
fn stacked_layout_still_tiles_after_the_edit() {
    let (left_pane_id, target_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, target_pane_id);
    let stacked_tree = add_pane_to_stack(&layout_tree, target_pane_id, new_pane_id).unwrap();
    assert_tiles(&stacked_tree, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn a_directional_split_treats_the_whole_stack_as_one_operand() {
    let (left_pane_id, first_stack_pane_id, second_stack_pane_id, new_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_stack_pane_id, second_stack_pane_id],
        1,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack.clone()],
    ));

    // Splitting downward from a stack member puts the new pane under the
    // stack, with the stack itself intact above it.
    let split_tree = split_leaf(
        &layout_tree,
        second_stack_pane_id,
        new_pane_id,
        Direction::Down,
    )
    .unwrap();
    let LayoutNode::Split(root_split_node) = &split_tree else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(column_split_node) = &root_split_node.children[1] else {
        panic!("the stack's slot must become a vertical split");
    };
    assert_eq!(column_split_node.direction, SplitDirection::Vertical);
    assert_eq!(column_split_node.children[0], stack);
    assert_eq!(column_split_node.children[1], LayoutNode::Pane(new_pane_id));
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [
            left_pane_id,
            first_stack_pane_id,
            second_stack_pane_id,
            new_pane_id,
        ]
    );
}

#[test]
fn a_directional_split_before_a_stack_places_the_new_pane_first() {
    let (first_stack_pane_id, second_stack_pane_id, new_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_stack_pane_id, second_stack_pane_id],
        0,
    ));

    let split_tree = split_leaf(&stack, first_stack_pane_id, new_pane_id, Direction::Left).unwrap();
    let LayoutNode::Split(row_split_node) = &split_tree else {
        panic!("root must become a split");
    };
    assert_eq!(row_split_node.direction, SplitDirection::Horizontal);
    assert_eq!(row_split_node.children[0], LayoutNode::Pane(new_pane_id));
    assert_eq!(row_split_node.children[1], stack);
}

#[test]
fn stacking_onto_a_missing_anchor_is_rejected() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let original_tree = layout_tree.clone();

    let missing_pane_id = PaneId::new();
    let stack_error = add_pane_to_stack(&layout_tree, missing_pane_id, PaneId::new()).unwrap_err();
    assert_eq!(
        stack_error,
        SplitError::PaneNotFound {
            target_pane_id: missing_pane_id
        }
    );
    assert_eq!(layout_tree, original_tree);
}

#[test]
fn missing_target_is_an_error_and_the_input_is_unchanged() {
    let (left_pane_id, right_pane_id, new_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let original_tree = layout_tree.clone();

    let missing_pane_id = PaneId::new();
    let split_error =
        split_leaf(&layout_tree, missing_pane_id, new_pane_id, Direction::Right).unwrap_err();
    assert_eq!(
        split_error,
        SplitError::PaneNotFound {
            target_pane_id: missing_pane_id
        }
    );
    assert_eq!(layout_tree, original_tree);
}

#[test]
fn a_removal_leaves_every_surviving_child_with_its_own_weight() {
    use crate::size::SizeConstraint;

    let (left_pane_id, removed_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut row_split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(removed_pane_id),
            build_leaf_node(right_pane_id),
        ],
    );
    row_split_node.weights = vec![
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(2)),
        SizeWeight::from_primary_constraint(SizeConstraint::Flex(3)),
    ];
    let layout_tree = LayoutNode::Split(row_split_node);
    let wide_layout_rect = Rect::from_size_at_origin(Size {
        column_count: 100,
        row_count: 24,
    });

    let removed_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    let LayoutNode::Split(remaining_split_node) = &removed_layout_tree else {
        panic!("the row must survive");
    };
    // Dropping the middle child drops its weight with it: the shares that
    // remain are 1 and 3, not 1 and 2.
    assert_eq!(
        remaining_split_node.weights,
        [
            SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            SizeWeight::from_primary_constraint(SizeConstraint::Flex(3)),
        ]
    );
    let widths: Vec<u16> = solve_layout(&removed_layout_tree, wide_layout_rect)
        .pane_rects
        .iter()
        .map(|(_, rect)| rect.size.column_count)
        .collect();
    assert_eq!(widths, [25, 75]);
}

#[test]
fn a_directional_split_from_a_nested_stack_wraps_the_outermost_stack() {
    let (outer_pane_id, first_inner_pane_id, second_inner_pane_id, new_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let inner_stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_inner_pane_id, second_inner_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(outer_pane_id), inner_stack],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 1,
    });

    // The second inner pane sits two stacks deep. The new pane lands beside the whole outer
    // stack, not beside the inner one.
    let split_tree = split_leaf(
        &layout_tree,
        second_inner_pane_id,
        new_pane_id,
        Direction::Right,
    )
    .unwrap();
    let LayoutNode::Split(row_split_node) = &split_tree else {
        panic!("the root must become a split");
    };
    assert_eq!(row_split_node.direction, SplitDirection::Horizontal);
    assert_eq!(row_split_node.children[0], layout_tree);
    assert_eq!(row_split_node.children[1], LayoutNode::Pane(new_pane_id));
    assert_eq!(
        split_tree.list_leaf_pane_ids(),
        [
            outer_pane_id,
            first_inner_pane_id,
            second_inner_pane_id,
            new_pane_id,
        ]
    );
}

#[test]
fn removing_a_pane_reflows_the_survivors_with_one_gap_between_them() {
    let (left_pane_id, middle_pane_id, removed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(middle_pane_id),
            build_leaf_node(removed_pane_id),
        ],
    ));
    let wide_layout_rect = Rect::from_size_at_origin(Size {
        column_count: 120,
        row_count: 24,
    });

    // Three columns reserve two gaps; two columns reserve one, so the
    // survivors share 118 cells as 59 each.
    let removed_layout_tree = remove_pane(&layout_tree, removed_pane_id).unwrap();
    let layout_after_removal =
        solve_layout_with_sizing(&removed_layout_tree, wide_layout_rect, build_pane_sizing(2));
    assert_eq!(
        layout_after_removal.pane_rects,
        [
            (
                left_pane_id,
                Rect::from_origin_and_size(
                    Point { column: 0, row: 0 },
                    Size {
                        column_count: 59,
                        row_count: 24
                    }
                )
            ),
            (
                middle_pane_id,
                Rect::from_origin_and_size(
                    Point { column: 61, row: 0 },
                    Size {
                        column_count: 59,
                        row_count: 24
                    }
                )
            ),
        ]
    );
}

#[test]
fn removing_the_last_pane_beside_an_empty_split_is_rejected() {
    // The empty split survives the removal, so the walk never reports the root
    // as emptied; the layout_tree it would leave behind holds no pane at all.
    let pane_id = PaneId::new();
    let empty_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(pane_id), empty_split],
    ));

    let removal_error =
        remove_pane(&layout_tree, pane_id).expect_err("the layout_tree holds no other pane");

    assert_eq!(removal_error, RemoveError::LastPane { pane_id });
}
