//! Tests for the client event stream's wire form: every frame survives an
//! encode/decode round trip with every field intact, the encodings are the ones
//! this protocol version pins, a field this build does not know is ignored,
//! a frame this build has no name for reads as unknown, and a host write frame
//! of [`MAX_HOST_WRITE_BYTE_COUNT`] bytes fits one frame.

use koshi_core::command::{PanePlacementAnchor, PanePlacementTarget, TiledPlacementTarget};
use koshi_core::geometry::Direction;
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::mouse::MouseAnswer;
use serde_json::json;

use crate::error::IpcError;
use crate::frame::{
    FrameGraphicsProtocol, FrameImageAction, FrameImageRecordHeader, FrameImageTransfer,
};
use crate::protocol::tests::build_fixed_test_uuid;

use super::*;

/// One valid image transfer header.
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
        SessionEvent::ImageContentReleased {
            image_content_ids: vec![3, 1, 4],
        },
        SessionEvent::PaneCreated {
            pane_id,
            tab_id: Some(tab_id),
        },
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: Some(130),
            signal: None,
        },
        SessionEvent::PaneClosing { pane_id },
        SessionEvent::PaneRemoved {
            pane_id,
            tab_id: Some(tab_id),
        },
        SessionEvent::PaneFocused {
            client_id,
            tab_id: Some(tab_id),
            pane_id,
            previous_pane_id: Some(pane_id),
        },
        SessionEvent::LayoutChanged { tab_id },
        SessionEvent::PanePlacementCommitted {
            command_id,
            source_pane_id: pane_id,
            source_tab_id: Some(tab_id),
            destination_tab_id: Some(tab_id),
            placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Split {
                destination_tab_id: tab_id,
                anchor: PanePlacementAnchor::Pane(pane_id),
                direction: Direction::Right,
            }),
        },
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
            owner_user_id: Some(1001),
        },
        SessionEvent::PlacementCommandRejected { command_id },
        SessionEvent::Pong,
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
fn an_image_content_start_survives_a_round_trip() {
    let expected_event = SessionEvent::ImageContentStart {
        image_transfer: build_test_image_transfer(),
    };

    let serialized_event_json =
        serde_json::to_string(&expected_event).expect("the image event encodes");
    let decoded_event: SessionEvent =
        serde_json::from_str(&serialized_event_json).expect("the image event decodes");
    assert_eq!(decoded_event, expected_event);
}

#[test]
fn an_image_content_release_has_the_pinned_wire_shape() {
    assert_eq!(
        serde_json::to_value(SessionEvent::ImageContentReleased {
            image_content_ids: vec![3, 1, 4],
        })
        .expect("the image release encodes"),
        json!({"ImageContentReleased": {"image_content_ids": [3, 1, 4]}})
    );
}

#[test]
fn an_image_content_start_has_the_pinned_wire_shape() {
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
        host_write_bytes: b"\x1b]52;c;aGVsbG8=\x07\xc3\xa9".to_vec(),
    };

    let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");
    let decoded_event: SessionEvent =
        serde_json::from_str(&serialized_event_json).expect("event decodes");

    assert_eq!(
        decoded_event,
        SessionEvent::HostWrite {
            host_write_bytes: vec![
                0x1b, b']', b'5', b'2', b';', b'c', b';', b'a', b'G', b'V', b's', b'b', b'G', b'8',
                b'=', 0x07, 0xc3, 0xa9,
            ],
        }
    );
    assert_eq!(decoded_event, expected_event);
}

#[test]
fn an_absent_optional_field_round_trips_as_absent() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let sent_events = [
        SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: None,
            signal: None,
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::from_uuid(build_fixed_test_uuid()),
            tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
            pane_id,
            previous_pane_id: None,
        },
    ];

    for sent_event in sent_events {
        let encoded_json = serde_json::to_string(&sent_event).expect("event encodes");
        let received_event: SessionEvent =
            serde_json::from_str(&encoded_json).expect("event decodes");

        assert_eq!(received_event, sent_event);
    }
}

