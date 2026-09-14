//! Tests for focus candidate ranking and stack activation.
//!
//! **Focus candidates** are candidate panes to receive keyboard focus when the currently-focused
//! pane is closed. Tests verify the three rankings — nearest center distance, largest absorbed
//! area, and layout order — with ties going to the earlier pane in layout order.
//!
//! **Stack activation** tests verify that focus can cycle forward/backward through a stack's
//! members (collapsing the prior and expanding the new), and that the deepest stack containing
//! a pane can be located and then activated by ID.

use koshi_core::geometry::{Point, Size};

use super::*;

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

#[test]
fn nearest_pane_by_center_is_the_spatial_neighbor() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    // The removed pane sat at columns 26..53; the left pane (0..40) is nearer
    // than the right pane (40..80) by center distance.
    let removed_pane_rect = build_cell_rect(26, 0, 27, 24);
    let surviving_pane_rects = [
        (left_pane_id, build_cell_rect(0, 0, 40, 24)),
        (right_pane_id, build_cell_rect(40, 0, 40, 24)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(left_pane_id));
}

#[test]
fn vertical_neighbors_rank_by_distance_too() {
    let (top_pane_id, bottom_pane_id) = (PaneId::new(), PaneId::new());
    // The removed pane filled rows 20..24; the bottom half is closer.
    let removed_pane_rect = build_cell_rect(0, 20, 80, 4);
    let surviving_pane_rects = [
        (top_pane_id, build_cell_rect(0, 0, 80, 12)),
        (bottom_pane_id, build_cell_rect(0, 12, 80, 8)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(bottom_pane_id));
}

#[test]
fn biggest_absorber_wins_absorbed_space() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    // The left pane's new rectangle covers 14 of the removed columns; the
    // right pane covers 13.
    let removed_pane_rect = build_cell_rect(26, 0, 27, 24);
    let surviving_pane_rects = [
        (left_pane_id, build_cell_rect(0, 0, 40, 24)),
        (right_pane_id, build_cell_rect(40, 0, 40, 24)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.absorbed_space_pane_id, Some(left_pane_id));
}

#[test]
fn no_overlap_means_no_absorber() {
    let pane_id = PaneId::new();
    let removed_pane_rect = build_cell_rect(40, 0, 40, 24);
    let surviving_pane_rects = [(pane_id, build_cell_rect(0, 0, 40, 24))];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.absorbed_space_pane_id, None);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(pane_id));
}

#[test]
fn equal_absorption_keeps_the_earlier_pane() {
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    // Both survivors absorb exactly half of the removed rect.
    let removed_pane_rect = build_cell_rect(20, 0, 40, 24);
    let surviving_pane_rects = [
        (left_pane_id, build_cell_rect(0, 0, 40, 24)),
        (right_pane_id, build_cell_rect(40, 0, 40, 24)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.absorbed_space_pane_id, Some(left_pane_id));
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(left_pane_id));
}

#[test]
fn zero_area_panes_are_never_candidates() {
    let (visible_pane_id, hidden_pane_id) = (PaneId::new(), PaneId::new());
    let removed_pane_rect = build_cell_rect(0, 0, 40, 24);
    let surviving_pane_rects = [
        (hidden_pane_id, Rect::empty_at_origin()),
        (visible_pane_id, build_cell_rect(0, 0, 80, 24)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(visible_pane_id));
    assert_eq!(candidates.absorbed_space_pane_id, Some(visible_pane_id));
    assert_eq!(candidates.layout_order_pane_ids, [visible_pane_id]);
}

#[test]
fn collapsed_stack_members_are_never_candidates() {
    use crate::solver::StackHeader;

    let (visible_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    // The collapsed member's one-row header strip sits right on the removed
    // rect: nearest center, biggest per-cell overlap share. It must still
    // lose everywhere.
    let removed_pane_rect = build_cell_rect(0, 12, 80, 2);
    let surviving_pane_rects = [
        (collapsed_pane_id, build_cell_rect(0, 12, 80, 1)),
        (visible_pane_id, build_cell_rect(0, 13, 80, 11)),
    ];
    let headers = [StackHeader {
        pane_id: collapsed_pane_id,
        header_rect: build_cell_rect(0, 12, 80, 1),
        member_index: 0,
        member_count: 2,
    }];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &headers);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(visible_pane_id));
    assert_eq!(candidates.absorbed_space_pane_id, Some(visible_pane_id));
    assert_eq!(candidates.layout_order_pane_ids, [visible_pane_id]);
}

