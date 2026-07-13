//! Tests for layout assertion helpers.

use super::*;
use koshi_core::geometry::Point;

/// Create a [`Rect`] at origin (x, y) with size (cols, rows).
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

/// Create a standard 80×24 cell tab rectangle.
fn tab() -> Rect {
    rect(0, 0, 80, 24)
}

/// Two panes split left/right that exactly tile the tab.
fn split_lr() -> Vec<PlacedPane> {
    vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(40, 0, 40, 24)),
    ]
}

#[test]
fn full_tiling_passes_all_invariants() {
    let panes = split_lr();
    assert_all_space_occupied(&panes, tab()).unwrap();
    assert_no_overlap(&panes).unwrap();
    assert_no_outside(&panes, tab()).unwrap();
    assert_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn odd_split_remainder_still_tiles() {
    // 81 columns split 40/41 — remainder lands on the right pane.
    let tab = rect(0, 0, 81, 24);
    let panes = vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(40, 0, 41, 24)),
    ];
    assert_all_space_occupied(&panes, tab).unwrap();
    assert_no_overlap(&panes).unwrap();
    assert_no_outside(&panes, tab).unwrap();
}

#[test]
fn gap_fails_occupancy() {
    // Right pane one column short, leaving a dead column.
    let panes = vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(40, 0, 39, 24)),
    ];
    let err = assert_all_space_occupied(&panes, tab()).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::SpaceNotFullyOccupied {
            tab_area: 80 * 24,
            occupied_area: (40 + 39) * 24,
        }
    );
}

#[test]
fn oversized_occupancy_sum_does_not_overflow() {
    let huge = rect(0, 0, u16::MAX, u16::MAX);
    let panes = vec![(PaneId::new(), huge), (PaneId::new(), huge)];
    let err = assert_all_space_occupied(&panes, huge).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::SpaceNotFullyOccupied {
            tab_area: 65_535_u64 * 65_535,
            occupied_area: 65_535_u64 * 65_535 * 2,
        }
    );
}

#[test]
fn overlap_is_detected_and_names_both_panes() {
    let a = PaneId::new();
    let b = PaneId::new();
    let panes = vec![(a, rect(0, 0, 41, 24)), (b, rect(40, 0, 40, 24))];
    let err = assert_no_overlap(&panes).unwrap_err();
    match err {
        LayoutAssertionError::Overlap {
            a: ea,
            b: eb,
            overlap,
            ..
        } => {
            assert_eq!(ea, a);
            assert_eq!(eb, b);
            assert_eq!(overlap, rect(40, 0, 1, 24));
        }
        other => panic!("expected overlap, got {other:?}"),
    }
}

#[test]
fn pane_past_tab_edge_fails_no_outside() {
    let pane = PaneId::new();
    let panes = vec![(pane, rect(40, 0, 41, 24))];
    let err = assert_no_outside(&panes, tab()).unwrap_err();
    match err {
        LayoutAssertionError::OutsideTab { pane: ep, .. } => assert_eq!(ep, pane),
        other => panic!("expected outside-tab, got {other:?}"),
    }
}

#[test]
fn undersized_pane_fails_min_size() {
    let pane = PaneId::new();
    let panes = vec![(pane, rect(0, 0, 1, 24))];
    let min = Size { cols: 2, rows: 1 };
    let err = assert_min_size_respected(&panes, min).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::MinSizeViolated {
            pane,
            size: Size { cols: 1, rows: 24 },
            min,
        }
    );
}

#[test]
fn live_pane_refs_pass_when_all_leaf_panes_are_live() {
    let a = PaneId::new();
    let b = PaneId::new();
    let live = HashSet::from([a, b]);
    assert_live_pane_refs(&[a, b], &live).unwrap();
}

#[test]
fn dead_pane_ref_is_detected() {
    let live_pane = PaneId::new();
    let dead_pane = PaneId::new();
    let live = HashSet::from([live_pane]);
    let err = assert_live_pane_refs(&[live_pane, dead_pane], &live).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::DeadPaneReference { pane: dead_pane }
    );
}

#[test]
fn empty_pane_list_fails_occupancy_against_nonempty_tab() {
    let panes: Vec<PlacedPane> = Vec::new();
    let err = assert_all_space_occupied(&panes, tab()).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::SpaceNotFullyOccupied {
            tab_area: 80 * 24,
            occupied_area: 0,
        }
    );
}

#[test]
fn empty_pane_list_vacuously_passes_overlap_outside_and_min_size() {
    let panes: Vec<PlacedPane> = Vec::new();
    assert_no_overlap(&panes).unwrap();
    assert_no_outside(&panes, tab()).unwrap();
    assert_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn single_pane_exactly_fills_tab_passes_all_invariants() {
    let panes = vec![(PaneId::new(), tab())];
    assert_all_space_occupied(&panes, tab()).unwrap();
    assert_no_overlap(&panes).unwrap();
    assert_no_outside(&panes, tab()).unwrap();
    assert_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn corner_touching_panes_do_not_overlap() {
    // Two panes sharing only the single corner point (10, 10). The half-open
    // rect semantics exclude the shared point from both, so no cell is
    // double-counted.
    let a = PaneId::new();
    let b = PaneId::new();
    let panes = vec![(a, rect(0, 0, 10, 10)), (b, rect(10, 10, 10, 10))];
    assert_no_overlap(&panes).unwrap();
}

#[test]
fn pane_at_exact_minimum_size_passes() {
    let pane = PaneId::new();
    let min = Size { cols: 2, rows: 1 };
    let panes = vec![(pane, rect(0, 0, 2, 1))];
    assert_min_size_respected(&panes, min).unwrap();
}

#[test]
fn overlap_check_reports_first_pair_found_in_iteration_order() {
    // A and B do not overlap; A and C do. Iteration is `i` then `panes[i+1..]`,
    // so scanning from A must find the A-C overlap before ever comparing B-C.
    let a = PaneId::new();
    let b = PaneId::new();
    let c = PaneId::new();
    let panes = vec![
        (a, rect(0, 0, 10, 10)),
        (b, rect(20, 20, 10, 10)),
        (c, rect(5, 5, 10, 10)),
    ];
    let err = assert_no_overlap(&panes).unwrap_err();
    match err {
        LayoutAssertionError::Overlap { a: ea, b: eb, .. } => {
            assert_eq!(ea, a);
            assert_eq!(eb, c);
        }
        other => panic!("expected overlap between a and c, got {other:?}"),
    }
}

#[test]
fn suppressed_panes_are_exempt() {
    // A live pane filling the tab plus a suppressed (zero-area) pane.
    let live = rect(0, 0, 80, 24);
    let panes = vec![(PaneId::new(), live), (PaneId::new(), Rect::zero())];
    // Empty pane adds no area, no overlap, no outside, and skips the floor.
    assert_all_space_occupied(&panes, tab()).unwrap();
    assert_no_overlap(&panes).unwrap();
    assert_no_outside(&panes, tab()).unwrap();
    assert_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}
