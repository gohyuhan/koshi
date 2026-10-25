//! Tests for resize transactions: moving pane borders by up to a signed cell
//! count — outward grows the pane, inward shrinks it toward the neighbor.

use koshi_core::geometry::Size;
use koshi_test_support::fixtures::build_cell_rect;
use koshi_test_support::layout_assert::check_exact_tiling;

use super::*;
use crate::solver::tests::{build_pane_sizing, build_size_weight_strategy};

use crate::solver::{solve_layout, solve_layout_with_sizing};
use crate::tree::{LayoutNode, SplitNode};

/// [`resize_layout_with_sizing`] with [`PaneSizing::default`].
fn resize_layout(
    layout_tree: &LayoutNode,
    tab_rect: Rect,
    pane_id: PaneId,
    direction: Direction,
    resize_cell_delta: i16,
) -> Result<(LayoutNode, u16), ResizeError> {
    resize_layout_with_sizing(
        layout_tree,
        tab_rect,
        pane_id,
        direction,
        resize_cell_delta,
        PaneSizing::default(),
    )
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

/// Solves the layout under `pane_sizing` and returns the rect of the given pane.
fn compute_solved_pane_rect(
    layout_tree: &LayoutNode,
    layout_area: Rect,
    pane_sizing: PaneSizing,
    pane_id: PaneId,
) -> Rect {
    solve_layout_with_sizing(layout_tree, layout_area, pane_sizing)
        .find_pane_rect(pane_id)
        .expect("pane is in the layout")
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
        .find_pane_rect(pane_id)
        .expect("pane is in the layout")
        .size
}

/// Verifies that the layout tiles the layout area correctly: all cells are occupied,
/// panes do not overlap, and none extend outside the layout area bounds.
fn assert_tiles(layout_tree: &LayoutNode, layout_area: Rect) {
    let layout_solve = solve_layout(layout_tree, layout_area);
    check_exact_tiling(&layout_solve.pane_rects, layout_area).unwrap();
}

#[test]
fn growing_right_by_one_cell_moves_one_column() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let (resized_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            left_pane_id
        )
        .column_count,
        41
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            right_pane_id
        )
        .column_count,
        39
    );
    assert_tiles(&resized_layout_tree, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn growing_left_takes_from_the_left_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let (resized_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        right_pane_id,
        Direction::Left,
        1,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            left_pane_id
        )
        .column_count,
        39
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            right_pane_id
        )
        .column_count,
        41
    );
}

#[test]
fn growing_down_and_up_move_rows() {
    let (top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Vertical, top_pane_id, bottom_pane_id);

    let (resized_down_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        top_pane_id,
        Direction::Down,
        1,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_size(
            &resized_down_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            top_pane_id,
        )
        .row_count,
        13
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_down_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            bottom_pane_id,
        )
        .row_count,
        11
    );

    let (resized_up_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        bottom_pane_id,
        Direction::Up,
        2,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 2);
    assert_eq!(
        compute_solved_pane_size(
            &resized_up_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            top_pane_id,
        )
        .row_count,
        10
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_up_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            bottom_pane_id,
        )
        .row_count,
        14
    );
}

#[test]
fn shrinking_right_gives_the_cells_to_the_right_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let (resized_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        -3,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 3);
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            left_pane_id
        )
        .column_count,
        37
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            right_pane_id
        )
        .column_count,
        43
    );
    assert_tiles(&resized_layout_tree, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn shrinking_left_gives_the_cells_to_the_left_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let (resized_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        right_pane_id,
        Direction::Left,
        -2,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 2);
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            left_pane_id
        )
        .column_count,
        42
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            right_pane_id
        )
        .column_count,
        38
    );
}

#[test]
fn a_shrink_mirrors_the_neighbors_grow_on_the_same_border() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let (shrunk_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        -3,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 3);
    let (grown_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        right_pane_id,
        Direction::Left,
        3,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 3);
    assert_eq!(shrunk_layout_tree, grown_layout_tree);
}

#[test]
fn a_donor_whose_floor_passes_u16_max_has_no_spare_cell() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let widest_layout_area = Rect::from_size_at_origin(Size {
        column_count: u16::MAX,
        row_count: 24,
    });
    let pane_sizing = PaneSizing {
        minimum_size: Size {
            column_count: u16::MAX - 1,
            row_count: 1,
        },
        gap_cell_count: 0,
    };

    // Each pane needs 65536 columns with its border: the right pane donates
    // from a floor of 65536.
    let resize_error = resize_layout_with_sizing(
        &layout_tree,
        widest_layout_area,
        left_pane_id,
        Direction::Right,
        1,
        pane_sizing,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::DonorAtMinimumSize {
            pane_id: left_pane_id,
            direction: Direction::Right,
            is_pane_growing: true,
        }
    );
}

#[test]
fn a_shrink_past_the_panes_own_floor_stops_at_the_floor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 10,
        row_count: 24,
    });

    // The left pane solves to five columns and must keep its border-inclusive
    // four: on a shrink the left pane is the donor, and asking for four moves
    // one column.
    let (shrunk_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        -4,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_size(&shrunk_layout_tree, narrow_layout_area, left_pane_id)
            .column_count,
        4
    );
    assert_eq!(
        compute_solved_pane_size(&shrunk_layout_tree, narrow_layout_area, right_pane_id)
            .column_count,
        6
    );

    // At its floor the left pane gives no further column.
    let resize_error = resize_layout(
        &shrunk_layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        -1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::DonorAtMinimumSize {
            pane_id: left_pane_id,
            direction: Direction::Right,
            is_pane_growing: false,
        }
    );
}

