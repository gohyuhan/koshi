//! Tests for ordered edge-region geometry.

use super::*;

fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

fn geometry(edge: Edge, extent: u16) -> RegionGeometry {
    RegionGeometry { edge, extent }
}

#[test]
fn empty_geometry_keeps_the_full_viewport() {
    let result = solve(Size { cols: 80, rows: 24 }, &[]);

    assert_eq!(result.regions, []);
    assert_eq!(result.pane_rect, rect(0, 0, 80, 24));
}

#[test]
fn zero_viewport_keeps_zero_regions_without_underflow() {
    let result = solve(
        Size { cols: 0, rows: 0 },
        &[
            geometry(Edge::Top, u16::MAX),
            geometry(Edge::Bottom, u16::MAX),
            geometry(Edge::Left, u16::MAX),
            geometry(Edge::Right, u16::MAX),
        ],
    );

    assert_eq!(
        result.regions,
        [
            rect(0, 0, 0, 0),
            rect(0, 0, 0, 0),
            rect(0, 0, 0, 0),
            rect(0, 0, 0, 0)
        ]
    );
    assert_eq!(result.pane_rect, rect(0, 0, 0, 0));
}

#[test]
fn top_and_bottom_regions_leave_the_middle() {
    let result = solve(
        Size { cols: 80, rows: 24 },
        &[geometry(Edge::Top, 1), geometry(Edge::Bottom, 1)],
    );

    assert_eq!(result.regions, [rect(0, 0, 80, 1), rect(0, 23, 80, 1)]);
    assert_eq!(result.pane_rect, rect(0, 1, 80, 22));
}

#[test]
fn all_edges_remove_cells_from_the_remaining_rectangle() {
    let result = solve(
        Size { cols: 20, rows: 10 },
        &[
            geometry(Edge::Top, 2),
            geometry(Edge::Left, 3),
            geometry(Edge::Bottom, 4),
            geometry(Edge::Right, 5),
        ],
    );

    assert_eq!(
        result.regions,
        [
            rect(0, 0, 20, 2),
            rect(0, 2, 3, 8),
            rect(3, 6, 17, 4),
            rect(15, 2, 5, 4),
        ]
    );
    assert_eq!(result.pane_rect, rect(3, 2, 12, 4));
}

#[test]
fn repeated_edges_keep_input_order() {
    let result = solve(
        Size { cols: 10, rows: 6 },
        &[
            geometry(Edge::Top, 1),
            geometry(Edge::Top, 2),
            geometry(Edge::Bottom, 1),
        ],
    );

    assert_eq!(
        result.regions,
        [rect(0, 0, 10, 1), rect(0, 1, 10, 2), rect(0, 5, 10, 1)]
    );
    assert_eq!(result.pane_rect, rect(0, 3, 10, 2));
}

#[test]
fn earlier_regions_own_the_reached_corners() {
    let top_first = solve(
        Size { cols: 6, rows: 5 },
        &[geometry(Edge::Top, 2), geometry(Edge::Left, 2)],
    );
    let left_first = solve(
        Size { cols: 6, rows: 5 },
        &[geometry(Edge::Left, 2), geometry(Edge::Top, 2)],
    );

    assert_eq!(top_first.regions, [rect(0, 0, 6, 2), rect(0, 2, 2, 3)]);
    assert_eq!(top_first.pane_rect, rect(2, 2, 4, 3));
    assert_eq!(left_first.regions, [rect(0, 0, 2, 5), rect(2, 0, 4, 2)]);
    assert_eq!(left_first.pane_rect, rect(2, 2, 4, 3));
}

#[test]
fn zero_extent_keeps_each_region_index_and_the_full_pane() {
    let result = solve(
        Size { cols: 8, rows: 4 },
        &[
            geometry(Edge::Top, 0),
            geometry(Edge::Left, 0),
            geometry(Edge::Bottom, 0),
            geometry(Edge::Right, 0),
        ],
    );

    assert_eq!(
        result.regions,
        [
            rect(0, 0, 8, 0),
            rect(0, 0, 0, 4),
            rect(0, 4, 8, 0),
            rect(8, 0, 0, 4),
        ]
    );
    assert_eq!(result.pane_rect, rect(0, 0, 8, 4));
}

#[test]
fn clamped_extent_keeps_a_zero_region_at_the_remaining_edge() {
    let result = solve(
        Size { cols: 4, rows: 3 },
        &[geometry(Edge::Top, 10), geometry(Edge::Bottom, 10)],
    );

    assert_eq!(result.regions, [rect(0, 0, 4, 3), rect(0, 3, 4, 0)]);
    assert_eq!(result.pane_rect, rect(0, 3, 4, 0));
}

#[test]
fn two_by_two_viewport_keeps_exact_remaining_cells() {
    let result = solve(
        Size { cols: 2, rows: 2 },
        &[geometry(Edge::Top, 1), geometry(Edge::Left, 1)],
    );

    assert_eq!(result.regions, [rect(0, 0, 2, 1), rect(0, 1, 1, 1)]);
    assert_eq!(result.pane_rect, rect(1, 1, 1, 1));
}

#[test]
fn one_by_one_viewport_clamps_every_edge_without_underflow() {
    let result = solve(
        Size { cols: 1, rows: 1 },
        &[
            geometry(Edge::Top, 2),
            geometry(Edge::Left, 2),
            geometry(Edge::Bottom, 2),
            geometry(Edge::Right, 2),
        ],
    );

    assert_eq!(
        result.regions,
        [
            rect(0, 0, 1, 1),
            rect(0, 1, 1, 0),
            rect(1, 1, 0, 0),
            rect(1, 1, 0, 0),
        ]
    );
    assert_eq!(result.pane_rect, rect(1, 1, 0, 0));
}

#[test]
fn maximum_viewport_clamps_without_overflow() {
    let result = solve(
        Size {
            cols: u16::MAX,
            rows: u16::MAX,
        },
        &[
            geometry(Edge::Bottom, u16::MAX),
            geometry(Edge::Right, u16::MAX),
        ],
    );

    assert_eq!(
        result.regions,
        [rect(0, 0, u16::MAX, u16::MAX), rect(0, 0, u16::MAX, 0),]
    );
    assert_eq!(result.pane_rect, rect(0, 0, 0, 0));
}

#[test]
fn repeated_solves_are_identical() {
    let geometries = [
        geometry(Edge::Right, 4),
        geometry(Edge::Top, 2),
        geometry(Edge::Bottom, 3),
        geometry(Edge::Left, 1),
    ];

    let first = solve(Size { cols: 12, rows: 9 }, &geometries);

    assert_eq!(solve(Size { cols: 12, rows: 9 }, &geometries), first);
}

#[test]
fn an_extent_equal_to_the_remaining_edge_takes_all_of_it() {
    let result = solve(
        Size { cols: 80, rows: 24 },
        &[geometry(Edge::Top, 24), geometry(Edge::Left, 80)],
    );

    assert_eq!(result.regions, [rect(0, 0, 80, 24), rect(0, 24, 80, 0)]);
    assert_eq!(result.pane_rect, rect(80, 24, 0, 0));
}
