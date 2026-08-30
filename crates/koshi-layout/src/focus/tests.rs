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

fn rect(x: u16, y: u16, cols: u16, rows: u16) -> Rect {
    Rect::new(Point { x, y }, Size { cols, rows })
}

#[test]
fn nearest_pane_by_center_is_the_spatial_neighbor() {
    let (a, c) = (PaneId::new(), PaneId::new());
    // The removed pane sat at columns 26..53; a (0..40) is nearer than
    // c (40..80) by center distance.
    let removed = rect(26, 0, 27, 24);
    let survivors = [(a, rect(0, 0, 40, 24)), (c, rect(40, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.spatial_neighbor, Some(a));
}

#[test]
fn vertical_neighbors_rank_by_distance_too() {
    let (top, bottom) = (PaneId::new(), PaneId::new());
    // The removed pane filled rows 20..24; the bottom half is closer.
    let removed = rect(0, 20, 80, 4);
    let survivors = [(top, rect(0, 0, 80, 12)), (bottom, rect(0, 12, 80, 8))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.spatial_neighbor, Some(bottom));
}

#[test]
fn biggest_absorber_wins_absorbed_space() {
    let (a, c) = (PaneId::new(), PaneId::new());
    // a's new rect covers 14 of the removed columns, c covers 13.
    let removed = rect(26, 0, 27, 24);
    let survivors = [(a, rect(0, 0, 40, 24)), (c, rect(40, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.absorbed_space, Some(a));
}

#[test]
fn no_overlap_means_no_absorber() {
    let a = PaneId::new();
    let removed = rect(40, 0, 40, 24);
    let survivors = [(a, rect(0, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.absorbed_space, None);
    assert_eq!(candidates.spatial_neighbor, Some(a));
}

#[test]
fn equal_absorption_keeps_the_earlier_pane() {
    let (a, b) = (PaneId::new(), PaneId::new());
    // Both survivors absorb exactly half of the removed rect.
    let removed = rect(20, 0, 40, 24);
    let survivors = [(a, rect(0, 0, 40, 24)), (b, rect(40, 0, 40, 24))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.absorbed_space, Some(a));
    assert_eq!(candidates.spatial_neighbor, Some(a));
}

#[test]
fn zero_area_panes_are_never_candidates() {
    let (visible, hidden) = (PaneId::new(), PaneId::new());
    let removed = rect(0, 0, 40, 24);
    let survivors = [(hidden, Rect::zero()), (visible, rect(0, 0, 80, 24))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.spatial_neighbor, Some(visible));
    assert_eq!(candidates.absorbed_space, Some(visible));
    assert_eq!(candidates.layout_order, [visible]);
}

#[test]
fn collapsed_stack_members_are_never_candidates() {
    use crate::solver::StackHeader;

    let (visible, collapsed) = (PaneId::new(), PaneId::new());
    // The collapsed member's one-row header strip sits right on the removed
    // rect: nearest center, biggest per-cell overlap share. It must still
    // lose everywhere.
    let removed = rect(0, 12, 80, 2);
    let survivors = [
        (collapsed, rect(0, 12, 80, 1)),
        (visible, rect(0, 13, 80, 11)),
    ];
    let headers = [StackHeader {
        pane: collapsed,
        rect: rect(0, 12, 80, 1),
        position: 0,
        total: 2,
    }];

    let candidates = focus_candidates(removed, &survivors, &headers);
    assert_eq!(candidates.spatial_neighbor, Some(visible));
    assert_eq!(candidates.absorbed_space, Some(visible));
    assert_eq!(candidates.layout_order, [visible]);
}

#[test]
fn layout_order_lists_visible_panes_in_input_order() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let survivors = [
        (a, rect(0, 0, 20, 24)),
        (b, rect(20, 0, 30, 24)),
        (c, rect(50, 0, 30, 24)),
    ];

    let candidates = focus_candidates(rect(0, 0, 10, 10), &survivors, &[]);
    assert_eq!(candidates.layout_order, [a, b, c]);
}

#[test]
fn no_survivors_yields_empty_candidates() {
    let candidates = focus_candidates(rect(0, 0, 10, 10), &[], &[]);
    assert_eq!(candidates.spatial_neighbor, None);
    assert_eq!(candidates.absorbed_space, None);
    assert!(candidates.layout_order.is_empty());
}

#[test]
fn survivors_that_are_all_hidden_or_collapsed_yield_empty_candidates() {
    use crate::solver::StackHeader;

    let (hidden, collapsed) = (PaneId::new(), PaneId::new());
    let survivors = [(hidden, Rect::zero()), (collapsed, rect(0, 0, 80, 1))];
    let headers = [StackHeader {
        pane: collapsed,
        rect: rect(0, 0, 80, 1),
        position: 1,
        total: 2,
    }];

    let candidates = focus_candidates(rect(0, 0, 80, 24), &survivors, &headers);
    assert_eq!(
        candidates,
        FocusCandidates {
            spatial_neighbor: None,
            absorbed_space: None,
            layout_order: Vec::new(),
        }
    );
}

#[test]
fn a_zero_area_removed_rect_ranks_neighbors_by_distance_to_its_origin() {
    let (far, near) = (PaneId::new(), PaneId::new());
    let survivors = [(far, rect(40, 0, 40, 24)), (near, rect(0, 0, 40, 24))];

    // The zero rect's center is (0, 0): `near` (center column 20) beats
    // `far` (center column 60). Nothing overlaps a zero-area rect.
    let candidates = focus_candidates(Rect::zero(), &survivors, &[]);
    assert_eq!(
        candidates,
        FocusCandidates {
            spatial_neighbor: Some(near),
            absorbed_space: None,
            layout_order: vec![far, near],
        }
    );
}

#[test]
fn panes_at_the_coordinate_limit_rank_without_overflow() {
    let (far, near) = (PaneId::new(), PaneId::new());
    let removed = rect(0, 0, 1, 1);
    let survivors = [
        (far, rect(u16::MAX, u16::MAX, u16::MAX, u16::MAX)),
        (near, rect(1, 0, 1, 1)),
    ];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.spatial_neighbor, Some(near));
    assert_eq!(candidates.absorbed_space, None);
    assert_eq!(candidates.layout_order, [far, near]);
}

fn collapsed_flags(stack: &SplitNode) -> Vec<bool> {
    (0..stack.children.len())
        .map(|index| stack.is_collapsed(index))
        .collect()
}

#[test]
fn activate_by_id_expands_the_target_and_collapses_the_prior() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b, c], 0);

    let change = stack_activate(&mut stack, c).unwrap();
    assert_eq!(change.newly_active, c);
    assert_eq!(change.deactivated, Some(a));
    assert_eq!(stack.active, 2);
    assert_eq!(collapsed_flags(&stack), [true, true, false]);
}

#[test]
fn activating_the_active_member_or_a_stranger_changes_nothing() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b], 0);
    let snapshot = stack.clone();

    assert_eq!(stack_activate(&mut stack, a), None);
    assert_eq!(stack_activate(&mut stack, PaneId::new()), None);
    assert_eq!(stack, snapshot);
}

