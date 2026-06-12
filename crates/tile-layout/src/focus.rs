//! Focus candidates after a pane disappears.
//!
//! Picking the next focused pane is session policy, not layout's job — the
//! session owns focus history and per-client state. What layout can answer
//! is the geometric part: given where the removed pane was and where the
//! survivors now sit, which panes are sensible focus targets? This module
//! returns those candidates, ranked three ways, and chooses nothing.
//!
//! Zero-area panes are never candidates: a pane without visible cells —
//! suppressed, hidden by a fullscreen overlay, or collapsed into a stack
//! header — must not silently receive focus. Reaching one of those goes
//! through an explicit activation, not through focus repair.

use tile_core::geometry::Rect;
use tile_core::ids::PaneId;

/// Focus targets after a removal, for the caller to rank against its own
/// focus history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusCandidates {
    /// The visible pane whose center is closest to the removed pane's
    /// center. Ties go to the earlier pane in layout order.
    pub spatial_neighbor: Option<PaneId>,
    /// The visible pane that took over the largest share of the removed
    /// pane's cells. `None` when nothing overlaps the old rect.
    pub absorbed_space: Option<PaneId>,
    /// Every visible pane, in layout order — the last-resort fallback.
    pub layout_order: Vec<PaneId>,
}

/// Rank the surviving panes as focus targets for a pane that occupied
/// `removed_rect`.
///
/// `surviving_panes` is the solved placement of the layout after the
/// removal, in layout order (exactly what the solver returns).
#[must_use]
pub fn focus_candidates(removed_rect: Rect, surviving_panes: &[(PaneId, Rect)]) -> FocusCandidates {
    let visible: Vec<(PaneId, Rect)> = surviving_panes
        .iter()
        .copied()
        .filter(|&(_, rect)| !rect.is_empty())
        .collect();

    let spatial_neighbor = visible
        .iter()
        .min_by_key(|&&(_, rect)| center_distance(removed_rect, rect))
        .map(|&(pane, _)| pane);

    // Largest absorbed area wins; on a tie the earlier pane in layout order
    // keeps it, because only strictly larger areas displace the holder.
    let mut absorbed: Option<(PaneId, u64)> = None;
    for &(pane, rect) in &visible {
        let Some(overlap) = rect.intersection(removed_rect) else {
            continue;
        };
        let area = cell_area(overlap);
        if absorbed.is_none_or(|(_, best)| area > best) {
            absorbed = Some((pane, area));
        }
    }
    let absorbed_space = absorbed.map(|(pane, _)| pane);

    let layout_order = visible.into_iter().map(|(pane, _)| pane).collect();

    FocusCandidates {
        spatial_neighbor,
        absorbed_space,
        layout_order,
    }
}

/// Squared distance between two rect centers, on doubled coordinates so
/// half-cell centers stay exact integers.
fn center_distance(a: Rect, b: Rect) -> u64 {
    let (ax, ay) = doubled_center(a);
    let (bx, by) = doubled_center(b);
    let dx = i64::from(ax) - i64::from(bx);
    let dy = i64::from(ay) - i64::from(by);
    (dx * dx + dy * dy) as u64
}

fn doubled_center(rect: Rect) -> (u32, u32) {
    (
        2 * u32::from(rect.origin.x) + u32::from(rect.size.cols),
        2 * u32::from(rect.origin.y) + u32::from(rect.size.rows),
    )
}

fn cell_area(rect: Rect) -> u64 {
    u64::from(rect.size.cols) * u64::from(rect.size.rows)
}

#[cfg(test)]
mod tests;
