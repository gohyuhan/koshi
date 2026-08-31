//! Unit tests for rectangular geometry operations and layout enums.
//!
//! Tests `Rect` containment, intersection, insetting, and serde round-trips;
//! `Direction` and `SplitDirection` enum serialization.

use super::*;

/// Constructs a [`Rect`] from origin (x, y) and size (cols, rows).
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

#[test]
fn zero_is_empty() {
    let z = Rect::zero();
    assert!(z.is_empty());
    assert_eq!(z, rect(0, 0, 0, 0));
}

#[test]
fn is_empty_on_either_axis() {
    assert!(rect(3, 3, 0, 5).is_empty());
    assert!(rect(3, 3, 5, 0).is_empty());
    assert!(!rect(3, 3, 1, 1).is_empty());
}

#[test]
fn containment_table() {
    let r = rect(2, 2, 4, 3); // x in [2,6), y in [2,5)
    let cases = [
        (Point { x: 2, y: 2 }, true),  // top-left corner (inclusive)
        (Point { x: 5, y: 4 }, true),  // last interior cell
        (Point { x: 6, y: 4 }, false), // right edge is exclusive
        (Point { x: 5, y: 5 }, false), // bottom edge is exclusive
        (Point { x: 1, y: 3 }, false), // left of origin
        (Point { x: 3, y: 1 }, false), // above origin
    ];
    for (p, expected) in cases {
        assert_eq!(r.contains(p), expected, "contains {p:?}");
    }
}

#[test]
fn empty_rect_contains_nothing() {
    let r = rect(2, 2, 0, 0);
    assert!(!r.contains(Point { x: 2, y: 2 }));
}

#[test]
fn intersection_table() {
    let base = rect(2, 2, 4, 4); // [2,6) x [2,6)

    // Overlapping: clipped to the shared region.
    assert_eq!(base.intersection(rect(4, 4, 4, 4)), Some(rect(4, 4, 2, 2)));

    // Fully contained.
    assert_eq!(base.intersection(rect(3, 3, 1, 1)), Some(rect(3, 3, 1, 1)));

    // Identical.
    assert_eq!(base.intersection(base), Some(base));

    // Adjacent on the right edge — touching, not overlapping.
    assert_eq!(base.intersection(rect(6, 2, 3, 4)), None);

    // Adjacent on the bottom edge.
    assert_eq!(base.intersection(rect(2, 6, 4, 3)), None);

    // Disjoint.
    assert_eq!(base.intersection(rect(20, 20, 4, 4)), None);

    // Zero-size operand never intersects.
    assert_eq!(base.intersection(rect(3, 3, 0, 0)), None);
}

#[test]
fn intersection_touching_only_at_a_corner_is_not_an_overlap() {
    // The rects meet only at the point (6, 6) and share no cell.
    let base = rect(2, 2, 4, 4); // [2,6) x [2,6)
    let corner_only = rect(6, 6, 4, 4); // [6,10) x [6,10), touches at (6,6)
    assert_eq!(base.intersection(corner_only), None);
}

#[test]
fn intersection_with_an_empty_self_is_none() {
    let base = rect(2, 2, 4, 4);
    assert_eq!(rect(3, 3, 0, 0).intersection(base), None);
    assert_eq!(rect(3, 3, 0, 2).intersection(base), None);
    assert_eq!(rect(3, 3, 2, 0).intersection(base), None);
    assert_eq!(base.intersection(rect(3, 3, 2, 0)), None);
}

#[test]
fn intersection_is_symmetric() {
    let a = rect(2, 2, 4, 4);
    let b = rect(4, 4, 4, 4);
    assert_eq!(b.intersection(a), Some(rect(4, 4, 2, 2)));
    assert_eq!(a.intersection(b), b.intersection(a));
}

#[test]
fn contains_at_the_grid_maximum_does_not_overflow() {
    // x in [65535, 65536): the right edge is one past u16::MAX.
    let corner = rect(u16::MAX, u16::MAX, 1, 1);
    assert!(corner.contains(Point {
        x: u16::MAX,
        y: u16::MAX
    }));
    assert!(!corner.contains(Point {
        x: u16::MAX - 1,
        y: u16::MAX
    }));

    let wide = rect(u16::MAX - 1, 0, 2, 1); // x in [65534, 65536), y in [0, 1)
    assert!(wide.contains(Point { x: u16::MAX, y: 0 }));
    assert!(!wide.contains(Point { x: u16::MAX, y: 1 }));
}

#[test]
fn inset_shrinks_all_sides() {
    let r = rect(2, 2, 10, 8);
    assert_eq!(r.inset(1), rect(3, 3, 8, 6));
    assert_eq!(r.inset(2), rect(4, 4, 6, 4));
    assert_eq!(r.inner_with_border(), rect(3, 3, 8, 6));
}

#[test]
fn inset_underflow_clamps_to_zero() {
    // Border larger than half the rect: dimensions clamp to zero, no panic.
    let r = rect(0, 0, 3, 2);
    assert_eq!(r.inset(5), rect(5, 5, 0, 0));
}

