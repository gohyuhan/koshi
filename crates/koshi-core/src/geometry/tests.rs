//! Unit tests for rectangular geometry operations and layout enums.
//!
//! Tests `Rect` containment, intersection, insetting, and serde round-trips;
//! `Direction` and `SplitDirection` enum serialization.

use super::*;

/// Constructs a [`Rect`] from an origin and cell dimensions.
fn build_rect(column_index: u16, row_index: u16, column_count: u16, row_count: u16) -> Rect {
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

#[test]
fn image_geometry_validates_the_visible_crop_without_overflow() {
    let geometry = ImageCellGeometry {
        full_size: Size {
            column_count: 4,
            row_count: 5,
        },
        cell_offset: Point { column: 1, row: 2 },
    };
    for (visible_size, expected_is_contained) in [
        (
            Size {
                column_count: 3,
                row_count: 3,
            },
            true,
        ),
        (
            Size {
                column_count: 4,
                row_count: 3,
            },
            false,
        ),
        (
            Size {
                column_count: 3,
                row_count: 4,
            },
            false,
        ),
        (
            Size {
                column_count: 0,
                row_count: 3,
            },
            false,
        ),
        (
            Size {
                column_count: 3,
                row_count: 0,
            },
            false,
        ),
        (
            Size {
                column_count: u16::MAX,
                row_count: u16::MAX,
            },
            false,
        ),
    ] {
        assert_eq!(
            geometry.is_visible_size_contained(visible_size),
            expected_is_contained,
            "{visible_size:?}"
        );
    }
}

#[test]
fn pixel_cell_dimensions_are_nonzero_and_round_trip_exactly() {
    assert_eq!(PixelCellSize::from_pixel_dimensions(0, 20), None);
    assert_eq!(PixelCellSize::from_pixel_dimensions(10, 0), None);
    let pixel_cell_size = PixelCellSize::from_pixel_dimensions(10, 20).expect("nonzero");
    assert_eq!(
        (
            pixel_cell_size.get_pixel_width(),
            pixel_cell_size.get_pixel_height()
        ),
        (10, 20)
    );
    let pixel_cell_size_json = serde_json::to_value(pixel_cell_size).expect("serialize");
    assert_eq!(
        pixel_cell_size_json,
        serde_json::json!({"pixel_width": 10, "pixel_height": 20})
    );
    assert_eq!(
        serde_json::from_value::<PixelCellSize>(pixel_cell_size_json).expect("restore"),
        pixel_cell_size
    );
    assert!(serde_json::from_value::<PixelCellSize>(
        serde_json::json!({"pixel_width": 0, "pixel_height": 20})
    )
    .is_err());
}

#[test]
fn zero_sized_rect_is_empty() {
    let empty_rect = Rect::empty_at_origin();
    assert!(empty_rect.is_empty());
    assert_eq!(empty_rect, build_rect(0, 0, 0, 0));
}

#[test]
fn rect_with_zero_width_or_height_is_empty() {
    assert!(build_rect(3, 3, 0, 5).is_empty());
    assert!(build_rect(3, 3, 5, 0).is_empty());
    assert!(!build_rect(3, 3, 1, 1).is_empty());
}

#[test]
fn rect_contains_points_only_inside_half_open_bounds() {
    let rect = build_rect(2, 2, 4, 3); // columns in [2,6), rows in [2,5)
    let cases = [
        (Point { column: 2, row: 2 }, true),
        (Point { column: 5, row: 4 }, true),
        (Point { column: 6, row: 4 }, false),
        (Point { column: 5, row: 5 }, false),
        (Point { column: 1, row: 3 }, false),
        (Point { column: 3, row: 1 }, false),
    ];
    for (point, expected_containment) in cases {
        assert_eq!(
            rect.is_point_inside(point),
            expected_containment,
            "contains {point:?}"
        );
    }
}

#[test]
fn empty_rect_contains_nothing() {
    let empty_rect = build_rect(2, 2, 0, 0);
    assert!(!empty_rect.is_point_inside(Point { column: 2, row: 2 }));
}

#[test]
fn rect_intersection_returns_each_expected_relationship() {
    let base_rect = build_rect(2, 2, 4, 4);

    // Overlapping: clipped to the shared region.
    assert_eq!(
        base_rect.compute_intersection(build_rect(4, 4, 4, 4)),
        Some(build_rect(4, 4, 2, 2))
    );

    // Fully contained.
    assert_eq!(
        base_rect.compute_intersection(build_rect(3, 3, 1, 1)),
        Some(build_rect(3, 3, 1, 1))
    );

    // Identical.
    assert_eq!(base_rect.compute_intersection(base_rect), Some(base_rect));

    // Adjacent on the right edge — touching, not overlapping.
    assert_eq!(base_rect.compute_intersection(build_rect(6, 2, 3, 4)), None);

    // Adjacent on the bottom edge.
    assert_eq!(base_rect.compute_intersection(build_rect(2, 6, 4, 3)), None);

    // Disjoint.
    assert_eq!(
        base_rect.compute_intersection(build_rect(20, 20, 4, 4)),
        None
    );

    // Zero-size operand never intersects.
    assert_eq!(base_rect.compute_intersection(build_rect(3, 3, 0, 0)), None);
}

#[test]
fn intersection_touching_only_at_a_corner_is_not_an_overlap() {
    // The rects meet only at the point (6, 6) and share no cell.
    let base_rect = build_rect(2, 2, 4, 4);
    let corner_touching_rect = build_rect(6, 6, 4, 4);
    assert_eq!(base_rect.compute_intersection(corner_touching_rect), None);
}

#[test]
fn intersection_with_an_empty_self_is_none() {
    let base_rect = build_rect(2, 2, 4, 4);
    assert_eq!(build_rect(3, 3, 0, 0).compute_intersection(base_rect), None);
    assert_eq!(build_rect(3, 3, 0, 2).compute_intersection(base_rect), None);
    assert_eq!(build_rect(3, 3, 2, 0).compute_intersection(base_rect), None);
    assert_eq!(base_rect.compute_intersection(build_rect(3, 3, 2, 0)), None);
}

#[test]
fn intersection_is_symmetric() {
    let first_rect = build_rect(2, 2, 4, 4);
    let second_rect = build_rect(4, 4, 4, 4);
    assert_eq!(
        second_rect.compute_intersection(first_rect),
        Some(build_rect(4, 4, 2, 2))
    );
    assert_eq!(
        first_rect.compute_intersection(second_rect),
        second_rect.compute_intersection(first_rect)
    );
}

#[test]
fn contains_at_the_grid_maximum_does_not_overflow() {
    // The right edge is one past u16::MAX.
    let corner = build_rect(u16::MAX, u16::MAX, 1, 1);
    assert!(corner.is_point_inside(Point {
        column: u16::MAX,
        row: u16::MAX
    }));
    assert!(!corner.is_point_inside(Point {
        column: u16::MAX - 1,
        row: u16::MAX
    }));

    let wide_rect = build_rect(u16::MAX - 1, 0, 2, 1);
    assert!(wide_rect.is_point_inside(Point {
        column: u16::MAX,
        row: 0,
    }));
    assert!(!wide_rect.is_point_inside(Point {
        column: u16::MAX,
        row: 1,
    }));
}

#[test]
fn inset_shrinks_all_sides() {
    let rect = build_rect(2, 2, 10, 8);
    assert_eq!(rect.compute_inset(1), build_rect(3, 3, 8, 6));
    assert_eq!(rect.compute_inset(2), build_rect(4, 4, 6, 4));
    assert_eq!(rect.compute_inner_with_border(), build_rect(3, 3, 8, 6));
}

#[test]
fn inset_underflow_clamps_to_zero() {
    // Border larger than half the rect: dimensions clamp to zero, no panic.
    let rect = build_rect(0, 0, 3, 2);
    assert_eq!(rect.compute_inset(5), build_rect(5, 5, 0, 0));
}

#[test]
fn inset_by_zero_is_the_same_rect() {
    let rect = build_rect(2, 3, 10, 8);
    assert_eq!(rect.compute_inset(0), rect);
}

#[test]
fn inset_by_exactly_half_leaves_an_empty_rect_at_the_center() {
    assert_eq!(
        build_rect(0, 0, 4, 4).compute_inset(2),
        build_rect(2, 2, 0, 0)
    );
    // An odd width keeps its middle column; the even height loses every row.
    assert_eq!(
        build_rect(0, 0, 5, 4).compute_inset(2),
        build_rect(2, 2, 1, 0)
    );
}

#[test]
fn inset_border_at_the_doubling_limit() {
    // 2 * 32767 = 65534 fits u16; 2 * 32768 saturates at u16::MAX.
    let maximum_rect = build_rect(0, 0, u16::MAX, u16::MAX);
    assert_eq!(
        maximum_rect.compute_inset(32767),
        build_rect(32767, 32767, 1, 1)
    );
    assert_eq!(
        maximum_rect.compute_inset(32768),
        build_rect(32768, 32768, 0, 0)
    );
}

#[test]
fn inset_origin_does_not_overflow() {
    // Origin near u16::MAX: saturating add keeps it in range, no panic.
    let rect = build_rect(u16::MAX - 1, u16::MAX - 1, 1, 1);
    assert_eq!(
        rect.compute_inset(u16::MAX),
        build_rect(u16::MAX, u16::MAX, 0, 0)
    );
}

#[test]
fn intersection_at_grid_max_edge_no_overflow() {
    // Right/bottom edges land at u16::MAX + 1.
    let first_rect = build_rect(u16::MAX - 3, u16::MAX - 3, 4, 4);
    let second_rect = build_rect(u16::MAX - 1, u16::MAX - 1, 4, 4);
    assert_eq!(
        first_rect.compute_intersection(second_rect),
        Some(build_rect(u16::MAX - 1, u16::MAX - 1, 2, 2))
    );
}

#[test]
fn serde_roundtrip_preserves_rect() {
    let rect = build_rect(1, 2, 3, 4);
    let rect_json = serde_json::to_string(&rect).expect("serialize");
    let decoded_rect: Rect = serde_json::from_str(&rect_json).expect("deserialize");
    assert_eq!(rect, decoded_rect);
}

#[test]
fn serde_roundtrip_preserves_directions_and_split_directions() {
    for direction in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        let direction_json = serde_json::to_string(&direction).expect("serialize");
        let decoded_direction: Direction =
            serde_json::from_str(&direction_json).expect("deserialize");
        assert_eq!(direction, decoded_direction);
    }
    for split_direction in [
        SplitDirection::Horizontal,
        SplitDirection::Vertical,
        SplitDirection::Stacked,
    ] {
        let split_direction_json = serde_json::to_string(&split_direction).expect("serialize");
        let decoded_split_direction: SplitDirection =
            serde_json::from_str(&split_direction_json).expect("deserialize");
        assert_eq!(split_direction, decoded_split_direction);
    }
}

