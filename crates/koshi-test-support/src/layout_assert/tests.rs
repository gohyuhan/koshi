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
    check_all_space_occupied(&panes, tab()).unwrap();
    check_no_overlap(&panes).unwrap();
    check_no_outside(&panes, tab()).unwrap();
    check_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn odd_split_remainder_still_tiles() {
    // 81 columns split 40/41 — remainder lands on the right pane.
    let tab = rect(0, 0, 81, 24);
    let panes = vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(40, 0, 41, 24)),
    ];
    check_all_space_occupied(&panes, tab).unwrap();
    check_no_overlap(&panes).unwrap();
    check_no_outside(&panes, tab).unwrap();
}

#[test]
fn gap_fails_occupancy() {
    // Right pane one column short, leaving a dead column.
    let panes = vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(40, 0, 39, 24)),
    ];
    let err = check_all_space_occupied(&panes, tab()).unwrap_err();
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
    let err = check_all_space_occupied(&panes, huge).unwrap_err();
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
    let err = check_no_overlap(&panes).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::Overlap {
            a,
            a_rect: rect(0, 0, 41, 24),
            b,
            b_rect: rect(40, 0, 40, 24),
            overlap: rect(40, 0, 1, 24),
        }
    );
}

#[test]
fn pane_past_tab_edge_fails_no_outside() {
    let pane = PaneId::new();
    let panes = vec![(pane, rect(40, 0, 41, 24))];
    let err = check_no_outside(&panes, tab()).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::OutsideTab {
            pane,
            rect: rect(40, 0, 41, 24),
            tab: tab(),
        }
    );
}

#[test]
fn undersized_pane_fails_min_size() {
    let pane = PaneId::new();
    let panes = vec![(pane, rect(0, 0, 1, 24))];
    let min = Size { cols: 2, rows: 1 };
    let err = check_min_size_respected(&panes, min).unwrap_err();
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
    check_live_pane_refs(&[a, b], &live).unwrap();
}

#[test]
fn dead_pane_ref_is_detected() {
    let live_pane = PaneId::new();
    let dead_pane = PaneId::new();
    let live = HashSet::from([live_pane]);
    let err = check_live_pane_refs(&[live_pane, dead_pane], &live).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::DeadPaneReference { pane: dead_pane }
    );
}

