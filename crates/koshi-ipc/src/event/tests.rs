//! Tests for the client event stream's wire form: every frame survives an
//! encode/decode round trip with every field intact, the encodings are the ones
//! this protocol version pins, a field this build does not know is ignored,
//! and a frame this build has no name for reads as unknown.

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseTracking};
use koshi_layout::mode::LayoutMode;
use koshi_pane::pane::state::PaneKind;
use serde_json::json;

use crate::frame::{
    FrameClient, FrameCursor, FrameCursorShape, FrameGraphicsProtocol, FrameImageAction,
    FrameImageChunk, FrameImageRecordHeader, FrameImageTransfer, FramePane, FrameScrollback,
    FrameSession, FrameSlot, FrameTab, FrameTabMeta,
};

use super::*;

/// The one UUID every id below is built from, so an encoding is byte-stable.
fn build_fixed_test_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// A one-pane frame at fixed ids, so its encoding is byte-stable. The pane
/// shows no terminal content this frame, so its `window` is `None`; the frame's
/// own wire shape is pinned in the frame module's tests.
fn build_test_painted_frame() -> PaintedFrame {
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());

    PaintedFrame {
        session_snapshot: FrameSession {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
            session_revision: 0,
            session_name: "quiet-lake".to_string(),
            active_tab_snapshot: FrameTab {
                tab_id,
                tab_name: "edit".to_string(),
                pane_slots: vec![FrameSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        cell_size: Size {
                            column_count: 4,
                            row_count: 3,
                        },
                    },
                    content_rect: Some(Rect {
                        origin: Point { column: 1, row: 1 },
                        cell_size: Size {
                            column_count: 2,
                            row_count: 1,
                        },
                    }),
                    pane_kind: PaneKind::Terminal,
                    is_visible: true,
                    is_suppressed: false,
                    is_dead: false,
                }],
                effective_cell_size: Size {
                    column_count: 4,
                    row_count: 3,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tab_snapshots: vec![FrameTabMeta {
                tab_id,
                tab_name: "edit".to_string(),
                tab_index: 0,
                is_active: true,
            }],
        },
        pane_snapshots: vec![FramePane {
            pane_id,
            pane_title: Some("vim".to_string()),
            cursor_snapshot: FrameCursor {
                row_index: 0,
                column_index: 1,
                is_visible: true,
                is_blinking: false,
                shape: Some(FrameCursorShape::Bar),
            },
            terminal_window: None,
            image_placement_snapshots: Vec::new(),
            is_reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            is_alt_scroll_enabled: false,
            is_on_alt_screen: false,
            view_top_row_index: 7,
            selection_spans: None,
            has_selection: false,
            scrollback_meta: FrameScrollback {
                is_truncated: false,
                retained_line_count: 12,
            },
        }],
        client_snapshot: FrameClient {
            client_id: ClientId::from_uuid(build_fixed_test_uuid()),
            client_revision: 0,
            viewport_size: Size {
                column_count: 4,
                row_count: 3,
            },
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
    }
}

/// One valid image transfer header for the fixed pane in [`painted_frame`].
fn build_test_image_transfer() -> FrameImageTransfer {
    FrameImageTransfer {
        image_content_id: 1,
        image_record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            pixel_width: 2,
            pixel_height: 1,
            image_action: FrameImageAction::Display,
            display: crate::frame::FrameImageDisplay::default(),
            anchor_cell: (0, 0),
        },
        image_byte_count: 8,
    }
}

/// Every small structure frame the stream can carry, at fixed ids, in the
/// order the enum declares them. Frames with larger payloads have their own
/// tests below.
fn list_test_events() -> Vec<SessionEvent> {
    let client_id = ClientId::from_uuid(build_fixed_test_uuid());
    let command_id = CommandId::from_uuid(build_fixed_test_uuid());
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());

    vec![
        SessionEvent::ImageCacheReset,
        SessionEvent::PaneCreated { pane_id, tab_id },
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: Some(130),
            signal: None,
        },
        SessionEvent::PaneClosing { pane_id },
        SessionEvent::PaneRemoved { pane_id, tab_id },
        SessionEvent::PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: Some(pane_id),
        },
        SessionEvent::LayoutChanged { tab_id },
        SessionEvent::TabCreated { tab_id },
        SessionEvent::TabClosed { tab_id },
        SessionEvent::TabFocused {
            client_id,
            tab_id,
            previous_tab_id: tab_id,
        },
        SessionEvent::TabMoved {
            tab_id,
            previous_tab_index: 2,
            new_tab_index: 0,
        },
        SessionEvent::Quit,
        SessionEvent::Restarting,
        SessionEvent::Detached,
        SessionEvent::Resync {
            dropped_event_count: 4,
        },
        SessionEvent::SwitchTo {
            session_id: SessionId::from_uuid(build_fixed_test_uuid()),
        },
        SessionEvent::PlacementCommandRejected { command_id },
    ]
}