#[test]
fn pane_area_reported_encodes_as_a_tagged_size() {
    let pane_area_json = serde_json::to_string(&PaneArea::Reported(Size {
        column_count: 80,
        row_count: 22,
    }))
    .expect("serialize");

    assert_eq!(
        pane_area_json,
        r#"{"Reported":{"column_count":80,"row_count":22}}"#
    );
}

#[test]
fn pane_area_starving_encodes_as_a_bare_tag() {
    let pane_area_json = serde_json::to_string(&PaneArea::Starving).expect("serialize");

    assert_eq!(pane_area_json, r#""Starving""#);
}

#[test]
fn serde_roundtrip_preserves_pane_area() {
    for pane_area in [
        PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22,
        }),
        PaneArea::Starving,
    ] {
        let pane_area_json = serde_json::to_string(&pane_area).expect("serialize");
        let decoded_pane_area: PaneArea =
            serde_json::from_str(&pane_area_json).expect("deserialize");
        assert_eq!(pane_area, decoded_pane_area, "{pane_area_json}");
    }
}

#[test]
fn direction_opposite_pairs_each_cardinal() {
    assert_eq!(
        Direction::Left.compute_opposite_direction(),
        Direction::Right
    );
    assert_eq!(
        Direction::Right.compute_opposite_direction(),
        Direction::Left
    );
    assert_eq!(Direction::Up.compute_opposite_direction(), Direction::Down);
    assert_eq!(Direction::Down.compute_opposite_direction(), Direction::Up);
}