#[test]
fn shrink_on_a_tab_edge_has_no_border_either() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resize_error = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
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

    let (resized, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        0,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 0);
    assert_eq!(resized, layout_tree);
}

#[test]
fn resizes_accumulate_across_transactions() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    (layout_tree, _) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    (layout_tree, _) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(
        compute_solved_pane_size(&layout_tree, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        42
    );
    assert_eq!(
        compute_solved_pane_size(&layout_tree, build_cell_rect(0, 0, 80, 24), right_pane_id)
            .column_count,
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

    let (wider, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        upper_pane_id,
        Direction::Left,
        2,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 2);
    assert_eq!(
        compute_solved_pane_size(&wider, build_cell_rect(0, 0, 80, 24), left_pane_id).column_count,
        38
    );
    assert_eq!(
        compute_solved_pane_size(&wider, build_cell_rect(0, 0, 80, 24), upper_pane_id).column_count,
        42
    );
    assert_eq!(
        compute_solved_pane_size(&wider, build_cell_rect(0, 0, 80, 24), lower_pane_id).column_count,
        42
    );

    let (taller, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        upper_pane_id,
        Direction::Down,
        3,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 3);
    assert_eq!(
        compute_solved_pane_size(&taller, build_cell_rect(0, 0, 80, 24), upper_pane_id).row_count,
        15
    );
    assert_eq!(
        compute_solved_pane_size(&taller, build_cell_rect(0, 0, 80, 24), lower_pane_id).row_count,
        9
    );
    assert_eq!(
        compute_solved_pane_size(&taller, build_cell_rect(0, 0, 80, 24), left_pane_id).row_count,
        24
    );
    assert_tiles(&taller, build_cell_rect(0, 0, 80, 24));
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
    let (resized, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        second_stack_pane_id,
        Direction::Left,
        5,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 5);
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        35
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), first_stack_pane_id)
            .column_count,
        45
    );
}

#[test]
fn resize_inside_a_suppressed_stack_is_refused() {
    // Hand-built: a stack whose rect cannot hold its headers plus the active
    // member, so the solver suppresses the whole stack to zero area. A resize
    // there has no cells to move: it is refused and stores no delta.
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
        ResizeError::DonorAtMinimumSize {
            pane_id: nested_pane_id,
            direction: Direction::Left,
            is_pane_growing: true,
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
    // whole stack rect. Asking for eleven moves those eight.
    let (allowed_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        lower_pane_id,
        Direction::Up,
        11,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 8);
    assert_eq!(
        compute_solved_pane_size(
            &allowed_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            upper_pane_id
        )
        .row_count,
        3
    );
    assert_eq!(
        compute_solved_pane_size(
            &allowed_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            lower_pane_id
        )
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

    let (resized, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        lower_pane_id,
        Direction::Left,
        5,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 5);
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        35
    );
    assert_eq!(
        compute_solved_pane_size(
            &resized,
            build_cell_rect(0, 0, 80, 24),
            active_stack_pane_id
        )
        .column_count,
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

    let (resized, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        41
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), right_pane_id)
            .column_count,
        39
    );
}

#[test]
fn a_grow_past_the_neighbors_floor_stops_at_the_floor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 10,
        row_count: 24,
    });

    // The right pane solves to five columns and must keep its border-inclusive
    // four: asking for four moves the one spare column.
    let (grown_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        4,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_size(&grown_layout_tree, narrow_layout_area, left_pane_id).column_count,
        6
    );
    assert_eq!(
        compute_solved_pane_size(&grown_layout_tree, narrow_layout_area, right_pane_id)
            .column_count,
        4
    );

    // At its floor the right pane donates no further column.
    let resize_error = resize_layout(
        &grown_layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap_err();
    assert_eq!(
        resize_error,
        ResizeError::DonorAtMinimumSize {
            pane_id: left_pane_id,
            direction: Direction::Right,
            is_pane_growing: true,
        }
    );
}

