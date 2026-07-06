//! Tests for focus candidate ranking and stack activation.
//!
//! **Focus candidates** are candidate panes to receive keyboard focus when the currently-focused
//! pane is closed. Tests verify that candidates are ranked by: spatial proximity (nearest center
//! distance), absorption of the removed pane's area, and layout order as a tiebreaker.
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

fn collapsed_flags(stack: &SplitNode) -> Vec<bool> {
    stack.children.iter().map(|child| child.collapsed).collect()
}

#[test]
fn focus_next_cycles_forward_and_wraps() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b, c], 0);

    let change = stack_focus_next(&mut stack).unwrap();
    assert_eq!(change.newly_active, b);
    assert_eq!(change.deactivated, Some(a));
    assert_eq!(stack.active, 1);
    assert_eq!(collapsed_flags(&stack), [true, false, true]);

    stack_focus_next(&mut stack).unwrap();
    let wrapped = stack_focus_next(&mut stack).unwrap();
    assert_eq!(wrapped.newly_active, a);
    assert_eq!(stack.active, 0);
}

#[test]
fn focus_prev_cycles_backward_and_wraps() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b, c], 0);

    let change = stack_focus_prev(&mut stack).unwrap();
    assert_eq!(change.newly_active, c);
    assert_eq!(change.deactivated, Some(a));
    assert_eq!(stack.active, 2);
    assert_eq!(collapsed_flags(&stack), [true, true, false]);
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
fn a_single_member_stack_cannot_cycle() {
    let a = PaneId::new();
    let mut stack = SplitNode::stack(vec![a], 0);
    assert_eq!(stack_focus_next(&mut stack), None);
    assert_eq!(stack_focus_prev(&mut stack), None);
}

#[test]
fn directional_splits_refuse_stack_focus_ops() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            crate::tree::LayoutChild::new(crate::tree::LayoutNode::Pane(a)),
            crate::tree::LayoutChild::new(crate::tree::LayoutNode::Pane(b)),
        ],
    );
    assert_eq!(stack_focus_next(&mut split), None);
    assert_eq!(stack_activate(&mut split, b), None);
}

#[test]
fn entering_a_stack_targets_its_active_member() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = SplitNode::stack(vec![a, b, c], 1);
    assert_eq!(stack_entry_target(&stack), Some(b));
}

#[test]
fn the_deepest_stack_holding_a_pane_is_found_for_activation() {
    use crate::tree::{LayoutChild, LayoutNode};

    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let mut tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(a)),
            LayoutChild::new(stack),
        ],
    ));

    let found = tree.stack_containing_mut(c).expect("c lives in a stack");
    let change = stack_activate(found, c).unwrap();
    assert_eq!(change.newly_active, c);
    assert!(tree.stack_containing_mut(a).is_none());
}
