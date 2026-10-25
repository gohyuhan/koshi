//! Tests for painted-frame wire encoding, image placement and transfer limits,
//! and compatibility defaults. They verify that exact field names stay on the
//! wire, and that a frame without its floating pane slots is refused.

use koshi_core::geometry::Point;
use serde_json::json;
use uuid::Uuid;

use super::*;

/// A one-pane frame at fixed ids, so its encoding is byte-stable. The pane
/// reports button-event mouse tracking, which travels as
/// [`MouseTracking::ButtonMotion`]. The client shows one pinned floating pane,
/// `pane-5`, at `(0, 0)` `4x5`.
pub(crate) fn build_painted_frame() -> PaintedFrame {
    let tab_id = TabId::from_uuid(Uuid::from_u128(2));
    let pane_id = PaneId::from_uuid(Uuid::from_u128(4));

    PaintedFrame {
        is_recovery_notice_visible: false,
        session_snapshot: FrameSession {
            session_id: SessionId::from_uuid(Uuid::from_u128(1)),
            session_name: "quiet-lake".to_string(),
            active_tab_snapshot: FrameTab {
                tab_id,
                tab_name: "edit".to_string(),
                pane_slots: vec![FrameSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        size: Size {
                            column_count: 4,
                            row_count: 3,
                        },
                    },
                    content_rect: Some(Rect {
                        origin: Point { column: 1, row: 1 },
                        size: Size {
                            column_count: 2,
                            row_count: 1,
                        },
                    }),
                    is_visible: true,
                    is_suppressed: false,
                }],
                tab_size: Size {
                    column_count: 4,
                    row_count: 3,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tab_snapshots: vec![FrameTabMetadata {
                tab_id,
                tab_name: "edit".to_string(),
                tab_index: 0,
                is_active: true,
            }],
            floating_pane_slots: vec![FrameFloatingPaneSlot {
                pane_id: PaneId::from_uuid(Uuid::from_u128(5)),
                outer_rect: Rect {
                    origin: Point { column: 0, row: 0 },
                    size: Size {
                        column_count: 4,
                        row_count: 5,
                    },
                },
                content_rect: Rect {
                    origin: Point { column: 1, row: 3 },
                    size: Size {
                        column_count: 2,
                        row_count: 1,
                    },
                },
                is_pinned: true,
            }],
            floating_panes: vec![
                FrameFloatingPane::Minimized {
                    pane_id: PaneId::from_uuid(Uuid::from_u128(6)),
                    pane_title: Some("htop".to_string()),
                },
                FrameFloatingPane::Shown {
                    pane_id: PaneId::from_uuid(Uuid::from_u128(5)),
                },
                FrameFloatingPane::Minimized {
                    pane_id: PaneId::from_uuid(Uuid::from_u128(8)),
                    pane_title: None,
                },
            ],
        },
        pane_snapshots: vec![FramePane {
            pane_id,
            pane_title: Some("vim".to_string()),
            image_placement_snapshots: vec![FrameImagePlacement {
                cell_geometry: None,
                image_record: None,
                placement_id: 7,
                image_content_id: 11,
                is_available: true,
                anchor_cell: (0, 1),
                column_count: 1,
                row_count: 1,
            }],
            is_reverse_video: false,
            mouse_tracking: MouseTracking::ButtonMotion,
            is_alternate_scroll_enabled: false,
            is_on_alt_screen: false,
            selection_spans: Some(FrameSelection {
                row_spans: vec![(0, 0, 1)],
            }),
            has_selection: true,
        }],
        client_snapshot: FrameClient {
            client_id: ClientId::from_uuid(Uuid::from_u128(3)),
            viewport_size: Size {
                column_count: 4,
                row_count: 3,
            },
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            active_tab_focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: Some(CommandId::from_uuid(Uuid::from_u128(5))),
            last_applied_floating_pane_move_command_id: Some(CommandId::from_uuid(
                Uuid::from_u128(6),
            )),
            last_applied_placement_command_id: Some(CommandId::from_uuid(Uuid::from_u128(7))),
            is_mouse_selection_enabled: false,
        },
    }
}

