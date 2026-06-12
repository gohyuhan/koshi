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
