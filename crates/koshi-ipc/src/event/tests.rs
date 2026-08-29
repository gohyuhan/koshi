//! Tests for the client event stream's wire form: every frame survives an
//! encode/decode round trip with every field intact, the encodings are the ones
//! this protocol version pins, a field this build does not know is ignored,
//! and a frame this build has no name for reads as unknown.

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::SessionId;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseTracking};
use koshi_layout::mode::LayoutMode;
use koshi_pane::pane::state::PaneKind;
use serde_json::json;

use crate::frame::{
    FrameClient, FrameCursor, FrameCursorShape, FramePane, FrameScrollback, FrameSession,
    FrameSlot, FrameTab, FrameTabMeta,
};

use super::*;

/// The one UUID every id below is built from, so an encoding is byte-stable.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A one-pane frame at fixed ids, so its encoding is byte-stable. The pane
/// shows no terminal content this frame, so its `window` is `None`; the frame's
/// own wire shape is pinned in the frame module's tests.
fn painted_frame() -> PaintedFrame {
    let tab_id = TabId::from_uuid(fixed_uuid());
    let pane_id = PaneId::from_uuid(fixed_uuid());

    PaintedFrame {
        session: FrameSession {
            id: SessionId::from_uuid(fixed_uuid()),
            name: "quiet-lake".to_string(),
            active_tab: FrameTab {
                id: tab_id,
                name: "edit".to_string(),
                slots: vec![FrameSlot {
                    pane_id,
                    rect: Rect {
                        origin: Point { x: 0, y: 0 },
                        size: Size { cols: 4, rows: 3 },
                    },
                    inner_rect: Some(Rect {
                        origin: Point { x: 1, y: 1 },
                        size: Size { cols: 2, rows: 1 },
                    }),
                    kind: PaneKind::Terminal,
                    visible: true,
                    suppressed: false,
                    dead: false,
                }],
                effective_size: Size { cols: 4, rows: 3 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
                gap: 0,
            },
            tabs: vec![FrameTabMeta {
                id: tab_id,
                name: "edit".to_string(),
                index: 0,
                active: true,
            }],
        },
        panes: vec![FramePane {
            id: pane_id,
            title: Some("vim".to_string()),
            cursor: FrameCursor {
                row: 0,
                col: 1,
                visible: true,
                blink: false,
                shape: Some(FrameCursorShape::Bar),
            },
            window: None,
            reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            alt_scroll: false,
            on_alt_screen: false,
            view_top_row: 7,
            selection: None,
            has_selection: false,
            scrollback: FrameScrollback {
                truncated: false,
                retained_lines: 12,
            },
        }],
        client: FrameClient {
            id: ClientId::from_uuid(fixed_uuid()),
            viewport: Size { cols: 4, rows: 3 },
            active_tab: tab_id,
            focused_pane: Some(pane_id),
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
    }
}

/// Every structure frame the stream can carry, at fixed ids, in the order the
/// enum declares them. `Painted` and `MouseAnswer` are left out: their payloads
/// have their own tests below.
fn every_event() -> Vec<SessionEvent> {
    let client_id = ClientId::from_uuid(fixed_uuid());
    let pane_id = PaneId::from_uuid(fixed_uuid());
    let tab_id = TabId::from_uuid(fixed_uuid());

    vec![
        SessionEvent::PaneCreated { pane_id, tab_id },
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: Some(130),
        },
        SessionEvent::PaneClosing { pane_id },
        SessionEvent::PaneRemoved { pane_id, tab_id },
        SessionEvent::PaneFocused {
            client_id,
            tab_id,
            pane_id,
            prior_pane: Some(pane_id),
        },
        SessionEvent::LayoutChanged { tab_id },
        SessionEvent::TabCreated { tab_id },
        SessionEvent::TabClosed { tab_id },
        SessionEvent::TabFocused {
            client_id,
            tab_id,
            prior_tab: tab_id,
        },
        SessionEvent::TabMoved {
            tab_id,
            old_index: 2,
            new_index: 0,
        },
        SessionEvent::Quit,
        SessionEvent::Restarting,
        SessionEvent::Detached,
        SessionEvent::Resync { dropped_count: 4 },
        SessionEvent::SwitchTo {
            session_id: SessionId::from_uuid(fixed_uuid()),
        },
    ]
}

#[test]
fn every_event_survives_a_round_trip_field_for_field() {
    for sent in every_event() {
        let encoded = serde_json::to_string(&sent).expect("event encodes");
        let received: SessionEvent = serde_json::from_str(&encoded).expect("event decodes");

        assert_eq!(received, sent);
    }
}

#[test]
fn a_painted_frame_survives_a_round_trip_field_for_field() {
    let sent = SessionEvent::Painted {
        frame: Box::new(painted_frame()),
    };

    let encoded = serde_json::to_string(&sent).expect("event encodes");
    let received: SessionEvent = serde_json::from_str(&encoded).expect("event decodes");

    assert_eq!(received, sent);
}

#[test]
fn a_mouse_answer_survives_a_round_trip_field_for_field() {
    let pane = PaneId::from_uuid(fixed_uuid());
    let other_pane = PaneId::new();
    let sent = [
        // The normal case: the round ran and had nothing to report.
        SessionEvent::MouseAnswer {
            request_id: 7,
            answers: Vec::new(),
        },
        SessionEvent::MouseAnswer {
            request_id: 8,
            answers: vec![MouseAnswer::Scrolled { pane, top: None }],
        },
        SessionEvent::MouseAnswer {
            request_id: 9,
            answers: vec![MouseAnswer::Scrolled {
                pane,
                top: Some(938),
            }],
        },
        SessionEvent::MouseAnswer {
            request_id: 10,
            answers: vec![MouseAnswer::Resized {
                pane,
                side: Direction::Up,
                step: -1,
                applied: 0,
            }],
        },
        SessionEvent::MouseAnswer {
            request_id: 11,
            answers: vec![
                MouseAnswer::Scrolled {
                    pane,
                    top: Some(938),
                },
                MouseAnswer::Resized {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    applied: 0,
                },
            ],
        },
        // Two border moves in one round: each entry names its own border and
        // its own direction, so the pair stays told apart across the wire.
        SessionEvent::MouseAnswer {
            request_id: 12,
            answers: vec![
                MouseAnswer::Resized {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    applied: 8,
                },
                MouseAnswer::Resized {
                    pane: other_pane,
                    side: Direction::Left,
                    step: 1,
                    applied: 1,
                },
            ],
        },
    ];

    for event in sent {
        let encoded = serde_json::to_string(&event).expect("event encodes");
        let received: SessionEvent = serde_json::from_str(&encoded).expect("event decodes");

        assert_eq!(received, event);
    }
}

#[test]
fn a_host_write_survives_a_round_trip() {
    // An OSC 52 copy of "hello": a byte over 127 and a control byte, so a
    // spelling that mangled either shows up here.
    let sent = SessionEvent::HostWrite {
        bytes: b"\x1b]52;c;aGVsbG8=\x07\xc3\xa9".to_vec(),
    };

    let encoded = serde_json::to_string(&sent).expect("event encodes");
    let received: SessionEvent = serde_json::from_str(&encoded).expect("event decodes");

    assert_eq!(
        received,
        SessionEvent::HostWrite {
            bytes: vec![
                0x1b, b']', b'5', b'2', b';', b'c', b';', b'a', b'G', b'V', b's', b'b', b'G', b'8',
                b'=', 0x07, 0xc3, 0xa9,
            ],
        }
    );
    assert_eq!(received, sent);
}

#[test]
fn a_painted_frame_carrying_an_unknown_field_ignores_it() {
    let mut encoded = serde_json::to_value(SessionEvent::Painted {
        frame: Box::new(painted_frame()),
    })
    .expect("event encodes");
    encoded["Painted"]["frame"]["panes"][0]
        .as_object_mut()
        .expect("a pane encodes as an object")
        .insert("zoomed".to_string(), serde_json::Value::Bool(true));

    // Decoded from text, the way the transport does it.
    let decoded: SessionEvent = serde_json::from_str(&encoded.to_string())
        .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        SessionEvent::Painted {
            frame: Box::new(painted_frame()),
        },
        "the extra field left nothing behind in the decoded event"
    );
}