#[test]
fn the_event_wire_shape_belongs_to_this_protocol_version() {
    // The exact encoding of every structure frame an attached client reads,
    // at protocol version 5.
    let wire_identifier = "00000000-0000-0000-0000-000000000001";

    assert_eq!(
        list_test_events()
            .iter()
            .map(|event| serde_json::to_value(event).expect("event encodes"))
            .collect::<Vec<serde_json::Value>>(),
        vec![
            json!("ImageCacheReset"),
            json!({ "ImageContentReleased": { "image_content_ids": [3, 1, 4] } }),
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
            json!({ "PanePlacementCommitted": {
                "command_id": wire_identifier,
                "source_pane_id": wire_identifier,
                "source_tab_id": wire_identifier,
                "destination_tab_id": wire_identifier,
                "placement_target": {
                    "Tiled": {
                        "Split": {
                            "destination_tab_id": wire_identifier,
                            "anchor": { "Pane": wire_identifier },
                            "direction": "Right"
                        }
                    }
                }
            } }),
            json!({ "TabCreated": { "tab_id": wire_identifier } }),
            json!({ "TabClosed": { "tab_id": wire_identifier } }),
            json!({ "TabFocused": { "client_id": wire_identifier, "tab_id": wire_identifier, "previous_tab_id": wire_identifier } }),
            json!({ "TabMoved": { "tab_id": wire_identifier, "previous_tab_index": 2, "new_tab_index": 0 } }),
            json!("Quit"),
            json!("Restarting"),
            json!("Detached"),
            json!({ "Resync": { "dropped_event_count": 4 } }),
            json!({ "SwitchTo": { "session_id": wire_identifier, "owner_user_id": 1001 } }),
            json!({ "PlacementCommandRejected": { "command_id": wire_identifier } }),
            json!("Pong"),
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
fn a_switch_without_its_user_id_is_refused() {
    let decoded_event_result: Result<SessionEvent, _> = serde_json::from_str(
        r#"{"SwitchTo":{"session_id":"00000000-0000-0000-0000-000000000001"}}"#,
    );

    let decode_error =
        decoded_event_result.expect_err("a switch without its user id decoded instead of failing");
    assert_eq!(
        decode_error.to_string(),
        "missing field `owner_user_id` at line 1 column 65"
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
    // Three frames `list_test_events` leaves out, pinned the same way: a
    // change to any name or type below turns this red.
    let wire_identifier = "00000000-0000-0000-0000-000000000001";
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());

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
            host_write_bytes: vec![0x1b, b']', 0xc3, 0xa9],
        })
        .expect("event encodes"),
        json!({ "HostWrite": { "host_write_bytes": "G13DqQ==" } })
    );
    assert_eq!(
        serde_json::to_value(SessionEvent::HostWritePart {
            host_write_bytes: vec![0x1b, b']', 0xc3, 0xa9],
        })
        .expect("event encodes"),
        json!({ "HostWritePart": { "host_write_bytes": "G13DqQ==" } })
    );
}

#[test]
fn a_host_write_travels_as_one_base64_string() {
    let expected_event = SessionEvent::HostWrite {
        host_write_bytes: vec![0x1b, b']', 0xc3, 0xa9],
    };

    let serialized_event_json = serde_json::to_string(&expected_event).expect("event encodes");

    assert_eq!(
        serialized_event_json,
        r#"{"HostWrite":{"host_write_bytes":"G13DqQ=="}}"#
    );
    let decoded_event: SessionEvent =
        serde_json::from_str(&serialized_event_json).expect("event decodes");
    assert_eq!(decoded_event, expected_event);
}

