//! Unit tests for rectangular geometry operations and layout enums.
//!
//! Tests `Rect` containment, intersection, insetting, and serde round-trips;
//! `Axis`, `Direction`, and `SplitDirection` enum serialization.

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
fn intersects_matches_intersection() {
    let base = rect(2, 2, 4, 4);
    assert!(base.intersects(rect(4, 4, 4, 4)));
    assert!(!base.intersects(rect(6, 2, 3, 4))); // adjacent
    assert!(!base.intersects(rect(20, 20, 1, 1)));
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
    let inset = r.inset(5);
    assert!(inset.is_empty());
    assert_eq!(inset.size, Size { cols: 0, rows: 0 });
}

#[test]
fn inset_origin_does_not_overflow() {
    // Origin near u16::MAX: saturating add keeps it in range, no panic.
    let r = rect(u16::MAX - 1, u16::MAX - 1, 1, 1);
    let inset = r.inset(u16::MAX);
    assert_eq!(
        inset.origin,
        Point {
            x: u16::MAX,
            y: u16::MAX
        }
    );
    assert!(inset.is_empty());
}

#[test]
fn intersection_at_grid_max_edge_no_overflow() {
    // Right/bottom edges land at u16::MAX + 1; widened math avoids overflow.
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
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let json = serde_json::to_string(&axis).expect("serialize");
        let back: Axis = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(axis, back);
    }
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
