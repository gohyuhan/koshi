//! Tests for the layout answer's wire form: a populated layout survives a
//! round trip, its encoded JSON shape is pinned field by field, the same bytes
//! decode back into the same values, a field this build does not know is
//! ignored, and a missing or malformed field is refused with its exact error.

use koshi_core::geometry::{Point, SplitDirection};
use koshi_layout::size::SizeWeight;
use koshi_layout::tree::{LayoutNode, SplitNode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use super::*;

/// A fixed UUID ending in `suffix_byte`, so every id in one encoding stays
/// distinguishable.
fn build_test_uuid_with_suffix(suffix_byte: u8) -> Uuid {
    Uuid::parse_str(&format!(
        "00000000-0000-0000-0000-0000000000{suffix_byte:02}"
    ))
    .expect("literal UUID parses")
}

/// The session in every fixture below.
fn build_test_session_id() -> SessionId {
    SessionId::from_uuid(build_test_uuid_with_suffix(1))
}

/// The tab in every fixture below.
fn build_test_tab_id() -> TabId {
    TabId::from_uuid(build_test_uuid_with_suffix(2))
}

/// The client viewing the tab in every fixture below.
fn build_test_client_id() -> ClientId {
    ClientId::from_uuid(build_test_uuid_with_suffix(3))
}

/// The stack's active member.
fn build_test_active_pane_id() -> PaneId {
    PaneId::from_uuid(build_test_uuid_with_suffix(4))
}

/// The stack's collapsed member, which owns the header strip.
fn build_test_collapsed_pane_id() -> PaneId {
    PaneId::from_uuid(build_test_uuid_with_suffix(5))
}

/// Encode `message` and decode it back.
fn round_trip_wire_message<WireMessage: Serialize + DeserializeOwned>(
    wire_message: &WireMessage,
) -> WireMessage {
    let encoded_json = serde_json::to_string(wire_message).expect("wire message encodes");
    serde_json::from_str(&encoded_json).expect("wire message decodes")
}

/// A layout with every field carrying a value: a stacked tab, one viewing
/// client zoomed on a pane, a suppressed pane, and a header strip.
fn build_populated_session_layout() -> SessionLayout {
    SessionLayout {
        session_id: build_test_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            tab_id: build_test_tab_id(),
            tab_name: "editor".to_string(),
            tab_index: 1,
            layout_tree: LayoutNode::Split(SplitNode {
                direction: SplitDirection::Stacked,
                children: vec![
                    LayoutNode::Pane(build_test_active_pane_id()),
                    LayoutNode::Pane(build_test_collapsed_pane_id()),
                ],
                weights: vec![SizeWeight::default(), SizeWeight::default()],
                active_child_index: 0,
            }),
            solved_tabs: vec![SolvedTab {
                client_id: build_test_client_id(),
                viewport_size: Size {
                    column_count: 80,
                    row_count: 22,
                },
                layout_mode: LayoutMode::Fullscreen {
                    focused_pane_id: build_test_active_pane_id(),
                },
                pane_rects: vec![
                    SolvedPane {
                        pane_id: build_test_active_pane_id(),
                        outer_rect: Rect::from_size_at_origin(Size {
                            column_count: 80,
                            row_count: 21,
                        }),
                    },
                    SolvedPane {
                        pane_id: build_test_collapsed_pane_id(),
                        outer_rect: Rect::from_origin_and_size(
                            Point { column: 0, row: 21 },
                            Size {
                                column_count: 80,
                                row_count: 1,
                            },
                        ),
                    },
                ],
                suppressed_pane_ids: vec![build_test_collapsed_pane_id()],
                is_every_pane_suppressed: true,
                stack_headers: vec![StackHeader {
                    pane_id: build_test_collapsed_pane_id(),
                    header_rect: Rect::from_origin_and_size(
                        Point { column: 0, row: 21 },
                        Size {
                            column_count: 80,
                            row_count: 1,
                        },
                    ),
                    member_index: 1,
                    member_count: 2,
                }],
            }],
        }],
        clients: vec![ClientFocus {
            client_id: build_test_client_id(),
            active_tab_id: build_test_tab_id(),
            focused_pane_id: Some(build_test_active_pane_id()),
        }],
    }
}

