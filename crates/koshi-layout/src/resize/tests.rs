//! Tests for resize transactions: moving pane borders by exact signed cell
//! counts — outward grows the pane, inward shrinks it toward the neighbor.

use koshi_core::geometry::{Point, Size};
use koshi_test_support::layout_assert::{
    assert_all_space_occupied, assert_no_outside, assert_no_overlap,
};

use super::*;
use crate::solver::solve;
use crate::tree::{LayoutChild, SplitNode};

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

/// Solves the layout and returns the allocated size for the given pane.
fn solved_size(tree: &LayoutNode, tab: Rect, pane: PaneId) -> Size {
    solve(tree, tab)
        .panes
        .into_iter()
        .find(|&(id, _)| id == pane)
        .expect("pane is in the layout")
        .1
        .size
}

/// Verifies that the layout tiles the tab correctly: all cells are occupied,
/// panes don't overlap, and none extend outside the tab bounds.
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
fn shrinking_right_gives_the_cells_to_the_right_neighbor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let resized = resize(&tree, tab(), a, Direction::Right, -3).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 37);
    assert_eq!(solved_size(&resized, tab(), b).cols, 43);
    assert_tiles(&resized, tab());
}

#[test]
fn shrinking_left_gives_the_cells_to_the_left_neighbor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let resized = resize(&tree, tab(), b, Direction::Left, -2).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 42);
    assert_eq!(solved_size(&resized, tab(), b).cols, 38);
}

#[test]
fn a_shrink_mirrors_the_neighbors_grow_on_the_same_border() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let shrunk = resize(&tree, tab(), a, Direction::Right, -3).unwrap();
    let grown = resize(&tree, tab(), b, Direction::Left, 3).unwrap();
    assert_eq!(shrunk, grown);
}

#[test]
fn shrink_blocked_by_the_panes_own_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let narrow = Rect::new(Point { x: 0, y: 0 }, Size { cols: 10, rows: 24 });

    // a solves to five columns and must keep its border-inclusive four: one
    // is spare — on a shrink, a itself is the donor.
    let err = resize(&tree, narrow, a, Direction::Right, -4).unwrap_err();
    assert_eq!(
        err,
        ResizeError::MinSize {
            requested: 4,
            spare: 1,
        }
    );

    let allowed = resize(&tree, narrow, a, Direction::Right, -1).unwrap();
    assert_eq!(solved_size(&allowed, narrow, a).cols, 4);
    assert_eq!(solved_size(&allowed, narrow, b).cols, 6);
}

#[test]
fn shrink_on_a_tab_edge_has_no_border_either() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let err = resize(&tree, tab(), a, Direction::Left, -1).unwrap_err();
    assert_eq!(
        err,
        ResizeError::NoAdjacentBorder {
            pane: a,
            direction: Direction::Left,
        }
    );
}

#[test]
fn zero_size_returns_the_tree_unchanged() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    let resized = resize(&tree, tab(), a, Direction::Right, 0).unwrap();
    assert_eq!(resized, tree);
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
fn resize_inside_an_active_stack_subtree_sees_the_header_carved_rect() {
    // Hand-built: a stack whose active member is a vertical pair. The
    // resize preflight measures the donor inside the header-carved active
    // rect, accounting for the invisible header row.
    let (a, upper, lower) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, upper], 1);
    stack.children[1].node = pair(SplitDirection::Vertical, upper, lower);
    let tree = LayoutNode::Split(stack);

    // One header row leaves 23 rows for the pair: upper 11, lower 12. The
    // donor above can spare eight rows — its eleven minus the border-inclusive
    // floor of three — measured inside the header-carved active rect, not the
    // whole stack rect.
    let err = resize(&tree, tab(), lower, Direction::Up, 11).unwrap_err();
    assert_eq!(
        err,
        ResizeError::MinSize {
            requested: 11,
            spare: 8,
        }
    );

    let allowed = resize(&tree, tab(), lower, Direction::Up, 8).unwrap();
    assert_eq!(solved_size(&allowed, tab(), upper).rows, 3);
    assert_eq!(solved_size(&allowed, tab(), lower).rows, 20);
}