#[test]
fn a_frame_survives_a_round_trip_field_for_field() {
    let sent_frame = build_painted_frame();

    let encoded_frame_json = serde_json::to_string(&sent_frame).expect("encodes");
    let received_frame: PaintedFrame = serde_json::from_str(&encoded_frame_json).expect("decodes");

    assert_eq!(received_frame, sent_frame);
    assert_eq!(
        received_frame.pane_snapshots[0].mouse_tracking,
        MouseTracking::ButtonMotion
    );
}

#[test]
fn recovery_notice_reads_from_painted_frame_and_absent_field_means_hidden() {
    let mut painted_frame = build_painted_frame();
    painted_frame.is_recovery_notice_visible = true;
    let encoded_frame = serde_json::to_string(&painted_frame).expect("serialize frame");
    assert!(encoded_frame.starts_with("{\"is_recovery_notice_visible\":true,"));
    let decoded_frame: PaintedFrame = serde_json::from_str(&encoded_frame).expect("decode frame");
    assert!(decoded_frame.is_recovery_notice_visible);

    let frame_without_notice_field =
        encoded_frame.replacen("\"is_recovery_notice_visible\":true,", "", 1);
    let decoded_frame: PaintedFrame =
        serde_json::from_str(&frame_without_notice_field).expect("decode frame without notice");
    assert!(!decoded_frame.is_recovery_notice_visible);
}

#[test]
fn a_frame_without_its_floating_pane_lists_is_refused() {
    let encoded_json = serde_json::to_value(build_painted_frame()).expect("the frame encodes");
    for field_name in ["floating_pane_slots", "floating_panes"] {
        let mut frame_json_without_field = encoded_json.clone();
        frame_json_without_field["session_snapshot"]
            .as_object_mut()
            .expect("the session snapshot is an object")
            .remove(field_name)
            .expect("the floating pane list is present");

        let decode_error =
            serde_json::from_str::<PaintedFrame>(&frame_json_without_field.to_string())
                .expect_err("a frame without the floating pane list is refused");
        assert_eq!(
            decode_error.to_string(),
            format!(
                "missing field `{field_name}` at line 1 column {}",
                decode_error.column()
            )
        );
    }
}

#[test]
fn the_active_tab_focus_travels_under_its_own_name_and_null_means_none() {
    let painted_frame = build_painted_frame();
    let mut encoded_json = serde_json::to_value(&painted_frame).expect("the frame encodes");
    assert_eq!(
        encoded_json["client_snapshot"]["active_tab_focused_pane_id"],
        json!(Uuid::from_u128(4).to_string())
    );
    assert_eq!(
        encoded_json["session_snapshot"]["floating_panes"],
        json!([
            {
                "Minimized": {
                    "pane_id": Uuid::from_u128(6).to_string(),
                    "pane_title": "htop"
                }
            },
            { "Shown": { "pane_id": Uuid::from_u128(5).to_string() } },
            {
                "Minimized": {
                    "pane_id": Uuid::from_u128(8).to_string(),
                    "pane_title": null
                }
            }
        ])
    );

    encoded_json["client_snapshot"]["active_tab_focused_pane_id"] = serde_json::Value::Null;
    let decoded_frame: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("a null active tab focus decodes");
    assert_eq!(
        decoded_frame.client_snapshot.active_tab_focused_pane_id,
        None
    );
}

#[test]
fn an_image_placement_without_availability_is_refused() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("the frame encodes");
    encoded_json["pane_snapshots"][0]["image_placement_snapshots"][0]
        .as_object_mut()
        .expect("the image placement is an object")
        .remove("is_available")
        .expect("the availability is present");

    let decode_error = serde_json::from_str::<PaintedFrame>(&encoded_json.to_string())
        .expect_err("an image placement without availability is refused");
    assert_eq!(
        decode_error.to_string(),
        format!(
            "missing field `is_available` at line 1 column {}",
            decode_error.column()
        )
    );
}

