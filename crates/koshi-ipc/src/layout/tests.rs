//! Tests for the layout answer's wire form: a populated layout survives a
//! round trip, its encoded shape is pinned field by field, the same bytes
//! decode back into the same values, and a field this build does not know is
//! ignored.

use koshi_core::geometry::{Point, SplitDirection};
use koshi_layout::size::SizeWeight;
use koshi_layout::tree::{LayoutChild, SplitNode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use super::*;

/// A fixed UUID ending in `tail`, so every id in one encoding stays
/// distinguishable.
fn uuid_ending(tail: u8) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-0000-0000-0000000000{tail:02}"))
        .expect("literal UUID parses")
}

/// The session in every fixture below.
fn session_id() -> SessionId {
    SessionId::from_uuid(uuid_ending(1))
}

/// The tab in every fixture below.
fn tab_id() -> TabId {
    TabId::from_uuid(uuid_ending(2))
}

/// The client viewing the tab in every fixture below.
fn client_id() -> ClientId {
    ClientId::from_uuid(uuid_ending(3))
}

/// The stack's active member.
fn active_pane() -> PaneId {
    PaneId::from_uuid(uuid_ending(4))
}

/// The stack's collapsed member, which owns the header strip.
fn collapsed_pane() -> PaneId {
    PaneId::from_uuid(uuid_ending(5))
}

/// Encode `message` and decode it back.
fn round_trip<T: Serialize + DeserializeOwned>(message: &T) -> T {
    let encoded = serde_json::to_string(message).expect("message encodes");
    serde_json::from_str(&encoded).expect("message decodes")
}

/// A layout with every field carrying a value: a stacked tab, one viewing
/// client zoomed on a pane, a suppressed pane, and a header strip.
fn populated_layout() -> SessionLayout {
    SessionLayout {
        id: session_id(),
        name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            id: tab_id(),
            name: "editor".to_string(),
            index: 1,
            tree: LayoutNode::Split(SplitNode {
                direction: SplitDirection::Stacked,
                children: vec![
                    LayoutChild {
                        node: LayoutNode::Pane(active_pane()),
                        collapsed: false,
                    },
                    LayoutChild {
                        node: LayoutNode::Pane(collapsed_pane()),
                        collapsed: true,
                    },
                ],
                weights: vec![SizeWeight::default(), SizeWeight::default()],
                active: 0,
            }),
            solved: vec![SolvedTab {
                client: client_id(),
                viewport: Size { cols: 80, rows: 22 },
                mode: LayoutMode::Fullscreen {
                    focused: active_pane(),
                },
                panes: vec![
                    SolvedPane {
                        id: active_pane(),
                        rect: Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 21 }),
                    },
                    SolvedPane {
                        id: collapsed_pane(),
                        rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
                    },
                ],
                suppressed: vec![collapsed_pane()],
                all_suppressed: true,
                stack_headers: vec![StackHeader {
                    pane: collapsed_pane(),
                    rect: Rect::new(Point { x: 0, y: 21 }, Size { cols: 80, rows: 1 }),
                    position: 1,
                    total: 2,
                }],
            }],
        }],
        clients: vec![ClientFocus {
            id: client_id(),
            active_tab: tab_id(),
            focused_pane: Some(active_pane()),
        }],
    }
}

/// The exact encoding of [`populated_layout`].
fn populated_layout_json() -> serde_json::Value {
    json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "name": "quiet-lake",
        "tabs": [{
            "id": "00000000-0000-0000-0000-000000000002",
            "name": "editor",
            "index": 1,
            "tree": {
                "Split": {
                    "direction": "Stacked",
                    "children": [
                        {
                            "node": { "Pane": "00000000-0000-0000-0000-000000000004" },
                            "collapsed": false
                        },
                        {
                            "node": { "Pane": "00000000-0000-0000-0000-000000000005" },
                            "collapsed": true
                        }
                    ],
                    "weights": [
                        {
                            "primary": { "Flex": 1 },
                            "min": null,
                            "preferred": null,
                            "resize_delta": 0
                        },
                        {
                            "primary": { "Flex": 1 },
                            "min": null,
                            "preferred": null,
                            "resize_delta": 0
                        }
                    ],
                    "active": 0
                }
            },
            "solved": [{
                "client": "00000000-0000-0000-0000-000000000003",
                "viewport": { "cols": 80, "rows": 22 },
                "mode": {
                    "Fullscreen": { "focused": "00000000-0000-0000-0000-000000000004" }
                },
                "panes": [
                    {
                        "id": "00000000-0000-0000-0000-000000000004",
                        "rect": {
                            "origin": { "x": 0, "y": 0 },
                            "size": { "cols": 80, "rows": 21 }
                        }
                    },
                    {
                        "id": "00000000-0000-0000-0000-000000000005",
                        "rect": {
                            "origin": { "x": 0, "y": 21 },
                            "size": { "cols": 80, "rows": 1 }
                        }
                    }
                ],
                "suppressed": ["00000000-0000-0000-0000-000000000005"],
                "all_suppressed": true,
                "stack_headers": [{
                    "pane": "00000000-0000-0000-0000-000000000005",
                    "rect": {
                        "origin": { "x": 0, "y": 21 },
                        "size": { "cols": 80, "rows": 1 }
                    },
                    "position": 1,
                    "total": 2
                }]
            }]
        }],
        "clients": [{
            "id": "00000000-0000-0000-0000-000000000003",
            "active_tab": "00000000-0000-0000-0000-000000000002",
            "focused_pane": "00000000-0000-0000-0000-000000000004"
        }]
    })
}

