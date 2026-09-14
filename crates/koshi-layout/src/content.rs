//! Derivation of per-pane **content rects** from a solved layout.
//!
//! The geometry solver ([`crate::solver`]) produces *outer* pane rects: the
//! full cell box a pane occupies, including the one-cell border drawn around
//! it. This module subtracts that border to give the **content rect**: the
//! cells the child PTY (the pseudo-terminal process running inside the pane)
//! draws into, and the cells the renderer fills. PTY sizing and the render
//! snapshot both consume this output.

use koshi_core::geometry::Rect;
use koshi_core::ids::PaneId;

use crate::solver::{is_content_visible, LayoutSolve};

/// The content rect for every pane in `solve`, in solve order.
///
/// Each entry is `(pane, Some(content_rect))` for a pane that shows content,
/// or `(pane, None)` for one that does not; the caller keeps that pane's PTY
/// at its last size. A pane shows no content when it is:
///
/// - listed in [`LayoutSolve::suppressed_pane_ids`],
/// - zero-area (for example a non-focused pane under fullscreen), or
/// - listed in [`LayoutSolve::stack_headers`]: a collapsed stack member whose
///   rect is its header strip.
///
/// A content-showing pane's rect is its outer rect inset by the fixed one-cell
/// border ([`Rect::compute_inner_with_border`]). The rect has no size floor: a pane
/// with two or fewer columns, or two or fewer rows, insets to a zero-area
/// content rect, still `Some`. The PTY layer applies its own minimum-size
/// floor.
#[must_use]
pub fn list_content_rects(solved_layout: &LayoutSolve) -> Vec<(PaneId, Option<Rect>)> {
    solved_layout
        .pane_rects
        .iter()
        .map(|&(pane_id, outer_pane_rect)| {
            let has_visible_content = !solved_layout.suppressed_pane_ids.contains(&pane_id)
                && is_content_visible(pane_id, outer_pane_rect, &solved_layout.stack_headers);
            (
                pane_id,
                has_visible_content.then(|| outer_pane_rect.compute_inner_with_border()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests;
