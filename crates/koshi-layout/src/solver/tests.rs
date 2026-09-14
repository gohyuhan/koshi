//! Tests for geometry solver: layout tree + rect → pane rectangles.

use koshi_core::geometry::{Point, SplitDirection};
use koshi_test_support::layout_assert::{
    check_exact_tiling, check_minimum_size_respected, check_no_outside, check_no_overlap,
};

use super::*;
use crate::test_trees::build_deep_alternating_layout;
use crate::tree::LayoutNode;

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

/// The default content floor with the given gap between kept children of a
/// directional split.
fn build_pane_sizing(gap_cell_count: u16) -> PaneSizing {
    PaneSizing {
        minimum_size: MIN_PANE_SIZE,
        gap_cell_count,
    }
}

/// Wrap a pane ID in a leaf node.
fn build_leaf_node(pane_id: PaneId) -> LayoutNode {
    LayoutNode::Pane(pane_id)
}

/// Create a split node in the given direction with equal-weight children for each pane.
fn build_equal_split_node(direction: SplitDirection, pane_ids: &[PaneId]) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        direction,
        pane_ids
            .iter()
            .map(|&pane_id| build_leaf_node(pane_id))
            .collect(),
    ))
}

/// A split whose children carry explicit primary constraints.
fn build_split_node_with_weights(
    direction: SplitDirection,
    children: Vec<(PaneId, SizeWeight)>,
) -> LayoutNode {
    let mut split_node = SplitNode::with_equal_weights(
        direction,
        children
            .iter()
            .map(|&(pane_id, _)| build_leaf_node(pane_id))
            .collect(),
    );
    split_node.weights = children
        .into_iter()
        .map(|(_, size_weight)| size_weight)
        .collect();
    LayoutNode::Split(split_node)
}

/// Verify that the solved panes fill the layout area completely with no gaps, overlaps, or spillage.
fn assert_tiles_exactly(layout_solve: &LayoutSolve, layout_area: Rect) {
    check_exact_tiling(&layout_solve.pane_rects, layout_area).unwrap();
}

#[test]
fn single_pane_fills_the_tab() {
    let pane_id = PaneId::new();
    let layout_area = build_cell_rect(0, 0, 80, 24);
    let layout_result = solve_layout(&LayoutNode::Pane(pane_id), layout_area);
    assert_eq!(layout_result.pane_rects, [(pane_id, layout_area)]);
    assert!(layout_result.suppressed_pane_ids.is_empty());
    assert!(!layout_result.is_all_panes_suppressed);
}

#[test]
fn horizontal_split_divides_columns() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_area = build_cell_rect(0, 0, 80, 24);
    let layout_result = solve_layout(
        &build_equal_split_node(SplitDirection::Horizontal, &[left_pane_id, right_pane_id]),
        layout_area,
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 40, 24)),
            (right_pane_id, build_cell_rect(40, 0, 40, 24))
        ]
    );
}

#[test]
fn vertical_split_divides_rows() {
    let (top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());
    let layout_area = build_cell_rect(0, 0, 80, 24);
    let layout_result = solve_layout(
        &build_equal_split_node(SplitDirection::Vertical, &[top_pane_id, bottom_pane_id]),
        layout_area,
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (top_pane_id, build_cell_rect(0, 0, 80, 12)),
            (bottom_pane_id, build_cell_rect(0, 12, 80, 12))
        ]
    );
}

#[test]
fn odd_remainder_goes_to_the_trailing_pane_and_is_stable() {
    let (leading_pane_id, trailing_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[leading_pane_id, trailing_pane_id],
    );
    let layout_area = build_cell_rect(0, 0, 101, 24);

    let stable_layout_solution = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        stable_layout_solution.pane_rects,
        [
            (leading_pane_id, build_cell_rect(0, 0, 50, 24)),
            (trailing_pane_id, build_cell_rect(50, 0, 51, 24))
        ]
    );
    for _solve_iteration in 0..10 {
        assert_eq!(
            solve_layout(&layout_tree, layout_area),
            stable_layout_solution
        );
    }
}

#[test]
fn three_way_split_sums_to_the_full_width() {
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let layout_area = build_cell_rect(0, 0, 80, 24);
    let layout_result = solve_layout(
        &build_equal_split_node(SplitDirection::Horizontal, &pane_ids),
        layout_area,
    );

    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [26, 27, 27]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn nested_tree_tiles_the_tab_exactly() {
    let (left_pane_id, top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_layout_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_leaf_node(bottom_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), inner_layout_split],
    ));
    let layout_area = build_cell_rect(0, 0, 81, 25);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_tiles_exactly(&layout_result, layout_area);
    check_minimum_size_respected(
        &layout_result.pane_rects,
        Size {
            column_count: 2,
            row_count: 1,
        },
    )
    .unwrap();
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 40, 25)),
            (top_pane_id, build_cell_rect(40, 0, 41, 12)),
            (bottom_pane_id, build_cell_rect(40, 12, 41, 13)),
        ]
    );
}

#[test]
fn fixed_then_percent_then_flex_distribution() {
    let (fixed_pane_id, percent_pane_id, flex_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(10)),
            ),
            (
                percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(50)),
            ),
            (
                flex_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [10, 50, 40]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn flex_weights_share_proportionally() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                first_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(2)),
            ),
            (
                second_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 90, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [60, 30]);
}

#[test]
fn missing_weights_fall_back_to_the_default_share() {
    // Hand-built: a deserialized split can carry fewer weights than
    // children. The unweighted child takes the default share.
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![
            build_leaf_node(first_pane_id),
            build_leaf_node(second_pane_id),
        ],
        weights: vec![SizeWeight::from_primary_constraint(SizeConstraint::Flex(1))],
        active_child_index: 0,
    });
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [40, 40]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn an_out_of_range_percent_caps_at_the_whole_axis() {
    // Hand-built: validation rejects Percent above 100, but a raw layout_tree can
    // carry one (via serde). The solver caps the value at 100: on a
    // 40,000-column axis the percent child takes 39,996 and the flex sibling
    // keeps its four-column floor.
    let (percent_pane_id, flex_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(255)),
            ),
            (
                flex_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 40_000, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [39_996, 4]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn all_zero_flex_weights_solve_without_panicking() {
    // The validated constructors reject `Flex(0)`, but the variant stays
    // representable through serde and direct construction. A zero total
    // weight yields zero shares; the leftover pass hands out one cell per
    // child from the end, the sum repair gives the rest to the last child,
    // and the floor clamp raises the first child to its four columns.
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                first_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(0)),
            ),
            (
                second_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(0)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [4, 76]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn resize_deltas_shift_cells_between_siblings() {
    let (growing_pane_id, shrinking_pane_id) = (PaneId::new(), PaneId::new());
    let growing_size_weight = SizeWeight {
        resize_delta: 5,
        ..SizeWeight::default()
    };
    let shrinking_size_weight = SizeWeight {
        resize_delta: -5,
        ..SizeWeight::default()
    };
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (growing_pane_id, growing_size_weight),
            (shrinking_pane_id, shrinking_size_weight),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [45, 35]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn all_fixed_underfill_gives_slack_to_the_last_child() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                first_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(10)),
            ),
            (
                second_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(10)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [10, 70]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn min_floor_is_honored_when_the_layout_fits() {
    let (wide_pane_id, middle_pane_id, narrow_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let wide_size_weight = SizeWeight {
        minimum_cell_count: Some(20),
        ..SizeWeight::default()
    };
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (wide_pane_id, wide_size_weight),
            (middle_pane_id, SizeWeight::default()),
            (narrow_pane_id, SizeWeight::default()),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 30, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    // The wide pane holds its declared min of 20; the two default siblings split the
    // remaining 10 down to their border-inclusive floor of 4.
    assert_eq!(column_widths, [20, 6, 4]);
    assert_tiles_exactly(&layout_result, layout_area);
    check_minimum_size_respected(
        &layout_result.pane_rects,
        Size {
            column_count: 2,
            row_count: 1,
        },
    )
    .unwrap();
}

#[test]
fn min_primary_acts_as_a_floor() {
    let (minimum_pane_id, flex_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                minimum_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Minimum(15)),
            ),
            (
                flex_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 20, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [15, 5]);
}

#[test]
fn preferred_target_is_honored_when_slack_allows() {
    let (preferred_pane_id, flex_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                preferred_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Preferred(30)),
            ),
            (
                flex_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [30, 70]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn preferred_target_stops_at_the_donors_floor() {
    let (preferred_pane_id, donor_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                preferred_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Preferred(90)),
            ),
            (
                donor_pane_id,
                SizeWeight {
                    minimum_cell_count: Some(20),
                    ..SizeWeight::default()
                },
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    // The donor gives down to its floor of 20; the target settles at 80.
    assert_eq!(column_widths, [80, 20]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn preferred_target_without_flexible_donors_stays_unmet() {
    let (preferred_pane_id, first_fixed_pane_id, second_fixed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                preferred_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Preferred(80)),
            ),
            (
                first_fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(20)),
            ),
            (
                second_fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(60)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // A preference is only a hint: with nothing but exact-sized siblings,
    // there is no slack and the target is quietly unmet.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [20, 20, 60]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn floors_outrank_fixed_sizes_when_no_flexible_donor_remains() {
    let (minimum_pane_id, fixed_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                minimum_pane_id,
                SizeWeight {
                    minimum_cell_count: Some(20),
                    ..SizeWeight::default()
                },
            ),
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(30)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 40, 24);

    // The fixed sibling claims 30 of 40 first, leaving the flexible child
    // at 10 — below its floor of 20. The clamp may tap fixed children, so
    // the floor wins and the fixed pane gives the difference back.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [20, 20]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn resize_deltas_clamp_at_zero_and_at_the_full_axis() {
    let (resized_pane_id, sibling_pane_id) = (PaneId::new(), PaneId::new());
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // A runaway positive delta saturates at the axis, then the floor clamp
    // claws back the sibling's minimum.
    let growing_size_weight = SizeWeight {
        resize_delta: 1000,
        ..SizeWeight::default()
    };
    let grown = solve_layout(
        &build_split_node_with_weights(
            SplitDirection::Horizontal,
            vec![
                (resized_pane_id, growing_size_weight),
                (sibling_pane_id, SizeWeight::default()),
            ],
        ),
        layout_area,
    );
    let grown_column_widths: Vec<u16> = grown
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(grown_column_widths, [76, 4]);
    assert_tiles_exactly(&grown, layout_area);

    // A runaway negative delta clamps to zero, and the floor clamp brings
    // the child back up to its minimum.
    let shrinking_size_weight = SizeWeight {
        resize_delta: -1000,
        ..SizeWeight::default()
    };
    let shrunk = solve_layout(
        &build_split_node_with_weights(
            SplitDirection::Horizontal,
            vec![
                (resized_pane_id, shrinking_size_weight),
                (sibling_pane_id, SizeWeight::default()),
            ],
        ),
        layout_area,
    );
    let shrunk_column_widths: Vec<u16> = shrunk
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(shrunk_column_widths, [4, 76]);
    assert_tiles_exactly(&shrunk, layout_area);
}

#[test]
fn underfilled_percents_leave_the_remainder_to_flex() {
    let (first_percent_pane_id, second_percent_pane_id, flex_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                first_percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(30)),
            ),
            (
                second_percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(30)),
            ),
            (
                flex_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [30, 30, 40]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn fits_accepts_a_layout_with_room_for_every_floor() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]);
    // Two bordered panes need a four-by-three box each: eight columns, three
    // rows side by side.
    assert!(is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 8, 3),
        build_pane_sizing(0)
    ));
    assert!(is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        build_pane_sizing(0)
    ));
}

