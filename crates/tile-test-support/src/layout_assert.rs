//! Layout invariant assertions for pure-layout tests.
//!
//! The layout engine maps a layout tree over a tab rect to placed pane
//! rectangles. Its correctness rests on a handful of geometric invariants:
//! the live panes tile the whole tab area, no two panes overlap, nothing
//! spills outside the tab, and every live pane respects the minimum cell
//! size. These helpers let a test state each invariant directly against a
//! slice of placed panes and get a structured, pane-identifying error when
//! one breaks.
//!
//! Each assertion is single-purpose so a test can check exactly one invariant.
//! "Exact tiling" is the conjunction of three of them:
//! [`layout_assert::assert_all_space_occupied`] (area is fully accounted for),
//! [`layout_assert::assert_no_overlap`] (no cell is double-counted), and
//! [`layout_assert::assert_no_outside`] (no cell lies beyond the tab). Area equality alone does
//! not prove a gap-free cover; pair it with the other two.
//!
//! ## Suppressed panes
//!
//! When the terminal shrinks below the fittable threshold the solver clips
//! trailing panes to a zero-area rect and marks them suppressed. A suppressed
//! pane occupies no cells, so these helpers treat any empty rect as suppressed.
//! For the occupancy check that needs no special handling — an empty rect
//! contributes zero area and cannot overlap anything; for the outside and
//! minimum-size checks, empty rects are explicitly skipped, because a pane
//! with no cells is neither placed wrongly nor undersized (its frozen PTY
//! size lives elsewhere).
//!
//! ## Live-pane reference checking
//!
//! Layout normalization also requires that every layout-tree leaf references a
//! live pane. [`layout_assert::assert_live_pane_refs`] checks this while staying decoupled
//! from the concrete tree and pane-registry types: it takes already-extracted
//! leaf pane ids and the set of live pane ids. The layout crate's tests pass
//! `tree.leaf_panes()` and their live set straight in, and this crate keeps
//! its dependency direction (it never depends on the layout crate).

use std::collections::HashSet;

use tile_core::geometry::{Rect, Size};
use tile_core::ids::PaneId;

/// A pane placed at a concrete rectangle, as produced by the layout solver
/// (`LayoutTree + TabRect -> Vec<(PaneId, Rect)>`).
pub type PlacedPane = (PaneId, Rect);

/// A violated layout invariant, carrying the geometry that broke it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutAssertionError {
    /// The live panes do not account for exactly the tab area.
    SpaceNotFullyOccupied { tab_area: u64, occupied_area: u64 },
    /// Two live panes share at least one cell.
    Overlap {
        a: PaneId,
        a_rect: Rect,
        b: PaneId,
        b_rect: Rect,
        overlap: Rect,
    },
    /// A live pane extends beyond the tab rect.
    OutsideTab { pane: PaneId, rect: Rect, tab: Rect },
    /// A live pane is smaller than the minimum cell size.
    MinSizeViolated { pane: PaneId, size: Size, min: Size },
    /// A layout leaf references a pane that is not live in the pane registry.
    DeadPaneReference { pane: PaneId },
}

impl std::fmt::Display for LayoutAssertionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SpaceNotFullyOccupied {
                tab_area,
                occupied_area,
            } => write!(
                f,
                "layout does not fully occupy the tab: tab area {tab_area} cells, \
                 panes occupy {occupied_area} cells"
            ),
            Self::Overlap {
                a,
                a_rect,
                b,
                b_rect,
                overlap,
            } => write!(
                f,
                "panes overlap: {a} {a_rect:?} and {b} {b_rect:?} share {overlap:?}"
            ),
            Self::OutsideTab { pane, rect, tab } => {
                write!(f, "pane {pane} {rect:?} extends outside the tab {tab:?}")
            }
            Self::MinSizeViolated { pane, size, min } => {
                write!(f, "pane {pane} size {size:?} is below the minimum {min:?}")
            }
            Self::DeadPaneReference { pane } => {
                write!(f, "layout references non-live pane {pane}")
            }
        }
    }
}

impl std::error::Error for LayoutAssertionError {}

/// Total cells a rect covers, widened so sums across many panes cannot overflow.
fn area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
}