/// The exact encoding of [`build_populated_session_layout`].
fn build_populated_layout_json() -> serde_json::Value {
    json!({
        "session_id": "00000000-0000-0000-0000-000000000001",
        "session_name": "quiet-lake",
        "tabs": [{
            "tab_id": "00000000-0000-0000-0000-000000000002",
            "tab_name": "editor",
            "tab_index": 1,
            "layout_tree": {
                "Split": {
                    "direction": "Stacked",
                    "children": [
                        { "Pane": "00000000-0000-0000-0000-000000000004" },
                        { "Pane": "00000000-0000-0000-0000-000000000005" }
                    ],
                    "weights": [
                        {
                            "primary_constraint": { "Flex": 1 },
                            "minimum_cell_count": null,
                            "preferred_cell_count": null,
                            "resize_delta": 0
                        },
                        {
                            "primary_constraint": { "Flex": 1 },
                            "minimum_cell_count": null,
                            "preferred_cell_count": null,
                            "resize_delta": 0
                        }
                    ],
                    "active_child_index": 0
                }
            },
            "solved_tabs": [{
                "client_id": "00000000-0000-0000-0000-000000000003",
                "viewport_size": { "column_count": 80, "row_count": 22 },
                "layout_mode": {
                    "Fullscreen": { "focused_pane_id": "00000000-0000-0000-0000-000000000004" }
                },
                "pane_rects": [
                    {
                        "pane_id": "00000000-0000-0000-0000-000000000004",
                        "outer_rect": {
                            "origin": { "column": 0, "row": 0 },
                            "cell_size": { "column_count": 80, "row_count": 21 }
                        }
                    },
                    {
                        "pane_id": "00000000-0000-0000-0000-000000000005",
                        "outer_rect": {
                            "origin": { "column": 0, "row": 21 },
                            "cell_size": { "column_count": 80, "row_count": 1 }
                        }
                    }
                ],
                "suppressed_pane_ids": ["00000000-0000-0000-0000-000000000005"],
                "is_every_pane_suppressed": true,
                "stack_headers": [{
                    "pane_id": "00000000-0000-0000-0000-000000000005",
                    "header_rect": {
                        "origin": { "column": 0, "row": 21 },
                        "cell_size": { "column_count": 80, "row_count": 1 }
                    },
                    "member_index": 1,
                    "member_count": 2
                }]
            }]
        }],
        "clients": [{
            "client_id": "00000000-0000-0000-0000-000000000003",
            "active_tab_id": "00000000-0000-0000-0000-000000000002",
            "focused_pane_id": "00000000-0000-0000-0000-000000000004"
        }]
    })
}

#[test]
fn a_populated_layout_survives_a_round_trip_wire_message() {
    let layout = build_populated_session_layout();

    assert_eq!(round_trip_wire_message(&layout), layout);
}

#[test]
fn a_layout_with_no_tabs_and_no_clients_survives_a_round_trip_wire_message() {
    let layout = SessionLayout {
        session_id: build_test_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    };

    assert_eq!(round_trip_wire_message(&layout), layout);
}

#[test]
fn a_tab_no_client_views_survives_a_round_trip_with_an_empty_solve_list() {
    let layout = SessionLayout {
        session_id: build_test_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            tab_id: build_test_tab_id(),
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Pane(build_test_active_pane_id()),
            solved_tabs: Vec::new(),
        }],
        clients: Vec::new(),
    };

    let decoded_tab_layout = round_trip_wire_message(&layout);

    assert_eq!(decoded_tab_layout, layout);
    assert_eq!(decoded_tab_layout.tabs[0].solved_tabs, Vec::new());
}

