//! Tests for resize transactions: moving pane borders by exact signed cell
//! counts — outward grows the pane, inward shrinks it toward the neighbor.

use koshi_core::geometry::{Point, Size};
use koshi_test_support::layout_assert::check_exact_tiling;

use super::*;
use crate::solver::{solve_layout, solve_layout_with_sizing, MIN_PANE_SIZE};
use crate::tree::{LayoutNode, SplitNode};

fn build_layout_area() -> Rect {
    Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 24,
    })
}

/// `true` when [`find_resize_border`] finds a border to move for `pane_id` toward
/// `direction`: the pane is in the layout tree and an ancestor split on the
/// matching axis, above any collapsed stack member, has a sibling on that
/// side. `false` for a pane not in the layout tree, for a side on the layout area edge, and
/// for the boundary against a collapsed stack header.
fn has_adjacent_border(layout_tree: &LayoutNode, pane_id: PaneId, direction: Direction) -> bool {
    let Some(pane_path) = layout_tree.find_pane_path(pane_id) else {
        return false;
    };
    find_resize_border(
        layout_tree,
        &pane_path,
        compute_split_direction(direction),
        direction,
    )
    .is_some()
}

/// The default content floor with the given gap between kept children of a
/// directional split.
fn build_pane_sizing(gap_cell_count: u16) -> PaneSizing {
    PaneSizing {
        minimum_size: MIN_PANE_SIZE,
        gap_cell_count,
    }
}

/// Constructs a cell rectangle with the given origin and dimensions.
fn build_cell_rect(column_index: u16, row_index: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: column_index,
            row: row_index,
        },
        Size {
            column_count,
            row_count,
        },
    )
}

/// Solves the layout under `sizing` and returns the rect of the given pane.
fn compute_solved_pane_rect(
    layout_tree: &LayoutNode,
    layout_area: Rect,
    sizing: PaneSizing,
    pane_id: PaneId,
) -> Rect {
    solve_layout_with_sizing(layout_tree, layout_area, sizing)
        .pane_rects
        .into_iter()
        .find(|&(candidate_pane_id, _)| candidate_pane_id == pane_id)
        .expect("pane is in the layout")
        .1
}

fn build_leaf_node(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
}

fn build_two_pane_split(
    direction: SplitDirection,
    left_pane_id: PaneId,
    right_pane_id: PaneId,
) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(right_pane_id),
        ],
    ))
}

/// Solves the layout and returns the allocated size for the given pane.
fn compute_solved_pane_size(layout_tree: &LayoutNode, layout_area: Rect, pane_id: PaneId) -> Size {
    solve_layout(layout_tree, layout_area)
        .pane_rects
        .into_iter()
        .find(|&(candidate_pane_id, _)| candidate_pane_id == pane_id)
        .expect("pane is in the layout")
        .1
        .cell_size
}

/// Verifies that the layout tiles the layout area correctly: all cells are occupied,
/// panes do not overlap, and none extend outside the layout area bounds.
fn assert_tiles(layout_tree: &LayoutNode, layout_area: Rect) {
    let layout_result = solve_layout(layout_tree, layout_area);
    check_exact_tiling(&layout_result.pane_rects, layout_area).unwrap();
}

#[test]
fn growing_right_by_one_cell_moves_one_column() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        41
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), right_pane_id).column_count,
        39
    );
    assert_tiles(&resized, build_layout_area());
}

#[test]
fn growing_left_takes_from_the_left_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        right_pane_id,
        Direction::Left,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        39
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), right_pane_id).column_count,
        41
    );
}

#[test]
fn growing_down_and_up_move_rows() {
    let (top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Vertical, top_pane_id, bottom_pane_id);

    let resized_down_layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        top_pane_id,
        Direction::Down,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized_down_layout_tree, build_layout_area(), top_pane_id,)
            .row_count,
        13
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_down_layout_tree,
            build_layout_area(),
            bottom_pane_id,
        )
        .row_count,
        11
    );

    let resized_up_layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        bottom_pane_id,
        Direction::Up,
        2,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized_up_layout_tree, build_layout_area(), top_pane_id,)
            .row_count,
        10
    );
    assert_eq!(
        compute_solved_pane_size(&resized_up_layout_tree, build_layout_area(), bottom_pane_id,)
            .row_count,
        14
    );
}