#[test]
fn pane_on_the_tab_edge_has_no_border_on_that_side() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    let resize_error = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
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
        build_cell_rect(0, 0, 80, 24),
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
        build_cell_rect(0, 0, 80, 24),
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
        let resize_error = resize_layout(
            &layout_tree,
            build_cell_rect(0, 0, 80, 24),
            pane_id,
            direction,
            1,
        )
        .unwrap_err();
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
    let (grown_right, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        middle_pane_id,
        Direction::Right,
        2,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 2);
    assert_eq!(
        compute_solved_pane_size(&grown_right, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        26
    );
    assert_eq!(
        compute_solved_pane_size(&grown_right, build_cell_rect(0, 0, 80, 24), middle_pane_id)
            .column_count,
        29
    );
    assert_eq!(
        compute_solved_pane_size(&grown_right, build_cell_rect(0, 0, 80, 24), right_pane_id)
            .column_count,
        25
    );
    assert_tiles(&grown_right, build_cell_rect(0, 0, 80, 24));

    let (grown_left, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        middle_pane_id,
        Direction::Left,
        2,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 2);
    assert_eq!(
        compute_solved_pane_size(&grown_left, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        24
    );
    assert_eq!(
        compute_solved_pane_size(&grown_left, build_cell_rect(0, 0, 80, 24), middle_pane_id)
            .column_count,
        29
    );
    assert_eq!(
        compute_solved_pane_size(&grown_left, build_cell_rect(0, 0, 80, 24), right_pane_id)
            .column_count,
        27
    );
    assert_tiles(&grown_left, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn a_maximal_grow_moves_exactly_the_neighbors_spare() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    // The right pane solves to 40 columns; its spare above the border-inclusive
    // floor of 4 is 36. A maximal signed request moves exactly those 36, with
    // no overflow and no panic.
    let (grown_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        i16::MAX,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 36);
    assert_eq!(
        compute_solved_pane_size(
            &grown_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            left_pane_id
        )
        .column_count,
        76
    );
    assert_eq!(
        compute_solved_pane_size(
            &grown_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            right_pane_id
        )
        .column_count,
        4
    );
    assert_tiles(&grown_layout_tree, build_cell_rect(0, 0, 80, 24));
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

    let after_close = remove_pane(&layout_tree, closed_pane_id).unwrap();
    assert_eq!(
        after_close.list_leaf_pane_ids(),
        [left_pane_id, right_pane_id]
    );
    // The two survivors share the layout area evenly with no leftover delta.
    assert_eq!(
        compute_solved_pane_size(&after_close, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        40
    );
    assert_eq!(
        compute_solved_pane_size(&after_close, build_cell_rect(0, 0, 80, 24), right_pane_id)
            .column_count,
        40
    );

    // Growing the left pane's right border by five moves exactly five columns.
    let (resized, moved_cell_count) = resize_layout(
        &after_close,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        5,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 5);
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), left_pane_id)
            .column_count,
        45
    );
    assert_eq!(
        compute_solved_pane_size(&resized, build_cell_rect(0, 0, 80, 24), right_pane_id)
            .column_count,
        35
    );
    assert_tiles(&resized, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn a_request_one_past_the_spare_moves_exactly_the_spare() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let narrow_layout_area = Rect::from_size_at_origin(Size {
        column_count: 10,
        row_count: 24,
    });

    // The right pane solves to five columns with a border-inclusive floor of four: one
    // spare cell exactly. Taking exactly that one cell moves it; asking for
    // one more moves the same one cell to the same tree.
    let (allowed_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        1,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
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

    let (grown_past_spare_layout_tree, moved_past_spare_cell_count) = resize_layout(
        &layout_tree,
        narrow_layout_area,
        left_pane_id,
        Direction::Right,
        2,
    )
    .unwrap();
    assert_eq!(moved_past_spare_cell_count, 1);
    assert_eq!(grown_past_spare_layout_tree, allowed_layout_tree);
}

#[test]
fn a_resize_moves_the_border_by_its_cell_count_with_a_gap_reserved() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let sizing = build_pane_sizing(2);

    // The two-column gap comes off the 80-column axis first, so the two
    // panes share 78 cells as 39 each.
    assert_eq!(
        compute_solved_pane_rect(
            &layout_tree,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            left_pane_id
        ),
        build_cell_rect(0, 0, 39, 24)
    );
    assert_eq!(
        compute_solved_pane_rect(
            &layout_tree,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            right_pane_id
        ),
        build_cell_rect(41, 0, 39, 24)
    );

    // Growing the left pane's right border by one column moves exactly one column
    // across; the gap keeps its two cells.
    let (resized, moved_cell_count) = resize_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        1,
        sizing,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(
        compute_solved_pane_rect(
            &resized,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            left_pane_id
        ),
        build_cell_rect(0, 0, 40, 24)
    );
    assert_eq!(
        compute_solved_pane_rect(
            &resized,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            right_pane_id
        ),
        build_cell_rect(42, 0, 38, 24)
    );
}

#[test]
fn the_spare_across_a_gap_comes_from_the_donor_rect_the_gap_left() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let sizing = build_pane_sizing(2);

    // The right pane holds 39 of the 78 columns past the gap and floors at four, so it
    // can give exactly 35. Taking all 35 leaves the right pane at its floor two
    // cells past the left pane's new edge; asking for one more moves the same
    // 35 to the same tree.
    let (allowed_layout_tree, moved_cell_count) = resize_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        35,
        sizing,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 35);
    assert_eq!(
        compute_solved_pane_rect(
            &allowed_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            left_pane_id,
        ),
        build_cell_rect(0, 0, 74, 24)
    );
    assert_eq!(
        compute_solved_pane_rect(
            &allowed_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            right_pane_id,
        ),
        build_cell_rect(76, 0, 4, 24)
    );

    let (grown_past_spare_layout_tree, moved_past_spare_cell_count) = resize_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        36,
        sizing,
    )
    .unwrap();
    assert_eq!(moved_past_spare_cell_count, 35);
    assert_eq!(grown_past_spare_layout_tree, allowed_layout_tree);
}

#[test]
fn a_minimum_signed_shrink_moves_exactly_the_panes_spare() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);

    // On a shrink the left pane is the donor: 40 columns above its floor of 4
    // leave 36, and a request of 32768 moves those 36.
    let (shrunk_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        i16::MIN,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 36);
    assert_eq!(
        compute_solved_pane_size(
            &shrunk_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            left_pane_id
        )
        .column_count,
        4
    );
    assert_eq!(
        compute_solved_pane_size(
            &shrunk_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            right_pane_id
        )
        .column_count,
        76
    );
}