#[test]
fn every_event_survives_a_round_trip_field_for_field() {
    for expected_event in list_test_events() {
        let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");
        let decoded_event: SessionEvent =
            serde_json::from_str(&serialized_event_json).expect("event decodes");

        assert_eq!(decoded_event, expected_event);
    }
}

#[test]
fn a_painted_frame_survives_a_round_trip_field_for_field() {
    let expected_event = SessionEvent::Painted {
        frame: Box::new(build_test_painted_frame()),
    };

    let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");
    let decoded_event: SessionEvent =
        serde_json::from_str(&serialized_event_json).expect("event decodes");

    assert_eq!(decoded_event, expected_event);
}

#[test]
fn an_image_content_start_and_chunk_survive_a_round_trip() {
    let expected_events = [
        SessionEvent::ImageContentStart {
            image_transfer: build_test_image_transfer(),
        },
        SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: 1,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: vec![0, 1, 2, 3, 4, 5, 6, 7],
            },
        },
    ];

    for expected_event in expected_events {
        let serialized_event_json =
            serde_json::to_string(&expected_event).expect("the image event encodes");
        let decoded_event: SessionEvent =
            serde_json::from_str(&serialized_event_json).expect("the image event decodes");
        assert_eq!(decoded_event, expected_event);
    }
}

#[test]
fn image_content_events_have_the_pinned_wire_shape() {
    assert_eq!(
        serde_json::to_value(SessionEvent::ImageContentStart {
            image_transfer: build_test_image_transfer(),
        })
        .expect("the image start encodes"),
        json!({
            "ImageContentStart": {
                "image_transfer": {
                    "image_content_id": 1,
                    "image_record": {
                        "protocol": "Kitty",
                        "pixel_width": 2,
                        "pixel_height": 1,
                        "image_action": "Display",
                        "display": {
                            "requested_width": null,
                            "requested_height": null,
                            "is_aspect_ratio_preserved": true,
                            "sixel_background": null,
                            "image_id": null,
                            "image_number": null,
                            "placement_id": null,
                            "usage_hints": 0,
                            "is_unicode_placeholder": false,
                            "z_index": 0,
                            "relative_image_id": null,
                            "relative_placement_id": null,
                            "relative_column_offset": 0,
                            "relative_row_offset": 0,
                            "requested_column_count": null,
                            "requested_row_count": null,
                            "source_pixel_offset_x": null,
                            "source_pixel_offset_y": null,
                            "cell_pixel_offset_x": null,
                            "cell_pixel_offset_y": null,
                            "should_move_cursor": true
                        },
                        "anchor_cell": [0, 0]
                    },
                    "image_byte_count": 8
                }
            }
        })
    );
    assert_eq!(
        serde_json::to_value(SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: 1,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: vec![0, 1, 2, 3, 4, 5, 6, 7],
            },
        })
        .expect("the image chunk encodes"),
        json!({
            "ImageContentChunk": {
                "image_chunk": {
                    "image_transfer_id": 1,
                    "byte_offset": 0,
                    "is_last": true,
                    "chunk_bytes": "AAECAwQFBgc="
                }
            }
        })
    );
}

