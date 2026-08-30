//! Layout invariant assertions for pure-layout tests.
//!
//! The layout engine maps a layout tree over a tab rect to placed pane
//! rectangles. Each helper here checks one geometric invariant against a slice
//! of placed panes: the live panes tile the whole tab area, no two panes
//! overlap, nothing spills outside the tab, and every live pane respects the
//! minimum cell size. A broken invariant returns a
//! [`layout_assert::LayoutAssertionError`] that names the panes involved.
//!
//! Exact tiling holds when three checks all pass:
//! [`layout_assert::assert_all_space_occupied`] (the summed pane area equals
//! the tab area), [`layout_assert::assert_no_overlap`] (no cell is counted
//! twice), and [`layout_assert::assert_no_outside`] (no cell lies beyond the
//! tab). Each check alone passes some layouts that are not exact tilings.
//!
//! ## Suppressed panes
//!
//! The solver clips a pane it cannot fit to a zero-area rect and marks it
//! suppressed. These helpers treat every empty rect (zero `cols` or zero
//! `rows`) as suppressed, wherever its origin lies. The occupancy check counts
//! it as zero area. The overlap check never reports it. The outside and
//! minimum-size checks skip it.
//!
//! ## Live pane references
//!
//! [`layout_assert::assert_live_pane_refs`] checks that every layout-tree
//! leaf references a live pane. It takes the extracted leaf pane ids and the
//! set of live pane ids. The layout crate's tests pass `tree.leaf_panes()` and
//! their live set straight in.

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

/// Total cells a rect covers, `cols * rows`, computed in `u64`.
fn area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
}

/// Assert the live panes occupy exactly the tab area, by cell count.
///
/// Sums `cols * rows` over every pane and compares the sum with the tab's.
/// Suppressed (empty) panes add zero. Passes when the sums are equal even if
/// the panes overlap or lie outside the tab; [`assert_no_overlap`] and
/// [`assert_no_outside`] catch those.
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
/// An empty (suppressed) pane intersects nothing and is never reported. Panes
/// that only touch along an edge or at a corner do not overlap. Reports the
/// first overlapping pair in iteration order: pane `0` is compared against
/// every pane after it, then pane `1`, and so on.
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
/// A pane is inside when its origin is at or past the tab's origin and its
/// right and bottom edges, computed in `u32`, do not pass the tab's. Empty
/// (suppressed) panes are skipped, wherever their origin lies.
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

/// Assert every live pane is at least `min.cols` wide and `min.rows` tall.
///
/// Empty (suppressed) panes are exempt.
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
/// Takes the extracted leaf pane ids, not a concrete tree type. Callers pass
/// `tree.leaf_panes()` and their live set in. An empty `layout_leaf_panes`
/// passes.
///
/// # Errors
///
/// [`LayoutAssertionError::DeadPaneReference`] for the first pane id, in slice
/// order, not present in `live_panes`.
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
