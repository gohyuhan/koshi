//! Tests for painted-frame wire encoding, image placement and transfer limits,
//! frame-row run-length expansion, and compatibility defaults. They verify that frame-row
//! runs expand to the original cells, counts above `u16::MAX` split, and exact
//! field names stay on the wire.

use koshi_core::geometry::Point;
use serde_json::json;
use uuid::Uuid;

use super::*;

/// A plain cell style with `fg` as its foreground: no underline color, no
/// attributes set.
fn build_frame_style(foreground_color: FrameColor) -> FrameStyle {
    FrameStyle {
        foreground_color,
        background_color: FrameColor::Default,
        underline_color: None,
        text_attributes: FrameAttrs {
            is_bold: false,
            is_italic: false,
            is_reverse: false,
            is_faint: false,
            is_blinking: false,
            is_concealed: false,
            is_struck_through: false,
            is_overlined: false,
            underline_style: FrameUnderline::None,
        },
    }
}

/// A one-column cell holding `ch` in `fg`.
fn build_frame_cell(character: char, foreground_color: FrameColor) -> FrameCell {
    FrameCell {
        character,
        combining_characters: Vec::new(),
        cell_width: 1,
        style: build_frame_style(foreground_color),
    }
}

/// A one-column blank cell in the default colors.
fn build_blank_frame_cell() -> FrameCell {
    build_frame_cell(' ', FrameColor::Default)
}

/// A one-pane frame at fixed ids, so its encoding is byte-stable. The pane
/// reports button-event mouse tracking, which travels as
/// [`MouseTracking::ButtonMotion`].
fn build_painted_frame() -> PaintedFrame {
    let tab = TabId::from_uuid(Uuid::from_u128(2));
    let pane = PaneId::from_uuid(Uuid::from_u128(4));

    PaintedFrame {
        session_snapshot: FrameSession {
            session_id: SessionId::from_uuid(Uuid::from_u128(1)),
            session_revision: 17,
            session_name: "quiet-lake".to_string(),
            active_tab_snapshot: FrameTab {
                tab_id: tab,
                tab_name: "edit".to_string(),
                pane_slots: vec![FrameSlot {
                    pane_id: pane,
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
                tab_id: tab,
                tab_name: "edit".to_string(),
                tab_index: 0,
                is_active: true,
            }],
        },
        pane_snapshots: vec![FramePane {
            pane_id: pane,
            pane_title: Some("vim".to_string()),
            cursor_snapshot: FrameCursor {
                row_index: 0,
                column_index: 1,
                is_visible: true,
                is_blinking: false,
                shape: Some(FrameCursorShape::Bar),
            },
            terminal_window: Some(FrameWindow {
                column_count: 2,
                row_snapshots: vec![FrameRow::from_cells(
                    [
                        build_frame_cell('h', FrameColor::Default),
                        build_frame_cell('i', FrameColor::Default),
                    ],
                    FrameRowEnd::Hard,
                )],
                view_row_offset: 0,
            }),
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
            is_alt_scroll_enabled: false,
            is_on_alt_screen: false,
            view_top_row_index: 7,
            selection_spans: Some(FrameSelection {
                row_spans: vec![(0, 0, 1)],
            }),
            has_selection: true,
            scrollback_meta: FrameScrollback {
                is_truncated: false,
                retained_line_count: 12,
            },
        }],
        client_snapshot: FrameClient {
            client_id: ClientId::from_uuid(Uuid::from_u128(3)),
            client_revision: 19,
            viewport_size: Size {
                column_count: 4,
                row_count: 3,
            },
            active_tab_id: tab,
            focused_pane_id: Some(pane),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
    }
}

#[test]
fn an_eighty_column_blank_row_travels_as_one_run() {
    let cells: Vec<FrameCell> = std::iter::repeat_n(build_blank_frame_cell(), 80).collect();

    let frame_row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        frame_row.cell_runs,
        vec![FrameRun {
            repeat_count: 80,
            cell: build_blank_frame_cell()
        }]
    );
    assert_eq!(frame_row.expand_cells(), cells);
}

#[test]
fn stretches_of_two_styles_fold_into_one_run_each() {
    let red = build_frame_cell('x', FrameColor::Indexed(1));
    let blue = build_frame_cell('x', FrameColor::Rgb(0, 0, 255));
    let cells = vec![
        red.clone(),
        red.clone(),
        blue.clone(),
        blue.clone(),
        red.clone(),
        red.clone(),
    ];

    let frame_row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        frame_row.cell_runs,
        vec![
            FrameRun {
                repeat_count: 2,
                cell: red.clone()
            },
            FrameRun {
                repeat_count: 2,
                cell: blue
            },
            FrameRun {
                repeat_count: 2,
                cell: red
            },
        ]
    );
    assert_eq!(frame_row.expand_cells(), cells);
}

