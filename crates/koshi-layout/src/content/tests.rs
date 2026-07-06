//! Tests for [`content_rects`].

use koshi_core::geometry::{Point, Size};

use super::*;
use crate::solver::StackHeader;

/// Constructs a test `Rect` at position (x, y) with the given dimensions.
fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

/// Constructs a test `SolveResult` with the given panes, suppression list, and stack headers.
fn solve_result(
    panes: Vec<(PaneId, Rect)>,
    suppressed: Vec<PaneId>,
    stack_headers: Vec<StackHeader>,
) -> SolveResult {
    SolveResult {
        panes,
        suppressed,
        all_suppressed: false,
        stack_headers,
    }
}

/// Constructs a test stack header for `pane`. The rect value is arbitrary;
/// `content_rects` only examines membership in the list.
fn header(pane: PaneId) -> StackHeader {
    StackHeader {
        pane,
        rect: rect(0, 0, 10, 1),
        position: 0,
        total: 2,
    }
}

#[test]
fn a_visible_pane_is_inset_by_one_cell() {
    let pane = PaneId::new();
    let solve = solve_result(vec![(pane, rect(0, 0, 10, 10))], vec![], vec![]);

    assert_eq!(content_rects(&solve), vec![(pane, Some(rect(1, 1, 8, 8)))]);
}

#[test]
fn a_suppressed_pane_yields_none_even_with_a_nonempty_rect() {
    // Suppression status is determined by the suppression list, not the rect.
    // This test isolates the list branch from the zero-area branch.
    let pane = PaneId::new();
    let solve = solve_result(vec![(pane, rect(0, 0, 10, 10))], vec![pane], vec![]);

    assert_eq!(content_rects(&solve), vec![(pane, None)]);
}

#[test]
fn a_hidden_zero_area_pane_yields_none() {
    let pane = PaneId::new();
    let solve = solve_result(vec![(pane, Rect::zero())], vec![], vec![]);

    assert_eq!(content_rects(&solve), vec![(pane, None)]);
}

#[test]
fn a_collapsed_stack_member_yields_none_despite_a_nonempty_strip() {
    // A collapsed stack member's rect is the header strip (non-empty), but
    // it yields None because the header is layout furniture, not pane content.
    let pane = PaneId::new();
    let solve = solve_result(vec![(pane, rect(0, 0, 10, 1))], vec![], vec![header(pane)]);

    assert_eq!(content_rects(&solve), vec![(pane, None)]);
}

#[test]
fn a_tiny_visible_pane_stays_some_with_a_zero_area_content_rect() {
    // A visible pane that is too small for the border insets to zero area but
    // still yields Some, signaling that the pane is shown. (Readers that care
    // about minimum content area handle the zero case themselves.)
    let pane = PaneId::new();
    let solve = solve_result(vec![(pane, rect(5, 5, 1, 1))], vec![], vec![]);

    let result = content_rects(&solve);
    assert_eq!(result, vec![(pane, Some(rect(6, 6, 0, 0)))]);
    assert!(result[0].1.is_some());
    assert!(result[0].1.unwrap().is_empty());
}

#[test]
fn solve_order_is_preserved() {
    let first = PaneId::new();
    let second = PaneId::new();
    let third = PaneId::new();
    let solve = solve_result(
        vec![
            (first, rect(0, 0, 10, 10)),
            (second, rect(10, 0, 10, 10)),
            (third, rect(20, 0, 10, 10)),
        ],
        vec![],
        vec![],
    );

    let panes: Vec<PaneId> = content_rects(&solve).into_iter().map(|(p, _)| p).collect();
    assert_eq!(panes, vec![first, second, third]);
}

#[test]
fn a_mixed_solve_maps_each_pane_by_its_state() {
    let visible = PaneId::new();
    let suppressed = PaneId::new();
    let hidden = PaneId::new();
    let collapsed = PaneId::new();
    let solve = solve_result(
        vec![
            (visible, rect(0, 0, 10, 10)),
            (suppressed, Rect::zero()),
            (hidden, Rect::zero()),
            (collapsed, rect(0, 0, 10, 1)),
        ],
        vec![suppressed],
        vec![header(collapsed)],
    );

    assert_eq!(
        content_rects(&solve),
        vec![
            (visible, Some(rect(1, 1, 8, 8))),
            (suppressed, None),
            (hidden, None),
            (collapsed, None),
        ]
    );
}