#[test]
fn fits_rejects_a_layout_whose_floors_exceed_the_rect() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]);
    // Two panes at two columns each need four; three is one short.
    assert!(!is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 3, 24),
        build_pane_sizing(0)
    ));
}

#[test]
fn fits_accounts_for_nested_axis_minimums() {
    let (left_pane_id, top_pane_id, middle_pane_id, bottom_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_leaf_node(middle_pane_id),
            build_leaf_node(bottom_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), vertical_split],
    ));

    // Each bordered pane needs a four-by-three box. The three-deep column
    // stacks to nine rows; the leaf beside it adds its four columns, so the
    // layout_tree needs eight columns and nine rows.
    assert!(is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 8, 9),
        build_pane_sizing(0)
    ));
    assert!(!is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 8, 8),
        build_pane_sizing(0)
    ));
    assert!(!is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 7, 9),
        build_pane_sizing(0)
    ));
}

#[test]
fn fits_uses_declared_floors_not_just_defaults() {
    let (wide_pane_id, default_pane_id) = (PaneId::new(), PaneId::new());
    let wide_size_weight = SizeWeight {
        minimum_cell_count: Some(30),
        ..SizeWeight::default()
    };
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (wide_pane_id, wide_size_weight),
            (default_pane_id, SizeWeight::default()),
        ],
    );
    // The wide pane's declared 30 plus the default sibling's border-inclusive floor of
    // 4 need 34 columns.
    assert!(is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 34, 24),
        build_pane_sizing(0)
    ));
    assert!(!is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 33, 24),
        build_pane_sizing(0)
    ));
}

#[test]
fn shrink_suppresses_trailing_panes_deterministically() {
    let (first_pane_id, second_pane_id, suppressed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[first_pane_id, second_pane_id, suppressed_pane_id],
    );
    // Three bordered panes need twelve columns; nine fit only the first two.
    let layout_area = build_cell_rect(0, 0, 9, 24);

    let stable_layout_solution = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        stable_layout_solution.suppressed_pane_ids,
        [suppressed_pane_id]
    );
    assert!(!stable_layout_solution.is_all_panes_suppressed);
    assert_eq!(
        stable_layout_solution.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 4, 24)),
            (second_pane_id, build_cell_rect(4, 0, 5, 24)),
            (suppressed_pane_id, Rect::empty_at_origin()),
        ]
    );
    assert_tiles_exactly(&stable_layout_solution, layout_area);
    for _solve_iteration in 0..10 {
        assert_eq!(
            solve_layout(&layout_tree, layout_area),
            stable_layout_solution
        );
    }
}

#[test]
fn a_larger_min_suppresses_a_pane_that_fits_at_the_default_floor() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]);
    // Twelve columns hold two bordered panes at the 2-column default floor
    // (four each), so nothing suppresses.
    let layout_area = build_cell_rect(0, 0, 12, 24);
    let default_layout_solution =
        solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(0));
    assert!(default_layout_solution.suppressed_pane_ids.is_empty());
    assert_eq!(
        default_layout_solution.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 6, 24)),
            (second_pane_id, build_cell_rect(6, 0, 6, 24))
        ]
    );

    // A content floor of eight columns needs ten per bordered pane — twenty
    // in all. The same twelve columns fit only the first pane; the second
    // suppresses.
    let raised_layout_solution = solve_layout_with_sizing(
        &layout_tree,
        layout_area,
        PaneSizing {
            minimum_size: Size {
                column_count: 8,
                row_count: 1,
            },
            gap_cell_count: 0,
        },
    );
    assert_eq!(raised_layout_solution.suppressed_pane_ids, [second_pane_id]);
    assert_eq!(
        raised_layout_solution.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 12, 24)),
            (second_pane_id, Rect::empty_at_origin())
        ]
    );
}

#[test]
fn regrow_restores_suppressed_panes() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[first_pane_id, second_pane_id, third_pane_id],
    );

    let shrunk_layout_solution = solve_layout(&layout_tree, build_cell_rect(0, 0, 9, 24));
    assert_eq!(shrunk_layout_solution.suppressed_pane_ids, [third_pane_id]);

    let regrown_layout_solution = solve_layout(&layout_tree, build_cell_rect(0, 0, 80, 24));
    assert!(regrown_layout_solution.suppressed_pane_ids.is_empty());
    assert_eq!(
        regrown_layout_solution.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 26, 24)),
            (second_pane_id, build_cell_rect(26, 0, 27, 24)),
            (third_pane_id, build_cell_rect(53, 0, 27, 24)),
        ]
    );
    assert_tiles_exactly(&regrown_layout_solution, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn a_tab_grown_shrunk_to_nothing_then_regrown_returns_the_exact_first_solve() {
    // Solving is stateless: a full big -> tiny -> big terminal-resize cycle
    // lands back on byte-identical geometry. The tiny step suppresses every
    // pane, yet regrowing to the original rect reproduces the first solve
    // exactly, deltas and remainders included.
    let (left_pane_id, top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    // A resize delta on the outer border makes the shape asymmetric, so the
    // exact-return check is meaningful and not just an even split.
    let inner_layout_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_leaf_node(bottom_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode {
        direction: SplitDirection::Horizontal,
        children: vec![build_leaf_node(left_pane_id), inner_layout_split],
        weights: vec![
            SizeWeight {
                resize_delta: 7,
                ..SizeWeight::default()
            },
            SizeWeight {
                resize_delta: -7,
                ..SizeWeight::default()
            },
        ],
        active_child_index: 0,
    });
    let original_layout_rect = build_cell_rect(0, 0, 80, 24);

    let large_layout_solution = solve_layout(&layout_tree, original_layout_rect);
    assert_eq!(
        large_layout_solution.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 47, 24)),
            (top_pane_id, build_cell_rect(47, 0, 33, 12)),
            (bottom_pane_id, build_cell_rect(47, 12, 33, 12)),
        ]
    );

    // Shrink to a single cell: nothing fits, everything suppresses.
    let tiny_layout_solution = solve_layout(&layout_tree, build_cell_rect(0, 0, 1, 1));
    assert!(tiny_layout_solution.is_all_panes_suppressed);
    assert_eq!(
        tiny_layout_solution.suppressed_pane_ids,
        [left_pane_id, top_pane_id, bottom_pane_id]
    );

    // Grow back to the original header_rect: identical placement.
    let regrown_layout_solution = solve_layout(&layout_tree, original_layout_rect);
    assert_eq!(regrown_layout_solution, large_layout_solution);
    assert_tiles_exactly(&regrown_layout_solution, original_layout_rect);
}