#[test]
fn shrinking_right_gives_the_cells_to_the_right_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        -3,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        37
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), right_pane_id).column_count,
        43
    );
    assert_tiles(&resized, build_layout_area());
}

#[test]
fn shrinking_left_gives_the_cells_to_the_left_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        right_pane_id,
        Direction::Left,
        -2,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        42
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), right_pane_id).column_count,
        38
    );
}

#[test]
fn a_shrink_mirrors_the_neighbors_grow_on_the_same_border() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let shrunk_layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        -3,
    )
    .unwrap();
    let grown_layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        right_pane_id,
        Direction::Left,
        3,
    )
    .unwrap();
    assert_eq!(shrunk_layout_tree, grown_layout_tree);
}

#[test]
fn shrink_blocked_by_the_panes_own_floor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 10,
        row_count: 24,
    });

    // The left pane solves to five columns and must keep its border-inclusive
    // four: one is spare — on a shrink, the left pane is the donor.
    let resize_error = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        -4,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 4,
            spare_cell_count: 1,
        }
    );

    let allowed_layout_tree = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        -1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, narrow_layout_area, left_pane_id)
            .column_count,
        4
    );
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, narrow_layout_area, right_pane_id)
            .column_count,
        6
    );
}

#[test]
fn shrink_on_a_tab_edge_has_no_border_either() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Left,
        -1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::NoAdjacentBorder {
            pane_id: left_pane_id,
            direction: Direction::Left,
        }
    );
}

#[test]
fn zero_size_returns_the_tree_unchanged() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        0,
    )
    .unwrap();
    assert_eq!(resized, layout_tree);
}

#[test]
fn resizes_accumulate_across_transactions() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&layout_tree, build_layout_area(), left_pane_id).column_count,
        42
    );
    assert_eq!(
        compute_solved_pane_size(&layout_tree, build_layout_area(), right_pane_id).column_count,
        38
    );
}

#[test]
fn nested_pane_resizes_at_the_level_that_owns_the_border() {
    // The left pane sits beside an upper/lower pair: growing the upper pane
    // leftward moves the column border, and growing it downward moves the
    // border it shares with the lower pane.
    let (left_pane_id, upper_pane_id, lower_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(upper_pane_id),
            build_leaf_node(lower_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), vertical_split],
    ));

    let wider = resize_layout(
        &layout_tree,
        build_layout_area(),
        upper_pane_id,
        Direction::Left,
        2,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&wider, build_layout_area(), left_pane_id).column_count,
        38
    );
    assert_eq!(
        compute_solved_pane_size(&wider, build_layout_area(), upper_pane_id).column_count,
        42
    );
    assert_eq!(
        compute_solved_pane_size(&wider, build_layout_area(), lower_pane_id).column_count,
        42
    );

    let taller = resize_layout(
        &layout_tree,
        build_layout_area(),
        upper_pane_id,
        Direction::Down,
        3,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&taller, build_layout_area(), upper_pane_id).row_count,
        15
    );
    assert_eq!(
        compute_solved_pane_size(&taller, build_layout_area(), lower_pane_id).row_count,
        9
    );
    assert_eq!(
        compute_solved_pane_size(&taller, build_layout_area(), left_pane_id).row_count,
        24
    );
    assert_tiles(&taller, build_layout_area());
}

#[test]
fn pane_inside_a_stack_resizes_the_stack_as_a_unit() {
    let (left_pane_id, first_stack_pane_id, second_stack_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_stack_pane_id, second_stack_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack],
    ));

    // Resizing the collapsed member moves the stack's outer border too.
    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        second_stack_pane_id,
        Direction::Left,
        5,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        35
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), first_stack_pane_id).column_count,
        45
    );
}

