//! Derivation of per-pane **content rects** from a solved layout.
//!
//! The geometry solver ([`crate::solver`]) produces *outer* pane rects — the
//! full cell box a pane occupies, including the one-cell border drawn around
//! it. Subtracting that border to obtain the **content rect** (the cells a
//! child PTY — the pseudo-terminal process running inside the pane — draws
//! into, and the cells the renderer fills) happens in exactly one place:
//! here. Both PTY sizing and the render snapshot consume this output, so the
//! size a child is given can never drift from the box drawn around it on
//! screen.

use std::collections::HashSet;

use koshi_core::geometry::Rect;
use koshi_core::ids::PaneId;

use crate::solver::SolveResult;

/// The content rect for every pane in `solve`, in solve order.
///
/// Each entry is `(pane, Some(content_rect))` for a pane currently showing
/// content, or `(pane, None)` for one that is not — meaning its PTY keeps its
/// last size and must not be resized. A pane shows no content when it is:
///
/// - space-suppressed (listed in [`SolveResult::suppressed`]),
/// - hidden — a zero-area rect, e.g. a non-focused pane under fullscreen, or
/// - a collapsed stack member, whose rect is the Koshi-owned header strip
///   rather than content (listed in [`SolveResult::stack_headers`]).
///
/// A content-showing pane's rect is its outer rect inset by the fixed one-cell
/// border ([`Rect::inner_with_border`]); the border is not configurable. The
/// rect is returned un-floored — a tiny visible pane can inset to a zero-area
/// content rect (still `Some`, distinct from a not-shown pane's `None`); the
/// PTY layer applies its own minimum-size floor.
#[must_use]
pub fn content_rects(solve: &SolveResult) -> Vec<(PaneId, Option<Rect>)> {
    let suppressed: HashSet<PaneId> = solve.suppressed.iter().copied().collect();
    let collapsed: HashSet<PaneId> = solve
        .stack_headers
        .iter()
        .map(|header| header.pane)
        .collect();

    solve
        .panes
        .iter()
        .map(|&(pane, outer)| {
            if suppressed.contains(&pane) || outer.is_empty() || collapsed.contains(&pane) {
                (pane, None)
            } else {
                (pane, Some(outer.inner_with_border()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