#[test]
fn all_panes_suppressed_is_flagged_for_the_overlay() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]);

    let layout_result = solve_layout(&layout_tree, build_cell_rect(0, 0, 1, 1));
    assert_eq!(
        layout_result.suppressed_pane_ids,
        [first_pane_id, second_pane_id]
    );
    assert!(layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_pane_id, Rect::empty_at_origin()),
            (second_pane_id, Rect::empty_at_origin()),
        ]
    );
}

#[test]
fn cross_axis_too_small_suppresses_only_the_unfittable_subtree() {
    let (left_pane_id, top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    // A pane beside a column of two, in a layout area only three rows tall: the column
    // needs six bordered rows and cannot fit, the lone pane (needing three)
    // still can.
    let vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_leaf_node(bottom_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), vertical_split],
    ));
    let layout_area = build_cell_rect(0, 0, 80, 3);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.suppressed_pane_ids,
        [top_pane_id, bottom_pane_id]
    );
    assert!(!layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, layout_area),
            (top_pane_id, Rect::empty_at_origin()),
            (bottom_pane_id, Rect::empty_at_origin())
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn suppression_never_overlaps_or_spills() {
    let pane_ids: Vec<PaneId> = (0..6).map(|_| PaneId::new()).collect();
    let layout_tree = build_equal_split_node(SplitDirection::Horizontal, &pane_ids);
    for column_count in 1..14 {
        let layout_area = build_cell_rect(0, 0, column_count, 4);
        let layout_result = solve_layout(&layout_tree, layout_area);
        check_no_overlap(&layout_result.pane_rects).unwrap();
        check_no_outside(&layout_result.pane_rects, layout_area).unwrap();
        check_minimum_size_respected(&layout_result.pane_rects, MIN_PANE_SIZE).unwrap();
    }
}

#[test]
fn stack_gives_the_active_child_everything_above_the_headers() {
    let (active_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![active_pane_id, collapsed_pane_id],
        0,
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (active_pane_id, build_cell_rect(0, 0, 80, 23)),
            (collapsed_pane_id, build_cell_rect(0, 23, 80, 1))
        ]
    );
    // Collapsed members occupy a header strip, not suppression.
    assert!(layout_result.suppressed_pane_ids.is_empty());
    assert!(!layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.stack_headers,
        [StackHeader {
            pane_id: collapsed_pane_id,
            header_rect: build_cell_rect(0, 23, 80, 1),
            member_index: 1,
            member_count: 2,
        }]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn five_member_stack_keeps_headers_in_layout_order_around_the_active_child() {
    let pane_ids: Vec<PaneId> = (0..5).map(|_| PaneId::new()).collect();
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(pane_ids.clone(), 2));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    // Members 0 and 1 sit above as single rows, the active member gets
    // rows 2..22, members 3 and 4 sit below.
    assert_eq!(
        layout_result.pane_rects,
        [
            (pane_ids[0], build_cell_rect(0, 0, 80, 1)),
            (pane_ids[1], build_cell_rect(0, 1, 80, 1)),
            (pane_ids[2], build_cell_rect(0, 2, 80, 20)),
            (pane_ids[3], build_cell_rect(0, 22, 80, 1)),
            (pane_ids[4], build_cell_rect(0, 23, 80, 1)),
        ]
    );
    assert_eq!(
        layout_result.stack_headers,
        [
            StackHeader {
                pane_id: pane_ids[0],
                header_rect: build_cell_rect(0, 0, 80, 1),
                member_index: 0,
                member_count: 5,
            },
            StackHeader {
                pane_id: pane_ids[1],
                header_rect: build_cell_rect(0, 1, 80, 1),
                member_index: 1,
                member_count: 5,
            },
            StackHeader {
                pane_id: pane_ids[3],
                header_rect: build_cell_rect(0, 22, 80, 1),
                member_index: 3,
                member_count: 5,
            },
            StackHeader {
                pane_id: pane_ids[4],
                header_rect: build_cell_rect(0, 23, 80, 1),
                member_index: 4,
                member_count: 5,
            },
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
    check_minimum_size_respected(&layout_result.pane_rects, MIN_PANE_SIZE).unwrap();
}

#[test]
fn stack_header_metadata_is_stable_across_solves() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(pane_ids, 1));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let stable_layout_solution = solve_layout(&layout_tree, layout_area);
    for _solve_iteration in 0..10 {
        assert_eq!(
            solve_layout(&layout_tree, layout_area),
            stable_layout_solution
        );
    }
}

#[test]
fn stack_beside_a_pane_solves_inside_its_own_slot() {
    let (left_pane_id, active_pane_id, collapsed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack_layout_node = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![active_pane_id, collapsed_pane_id],
        0,
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack_layout_node],
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 40, 24)),
            (active_pane_id, build_cell_rect(40, 0, 40, 23)),
            (collapsed_pane_id, build_cell_rect(40, 23, 40, 1)),
        ]
    );
    assert_eq!(
        layout_result.stack_headers,
        [StackHeader {
            pane_id: collapsed_pane_id,
            header_rect: build_cell_rect(40, 23, 40, 1),
            member_index: 1,
            member_count: 2,
        }]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_stack_too_small_for_its_active_child_suppresses_as_one_unit() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(pane_ids.clone(), 0));
    // Three members need two header rows plus one active row; two rows are
    // not enough, and the stack suppresses whole.
    let layout_area = build_cell_rect(0, 0, 80, 2);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(layout_result.suppressed_pane_ids, pane_ids);
    assert!(layout_result.is_all_panes_suppressed);
    assert!(layout_result.stack_headers.is_empty());
    let expected_pane_rects: Vec<(PaneId, Rect)> = pane_ids
        .iter()
        .map(|&pane_id| (pane_id, Rect::empty_at_origin()))
        .collect();
    assert_eq!(layout_result.pane_rects, expected_pane_rects);
}

#[test]
fn an_out_of_bounds_active_index_still_suppresses_as_one_unit() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    // Hand-built: the constructors and edits clamp `active`, but a
    // deserialized stack may carry an out-of-range index. The min-size
    // check counts the clamped active member (the last one), and a rect too
    // short for the headers plus that member suppresses the whole stack.
    let mut stack = SplitNode::from_stacked_pane_ids(pane_ids.clone(), 0);
    stack.active_child_index = pane_ids.len() + 4;
    let layout_tree = LayoutNode::Split(stack);
    let layout_area = build_cell_rect(0, 0, 80, 2);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(layout_result.suppressed_pane_ids, pane_ids);
    assert!(layout_result.is_all_panes_suppressed);
    assert!(layout_result.stack_headers.is_empty());
    let expected_pane_rects: Vec<(PaneId, Rect)> = pane_ids
        .iter()
        .map(|&pane_id| (pane_id, Rect::empty_at_origin()))
        .collect();
    assert_eq!(layout_result.pane_rects, expected_pane_rects);
}

#[test]
fn a_stack_narrower_than_its_members_suppresses_as_one_unit() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![first_pane_id, second_pane_id],
        0,
    ));

    let layout_result = solve_layout(&layout_tree, build_cell_rect(0, 0, 1, 24));
    assert_eq!(
        layout_result.suppressed_pane_ids,
        [first_pane_id, second_pane_id]
    );
    assert!(layout_result.is_all_panes_suppressed);
    assert!(layout_result.stack_headers.is_empty());
}

