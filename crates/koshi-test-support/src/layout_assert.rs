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
//! `tree.list_leaf_pane_ids()` and their live pane-id set to it.

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
    SpaceNotFullyOccupied {
        tab_cell_area: u64,
        occupied_cell_area: u64,
    },
    /// Two live panes share at least one cell.
    Overlap {
        first_pane_id: PaneId,
        first_pane_rect: Rect,
        second_pane_id: PaneId,
        second_pane_rect: Rect,
        overlap_rect: Rect,
    },
    /// A live pane extends beyond the tab rect.
    OutsideTab {
        pane_id: PaneId,
        pane_rect: Rect,
        tab_rect: Rect,
    },
    /// A live pane is smaller than the minimum cell size.
    MinimumSizeViolated {
        pane_id: PaneId,
        pane_size: Size,
        minimum_size: Size,
    },
    /// A layout leaf references a pane that is not live in the pane registry.
    DeadPaneReference { pane_id: PaneId },
}

impl std::fmt::Display for LayoutAssertionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SpaceNotFullyOccupied {
                tab_cell_area,
                occupied_cell_area,
            } => write!(
                f,
                "layout does not fully occupy the tab: tab area {tab_cell_area} cells, \
                 panes occupy {occupied_cell_area} cells"
            ),
            Self::Overlap {
                first_pane_id,
                first_pane_rect,
                second_pane_id,
                second_pane_rect,
                overlap_rect,
            } => write!(
                f,
                "panes overlap: {first_pane_id} {first_pane_rect:?} and \
                 {second_pane_id} {second_pane_rect:?} share {overlap_rect:?}"
            ),
            Self::OutsideTab {
                pane_id,
                pane_rect,
                tab_rect,
            } => {
                write!(
                    f,
                    "pane {pane_id} {pane_rect:?} extends outside the tab {tab_rect:?}"
                )
            }
            Self::MinimumSizeViolated {
                pane_id,
                pane_size,
                minimum_size,
            } => {
                write!(
                    f,
                    "pane {pane_id} size {pane_size:?} is below the minimum {minimum_size:?}"
                )
            }
            Self::DeadPaneReference { pane_id } => {
                write!(f, "layout references non-live pane {pane_id}")
            }
        }
    }
}

impl std::error::Error for LayoutAssertionError {}

/// Return the cells covered by `rect` as `cols * rows` in `u64`.
fn compute_cell_area(rect: Rect) -> u64 {
    u64::from(rect.cell_size.column_count) * u64::from(rect.cell_size.row_count)
}

/// Check that placed pane areas sum to the tab area.
///
/// Sums `column_count * row_count` for every placed pane and compares the result with the tab area.
/// Empty panes add zero. Equal sums do not prove that panes do not overlap or
/// stay inside the tab; use [`check_no_overlap`] and [`check_no_outside`] too.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::SpaceNotFullyOccupied`] when the sums differ.
pub fn check_all_space_occupied(
    placed_panes: &[PlacedPane],
    tab_rect: Rect,
) -> Result<(), LayoutAssertionError> {
    let occupied_cell_area: u64 = placed_panes
        .iter()
        .map(|&(_, pane_rect)| compute_cell_area(pane_rect))
        .sum();
    let tab_cell_area = compute_cell_area(tab_rect);
    if occupied_cell_area == tab_cell_area {
        Ok(())
    } else {
        Err(LayoutAssertionError::SpaceNotFullyOccupied {
            tab_cell_area,
            occupied_cell_area,
        })
    }
}

/// Check that no two placed panes share a cell.
///
/// Empty panes never overlap. Panes that touch at an edge or corner do not
/// overlap. Returns the first pair in iteration order: pane `0` is compared
/// with each following pane, then pane `1`, and so on.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::Overlap`] with both panes and their shared
/// region.
pub fn check_no_overlap(placed_panes: &[PlacedPane]) -> Result<(), LayoutAssertionError> {
    for (pane_index, &(first_pane_id, first_pane_rect)) in placed_panes.iter().enumerate() {
        for &(second_pane_id, second_pane_rect) in &placed_panes[pane_index + 1..] {
            if let Some(overlap_rect) = first_pane_rect.compute_intersection(second_pane_rect) {
                return Err(LayoutAssertionError::Overlap {
                    first_pane_id,
                    first_pane_rect,
                    second_pane_id,
                    second_pane_rect,
                    overlap_rect,
                });
            }
        }
    }
    Ok(())
}