#[test]
fn a_mouse_answer_survives_a_round_trip_field_for_field() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let other_pane_id = PaneId::new();
    let expected_events = [
        // The normal case: the round ran and had nothing to report.
        SessionEvent::MouseAnswer {
            request_id: 7,
            mouse_answers: Vec::new(),
        },
        SessionEvent::MouseAnswer {
            request_id: 8,
            mouse_answers: vec![MouseAnswer::Scrolled {
                pane_id,
                top_row_number: None,
            }],
        },
        SessionEvent::MouseAnswer {
            request_id: 9,
            mouse_answers: vec![MouseAnswer::Scrolled {
                pane_id,
                top_row_number: Some(938),
            }],
        },
        SessionEvent::MouseAnswer {
            request_id: 10,
            mouse_answers: vec![MouseAnswer::Resized {
                pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                applied_cell_count: 0,
            }],
        },
        SessionEvent::MouseAnswer {
            request_id: 11,
            mouse_answers: vec![
                MouseAnswer::Scrolled {
                    pane_id,
                    top_row_number: Some(938),
                },
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    applied_cell_count: 0,
                },
            ],
        },
        // Two border moves in one round: each entry names its own border and
        // its own direction, so the pair stays told apart across the wire.
        SessionEvent::MouseAnswer {
            request_id: 12,
            mouse_answers: vec![
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    applied_cell_count: 8,
                },
                MouseAnswer::Resized {
                    pane_id: other_pane_id,
                    border_side: Direction::Left,
                    resize_step: 1,
                    applied_cell_count: 1,
                },
            ],
        },
    ];

    for expected_event in expected_events {
        let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");
        let decoded_event: SessionEvent =
            serde_json::from_str(&serialized_event_json).expect("event decodes");

        assert_eq!(decoded_event, expected_event);
    }
}

#[test]
fn a_host_write_survives_a_round_trip() {
    // An OSC 52 copy of "hello": a byte over 127 and a control byte, so a
    // spelling that mangled either shows up here.
    let expected_event = SessionEvent::HostWrite {
        host_output_bytes: b"\x1b]52;c;aGVsbG8=\x07\xc3\xa9".to_vec(),
    };

    let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");
    let decoded_event: SessionEvent =
        serde_json::from_str(&serialized_event_json).expect("event decodes");

    assert_eq!(
        decoded_event,
        SessionEvent::HostWrite {
            host_output_bytes: vec![
                0x1b, b']', b'5', b'2', b';', b'c', b';', b'a', b'G', b'V', b's', b'b', b'G', b'8',
                b'=', 0x07, 0xc3, 0xa9,
            ],
        }
    );
    assert_eq!(decoded_event, expected_event);
}

#[test]
fn a_painted_frame_carrying_an_unknown_field_ignores_it() {
    let mut encoded_json = serde_json::to_value(SessionEvent::Painted {
        frame: Box::new(build_test_painted_frame()),
    })
    .expect("event encodes");
    encoded_json["Painted"]["frame"]["pane_snapshots"][0]
        .as_object_mut()
        .expect("a pane encodes as an object")
        .insert("zoomed".to_string(), serde_json::Value::Bool(true));

    // Decoded from text, the way the transport does it.
    let decoded_event: SessionEvent = serde_json::from_str(&encoded_json.to_string())
        .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_event,
        SessionEvent::Painted {
            frame: Box::new(build_test_painted_frame()),
        },
        "the extra field left nothing behind in the decoded event"
    );
}

#[test]
fn an_absent_optional_field_round_trips_as_absent() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let sent = [
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: None,
            signal: None,
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::from_uuid(build_fixed_test_uuid()),
            tab_id: TabId::from_uuid(build_fixed_test_uuid()),
            pane_id,
            previous_pane_id: None,
        },
    ];

    for event in sent {
        let encoded_json = serde_json::to_string(&event).expect("event encodes");
        let received: SessionEvent = serde_json::from_str(&encoded_json).expect("event decodes");

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
    // Shape as of protocol version 4. Round-trip tests cannot catch this: one
    // build encoding and decoding its own structs always agrees with itself.
    let wire_identifier = "00000000-0000-0000-0000-000000000001";

    assert_eq!(
        list_test_events()
            .iter()
            .map(|event| serde_json::to_value(event).expect("event encodes"))
            .collect::<Vec<serde_json::Value>>(),
        vec![
            json!("ImageCacheReset"),
            json!({ "PaneCreated": { "pane_id": wire_identifier, "tab_id": wire_identifier } }),
            json!({ "PaneProcessExited": { "pane_id": wire_identifier, "exit_code": 130, "signal": null } }),
            json!({ "PaneClosing": { "pane_id": wire_identifier } }),
            json!({ "PaneRemoved": { "pane_id": wire_identifier, "tab_id": wire_identifier } }),
            json!({ "PaneFocused": {
                "client_id": wire_identifier,
                "tab_id": wire_identifier,
                "pane_id": wire_identifier,
                "previous_pane_id": wire_identifier
            } }),
            json!({ "LayoutChanged": { "tab_id": wire_identifier } }),
            json!({ "TabCreated": { "tab_id": wire_identifier } }),
            json!({ "TabClosed": { "tab_id": wire_identifier } }),
            json!({ "TabFocused": { "client_id": wire_identifier, "tab_id": wire_identifier, "previous_tab_id": wire_identifier } }),
            json!({ "TabMoved": { "tab_id": wire_identifier, "previous_tab_index": 2, "new_tab_index": 0 } }),
            json!("Quit"),
            json!("Restarting"),
            json!("Detached"),
            json!({ "Resync": { "dropped_event_count": 4 } }),
            json!({ "SwitchTo": { "session_id": wire_identifier } }),
            json!({ "PlacementCommandRejected": { "command_id": wire_identifier } }),
        ]
    );
}