#[test]
fn an_active_subtree_splits_the_active_region() {
    let (header_pane_id, top_pane_id, bottom_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    // Hand-built: a stack whose active member is itself a vertical pair.
    // The edits never create this shape.
    let active_layout_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_pane_id),
            build_leaf_node(bottom_pane_id),
        ],
    ));
    let stack_node = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(header_pane_id), active_layout_split],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active_child_index: 1,
    };
    let layout_area = build_cell_rect(0, 0, 80, 25);

    let layout_result = solve_layout(&LayoutNode::Split(stack_node), layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (header_pane_id, build_cell_rect(0, 0, 80, 1)),
            (top_pane_id, build_cell_rect(0, 1, 80, 12)),
            (bottom_pane_id, build_cell_rect(0, 13, 80, 12)),
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn border_inclusive_min_adds_one_cell_per_side() {
    let content_minimum_size = Size {
        column_count: 2,
        row_count: 1,
    };
    assert_eq!(
        compute_border_inclusive_minimum(content_minimum_size, true),
        Size {
            column_count: 4,
            row_count: 3
        }
    );
    assert_eq!(
        compute_border_inclusive_minimum(content_minimum_size, false),
        content_minimum_size
    );
}

#[test]
fn a_layout_whose_content_mins_fit_but_borders_do_not_suppresses_trailing() {
    // Two panes fit at the bare (2,1) content floor in four columns, but each
    // is drawn inside a one-cell border: together they need eight. At seven
    // the leading pane keeps its border-inclusive slot and the trailing pane
    // suppresses.
    let (leading_pane_id, trailing_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[leading_pane_id, trailing_pane_id],
    );
    let layout_area = build_cell_rect(0, 0, 7, 3);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(layout_result.suppressed_pane_ids, [trailing_pane_id]);
    assert!(!layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.pane_rects,
        [
            (leading_pane_id, build_cell_rect(0, 0, 7, 3)),
            (trailing_pane_id, Rect::empty_at_origin())
        ]
    );
}

#[test]
fn every_visible_pane_insets_to_at_least_the_content_floor() {
    // Any pane the solver leaves visible still holds the (2,1) content
    // minimum after the one-cell border inset. Sweep tight tabs and check the
    // inner rect of every non-suppressed pane.
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = build_equal_split_node(SplitDirection::Horizontal, &pane_ids);
    for column_count in 1..24 {
        for row_count in 1..7 {
            let layout_area = build_cell_rect(0, 0, column_count, row_count);
            let layout_result = solve_layout(&layout_tree, layout_area);
            for (_, outer_pane_rect) in &layout_result.pane_rects {
                if outer_pane_rect.is_empty() {
                    continue;
                }
                let content_rect = outer_pane_rect.compute_inner_with_border();
                assert!(
                    content_rect.cell_size.column_count >= 2 && content_rect.cell_size.row_count >= 1,
                    "visible pane {outer_pane_rect:?} insets to {content_rect:?}, below the content floor",
                );
            }
        }
    }
}

#[test]
fn zero_area_tab_solves_every_pane_to_zero_without_panicking() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_result = solve_layout(
        &build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]),
        Rect::empty_at_origin(),
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_pane_id, Rect::empty_at_origin()),
            (second_pane_id, Rect::empty_at_origin()),
        ]
    );
}

#[test]
fn offset_tab_origin_is_respected() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_area = build_cell_rect(5, 3, 40, 20);
    let layout_result = solve_layout(
        &build_equal_split_node(SplitDirection::Horizontal, &[left_pane_id, right_pane_id]),
        layout_area,
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, build_cell_rect(5, 3, 20, 20)),
            (right_pane_id, build_cell_rect(25, 3, 20, 20))
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn an_empty_directional_split_solves_to_no_panes_without_panicking() {
    // Hand-built: a split with no children at all, representable directly
    // though the public edits never produce it.
    let empty_layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    let layout_result = solve_layout(&empty_layout_tree, build_cell_rect(0, 0, 80, 24));
    assert!(layout_result.pane_rects.is_empty());
    assert!(layout_result.suppressed_pane_ids.is_empty());
    assert!(!layout_result.is_all_panes_suppressed);
    assert_eq!(
        compute_minimum_size(&empty_layout_tree, build_pane_sizing(0)),
        Size {
            column_count: 0,
            row_count: 0
        }
    );
}

#[test]
fn an_empty_stack_solves_to_no_panes_without_panicking() {
    let empty_layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Stacked,
        Vec::new(),
    ));
    let layout_result = solve_layout(&empty_layout_tree, build_cell_rect(0, 0, 80, 24));
    assert!(layout_result.pane_rects.is_empty());
    assert!(layout_result.stack_headers.is_empty());
    assert_eq!(
        compute_minimum_size(&empty_layout_tree, build_pane_sizing(0)),
        Size {
            column_count: 0,
            row_count: 0
        }
    );
}

#[test]
fn single_member_stack_has_no_headers_and_fills_the_rect() {
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(vec![pane_id], 0));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(layout_result.pane_rects, [(pane_id, layout_area)]);
    assert!(layout_result.stack_headers.is_empty());
    assert!(layout_result.suppressed_pane_ids.is_empty());
}

#[test]
fn leftover_cells_distribute_to_multiple_trailing_flex_children() {
    // Four equal flex shares over 10 cells: 10/4 floors to 2 each with 2
    // left over, and both leftover cells go to the two trailing children.
    let size_weights = vec![SizeWeight::default(); 4];
    let floor_cell_counts = vec![0u16; 4];
    assert_eq!(
        distribute_axis_cells(&size_weights, &floor_cell_counts, 10),
        [2, 2, 3, 3]
    );
}

#[test]
fn a_fixed_child_is_raised_to_its_own_border_floor_by_a_flexible_donor() {
    // A Fixed(1) constraint claims only one cell in the primary pass, but
    // every leaf still carries its border-inclusive floor of four; the
    // floor clamp pulls the difference from the flexible sibling.
    let (fixed_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(1)),
            ),
            (flexible_pane_id, SizeWeight::default()),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 20, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [4, 16]);
    assert_tiles_exactly(&layout_result, layout_area);
    check_minimum_size_respected(&layout_result.pane_rects, MIN_PANE_SIZE).unwrap();
}

#[test]
fn a_declared_min_overlay_outranks_a_smaller_min_primary() {
    let (overlaid_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let overlaid_size_weight = SizeWeight {
        minimum_cell_count: Some(20),
        ..SizeWeight::from_primary_constraint(SizeConstraint::Minimum(10))
    };
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (overlaid_pane_id, overlaid_size_weight),
            (
                flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 30, 24);

    // The overlay's 20 wins over the primary's 10, so the overlaid pane holds 20 and the flexible pane
    // takes the rest.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [20, 10]);
}

#[test]
fn fits_accepts_a_zero_rect_for_an_empty_split() {
    let empty_layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    assert!(is_layout_within_rect(
        &empty_layout_tree,
        Rect::empty_at_origin(),
        build_pane_sizing(0)
    ));
}

#[test]
fn border_inclusive_min_saturates_at_u16_max() {
    let content_minimum_size = Size {
        column_count: u16::MAX,
        row_count: u16::MAX,
    };
    assert_eq!(
        compute_border_inclusive_minimum(content_minimum_size, true),
        Size {
            column_count: u16::MAX,
            row_count: u16::MAX,
        }
    );
}

#[test]
fn a_single_pane_needs_three_rows_for_its_border() {
    // Starvation geometry. One cell of height is not enough: a bare
    // leaf still reserves a one-cell border on every side, so its floor is
    // (4, 3). One or two rows suppress it; three rows and four columns is the
    // exact smallest layout area that keeps it visible.
    let pane_id = PaneId::new();
    let layout_tree = LayoutNode::Pane(pane_id);

    let single_row_layout = solve_layout(&layout_tree, build_cell_rect(0, 0, 80, 1));
    assert_eq!(single_row_layout.suppressed_pane_ids, [pane_id]);
    assert!(single_row_layout.is_all_panes_suppressed);
    assert_eq!(
        single_row_layout.pane_rects,
        [(pane_id, Rect::empty_at_origin())]
    );

    let two_row_layout = solve_layout(&layout_tree, build_cell_rect(0, 0, 80, 2));
    assert_eq!(two_row_layout.suppressed_pane_ids, [pane_id]);
    assert!(two_row_layout.is_all_panes_suppressed);

    let three_column_layout = solve_layout(&layout_tree, build_cell_rect(0, 0, 3, 24));
    assert_eq!(three_column_layout.suppressed_pane_ids, [pane_id]);
    assert!(three_column_layout.is_all_panes_suppressed);

    // Exactly the floor: four columns by three rows keeps the pane visible.
    let minimum_layout = solve_layout(&layout_tree, build_cell_rect(0, 0, 4, 3));
    assert_eq!(
        minimum_layout.pane_rects,
        [(pane_id, build_cell_rect(0, 0, 4, 3))]
    );
    assert!(minimum_layout.suppressed_pane_ids.is_empty());
    assert!(!minimum_layout.is_all_panes_suppressed);
}

#[test]
fn a_two_by_two_cell_tab_suppresses_every_pane() {
    // Starvation geometry. A 2x2 terminal cannot hold even one
    // bordered pane, so a four-pane grid drops entirely.
    let (top_left_pane_id, bottom_left_pane_id, top_right_pane_id, bottom_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let left_vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_left_pane_id),
            build_leaf_node(bottom_left_pane_id),
        ],
    ));
    let right_vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_right_pane_id),
            build_leaf_node(bottom_right_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![left_vertical_split, right_vertical_split],
    ));

    let layout_result = solve_layout(&layout_tree, build_cell_rect(0, 0, 2, 2));
    assert_eq!(
        layout_result.suppressed_pane_ids,
        [
            top_left_pane_id,
            bottom_left_pane_id,
            top_right_pane_id,
            bottom_right_pane_id,
        ]
    );
    assert!(layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.pane_rects,
        [
            (top_left_pane_id, Rect::empty_at_origin()),
            (bottom_left_pane_id, Rect::empty_at_origin()),
            (top_right_pane_id, Rect::empty_at_origin()),
            (bottom_right_pane_id, Rect::empty_at_origin()),
        ]
    );
}

