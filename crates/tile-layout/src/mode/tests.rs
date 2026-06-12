use tile_core::geometry::{Point, Rect, Size, SplitDirection};

use super::*;
use crate::solver::{solve, solve_with_mode};
use crate::tree::{LayoutChild, LayoutNode, SplitNode};

fn leaf(pane: PaneId) -> LayoutChild {
    LayoutChild::new(LayoutNode::Pane(pane))
}

fn tab() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 })
}

/// a beside (b over c).
fn nested(a: PaneId, b: PaneId, c: PaneId) -> LayoutNode {
    let column = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![leaf(b), leaf(c)],
    ));
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(column)],
    ))
}

#[test]
fn fullscreen_promotes_the_focused_pane_and_hides_the_rest() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested(a, b, c);

    let result = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: b }, tab());
    assert_eq!(
        result.panes,
        [(a, Rect::zero()), (b, tab()), (c, Rect::zero())]
    );
    // Hidden panes are not suppressed: they come back on toggle, and no
    // terminal-too-small overlay belongs over a working fullscreen pane.
    assert!(result.suppressed.is_empty());
    assert!(!result.all_suppressed);
}

#[test]
fn leaving_fullscreen_restores_the_exact_prior_layout() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested(a, b, c);
    let before = solve_with_mode(&tree, LayoutMode::Tiled, tab());

    // Entering and leaving fullscreen never rewrites the tree, so the tiled
    // solve afterwards is identical — including the tree itself.
    let snapshot = tree.clone();
    let _ = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: c }, tab());
    assert_eq!(tree, snapshot);
    assert_eq!(solve_with_mode(&tree, LayoutMode::Tiled, tab()), before);
}

#[test]
fn tiled_mode_matches_the_plain_solve() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested(a, b, c);
    assert_eq!(
        solve_with_mode(&tree, LayoutMode::Tiled, tab()),
        solve(&tree, tab())
    );
}

#[test]
fn stale_fullscreen_focus_falls_back_to_tiled() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tree = nested(a, b, c);

    let gone = PaneId::new();
    let result = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: gone }, tab());
    assert_eq!(result, solve(&tree, tab()));
}

#[test]
fn fullscreen_in_a_too_small_tab_suppresses_and_flags_the_overlay() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b)],
    ));
    let tiny = Rect::new(Point { x: 0, y: 0 }, Size { cols: 1, rows: 1 });

    let result = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: a }, tiny);
    assert_eq!(result.suppressed, [a]);
    assert!(result.all_suppressed);
}