#[test]
fn compute_minimum_axes_returns_the_smaller_count_on_each_axis() {
    let first_size = Size {
        column_count: 40,
        row_count: 10,
    };
    let second_size = Size {
        column_count: 20,
        row_count: 24,
    };
    assert_eq!(
        first_size.compute_minimum_axes(second_size),
        Size {
            column_count: 20,
            row_count: 10
        }
    );
    assert_eq!(
        second_size.compute_minimum_axes(first_size),
        Size {
            column_count: 20,
            row_count: 10
        }
    );
    assert_eq!(first_size.compute_minimum_axes(first_size), first_size);
    assert_eq!(
        first_size.compute_minimum_axes(Size {
            column_count: 0,
            row_count: 0,
        }),
        Size {
            column_count: 0,
            row_count: 0,
        }
    );
}

#[test]
fn rect_encodes_origin_then_size() {
    let rect_json = serde_json::to_string(&build_rect(1, 2, 3, 4)).expect("serialize");

    assert_eq!(
        rect_json,
        r#"{"origin":{"column":1,"row":2},"cell_size":{"column_count":3,"row_count":4}}"#
    );
}

#[test]
fn layout_enums_encode_as_bare_variant_names() {
    assert_eq!(
        serde_json::to_string(&Direction::Up).expect("serialize"),
        r#""Up""#
    );
    assert_eq!(
        serde_json::to_string(&SplitDirection::Stacked).expect("serialize"),
        r#""Stacked""#
    );
}

#[test]
fn point_rejects_a_coordinate_outside_u16() {
    let negative_coordinate_error =
        serde_json::from_str::<Point>(r#"{"column":-1,"row":0}"#).expect_err("negative");
    assert_eq!(
        negative_coordinate_error.to_string(),
        "invalid value: integer `-1`, expected u16 at line 1 column 12"
    );

    let oversized_coordinate_error =
        serde_json::from_str::<Point>(r#"{"column":65536,"row":0}"#).expect_err("too big");
    assert_eq!(
        oversized_coordinate_error.to_string(),
        "invalid value: integer `65536`, expected u16 at line 1 column 15"
    );
}

#[test]
fn point_ignores_an_unknown_field() {
    let point: Point =
        serde_json::from_str(r#"{"column":1,"row":2,"extra":3}"#).expect("deserialize");

    assert_eq!(point, Point { column: 1, row: 2 });
}
