//! Derivation of per-pane **content rects** from a solved layout.
//!
//! The geometry solver ([`crate::solver`]) produces *outer* pane rects — the
//! full cell box a pane occupies, including the one-cell border drawn around
//! it. Subtracting that border to obtain the **content rect** (the cells a
//! child PTY — the pseudo-terminal process running inside the pane — draws
//! into, and the cells the renderer fills) happens in exactly one place: here.
//! Both PTY sizing and the render snapshot consume this output.

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
    // `suppressed` and `stack_headers` each hold one entry per pane that is out
    // of room or collapsed, so both are scanned in place. This runs on every
    // pointer move.
    let collapsed = |pane: PaneId| solve.stack_headers.iter().any(|header| header.pane == pane);

    solve
        .panes
        .iter()
        .map(|&(pane, outer)| {
            if solve.suppressed.contains(&pane) || outer.is_empty() || collapsed(pane) {
                (pane, None)
            } else {
                (pane, Some(outer.inner_with_border()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