#[test]
fn a_run_longer_than_a_count_can_hold_splits_at_the_cap() {
    let cells: Vec<FrameCell> = std::iter::repeat_n(build_blank_frame_cell(), 70_000).collect();

    let frame_row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        frame_row.cell_runs,
        vec![
            FrameRun {
                repeat_count: u16::MAX,
                cell: build_blank_frame_cell()
            },
            FrameRun {
                repeat_count: 4_465,
                cell: build_blank_frame_cell()
            },
        ]
    );
    assert_eq!(frame_row.expand_cells(), cells);
}

#[test]
fn an_empty_row_folds_to_no_runs_and_expands_to_no_cells() {
    let frame_row = FrameRow::from_cells([], FrameRowEnd::Hard);

    assert_eq!(frame_row.cell_runs, Vec::new());
    assert_eq!(frame_row.expand_cells(), Vec::new());
}

#[test]
fn a_frame_survives_a_round_trip_field_for_field() {
    let sent = build_painted_frame();

    let encoded_json = serde_json::to_string(&sent).expect("encodes");
    let received: PaintedFrame = serde_json::from_str(&encoded_json).expect("decodes");

    assert_eq!(received, sent);
    assert_eq!(
        received.pane_snapshots[0].mouse_tracking,
        MouseTracking::ButtonMotion
    );
}

#[test]
fn an_old_frame_without_placement_revisions_reads_zero_generations() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("the frame encodes");
    encoded_json["session_snapshot"]
        .as_object_mut()
        .expect("the session snapshot is an object")
        .remove("session_revision")
        .expect("the session revision is present");
    encoded_json["client_snapshot"]
        .as_object_mut()
        .expect("the client snapshot is an object")
        .remove("client_revision")
        .expect("the client revision is present");

    let received: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("an old frame decodes");

    assert_eq!(received.session_snapshot.session_revision, 0);
    assert_eq!(received.client_snapshot.client_revision, 0);
}

#[test]
fn an_image_placement_without_availability_expects_its_record() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("the frame encodes");
    encoded_json["pane_snapshots"][0]["image_placement_snapshots"][0]
        .as_object_mut()
        .expect("the image placement is an object")
        .remove("is_available");

    let received: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("the frame decodes");

    assert!(received.pane_snapshots[0].image_placement_snapshots[0].is_available);
}

#[test]
fn image_chunk_bytes_use_base64_on_wire_and_read_old_number_lists() {
    let chunk = FrameImageChunk {
        image_transfer_id: 1,
        byte_offset: 0,
        is_last: true,
        chunk_bytes: vec![0, 1, 2, 255, 4, 5, 6, 7],
    };

    assert_eq!(
        serde_json::to_value(&chunk).expect("the image chunk encodes"),
        json!({
            "image_transfer_id": 1,
            "byte_offset": 0,
            "is_last": true,
            "chunk_bytes": "AAEC/wQFBgc="
        })
    );

    let from_list: FrameImageChunk = serde_json::from_value(json!({
        "image_transfer_id": 1,
        "byte_offset": 0,
        "is_last": true,
        "chunk_bytes": [0, 1, 2, 255, 4, 5, 6, 7]
    }))
    .expect("the number-list image chunk decodes");

    assert_eq!(from_list, chunk);
    assert_eq!(
        serde_json::to_value(from_list).expect("the decoded image chunk re-encodes"),
        json!({
            "image_transfer_id": 1,
            "byte_offset": 0,
            "is_last": true,
            "chunk_bytes": "AAEC/wQFBgc="
        })
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
        "row_count": 2
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
        "row_count": 1
    }))
    .expect_err("two columns cannot start at the last u16 column");
    assert_eq!(
        image_column_range_error.to_string(),
        "image placement exceeds the cell coordinate range"
    );

    let edge: FrameImagePlacement = serde_json::from_value(json!({
        "placement_id": 1,
        "image_content_id": 2,
        "anchor_cell": [65535, 65535],
        "column_count": 1,
        "row_count": 1
    }))
    .expect("one cell may occupy the last row and column");
    assert_eq!(edge.anchor_cell, (u16::MAX, u16::MAX));
    assert_eq!((edge.column_count, edge.row_count), (1, 1));
}