#[test]
fn resize_inside_a_suppressed_stack_is_refused() {
    // Hand-built: a stack whose rect cannot hold its headers plus the active
    // member, so the solver suppresses the whole stack to zero area. A resize
    // there has no cells to move: it is refused with zero spare and stores no
    // delta, matching the geometry the solver draws.
    let (first_pane_id, nested_pane_id, active_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, active_pane_id], 0);
    stack.children[0] =
        build_two_pane_split(SplitDirection::Horizontal, first_pane_id, nested_pane_id);
    let layout_tree = LayoutNode::Split(stack);

    let undersized_layout_area = Rect::from_size_at_origin(Size {
        column_count: 80,
        row_count: 1,
    });
    let resize_error = resize_layout(
        &layout_tree,
        undersized_layout_area,
        nested_pane_id,
        Direction::Left,
        5,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 5,
            spare_cell_count: 0,
        }
    );
}

#[test]
fn has_adjacent_border_holds_for_inner_dividers_only() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    // The shared column divider is real from either side.
    assert!(has_adjacent_border(
        &layout_tree,
        left_pane_id,
        Direction::Right
    ));
    assert!(has_adjacent_border(
        &layout_tree,
        right_pane_id,
        Direction::Left
    ));
    // The layout area's outer frame has no neighbor on any outer side.
    assert!(!has_adjacent_border(
        &layout_tree,
        left_pane_id,
        Direction::Left
    ));
    assert!(!has_adjacent_border(
        &layout_tree,
        right_pane_id,
        Direction::Right
    ));
    assert!(!has_adjacent_border(
        &layout_tree,
        left_pane_id,
        Direction::Up
    ));
    assert!(!has_adjacent_border(
        &layout_tree,
        left_pane_id,
        Direction::Down
    ));
}

#[test]
fn has_adjacent_border_is_false_above_a_collapsed_stack_header() {
    // The left pane sits beside a stack with an active and a collapsed pane.
    // The active pane's rectangle stops short of the layout area edge to leave the
    // collapsed pane's header row, but the boundary above that header is not a
    // resizable divider — the geometry says "inside bounds", the layout_tree says no.
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

    assert!(
        !has_adjacent_border(&layout_tree, active_pane_id, Direction::Down),
        "the boundary above a stack header has no resizable neighbor"
    );
    // The stack's outer-left border is real: the left pane lies beyond it.
    assert!(has_adjacent_border(
        &layout_tree,
        active_pane_id,
        Direction::Left
    ));
}

#[test]
fn resize_inside_an_active_stack_subtree_sees_the_header_carved_rect() {
    // Hand-built: a stack whose active member is a vertical pair. The
    // resize preflight measures the donor inside the header-carved active
    // rect, accounting for the invisible header row.
    let (first_stack_pane_id, upper_pane_id, lower_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![first_stack_pane_id, upper_pane_id], 1);
    stack.children[1] =
        build_two_pane_split(SplitDirection::Vertical, upper_pane_id, lower_pane_id);
    let layout_tree = LayoutNode::Split(stack);

    // One header row leaves 23 rows for the pair: upper 11, lower 12. The
    // donor above can spare eight rows — its eleven minus the border-inclusive
    // floor of three — measured inside the header-carved active rect, not the
    // whole stack rect.
    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        lower_pane_id,
        Direction::Up,
        11,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 11,
            spare_cell_count: 8,
        }
    );

    let allowed_layout_tree = resize_layout(
        &layout_tree,
        build_layout_area(),
        lower_pane_id,
        Direction::Up,
        8,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, build_layout_area(), upper_pane_id)
            .row_count,
        3
    );
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, build_layout_area(), lower_pane_id)
            .row_count,
        20
    );
}

#[test]
fn resize_inside_a_collapsed_member_moves_the_stack_border() {
    // Hand-built: a collapsed member that is itself a split. Its inner
    // borders are invisible; resizing one of its panes bubbles to the
    // stack's outer border, resizing it as a unit.
    let (left_pane_id, active_stack_pane_id, upper_pane_id, lower_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![active_stack_pane_id, upper_pane_id], 0);
    stack.children[1] =
        build_two_pane_split(SplitDirection::Horizontal, upper_pane_id, lower_pane_id);
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), LayoutNode::Split(stack)],
    ));

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        lower_pane_id,
        Direction::Left,
        5,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        35
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), active_stack_pane_id).column_count,
        45
    );
}