#[test]
fn an_event_carrying_an_unknown_field_ignores_it() {
    let decoded_event_with_unknown_field: SessionEvent = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","previous_tab_index":2,"new_tab_index":0,"pinned":true}}"#,
    )
    .expect("a field this build does not know is ignored");

    let decoded_event_without_unknown_field: SessionEvent = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","previous_tab_index":2,"new_tab_index":0}}"#,
    )
    .expect("the same frame without the extra field decodes");

    let expected_tab_moved_event = SessionEvent::TabMoved {
        tab_id: TabId::from_uuid(build_fixed_test_uuid()),
        previous_tab_index: 2,
        new_tab_index: 0,
    };

    assert_eq!(
        decoded_event_with_unknown_field, expected_tab_moved_event,
        "the extra field left nothing behind in the decoded event"
    );
    assert_eq!(
        decoded_event_without_unknown_field,
        expected_tab_moved_event
    );
}

/// A whole frame this build has no name for is handed back as
/// [`MaybeKnown::Unknown`], so the client skips it and keeps reading.
#[test]
fn an_event_this_build_has_no_name_for_reads_as_unknown() {
    let decoded_event: IncomingEvent =
        serde_json::from_str(r#"{"Floated":{"pane_id":"00000000-0000-0000-0000-000000000001"}}"#)
            .expect("an unfamiliar frame reads as unknown, it does not fail");

    assert_eq!(
        decoded_event,
        MaybeKnown::Unknown {
            variant_name: "Floated".to_string()
        }
    );
}

#[test]
fn an_event_missing_a_field_this_version_needs_is_refused() {
    let decoded_event_result: Result<SessionEvent, _> = serde_json::from_str(
        r#"{"PaneCreated":{"pane_id":"00000000-0000-0000-0000-000000000001"}}"#,
    );

    let decode_error =
        decoded_event_result.expect_err("a frame without its tab decoded instead of failing");
    assert_eq!(
        decode_error.to_string(),
        "missing field `tab_id` at line 1 column 65"
    );
}

#[test]
fn the_payload_frames_wire_shape_belongs_to_this_protocol_version() {
    // The three frames `list_test_events` leaves out, pinned the same way: a
    // change to any name or type below turns this red.
    let wire_identifier = "00000000-0000-0000-0000-000000000001";
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());

    assert_eq!(
        serde_json::to_value(SessionEvent::Painted {
            frame: Box::new(build_test_painted_frame()),
        })
        .expect("event encodes"),
        json!({ "Painted": {
            "frame": serde_json::to_value(build_test_painted_frame()).expect("frame encodes")
        } })
    );
    assert_eq!(
        serde_json::to_value(SessionEvent::MouseAnswer {
            request_id: 9,
            mouse_answers: vec![
                MouseAnswer::Scrolled {
                    pane_id,
                    top_row_number: Some(938),
                },
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    applied_cell_count: 0,
                },
            ],
        })
        .expect("event encodes"),
        json!({ "MouseAnswer": {
            "request_id": 9,
            "mouse_answers": [
                { "Scrolled": { "pane_id": wire_identifier, "top_row_number": 938 } },
                { "Resized": { "pane_id": wire_identifier, "border_side": "Up", "resize_step": -1, "applied_cell_count": 0 } }
            ]
        } })
    );
    assert_eq!(
        serde_json::to_value(SessionEvent::HostWrite {
            host_output_bytes: vec![0x1b, b']', 0xc3, 0xa9],
        })
        .expect("event encodes"),
        json!({ "HostWrite": { "host_output_bytes": "G13DqQ==" } })
    );
}