#[test]
fn a_two_by_two_grid_tiles_a_normal_tab_exactly() {
    // The same 2x2 grid at a real size lays four equal quadrants.
    let (top_left_pane_id, bottom_left_pane_id, top_right_pane_id, bottom_right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let left_vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_left_pane_id),
            build_leaf_node(bottom_left_pane_id),
        ],
    ));
    let right_vertical_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            build_leaf_node(top_right_pane_id),
            build_leaf_node(bottom_right_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![left_vertical_split, right_vertical_split],
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (top_left_pane_id, build_cell_rect(0, 0, 40, 12)),
            (bottom_left_pane_id, build_cell_rect(0, 12, 40, 12)),
            (top_right_pane_id, build_cell_rect(40, 0, 40, 12)),
            (bottom_right_pane_id, build_cell_rect(40, 12, 40, 12)),
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn far_more_panes_than_fit_suppress_every_trailing_one() {
    // Starvation. A hundred equal columns in eighty cells: each
    // bordered pane needs four columns, so exactly the first twenty are kept
    // (twenty times four is eighty), and the remaining eighty suppress in
    // trailing order.
    let pane_ids: Vec<PaneId> = (0..100).map(|_| PaneId::new()).collect();
    let layout_tree = build_equal_split_node(SplitDirection::Horizontal, &pane_ids);
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(layout_result.suppressed_pane_ids, pane_ids[20..].to_vec());
    assert!(!layout_result.is_all_panes_suppressed);
    // The kept twenty each take exactly their four-column floor, back to back.
    for (pane_index, &pane_id) in pane_ids.iter().take(20).enumerate() {
        let placed_pane_rect = layout_result
            .pane_rects
            .iter()
            .find(|&&(placed_pane_id, _)| placed_pane_id == pane_id)
            .unwrap()
            .1;
        assert_eq!(
            placed_pane_rect,
            build_cell_rect(pane_index as u16 * 4, 0, 4, 24)
        );
    }
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn exactly_enough_columns_for_every_pane_keeps_all_of_them() {
    // The boundary: twenty bordered panes need exactly eighty
    // columns, and nothing suppresses.
    let pane_ids: Vec<PaneId> = (0..20).map(|_| PaneId::new()).collect();
    let layout_tree = build_equal_split_node(SplitDirection::Horizontal, &pane_ids);
    let layout_area = build_cell_rect(0, 0, 80, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert!(layout_result.suppressed_pane_ids.is_empty());
    let expected_pane_rects: Vec<(PaneId, Rect)> = pane_ids
        .iter()
        .enumerate()
        .map(|(pane_index, &pane_id)| (pane_id, build_cell_rect(pane_index as u16 * 4, 0, 4, 24)))
        .collect();
    assert_eq!(layout_result.pane_rects, expected_pane_rects);
    assert_tiles_exactly(&layout_result, layout_area);

    // One column short and the last pane drops.
    let undersized_layout_solution = solve_layout(&layout_tree, build_cell_rect(0, 0, 79, 24));
    assert_eq!(
        undersized_layout_solution.suppressed_pane_ids,
        [pane_ids[19]]
    );
}

#[test]
fn odd_rows_send_the_extra_row_to_the_trailing_pane() {
    // Off-by-one. Twenty-five rows split two ways is 12 then 13.
    let (top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Vertical, &[top_pane_id, bottom_pane_id]);
    let layout_area = build_cell_rect(0, 0, 80, 25);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (top_pane_id, build_cell_rect(0, 0, 80, 12)),
            (bottom_pane_id, build_cell_rect(0, 12, 80, 13))
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn odd_three_way_rows_give_the_remainder_to_the_last_pane() {
    // Off-by-one. Twenty-five rows three ways floors to 8 each with
    // one left over, which goes to the trailing child: 8, 8, 9.
    let (top_pane_id, middle_pane_id, bottom_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Vertical,
        &[top_pane_id, middle_pane_id, bottom_pane_id],
    );
    let layout_area = build_cell_rect(0, 0, 80, 25);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (top_pane_id, build_cell_rect(0, 0, 80, 8)),
            (middle_pane_id, build_cell_rect(0, 8, 80, 8)),
            (bottom_pane_id, build_cell_rect(0, 16, 80, 9)),
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn nested_same_direction_splits_send_each_levels_remainder_trailing() {
    // Rounding accumulation. An unnormalized h(a, h(b, c)) over an
    // 81-column layout area: the outer split rounds to 40 then 41, and the inner
    // split rounds its own 41 to 20 then 21. Each level's leftover cell lands
    // on that level's trailing child, and the whole thing still tiles.
    let (leading_pane_id, middle_pane_id, trailing_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let inner_split = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(middle_pane_id),
            build_leaf_node(trailing_pane_id),
        ],
    ));
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(leading_pane_id), inner_split],
    ));
    let layout_area = build_cell_rect(0, 0, 81, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (leading_pane_id, build_cell_rect(0, 0, 40, 24)),
            (middle_pane_id, build_cell_rect(40, 0, 20, 24)),
            (trailing_pane_id, build_cell_rect(60, 0, 21, 24)),
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_fifty_deep_alternating_tree_tiles_exactly_and_solves_deterministically() {
    // Deep nesting. Fifty alternating horizontal/vertical splits
    // nest fifty-one leaves. Over a layout area large enough to hold every floor the
    // panes tile exactly, meet the minimum, and two solves agree.
    let pane_ids: Vec<PaneId> = (0..51).map(|_| PaneId::new()).collect();
    let layout_tree = build_deep_alternating_layout(&pane_ids);
    let layout_area = build_cell_rect(0, 0, 1000, 1000);

    assert!(is_layout_within_rect(
        &layout_tree,
        layout_area,
        build_pane_sizing(0)
    ));
    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(layout_result.pane_rects.len(), 51);
    assert_eq!(
        layout_result
            .pane_rects
            .iter()
            .map(|&(pane_id, _)| pane_id)
            .collect::<Vec<_>>(),
        pane_ids
    );
    assert!(layout_result.suppressed_pane_ids.is_empty());
    assert_tiles_exactly(&layout_result, layout_area);
    check_minimum_size_respected(&layout_result.pane_rects, MIN_PANE_SIZE).unwrap();
    assert_eq!(solve_layout(&layout_tree, layout_area), layout_result);
}

#[test]
fn two_full_percent_children_over_the_total_share_donate_at_the_floor() {
    // Both children claim 100% of the axis; the second gets nothing from
    // the percent pass, then the floor clamp pulls its four cells back
    // from the first, which is not flexible but is still tapped once the
    // flexible-only donor pool comes up empty.
    let (first_percent_pane_id, second_percent_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                first_percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(100)),
            ),
            (
                second_percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(100)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [96, 4]);
    assert_tiles_exactly(&layout_result, layout_area);
    check_minimum_size_respected(&layout_result.pane_rects, MIN_PANE_SIZE).unwrap();
}

#[test]
fn a_stack_header_survives_a_serde_round_trip() {
    let header = StackHeader {
        pane_id: PaneId::new(),
        header_rect: build_cell_rect(0, 23, 80, 1),
        member_index: 1,
        member_count: 5,
    };

    let serialized_header = serde_json::to_string(&header).expect("serialize");
    let restored_header: StackHeader =
        serde_json::from_str(&serialized_header).expect("deserialize");
    assert_eq!(header, restored_header);
}

#[test]
fn clicking_a_collapsed_members_header_strip_activates_it() {
    use crate::focus::activate_stack_member;

    let (active_pane_id, first_collapsed_pane_id, second_collapsed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![
            active_pane_id,
            first_collapsed_pane_id,
            second_collapsed_pane_id,
        ],
        0,
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // The active pane is expanded over rows 0..22; the other panes sit on the two header rows below.
    let layout_before_activation = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_before_activation.pane_rects,
        [
            (active_pane_id, build_cell_rect(0, 0, 80, 22)),
            (first_collapsed_pane_id, build_cell_rect(0, 22, 80, 1)),
            (second_collapsed_pane_id, build_cell_rect(0, 23, 80, 1)),
        ]
    );

    // Mouse routing hit-tests the strips: the cell at column 40, row 23 lies
    // on the second collapsed pane's header, so it is the pane the click selects.
    let clicked_pane_id = layout_before_activation
        .stack_headers
        .iter()
        .find(|header| {
            header.header_rect.is_point_inside(Point {
                column: 40,
                row: 23,
            })
        })
        .expect("row 23 is a header strip")
        .pane_id;
    assert_eq!(clicked_pane_id, second_collapsed_pane_id);

    let stack_node = layout_tree
        .find_containing_stack_mut(clicked_pane_id)
        .expect("the pane lives in a stack");
    let stack_focus_change = activate_stack_member(stack_node, clicked_pane_id).unwrap();
    assert_eq!(
        stack_focus_change.newly_active_pane_id,
        second_collapsed_pane_id
    );
    assert_eq!(stack_focus_change.deactivated_pane_id, Some(active_pane_id));

    // The selected pane holds the content region and the other panes sit on header strips.
    let layout_after_activation = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_after_activation.pane_rects,
        [
            (active_pane_id, build_cell_rect(0, 0, 80, 1)),
            (first_collapsed_pane_id, build_cell_rect(0, 1, 80, 1)),
            (second_collapsed_pane_id, build_cell_rect(0, 2, 80, 22)),
        ]
    );
    assert_eq!(
        layout_after_activation.stack_headers,
        [
            StackHeader {
                pane_id: active_pane_id,
                header_rect: build_cell_rect(0, 0, 80, 1),
                member_index: 0,
                member_count: 3,
            },
            StackHeader {
                pane_id: first_collapsed_pane_id,
                header_rect: build_cell_rect(0, 1, 80, 1),
                member_index: 1,
                member_count: 3,
            },
        ]
    );
    assert_tiles_exactly(&layout_after_activation, layout_area);
}