#[test]
fn missing_weights_are_repaired_before_a_resize() {
    // Hand-built: a deserialized split can carry fewer weights than
    // children. The transaction pads the missing ones with the default
    // share instead of panicking when it indexes them.
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(right_pane_id),
        ],
        weights: Vec::new(),
        active_child_index: 0,
    });

    let resized = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        41
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), right_pane_id).column_count,
        39
    );
}

#[test]
fn resize_blocked_by_the_neighbors_floor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 10,
        row_count: 24,
    });

    // The right pane solves to five columns and must keep its border-inclusive four: one
    // is spare.
    let resize_error = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        4,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 4,
            spare_cell_count: 1,
        }
    );

    let allowed_layout_tree = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, narrow_layout_area, left_pane_id)
            .column_count,
        6
    );
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, narrow_layout_area, right_pane_id)
            .column_count,
        4
    );
}

#[test]
fn pane_on_the_tab_edge_has_no_border_on_that_side() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Left,
        1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::NoAdjacentBorder {
            pane_id: left_pane_id,
            direction: Direction::Left,
        }
    );
    // No vertical border exists in a purely horizontal split either.
    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Down,
        1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::NoAdjacentBorder {
            pane_id: left_pane_id,
            direction: Direction::Down,
        }
    );
}

#[test]
fn missing_pane_is_reported_and_the_input_is_unchanged() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let original_layout_tree = layout_tree.clone();

    let missing_pane_id = PaneId::new();
    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        missing_pane_id,
        Direction::Right,
        1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::PaneNotFound {
            pane_id: missing_pane_id
        }
    );
    assert_eq!(layout_tree, original_layout_tree);
}

#[test]
fn resizing_the_only_pane_in_the_tree_has_no_border_anywhere() {
    // A bare single-pane layout_tree: the pane's path to itself is empty, so no
    // ancestor split exists on any axis, on any side.
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Pane(pane_id);

    for direction in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        let resize_error =
            resize_layout(&layout_tree, build_layout_area(), pane_id, direction, 1).unwrap_err();
        assert_eq!(
            resize_error,
            ResizeError::NoAdjacentBorder { pane_id, direction }
        );
    }
}

#[test]
fn middle_pane_in_a_three_way_split_resizes_either_border() {
    let (left_pane_id, middle_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(middle_pane_id),
            build_leaf_node(right_pane_id),
        ],
    ));

    // Baseline three-way split of 80 columns: 26 / 27 / 27.
    let grown_right = resize_layout(
        &layout_tree,
        build_layout_area(),
        middle_pane_id,
        Direction::Right,
        2,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&grown_right, build_layout_area(), left_pane_id).column_count,
        26
    );
    assert_eq!(
        compute_solved_pane_size(&grown_right, build_layout_area(), middle_pane_id).column_count,
        29
    );
    assert_eq!(
        compute_solved_pane_size(&grown_right, build_layout_area(), right_pane_id).column_count,
        25
    );
    assert_tiles(&grown_right, build_layout_area());

    let grown_left = resize_layout(
        &layout_tree,
        build_layout_area(),
        middle_pane_id,
        Direction::Left,
        2,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&grown_left, build_layout_area(), left_pane_id).column_count,
        24
    );
    assert_eq!(
        compute_solved_pane_size(&grown_left, build_layout_area(), middle_pane_id).column_count,
        29
    );
    assert_eq!(
        compute_solved_pane_size(&grown_left, build_layout_area(), right_pane_id).column_count,
        27
    );
    assert_tiles(&grown_left, build_layout_area());
}

#[test]
fn resize_amount_far_exceeding_available_reports_the_exact_spare() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    // The right pane solves to 40 columns; its spare above the border-inclusive floor
    // of 4 is 36. A maximal signed request is rejected with that exact
    // figure, not an overflow or a panic.
    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        i16::MAX,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: i16::MAX as u16,
            spare_cell_count: 36,
        }
    );
}