#[test]
fn a_zero_resize_still_reports_a_missing_pane_or_border() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let missing_pane_id = PaneId::new();

    let resize_error = resize_layout(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
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
        build_cell_rect(0, 0, 80, 24),
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
    // pane rightward asks the right pane, which holds nothing, and so does a
    // shrink of the right pane from its left border.
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
        ResizeError::DonorAtMinimumSize {
            pane_id: middle_pane_id,
            direction: Direction::Right,
            is_pane_growing: true,
        }
    );
    assert_eq!(
        resize_layout(
            &layout_tree,
            narrow_layout_area,
            suppressed_pane_id,
            Direction::Left,
            -1,
        ),
        Err(ResizeError::DonorAtMinimumSize {
            pane_id: suppressed_pane_id,
            direction: Direction::Left,
            is_pane_growing: false,
        })
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
        build_cell_rect(0, 0, 80, 24),
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
    // minimum plus the two border columns: 28 are spare, and asking for 29
    // moves those 28.
    let (allowed_layout_tree, moved_cell_count) = resize_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        left_pane_id,
        Direction::Right,
        29,
        sizing,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 28);
    assert_eq!(
        compute_solved_pane_rect(
            &allowed_layout_tree,
            build_cell_rect(0, 0, 80, 24),
            sizing,
            right_pane_id,
        ),
        build_cell_rect(68, 0, 12, 24)
    );
}

/// A horizontal split of `left_pane_id` and `right_pane_id` in equal shares,
/// whose left child aims for `preferred_cell_count` columns.
fn build_preferred_left_split(
    left_pane_id: PaneId,
    right_pane_id: PaneId,
    preferred_cell_count: u16,
) -> LayoutNode {
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(right_pane_id),
        ],
    );
    split.weights[0].preferred_cell_count = Some(preferred_cell_count);
    LayoutNode::Split(split)
}

#[test]
fn a_negative_resize_delta_moves_a_pane_below_its_preferred_size() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_preferred_left_split(left_pane_id, right_pane_id, 20);
    let mut offset_layout_tree = layout_tree.clone();
    let LayoutNode::Split(offset_split) = &mut offset_layout_tree else {
        unreachable!("the tree is a split");
    };
    offset_split.weights[0].resize_delta = -10;
    offset_split.weights[1].resize_delta = 10;

    // The left pane's 40-column share is cut to its preferred 20, and the
    // right pane takes the rest. Ten cells of offset away from the left pane
    // then move the border ten columns left.
    for (solved_layout_tree, expected_column_counts) in
        [(&layout_tree, [20, 60]), (&offset_layout_tree, [10, 70])]
    {
        assert_eq!(
            [left_pane_id, right_pane_id].map(|pane_id| compute_solved_pane_size(
                solved_layout_tree,
                build_cell_rect(0, 0, 80, 24),
                pane_id
            )
            .column_count),
            expected_column_counts
        );
    }
}

#[test]
fn a_border_beside_a_pane_at_its_preferred_size_moves_in_every_direction() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_preferred_left_split(left_pane_id, right_pane_id, 20);
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // The left pane holds 20 columns, 16 above its floor of 4. A request
    // larger than that moves the border 16 columns.
    let moving_resizes = [
        (right_pane_id, Direction::Left, 5, 5, [15, 65], [-5, 5]),
        (left_pane_id, Direction::Right, 5, 5, [25, 55], [5, -5]),
        (left_pane_id, Direction::Right, -5, 5, [15, 65], [-5, 5]),
        (
            right_pane_id,
            Direction::Left,
            i16::MAX,
            16,
            [4, 76],
            [-16, 16],
        ),
    ];
    for (
        resized_pane_id,
        direction,
        resize_cell_delta,
        expected_moved_cell_count,
        expected_column_counts,
        expected_resize_deltas,
    ) in moving_resizes
    {
        let (resized_layout_tree, moved_cell_count) = resize_layout(
            &layout_tree,
            layout_area,
            resized_pane_id,
            direction,
            resize_cell_delta,
        )
        .unwrap();
        assert_eq!(moved_cell_count, expected_moved_cell_count);
        assert_eq!(
            [left_pane_id, right_pane_id].map(|pane_id| compute_solved_pane_size(
                &resized_layout_tree,
                layout_area,
                pane_id
            )
            .column_count),
            expected_column_counts
        );
        let LayoutNode::Split(resized_split) = &resized_layout_tree else {
            unreachable!("the tree is a split");
        };
        assert_eq!(
            resized_split
                .weights
                .iter()
                .map(|size_weight| size_weight.resize_delta)
                .collect::<Vec<i32>>(),
            expected_resize_deltas
        );
    }
}