#[test]
fn an_image_value_this_build_does_not_know_falls_back_without_dropping_the_frame() {
    let frame_image_record_header: FrameImageRecordHeader = serde_json::from_value(json!({
        "protocol": "Vector",
        "pixel_width": 1,
        "pixel_height": 1,
        "image_action": "Replace",
        "display": {
            "requested_width": "AutoSize",
            "sixel_background": "Opaque"
        },
        "anchor_cell": [0, 0]
    }))
    .expect("unknown image values use presentation defaults");

    assert_eq!(
        frame_image_record_header.protocol,
        FrameGraphicsProtocol::Kitty
    );
    assert_eq!(
        frame_image_record_header.image_action,
        FrameImageAction::Display
    );
    assert_eq!(
        frame_image_record_header.display,
        FrameImageDisplay::default()
    );
}

#[test]
fn an_image_placement_cannot_cross_the_cell_coordinate_limit() {
    let image_row_range_error = serde_json::from_value::<FrameImagePlacement>(json!({
        "placement_id": 1,
        "image_content_id": 2,
        "anchor_cell": [65535, 0],
        "column_count": 1,
        "row_count": 2,
        "is_available": true
    }))
    .expect_err("two rows cannot start at the last u16 row");
    assert_eq!(
        image_row_range_error.to_string(),
        "image placement exceeds the cell coordinate range"
    );

    let image_column_range_error = serde_json::from_value::<FrameImagePlacement>(json!({
        "placement_id": 1,
        "image_content_id": 2,
        "anchor_cell": [0, 65535],
        "column_count": 2,
        "row_count": 1,
        "is_available": true
    }))
    .expect_err("two columns cannot start at the last u16 column");
    assert_eq!(
        image_column_range_error.to_string(),
        "image placement exceeds the cell coordinate range"
    );

    let last_cell_placement: FrameImagePlacement = serde_json::from_value(json!({
        "placement_id": 1,
        "image_content_id": 2,
        "anchor_cell": [65535, 65535],
        "column_count": 1,
        "row_count": 1,
        "is_available": true
    }))
    .expect("one cell may occupy the last row and column");
    assert_eq!(last_cell_placement.anchor_cell, (u16::MAX, u16::MAX));
    assert_eq!(
        (
            last_cell_placement.column_count,
            last_cell_placement.row_count
        ),
        (1, 1)
    );
}

#[test]
fn an_image_transfer_with_a_wrong_byte_count_is_refused_exactly() {
    let transfer = FrameImageTransfer {
        image_content_id: 1,
        image_record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            pixel_width: 2,
            pixel_height: 1,
            image_action: FrameImageAction::Display,
            display: FrameImageDisplay::default(),
            anchor_cell: (0, 0),
        },
        image_byte_count: 8,
    };
    let mut wrong_length = serde_json::to_value(&transfer).expect("the transfer encodes");
    wrong_length["image_byte_count"] = json!(4);
    let deserialize_error = serde_json::from_value::<FrameImageTransfer>(wrong_length)
        .expect_err("a transfer with a wrong byte count is refused");
    assert_eq!(
        deserialize_error.to_string(),
        "image transfer byte length does not match its dimensions"
    );
}