#[test]
fn a_client_that_has_focused_nothing_survives_a_round_trip_wire_message() {
    let layout = SessionLayout {
        session_id: build_test_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: Vec::new(),
        clients: vec![ClientFocus {
            client_id: build_test_client_id(),
            active_tab_id: build_test_tab_id(),
            focused_pane_id: None,
        }],
    };

    let decoded_client_focus = round_trip_wire_message(&layout);

    assert_eq!(decoded_client_focus, layout);
    assert_eq!(decoded_client_focus.clients[0].focused_pane_id, None);
}

#[test]
fn a_split_with_no_children_survives_a_round_trip_wire_message() {
    let layout = SessionLayout {
        session_id: build_test_session_id(),
        session_name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            tab_id: build_test_tab_id(),
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Split(SplitNode {
                direction: SplitDirection::Horizontal,
                children: Vec::new(),
                weights: Vec::new(),
                active_child_index: 0,
            }),
            solved_tabs: Vec::new(),
        }],
        clients: Vec::new(),
    };

    assert_eq!(round_trip_wire_message(&layout), layout);
}

#[test]
fn the_layout_wire_shape_belongs_to_this_protocol_version() {
    // Every field of every struct a `Layout` answer carries, pinned. Two
    // builds only understand each other's bytes when they agree on this
    // shape, and the version in the Hello is the only thing that catches a
    // pair that does not. So a change here is a change to the wire: add,
    // remove, or rename anything below and `PROTOCOL_VERSION` goes up in the
    // same commit.
    //
    // Round-trip tests cannot catch this: one build encoding and decoding its
    // own structs always agrees with itself.
    assert_eq!(
        serde_json::to_value(build_populated_session_layout()).expect("layout encodes"),
        build_populated_layout_json(),
    );
}

#[test]
fn the_pinned_wire_shape_decodes_back_into_the_same_layout() {
    let decoded_session_layout: SessionLayout =
        serde_json::from_value(build_populated_layout_json()).expect("the pinned shape decodes");

    assert_eq!(decoded_session_layout, build_populated_session_layout());
}

#[test]
fn a_layout_carrying_an_unknown_field_ignores_it() {
    let decoded_session_layout: SessionLayout = serde_json::from_str(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","tabs":[],"clients":[],"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_session_layout,
        SessionLayout {
            session_id: build_test_session_id(),
            session_name: "quiet-lake".to_string(),
            tabs: Vec::new(),
            clients: Vec::new(),
        }
    );
}

#[test]
fn a_tab_carrying_an_unknown_field_ignores_it() {
    let decoded_tab_layout: TabLayout = serde_json::from_str(
        r#"{"tab_id":"00000000-0000-0000-0000-000000000002","tab_name":"editor","tab_index":0,"layout_tree":{"Pane":"00000000-0000-0000-0000-000000000004"},"solved_tabs":[],"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_tab_layout,
        TabLayout {
            tab_id: build_test_tab_id(),
            tab_name: "editor".to_string(),
            tab_index: 0,
            layout_tree: LayoutNode::Pane(build_test_active_pane_id()),
            solved_tabs: Vec::new(),
        }
    );
}

#[test]
fn a_solved_tab_carrying_an_unknown_field_ignores_it() {
    let decoded_solved_tab: SolvedTab = serde_json::from_str(
        r#"{"client_id":"00000000-0000-0000-0000-000000000003","viewport_size":{"column_count":80,"row_count":22},"layout_mode":"Tiled","pane_rects":[],"suppressed_pane_ids":[],"is_every_pane_suppressed":false,"stack_headers":[],"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_solved_tab,
        SolvedTab {
            client_id: build_test_client_id(),
            viewport_size: Size {
                column_count: 80,
                row_count: 22
            },
            layout_mode: LayoutMode::Tiled,
            pane_rects: Vec::new(),
            suppressed_pane_ids: Vec::new(),
            is_every_pane_suppressed: false,
            stack_headers: Vec::new(),
        }
    );
}