#[test]
fn inset_by_zero_is_the_same_rect() {
    let r = rect(2, 3, 10, 8);
    assert_eq!(r.inset(0), r);
}

#[test]
fn inset_by_exactly_half_leaves_an_empty_rect_at_the_center() {
    assert_eq!(rect(0, 0, 4, 4).inset(2), rect(2, 2, 0, 0));
    // An odd width keeps its middle column; the even height loses every row.
    assert_eq!(rect(0, 0, 5, 4).inset(2), rect(2, 2, 1, 0));
}

#[test]
fn inset_border_at_the_doubling_limit() {
    // 2 * 32767 = 65534 fits u16; 2 * 32768 saturates at u16::MAX.
    let full = rect(0, 0, u16::MAX, u16::MAX);
    assert_eq!(full.inset(32767), rect(32767, 32767, 1, 1));
    assert_eq!(full.inset(32768), rect(32768, 32768, 0, 0));
}

#[test]
fn inset_origin_does_not_overflow() {
    // Origin near u16::MAX: saturating add keeps it in range, no panic.
    let r = rect(u16::MAX - 1, u16::MAX - 1, 1, 1);
    assert_eq!(r.inset(u16::MAX), rect(u16::MAX, u16::MAX, 0, 0));
}

#[test]
fn intersection_at_grid_max_edge_no_overflow() {
    // Right/bottom edges land at u16::MAX + 1.
    let a = rect(u16::MAX - 3, u16::MAX - 3, 4, 4);
    let b = rect(u16::MAX - 1, u16::MAX - 1, 4, 4);
    assert_eq!(
        a.intersection(b),
        Some(rect(u16::MAX - 1, u16::MAX - 1, 2, 2))
    );
}

#[test]
fn serde_roundtrip_rect() {
    let r = rect(1, 2, 3, 4);
    let json = serde_json::to_string(&r).expect("serialize");
    let back: Rect = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(r, back);
}

#[test]
fn serde_roundtrip_enums() {
    for dir in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        let json = serde_json::to_string(&dir).expect("serialize");
        let back: Direction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(dir, back);
    }
    for split in [
        SplitDirection::Horizontal,
        SplitDirection::Vertical,
        SplitDirection::Stacked,
    ] {
        let json = serde_json::to_string(&split).expect("serialize");
        let back: SplitDirection = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(split, back);
    }
}

#[test]
fn pane_area_reported_encodes_as_a_tagged_size() {
    let json =
        serde_json::to_string(&PaneArea::Reported(Size { cols: 80, rows: 22 })).expect("serialize");

    assert_eq!(json, r#"{"Reported":{"cols":80,"rows":22}}"#);
}

#[test]
fn pane_area_starving_encodes_as_a_bare_tag() {
    let json = serde_json::to_string(&PaneArea::Starving).expect("serialize");

    assert_eq!(json, r#""Starving""#);
}

#[test]
fn pane_area_round_trips() {
    for area in [
        PaneArea::Reported(Size { cols: 80, rows: 22 }),
        PaneArea::Starving,
    ] {
        let json = serde_json::to_string(&area).expect("serialize");
        let back: PaneArea = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(area, back, "{json}");
    }
}

#[test]
fn direction_opposite_pairs_each_cardinal() {
    assert_eq!(Direction::Left.opposite(), Direction::Right);
    assert_eq!(Direction::Right.opposite(), Direction::Left);
    assert_eq!(Direction::Up.opposite(), Direction::Down);
    assert_eq!(Direction::Down.opposite(), Direction::Up);
}

#[test]
fn min_axes_takes_the_smaller_of_each_axis() {
    let a = Size { cols: 40, rows: 10 };
    let b = Size { cols: 20, rows: 24 };
    assert_eq!(a.min_axes(b), Size { cols: 20, rows: 10 });
    assert_eq!(b.min_axes(a), Size { cols: 20, rows: 10 });
    assert_eq!(a.min_axes(a), a);
    assert_eq!(
        a.min_axes(Size { cols: 0, rows: 0 }),
        Size { cols: 0, rows: 0 }
    );
}

#[test]
fn rect_encodes_origin_then_size() {
    let json = serde_json::to_string(&rect(1, 2, 3, 4)).expect("serialize");

    assert_eq!(
        json,
        r#"{"origin":{"x":1,"y":2},"size":{"cols":3,"rows":4}}"#
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
    let negative = serde_json::from_str::<Point>(r#"{"x":-1,"y":0}"#).expect_err("negative");
    assert_eq!(
        negative.to_string(),
        "invalid value: integer `-1`, expected u16 at line 1 column 7"
    );

    let too_big = serde_json::from_str::<Point>(r#"{"x":65536,"y":0}"#).expect_err("too big");
    assert_eq!(
        too_big.to_string(),
        "invalid value: integer `65536`, expected u16 at line 1 column 10"
    );
}

#[test]
fn point_ignores_an_unknown_field() {
    let point: Point = serde_json::from_str(r#"{"x":1,"y":2,"z":3}"#).expect("deserialize");

    assert_eq!(point, Point { x: 1, y: 2 });
}
