//! Tests for snapshot capture and restore.

use tile_core::geometry::{Point, Rect, Size};

use super::*;
use crate::solver::solve;
use crate::tree::{LayoutChild, LayoutNode};

#[test]
fn snapshot_round_trip_preserves_membership_active_and_collapsed() {
    let members = vec![PaneId::new(), PaneId::new(), PaneId::new()];
    let stack = SplitNode::stack(members.clone(), 1);

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(snapshot.members, members);
    assert_eq!(snapshot.active, 1);
    assert_eq!(snapshot.collapsed_states, [true, false, true]);

    let restored = snapshot.restore();
    assert_eq!(restored, stack);
    assert_eq!(StackSnapshot::capture(&restored).unwrap(), snapshot);
}

#[test]
fn snapshot_survives_serde() {
    let stack = SplitNode::stack(vec![PaneId::new(), PaneId::new()], 0);
    let snapshot = StackSnapshot::capture(&stack).unwrap();

    let json = serde_json::to_string(&snapshot).expect("serialize");
    let back: StackSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, snapshot);
    assert_eq!(back.restore(), stack);
}

#[test]
fn capture_keeps_the_active_member_when_an_empty_member_is_dropped() {
    // Hand-built: a member with no pane is dropped from the snapshot,
    // and the active index is adjusted to follow its member through filtering.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        tile_core::geometry::SplitDirection::Vertical,
        Vec::new(),
    ));
    let mut stack = SplitNode::stack(vec![a, b, c], 1);
    stack.children.insert(
        0,
        LayoutChild {
            node: empty,
            collapsed: true,
        },
    );
    stack.active = 2; // still member b, now shifted one slot right

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(snapshot.members, [a, b, c]);
    assert_eq!(snapshot.active, 1);
    assert_eq!(snapshot.restore().active, 1);
}

#[test]
fn capturing_a_directional_split_yields_nothing() {
    let split = SplitNode::with_equal_weights(
        tile_core::geometry::SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(PaneId::new())),
            LayoutChild::new(LayoutNode::Pane(PaneId::new())),
        ],
    );
    assert_eq!(StackSnapshot::capture(&split), None);
}

#[test]
fn restore_clamps_a_stale_active_index_and_repairs_flags() {
    let members = vec![PaneId::new(), PaneId::new()];
    let snapshot = StackSnapshot {
        members: members.clone(),
        active: 9,
        collapsed_states: vec![true],
    };

    let restored = snapshot.restore();
    assert_eq!(restored.active, 1);
    assert_eq!(restored.children.len(), 2);
    // The stored flag wins for member 0; member 1 keeps the derived state.
    assert!(restored.children[0].collapsed);
    assert!(!restored.children[1].collapsed);
}

#[test]
fn a_stack_beside_a_pane_suppresses_as_a_unit_while_the_sibling_survives() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        tile_core::geometry::SplitDirection::Vertical,
        vec![
            LayoutChild::new(LayoutNode::Pane(a)),
            LayoutChild::new(stack),
        ],
    ));

    // Three rows: the stack needs four (one header plus a bordered active),
    // a alone fits.
    let tab = Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 3 });
    let result = solve(&tree, tab);
    assert_eq!(result.panes[0], (a, tab));
    assert_eq!(result.suppressed, [b, c]);
    // No headers are drawn for a suppressed stack.
    assert!(result.stack_headers.is_empty());
    assert!(!result.all_suppressed);
}
