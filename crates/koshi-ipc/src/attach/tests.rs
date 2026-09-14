//! Tests for the attach structure's wire form: it survives an encode/decode
//! round trip with every field intact, including a stack's active member, a
//! field this build does not know is ignored, and a field this build needs is
//! refused when absent.

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::{PaneId, PluginId, SessionId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};

use super::*;

/// A two-tab session: one stacked tab of three panes with the middle one
/// expanded, one single-pane tab, and a plugin pane alongside the terminals.
fn build_attached_session_structure_snapshot() -> AttachedSessionStructureSnapshot {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let logs_pane_id = PaneId::new();
    let plugin_id = PluginId::new();

    AttachedSessionStructureSnapshot {
        session_id: SessionId::new(),
        session_name: "koshi-dev".to_string(),
        tabs: vec![
            TabStructure {
                tab_id: TabId::new(),
                tab_name: "edit".to_string(),
                tab_index: 0,
                layout: LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![first_pane_id, second_pane_id, third_pane_id],
                    1,
                )),
                focus_mru: vec![second_pane_id, first_pane_id],
            },
            TabStructure {
                tab_id: TabId::new(),
                tab_name: "logs".to_string(),
                tab_index: 1,
                layout: LayoutNode::Pane(logs_pane_id),
                focus_mru: vec![logs_pane_id],
            },
        ],
        panes: vec![
            PaneStructure {
                pane_id: first_pane_id,
                pane_kind: PaneKind::Terminal,
            },
            PaneStructure {
                pane_id: second_pane_id,
                pane_kind: PaneKind::Terminal,
            },
            PaneStructure {
                pane_id: third_pane_id,
                pane_kind: PaneKind::Plugin { plugin_id },
            },
            PaneStructure {
                pane_id: logs_pane_id,
                pane_kind: PaneKind::Terminal,
            },
        ],
    }
}

#[test]
fn the_structure_survives_a_round_trip_field_for_field() {
    let expected_structure = build_attached_session_structure_snapshot();

    let encoded_json = serde_json::to_string(&expected_structure).expect("encodes");
    let decoded_structure: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded_json).expect("decodes");

    assert_eq!(decoded_structure, expected_structure);
}

#[test]
fn a_stacked_tab_arrives_with_its_collapsed_flags_and_active_index() {
    let expected_structure = build_attached_session_structure_snapshot();

    let encoded_json = serde_json::to_string(&expected_structure).expect("encodes");
    let decoded_structure: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded_json).expect("decodes");

    let LayoutNode::Split(stack_node) = &decoded_structure.tabs[0].layout else {
        panic!("the first tab's layout is a split");
    };
    assert_eq!(stack_node.direction, SplitDirection::Stacked);
    assert_eq!(stack_node.active_child_index, 1);
    assert_eq!(
        (0..stack_node.children.len())
            .map(|child_index| stack_node.is_child_collapsed(child_index))
            .collect::<Vec<bool>>(),
        vec![true, false, true]
    );
}

#[test]
fn a_tab_carrying_an_unknown_field_ignores_it() {
    // One snapshot, encoded JSON once: the builder mints fresh ids per call, so
    // the comparison is against this exact value.
    let expected_structure = build_attached_session_structure_snapshot();
    let mut encoded_json = serde_json::to_value(&expected_structure).expect("encodes");
    encoded_json["tabs"][0]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .insert("pinned".to_string(), serde_json::Value::Bool(true));

    let decoded_structure: AttachedSessionStructureSnapshot =
        serde_json::from_value(encoded_json).expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_structure, expected_structure,
        "the extra field left nothing behind in the decoded snapshot"
    );
}

#[test]
fn a_directional_split_arrives_with_its_direction_and_child_order() {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let expected_structure = AttachedSessionStructureSnapshot {
        session_id: SessionId::new(),
        session_name: "s".to_string(),
        tabs: vec![TabStructure {
            tab_id: TabId::new(),
            tab_name: "edit".to_string(),
            tab_index: 0,
            layout: LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![
                    LayoutNode::Pane(left_pane_id),
                    LayoutNode::Pane(right_pane_id),
                ],
            )),
            focus_mru: vec![left_pane_id],
        }],
        panes: vec![
            PaneStructure {
                pane_id: left_pane_id,
                pane_kind: PaneKind::Terminal,
            },
            PaneStructure {
                pane_id: right_pane_id,
                pane_kind: PaneKind::Terminal,
            },
        ],
    };

    let encoded_json = serde_json::to_string(&expected_structure).expect("encodes");
    let decoded_structure: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded_json).expect("decodes");

    let LayoutNode::Split(split_node) = &decoded_structure.tabs[0].layout else {
        panic!("the tab's layout is a split");
    };
    assert_eq!(split_node.direction, SplitDirection::Vertical);
    assert_eq!(
        split_node.children,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ]
    );
}

#[test]
fn a_session_with_no_tabs_and_no_panes_survives_a_round_trip() {
    let expected_structure = AttachedSessionStructureSnapshot {
        session_id: SessionId::new(),
        session_name: String::new(),
        tabs: Vec::new(),
        panes: Vec::new(),
    };

    let encoded_json = serde_json::to_string(&expected_structure).expect("encodes");
    let decoded_structure: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded_json).expect("decodes");

    assert_eq!(decoded_structure, expected_structure);
}

#[test]
fn a_tab_that_has_focused_nothing_yet_arrives_with_an_empty_focus_list() {
    let pane_id = PaneId::new();
    let expected_structure = AttachedSessionStructureSnapshot {
        session_id: SessionId::new(),
        session_name: "s".to_string(),
        tabs: vec![TabStructure {
            tab_id: TabId::new(),
            tab_name: "fresh".to_string(),
            tab_index: 0,
            layout: LayoutNode::Pane(pane_id),
            focus_mru: Vec::new(),
        }],
        panes: vec![PaneStructure {
            pane_id,
            pane_kind: PaneKind::Terminal,
        }],
    };

    let encoded_json = serde_json::to_string(&expected_structure).expect("encodes");
    let decoded_structure: AttachedSessionStructureSnapshot =
        serde_json::from_str(&encoded_json).expect("decodes");

    assert_eq!(decoded_structure.tabs[0].focus_mru, Vec::<PaneId>::new());
    assert_eq!(decoded_structure, expected_structure);
}

#[test]
fn a_tab_missing_its_focus_list_is_refused() {
    let mut encoded_json =
        serde_json::to_value(build_attached_session_structure_snapshot()).expect("encodes");
    encoded_json["tabs"][0]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("focus_mru");

    let decode_error = serde_json::from_value::<AttachedSessionStructureSnapshot>(encoded_json)
        .expect_err("a tab without its focus list decoded instead of failing");

    assert_eq!(decode_error.to_string(), "missing field `focus_mru`");
}

#[test]
fn a_pane_missing_its_kind_is_refused() {
    let mut encoded_json =
        serde_json::to_value(build_attached_session_structure_snapshot()).expect("encodes");
    encoded_json["panes"][0]
        .as_object_mut()
        .expect("a pane encodes as an object")
        .remove("kind");

    let decode_error = serde_json::from_value::<AttachedSessionStructureSnapshot>(encoded_json)
        .expect_err("a pane without its kind decoded instead of failing");

    assert_eq!(decode_error.to_string(), "missing field `kind`");
}