#[test]
fn a_border_moves_until_a_donor_with_a_preferred_size_reaches_its_floor() {
    // Ninety columns in three equal shares. The middle pane aims for ten, and
    // its surplus goes to the right pane: 30, 10, 50.
    let (left_pane_id, middle_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(middle_pane_id),
            build_leaf_node(right_pane_id),
        ],
    );
    split.weights[1].preferred_cell_count = Some(10);
    let layout_tree = LayoutNode::Split(split);
    let layout_area = build_cell_rect(0, 0, 90, 24);
    let list_column_counts = |solved_layout_tree: &LayoutNode| {
        [left_pane_id, middle_pane_id, right_pane_id].map(|pane_id| {
            compute_solved_pane_size(solved_layout_tree, layout_area, pane_id).column_count
        })
    };
    assert_eq!(list_column_counts(&layout_tree), [30, 10, 50]);

    // Growing the left pane by five moves the border five columns, and the
    // middle pane gives the cells.
    let (grown_layout_tree, moved_cell_count) =
        resize_layout(&layout_tree, layout_area, left_pane_id, Direction::Right, 5).unwrap();
    assert_eq!(moved_cell_count, 5);
    assert_eq!(list_column_counts(&grown_layout_tree), [35, 5, 50]);

    // Asking for 50 moves the 6 columns the middle pane holds above its floor
    // of 4, and keeps the 6 cells of offset that moved them.
    let (grown_to_floor_layout_tree, moved_to_floor_cell_count) = resize_layout(
        &layout_tree,
        layout_area,
        left_pane_id,
        Direction::Right,
        50,
    )
    .unwrap();
    assert_eq!(moved_to_floor_cell_count, 6);
    assert_eq!(list_column_counts(&grown_to_floor_layout_tree), [36, 4, 50]);
    let LayoutNode::Split(grown_to_floor_split) = &grown_to_floor_layout_tree else {
        unreachable!("the tree is a split");
    };
    assert_eq!(
        grown_to_floor_split
            .weights
            .iter()
            .map(|size_weight| size_weight.resize_delta)
            .collect::<Vec<i32>>(),
        vec![6, -6, 0]
    );
}

#[test]
fn a_border_beside_a_preferred_pane_moves_to_the_donors_floor() {
    // Sixty columns in three equal shares. The left pane aims for ten, and its
    // surplus goes to the right pane: 10, 20, 30.
    let (left_pane_id, middle_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_leaf_node(middle_pane_id),
            build_leaf_node(right_pane_id),
        ],
    );
    split.weights[0].preferred_cell_count = Some(10);
    let layout_tree = LayoutNode::Split(split);
    let layout_area = build_cell_rect(0, 0, 60, 24);
    let list_column_counts = |solved_layout_tree: &LayoutNode| {
        [left_pane_id, middle_pane_id, right_pane_id].map(|pane_id| {
            compute_solved_pane_size(solved_layout_tree, layout_area, pane_id).column_count
        })
    };
    assert_eq!(list_column_counts(&layout_tree), [10, 20, 30]);

    // The middle pane grows by the 26 columns the right pane holds above its
    // floor of 4, and the tree keeps the 26 cells of offset that moved them.
    let (grown_layout_tree, moved_cell_count) = resize_layout(
        &layout_tree,
        layout_area,
        middle_pane_id,
        Direction::Right,
        200,
    )
    .unwrap();
    assert_eq!(moved_cell_count, 26);
    assert_eq!(list_column_counts(&grown_layout_tree), [10, 46, 4]);
    let LayoutNode::Split(grown_split) = &grown_layout_tree else {
        unreachable!("the tree is a split");
    };
    assert_eq!(
        grown_split
            .weights
            .iter()
            .map(|size_weight| size_weight.resize_delta)
            .collect::<Vec<i32>>(),
        vec![0, 26, -26]
    );

    // The next press finds the right pane at its floor.
    assert_eq!(
        resize_layout(
            &grown_layout_tree,
            layout_area,
            middle_pane_id,
            Direction::Right,
            1
        ),
        Err(ResizeError::DonorAtMinimumSize {
            pane_id: middle_pane_id,
            direction: Direction::Right,
            is_pane_growing: true,
        })
    );
}

/// A vertical split of `top_pane_id` over a vertical pair of `middle_pane_id`
/// and `bottom_pane_id`, in equal shares.
fn build_top_over_pair_split(
    top_pane_id: PaneId,
    middle_pane_id: PaneId,
    bottom_pane_id: PaneId,
) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_two_pane_split(SplitDirection::Vertical, middle_pane_id, bottom_pane_id),
        ],
    ))
}