#[test]
fn a_chunked_image_header_and_empty_chunk_are_refused_exactly() {
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
    let error = serde_json::from_value::<FrameImageTransfer>(wrong_length)
        .expect_err("a transfer with a wrong byte count is refused");
    assert_eq!(
        error.to_string(),
        "image transfer byte length does not match its dimensions"
    );

    let empty_chunk = serde_json::json!({
        "image_transfer_id": 1,
        "byte_offset": 0,
        "is_last": true,
        "chunk_bytes": ""
    });
    let error = serde_json::from_value::<FrameImageChunk>(empty_chunk)
        .expect_err("an empty image chunk is refused");
    assert_eq!(error.to_string(), "image chunk must not be empty");
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

    for (width, height, byte_len) in [(0, 1, 0), (16_385, 1, 65_540), (16_384, 1_025, 67_174_400)] {
        let error = serde_json::from_value::<FrameImageTransfer>(json!({
            "image_content_id": 1,
            "image_record": {
                "protocol": "Kitty",
                "pixel_width": width,
                "pixel_height": height,
                "image_action": "Display",
                "display": FrameImageDisplay::default(),
                "anchor_cell": [0, 0]
            },
            "image_byte_count": byte_len
        }))
        .expect_err("dimensions beyond the graphics limits are refused");
        assert_eq!(error.to_string(), "image dimensions exceed graphics limits");
    }
}

#[test]
fn a_frame_encodes_to_the_shape_a_client_decodes() {
    // A client and a server only agree on a frame if both were built at this
    // shape. Add, remove or rename anything below and every client older than
    // the change stops decoding the frames it is sent.
    //
    // A default cell carries no `combining`, no `underline_color` and no set
    // attribute, so those names are absent from the encoding below.
    let plain_cell = json!({
        "character": "h",
        "cell_width": 1,
        "style": {
            "foreground_color": "Default",
            "background_color": "Default",
            "text_attributes": { "underline_style": "None" }
        }
    });
    let mut second_cell = plain_cell.clone();
    second_cell["character"] = json!("i");

    assert_eq!(
        serde_json::to_value(build_painted_frame()).expect("frame encodes"),
        json!({
            "session_snapshot": {
                "session_id": "00000000-0000-0000-0000-000000000001",
                "session_revision": 17,
                "session_name": "quiet-lake",
                "active_tab_snapshot": {
                    "tab_id": "00000000-0000-0000-0000-000000000002",
                    "tab_name": "edit",
                    "pane_slots": [{
                        "pane_id": "00000000-0000-0000-0000-000000000004",
                        "outer_rect": {
                            "origin": { "column": 0, "row": 0 },
                            "cell_size": { "column_count": 4, "row_count": 3 }
                        },
                        "content_rect": {
                            "origin": { "column": 1, "row": 1 },
                            "cell_size": { "column_count": 2, "row_count": 1 }
                        },
                        "pane_kind": "Terminal",
                        "is_visible": true,
                        "is_suppressed": false,
                        "is_dead": false
                    }],
                    "effective_cell_size": { "column_count": 4, "row_count": 3 },
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
                }]
            },
            "pane_snapshots": [{
                "pane_id": "00000000-0000-0000-0000-000000000004",
                "pane_title": "vim",
                "cursor_snapshot": {
                    "row_index": 0,
                    "column_index": 1,
                    "is_visible": true,
                    "is_blinking": false,
                    "shape": "Bar"
                },
                "terminal_window": {
                    "column_count": 2,
                    "row_snapshots": [{
                        "cell_runs": [
                            { "repeat_count": 1, "cell": plain_cell },
                            { "repeat_count": 1, "cell": second_cell }
                        ]
                    }],
                    "view_row_offset": 0
                },
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
                "is_alt_scroll_enabled": false,
                "is_on_alt_screen": false,
                "view_top_row_index": 7,
                "selection_spans": { "row_spans": [[0, 0, 1]] },
                "has_selection": true,
                "scrollback_meta": { "is_truncated": false, "retained_line_count": 12 }
            }],
            "client_snapshot": {
                "client_id": "00000000-0000-0000-0000-000000000003",
                "client_revision": 19,
                "viewport_size": { "column_count": 4, "row_count": 3 },
                "active_tab_id": "00000000-0000-0000-0000-000000000002",
                "focused_pane_id": "00000000-0000-0000-0000-000000000004",
                "lock_mode": "Normal",
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

    // Decoded from text, the way the transport does it: the frame arrives as
    // bytes on a socket, never as an already-built value.
    let decoded: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
        .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        build_painted_frame(),
        "the extra field left nothing behind in the decoded frame"
    );
}

/// A frame from a server that sends no `gap` reads as `0`, and a frame that
/// sends one reads back the value it was written with.
#[test]
fn a_frame_without_a_gap_reads_as_zero() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
    encoded_json["session_snapshot"]["active_tab_snapshot"]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("gap_cell_count")
        .expect("the tab encodes a gap");

    // Decoded from text, the way the transport does it.
    let decoded: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("a frame with no gap decodes");
    assert_eq!(
        decoded.session_snapshot.active_tab_snapshot.gap_cell_count,
        0
    );

    let mut spaced = build_painted_frame();
    spaced.session_snapshot.active_tab_snapshot.gap_cell_count = 2;
    let encoded_json = serde_json::to_value(&spaced).expect("frame encodes");
    let decoded: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("a frame with a gap decodes");
    assert_eq!(
        decoded.session_snapshot.active_tab_snapshot.gap_cell_count,
        2
    );
}

/// A value enum this build has no name for falls back to its plainest value,
/// so one unfamiliar colour or underline never costs the whole frame.
#[test]
fn a_cell_value_this_build_has_no_name_for_falls_back() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
    let style = encoded_json["pane_snapshots"][0]["terminal_window"]["row_snapshots"][0]
        ["cell_runs"][0]["cell"]["style"]
        .as_object_mut()
        .expect("a style encodes as an object");
    style.insert("foreground_color".to_string(), serde_json::json!("Neon"));
    style["text_attributes"]
        .as_object_mut()
        .expect("attributes encode as an object")
        .insert("underline_style".to_string(), serde_json::json!("Dotted2"));

    // Decoded from text, the way the transport does it.
    let decoded: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
        .expect("an unfamiliar value falls back, it does not fail");

    let cell = &decoded.pane_snapshots[0]
        .terminal_window
        .as_ref()
        .expect("the pane has a window")
        .row_snapshots[0]
        .cell_runs[0]
        .cell;
    assert_eq!(
        cell.style.foreground_color,
        FrameColor::Default,
        "a colour with no name here draws as the default colour"
    );
    assert_eq!(
        cell.style.text_attributes.underline_style,
        FrameUnderline::None,
        "an underline style with no name here draws as no underline"
    );
}