#[test]
fn targeting_a_collapsed_stack_member_by_name_expands_it() {
    use crate::focus::{activate_stack_member, compute_focus_candidates};

    let (left_pane_id, active_pane_id, collapsed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack_layout_node = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![active_pane_id, collapsed_pane_id],
        0,
    ));
    let mut layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![build_leaf_node(left_pane_id), stack_layout_node],
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // The active pane is expanded in the stack; the collapsed pane is on the bottom row.
    let layout_before_activation = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_before_activation.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 40, 24)),
            (active_pane_id, build_cell_rect(40, 0, 40, 23)),
            (collapsed_pane_id, build_cell_rect(40, 23, 40, 1)),
        ]
    );

    // The normal focus path never offers the collapsed pane: its rect is a header strip.
    let candidates = compute_focus_candidates(
        build_cell_rect(40, 23, 40, 1),
        &layout_before_activation.pane_rects,
        &layout_before_activation.stack_headers,
    );
    assert_eq!(
        candidates.layout_order_pane_ids,
        [left_pane_id, active_pane_id]
    );
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(active_pane_id));

    // Naming the collapsed pane reaches it through the stack that holds it.
    let stack_node = layout_tree
        .find_containing_stack_mut(collapsed_pane_id)
        .expect("the pane lives in a stack");
    let stack_focus_change = activate_stack_member(stack_node, collapsed_pane_id).unwrap();
    assert_eq!(stack_focus_change.newly_active_pane_id, collapsed_pane_id);
    assert_eq!(stack_focus_change.deactivated_pane_id, Some(active_pane_id));

    let layout_after_activation = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_after_activation.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 40, 24)),
            (active_pane_id, build_cell_rect(40, 0, 40, 1)),
            (collapsed_pane_id, build_cell_rect(40, 1, 40, 23)),
        ]
    );
    assert_eq!(
        layout_after_activation.stack_headers,
        [StackHeader {
            pane_id: active_pane_id,
            header_rect: build_cell_rect(40, 0, 40, 1),
            member_index: 0,
            member_count: 2,
        }]
    );
    assert_tiles_exactly(&layout_after_activation, layout_area);
}

#[test]
fn a_preferred_child_above_its_target_hands_the_surplus_to_a_flexible_sibling() {
    let (preferred_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                preferred_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Preferred(20)),
            ),
            (
                flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // Both children flex with weight 1, so the preferred pane starts at 50 — 30 columns over
    // its target of 20. The surplus goes to the trailing flexible sibling.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [20, 80]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_preferred_child_keeps_its_surplus_when_no_flexible_sibling_can_take_it() {
    let (preferred_pane_id, fixed_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                preferred_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Preferred(20)),
            ),
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(60)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // The preferred pane solves to 40, over its target of 20, but the only sibling is fixed:
    // there is nobody to hand the 20 spare columns to, so it keeps them.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [40, 60]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_floor_deficit_is_funded_by_the_flexible_sibling_before_the_fixed_one() {
    let (minimum_pane_id, fixed_pane_id, flexible_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                minimum_pane_id,
                SizeWeight {
                    minimum_cell_count: Some(50),
                    ..SizeWeight::default()
                },
            ),
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(30)),
            ),
            (
                flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // The fixed pane takes its 30 first, leaving the minimum and flexible panes 35 each. The minimum pane is 15 short of its
    // floor of 50; the flexible pane pays all 15 and the fixed pane is untouched.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [50, 30, 20]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_floor_deficit_takes_from_the_trailing_flexible_sibling_first() {
    let (minimum_pane_id, middle_flexible_pane_id, trailing_flexible_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                minimum_pane_id,
                SizeWeight {
                    minimum_cell_count: Some(50),
                    ..SizeWeight::default()
                },
            ),
            (
                middle_flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
            (
                trailing_flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // Even shares are 33, 33 and 34 (the leftover column trails). The minimum pane is 17
    // short of its floor of 50: the trailing child pays all of it, and the middle
    // flexible pane keeps its 33.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [50, 33, 17]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_collapsed_member_that_is_a_split_puts_only_its_first_leaf_on_the_strip() {
    let (first_collapsed_pane_id, second_collapsed_pane_id, active_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack_node = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            build_equal_split_node(
                SplitDirection::Horizontal,
                &[first_collapsed_pane_id, second_collapsed_pane_id],
            ),
            LayoutNode::Pane(active_pane_id),
        ],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 1,
    };
    let layout_tree = LayoutNode::Split(stack_node);
    let layout_area = build_cell_rect(0, 0, 20, 10);

    // The collapsed member holds two panes but owns one header row: its first
    // leaf stands on the strip and its second leaf solves to nothing.
    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_collapsed_pane_id, build_cell_rect(0, 0, 20, 1)),
            (second_collapsed_pane_id, Rect::empty_at_origin()),
            (active_pane_id, build_cell_rect(0, 1, 20, 9)),
        ]
    );
    assert_eq!(
        layout_result.stack_headers,
        [StackHeader {
            pane_id: first_collapsed_pane_id,
            header_rect: build_cell_rect(0, 0, 20, 1),
            member_index: 0,
            member_count: 2,
        }]
    );
    assert!(layout_result.suppressed_pane_ids.is_empty());
}

#[test]
fn a_zero_rect_suppresses_every_pane_of_a_split() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_result = solve_layout(
        &build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]),
        build_cell_rect(0, 0, 0, 0),
    );

    assert!(layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.suppressed_pane_ids,
        [first_pane_id, second_pane_id]
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_pane_id, Rect::empty_at_origin()),
            (second_pane_id, Rect::empty_at_origin()),
        ]
    );
}

#[test]
fn a_zero_rect_suppresses_every_pane_of_a_stack() {
    let (active_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    let layout_result = solve_layout(
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![active_pane_id, collapsed_pane_id],
            0,
        )),
        build_cell_rect(0, 0, 0, 0),
    );

    assert!(layout_result.is_all_panes_suppressed);
    assert_eq!(
        layout_result.suppressed_pane_ids,
        [active_pane_id, collapsed_pane_id]
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (active_pane_id, Rect::empty_at_origin()),
            (collapsed_pane_id, Rect::empty_at_origin()),
        ]
    );
    assert_eq!(layout_result.stack_headers, Vec::new());
}