#[test]
fn a_host_write_travels_as_one_base64_string() {
    let expected_event = SessionEvent::HostWrite {
        host_output_bytes: vec![0x1b, b']', 0xc3, 0xa9],
    };

    let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");

    assert_eq!(
        serialized_event_json,
        r#"{"HostWrite":{"host_output_bytes":"G13DqQ=="}}"#
    );
    let decoded_event: SessionEvent =
        serde_json::from_str(&serialized_event_json).expect("event decodes");
    assert_eq!(decoded_event, expected_event);
}

#[test]
fn every_byte_value_survives_a_host_write() {
    let all_host_output_bytes: Vec<u8> = (0..=u8::MAX).collect();
    let expected_event = SessionEvent::HostWrite {
        host_output_bytes: all_host_output_bytes.clone(),
    };

    let serialized_event_json = serde_json::to_value(&expected_event).expect("event encodes");

    assert_eq!(
        serialized_event_json,
        json!({ "HostWrite": { "host_output_bytes": "\
AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7\
PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3\
eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKz\
tLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v\
8PHy8/T19vf4+fr7/P3+/w==" } })
    );
    let decoded_event: SessionEvent =
        serde_json::from_value(serialized_event_json).expect("event decodes");
    assert_eq!(
        decoded_event,
        SessionEvent::HostWrite {
            host_output_bytes: all_host_output_bytes,
        }
    );
}