/// A frame row that soft-wrapped must arrive soft-wrapped. A viewer that reads a
/// soft wrap as a hard one breaks the logical line when its text is copied
/// out, and the wire form leaves the default off, so only the two wrapped
/// endings travel at all.
#[test]
fn a_wrapped_row_carries_its_ending_and_an_ended_row_leaves_it_off() {
    let encoded_json = |end| {
        serde_json::to_value(FrameRow::from_cells([build_blank_frame_cell()], end))
            .expect("a frame row encodes")
    };

    assert_eq!(encoded_json(FrameRowEnd::Soft)["row_end"], json!("Soft"));
    assert_eq!(
        encoded_json(FrameRowEnd::SoftWide)["row_end"],
        json!("SoftWide")
    );
    assert_eq!(encoded_json(FrameRowEnd::Hard).get("row_end"), None);
}

#[test]
fn a_row_reads_back_with_the_ending_it_was_written_with() {
    for end in [FrameRowEnd::Hard, FrameRowEnd::Soft, FrameRowEnd::SoftWide] {
        let serialized_frame_row_json =
            serde_json::to_string(&FrameRow::from_cells([build_blank_frame_cell()], end))
                .expect("a frame row encodes");

        let decoded_frame_row: FrameRow =
            serde_json::from_str(&serialized_frame_row_json).expect("a frame row decodes");

        assert_eq!(decoded_frame_row.row_end, end);
        assert_eq!(
            decoded_frame_row.expand_cells(),
            vec![build_blank_frame_cell()]
        );
    }
}

