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

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::PaneId;

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
mod tests;
