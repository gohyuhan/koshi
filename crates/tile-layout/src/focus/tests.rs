use tile_core::geometry::{Point, Size};

use super::*;

fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

#[test]
fn nearest_pane_by_center_is_the_spatial_neighbor() {
    let (a, c) = (PaneId::new(), PaneId::new());
    // The removed pane sat at columns 26..53; a (0..40) is nearer than
    // c (40..80) by center distance.
    let removed = rect(26, 0, 27, 24);
    let survivors = [(a, rect(0, 0, 40, 24)), (c, rect(40, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors);
    assert_eq!(candidates.spatial_neighbor, Some(a));
}

#[test]
fn vertical_neighbors_rank_by_distance_too() {
    let (top, bottom) = (PaneId::new(), PaneId::new());
    // The removed pane filled rows 20..24; the bottom half is closer.
    let removed = rect(0, 20, 80, 4);
    let survivors = [(top, rect(0, 0, 80, 12)), (bottom, rect(0, 12, 80, 8))];

    let candidates = focus_candidates(removed, &survivors);
    assert_eq!(candidates.spatial_neighbor, Some(bottom));
}

#[test]
fn biggest_absorber_wins_absorbed_space() {
    let (a, c) = (PaneId::new(), PaneId::new());
    // a's new rect covers 14 of the removed columns, c covers 13.
    let removed = rect(26, 0, 27, 24);
    let survivors = [(a, rect(0, 0, 40, 24)), (c, rect(40, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors);
    assert_eq!(candidates.absorbed_space, Some(a));
}

#[test]
fn no_overlap_means_no_absorber() {
    let a = PaneId::new();
    let removed = rect(40, 0, 40, 24);
    let survivors = [(a, rect(0, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors);
    assert_eq!(candidates.absorbed_space, None);
    assert_eq!(candidates.spatial_neighbor, Some(a));
}

#[test]
fn equal_absorption_keeps_the_earlier_pane() {
    let (a, b) = (PaneId::new(), PaneId::new());
    // Both survivors absorb exactly half of the removed rect.
    let removed = rect(20, 0, 40, 24);
    let survivors = [(a, rect(0, 0, 40, 24)), (b, rect(40, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors);
    assert_eq!(candidates.absorbed_space, Some(a));
    assert_eq!(candidates.spatial_neighbor, Some(a));
}

#[test]
fn zero_area_panes_are_never_candidates() {
    let (visible, hidden) = (PaneId::new(), PaneId::new());
    let removed = rect(0, 0, 40, 24);
    let survivors = [(hidden, Rect::zero()), (visible, rect(0, 0, 80, 24))];

    let candidates = focus_candidates(removed, &survivors);
    assert_eq!(candidates.spatial_neighbor, Some(visible));
    assert_eq!(candidates.absorbed_space, Some(visible));
    assert_eq!(candidates.layout_order, [visible]);
}

#[test]
fn layout_order_lists_visible_panes_in_input_order() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let survivors = [
        (a, rect(0, 0, 20, 24)),
        (b, rect(20, 0, 30, 24)),
        (c, rect(50, 0, 30, 24)),
    ];

    let candidates = focus_candidates(rect(0, 0, 10, 10), &survivors);
    assert_eq!(candidates.layout_order, [a, b, c]);
}

#[test]
fn no_survivors_yields_empty_candidates() {
    let candidates = focus_candidates(rect(0, 0, 10, 10), &[]);
    assert_eq!(candidates.spatial_neighbor, None);
    assert_eq!(candidates.absorbed_space, None);
    assert!(candidates.layout_order.is_empty());
}