#[test]
fn a_row_ending_this_build_has_no_name_for_reads_as_hard() {
    let read: FrameRow = serde_json::from_str(r#"{"cell_runs":[],"row_end":"SoftDouble"}"#)
        .expect("an ending with no name here falls back, it does not fail");

    assert_eq!(read.row_end, FrameRowEnd::Hard);
}

/// The two optional presentation values fall back the same way the colours
/// do: to nothing at all, leaving the user's own cursor and the foreground
/// colour standing.
#[test]
fn a_cursor_shape_and_an_underline_colour_with_no_name_here_read_as_none() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
    encoded_json["pane_snapshots"][0]["cursor_snapshot"]["shape"] = json!("Beam");
    encoded_json["pane_snapshots"][0]["terminal_window"]["row_snapshots"][0]["cell_runs"][0]
        ["cell"]["style"]["underline_color"] = json!("Neon");

    // Decoded from text, the way the transport does it.
    let decoded: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
        .expect("a value with no name here falls back, it does not fail");

    assert_eq!(decoded.pane_snapshots[0].cursor_snapshot.shape, None);
    assert_eq!(
        decoded.pane_snapshots[0]
            .terminal_window
            .as_ref()
            .expect("the pane has a window")
            .row_snapshots[0]
            .cell_runs[0]
            .cell
            .style
            .underline_color,
        None
    );
}

/// A `gap` that is not a cell count — negative, or a string — reads as `0`
/// and leaves the rest of the frame intact.
#[test]
fn a_frame_whose_gap_is_not_a_count_reads_as_zero() {
    for hostile in [
        serde_json::json!(-1),
        serde_json::json!("2"),
        serde_json::json!(null),
        serde_json::json!(70_000),
    ] {
        let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
        encoded_json["session_snapshot"]["active_tab_snapshot"]["gap_cell_count"] = hostile;
        let decoded: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
            .expect("a frame with a bad gap decodes");
        assert_eq!(decoded, build_painted_frame());
    }
}

#[test]
fn a_run_of_exactly_the_cap_stays_one_run_and_one_more_cell_opens_a_second() {
    let at_cap = FrameRow::from_cells(
        std::iter::repeat_n(build_blank_frame_cell(), usize::from(u16::MAX)),
        FrameRowEnd::Hard,
    );
    let past_cap = FrameRow::from_cells(
        std::iter::repeat_n(build_blank_frame_cell(), usize::from(u16::MAX) + 1),
        FrameRowEnd::Hard,
    );

    assert_eq!(
        at_cap.cell_runs,
        vec![FrameRun {
            repeat_count: u16::MAX,
            cell: build_blank_frame_cell()
        }]
    );
    assert_eq!(
        past_cap.cell_runs,
        vec![
            FrameRun {
                repeat_count: u16::MAX,
                cell: build_blank_frame_cell()
            },
            FrameRun {
                repeat_count: 1,
                cell: build_blank_frame_cell()
            },
        ]
    );
    assert_eq!(past_cap.expand_cells().len(), usize::from(u16::MAX) + 1);
}

/// Two cells fold only when every field is equal: the same character with a
/// different grapheme cluster or a different width opens its own run.
#[test]
fn cells_equal_in_character_but_not_in_cluster_or_width_do_not_fold() {
    let plain = build_frame_cell('e', FrameColor::Default);
    let accented = FrameCell {
        combining_characters: vec!['\u{301}'],
        ..build_frame_cell('e', FrameColor::Default)
    };
    let wide = FrameCell {
        cell_width: 2,
        ..build_frame_cell('e', FrameColor::Default)
    };
    let cells = vec![plain.clone(), accented.clone(), wide.clone()];

    let frame_row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        frame_row.cell_runs,
        vec![
            FrameRun {
                repeat_count: 1,
                cell: plain
            },
            FrameRun {
                repeat_count: 1,
                cell: accented
            },
            FrameRun {
                repeat_count: 1,
                cell: wide
            },
        ]
    );
    assert_eq!(frame_row.expand_cells(), cells);
}

#[test]
fn hard_is_the_only_ending_that_is_hard() {
    assert!(FrameRowEnd::Hard.is_hard());
    assert!(!FrameRowEnd::Soft.is_hard());
    assert!(!FrameRowEnd::SoftWide.is_hard());
}