#[test]
fn an_absent_optional_field_round_trips_as_absent() {
    let pane_id = PaneId::from_uuid(fixed_uuid());
    let sent = [
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: None,
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::from_uuid(fixed_uuid()),
            tab_id: TabId::from_uuid(fixed_uuid()),
            pane_id,
            prior_pane: None,
        },
    ];

    for event in sent {
        let encoded = serde_json::to_string(&event).expect("event encodes");
        let received: SessionEvent = serde_json::from_str(&encoded).expect("event decodes");

        assert_eq!(received, event);
    }
}

#[test]
fn the_event_wire_shape_belongs_to_this_protocol_version() {
    // Every structure frame an attached client reads, pinned. A client at the
    // old shape passes the handshake, attaches, and then fails to decode the
    // stream, which reads to the user as a session that stops updating.
    //
    // So a change here — add, remove, rename, or retype anything below — turns
    // this red. Renaming or retyping a field also moves `PROTOCOL_VERSION` in
    // the same commit; adding a whole frame, which an older client skips as
    // unknown and keeps reading past, does not.
    //
    // Shape as of protocol version 2. Round-trip tests cannot catch this: one
    // build encoding and decoding its own structs always agrees with itself.
    let id = "00000000-0000-0000-0000-000000000001";

    assert_eq!(
        every_event()
            .iter()
            .map(|event| serde_json::to_value(event).expect("event encodes"))
            .collect::<Vec<serde_json::Value>>(),
        vec![
            json!({ "PaneCreated": { "pane_id": id, "tab_id": id } }),
            json!({ "PaneProcessExited": { "pane_id": id, "exit_code": 130 } }),
            json!({ "PaneClosing": { "pane_id": id } }),
            json!({ "PaneRemoved": { "pane_id": id, "tab_id": id } }),
            json!({ "PaneFocused": {
                "client_id": id,
                "tab_id": id,
                "pane_id": id,
                "prior_pane": id
            } }),
            json!({ "LayoutChanged": { "tab_id": id } }),
            json!({ "TabCreated": { "tab_id": id } }),
            json!({ "TabClosed": { "tab_id": id } }),
            json!({ "TabFocused": { "client_id": id, "tab_id": id, "prior_tab": id } }),
            json!({ "TabMoved": { "tab_id": id, "old_index": 2, "new_index": 0 } }),
            json!("Quit"),
            json!("Restarting"),
            json!("Detached"),
            json!({ "Resync": { "dropped_count": 4 } }),
            json!({ "SwitchTo": { "session_id": id } }),
        ]
    );
}