/// Assert the live panes occupy exactly the tab area, by cell count.
///
/// Suppressed (empty) panes contribute nothing. This is necessary but not
/// sufficient for a gap-free tiling on its own — see the module docs; combine
/// with [`assert_no_overlap`] and [`assert_no_outside`].
///
/// # Errors
///
/// [`LayoutAssertionError::SpaceNotFullyOccupied`] if the summed pane area does
/// not equal the tab area.
pub fn assert_all_space_occupied(
    panes: &[PlacedPane],
    tab_rect: Rect,
) -> Result<(), LayoutAssertionError> {
    let occupied_area: u64 = panes.iter().map(|&(_, rect)| area(rect)).sum();
    let tab_area = area(tab_rect);
    if occupied_area == tab_area {
        Ok(())
    } else {
        Err(LayoutAssertionError::SpaceNotFullyOccupied {
            tab_area,
            occupied_area,
        })
    }
}

/// Assert no two live panes share a cell.
///
/// Empty (suppressed) panes never intersect, so they are skipped implicitly.
/// Reports the first overlapping pair found in iteration order.
///
/// # Errors
///
/// [`LayoutAssertionError::Overlap`] naming both panes and the shared region.
pub fn assert_no_overlap(panes: &[PlacedPane]) -> Result<(), LayoutAssertionError> {
    for (i, &(a, a_rect)) in panes.iter().enumerate() {
        for &(b, b_rect) in &panes[i + 1..] {
            if let Some(overlap) = a_rect.intersection(b_rect) {
                return Err(LayoutAssertionError::Overlap {
                    a,
                    a_rect,
                    b,
                    b_rect,
                    overlap,
                });
            }
        }
    }
    Ok(())
}

/// Assert every live pane lies fully within the tab rect.
///
/// Empty (suppressed) panes cover no cells and are skipped.
///
/// # Errors
///
/// [`LayoutAssertionError::OutsideTab`] for the first pane that spills out.
pub fn assert_no_outside(panes: &[PlacedPane], tab_rect: Rect) -> Result<(), LayoutAssertionError> {
    let tab_right = u32::from(tab_rect.origin.x) + u32::from(tab_rect.size.cols);
    let tab_bottom = u32::from(tab_rect.origin.y) + u32::from(tab_rect.size.rows);
    for &(pane, rect) in panes {
        if rect.is_empty() {
            continue;
        }
        let right = u32::from(rect.origin.x) + u32::from(rect.size.cols);
        let bottom = u32::from(rect.origin.y) + u32::from(rect.size.rows);
        if rect.origin.x < tab_rect.origin.x
            || rect.origin.y < tab_rect.origin.y
            || right > tab_right
            || bottom > tab_bottom
        {
            return Err(LayoutAssertionError::OutsideTab {
                pane,
                rect,
                tab: tab_rect,
            });
        }
    }
    Ok(())
}

/// Assert every live pane is at least `min` cells in each dimension.
///
/// Empty (suppressed) panes are exempt: their geometry is frozen at the last
/// valid size and is not subject to the live floor.
///
/// # Errors
///
/// [`LayoutAssertionError::MinSizeViolated`] for the first undersized pane.
pub fn assert_min_size_respected(
    panes: &[PlacedPane],
    min: Size,
) -> Result<(), LayoutAssertionError> {
    for &(pane, rect) in panes {
        if rect.is_empty() {
            continue;
        }
        if rect.size.cols < min.cols || rect.size.rows < min.rows {
            return Err(LayoutAssertionError::MinSizeViolated {
                pane,
                size: rect.size,
                min,
            });
        }
    }
    Ok(())
}

/// Assert every layout leaf references a live pane.
///
/// This helper intentionally accepts the already-extracted leaf pane ids
/// rather than a concrete tree type, keeping this crate independent of the
/// layout crate. Callers pass `tree.leaf_panes()` and their live set in.
///
/// # Errors
///
/// [`LayoutAssertionError::DeadPaneReference`] for the first pane id not present
/// in `live_panes`.
pub fn assert_live_pane_refs(
    layout_leaf_panes: &[PaneId],
    live_panes: &HashSet<PaneId>,
) -> Result<(), LayoutAssertionError> {
    for &pane in layout_leaf_panes {
        if !live_panes.contains(&pane) {
            return Err(LayoutAssertionError::DeadPaneReference { pane });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for layout assertion helpers.

    use super::*;
    use tile_core::geometry::Point;

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
}