#[test]
fn image_transfer_dimensions_accept_the_limits_and_refuse_the_next_value() {
    let at_pixel_limit: FrameImageTransfer = serde_json::from_value(json!({
        "image_content_id": 1,
        "image_record": {
            "protocol": "Kitty",
            "pixel_width": 16_384,
            "pixel_height": 1_024,
            "image_action": "Display",
            "display": FrameImageDisplay::default(),
            "anchor_cell": [0, 0]
        },
        "image_byte_count": 67_108_864
    }))
    .expect("the exact graphics limits are accepted");
    assert_eq!(at_pixel_limit.image_byte_count, 67_108_864);

    for (width, height, expected_byte_count) in
        [(0, 1, 0), (16_385, 1, 65_540), (16_384, 1_025, 67_174_400)]
    {
        let deserialize_error = serde_json::from_value::<FrameImageTransfer>(json!({
            "image_content_id": 1,
            "image_record": {
                "protocol": "Kitty",
                "pixel_width": width,
                "pixel_height": height,
                "image_action": "Display",
                "display": FrameImageDisplay::default(),
                "anchor_cell": [0, 0]
            },
            "image_byte_count": expected_byte_count
        }))
        .expect_err("dimensions beyond the graphics limits are refused");
        assert_eq!(
            deserialize_error.to_string(),
            "image dimensions exceed graphics limits"
        );
    }
}

#[test]
fn a_frame_encodes_to_the_shape_a_client_decodes() {
    // The exact encoding of a frame at this protocol version.
    assert_eq!(
        serde_json::to_value(build_painted_frame()).expect("frame encodes"),
        json!({
            "session_snapshot": {
                "session_id": "00000000-0000-0000-0000-000000000001",
                "session_name": "quiet-lake",
                "active_tab_snapshot": {
                    "tab_id": "00000000-0000-0000-0000-000000000002",
                    "tab_name": "edit",
                    "pane_slots": [{
                        "pane_id": "00000000-0000-0000-0000-000000000004",
                        "outer_rect": {
                            "origin": { "column": 0, "row": 0 },
                            "size": { "column_count": 4, "row_count": 3 }
                        },
                        "content_rect": {
                            "origin": { "column": 1, "row": 1 },
                            "size": { "column_count": 2, "row_count": 1 }
                        },
                        "is_visible": true,
                        "is_suppressed": false
                    }],
                    "tab_size": { "column_count": 4, "row_count": 3 },
                    "stack_headers": [],
                    "layout_mode": "Tiled",
                    "is_every_pane_suppressed": false,
                    "gap_cell_count": 0
                },
                "tab_snapshots": [{
                    "tab_id": "00000000-0000-0000-0000-000000000002",
                    "tab_name": "edit",
                    "tab_index": 0,
                    "is_active": true
                }],
                "floating_pane_slots": [{
                    "pane_id": "00000000-0000-0000-0000-000000000005",
                    "outer_rect": {
                        "origin": { "column": 0, "row": 0 },
                        "size": { "column_count": 4, "row_count": 5 }
                    },
                    "content_rect": {
                        "origin": { "column": 1, "row": 3 },
                        "size": { "column_count": 2, "row_count": 1 }
                    },
                    "is_pinned": true
                }],
                "floating_panes": [
                    {
                        "Minimized": {
                            "pane_id": "00000000-0000-0000-0000-000000000006",
                            "pane_title": "htop"
                        }
                    },
                    {
                        "Shown": {
                            "pane_id": "00000000-0000-0000-0000-000000000005"
                        }
                    },
                    {
                        "Minimized": {
                            "pane_id": "00000000-0000-0000-0000-000000000008",
                            "pane_title": null
                        }
                    }
                ]
            },
            "pane_snapshots": [{
                "pane_id": "00000000-0000-0000-0000-000000000004",
                "pane_title": "vim",
                "image_placement_snapshots": [{
                    "placement_id": 7,
                    "image_content_id": 11,
                    "is_available": true,
                    "anchor_cell": [0, 1],
                    "column_count": 1,
                    "row_count": 1
                }],
                "is_reverse_video": false,
                "mouse_tracking": "ButtonMotion",
                "is_alternate_scroll_enabled": false,
                "is_on_alt_screen": false,
                "selection_spans": { "row_spans": [[0, 0, 1]] },
                "has_selection": true
            }],
            "client_snapshot": {
                "client_id": "00000000-0000-0000-0000-000000000003",
                "viewport_size": { "column_count": 4, "row_count": 3 },
                "active_tab_id": "00000000-0000-0000-0000-000000000002",
                "focused_pane_id": "00000000-0000-0000-0000-000000000004",
                "active_tab_focused_pane_id": "00000000-0000-0000-0000-000000000004",
                "lock_mode": "Normal",
                "last_applied_lock_command_id": "00000000-0000-0000-0000-000000000005",
                "last_applied_floating_pane_move_command_id":
                    "00000000-0000-0000-0000-000000000006",
                "last_applied_placement_command_id": "00000000-0000-0000-0000-000000000007",
                "is_mouse_selection_enabled": false
            }
        })
    );
}