#[test]
fn directional_splits_refuse_stack_focus_ops() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            crate::tree::LayoutNode::Pane(a),
            crate::tree::LayoutNode::Pane(b),
        ],
    );
    assert_eq!(stack_activate(&mut split, b), None);
}

#[test]
fn activating_a_pane_nested_in_a_split_member_expands_that_member() {
    use crate::size::SizeWeight;
    use crate::tree::LayoutNode;

    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let row = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(b), LayoutNode::Pane(c)],
    ));
    let mut stack = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(a), row],
        weights: vec![SizeWeight::default(); 2],
        active: 0,
    };

    // `c` sits inside the second member; the member expands and reports its
    // first leaf, `b`, as the newly active pane.
    let change = stack_activate(&mut stack, c).unwrap();
    assert_eq!(
        change,
        StackFocusChange {
            newly_active: b,
            deactivated: Some(a),
        }
    );
    assert_eq!(stack.active, 1);
    assert_eq!(collapsed_flags(&stack), [true, false]);
}

#[test]
fn an_out_of_range_active_index_counts_as_the_last_member() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b], 0);
    stack.active = 7;

    // Index 7 clamps to the last member, so `b` is already active.
    assert_eq!(stack_activate(&mut stack, b), None);
    assert_eq!(stack.active, 7);

    let change = stack_activate(&mut stack, a).unwrap();
    assert_eq!(
        change,
        StackFocusChange {
            newly_active: a,
            deactivated: Some(b),
        }
    );
    assert_eq!(stack.active, 0);
    assert_eq!(collapsed_flags(&stack), [false, true]);
}

#[test]
fn the_deepest_stack_holding_a_pane_is_found_for_activation() {
    use crate::tree::LayoutNode;

    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let mut tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(a), stack],
    ));

    let found = tree.stack_containing_mut(c).expect("c lives in a stack");
    let change = stack_activate(found, c).unwrap();
    assert_eq!(change.newly_active, c);
    assert!(tree.stack_containing_mut(a).is_none());
}

/// A three-member stack whose middle member is an empty split — a member
/// that holds no pane at all — with `active` naming the expanded one.
fn stack_with_an_empty_middle_member(first: PaneId, last: PaneId, active: usize) -> SplitNode {
    use crate::size::SizeWeight;
    use crate::tree::LayoutNode;

    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![LayoutNode::Pane(first), empty, LayoutNode::Pane(last)],
        weights: vec![SizeWeight::default(); 3],
        active,
    }
}

#[test]
fn activating_away_from_a_member_with_no_pane_deactivates_nothing() {
    let (a, c) = (PaneId::new(), PaneId::new());
    let mut stack = stack_with_an_empty_middle_member(a, c, 1);

    let change = stack_activate(&mut stack, c).unwrap();
    assert_eq!(change.newly_active, c);
    assert_eq!(change.deactivated, None);
    assert_eq!(stack.active, 2);
    assert_eq!(collapsed_flags(&stack), [true, true, false]);
}

#[test]
fn an_odd_width_pane_keeps_its_half_cell_center_when_ranking_neighbors() {
    let (odd, even) = (PaneId::new(), PaneId::new());
    // The removed pane's center is column 10.0. `odd` spans columns 11..14,
    // center 12.5, distance 2.5; `even` spans 7..9, center 8.0, distance
    // 2.0, so `even` is nearer. Rounding both centers down to whole cells
    // would tie the two at distance 2 and hand the tie to `odd`.
    let removed = rect(0, 0, 20, 2);
    let survivors = [(odd, rect(11, 0, 3, 2)), (even, rect(7, 0, 2, 2))];

    let candidates = focus_candidates(removed, &survivors, &[]);
    assert_eq!(candidates.spatial_neighbor, Some(even));
}