#[test]
fn a_solved_pane_carrying_an_unknown_field_ignores_it() {
    let decoded_solved_pane: SolvedPane = serde_json::from_str(
        r#"{"pane_id":"00000000-0000-0000-0000-000000000004","outer_rect":{"origin":{"column":0,"row":0},"cell_size":{"column_count":80,"row_count":22}},"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_solved_pane,
        SolvedPane {
            pane_id: build_test_active_pane_id(),
            outer_rect: Rect::from_size_at_origin(Size {
                column_count: 80,
                row_count: 22
            }),
        }
    );
}

#[test]
fn a_client_focus_carrying_an_unknown_field_ignores_it() {
    let decoded_client_focus: ClientFocus = serde_json::from_str(
        r#"{"client_id":"00000000-0000-0000-0000-000000000003","active_tab_id":"00000000-0000-0000-0000-000000000002","focused_pane_id":null,"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_client_focus,
        ClientFocus {
            client_id: build_test_client_id(),
            active_tab_id: build_test_tab_id(),
            focused_pane_id: None,
        }
    );
}

#[test]
fn a_client_focus_with_no_focused_pane_key_reads_as_focusing_nothing() {
    let decoded_client_focus: ClientFocus = serde_json::from_str(
        r#"{"client_id":"00000000-0000-0000-0000-000000000003","active_tab_id":"00000000-0000-0000-0000-000000000002"}"#,
    )
    .expect("a missing `focused_pane` reads as `None`");

    assert_eq!(
        decoded_client_focus,
        ClientFocus {
            client_id: build_test_client_id(),
            active_tab_id: build_test_tab_id(),
            focused_pane_id: None,
        }
    );
}

#[test]
fn a_layout_with_a_misspelled_field_name_is_refused() {
    let decoded_layout_result: Result<SessionLayout, _> = serde_json::from_str(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","nmae":"quiet-lake","tabs":[],"clients":[]}"#,
    );

    assert_eq!(
        decoded_layout_result
            .expect_err("a misspelled field is refused")
            .to_string(),
        "missing field `session_name` at line 1 column 96"
    );
}

#[test]
fn a_tab_whose_index_is_below_zero_is_refused() {
    let decoded_tab_result: Result<TabLayout, _> = serde_json::from_str(
        r#"{"tab_id":"00000000-0000-0000-0000-000000000002","tab_name":"editor","tab_index":-1,"layout_tree":{"Pane":"00000000-0000-0000-0000-000000000004"},"solved_tabs":[]}"#,
    );

    assert_eq!(
        decoded_tab_result
            .expect_err("a negative index is refused")
            .to_string(),
        "invalid value: integer `-1`, expected usize at line 1 column 83"
    );
}

#[test]
fn a_solve_whose_mode_this_build_does_not_have_is_refused() {
    let decoded_solved_tab_result: Result<SolvedTab, _> = serde_json::from_str(
        r#"{"client_id":"00000000-0000-0000-0000-000000000003","viewport_size":{"column_count":80,"row_count":22},"layout_mode":"Floating","pane_rects":[],"suppressed_pane_ids":[],"is_every_pane_suppressed":false,"stack_headers":[]}"#,
    );

    assert_eq!(
        decoded_solved_tab_result
            .expect_err("a mode this build does not have is refused")
            .to_string(),
        "unknown variant `Floating`, expected `Tiled` or `Fullscreen` at line 1 column 127"
    );
}

#[test]
fn a_layout_missing_its_clients_is_refused() {
    let decoded_layout_result: Result<SessionLayout, _> = serde_json::from_str(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","tabs":[]}"#,
    );

    assert_eq!(
        decoded_layout_result
            .expect_err("a missing field is refused")
            .to_string(),
        "missing field `clients` at line 1 column 91"
    );
}
