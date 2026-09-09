//! Layout invariant checks for pure-layout tests.
//!
//! The layout solver maps a layout tree onto a tab rect and returns placed pane
//! rectangles. These helpers check pane area, overlap, tab bounds, minimum cell
//! size, and live pane references. Each helper returns `Result`; a failure is a
//! [`layout_assert::LayoutAssertionError`] that carries the relevant pane or
//! geometry.
//!
//! Exact tiling requires all three checks:
//! [`layout_assert::check_all_space_occupied`] compares total pane area with the
//! tab area, [`layout_assert::check_no_overlap`] checks that no cell is shared,
//! and [`layout_assert::check_no_outside`] checks the tab bounds. Each check
//! alone can accept a layout that is not an exact tiling.
//! [`layout_assert::check_exact_tiling`] runs them in that order.
//!
//! ## Suppressed panes
//!
//! The solver clips a pane that cannot fit to an empty rect and marks it
//! suppressed. These helpers treat every rect with zero `cols` or zero `rows`
//! as suppressed, regardless of its origin. Occupancy counts it as zero area;
//! overlap ignores it; bounds and minimum-size checks skip it.
//!
//! ## Live pane references
//!
//! [`layout_assert::check_live_pane_refs`] checks the pane ids extracted from
//! layout leaves against a set of live pane ids. Layout tests pass
//! `tree.leaf_panes()` and their live set to it.

use std::collections::HashSet;

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::PaneId;

/// A pane id paired with the rectangle assigned by the layout solver
/// (`LayoutTree + TabRect -> Vec<(PaneId, Rect)>`).
pub type PlacedPane = (PaneId, Rect);

/// A layout invariant failure with the geometry that caused it.
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

/// Return the cells covered by `rect` as `cols * rows` in `u64`.
fn area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
}

/// Check that pane areas sum to the tab area.
///
/// Sums `cols * rows` for every pane and compares the result with the tab area.
/// Empty panes add zero. Equal sums do not prove that panes do not overlap or
/// stay inside the tab; use [`check_no_overlap`] and [`check_no_outside`] too.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::SpaceNotFullyOccupied`] when the sums differ.
pub fn check_all_space_occupied(
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

/// Check that no two panes share a cell.
///
/// Empty panes never overlap. Panes that touch at an edge or corner do not
/// overlap. Returns the first pair in iteration order: pane `0` is compared
/// with each following pane, then pane `1`, and so on.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::Overlap`] with both panes and their shared
/// region.
pub fn check_no_overlap(panes: &[PlacedPane]) -> Result<(), LayoutAssertionError> {
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

/// Check that every non-empty pane lies within `tab_rect`.
///
/// A pane is inside when its origin is not before the tab origin and its right
/// and bottom edges, computed in `u32`, do not pass the tab edges. Empty panes
/// are skipped, regardless of their origin.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::OutsideTab`] for the first pane that spills
/// out.
pub fn check_no_outside(panes: &[PlacedPane], tab_rect: Rect) -> Result<(), LayoutAssertionError> {
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

/// Check that panes tile `tab_rect` exactly.
///
/// Runs [`check_all_space_occupied`], [`check_no_overlap`], and
/// [`check_no_outside`] in that order and stops at the first failure.
///
/// # Errors
///
/// Returns the first error in that order.
pub fn check_exact_tiling(
    panes: &[PlacedPane],
    tab_rect: Rect,
) -> Result<(), LayoutAssertionError> {
    check_all_space_occupied(panes, tab_rect)?;
    check_no_overlap(panes)?;
    check_no_outside(panes, tab_rect)
}

/// Check that every non-empty pane is at least `min.cols` wide and `min.rows`
/// tall.
///
/// Empty panes are exempt.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::MinSizeViolated`] for the first undersized
/// pane.
pub fn check_min_size_respected(
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

/// Check that every layout leaf references a live pane.
///
/// Takes extracted leaf pane ids rather than a concrete tree type. Callers pass
/// `tree.leaf_panes()` and a set of live pane ids. An empty
/// `layout_leaf_panes` passes.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::DeadPaneReference`] for the first id in
/// slice order that is absent from `live_panes`.
pub fn check_live_pane_refs(
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