#[test]
fn two_columns_split_the_axis_left_after_one_gap() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[left_pane_id, right_pane_id]);
    let layout_area = build_cell_rect(0, 0, 120, 24);

    // No gap: the two halves meet at column 60.
    let tight_layout_solution =
        solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(0));
    assert_eq!(
        tight_layout_solution.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 60, 24)),
            (right_pane_id, build_cell_rect(60, 0, 60, 24))
        ]
    );

    // A two-column gap leaves 118 cells to share, 59 each, and columns 59
    // and 60 stay blank.
    let spaced_layout_solution =
        solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(2));
    assert_eq!(
        spaced_layout_solution.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 59, 24)),
            (right_pane_id, build_cell_rect(61, 0, 59, 24))
        ]
    );
    assert!(spaced_layout_solution.suppressed_pane_ids.is_empty());
}

#[test]
fn three_columns_reserve_two_gaps() {
    let (left_pane_id, middle_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[left_pane_id, middle_pane_id, right_pane_id],
    );
    let layout_area = build_cell_rect(0, 0, 120, 24);

    // Two gaps of two take four cells; 116 divided three ways is 38 each
    // with two over, and the two trailing children take one each.
    let layout_result = solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(2));
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 38, 24)),
            (middle_pane_id, build_cell_rect(40, 0, 39, 24)),
            (right_pane_id, build_cell_rect(81, 0, 39, 24)),
        ]
    );
}

#[test]
fn a_fixed_child_keeps_its_cells_when_a_gap_is_reserved() {
    let (fixed_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(30)),
            ),
            (flexible_pane_id, SizeWeight::default()),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 120, 24);

    // The gap comes off the axis first: 118 cells reach the children, the
    // fixed 30 is untouched, and the flexible sibling takes the other 88.
    let layout_result = solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(2));
    assert_eq!(
        layout_result.pane_rects,
        [
            (fixed_pane_id, build_cell_rect(0, 0, 30, 24)),
            (flexible_pane_id, build_cell_rect(32, 0, 88, 24))
        ]
    );
}

#[test]
fn a_percent_child_takes_its_share_of_the_axis_left_after_the_gap() {
    let (percent_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(50)),
            ),
            (flexible_pane_id, SizeWeight::default()),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 120, 24);

    // Half of the 118 cells left after the gap is 59; the flexible sibling
    // takes the remaining 59.
    let layout_result = solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(2));
    assert_eq!(
        layout_result.pane_rects,
        [
            (percent_pane_id, build_cell_rect(0, 0, 59, 24)),
            (flexible_pane_id, build_cell_rect(61, 0, 59, 24))
        ]
    );
}

#[test]
fn a_nested_split_reserves_its_own_gap_inside_its_rect() {
    let (left_pane_id, top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(left_pane_id),
            build_equal_split_node(SplitDirection::Vertical, &[top_pane_id, bottom_pane_id]),
        ],
    ));
    let layout_area = build_cell_rect(0, 0, 120, 24);

    // The outer split hands 119 columns to two children as 59 then 60, with
    // column 59 blank. The inner split hands 23 rows as 11 then 12, with row
    // 11 blank.
    let layout_result = solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(1));
    assert_eq!(
        layout_result.pane_rects,
        [
            (left_pane_id, build_cell_rect(0, 0, 59, 24)),
            (top_pane_id, build_cell_rect(60, 0, 60, 11)),
            (bottom_pane_id, build_cell_rect(60, 12, 60, 12)),
        ]
    );
}

#[test]
fn min_size_and_fits_count_the_gap() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[left_pane_id, right_pane_id]);

    // Two bordered panes need four columns each; one gap sits between them.
    for gap_cell_count in [0u16, 2] {
        assert_eq!(
            compute_minimum_size(&layout_tree, build_pane_sizing(gap_cell_count)),
            Size {
                column_count: 8 + gap_cell_count,
                row_count: 3,
            }
        );
        assert!(!is_layout_within_rect(
            &layout_tree,
            build_cell_rect(0, 0, 8 + gap_cell_count - 1, 24),
            build_pane_sizing(gap_cell_count)
        ));
        assert!(is_layout_within_rect(
            &layout_tree,
            build_cell_rect(0, 0, 8 + gap_cell_count, 24),
            build_pane_sizing(gap_cell_count)
        ));
    }
}

#[test]
fn a_suppressed_trailing_pane_gives_its_leading_gap_back() {
    let (first_pane_id, second_pane_id, suppressed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[first_pane_id, second_pane_id, suppressed_pane_id],
    );

    // Twelve columns hold two floors of four plus one gap of two, but not
    // the third floor and its gap: c suppresses and the ten remaining cells
    // are shared by a and b.
    let two_pane_layout = solve_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 12, 24),
        build_pane_sizing(2),
    );
    assert_eq!(
        two_pane_layout.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 5, 24)),
            (second_pane_id, build_cell_rect(7, 0, 5, 24)),
            (suppressed_pane_id, Rect::empty_at_origin()),
        ]
    );
    assert_eq!(two_pane_layout.suppressed_pane_ids, [suppressed_pane_id]);
    assert!(!two_pane_layout.is_all_panes_suppressed);

    // Nine columns hold one floor but not a second floor plus its gap, so a
    // alone survives and takes the whole axis with no gap reserved.
    let one_pane_layout = solve_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 9, 24),
        build_pane_sizing(2),
    );
    assert_eq!(
        one_pane_layout.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 9, 24)),
            (second_pane_id, Rect::empty_at_origin()),
            (suppressed_pane_id, Rect::empty_at_origin()),
        ]
    );
    assert_eq!(
        one_pane_layout.suppressed_pane_ids,
        [second_pane_id, suppressed_pane_id]
    );
}

#[test]
fn a_stack_places_no_gap_between_its_members() {
    let (active_pane_id, first_collapsed_pane_id, second_collapsed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![
            active_pane_id,
            first_collapsed_pane_id,
            second_collapsed_pane_id,
        ],
        0,
    ));
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // The active member takes every row the two header strips leave, and the
    // three rects run row by row with nothing between them.
    let spaced_layout = solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(5));
    assert_eq!(
        spaced_layout.pane_rects,
        [
            (active_pane_id, build_cell_rect(0, 0, 80, 22)),
            (first_collapsed_pane_id, build_cell_rect(0, 22, 80, 1)),
            (second_collapsed_pane_id, build_cell_rect(0, 23, 80, 1)),
        ]
    );
    assert_eq!(
        spaced_layout,
        solve_layout_with_sizing(&layout_tree, layout_area, build_pane_sizing(0))
    );
}

#[test]
fn min_size_saturates_when_the_gap_fills_the_axis() {
    let (left_pane_id, middle_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_equal_split_node(
        SplitDirection::Horizontal,
        &[left_pane_id, middle_pane_id, right_pane_id],
    );

    assert_eq!(
        compute_minimum_size(&layout_tree, build_pane_sizing(u16::MAX)),
        Size {
            column_count: u16::MAX,
            row_count: 3,
        }
    );
}

#[test]
fn a_gap_wider_than_the_axis_keeps_only_the_first_pane() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[first_pane_id, second_pane_id]);

    // The first floor of four fits eight columns; the second needs its
    // leading gap first, which alone is wider than the axis, so b suppresses
    // and no gap is reserved for it.
    let layout_result = solve_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 8, 24),
        build_pane_sizing(u16::MAX),
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (first_pane_id, build_cell_rect(0, 0, 8, 24)),
            (second_pane_id, Rect::empty_at_origin())
        ]
    );
    assert_eq!(layout_result.suppressed_pane_ids, [second_pane_id]);
    assert!(!layout_result.is_all_panes_suppressed);
}

#[test]
fn two_rows_split_the_axis_left_after_one_gap() {
    let (top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Vertical, &[top_pane_id, bottom_pane_id]);

    // A two-row gap leaves 22 rows to share, 11 each; rows 11 and 12 stay
    // blank.
    let layout_result = solve_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 80, 24),
        build_pane_sizing(2),
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (top_pane_id, build_cell_rect(0, 0, 80, 11)),
            (bottom_pane_id, build_cell_rect(0, 13, 80, 11))
        ]
    );
}

#[test]
fn a_full_percent_child_leaves_its_sibling_its_floor_past_the_gap() {
    let (percent_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(100)),
            ),
            (flexible_pane_id, SizeWeight::default()),
        ],
    );

    // All 118 cells past the gap go to the percent child first; the floor
    // pass then takes four back for the flexible pane, which starts after the two-cell gap.
    let layout_result = solve_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 120, 24),
        build_pane_sizing(2),
    );
    assert_eq!(
        layout_result.pane_rects,
        [
            (percent_pane_id, build_cell_rect(0, 0, 114, 24)),
            (flexible_pane_id, build_cell_rect(116, 0, 4, 24))
        ]
    );
    assert!(layout_result.suppressed_pane_ids.is_empty());
}