#[test]
fn a_pane_still_resizes_after_a_sibling_was_closed() {
    // Closing the middle of three columns reflows to two even halves; the
    // survivors carry no stale resize delta, so a following resize moves the
    // border by exactly its cell count.
    use crate::edit::remove_pane;

    let (left_pane_id, closed_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(closed_pane_id),
            build_leaf_node(right_pane_id),
        ],
    ));

    let (after_close, _) = remove_pane(
        &layout_tree,
        build_layout_area(),
        closed_pane_id,
        build_pane_sizing(0),
    )
    .unwrap();
    assert_eq!(
        after_close.list_leaf_pane_ids(),
        [left_pane_id, right_pane_id]
    );
    // The two survivors share the layout area evenly with no leftover delta.
    assert_eq!(
        compute_solved_pane_size(&after_close, build_layout_area(), left_pane_id).column_count,
        40
    );
    assert_eq!(
        compute_solved_pane_size(&after_close, build_layout_area(), right_pane_id).column_count,
        40
    );

    // Growing the left pane's right border by five moves exactly five columns.
    let resized = resize_layout(
        &after_close,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        5,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), left_pane_id).column_count,
        45
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_layout_area(), right_pane_id).column_count,
        35
    );
    assert_tiles(&resized, build_layout_area());
}

#[test]
fn a_request_exactly_at_the_spare_boundary_succeeds_one_past_it_fails() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 10,
        row_count: 24,
    });

    // The right pane solves to five columns with a border-inclusive floor of four: one
    // spare cell exactly. Taking exactly that one cell succeeds; asking
    // for one more is rejected with the same spare figure.
    let allowed_layout_tree = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, narrow_layout_area, left_pane_id)
            .column_count,
        6
    );
    assert_eq!(
        compute_solved_pane_size(&allowed_layout_tree, narrow_layout_area, right_pane_id)
            .column_count,
        4
    );

    let resize_error = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        2,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 2,
            spare_cell_count: 1,
        }
    );
}

#[test]
fn a_resize_moves_the_border_by_its_cell_count_with_a_gap_reserved() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let sizing = PaneSizing {
        minimum_size: MIN_PANE_SIZE,
        gap_cell_count: 2,
    };

    // The two-column gap comes off the 80-column axis first, so the two
    // panes share 78 cells as 39 each.
    assert_eq!(
        compute_solved_pane_rect(&layout_tree, build_layout_area(), sizing, left_pane_id),
        build_cell_rect(0, 0, 39, 24)
    );
    assert_eq!(
        compute_solved_pane_rect(&layout_tree, build_layout_area(), sizing, right_pane_id),
        build_cell_rect(41, 0, 39, 24)
    );

    // Growing the left pane's right border by one column moves exactly one column
    // across; the gap keeps its two cells.
    let resized = resize_layout_with_sizing(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        1,
        sizing,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_rect(&resized, build_layout_area(), sizing, left_pane_id),
        build_cell_rect(0, 0, 40, 24)
    );
    assert_eq!(
        compute_solved_pane_rect(&resized, build_layout_area(), sizing, right_pane_id),
        build_cell_rect(42, 0, 38, 24)
    );
}

#[test]
fn the_spare_across_a_gap_comes_from_the_donor_rect_the_gap_left() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let sizing = PaneSizing {
        minimum_size: MIN_PANE_SIZE,
        gap_cell_count: 2,
    };

    // The right pane holds 39 of the 78 columns past the gap and floors at four, so it
    // can give exactly 35. Taking all 35 leaves the right pane at its floor two
    // cells past the left pane's new edge; asking for one more is refused with
    // that same spare.
    let allowed_layout_tree = resize_layout_with_sizing(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        35,
        sizing,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_rect(
            &allowed_layout_tree,
            build_layout_area(),
            sizing,
            left_pane_id,
        ),
        build_cell_rect(0, 0, 74, 24)
    );
    assert_eq!(
        compute_solved_pane_rect(
            &allowed_layout_tree,
            build_layout_area(),
            sizing,
            right_pane_id,
        ),
        build_cell_rect(76, 0, 4, 24)
    );

    let resize_error = resize_layout_with_sizing(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        36,
        sizing,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 36,
            spare_cell_count: 35,
        }
    );
}

