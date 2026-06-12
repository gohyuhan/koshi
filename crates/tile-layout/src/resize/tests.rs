use tile_core::geometry::{Point, Size};
use tile_test_support::layout_assert::{
    assert_all_space_occupied, assert_no_outside, assert_no_overlap,
};

use super::*;
use crate::solver::solve;
use crate::tree::LayoutChild;

fn tab() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 })
}

fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

fn pair(direction: SplitDirection, a: PaneId, b: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        vec![leaf(a), leaf(b)],
    ))
}

fn solved_size(tree: &LayoutNode, tab: Rect, pane: PaneId) -> Size {
    solve(tree, tab)
        .panes
        .into_iter()
        .find(|&(id, _)| id == pane)
        .expect("pane is in the layout")
        .1
        .size
}

fn assert_tiles(tree: &LayoutNode, tab: Rect) {
    let result = solve(tree, tab);
    assert_all_space_occupied(&result.panes, tab).unwrap();
    assert_no_overlap(&result.panes).unwrap();
    assert_no_outside(&result.panes, tab).unwrap();
}

#[test]
fn growing_right_by_one_cell_moves_one_column() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let resized = resize(&tree, tab(), a, Direction::Right, 1).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 41);
    assert_eq!(solved_size(&resized, tab(), b).cols, 39);
    assert_tiles(&resized, tab());
}

#[test]
fn growing_left_takes_from_the_left_neighbor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let resized = resize(&tree, tab(), b, Direction::Left, 1).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 39);
    assert_eq!(solved_size(&resized, tab(), b).cols, 41);
}

#[test]
fn growing_down_and_up_move_rows() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Vertical, a, b);

    let down = resize(&tree, tab(), a, Direction::Down, 1).unwrap();
    assert_eq!(solved_size(&down, tab(), a).rows, 13);
    assert_eq!(solved_size(&down, tab(), b).rows, 11);

    let up = resize(&tree, tab(), b, Direction::Up, 2).unwrap();
    assert_eq!(solved_size(&up, tab(), a).rows, 10);
    assert_eq!(solved_size(&up, tab(), b).rows, 14);
}

#[test]
fn resizes_accumulate_across_transactions() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut tree = pair(SplitDirection::Horizontal, a, b);

    tree = resize(&tree, tab(), a, Direction::Right, 1).unwrap();
    tree = resize(&tree, tab(), a, Direction::Right, 1).unwrap();
    assert_eq!(solved_size(&tree, tab(), a).cols, 42);
    assert_eq!(solved_size(&tree, tab(), b).cols, 38);
}

#[test]
fn nested_pane_resizes_at_the_level_that_owns_the_border() {
    // a | (b over c): growing b leftward moves the column border, growing b
    // downward moves the border b shares with c.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ));

    let wider = resize(&tree, tab(), b, Direction::Left, 2).unwrap();
    assert_eq!(solved_size(&wider, tab(), a).cols, 38);
    assert_eq!(solved_size(&wider, tab(), b).cols, 42);
    assert_eq!(solved_size(&wider, tab(), c).cols, 42);

    let taller = resize(&tree, tab(), b, Direction::Down, 3).unwrap();
    assert_eq!(solved_size(&taller, tab(), b).rows, 15);
    assert_eq!(solved_size(&taller, tab(), c).rows, 9);
    assert_eq!(solved_size(&taller, tab(), a).rows, 24);
    assert_tiles(&taller, tab());
}

#[test]
fn pane_inside_a_stack_resizes_the_stack_as_a_unit() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(stack)],
    ));

    // Resizing the collapsed member moves the stack's outer border too.
    let resized = resize(&tree, tab(), c, Direction::Left, 5).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 35);
    assert_eq!(solved_size(&resized, tab(), b).cols, 45);
}

#[test]
fn resize_blocked_by_the_neighbors_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let narrow = Rect::new(Point { x: 0, y: 0 }, Size { cols: 10, rows: 24 });

    // b solves to five columns and must keep two: three are spare.
    let err = resize(&tree, narrow, a, Direction::Right, 4).unwrap_err();
    assert_eq!(
        err,
        ResizeError::MinSize {
            requested: 4,
            spare: 3,
        }
    );

    let allowed = resize(&tree, narrow, a, Direction::Right, 3).unwrap();
    assert_eq!(solved_size(&allowed, narrow, a).cols, 8);
    assert_eq!(solved_size(&allowed, narrow, b).cols, 2);
}

#[test]
fn pane_on_the_tab_edge_has_no_border_on_that_side() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let err = resize(&tree, tab(), a, Direction::Left, 1).unwrap_err();
    assert_eq!(
        err,
        ResizeError::NoAdjacentBorder {
            pane: a,
            direction: Direction::Left,
        }
    );
    // No vertical border exists in a purely horizontal split either.
    let err = resize(&tree, tab(), a, Direction::Down, 1).unwrap_err();
    assert_eq!(
        err,
        ResizeError::NoAdjacentBorder {
            pane: a,
            direction: Direction::Down,
        }
    );
}

#[test]
fn missing_pane_is_reported_and_the_input_is_unchanged() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let snapshot = tree.clone();

    let missing = PaneId::new();
    let err = resize(&tree, tab(), missing, Direction::Right, 1).unwrap_err();
    assert_eq!(err, ResizeError::PaneNotFound { pane: missing });
    assert_eq!(tree, snapshot);
}
