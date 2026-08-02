//! Tests for the client event stream's wire form: every frame survives an
//! encode/decode round trip with every field intact, the encodings are the ones
//! this protocol version pins, and a frame carrying a field this build does not
//! know is refused.

use serde_json::json;

use super::*;

/// The one UUID every id below is built from, so an encoding is byte-stable.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// Every frame the stream can carry, at fixed ids, in the order the enum
/// declares them.
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
        SessionEvent::Resync { dropped_count: 4 },
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
    // Every frame an attached client reads, pinned. A client at the old shape
    // passes the handshake, attaches, and then fails to decode the stream,
    // which reads to the user as a session that stops updating.
    //
    // So a change here — add, remove, rename, or retype anything below — turns
    // this red, and `PROTOCOL_VERSION` goes up in the same commit.
    //
    // Shape as of protocol version 3. Round-trip tests cannot catch this: one
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
            json!({ "Resync": { "dropped_count": 4 } }),
        ]
    );
}

#[test]
fn an_event_carrying_an_unknown_field_is_refused() {
    let decoded: Result<SessionEvent, _> = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","old_index":2,"new_index":0,"pinned":true}}"#,
    );

    let error = decoded.expect_err("an unknown field decoded instead of failing");
    assert!(
        error.to_string().contains("unknown field `pinned`"),
        "unexpected error: {error}"
    );

    // The same frame without that one field decodes, so the refusal above is
    // the field's doing and not a typo elsewhere in the bytes.
    let without_it: SessionEvent = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","old_index":2,"new_index":0}}"#,
    )
    .expect("the same frame without the extra field decodes");

    assert_eq!(
        without_it,
        SessionEvent::TabMoved {
            tab_id: TabId::from_uuid(fixed_uuid()),
            old_index: 2,
            new_index: 0,
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