#[test]
fn an_offset_cell_that_a_floor_absorbs_is_followed_by_one_that_moves_the_border() {
    let (top_pane_id, middle_pane_id, bottom_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_top_over_pair_split(top_pane_id, middle_pane_id, bottom_pane_id);
    let layout_area = build_cell_rect(0, 0, 80, 10);
    let list_row_counts = |solved_layout_tree: &LayoutNode| {
        [top_pane_id, middle_pane_id, bottom_pane_id].map(|pane_id| {
            compute_solved_pane_size(solved_layout_tree, layout_area, pane_id).row_count
        })
    };
    // Ten rows share as 5 and 5, and the pair's floor of 6 rows takes one row
    // from the top pane: 4 over 3 and 3.
    assert_eq!(list_row_counts(&layout_tree), [4, 3, 3]);

    // The first offset cell only lifts the pair's share to its floor; the
    // second moves the border one row up. One press keeps both cells.
    let (pressed_layout_tree, moved_cell_count) =
        resize_layout(&layout_tree, layout_area, middle_pane_id, Direction::Up, 1).unwrap();
    assert_eq!(moved_cell_count, 1);
    assert_eq!(list_row_counts(&pressed_layout_tree), [3, 3, 4]);
    let LayoutNode::Split(pressed_split) = &pressed_layout_tree else {
        unreachable!("the tree is a split");
    };
    assert_eq!(
        pressed_split
            .weights
            .iter()
            .map(|size_weight| size_weight.resize_delta)
            .collect::<Vec<i32>>(),
        vec![-2, 2]
    );

    // Asking for two lands on the same tree: the top pane is then at its
    // floor of 3 rows.
    let (grown_layout_tree, grown_moved_cell_count) =
        resize_layout(&layout_tree, layout_area, middle_pane_id, Direction::Up, 2).unwrap();
    assert_eq!(grown_moved_cell_count, 1);
    assert_eq!(grown_layout_tree, pressed_layout_tree);

    // A second press finds the top pane at its floor.
    assert_eq!(
        resize_layout(
            &pressed_layout_tree,
            layout_area,
            middle_pane_id,
            Direction::Up,
            1
        ),
        Err(ResizeError::DonorAtMinimumSize {
            pane_id: middle_pane_id,
            direction: Direction::Up,
            is_pane_growing: true,
        })
    );
}

#[test]
fn a_receiver_lifted_to_its_floor_by_a_third_pane_holds_the_border() {
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
    // At 300 columns the left pane shrinks by 60 and the right pane grows by
    // 80: 40, 80, 180.
    let wide_layout_area = build_cell_rect(0, 0, 300, 24);
    let (shrunk_layout_tree, _) = resize_layout(
        &layout_tree,
        wide_layout_area,
        left_pane_id,
        Direction::Right,
        -60,
    )
    .unwrap();
    let (offset_layout_tree, _) = resize_layout(
        &shrunk_layout_tree,
        wide_layout_area,
        right_pane_id,
        Direction::Left,
        80,
    )
    .unwrap();
    let LayoutNode::Split(offset_split) = &offset_layout_tree else {
        unreachable!("the tree is a split");
    };
    assert_eq!(
        offset_split
            .weights
            .iter()
            .map(|size_weight| size_weight.resize_delta)
            .collect::<Vec<i32>>(),
        vec![-60, -20, 80]
    );

    // At 90 columns each share is 30. The left pane's share plus its offset
    // is 0, and the right pane gives the 4 columns of its floor: 4, 10, 76.
    let narrow_layout_area = build_cell_rect(0, 0, 90, 24);
    assert_eq!(
        [left_pane_id, middle_pane_id, right_pane_id].map(|pane_id| {
            compute_solved_pane_size(&offset_layout_tree, narrow_layout_area, pane_id).column_count
        }),
        [4, 10, 76]
    );

    // The middle pane holds 6 columns above its floor of 4. Each offset cell
    // it gives goes to the right pane, and the left pane stays at its floor
    // until the middle pane is at its own.
    let held_resizes = [
        (left_pane_id, Direction::Right, 1, true),
        (middle_pane_id, Direction::Left, -6, false),
    ];
    for (resized_pane_id, direction, resize_cell_delta, is_pane_growing) in held_resizes {
        assert_eq!(
            resize_layout(
                &offset_layout_tree,
                narrow_layout_area,
                resized_pane_id,
                direction,
                resize_cell_delta,
            ),
            Err(ResizeError::BorderHeld {
                pane_id: resized_pane_id,
                direction,
                is_pane_growing,
            })
        );
    }
}

#[test]
fn a_border_beside_a_suppressed_pane_is_held() {
    // Three columns in nine cells: the left and middle panes solve to 4 and
    // 5, and the right pane is suppressed. The middle pane holds one column
    // above its floor of 4, and the suppressed pane takes no cell.
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
    let narrow_layout_area = build_cell_rect(0, 0, 9, 24);
    assert_eq!(
        solve_layout(&layout_tree, narrow_layout_area).suppressed_pane_ids,
        [suppressed_pane_id]
    );

    let held_resizes = [
        (middle_pane_id, Direction::Right, -1, false),
        (suppressed_pane_id, Direction::Left, 1, true),
    ];
    for (resized_pane_id, direction, resize_cell_delta, is_pane_growing) in held_resizes {
        assert_eq!(
            resize_layout(
                &layout_tree,
                narrow_layout_area,
                resized_pane_id,
                direction,
                resize_cell_delta,
            ),
            Err(ResizeError::BorderHeld {
                pane_id: resized_pane_id,
                direction,
                is_pane_growing,
            })
        );
    }
}

#[test]
fn offsets_at_the_i32_limits_still_move_the_border_by_the_request() {
    // The left pane carries an offset of `i32::MIN`. Its share solves to 0 and
    // its floor lifts it to 4: 80 columns split 4 and 76.
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let mut layout_tree =
        build_two_pane_split(SplitDirection::Horizontal, left_pane_id, right_pane_id);
    let LayoutNode::Split(offset_split) = &mut layout_tree else {
        unreachable!("the tree is a split");
    };
    offset_split.weights[0].resize_delta = i32::MIN;
    let layout_area = build_cell_rect(0, 0, 80, 24);
    let list_column_counts = |solved_layout_tree: &LayoutNode| {
        [left_pane_id, right_pane_id].map(|pane_id| {
            compute_solved_pane_size(solved_layout_tree, layout_area, pane_id).column_count
        })
    };
    assert_eq!(list_column_counts(&layout_tree), [4, 76]);

    // Growing the left pane by 5 lands on 9 and 71. The left pane keeps an
    // offset of -31, and the right pane takes the rest of the added offset.
    let (grown_layout_tree, moved_cell_count) =
        resize_layout(&layout_tree, layout_area, left_pane_id, Direction::Right, 5).unwrap();
    assert_eq!(moved_cell_count, 5);
    assert_eq!(list_column_counts(&grown_layout_tree), [9, 71]);
    let LayoutNode::Split(grown_split) = &grown_layout_tree else {
        unreachable!("the tree is a split");
    };
    assert_eq!(
        grown_split
            .weights
            .iter()
            .map(|size_weight| size_weight.resize_delta)
            .collect::<Vec<i32>>(),
        vec![-31, i32::MIN + 31]
    );
}

/// The resize of the child at `target_child_index` of the horizontal split of
/// leaves `layout_tree` over `split_rect`, found by solving the split at every
/// candidate added count in ascending order: each count at which the
/// receiver's or the donor's offset lies between minus and plus the split's
/// column count. The walk keeps the first count that reaches the largest move
/// up to the request, and stops at the first count whose border moved the
/// whole request or whose donor holds no spare cell.
fn walk_every_candidate(
    layout_tree: &LayoutNode,
    split_rect: Rect,
    target_child_index: usize,
    direction: Direction,
    resize_cell_delta: i16,
    pane_sizing: PaneSizing,
) -> Result<(LayoutNode, u16), ResizeError> {
    let LayoutNode::Split(split) = layout_tree else {
        panic!("the tree is a split");
    };
    let LayoutNode::Pane(pane_id) = split.children[target_child_index] else {
        panic!("every child is a leaf");
    };
    let adjacent_child_index = match direction {
        Direction::Left => target_child_index.checked_sub(1),
        Direction::Right => {
            (target_child_index + 1 < split.children.len()).then_some(target_child_index + 1)
        }
        Direction::Up | Direction::Down => None,
    }
    .ok_or(ResizeError::NoAdjacentBorder { pane_id, direction })?;
    let requested_cell_count = resize_cell_delta.unsigned_abs();
    let is_pane_growing = resize_cell_delta > 0;
    let (receiving_child_index, donating_child_index) = if is_pane_growing {
        (target_child_index, adjacent_child_index)
    } else {
        (adjacent_child_index, target_child_index)
    };

    let mut walked_split = split.clone();
    walked_split
        .weights
        .resize(walked_split.children.len(), SizeWeight::default());
    let donating_slot_floor =
        compute_slot_floor(&walked_split, donating_child_index, true, pane_sizing);
    let border_child_index = receiving_child_index.max(donating_child_index);
    let is_border_moving_forward = donating_child_index > receiving_child_index;
    let measure_border = |measured_split: &SplitNode| -> (i32, u16) {
        let child_rects = compute_directional_child_rects(measured_split, split_rect, pane_sizing);
        let spare_cell_count = u16::try_from(donating_slot_floor).map_or(0, |floor_cell_count| {
            child_rects[donating_child_index]
                .size
                .column_count
                .saturating_sub(floor_cell_count)
        });
        (
            i32::from(child_rects[border_child_index].origin.column),
            spare_cell_count,
        )
    };
    let starting_receiving_resize_delta =
        i64::from(walked_split.weights[receiving_child_index].resize_delta);
    let starting_donating_resize_delta =
        i64::from(walked_split.weights[donating_child_index].resize_delta);
    let set_added_cell_count = |walked_split: &mut SplitNode, added_cell_count: i64| {
        walked_split.weights[receiving_child_index].resize_delta =
            (starting_receiving_resize_delta + added_cell_count)
                .clamp(i32::MIN.into(), i32::MAX.into()) as i32;
        walked_split.weights[donating_child_index].resize_delta =
            (starting_donating_resize_delta - added_cell_count)
                .clamp(i32::MIN.into(), i32::MAX.into()) as i32;
    };

    let column_count = i64::from(split_rect.size.column_count);
    let mut added_cell_counts: Vec<i64> = ((1 - column_count - starting_receiving_resize_delta)
        .max(1)
        ..=(column_count - starting_receiving_resize_delta))
        .chain(
            (1 - column_count + starting_donating_resize_delta).max(1)
                ..=(column_count + starting_donating_resize_delta),
        )
        .collect();
    added_cell_counts.sort_unstable();
    added_cell_counts.dedup();

    let (starting_border_cell, starting_spare_cell_count) = measure_border(&walked_split);
    let mut walked_moved_cell_count: u16 = 0;
    let mut walked_added_cell_count: i64 = 0;
    if requested_cell_count > 0 && starting_spare_cell_count > 0 {
        for added_cell_count in added_cell_counts {
            set_added_cell_count(&mut walked_split, added_cell_count);
            let (border_cell, spare_cell_count) = measure_border(&walked_split);
            let moved_cell_count = if is_border_moving_forward {
                border_cell - starting_border_cell
            } else {
                starting_border_cell - border_cell
            };
            if moved_cell_count > i32::from(walked_moved_cell_count)
                && moved_cell_count <= i32::from(requested_cell_count)
            {
                walked_moved_cell_count = moved_cell_count as u16;
                walked_added_cell_count = added_cell_count;
            }
            if moved_cell_count >= i32::from(requested_cell_count) || spare_cell_count == 0 {
                break;
            }
        }
    }
    if requested_cell_count > 0 && walked_moved_cell_count == 0 {
        return Err(if starting_spare_cell_count == 0 {
            ResizeError::DonorAtMinimumSize {
                pane_id,
                direction,
                is_pane_growing,
            }
        } else {
            ResizeError::BorderHeld {
                pane_id,
                direction,
                is_pane_growing,
            }
        });
    }
    set_added_cell_count(&mut walked_split, walked_added_cell_count);
    Ok((LayoutNode::Split(walked_split), walked_moved_cell_count))
}

/// A horizontal split of one leaf per weight in `weights`.
fn build_weighted_leaf_split(weights: Vec<SizeWeight>) -> LayoutNode {
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        weights
            .iter()
            .map(|_| build_leaf_node(PaneId::new()))
            .collect(),
    );
    split.weights = weights;
    LayoutNode::Split(split)
}