/// The shape a session server speaking session protocol 2 writes. A client
/// that upgraded while such a session is still running reads it.
#[test]
fn a_host_write_carrying_a_list_of_numbers_still_reads() {
    let decoded_event: SessionEvent =
        serde_json::from_str(r#"{"HostWrite":{"host_output_bytes":[27,93,195,169]}}"#)
            .expect("event decodes");

    assert_eq!(
        decoded_event,
        SessionEvent::HostWrite {
            host_output_bytes: vec![0x1b, b']', 0xc3, 0xa9],
        }
    );
    // What it decoded to is written back as base64, never as the list it came
    // from.
    assert_eq!(
        serde_json::to_string(&decoded_event).expect("event encodes"),
        r#"{"HostWrite":{"host_output_bytes":"G13DqQ=="}}"#
    );
}

#[test]
fn an_empty_host_write_reads_from_either_shape() {
    let list_encoded_event: SessionEvent =
        serde_json::from_str(r#"{"HostWrite":{"host_output_bytes":[]}}"#).expect("event decodes");
    let base64_encoded_event: SessionEvent =
        serde_json::from_str(r#"{"HostWrite":{"host_output_bytes":""}}"#).expect("event decodes");

    assert_eq!(
        list_encoded_event,
        SessionEvent::HostWrite {
            host_output_bytes: Vec::new(),
        }
    );
    assert_eq!(base64_encoded_event, list_encoded_event);
}

#[test]
fn a_host_write_list_entry_outside_a_byte_is_refused() {
    let decode_error =
        serde_json::from_str::<SessionEvent>(r#"{"HostWrite":{"host_output_bytes":[27,256]}}"#)
            .expect_err("256 is not a byte");

    assert!(
        decode_error.to_string().contains("invalid value"),
        "unexpected refusal: {decode_error}"
    );
}

#[test]
fn a_host_write_carrying_neither_shape_is_refused() {
    let decode_error =
        serde_json::from_str::<SessionEvent>(r#"{"HostWrite":{"host_output_bytes":27}}"#)
            .expect_err("a number is neither shape");

    assert!(
        decode_error
            .to_string()
            .contains("bytes as a base64 string or as a list of numbers"),
        "unexpected refusal: {decode_error}"
    );
}

#[test]
fn a_host_write_carrying_text_that_is_not_base64_is_refused() {
    let decode_error =
        serde_json::from_str::<SessionEvent>(r#"{"HostWrite":{"host_output_bytes":"a"}}"#)
            .expect_err("one character is not a base64 group");

    assert!(
        decode_error
            .to_string()
            .contains("the base64 text length is not a multiple of four"),
        "unexpected refusal: {decode_error}"
    );
}

#[test]
fn numeric_fields_round_trip_at_their_extremes() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let expected_events = [
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: Some(i32::MIN),
            signal: None,
        },
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: Some(i32::MAX),
            signal: None,
        },
        SessionEvent::TabMoved {
            tab_id: TabId::from_uuid(build_fixed_test_uuid()),
            previous_tab_index: usize::MAX,
            new_tab_index: 0,
        },
        SessionEvent::Resync {
            dropped_event_count: u64::MAX,
        },
        SessionEvent::MouseAnswer {
            request_id: u64::MAX,
            mouse_answers: Vec::new(),
        },
        SessionEvent::HostWrite {
            host_output_bytes: vec![0, 255],
        },
    ];

    for expected_event in expected_events {
        let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");
        let decoded_event: SessionEvent =
            serde_json::from_str(&serialized_event_json).expect("event decodes");

        assert_eq!(decoded_event, expected_event);
    }
}

#[test]
fn an_event_whose_count_is_negative_is_refused() {
    let dropped_event: Result<SessionEvent, _> =
        serde_json::from_str(r#"{"Resync":{"dropped_event_count":-4}}"#);
    let malformed_tab_move_event: Result<SessionEvent, _> = serde_json::from_str(
        r#"{"TabMoved":{"tab_id":"00000000-0000-0000-0000-000000000001","previous_tab_index":-1,"new_tab_index":0}}"#,
    );

    assert_eq!(
        dropped_event
            .expect_err("a negative dropped count decoded instead of failing")
            .to_string(),
        "invalid value: integer `-4`, expected u64 at line 1 column 35"
    );
    assert_eq!(
        malformed_tab_move_event
            .expect_err("a negative index decoded instead of failing")
            .to_string(),
        "invalid value: integer `-1`, expected usize at line 1 column 84"
    );
}

#[test]
fn an_event_whose_id_is_not_a_uuid_is_refused() {
    let decoded_event: Result<SessionEvent, _> =
        serde_json::from_str(r#"{"PaneClosing":{"pane_id":"not-a-uuid"}}"#);

    assert_eq!(
        decoded_event
            .expect_err("a pane id that is not a UUID decoded instead of failing")
            .to_string(),
        "UUID parsing failed: invalid character: found `n` at 0 at line 1 column 38"
    );
}

/// `Quit` carries no payload on the wire: it serializes as the bare name.
#[test]
fn quit_serializes_as_the_bare_name() {
    assert_eq!(
        serde_json::to_string(&SessionEvent::Quit).expect("serialize"),
        r#""Quit""#
    );
}

/// A peer that predates the `signal` field sends a `PaneProcessExited`
/// without it; the field reads as `None`.
#[test]
fn a_pane_exit_frame_without_a_signal_field_decodes_with_no_signal() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let exit_event_json = format!(
        r#"{{"PaneProcessExited":{{"pane_id":"{}","exit_code":null}}}}"#,
        pane_id.get_uuid()
    );

    let decoded_event: SessionEvent =
        serde_json::from_str(&exit_event_json).expect("decodes without signal");

    assert_eq!(
        decoded_event,
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: None,
            signal: None,
        }
    );
}

/// A frame this build has reads as [`MaybeKnown::Known`] through
/// [`IncomingEvent`], whether it carries fields or is a bare name.
#[test]
fn a_frame_this_build_has_reads_as_known() {
    let bare_event: IncomingEvent = serde_json::from_str(r#""Quit""#).expect("a bare name decodes");
    let event_with_fields: IncomingEvent =
        serde_json::from_str(r#"{"Resync":{"dropped_event_count":4}}"#).expect("a frame decodes");

    assert_eq!(bare_event, MaybeKnown::Known(SessionEvent::Quit));
    assert_eq!(
        event_with_fields,
        MaybeKnown::Known(SessionEvent::Resync {
            dropped_event_count: 4,
        })
    );
}
