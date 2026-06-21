//! Tests for layout modes: fullscreen and tiled.

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
fn fullscreen_promotes_a_collapsed_stack_member_without_touching_the_stack() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), LayoutChild::new(stack)],
    ));
    let snapshot = tree.clone();

    // c is collapsed; fullscreen still promotes it to the whole tab while
    // the stack and every sibling hide.
    let result = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: c }, tab());
    assert_eq!(
        result.panes,
        [(a, Rect::zero()), (b, Rect::zero()), (c, tab())]
    );
    assert!(result.stack_headers.is_empty());
    assert!(result.suppressed.is_empty());

    // The stack was never rewritten: same membership, same active member,
    // same collapsed flags — so restoring shows b expanded again.
    assert_eq!(tree, snapshot);
    let restored = solve_with_mode(&tree, LayoutMode::Tiled, tab());
    let LayoutNode::Split(outer) = &tree else {
        panic!("root must stay a split");
    };
    let LayoutNode::Split(stack) = &outer.children[1].node else {
        panic!("stack must survive");
    };
    assert_eq!(stack.active, 0);
    assert_eq!(restored, solve(&tree, tab()));
    assert_eq!(restored.stack_headers.len(), 1);
    assert_eq!(restored.stack_headers[0].pane, c);
}

#[test]
fn fullscreen_of_the_active_stack_member_round_trips_identically() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::stack(vec![a, b], 1));
    let before = solve(&tree, tab());

    let zoomed = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: b }, tab());
    assert_eq!(zoomed.panes, [(a, Rect::zero()), (b, tab())]);

    assert_eq!(solve_with_mode(&tree, LayoutMode::Tiled, tab()), before);
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

#[test]
fn fullscreen_suppresses_a_tab_that_fits_content_but_not_the_border() {
    // The focused pane clears the bare (2,1) content floor in a 3x2 tab but not
    // the border-inclusive (4,3). Fullscreen must suppress it — matching the
    // tiled solve — instead of drawing it under an overlapping border. At
    // exactly (4,3) it becomes visible, insetting to the (2,1) content minimum.
    let (a, b) = (PaneId::new(), PaneId::new());
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![leaf(a), leaf(b)],
    ));

    let cramped = Rect::new(Point { x: 0, y: 0 }, Size { cols: 3, rows: 2 });
    let suppressed = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: a }, cramped);
    assert_eq!(suppressed.suppressed, [a]);
    assert!(suppressed.all_suppressed);

    let snug = Rect::new(Point { x: 0, y: 0 }, Size { cols: 4, rows: 3 });
    let shown = solve_with_mode(&tree, LayoutMode::Fullscreen { focused: a }, snug);
    assert!(shown.suppressed.is_empty());
    assert!(!shown.all_suppressed);
    assert_eq!(shown.panes, [(a, snug), (b, Rect::zero())]);
    assert_eq!(snug.inner_with_border().size, Size { cols: 2, rows: 1 });
}