/// A run with `repeat_count: 0` never comes out of `from_cells`, but the wire can
/// carry one. It expands to no cells and leaves the other runs intact.
#[test]
fn a_run_whose_count_is_zero_expands_to_no_cells() {
    let encoded_json = json!({
        "cell_runs": [
            { "repeat_count": 0, "cell": build_frame_cell('x', FrameColor::Default) },
            { "repeat_count": 2, "cell": build_blank_frame_cell() }
        ]
    });

    // Decoded from text, the way the transport does it.
    let frame_row: FrameRow = serde_json::from_str(&encoded_json.to_string())
        .expect("a frame row with a zero-count run decodes");

    assert_eq!(
        frame_row.expand_cells(),
        vec![build_blank_frame_cell(), build_blank_frame_cell()]
    );
}

/// Every value a cell can carry beyond the plain default, pinned on the wire:
/// a grapheme cluster, a double width, palette and truecolor colors, an
/// underline color, and the attributes that are set. An attribute that is
/// not set is absent.
#[test]
fn a_dressed_cell_encodes_every_value_it_sets_and_nothing_it_does_not() {
    let dressed = FrameCell {
        character: 'e',
        combining_characters: vec!['\u{301}'],
        cell_width: 2,
        style: FrameStyle {
            foreground_color: FrameColor::Indexed(1),
            background_color: FrameColor::Rgb(0, 0, 255),
            underline_color: Some(FrameColor::Indexed(3)),
            text_attributes: FrameAttrs {
                is_bold: true,
                is_italic: false,
                is_reverse: true,
                is_faint: false,
                is_blinking: false,
                is_concealed: false,
                is_struck_through: true,
                is_overlined: false,
                underline_style: FrameUnderline::Curly,
            },
        },
    };

    let encoded_json = serde_json::to_value(&dressed).expect("a cell encodes");

    assert_eq!(
        encoded_json,
        json!({
            "character": "e",
            "combining_characters": ["\u{301}"],
            "cell_width": 2,
            "style": {
                "foreground_color": { "Indexed": 1 },
                "background_color": { "Rgb": [0, 0, 255] },
                "underline_color": { "Indexed": 3 },
                "text_attributes": {
                    "is_bold": true,
                    "is_reverse": true,
                    "is_struck_through": true,
                    "underline_style": "Curly"
                }
            }
        })
    );
    let decoded: FrameCell =
        serde_json::from_str(&encoded_json.to_string()).expect("a dressed cell decodes");
    assert_eq!(decoded, dressed);
}

/// An optional value that is `None` travels as `null`, never as an absent
/// key: a pane with no title, no window and no highlight, a slot with no
/// content rect, a cursor with no shape, and a client with no focused pane.
#[test]
fn absent_optional_values_encode_as_null() {
    let mut bare = build_painted_frame();
    bare.session_snapshot.active_tab_snapshot.pane_slots[0].content_rect = None;
    bare.pane_snapshots[0].pane_title = None;
    bare.pane_snapshots[0].cursor_snapshot.shape = None;
    bare.pane_snapshots[0].terminal_window = None;
    bare.pane_snapshots[0].selection_spans = None;
    bare.client_snapshot.focused_pane_id = None;

    let encoded_json = serde_json::to_value(&bare).expect("frame encodes");

    assert_eq!(
        encoded_json["session_snapshot"]["active_tab_snapshot"]["pane_slots"][0]["content_rect"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_json["pane_snapshots"][0]["pane_title"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_json["pane_snapshots"][0]["cursor_snapshot"]["shape"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_json["pane_snapshots"][0]["terminal_window"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_json["pane_snapshots"][0]["selection_spans"],
        serde_json::Value::Null
    );
    assert_eq!(
        encoded_json["client_snapshot"]["focused_pane_id"],
        serde_json::Value::Null
    );
    let decoded: PaintedFrame =
        serde_json::from_str(&encoded_json.to_string()).expect("a bare frame decodes");
    assert_eq!(decoded, bare);
}

/// A cursor from a server that sends no `shape` at all reads as `None`, the
/// same as one that sends `null`.
#[test]
fn a_cursor_without_a_shape_key_reads_as_no_shape() {
    let mut encoded_json = serde_json::to_value(build_painted_frame()).expect("frame encodes");
    encoded_json["pane_snapshots"][0]["cursor_snapshot"]
        .as_object_mut()
        .expect("a cursor encodes as an object")
        .remove("shape")
        .expect("the cursor encodes a shape");

    let decoded: PaintedFrame = serde_json::from_str(&encoded_json.to_string())
        .expect("a cursor with no shape key decodes");

    assert_eq!(decoded.pane_snapshots[0].cursor_snapshot.shape, None);
}
