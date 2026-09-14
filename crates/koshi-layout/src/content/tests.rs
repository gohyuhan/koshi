//! Tests for [`list_content_rects`].

use koshi_core::geometry::{Point, Size};

use super::*;
use crate::solver::StackHeader;

/// Constructs a test cell rectangle at the given origin and dimensions.
fn build_cell_rect(column_index: u16, row_index: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(
        Point {
            column: column_index,
            row: row_index,
        },
        Size {
            column_count,
            row_count,
        },
    )
}

/// Constructs a test layout solution with the given pane rectangles, suppressed pane IDs, and stack headers.
fn build_layout_solution(
    pane_rects: Vec<(PaneId, Rect)>,
    suppressed_pane_ids: Vec<PaneId>,
    stack_headers: Vec<StackHeader>,
) -> LayoutSolve {
    LayoutSolve {
        pane_rects,
        suppressed_pane_ids,
        is_all_panes_suppressed: false,
        stack_headers,
    }
}

/// Constructs a test stack header for `pane_id`. The rectangle value is arbitrary;
/// `list_content_rects` only examines membership in the list.
fn build_stack_header(pane_id: PaneId) -> StackHeader {
    StackHeader {
        pane_id,
        header_rect: build_cell_rect(0, 0, 10, 1),
        member_index: 0,
        member_count: 2,
    }
}

#[test]
fn a_visible_pane_is_inset_by_one_cell() {
    let pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![(pane_id, build_cell_rect(0, 0, 10, 10))],
        vec![],
        vec![],
    );

    assert_eq!(
        list_content_rects(&layout_solution),
        vec![(pane_id, Some(build_cell_rect(1, 1, 8, 8)))],
    );
}

#[test]
fn a_suppressed_pane_yields_none_even_with_a_nonempty_rect() {
    // Suppression status is determined by the suppression list, not the rect.
    // This test isolates the list branch from the zero-area branch.
    let pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![(pane_id, build_cell_rect(0, 0, 10, 10))],
        vec![pane_id],
        vec![],
    );

    assert_eq!(list_content_rects(&layout_solution), vec![(pane_id, None)]);
}

#[test]
fn a_hidden_zero_area_pane_yields_none() {
    let pane_id = PaneId::new();
    let layout_solution =
        build_layout_solution(vec![(pane_id, Rect::empty_at_origin())], vec![], vec![]);

    assert_eq!(list_content_rects(&layout_solution), vec![(pane_id, None)]);
}

#[test]
fn a_collapsed_stack_member_yields_none_despite_a_nonempty_strip() {
    // A collapsed stack member's rect is its header strip (non-empty); the
    // header list, not the rect, decides that it yields None.
    let pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![(pane_id, build_cell_rect(0, 0, 10, 1))],
        vec![],
        vec![build_stack_header(pane_id)],
    );

    assert_eq!(list_content_rects(&layout_solution), vec![(pane_id, None)]);
}

#[test]
fn a_tiny_visible_pane_stays_some_with_a_zero_area_content_rect() {
    // A visible pane that is too small for the border insets to zero area but
    // still yields Some, signaling that the pane is shown. (Readers that care
    // about minimum content area handle the zero case themselves.)
    let pane_id = PaneId::new();
    let layout_solution =
        build_layout_solution(vec![(pane_id, build_cell_rect(5, 5, 1, 1))], vec![], vec![]);

    let content_rect_entries = list_content_rects(&layout_solution);
    assert_eq!(
        content_rect_entries,
        vec![(pane_id, Some(build_cell_rect(6, 6, 0, 0)))],
    );
    assert!(content_rect_entries[0].1.is_some());
    assert!(content_rect_entries[0].1.unwrap().is_empty());
}

#[test]
fn a_three_by_three_pane_insets_to_one_content_cell() {
    let pane_id = PaneId::new();
    let layout_solution =
        build_layout_solution(vec![(pane_id, build_cell_rect(4, 2, 3, 3))], vec![], vec![]);

    assert_eq!(
        list_content_rects(&layout_solution),
        vec![(pane_id, Some(build_cell_rect(5, 3, 1, 1)))],
    );
}

#[test]
fn a_pane_with_columns_but_no_rows_yields_none() {
    let pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![(pane_id, build_cell_rect(0, 0, 10, 0))],
        vec![],
        vec![],
    );

    assert_eq!(list_content_rects(&layout_solution), vec![(pane_id, None)]);
}

#[test]
fn a_pane_at_the_coordinate_limit_insets_without_overflow() {
    let pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![(pane_id, build_cell_rect(u16::MAX, u16::MAX, 1, 1))],
        vec![],
        vec![],
    );

    assert_eq!(
        list_content_rects(&layout_solution),
        vec![(pane_id, Some(build_cell_rect(u16::MAX, u16::MAX, 0, 0)))],
    );
}

#[test]
fn an_empty_solve_yields_no_entries() {
    let layout_solution = build_layout_solution(vec![], vec![], vec![]);

    assert_eq!(
        list_content_rects(&layout_solution),
        Vec::<(PaneId, Option<Rect>)>::new()
    );
}

#[test]
fn solve_order_is_preserved() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![
            (first_pane_id, build_cell_rect(0, 0, 10, 10)),
            (second_pane_id, build_cell_rect(10, 0, 10, 10)),
            (third_pane_id, build_cell_rect(20, 0, 10, 10)),
        ],
        vec![],
        vec![],
    );

    let pane_ids: Vec<PaneId> = list_content_rects(&layout_solution)
        .into_iter()
        .map(|(pane_id, _)| pane_id)
        .collect();
    assert_eq!(pane_ids, vec![first_pane_id, second_pane_id, third_pane_id]);
}

#[test]
fn a_mixed_solve_maps_each_pane_by_its_state() {
    let visible_pane_id = PaneId::new();
    let suppressed_pane_id = PaneId::new();
    let hidden_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let layout_solution = build_layout_solution(
        vec![
            (visible_pane_id, build_cell_rect(0, 0, 10, 10)),
            (suppressed_pane_id, Rect::empty_at_origin()),
            (hidden_pane_id, Rect::empty_at_origin()),
            (collapsed_pane_id, build_cell_rect(0, 0, 10, 1)),
        ],
        vec![suppressed_pane_id],
        vec![build_stack_header(collapsed_pane_id)],
    );

    assert_eq!(
        list_content_rects(&layout_solution),
        vec![
            (visible_pane_id, Some(build_cell_rect(1, 1, 8, 8))),
            (suppressed_pane_id, None),
            (hidden_pane_id, None),
            (collapsed_pane_id, None),
        ]
    );
}
