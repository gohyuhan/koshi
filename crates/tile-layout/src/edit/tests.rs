use tile_core::geometry::{Point, Rect, Size};
use tile_test_support::layout_assert::{
    assert_all_space_occupied, assert_no_outside, assert_no_overlap,
};

use super::*;
use crate::size::SizeWeight;
use crate::solver::solve;

fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

fn pair(direction: SplitDirection, a: PaneId, b: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        vec![leaf(a), leaf(b)],
    ))
}

/// The split node that replaced the target leaf, wherever it ended up.
fn find_split_of(tree: &LayoutNode, member: PaneId) -> &SplitNode {
    match tree {
        LayoutNode::Pane(_) => panic!("expected a split in {tree:?}"),
        LayoutNode::Split(split) => {
            if split
                .children
                .iter()
                .any(|child| matches!(child.node, LayoutNode::Pane(id) if id == member))
            {
                split
            } else {
                split
                    .children
                    .iter()
                    .find_map(|child| {
                        child
                            .node
                            .contains_pane(member)
                            .then(|| find_split_of(&child.node, member))
                    })
                    .expect("member not found")
            }
        }
    }
}

#[test]
fn split_right_places_the_new_pane_after_the_target() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Right).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Horizontal);
    assert_eq!(split.leaf_panes(), [target, new]);
}

#[test]
fn split_left_places_the_new_pane_before_the_target() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Left).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Horizontal);
    assert_eq!(split.leaf_panes(), [new, target]);
}

#[test]
fn split_down_stacks_the_new_pane_below() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Down).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Vertical);
    assert_eq!(split.leaf_panes(), [target, new]);
}

#[test]
fn split_up_stacks_the_new_pane_above() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Up).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.direction, SplitDirection::Vertical);
    assert_eq!(split.leaf_panes(), [new, target]);
}

#[test]
fn new_siblings_share_space_equally() {
    let (target, new) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Pane(target);

    let split = split_leaf(&tree, target, new, Direction::Right).unwrap();
    let node = find_split_of(&split, target);
    assert_eq!(node.weights, [SizeWeight::default(), SizeWeight::default()]);
}

#[test]
fn splitting_a_nested_leaf_touches_only_that_leaf() {
    let (a, b, new) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let split = split_leaf(&tree, b, new, Direction::Down).unwrap();
    assert_eq!(split.leaf_panes(), [a, b, new]);
    // The original left side is untouched; only b's slot became a split.
    let inner = find_split_of(&split, b);
    assert_eq!(inner.direction, SplitDirection::Vertical);
    assert_eq!(inner.children.len(), 2);
}

#[test]
fn split_result_still_tiles_the_tab() {
    let (a, b, new) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let split = split_leaf(&tree, a, new, Direction::Down).unwrap();

    let tab = Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 });
    let result = solve(&split, tab);
    assert_all_space_occupied(&result.panes, tab).unwrap();
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
}

fn tab() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 })
}

fn assert_tiles(tree: &LayoutNode, tab: Rect) {
    let result = solve(tree, tab);
    assert_all_space_occupied(&result.panes, tab).unwrap();
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
}

#[test]
fn removing_a_middle_pane_reflows_with_no_dead_region() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b), leaf(c)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), b).unwrap();
    assert_eq!(removed.leaf_panes(), [a, c]);
    assert_tiles(&removed, tab());

    // Before: a 0..26, b 26..53, c 53..80. After: a 0..40 takes 14 of b's
    // columns, c 40..80 takes 13 — a absorbed more, so it leads.
    assert_eq!(
        info.old_rect,
        Rect::new(Point { x: 26, y: 0 }, Size { cols: 27, rows: 24 })
    );
    assert_eq!(info.absorbed_by, [a, c]);
}

#[test]
fn removing_a_siblingless_leaf_prunes_the_emptied_split() {
    // a beside a column holding only b: removing b must not leave an empty
    // split claiming dead space.
    let (a, b) = (PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));

    let (removed, info) = remove_pane(&tree, tab(), b).unwrap();
    assert_eq!(removed.leaf_panes(), [a]);
    assert_tiles(&removed, tab());
    assert_eq!(info.absorbed_by, [a]);
}

#[test]
fn removing_the_last_pane_in_a_split_leaves_a_unary_split_for_normalization() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));

    let (removed, _) = remove_pane(&tree, tab(), c).unwrap();
    assert_eq!(removed.leaf_panes(), [a, b]);
    assert_tiles(&removed, tab());
    // The column still exists with one child; normalization collapses it.
    let LayoutNode::Split(outer) = &removed else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(inner) = &outer.children[1].node else {
        panic!("column must survive as a unary split");
    };
    assert_eq!(inner.children.len(), 1);
    assert_eq!(inner.weights.len(), 1);
}

#[test]
fn removing_the_active_stack_child_activates_the_next_one() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 1));

    let (removed, _) = remove_pane(&tree, tab(), b).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 1);
    let collapsed: Vec<bool> = stack.children.iter().map(|child| child.collapsed).collect();
    assert_eq!(collapsed, [true, false]);
    assert_eq!(removed.leaf_panes(), [a, c]);
}

#[test]
fn removing_the_last_active_stack_child_steps_back() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 1));

    let (removed, _) = remove_pane(&tree, tab(), b).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 0);
    assert!(!stack.children[0].collapsed);
}

#[test]
fn removing_before_the_active_stack_child_keeps_it_active() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b, c], 2));

    let (removed, _) = remove_pane(&tree, tab(), a).unwrap();
    let LayoutNode::Split(stack) = &removed else {
        panic!("stack must survive");
    };
    // c is still the active pane, now at index 1.
    assert_eq!(stack.active, 1);
    assert!(!stack.children[1].collapsed);
    assert_eq!(removed.leaf_panes(), [b, c]);
}

#[test]
fn removing_the_only_pane_is_rejected() {
    let a = PaneId::new();
    let tree = LayoutNode::Pane(a);
    let err = remove_pane(&tree, tab(), a).unwrap_err();
    assert_eq!(err, RemoveError::LastPane { pane: a });
}

#[test]
fn removing_a_missing_pane_is_rejected_and_the_input_is_unchanged() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let snapshot = tree.clone();

    let missing = PaneId::new();
    let err = remove_pane(&tree, tab(), missing).unwrap_err();
    assert_eq!(err, RemoveError::PaneNotFound { pane: missing });
    assert_eq!(tree, snapshot);
}

#[test]
fn missing_target_is_an_error_and_the_input_is_unchanged() {
    let (a, b, new) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let snapshot = tree.clone();

    let missing = PaneId::new();
    let err = split_leaf(&tree, missing, new, Direction::Right).unwrap_err();
    assert_eq!(err, SplitError::PaneNotFound { target: missing });
    assert_eq!(tree, snapshot);
}