#[test]
fn a_middle_child_dropped_for_its_height_gives_its_gap_to_the_survivors() {
    let first_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let tall_pane_ids: Vec<PaneId> = (0..9).map(|_| PaneId::new()).collect();
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(first_pane_id),
            build_equal_split_node(SplitDirection::Vertical, &tall_pane_ids),
            build_leaf_node(third_pane_id),
        ],
    ));

    // Nine bordered rows need 27 rows; the layout area has 24, so the middle column
    // drops out on its own. The first and third panes then share 118 columns with one gap
    // between them, not two.
    let layout_result = solve_layout_with_sizing(
        &layout_tree,
        build_cell_rect(0, 0, 120, 24),
        build_pane_sizing(2),
    );
    let mut expected_pane_rects = vec![(first_pane_id, build_cell_rect(0, 0, 59, 24))];
    expected_pane_rects.extend(
        tall_pane_ids
            .iter()
            .map(|&pane_id| (pane_id, Rect::empty_at_origin())),
    );
    expected_pane_rects.push((third_pane_id, build_cell_rect(61, 0, 59, 24)));
    assert_eq!(layout_result.pane_rects, expected_pane_rects);
    assert_eq!(layout_result.suppressed_pane_ids, tall_pane_ids);
    assert!(!layout_result.is_all_panes_suppressed);
}

#[test]
fn resize_deltas_that_overfill_are_trimmed_from_the_trailing_child() {
    let (leading_pane_id, trailing_pane_id) = (PaneId::new(), PaneId::new());
    let growing_size_weight = SizeWeight {
        resize_delta: 30,
        ..SizeWeight::default()
    };
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (leading_pane_id, growing_size_weight),
            (trailing_pane_id, growing_size_weight),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // Both children start at 40 and grow to 70: 140 cells over an axis of
    // 80. The 60 excess cells come off the trailing child, which lands at
    // 10, above its four-column floor.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [70, 10]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_fixed_size_wider_than_the_axis_is_cut_to_the_axis_and_leaves_its_sibling_the_floor() {
    let (fixed_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(100)),
            ),
            (flexible_pane_id, SizeWeight::default()),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 80, 24);

    // Fixed(100) claims all 80 cells; the flexible sibling starts at zero
    // and the floor clamp takes its four columns back from the fixed child.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [76, 4]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_percent_share_that_floors_to_zero_cells_is_raised_to_the_border_floor() {
    let (percent_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                percent_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Percent(1)),
            ),
            (flexible_pane_id, SizeWeight::default()),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 50, 24);

    // One percent of 50 columns floors to zero cells; the floor clamp then
    // takes the four-column floor from the flexible sibling.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [4, 46]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_preferred_overlay_outranks_a_preferred_primary() {
    let (overlaid_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let overlaid_size_weight = SizeWeight {
        preferred_cell_count: Some(30),
        ..SizeWeight::from_primary_constraint(SizeConstraint::Preferred(10))
    };
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (overlaid_pane_id, overlaid_size_weight),
            (
                flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // Both children flex with weight 1: the overlaid pane starts at 50. Its target is the
    // overlay's 30, not the primary's 10, and the 20 surplus columns go to the flexible pane.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [30, 70]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn a_preferred_target_below_the_border_floor_settles_at_the_floor() {
    let (preferred_pane_id, flexible_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                preferred_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Preferred(2)),
            ),
            (
                flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // The preferred pane starts at 50 with a target of 2, below its four-column border
    // floor: the 46 columns above the floor go to b.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [4, 96]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn distribute_leaves_sizes_alone_when_the_floors_cannot_all_fit() {
    // Two floors of 50 need 100 cells; over 80 the floor clamp does nothing
    // and the even flex shares stand.
    let size_weights = vec![SizeWeight::from_primary_constraint(SizeConstraint::Minimum(50)); 2];
    let floor_cell_counts = vec![50u16; 2];
    assert_eq!(
        distribute_axis_cells(&size_weights, &floor_cell_counts, 80),
        [40, 40]
    );
}

#[test]
fn a_floor_deficit_taps_the_fixed_sibling_once_the_flexible_one_is_at_its_floor() {
    let (minimum_pane_id, fixed_pane_id, flexible_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let layout_tree = build_split_node_with_weights(
        SplitDirection::Horizontal,
        vec![
            (
                minimum_pane_id,
                SizeWeight {
                    minimum_cell_count: Some(70),
                    ..SizeWeight::default()
                },
            ),
            (
                fixed_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Fixed(30)),
            ),
            (
                flexible_pane_id,
                SizeWeight::from_primary_constraint(SizeConstraint::Flex(1)),
            ),
        ],
    );
    let layout_area = build_cell_rect(0, 0, 100, 24);

    // The fixed pane takes its 30 first, leaving the minimum and flexible panes 35 each. The minimum pane is 35 short of its
    // floor of 70: the flexible pane gives 31 down to its own floor of 4, and
    // the fixed pane gives the last 4.
    let layout_result = solve_layout(&layout_tree, layout_area);
    let column_widths: Vec<u16> = layout_result
        .pane_rects
        .iter()
        .map(|(_, pane_layout)| pane_layout.cell_size.column_count)
        .collect();
    assert_eq!(column_widths, [70, 26, 4]);
    assert_tiles_exactly(&layout_result, layout_area);
}

#[test]
fn min_size_of_a_stack_counts_one_header_row_per_collapsed_member() {
    let pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    let layout_tree = LayoutNode::Split(SplitNode::from_stacked_pane_ids(pane_ids, 0));

    // Three bordered members: the widest needs four columns; two header rows
    // plus the active member's three rows make five.
    assert_eq!(
        compute_minimum_size(&layout_tree, build_pane_sizing(0)),
        Size {
            column_count: 4,
            row_count: 5
        }
    );
    assert!(is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 4, 5),
        build_pane_sizing(0)
    ));
    assert!(!is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 4, 4),
        build_pane_sizing(0)
    ));
    assert!(!is_layout_within_rect(
        &layout_tree,
        build_cell_rect(0, 0, 3, 5),
        build_pane_sizing(0)
    ));
    // A stack places no gap between its members.
    assert_eq!(
        compute_minimum_size(&layout_tree, build_pane_sizing(2)),
        Size {
            column_count: 4,
            row_count: 5
        }
    );
}

#[test]
fn cell_area_multiplies_columns_by_rows_without_overflow() {
    assert_eq!(compute_cell_area(build_cell_rect(0, 0, 40, 24)), 960);
    assert_eq!(compute_cell_area(Rect::empty_at_origin()), 0);
    assert_eq!(
        compute_cell_area(build_cell_rect(0, 0, u16::MAX, u16::MAX)),
        4_294_836_225
    );
}

#[test]
fn slot_floor_is_the_larger_of_the_subtree_minimum_and_the_declared_floor() {
    let (declared_minimum_pane_id, default_pane_id) = (PaneId::new(), PaneId::new());
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            build_leaf_node(declared_minimum_pane_id),
            build_leaf_node(default_pane_id),
        ],
    );
    split_node.weights[0] = SizeWeight {
        minimum_cell_count: Some(20),
        ..SizeWeight::default()
    };

    // Along the columns: the first pane declares 20, above its four-column border
    // minimum; the second pane declares nothing and keeps the four. A slot past the last
    // child has no floor.
    assert_eq!(
        compute_slot_floor(&split_node, 0, true, build_pane_sizing(0)),
        20
    );
    assert_eq!(
        compute_slot_floor(&split_node, 1, true, build_pane_sizing(0)),
        4
    );
    assert_eq!(
        compute_slot_floor(&split_node, 2, true, build_pane_sizing(0)),
        0
    );
    // Along the row_count: a's declared 20 still wins, b keeps its three-row
    // border minimum.
    assert_eq!(
        compute_slot_floor(&split_node, 0, false, build_pane_sizing(0)),
        20
    );
    assert_eq!(
        compute_slot_floor(&split_node, 1, false, build_pane_sizing(0)),
        3
    );
}

#[test]
fn a_tab_ending_at_the_u16_edge_places_children_without_overflow() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree =
        build_equal_split_node(SplitDirection::Horizontal, &[left_pane_id, right_pane_id]);
    let layout_area = build_cell_rect(u16::MAX - 8, u16::MAX - 3, 8, 3);

    let layout_result = solve_layout(&layout_tree, layout_area);
    assert_eq!(
        layout_result.pane_rects,
        [
            (
                left_pane_id,
                build_cell_rect(u16::MAX - 8, u16::MAX - 3, 4, 3),
            ),
            (
                right_pane_id,
                build_cell_rect(u16::MAX - 4, u16::MAX - 3, 4, 3),
            ),
        ]
    );
    assert_tiles_exactly(&layout_result, layout_area);
}