#[test]
fn layout_order_lists_visible_panes_in_input_order() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let survivors = [
        (first_pane_id, build_cell_rect(0, 0, 20, 24)),
        (second_pane_id, build_cell_rect(20, 0, 30, 24)),
        (third_pane_id, build_cell_rect(50, 0, 30, 24)),
    ];

    let candidates = compute_focus_candidates(build_cell_rect(0, 0, 10, 10), &survivors, &[]);
    assert_eq!(
        candidates.layout_order_pane_ids,
        [first_pane_id, second_pane_id, third_pane_id]
    );
}

#[test]
fn no_survivors_yields_empty_candidates() {
    let candidates = compute_focus_candidates(build_cell_rect(0, 0, 10, 10), &[], &[]);
    assert_eq!(candidates.spatial_neighbor_pane_id, None);
    assert_eq!(candidates.absorbed_space_pane_id, None);
    assert!(candidates.layout_order_pane_ids.is_empty());
}

#[test]
fn survivors_that_are_all_hidden_or_collapsed_yield_empty_candidates() {
    use crate::solver::StackHeader;

    let (hidden_pane_id, collapsed_pane_id) = (PaneId::new(), PaneId::new());
    let surviving_pane_rects = [
        (hidden_pane_id, Rect::empty_at_origin()),
        (collapsed_pane_id, build_cell_rect(0, 0, 80, 1)),
    ];
    let headers = [StackHeader {
        pane_id: collapsed_pane_id,
        header_rect: build_cell_rect(0, 0, 80, 1),
        member_index: 1,
        member_count: 2,
    }];

    let candidates = compute_focus_candidates(
        build_cell_rect(0, 0, 80, 24),
        &surviving_pane_rects,
        &headers,
    );
    assert_eq!(
        candidates,
        FocusCandidates {
            spatial_neighbor_pane_id: None,
            absorbed_space_pane_id: None,
            layout_order_pane_ids: Vec::new(),
        }
    );
}

#[test]
fn a_zero_area_removed_rect_ranks_neighbors_by_distance_to_its_origin() {
    let (far_pane_id, near_pane_id) = (PaneId::new(), PaneId::new());
    let surviving_pane_rects = [
        (far_pane_id, build_cell_rect(40, 0, 40, 24)),
        (near_pane_id, build_cell_rect(0, 0, 40, 24)),
    ];

    // The zero rect's center is (0, 0): `near` (center column 20) beats
    // `far` (center column 60). Nothing overlaps a zero-area rect.
    let candidates = compute_focus_candidates(Rect::empty_at_origin(), &surviving_pane_rects, &[]);
    assert_eq!(
        candidates,
        FocusCandidates {
            spatial_neighbor_pane_id: Some(near_pane_id),
            absorbed_space_pane_id: None,
            layout_order_pane_ids: vec![far_pane_id, near_pane_id],
        }
    );
}

#[test]
fn panes_at_the_coordinate_limit_rank_without_overflow() {
    let (far_pane_id, near_pane_id) = (PaneId::new(), PaneId::new());
    let removed_pane_rect = build_cell_rect(0, 0, 1, 1);
    let surviving_pane_rects = [
        (
            far_pane_id,
            build_cell_rect(u16::MAX, u16::MAX, u16::MAX, u16::MAX),
        ),
        (near_pane_id, build_cell_rect(1, 0, 1, 1)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(near_pane_id));
    assert_eq!(candidates.absorbed_space_pane_id, None);
    assert_eq!(
        candidates.layout_order_pane_ids,
        [far_pane_id, near_pane_id]
    );
}

fn list_collapsed_child_flags(stack: &SplitNode) -> Vec<bool> {
    (0..stack.children.len())
        .map(|child_index| stack.is_child_collapsed(child_index))
        .collect()
}

#[test]
fn activate_by_id_expands_the_target_and_collapses_the_prior() {
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack =
        SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id, third_pane_id], 0);

    let change = activate_stack_member(&mut stack, third_pane_id).unwrap();
    assert_eq!(change.newly_active_pane_id, third_pane_id);
    assert_eq!(change.deactivated_pane_id, Some(first_pane_id));
    assert_eq!(stack.active_child_index, 2);
    assert_eq!(list_collapsed_child_flags(&stack), [true, true, false]);
}

#[test]
fn activating_the_active_member_or_a_stranger_changes_nothing() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 0);
    let original_stack = stack.clone();

    assert_eq!(activate_stack_member(&mut stack, first_pane_id), None);
    assert_eq!(activate_stack_member(&mut stack, PaneId::new()), None);
    assert_eq!(stack, original_stack);
}

#[test]
fn directional_splits_refuse_stack_focus_ops() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut split_node = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            crate::tree::LayoutNode::Pane(first_pane_id),
            crate::tree::LayoutNode::Pane(second_pane_id),
        ],
    );
    assert_eq!(activate_stack_member(&mut split_node, second_pane_id), None);
}

