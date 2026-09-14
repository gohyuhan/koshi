//! Tests for ordered edge-region geometry.

use super::*;

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

fn build_region_geometry(edge: Edge, extent_cell_count: u16) -> RegionGeometry {
    RegionGeometry {
        edge,
        extent_cell_count,
    }
}

#[test]
fn empty_geometry_keeps_the_full_viewport() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 80,
            row_count: 24,
        },
        &[],
    );

    assert_eq!(regions_result.region_rects, []);
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 0, 80, 24));
}

#[test]
fn zero_viewport_keeps_zero_regions_without_underflow() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 0,
            row_count: 0,
        },
        &[
            build_region_geometry(Edge::Top, u16::MAX),
            build_region_geometry(Edge::Bottom, u16::MAX),
            build_region_geometry(Edge::Left, u16::MAX),
            build_region_geometry(Edge::Right, u16::MAX),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [
            build_cell_rect(0, 0, 0, 0),
            build_cell_rect(0, 0, 0, 0),
            build_cell_rect(0, 0, 0, 0),
            build_cell_rect(0, 0, 0, 0)
        ]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 0, 0, 0));
}

#[test]
fn top_and_bottom_regions_leave_the_middle() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 80,
            row_count: 24,
        },
        &[
            build_region_geometry(Edge::Top, 1),
            build_region_geometry(Edge::Bottom, 1),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [build_cell_rect(0, 0, 80, 1), build_cell_rect(0, 23, 80, 1)]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 1, 80, 22));
}

#[test]
fn all_edges_remove_cells_from_the_remaining_rectangle() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 20,
            row_count: 10,
        },
        &[
            build_region_geometry(Edge::Top, 2),
            build_region_geometry(Edge::Left, 3),
            build_region_geometry(Edge::Bottom, 4),
            build_region_geometry(Edge::Right, 5),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [
            build_cell_rect(0, 0, 20, 2),
            build_cell_rect(0, 2, 3, 8),
            build_cell_rect(3, 6, 17, 4),
            build_cell_rect(15, 2, 5, 4),
        ]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(3, 2, 12, 4));
}

#[test]
fn repeated_edges_keep_input_order() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 10,
            row_count: 6,
        },
        &[
            build_region_geometry(Edge::Top, 1),
            build_region_geometry(Edge::Top, 2),
            build_region_geometry(Edge::Bottom, 1),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [
            build_cell_rect(0, 0, 10, 1),
            build_cell_rect(0, 1, 10, 2),
            build_cell_rect(0, 5, 10, 1)
        ]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 3, 10, 2));
}

#[test]
fn earlier_regions_own_the_reached_corners() {
    let top_first_solution = solve_region_rects(
        Size {
            column_count: 6,
            row_count: 5,
        },
        &[
            build_region_geometry(Edge::Top, 2),
            build_region_geometry(Edge::Left, 2),
        ],
    );
    let left_first_solution = solve_region_rects(
        Size {
            column_count: 6,
            row_count: 5,
        },
        &[
            build_region_geometry(Edge::Left, 2),
            build_region_geometry(Edge::Top, 2),
        ],
    );

    assert_eq!(
        top_first_solution.region_rects,
        [build_cell_rect(0, 0, 6, 2), build_cell_rect(0, 2, 2, 3)]
    );
    assert_eq!(top_first_solution.pane_rect, build_cell_rect(2, 2, 4, 3));
    assert_eq!(
        left_first_solution.region_rects,
        [build_cell_rect(0, 0, 2, 5), build_cell_rect(2, 0, 4, 2)]
    );
    assert_eq!(left_first_solution.pane_rect, build_cell_rect(2, 2, 4, 3));
}

#[test]
fn zero_extent_keeps_each_region_index_and_the_full_pane() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 8,
            row_count: 4,
        },
        &[
            build_region_geometry(Edge::Top, 0),
            build_region_geometry(Edge::Left, 0),
            build_region_geometry(Edge::Bottom, 0),
            build_region_geometry(Edge::Right, 0),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [
            build_cell_rect(0, 0, 8, 0),
            build_cell_rect(0, 0, 0, 4),
            build_cell_rect(0, 4, 8, 0),
            build_cell_rect(8, 0, 0, 4),
        ]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 0, 8, 4));
}

#[test]
fn clamped_extent_keeps_a_zero_region_at_the_remaining_edge() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 4,
            row_count: 3,
        },
        &[
            build_region_geometry(Edge::Top, 10),
            build_region_geometry(Edge::Bottom, 10),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [build_cell_rect(0, 0, 4, 3), build_cell_rect(0, 3, 4, 0)]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 3, 4, 0));
}

#[test]
fn two_by_two_viewport_keeps_exact_remaining_cells() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 2,
            row_count: 2,
        },
        &[
            build_region_geometry(Edge::Top, 1),
            build_region_geometry(Edge::Left, 1),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [build_cell_rect(0, 0, 2, 1), build_cell_rect(0, 1, 1, 1)]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(1, 1, 1, 1));
}

#[test]
fn one_by_one_viewport_clamps_every_edge_without_underflow() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 1,
            row_count: 1,
        },
        &[
            build_region_geometry(Edge::Top, 2),
            build_region_geometry(Edge::Left, 2),
            build_region_geometry(Edge::Bottom, 2),
            build_region_geometry(Edge::Right, 2),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [
            build_cell_rect(0, 0, 1, 1),
            build_cell_rect(0, 1, 1, 0),
            build_cell_rect(1, 1, 0, 0),
            build_cell_rect(1, 1, 0, 0),
        ]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(1, 1, 0, 0));
}

#[test]
fn maximum_viewport_clamps_without_overflow() {
    let regions_result = solve_region_rects(
        Size {
            column_count: u16::MAX,
            row_count: u16::MAX,
        },
        &[
            build_region_geometry(Edge::Bottom, u16::MAX),
            build_region_geometry(Edge::Right, u16::MAX),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [
            build_cell_rect(0, 0, u16::MAX, u16::MAX),
            build_cell_rect(0, 0, u16::MAX, 0),
        ]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(0, 0, 0, 0));
}

#[test]
fn repeated_solves_are_identical() {
    let geometries = [
        build_region_geometry(Edge::Right, 4),
        build_region_geometry(Edge::Top, 2),
        build_region_geometry(Edge::Bottom, 3),
        build_region_geometry(Edge::Left, 1),
    ];

    let first_region_solution = solve_region_rects(
        Size {
            column_count: 12,
            row_count: 9,
        },
        &geometries,
    );

    assert_eq!(
        solve_region_rects(
            Size {
                column_count: 12,
                row_count: 9
            },
            &geometries
        ),
        first_region_solution
    );
}

#[test]
fn an_extent_equal_to_the_remaining_edge_takes_all_of_it() {
    let regions_result = solve_region_rects(
        Size {
            column_count: 80,
            row_count: 24,
        },
        &[
            build_region_geometry(Edge::Top, 24),
            build_region_geometry(Edge::Left, 80),
        ],
    );

    assert_eq!(
        regions_result.region_rects,
        [build_cell_rect(0, 0, 80, 24), build_cell_rect(0, 24, 80, 0)]
    );
    assert_eq!(regions_result.pane_rect, build_cell_rect(80, 24, 0, 0));
}