#[test]
fn an_event_carrying_an_unknown_field_ignores_it() {
    let with_pinned: SessionEvent = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","old_index":2,"new_index":0,"pinned":true}}"#,
    )
    .expect("a field this build does not know is ignored");

    let without_it: SessionEvent = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","old_index":2,"new_index":0}}"#,
    )
    .expect("the same frame without the extra field decodes");

    let expected = SessionEvent::TabMoved {
        tab_id: TabId::from_uuid(fixed_uuid()),
        old_index: 2,
        new_index: 0,
    };

    assert_eq!(
        with_pinned, expected,
        "the extra field left nothing behind in the decoded event"
    );
    assert_eq!(without_it, expected);
}

/// A whole frame this build has no name for is handed back as
/// [`MaybeKnown::Unknown`], so the client skips it and keeps reading.
#[test]
fn an_event_this_build_has_no_name_for_reads_as_unknown() {
    let decoded: IncomingEvent =
        serde_json::from_str(r#"{"Floated":{"pane_id":"00000000-0000-0000-0000-000000000001"}}"#)
            .expect("an unfamiliar frame reads as unknown, it does not fail");

    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: "Floated".to_string()
        }
    );
}

#[test]
fn an_event_missing_a_field_this_version_needs_is_refused() {
    let decoded: Result<SessionEvent, _> = serde_json::from_str(
        r#"{"PaneCreated":{"pane_id":"00000000-0000-0000-0000-000000000001"}}"#,
    );

    let error = decoded.expect_err("a frame without its tab decoded instead of failing");
    assert!(
        error.to_string().contains("missing field `tab_id`"),
        "unexpected error: {error}"
    );
}