#[test]
fn empty_pane_list_fails_occupancy_against_nonempty_tab() {
    let panes: Vec<PlacedPane> = Vec::new();
    let err = check_all_space_occupied(&panes, tab()).unwrap_err();
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
    check_no_overlap(&panes).unwrap();
    check_no_outside(&panes, tab()).unwrap();
    check_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn single_pane_exactly_fills_tab_passes_all_invariants() {
    let panes = vec![(PaneId::new(), tab())];
    check_all_space_occupied(&panes, tab()).unwrap();
    check_no_overlap(&panes).unwrap();
    check_no_outside(&panes, tab()).unwrap();
    check_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn corner_touching_panes_do_not_overlap() {
    // Two panes sharing only the single corner point (10, 10). The half-open
    // rect semantics exclude the shared point from both, so no cell is
    // double-counted.
    let a = PaneId::new();
    let b = PaneId::new();
    let panes = vec![(a, rect(0, 0, 10, 10)), (b, rect(10, 10, 10, 10))];
    check_no_overlap(&panes).unwrap();
}

#[test]
fn pane_at_exact_minimum_size_passes() {
    let pane = PaneId::new();
    let min = Size { cols: 2, rows: 1 };
    let panes = vec![(pane, rect(0, 0, 2, 1))];
    check_min_size_respected(&panes, min).unwrap();
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
    let err = check_no_overlap(&panes).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::Overlap {
            a,
            a_rect: rect(0, 0, 10, 10),
            b: c,
            b_rect: rect(5, 5, 10, 10),
            overlap: rect(5, 5, 5, 5),
        }
    );
}

#[test]
fn suppressed_panes_are_exempt() {
    // A live pane filling the tab plus a suppressed (zero-area) pane.
    let live = rect(0, 0, 80, 24);
    let panes = vec![(PaneId::new(), live), (PaneId::new(), Rect::zero())];
    // Empty pane adds no area, no overlap, no outside, and skips the floor.
    check_all_space_occupied(&panes, tab()).unwrap();
    check_no_overlap(&panes).unwrap();
    check_no_outside(&panes, tab()).unwrap();
    check_min_size_respected(&panes, Size { cols: 2, rows: 1 }).unwrap();
}

#[test]
fn a_pane_past_any_of_the_four_tab_edges_fails_no_outside() {
    // A tab that does not start at (0, 0), so a pane can sit left of or above
    // it. Its cells are x 10..30 and y 5..15.
    let tab = rect(10, 5, 20, 10);
    let cases = [
        // One column left of the tab's left edge.
        ("left", rect(9, 5, 20, 10)),
        // One row above the tab's top edge.
        ("top", rect(10, 4, 20, 10)),
        // One column past the tab's right edge.
        ("right", rect(11, 5, 20, 10)),
        // One row past the tab's bottom edge.
        ("bottom", rect(10, 6, 20, 10)),
    ];
    for (edge, spill) in cases {
        let pane = PaneId::new();
        let err = check_no_outside(&[(pane, spill)], tab).unwrap_err();
        assert_eq!(
            err,
            LayoutAssertionError::OutsideTab {
                pane,
                rect: spill,
                tab,
            },
            "{edge} edge"
        );
    }
}

#[test]
fn a_pane_filling_a_tab_that_does_not_start_at_the_origin_stays_inside() {
    let tab = rect(10, 5, 20, 10);
    check_no_outside(&[(PaneId::new(), tab)], tab).unwrap();
}

#[test]
fn a_suppressed_pane_placed_outside_the_tab_is_still_exempt() {
    // The solver clips a pane it cannot fit to zero area. Such a pane covers no
    // cell, so it never spills, wherever its origin lands.
    let far_away = Rect::new(Point { x: 500, y: 500 }, Size { cols: 0, rows: 0 });
    check_no_outside(&[(PaneId::new(), far_away)], tab()).unwrap();
}

#[test]
fn every_error_variant_displays_its_geometry() {
    let a = PaneId::new();
    let b = PaneId::new();
    let cases = [
        (
            LayoutAssertionError::SpaceNotFullyOccupied {
                tab_area: 1920,
                occupied_area: 1896,
            },
            "layout does not fully occupy the tab: tab area 1920 cells, panes occupy 1896 cells"
                .to_string(),
        ),
        (
            LayoutAssertionError::Overlap {
                a,
                a_rect: rect(0, 0, 41, 24),
                b,
                b_rect: rect(40, 0, 40, 24),
                overlap: rect(40, 0, 1, 24),
            },
            format!(
                "panes overlap: {a} {:?} and {b} {:?} share {:?}",
                rect(0, 0, 41, 24),
                rect(40, 0, 40, 24),
                rect(40, 0, 1, 24)
            ),
        ),
        (
            LayoutAssertionError::OutsideTab {
                pane: a,
                rect: rect(40, 0, 41, 24),
                tab: tab(),
            },
            format!(
                "pane {a} {:?} extends outside the tab {:?}",
                rect(40, 0, 41, 24),
                tab()
            ),
        ),
        (
            LayoutAssertionError::MinSizeViolated {
                pane: a,
                size: Size { cols: 1, rows: 24 },
                min: Size { cols: 2, rows: 1 },
            },
            format!(
                "pane {a} size {:?} is below the minimum {:?}",
                Size { cols: 1, rows: 24 },
                Size { cols: 2, rows: 1 }
            ),
        ),
        (
            LayoutAssertionError::DeadPaneReference { pane: a },
            format!("layout references non-live pane {a}"),
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn overlapping_panes_whose_areas_sum_to_the_tab_pass_occupancy_and_fail_overlap() {
    // Two identical half-tab panes stacked on the same cells: 2 * 960 = 1920
    // cells, the tab's area, with the lower half of the tab left empty.
    let a = PaneId::new();
    let b = PaneId::new();
    let panes = vec![(a, rect(0, 0, 80, 12)), (b, rect(0, 0, 80, 12))];
    check_all_space_occupied(&panes, tab()).unwrap();
    assert_eq!(
        check_no_overlap(&panes).unwrap_err(),
        LayoutAssertionError::Overlap {
            a,
            a_rect: rect(0, 0, 80, 12),
            b,
            b_rect: rect(0, 0, 80, 12),
            overlap: rect(0, 0, 80, 12),
        }
    );
}

#[test]
fn a_pane_outside_the_tab_with_the_tab_area_passes_occupancy_and_fails_no_outside() {
    let pane = PaneId::new();
    let panes = vec![(pane, rect(80, 0, 80, 24))];
    check_all_space_occupied(&panes, tab()).unwrap();
    assert_eq!(
        check_no_outside(&panes, tab()).unwrap_err(),
        LayoutAssertionError::OutsideTab {
            pane,
            rect: rect(80, 0, 80, 24),
            tab: tab(),
        }
    );
}

#[test]
fn an_empty_tab_with_no_panes_passes_occupancy() {
    check_all_space_occupied(&[], Rect::zero()).unwrap();
}

#[test]
fn a_live_pane_on_an_empty_tab_fails_occupancy_with_zero_tab_area() {
    let panes = vec![(PaneId::new(), rect(0, 0, 1, 1))];
    assert_eq!(
        check_all_space_occupied(&panes, Rect::zero()).unwrap_err(),
        LayoutAssertionError::SpaceNotFullyOccupied {
            tab_area: 0,
            occupied_area: 1,
        }
    );
}

#[test]
fn a_pane_short_in_rows_only_fails_min_size() {
    let pane = PaneId::new();
    let min = Size { cols: 2, rows: 3 };
    let panes = vec![(pane, rect(0, 0, 80, 2))];
    assert_eq!(
        check_min_size_respected(&panes, min).unwrap_err(),
        LayoutAssertionError::MinSizeViolated {
            pane,
            size: Size { cols: 80, rows: 2 },
            min,
        }
    );
}

#[test]
fn min_size_reports_the_first_undersized_pane_in_slice_order() {
    let first = PaneId::new();
    let second = PaneId::new();
    let min = Size { cols: 2, rows: 1 };
    let panes = vec![(first, rect(0, 0, 1, 1)), (second, rect(1, 0, 1, 1))];
    assert_eq!(
        check_min_size_respected(&panes, min).unwrap_err(),
        LayoutAssertionError::MinSizeViolated {
            pane: first,
            size: Size { cols: 1, rows: 1 },
            min,
        }
    );
}

#[test]
fn a_zero_minimum_passes_every_live_pane() {
    let panes = vec![(PaneId::new(), rect(0, 0, 1, 1))];
    check_min_size_respected(&panes, Size { cols: 0, rows: 0 }).unwrap();
}

#[test]
fn a_pane_whose_edge_passes_u16_max_is_reported_not_wrapped() {
    // x + cols = 65_536 does not fit in u16. The check computes the edge in
    // u32 and reports the pane as outside; a u16 edge would wrap to column 0.
    let pane = PaneId::new();
    let max_tab = rect(0, 0, u16::MAX, u16::MAX);
    let spill = rect(u16::MAX - 1, 0, 2, 1);
    assert_eq!(
        check_no_outside(&[(pane, spill)], max_tab).unwrap_err(),
        LayoutAssertionError::OutsideTab {
            pane,
            rect: spill,
            tab: max_tab,
        }
    );
}

#[test]
fn a_pane_ending_exactly_at_u16_max_stays_inside_a_max_tab() {
    let max_tab = rect(0, 0, u16::MAX, u16::MAX);
    let last_cell = rect(u16::MAX - 1, u16::MAX - 1, 1, 1);
    check_no_outside(&[(PaneId::new(), last_cell)], max_tab).unwrap();
}

#[test]
fn edge_touching_panes_do_not_overlap() {
    let panes = vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(40, 0, 40, 24)),
        (PaneId::new(), rect(0, 24, 80, 10)),
    ];
    check_no_overlap(&panes).unwrap();
}

#[test]
fn an_empty_pane_placed_over_a_live_pane_does_not_overlap_it() {
    let live = PaneId::new();
    let empty = PaneId::new();
    let panes = vec![
        (live, rect(0, 0, 80, 24)),
        (
            empty,
            Rect::new(Point { x: 10, y: 10 }, Size { cols: 0, rows: 5 }),
        ),
    ];
    check_no_overlap(&panes).unwrap();
}

#[test]
fn live_pane_refs_pass_with_no_leaf_panes() {
    check_live_pane_refs(&[], &HashSet::new()).unwrap();
    check_live_pane_refs(&[], &HashSet::from([PaneId::new()])).unwrap();
}

#[test]
fn live_pane_refs_report_the_first_dead_pane_in_slice_order() {
    let live = PaneId::new();
    let first_dead = PaneId::new();
    let second_dead = PaneId::new();
    let err =
        check_live_pane_refs(&[live, first_dead, second_dead], &HashSet::from([live])).unwrap_err();
    assert_eq!(
        err,
        LayoutAssertionError::DeadPaneReference { pane: first_dead }
    );
}

#[test]
fn a_leaf_pane_listed_twice_passes_when_it_is_live() {
    let pane = PaneId::new();
    check_live_pane_refs(&[pane, pane], &HashSet::from([pane])).unwrap();
}

#[test]
fn exact_tiling_passes_on_a_full_split() {
    check_exact_tiling(&split_lr(), tab()).unwrap();
}

#[test]
fn exact_tiling_reports_the_occupancy_failure_first() {
    // Three panes stacked on the same 40 columns: the summed area is half a
    // tab too large and the panes also overlap.
    let panes = vec![
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(0, 0, 40, 24)),
        (PaneId::new(), rect(0, 0, 40, 24)),
    ];
    assert_eq!(
        check_exact_tiling(&panes, tab()).unwrap_err(),
        LayoutAssertionError::SpaceNotFullyOccupied {
            tab_area: 80 * 24,
            occupied_area: 3 * 40 * 24,
        }
    );
}

#[test]
fn exact_tiling_reports_an_overlap_when_the_area_sums_up() {
    // Right pane sits one column left of its slot: the areas still sum to the
    // tab, but the two panes share a column.
    let (a, b) = (PaneId::new(), PaneId::new());
    let panes = vec![(a, rect(0, 0, 40, 24)), (b, rect(39, 0, 40, 24))];
    assert_eq!(
        check_exact_tiling(&panes, tab()).unwrap_err(),
        LayoutAssertionError::Overlap {
            a,
            a_rect: rect(0, 0, 40, 24),
            b,
            b_rect: rect(39, 0, 40, 24),
            overlap: rect(39, 0, 1, 24),
        }
    );
}

#[test]
fn exact_tiling_reports_a_spill_when_the_area_sums_up_and_nothing_overlaps() {
    // Both panes sit one column right of their slots: the areas sum to the tab
    // and they do not overlap, but the right one runs past the tab edge.
    let (a, b) = (PaneId::new(), PaneId::new());
    let panes = vec![(a, rect(1, 0, 40, 24)), (b, rect(41, 0, 40, 24))];
    assert_eq!(
        check_exact_tiling(&panes, tab()).unwrap_err(),
        LayoutAssertionError::OutsideTab {
            pane: b,
            rect: rect(41, 0, 40, 24),
            tab: tab(),
        }
    );
}