#[test]
fn a_populated_layout_survives_a_round_trip() {
    let layout = populated_layout();

    assert_eq!(round_trip(&layout), layout);
}

#[test]
fn a_layout_with_no_tabs_and_no_clients_survives_a_round_trip() {
    let layout = SessionLayout {
        id: session_id(),
        name: "quiet-lake".to_string(),
        tabs: Vec::new(),
        clients: Vec::new(),
    };

    assert_eq!(round_trip(&layout), layout);
}

#[test]
fn a_tab_no_client_views_survives_a_round_trip_with_an_empty_solve_list() {
    let layout = SessionLayout {
        id: session_id(),
        name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            id: tab_id(),
            name: "editor".to_string(),
            index: 0,
            tree: LayoutNode::Pane(active_pane()),
            solved: Vec::new(),
        }],
        clients: Vec::new(),
    };

    let decoded = round_trip(&layout);

    assert_eq!(decoded, layout);
    assert_eq!(decoded.tabs[0].solved, Vec::new());
}

#[test]
fn a_client_that_has_focused_nothing_survives_a_round_trip() {
    let layout = SessionLayout {
        id: session_id(),
        name: "quiet-lake".to_string(),
        tabs: Vec::new(),
        clients: vec![ClientFocus {
            id: client_id(),
            active_tab: tab_id(),
            focused_pane: None,
        }],
    };

    let decoded = round_trip(&layout);

    assert_eq!(decoded, layout);
    assert_eq!(decoded.clients[0].focused_pane, None);
}

#[test]
fn a_split_with_no_children_survives_a_round_trip() {
    let layout = SessionLayout {
        id: session_id(),
        name: "quiet-lake".to_string(),
        tabs: vec![TabLayout {
            id: tab_id(),
            name: "editor".to_string(),
            index: 0,
            tree: LayoutNode::Split(SplitNode {
                direction: SplitDirection::Horizontal,
                children: Vec::new(),
                weights: Vec::new(),
                active: 0,
            }),
            solved: Vec::new(),
        }],
        clients: Vec::new(),
    };

    assert_eq!(round_trip(&layout), layout);
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
        serde_json::to_value(populated_layout()).expect("layout encodes"),
        populated_layout_json(),
    );
}

#[test]
fn the_pinned_wire_shape_decodes_back_into_the_same_layout() {
    let decoded: SessionLayout =
        serde_json::from_value(populated_layout_json()).expect("the pinned shape decodes");

    assert_eq!(decoded, populated_layout());
}

#[test]
fn a_layout_carrying_an_unknown_field_ignores_it() {
    let decoded: SessionLayout = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","tabs":[],"clients":[],"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(decoded.name, "quiet-lake");
    assert!(decoded.tabs.is_empty());
    assert!(decoded.clients.is_empty());
}

#[test]
fn a_tab_carrying_an_unknown_field_ignores_it() {
    let decoded: TabLayout = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-000000000002","name":"editor","index":0,"tree":{"Pane":"00000000-0000-0000-0000-000000000004"},"solved":[],"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(decoded.name, "editor");
    assert_eq!(decoded.index, 0);
    assert!(decoded.solved.is_empty());
}

#[test]
fn a_solved_tab_carrying_an_unknown_field_ignores_it() {
    let decoded: SolvedTab = serde_json::from_str(
        r#"{"client":"00000000-0000-0000-0000-000000000003","viewport":{"cols":80,"rows":22},"mode":"Tiled","panes":[],"suppressed":[],"all_suppressed":false,"stack_headers":[],"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(decoded.viewport, Size { cols: 80, rows: 22 });
    assert_eq!(decoded.mode, LayoutMode::Tiled);
    assert!(!decoded.all_suppressed);
}

#[test]
fn a_solved_pane_carrying_an_unknown_field_ignores_it() {
    let decoded: SolvedPane = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-000000000004","rect":{"origin":{"x":0,"y":0},"size":{"cols":80,"rows":22}},"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(decoded.rect.size, Size { cols: 80, rows: 22 });
}

#[test]
fn a_client_focus_carrying_an_unknown_field_ignores_it() {
    let decoded: ClientFocus = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-000000000003","active_tab":"00000000-0000-0000-0000-000000000002","focused_pane":null,"junk":5}"#,
    )
    .expect("a field this build does not know is ignored");

    assert_eq!(decoded.focused_pane, None);
}

#[test]
fn a_layout_with_a_misspelled_field_name_is_refused() {
    let decoded: Result<SessionLayout, _> = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-000000000001","nmae":"quiet-lake","tabs":[],"clients":[]}"#,
    );

    assert!(
        decoded.is_err(),
        "a misspelled field decoded instead of failing: {decoded:?}"
    );
}
