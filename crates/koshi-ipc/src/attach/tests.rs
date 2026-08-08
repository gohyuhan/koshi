//! Tests for the attach structure's wire form: it survives an encode/decode
//! round trip with every field intact, including each stacked child's collapsed
//! flag, and a field this build does not know is ignored.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::{PaneId, PluginId, SessionId, TabId};
use koshi_layout::tree::{LayoutChild, SplitNode};

use super::*;

/// A two-tab session: one stacked tab of three panes with the middle one
/// expanded, one single-pane tab, and a plugin pane alongside the terminals.
fn structure() -> AttachedSessionStructureSnapshot {
    let first = PaneId::new();
    let second = PaneId::new();
    let third = PaneId::new();
    let logs = PaneId::new();
    let plugin_id = PluginId::new();

    AttachedSessionStructureSnapshot {
        id: SessionId::new(),
        name: "koshi-dev".to_string(),
        tabs: vec![
            TabStructure {
                id: TabId::new(),
                name: "edit".to_string(),
                index: 0,
                layout: LayoutNode::Split(SplitNode::stack(vec![first, second, third], 1)),
                focus_mru: vec![second, first],
            },
            TabStructure {
                id: TabId::new(),
                name: "logs".to_string(),
                index: 1,
                layout: LayoutNode::Pane(logs),
                focus_mru: vec![logs],
            },
        ],
        panes: vec![
            PaneStructure {
                id: first,
                kind: PaneKind::Terminal,
            },
            PaneStructure {
                id: second,
                kind: PaneKind::Terminal,
            },
            PaneStructure {
                id: third,
                kind: PaneKind::Plugin { plugin_id },
            },
            PaneStructure {
                id: logs,
                kind: PaneKind::Terminal,
            },
        ],
    }
}

#[test]
fn the_structure_survives_a_round_trip_field_for_field() {
    let sent = structure();

    let encoded = serde_json::to_string(&sent).expect("encodes");
    let received: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded).expect("decodes");

    assert_eq!(received, sent);
}

#[test]
fn a_stacked_tab_arrives_with_its_collapsed_flags_and_active_index() {
    let sent = structure();

    let encoded = serde_json::to_string(&sent).expect("encodes");
    let received: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded).expect("decodes");

    let LayoutNode::Split(stack) = &received.tabs[0].layout else {
        panic!("the first tab's layout is a split");
    };
    assert_eq!(stack.direction, SplitDirection::Stacked);
    assert_eq!(stack.active, 1);
    assert_eq!(
        stack
            .children
            .iter()
            .map(|child| child.collapsed)
            .collect::<Vec<bool>>(),
        vec![true, false, true]
    );
}

#[test]
fn a_tab_carrying_an_unknown_field_ignores_it() {
    // One snapshot, encoded once: `structure()` mints fresh ids per call, so
    // the comparison is against this exact value.
    let sent = structure();
    let mut encoded = serde_json::to_value(&sent).expect("encodes");
    encoded["tabs"][0]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .insert("pinned".to_string(), serde_json::Value::Bool(true));

    let decoded: AttachedSessionStructureSnapshot =
        serde_json::from_value(encoded).expect("a field this build does not know is ignored");

    assert_eq!(
        decoded, sent,
        "the extra field left nothing behind in the decoded snapshot"
    );
}

#[test]
fn a_directional_split_arrives_with_its_direction_and_child_order() {
    let left = PaneId::new();
    let right = PaneId::new();
    let sent = AttachedSessionStructureSnapshot {
        id: SessionId::new(),
        name: "s".to_string(),
        tabs: vec![TabStructure {
            id: TabId::new(),
            name: "edit".to_string(),
            index: 0,
            layout: LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![
                    LayoutChild::new(LayoutNode::Pane(left)),
                    LayoutChild::new(LayoutNode::Pane(right)),
                ],
            )),
            focus_mru: vec![left],
        }],
        panes: vec![
            PaneStructure {
                id: left,
                kind: PaneKind::Terminal,
            },
            PaneStructure {
                id: right,
                kind: PaneKind::Terminal,
            },
        ],
    };

    let encoded = serde_json::to_string(&sent).expect("encodes");
    let received: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded).expect("decodes");

    let LayoutNode::Split(split) = &received.tabs[0].layout else {
        panic!("the tab's layout is a split");
    };
    assert_eq!(split.direction, SplitDirection::Vertical);
    assert_eq!(
        split
            .children
            .iter()
            .map(|child| child.node.clone())
            .collect::<Vec<LayoutNode>>(),
        vec![LayoutNode::Pane(left), LayoutNode::Pane(right)]
    );
}