#[test]
fn resize_inside_a_collapsed_member_moves_the_stack_border() {
    // Hand-built: a collapsed member that is itself a split. Its inner
    // borders are invisible; resizing one of its panes bubbles to the
    // stack's outer border, resizing it as a unit.
    let (a, x, u, v) = (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![x, u], 0);
    stack.children[1].node = pair(SplitDirection::Horizontal, u, v);
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(LayoutNode::Split(stack))],
    ));

    let resized = resize(&tree, tab(), v, Direction::Left, 5).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 35);
    assert_eq!(solved_size(&resized, tab(), x).cols, 45);
}

#[test]
fn missing_weights_are_repaired_before_a_resize() {
    // Hand-built: a deserialized split can carry fewer weights than
    // children. The transaction pads the missing ones with the default
    // share instead of panicking when it indexes them.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![leaf(a), leaf(b)],
        weights: Vec::new(),
        active: 0,
    });

    let resized = resize(&tree, tab(), a, Direction::Right, 1).unwrap();
    assert_eq!(solved_size(&resized, tab(), a).cols, 41);
    assert_eq!(solved_size(&resized, tab(), b).cols, 39);
}

#[test]
fn resize_blocked_by_the_neighbors_floor() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let narrow = Rect::new(Point { x: 0, y: 0 }, Size { cols: 10, rows: 24 });

    // b solves to five columns and must keep its border-inclusive four: one
    // is spare.
    let err = resize(&tree, narrow, a, Direction::Right, 4).unwrap_err();
    assert_eq!(
        err,
        ResizeError::MinSize {
            requested: 4,
            spare: 1,
        }
    );

    let allowed = resize(&tree, narrow, a, Direction::Right, 1).unwrap();
    assert_eq!(solved_size(&allowed, narrow, a).cols, 6);
    assert_eq!(solved_size(&allowed, narrow, b).cols, 4);
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

#[test]
fn resizing_the_only_pane_in_the_tree_has_no_border_anywhere() {
    // A bare single-pane tree: the pane's path to itself is empty, so no
    // ancestor split exists on any axis, on any side.
    let a = PaneId::new();
    let tree = LayoutNode::Pane(a);

    for direction in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        let err = resize(&tree, tab(), a, direction, 1).unwrap_err();
        assert_eq!(err, ResizeError::NoAdjacentBorder { pane: a, direction });
    }
}

#[test]
fn middle_pane_in_a_three_way_split_resizes_either_border() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b), leaf(c)],
    ));

    // Baseline three-way split of 80 columns: 26 / 27 / 27.
    let grown_right = resize(&tree, tab(), b, Direction::Right, 2).unwrap();
    assert_eq!(solved_size(&grown_right, tab(), a).cols, 26);
    assert_eq!(solved_size(&grown_right, tab(), b).cols, 29);
    assert_eq!(solved_size(&grown_right, tab(), c).cols, 25);
    assert_tiles(&grown_right, tab());

    let grown_left = resize(&tree, tab(), b, Direction::Left, 2).unwrap();
    assert_eq!(solved_size(&grown_left, tab(), a).cols, 24);
    assert_eq!(solved_size(&grown_left, tab(), b).cols, 29);
    assert_eq!(solved_size(&grown_left, tab(), c).cols, 27);
    assert_tiles(&grown_left, tab());
}

#[test]
fn resize_amount_far_exceeding_available_reports_the_exact_spare() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);

    // b solves to 40 columns; its spare above the border-inclusive floor
    // of 4 is 36. A maximal signed request is rejected with that exact
    // figure, not an overflow or a panic.
    let err = resize(&tree, tab(), a, Direction::Right, i16::MAX).unwrap_err();
    assert_eq!(
        err,
        ResizeError::MinSize {
            requested: i16::MAX as u16,
            spare: 36,
        }
    );
}

#[test]
fn a_request_exactly_at_the_spare_boundary_succeeds_one_past_it_fails() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = pair(SplitDirection::Horizontal, a, b);
    let narrow = Rect::new(Point { x: 0, y: 0 }, Size { cols: 10, rows: 24 });

    // b solves to five columns with a border-inclusive floor of four: one
    // spare cell exactly. Taking exactly that one cell succeeds; asking
    // for one more is rejected with the same spare figure.
    let allowed = resize(&tree, narrow, a, Direction::Right, 1).unwrap();
    assert_eq!(solved_size(&allowed, narrow, a).cols, 6);
    assert_eq!(solved_size(&allowed, narrow, b).cols, 4);

    let err = resize(&tree, narrow, a, Direction::Right, 2).unwrap_err();
    assert_eq!(
        err,
        ResizeError::MinSize {
            requested: 2,
            spare: 1,
        }
    );
}