/// Check that every non-empty placed pane lies within `tab_rect`.
///
/// A pane is inside when its origin is not before the tab origin and its right
/// and bottom edges, computed in `u32`, do not pass the tab edges. Empty panes
/// are skipped, regardless of their origin.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::OutsideTab`] for the first pane that spills
/// out.
pub fn check_no_outside(
    placed_panes: &[PlacedPane],
    tab_rect: Rect,
) -> Result<(), LayoutAssertionError> {
    let tab_right = u32::from(tab_rect.origin.column) + u32::from(tab_rect.cell_size.column_count);
    let tab_bottom = u32::from(tab_rect.origin.row) + u32::from(tab_rect.cell_size.row_count);
    for &(pane_id, pane_rect) in placed_panes {
        if pane_rect.is_empty() {
            continue;
        }
        let pane_right_edge_column =
            u32::from(pane_rect.origin.column) + u32::from(pane_rect.cell_size.column_count);
        let pane_bottom_edge_row =
            u32::from(pane_rect.origin.row) + u32::from(pane_rect.cell_size.row_count);
        if pane_rect.origin.column < tab_rect.origin.column
            || pane_rect.origin.row < tab_rect.origin.row
            || pane_right_edge_column > tab_right
            || pane_bottom_edge_row > tab_bottom
        {
            return Err(LayoutAssertionError::OutsideTab {
                pane_id,
                pane_rect,
                tab_rect,
            });
        }
    }
    Ok(())
}

/// Check that placed panes tile `tab_rect` exactly.
///
/// Runs [`check_all_space_occupied`], [`check_no_overlap`], and
/// [`check_no_outside`] in that order and stops at the first failure.
///
/// # Errors
///
/// Returns the first error in that order.
pub fn check_exact_tiling(
    placed_panes: &[PlacedPane],
    tab_rect: Rect,
) -> Result<(), LayoutAssertionError> {
    check_all_space_occupied(placed_panes, tab_rect)?;
    check_no_overlap(placed_panes)?;
    check_no_outside(placed_panes, tab_rect)
}

/// Check that every non-empty placed pane is at least `minimum_size.column_count` wide and `minimum_size.row_count`
/// tall.
///
/// Empty panes are exempt.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::MinimumSizeViolated`] for the first undersized
/// pane.
pub fn check_minimum_size_respected(
    placed_panes: &[PlacedPane],
    minimum_size: Size,
) -> Result<(), LayoutAssertionError> {
    for &(pane_id, pane_rect) in placed_panes {
        if pane_rect.is_empty() {
            continue;
        }
        if pane_rect.cell_size.column_count < minimum_size.column_count
            || pane_rect.cell_size.row_count < minimum_size.row_count
        {
            return Err(LayoutAssertionError::MinimumSizeViolated {
                pane_id,
                pane_size: pane_rect.cell_size,
                minimum_size,
            });
        }
    }
    Ok(())
}

/// Check that every layout leaf references a live pane.
///
/// Takes extracted leaf pane ids rather than a concrete tree type. Callers pass
/// `tree.list_leaf_pane_ids()` and a set of live pane ids. An empty
/// `layout_leaf_pane_ids` passes.
///
/// # Errors
///
/// Returns [`LayoutAssertionError::DeadPaneReference`] for the first id in
/// slice order that is absent from `live_pane_ids`.
pub fn check_live_pane_refs(
    layout_leaf_pane_ids: &[PaneId],
    live_pane_ids: &HashSet<PaneId>,
) -> Result<(), LayoutAssertionError> {
    for &pane_id in layout_leaf_pane_ids {
        if !live_pane_ids.contains(&pane_id) {
            return Err(LayoutAssertionError::DeadPaneReference { pane_id });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