#[test]
fn a_frame_carrying_an_unknown_field_ignores_it() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
    encoded_json["pane_snapshots"][0]
        .as_object_mut()
        .expect("a pane encodes as an object")
        .insert("zoomed".to_string(), serde_json::Value::Bool(true));

    // Decoded from text, as the transport decodes it.
    let decoded_painted_frame: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
        .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded_painted_frame,
        build_painted_frame(),
        "the extra field left nothing behind in the decoded frame"
    );
}

/// A frame with no `gap_cell_count` reads as `0`, and a frame that carries one
/// reads back the value it was written with.
#[test]
fn a_frame_without_a_gap_reads_as_zero() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
    encoded_json["session_snapshot"]["active_tab_snapshot"]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("gap_cell_count")
        .expect("the tab encodes a gap");

    // Decoded from text, the way the transport does it.
    let decoded_painted_frame: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("a frame with no gap decodes");
    assert_eq!(
        decoded_painted_frame
            .session_snapshot
            .active_tab_snapshot
            .gap_cell_count,
        0
    );

    let mut gapped_painted_frame = build_painted_frame();
    gapped_painted_frame
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count = 2;
    let encoded_json = serde_json::to_value(&gapped_painted_frame).expect("frame encodes");
    let decoded_painted_frame: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("a frame with a gap decodes");
    assert_eq!(
        decoded_painted_frame
            .session_snapshot
            .active_tab_snapshot
            .gap_cell_count,
        2
    );
}

/// A `gap_cell_count` that is not a cell count — negative, or a string — reads as `0`
/// and leaves the rest of the frame intact.
#[test]
fn a_frame_whose_gap_is_not_a_count_reads_as_zero() {
    for invalid_gap_value in [
        serde_json::json!(-1),
        serde_json::json!("2"),
        serde_json::json!(null),
        serde_json::json!(70_000),
    ] {
        let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
        encoded_json["session_snapshot"]["active_tab_snapshot"]["gap_cell_count"] =
            invalid_gap_value;
        let decoded_painted_frame: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
            .expect("a frame with a bad gap decodes");
        assert_eq!(decoded_painted_frame, build_painted_frame());
    }
}

/// An optional value that is `None` travels as `null`, never as an absent
/// key: a pane with no title and no highlight, a slot with no content rect,
/// and a client with no focused pane.
#[test]
fn absent_optional_values_encode_as_null() {
    let mut bare_frame = build_painted_frame();
    bare_frame.session_snapshot.active_tab_snapshot.pane_slots[0].content_rect = None;
    bare_frame.pane_snapshots[0].pane_title = None;
    bare_frame.pane_snapshots[0].selection_spans = None;
    bare_frame.client_snapshot.focused_pane_id = None;

    let encoded_frame_json = serde_json::to_value(&bare_frame).expect("frame encodes");

    assert_eq!(
        encoded_frame_json["session_snapshot"]["active_tab_snapshot"]["pane_slots"][0]
            ["content_rect"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_frame_json["pane_snapshots"][0]["pane_title"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_frame_json["pane_snapshots"][0]["selection_spans"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_frame_json["client_snapshot"]["focused_pane_id"],
        serde_json::Value::Null
    );
    let decoded_frame: PaintedFrame =
        serde_json::from_str(&encoded_frame_json.to_string()).expect("a bare frame decodes");
    assert_eq!(decoded_frame, bare_frame);
}