/// A content floor `minimum_column_count` columns wide and one row high, and
/// `gap_cell_count` cells between kept children.
fn build_narrow_pane_sizing(minimum_column_count: u16, gap_cell_count: u16) -> PaneSizing {
    PaneSizing {
        minimum_size: Size {
            column_count: minimum_column_count,
            row_count: 1,
        },
        gap_cell_count,
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]

    #[test]
    fn the_search_lands_where_a_walk_over_every_candidate_lands(
        weights in proptest::collection::vec(build_size_weight_strategy(), 2..=4),
        column_count in 8u16..=120,
        minimum_column_count in 1u16..=4,
        gap_cell_count in 0u16..=2,
        target_child_choice in proptest::prelude::any::<proptest::sample::Index>(),
        is_resizing_right in proptest::prelude::any::<bool>(),
        resize_cell_delta in -130i16..=130,
    ) {
        let target_child_index = target_child_choice.index(weights.len());
        let layout_tree = build_weighted_leaf_split(weights);
        let LayoutNode::Split(split) = &layout_tree else {
            panic!("the tree is a split");
        };
        let LayoutNode::Pane(target_pane_id) = split.children[target_child_index] else {
            panic!("every child is a leaf");
        };
        let direction = if is_resizing_right { Direction::Right } else { Direction::Left };
        let split_rect = build_cell_rect(0, 0, column_count, 24);
        let pane_sizing = build_narrow_pane_sizing(minimum_column_count, gap_cell_count);

        proptest::prop_assert_eq!(
            resize_layout_with_sizing(
                &layout_tree,
                split_rect,
                target_pane_id,
                direction,
                resize_cell_delta,
                pane_sizing,
            ),
            walk_every_candidate(
                &layout_tree,
                split_rect,
                target_child_index,
                direction,
                resize_cell_delta,
                pane_sizing,
            )
        );
    }

    #[test]
    fn the_border_never_moves_back_while_the_donor_has_spare(
        weights in proptest::collection::vec(build_size_weight_strategy(), 2..=4),
        column_count in 8u16..=120,
        minimum_column_count in 1u16..=4,
        gap_cell_count in 0u16..=2,
        border_choice in proptest::prelude::any::<proptest::sample::Index>(),
        is_left_child_receiving in proptest::prelude::any::<bool>(),
    ) {
        let mut split = match build_weighted_leaf_split(weights) {
            LayoutNode::Split(split) => split,
            LayoutNode::Pane(_) => panic!("the tree is a split"),
        };
        let left_child_index = border_choice.index(split.children.len() - 1);
        let (receiving_child_index, donating_child_index) = if is_left_child_receiving {
            (left_child_index, left_child_index + 1)
        } else {
            (left_child_index + 1, left_child_index)
        };
        let split_rect = build_cell_rect(0, 0, column_count, 24);
        let pane_sizing = build_narrow_pane_sizing(minimum_column_count, gap_cell_count);
        let donating_slot_floor =
            compute_slot_floor(&split, donating_child_index, true, pane_sizing);
        let starting_receiving_resize_delta = split.weights[receiving_child_index].resize_delta;
        let starting_donating_resize_delta = split.weights[donating_child_index].resize_delta;
        // The border cell and the donor's spare cells at `added_cell_count`.
        let mut measure_border = |added_cell_count: i32| -> (i32, u32) {
            split.weights[receiving_child_index].resize_delta =
                starting_receiving_resize_delta + added_cell_count;
            split.weights[donating_child_index].resize_delta =
                starting_donating_resize_delta - added_cell_count;
            let child_rects = compute_directional_child_rects(&split, split_rect, pane_sizing);
            let border_cell = i32::from(child_rects[left_child_index + 1].origin.column);
            let moved_border_cell = if is_left_child_receiving { border_cell } else { -border_cell };
            let spare_cell_count = u32::from(child_rects[donating_child_index].size.column_count)
                .saturating_sub(donating_slot_floor);
            (moved_border_cell, spare_cell_count)
        };

        // Past `last_added_cell_count`, both offsets lie outside minus to plus
        // the column count, and every solve equals the one before it.
        let last_added_cell_count = (i32::from(column_count) - starting_receiving_resize_delta)
            .max(i32::from(column_count) + starting_donating_resize_delta)
            + 1;
        let (mut previous_moved_border_cell, mut previous_spare_cell_count) = measure_border(0);
        for added_cell_count in 1..=last_added_cell_count {
            let (moved_border_cell, spare_cell_count) = measure_border(added_cell_count);
            if previous_spare_cell_count > 0 {
                let moved_cell_count = moved_border_cell - previous_moved_border_cell;
                proptest::prop_assert!(
                    (0..=1).contains(&moved_cell_count),
                    "added count {added_cell_count} moved the border {moved_cell_count} cells"
                );
            } else {
                proptest::prop_assert_eq!(
                    spare_cell_count,
                    0,
                    "the donor held a spare cell again at added count {}",
                    added_cell_count
                );
            }
            previous_moved_border_cell = moved_border_cell;
            previous_spare_cell_count = spare_cell_count;
        }
    }
}
