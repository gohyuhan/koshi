//! Tests for snapshot capture and restore.

use koshi_core::geometry::{Rect, Size};

use super::*;
use crate::size::SizeWeight;
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
        SplitDirection::Vertical,
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
    assert_eq!(snapshot.collapsed_states, [true, false, true]);
    assert_eq!(snapshot.restore(), SplitNode::stack(vec![a, b, c], 1));
}

#[test]
fn capturing_a_directional_split_yields_nothing() {
    let split = SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
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

    // Member 0 takes its stored flag; member 1 has none stored and keeps
    // the flag derived from the clamped active index.
    let restored = snapshot.restore();
    assert_eq!(restored, SplitNode::stack(members, 1));
}

#[test]
fn capture_of_an_all_empty_stack_yields_no_members() {
    // Hand-built: every member subtree is an empty split with no leaf pane.
    let empty = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));
    let stack = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutChild {
                node: empty.clone(),
                collapsed: false,
            },
            LayoutChild {
                node: empty,
                collapsed: true,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 0,
    };

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(
        snapshot,
        StackSnapshot {
            members: Vec::new(),
            active: 0,
            collapsed_states: Vec::new(),
        }
    );
    assert_eq!(snapshot.restore(), SplitNode::stack(Vec::new(), 0));
}

#[test]
fn capture_of_a_single_member_stack_round_trips() {
    let a = PaneId::new();
    let stack = SplitNode::stack(vec![a], 0);

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(
        snapshot,
        StackSnapshot {
            members: vec![a],
            active: 0,
            collapsed_states: vec![false],
        }
    );
    assert_eq!(snapshot.restore(), stack);
}

#[test]
fn capture_stands_in_the_last_member_when_the_active_member_is_dropped() {
    // Hand-built: the active member b is replaced by an empty split, so it
    // has no pane to record. The last surviving member, c, becomes active.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b, c], 1);
    stack.children[1].node = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        Vec::new(),
    ));

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(
        snapshot,
        StackSnapshot {
            members: vec![a, c],
            active: 1,
            collapsed_states: vec![true, true],
        }
    );
}

#[test]
fn capture_clamps_an_out_of_bounds_active_index_to_the_last_member() {
    // Hand-built: a deserialized stack can carry `active` past its children.
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut stack = SplitNode::stack(vec![a, b, c], 0);
    stack.active = 7;

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(
        snapshot,
        StackSnapshot {
            members: vec![a, b, c],
            active: 2,
            collapsed_states: vec![false, true, true],
        }
    );
}

#[test]
fn capture_represents_a_subtree_member_by_its_first_pane() {
    // Hand-built: a collapsed member that is a horizontal pair. The pair's
    // first pane, x, stands for the member; y is not recorded.
    let (x, y, z) = (PaneId::new(), PaneId::new(), PaneId::new());
    let pair = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(x)),
            LayoutChild::new(LayoutNode::Pane(y)),
        ],
    ));
    let stack = SplitNode {
        direction: SplitDirection::Stacked,
        children: vec![
            LayoutChild {
                node: pair,
                collapsed: true,
            },
            LayoutChild {
                node: LayoutNode::Pane(z),
                collapsed: false,
            },
        ],
        weights: vec![SizeWeight::default(), SizeWeight::default()],
        active: 1,
    };

    let snapshot = StackSnapshot::capture(&stack).unwrap();
    assert_eq!(
        snapshot,
        StackSnapshot {
            members: vec![x, z],
            active: 1,
            collapsed_states: vec![true, false],
        }
    );
    assert_eq!(snapshot.restore(), SplitNode::stack(vec![x, z], 1));
}

#[test]
fn restore_ignores_flags_past_the_last_member() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let snapshot = StackSnapshot {
        members: vec![a, b],
        active: 0,
        collapsed_states: vec![false, true, true, false],
    };

    assert_eq!(snapshot.restore(), SplitNode::stack(vec![a, b], 0));
}

#[test]
fn restore_applies_a_stored_flag_that_disagrees_with_the_active_index() {
    // The stored flags are applied as captured: member 0 is active and
    // flagged collapsed, member 1 is inactive and flagged expanded.
    let (a, b) = (PaneId::new(), PaneId::new());
    let snapshot = StackSnapshot {
        members: vec![a, b],
        active: 0,
        collapsed_states: vec![true, false],
    };

    let mut expected = SplitNode::stack(vec![a, b], 0);
    expected.children[0].collapsed = true;
    expected.children[1].collapsed = false;
    assert_eq!(snapshot.restore(), expected);
}

#[test]
fn snapshot_json_carries_members_active_and_collapsed_states() {
    let (a, b) = (PaneId::new(), PaneId::new());
    let snapshot = StackSnapshot::capture(&SplitNode::stack(vec![a, b], 1)).unwrap();

    let json = serde_json::to_value(&snapshot).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "members": [a, b],
            "active": 1,
            "collapsed_states": [true, false],
        })
    );
}

#[test]
fn a_stack_beside_a_pane_suppresses_as_a_unit_while_the_sibling_survives() {
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let stack = LayoutNode::Split(SplitNode::stack(vec![b, c], 0));
    let tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Vertical,
        vec![
            LayoutChild::new(LayoutNode::Pane(a)),
            LayoutChild::new(stack),
        ],
    ));

    // Three rows: the stack needs four (one header plus a bordered active),
    // a alone fits.
    let tab = Rect::at_origin(Size { cols: 80, rows: 3 });
    let result = solve(&tree, tab);
    assert_eq!(
        result.panes,
        [(a, tab), (b, Rect::zero()), (c, Rect::zero())]
    );
    assert_eq!(result.suppressed, [b, c]);
    // No headers are drawn for a suppressed stack.
    assert_eq!(result.stack_headers, Vec::new());
    assert!(!result.all_suppressed);
}