#[test]
fn every_byte_value_survives_a_host_write() {
    let all_host_write_bytes: Vec<u8> = (0..=u8::MAX).collect();
    let expected_event = SessionEvent::HostWrite {
        host_write_bytes: all_host_write_bytes.clone(),
    };

    let serialized_event_json = serde_json::to_value(&expected_event).expect("event encodes");

    assert_eq!(
        serialized_event_json,
        json!({ "HostWrite": { "host_write_bytes": "\
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
            host_write_bytes: all_host_write_bytes,
        }
    );
}

#[test]
fn a_host_write_carrying_a_list_of_numbers_is_refused() {
    let decode_error = serde_json::from_str::<SessionEvent>(
        r#"{"HostWrite":{"host_write_bytes":[27,93,195,169]}}"#,
    )
    .expect_err("a number list is not base64 text");

    assert_eq!(
        decode_error.to_string(),
        "invalid type: sequence, expected bytes as a base64 string at line 1 column 33"
    );
}

#[test]
fn an_empty_host_write_reads_as_no_bytes() {
    let decoded_event: SessionEvent =
        serde_json::from_str(r#"{"HostWrite":{"host_write_bytes":""}}"#).expect("event decodes");

    assert_eq!(
        decoded_event,
        SessionEvent::HostWrite {
            host_write_bytes: Vec::new(),
        }
    );
}

#[test]
fn a_host_write_carrying_a_number_is_refused() {
    let decode_error =
        serde_json::from_str::<SessionEvent>(r#"{"HostWrite":{"host_write_bytes":27}}"#)
            .expect_err("a number is not base64 text");

    assert_eq!(
        decode_error.to_string(),
        "invalid type: integer `27`, expected bytes as a base64 string at line 1 column 35"
    );
}

#[test]
fn a_host_write_carrying_text_that_is_not_base64_is_refused() {
    let decode_error =
        serde_json::from_str::<SessionEvent>(r#"{"HostWrite":{"host_write_bytes":"a"}}"#)
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
            host_write_bytes: vec![0, 255],
        },
        SessionEvent::HostWritePart {
            host_write_bytes: vec![0, 255],
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

#[test]
fn a_floating_pane_event_writes_a_null_tab_id_and_reads_it_back() {
    let wire_identifier = "00000000-0000-0000-0000-000000000001";
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let floating_pane_events = [
        (
            SessionEvent::PaneCreated {
                pane_id,
                tab_id: None,
            },
            json!({ "PaneCreated": { "pane_id": wire_identifier, "tab_id": null } }),
        ),
        (
            SessionEvent::PaneRemoved {
                pane_id,
                tab_id: None,
            },
            json!({ "PaneRemoved": { "pane_id": wire_identifier, "tab_id": null } }),
        ),
        (
            SessionEvent::PaneFocused {
                client_id: ClientId::from_uuid(build_fixed_test_uuid()),
                tab_id: None,
                pane_id,
                previous_pane_id: None,
            },
            json!({ "PaneFocused": {
                "client_id": wire_identifier,
                "tab_id": null,
                "pane_id": wire_identifier,
                "previous_pane_id": null
            } }),
        ),
        (
            SessionEvent::PanePlacementCommitted {
                command_id: CommandId::from_uuid(build_fixed_test_uuid()),
                source_pane_id: pane_id,
                source_tab_id: None,
                destination_tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
                placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Split {
                    destination_tab_id: TabId::from_uuid(build_fixed_test_uuid()),
                    anchor: PanePlacementAnchor::Tab,
                    direction: Direction::Right,
                }),
            },
            json!({ "PanePlacementCommitted": {
                "command_id": wire_identifier,
                "source_pane_id": wire_identifier,
                "source_tab_id": null,
                "destination_tab_id": wire_identifier,
                "placement_target": {
                    "Tiled": {
                        "Split": {
                            "destination_tab_id": wire_identifier,
                            "anchor": "Tab",
                            "direction": "Right"
                        }
                    }
                }
            } }),
        ),
    ];

    for (event, expected_json) in floating_pane_events {
        assert_eq!(
            serde_json::to_value(&event).expect("event encodes"),
            expected_json
        );
        assert_eq!(
            serde_json::from_value::<SessionEvent>(expected_json).expect("event decodes"),
            event
        );
    }
}

#[test]
fn an_event_missing_a_tab_id_field_is_refused() {
    let wire_identifier = "00000000-0000-0000-0000-000000000001";
    let incomplete_events = [
        (
            json!({ "PaneCreated": { "pane_id": wire_identifier } }),
            "tab_id",
        ),
        (
            json!({ "PaneRemoved": { "pane_id": wire_identifier } }),
            "tab_id",
        ),
        (
            json!({ "PaneFocused": {
                "client_id": wire_identifier,
                "pane_id": wire_identifier,
                "previous_pane_id": null
            } }),
            "tab_id",
        ),
        (
            json!({ "PanePlacementCommitted": {
                "command_id": wire_identifier,
                "source_pane_id": wire_identifier,
                "destination_tab_id": null,
                "placement_target": { "Tiled": { "Swap": { "target_pane_id": wire_identifier } } }
            } }),
            "source_tab_id",
        ),
        (
            json!({ "PanePlacementCommitted": {
                "command_id": wire_identifier,
                "source_pane_id": wire_identifier,
                "source_tab_id": null,
                "placement_target": { "Tiled": { "Swap": { "target_pane_id": wire_identifier } } }
            } }),
            "destination_tab_id",
        ),
    ];

    for (incomplete_event_json, missing_field_name) in incomplete_events {
        assert_eq!(
            serde_json::from_value::<SessionEvent>(incomplete_event_json)
                .expect_err("an event without the field is refused")
                .to_string(),
            format!("missing field `{missing_field_name}`")
        );
    }
}

/// Encode `session_event` as one frame into a new buffer.
fn write_session_event_frame(session_event: &SessionEvent) -> Result<Vec<u8>, IpcError> {
    let mut written_frame_bytes = Vec::new();
    crate::transport::write_message(&mut written_frame_bytes, session_event)?;
    Ok(written_frame_bytes)
}

#[test]
fn host_write_frames_of_the_largest_size_fit_one_frame() {
    let host_write_part = SessionEvent::HostWritePart {
        host_write_bytes: vec![0xff; MAX_HOST_WRITE_BYTE_COUNT],
    };
    let host_write = SessionEvent::HostWrite {
        host_write_bytes: vec![0xff; MAX_HOST_WRITE_BYTE_COUNT],
    };

    assert_eq!(
        serde_json::to_vec(&host_write_part)
            .expect("event encodes")
            .len(),
        16_777_213
    );
    assert_eq!(
        serde_json::to_vec(&host_write)
            .expect("event encodes")
            .len(),
        16_777_209
    );
    assert_eq!(
        write_session_event_frame(&host_write_part)
            .expect("the largest part fits one frame")
            .len(),
        4 + 16_777_213
    );
    assert_eq!(
        write_session_event_frame(&host_write)
            .expect("the largest final write fits one frame")
            .len(),
        4 + 16_777_209
    );
}

#[test]
fn a_host_write_part_three_bytes_past_the_largest_size_is_refused_by_the_frame_limit() {
    let host_write_part = SessionEvent::HostWritePart {
        host_write_bytes: vec![0xff; MAX_HOST_WRITE_BYTE_COUNT + 3],
    };

    assert_eq!(
        serde_json::to_vec(&host_write_part)
            .expect("event encodes")
            .len(),
        16_777_217
    );
    let Err(IpcError::FrameTooLarge {
        maximum_frame_byte_count,
        ..
    }) = write_session_event_frame(&host_write_part)
    else {
        panic!("a part three bytes past the largest one must not fit one frame");
    };
    assert_eq!(
        maximum_frame_byte_count,
        crate::transport::MAX_FRAME_BYTE_COUNT
    );
}