#[test]
fn activating_a_pane_nested_in_a_split_member_expands_that_member() {
    use crate::size::SizeWeight;
    use crate::tree::LayoutNode;

    let (first_pane_id, second_pane_id, nested_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let nested_split_node = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(second_pane_id),
            LayoutNode::Pane(nested_pane_id),
        ],
    ));
    let mut stack = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(first_pane_id), nested_split_node],
        weights: vec![SizeWeight::default(); 2],
        active_child_index: 0,
    };

    // The nested pane sits inside the second member; the member expands and
    // reports its first leaf as the newly active pane.
    let change = activate_stack_member(&mut stack, nested_pane_id).unwrap();
    assert_eq!(
        change,
        StackFocusChange {
            newly_active_pane_id: second_pane_id,
            deactivated_pane_id: Some(first_pane_id),
        }
    );
    assert_eq!(stack.active_child_index, 1);
    assert_eq!(list_collapsed_child_flags(&stack), [true, false]);
}

#[test]
fn an_out_of_range_active_index_counts_as_the_last_member() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::from_stacked_pane_ids(vec![first_pane_id, second_pane_id], 0);
    stack.active_child_index = 7;

    // Index 7 clamps to the last member, so the second pane is already active.
    assert_eq!(activate_stack_member(&mut stack, second_pane_id), None);
    assert_eq!(stack.active_child_index, 7);

    let change = activate_stack_member(&mut stack, first_pane_id).unwrap();
    assert_eq!(
        change,
        StackFocusChange {
            newly_active_pane_id: first_pane_id,
            deactivated_pane_id: Some(second_pane_id),
        }
    );
    assert_eq!(stack.active_child_index, 0);
    assert_eq!(list_collapsed_child_flags(&stack), [false, true]);
}

#[test]
fn the_deepest_stack_holding_a_pane_is_found_for_activation() {
    use crate::tree::LayoutNode;

    let (first_pane_id, second_pane_id, nested_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![second_pane_id, nested_pane_id],
        0,
    ));
    let mut layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(first_pane_id), stack],
    ));

    let containing_stack = layout_tree
        .find_containing_stack_mut(nested_pane_id)
        .expect("the nested pane lives in a stack");
    let change = activate_stack_member(containing_stack, nested_pane_id).unwrap();
    assert_eq!(change.newly_active_pane_id, nested_pane_id);
    assert!(layout_tree
        .find_containing_stack_mut(first_pane_id)
        .is_none());
}

/// A three-member stack whose middle member is an empty split — a member
/// that holds no pane at all — with `active` naming the expanded one.
fn stack_with_an_empty_middle_member(
    first_pane_id: PaneId,
    last_pane_id: PaneId,
    active_child_index: usize,
) -> SplitNode {
    use crate::size::SizeWeight;
    use crate::tree::LayoutNode;

    let empty_split_node = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutNode::Pane(first_pane_id),
            empty_split_node,
            LayoutNode::Pane(last_pane_id),
        ],
        weights: vec![SizeWeight::default(); 3],
        active_child_index,
    }
}

#[test]
fn activating_away_from_a_member_with_no_pane_deactivates_nothing() {
    let (first_pane_id, last_pane_id) = (PaneId::new(), PaneId::new());
    let mut stack = stack_with_an_empty_middle_member(first_pane_id, last_pane_id, 1);

    let change = activate_stack_member(&mut stack, last_pane_id).unwrap();
    assert_eq!(change.newly_active_pane_id, last_pane_id);
    assert_eq!(change.deactivated_pane_id, None);
    assert_eq!(stack.active_child_index, 2);
    assert_eq!(list_collapsed_child_flags(&stack), [true, true, false]);
}

#[test]
fn an_odd_width_pane_keeps_its_half_cell_center_when_ranking_neighbors() {
    let (odd_pane_id, even_pane_id) = (PaneId::new(), PaneId::new());
    // The removed pane's center is column 10.0. `odd` spans columns 11..14,
    // center 12.5, distance 2.5; `even` spans 7..9, center 8.0, distance
    // 2.0, so `even` is nearer. Rounding both centers down to whole cells
    // would tie the two at distance 2 and hand the tie to `odd`.
    let removed_pane_rect = build_cell_rect(0, 0, 20, 2);
    let surviving_pane_rects = [
        (odd_pane_id, build_cell_rect(11, 0, 3, 2)),
        (even_pane_id, build_cell_rect(7, 0, 2, 2)),
    ];

    let candidates = compute_focus_candidates(removed_pane_rect, &surviving_pane_rects, &[]);
    assert_eq!(candidates.spatial_neighbor_pane_id, Some(even_pane_id));
}