#[test]
fn a_minimum_signed_shrink_reports_its_full_magnitude() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    // On a shrink the left pane is the donor: 40 columns above its floor of 4 leave 36.
    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        i16::MIN,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 32_768,
            spare_cell_count: 36,
        }
    );
}

#[test]
fn a_zero_resize_still_reports_a_missing_pane_or_border() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let missing_pane_id = PaneId::new();

    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        missing_pane_id,
        Direction::Right,
        0,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::PaneNotFound {
            pane_id: missing_pane_id
        }
    );
    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Left,
        0,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::NoAdjacentBorder {
            pane_id: left_pane_id,
            direction: Direction::Left,
        }
    );
}

#[test]
fn has_adjacent_border_is_false_for_a_pane_not_in_the_tree() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    assert!(!has_adjacent_border(
        &layout_tree,
        PaneId::new(),
        Direction::Right
    ));
}

#[test]
fn has_adjacent_border_bubbles_out_of_a_collapsed_member() {
    // The left pane sits beside a stack with an active pane and a collapsed
    // split. The inner divider sits under the header, but the stack's left
    // border beside the left pane is real.
    let (left_pane_id, active_pane_id, upper_pane_id, lower_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![active_pane_id, upper_pane_id], 0);
    stack.children[1] =
        build_two_pane_split(SplitDirection::Horizontal, upper_pane_id, lower_pane_id);
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), LayoutNode::Split(stack)],
    ));

    assert!(!has_adjacent_border(
        &layout_tree,
        upper_pane_id,
        Direction::Right
    ));
    assert!(has_adjacent_border(
        &layout_tree,
        lower_pane_id,
        Direction::Left
    ));
    assert!(!has_adjacent_border(
        &layout_tree,
        lower_pane_id,
        Direction::Right
    ));
}

#[test]
fn a_suppressed_donor_has_no_spare_cells() {
    // Three columns in nine cells: the left and middle panes keep their
    // four-cell floors and the right pane is suppressed. Growing the middle
    // pane rightward asks the right pane, which holds nothing.
    let (left_pane_id, middle_pane_id, suppressed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(middle_pane_id),
            build_leaf_node(suppressed_pane_id),
        ],
    ));
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 9,
        row_count: 24,
    });

    assert_eq!(
        solve_layout(&layout_tree, narrow_layout_area).suppressed_pane_ids,
        [suppressed_pane_id]
    );
    let resize_error = resize_layout(
        &layout_tree,
        narrow_layout_area,
        middle_pane_id,
        Direction::Right,
        1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 1,
            spare_cell_count: 0,
        }
    );
}

#[test]
fn a_collapsed_member_of_a_root_stack_has_no_border() {
    let (active_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![active_pane_id, collapsed_pane_id],
        0,
    ));

    let resize_error = resize_layout(
        &layout_tree,
        build_layout_area(),
        collapsed_pane_id,
        Direction::Left,
        1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::NoAdjacentBorder {
            pane_id: collapsed_pane_id,
            direction: Direction::Left,
        }
    );
    assert!(!has_adjacent_border(
        &layout_tree,
        active_pane_id,
        Direction::Down
    ));
}

#[test]
fn a_larger_content_minimum_raises_the_donor_floor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let sizing = PaneSizing {
        minimum_size: Size {
            column_count: 10,
            row_count: 5,
        },
        gap_cell_count: 0,
    };

    // The right pane holds 40 columns and floors at twelve, its ten-column content
    // minimum plus the two border columns: 28 are spare.
    let resize_error = resize_layout_with_sizing(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        29,
        sizing,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::MinimumSizeExceeded {
            requested_cell_count: 29,
            spare_cell_count: 28,
        }
    );

    let allowed_layout_tree = resize_layout_with_sizing(
        &layout_tree,
        build_layout_area(),
        left_pane_id,
        Direction::Right,
        28,
        sizing,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_rect(
            &allowed_layout_tree,
            build_layout_area(),
            sizing,
            right_pane_id,
        ),
        build_cell_rect(68, 0, 12, 24)
    );
}
