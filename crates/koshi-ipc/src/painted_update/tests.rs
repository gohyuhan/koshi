//! Tests for the painted update format: the exact bytes of a small update,
//! a round trip of every field, an update cut into parts at row boundaries,
//! the two encode refusals, and each decode error. A part cut at any byte
//! reads as truncated.

use koshi_core::ids::PaneId;
use uuid::Uuid;

use crate::frame::tests::build_painted_frame;

use super::*;

fn build_pane_id() -> PaneId {
    PaneId::from_uuid(Uuid::from_u128(4))
}

/// The header of a 3×3 pane whose visible cursor sits at row 1, column 2,
/// with no shape, at the live bottom of an empty history.
fn build_window_pane_header() -> PaneHeader {
    PaneHeader {
        cursor: PaneCursor {
            row_index: 1,
            column_index: 2,
            is_visible: true,
            is_blinking: false,
            shape: None,
        },
        view_top_row_index: 0,
        retained_line_count: 0,
        pane_window: Some(PaneWindow {
            column_count: 3,
            row_count: 3,
            view_row_offset: 0,
            is_reset: false,
        }),
    }
}

/// The payload of the module doc's keystroke update: sequence 7, the JSON
/// kept, and `keep 1`, `new 02 aa bb`, `keep 1` for the 3×3 pane.
fn build_keystroke_payload() -> Vec<u8> {
    let mut payload_bytes = vec![PAINTED_UPDATE_FRAME_KIND, 0x03, 0x07, 0x00, 0x01];
    payload_bytes.extend_from_slice(build_pane_id().get_uuid().as_bytes());
    payload_bytes.extend_from_slice(&[
        0x03, 0x01, 0x02, 0x01, 0x00, 0x00, 0x00, 0x03, 0x03, 0x00, 0x03, 0x00, 0x01, 0x02, 0x02,
        0xaa, 0xbb, 0x00, 0x01,
    ]);
    payload_bytes
}

/// A placeholder encoded row of `row_byte_count` bytes: its body byte count,
/// then that many bytes of `row_marker`.
fn build_encoded_row(row_byte_count: usize, row_marker: u8) -> Vec<u8> {
    let mut encoded_row_bytes = vec![u8::try_from(row_byte_count - 1).expect("a short row")];
    encoded_row_bytes.resize(row_byte_count, row_marker);
    encoded_row_bytes
}

fn decode_painted_update_part(payload_bytes: &[u8]) -> PaintedUpdatePart {
    match decode_session_binary_frame(payload_bytes.to_vec()) {
        Ok(SessionBinaryFrame::PaintedUpdatePart(painted_update_part)) => painted_update_part,
        other_result => panic!("expected a painted update part, got {other_result:?}"),
    }
}

#[test]
fn a_keystroke_update_encodes_to_the_exact_bytes_of_the_module_example() {
    let new_row_bytes = [0x02, 0xaa, 0xbb];
    let pane_update = PaneUpdate {
        pane_id: build_pane_id(),
        pane_header: build_window_pane_header(),
        row_instructions: vec![
            RowInstruction::Keep { row_count: 1 },
            RowInstruction::New {
                encoded_row_bytes: &new_row_bytes[..],
            },
            RowInstruction::Keep { row_count: 1 },
        ],
    };

    let part_payloads =
        encode_painted_update(7, None, PlacementPreviewUpdate::Unchanged, &[pane_update])
            .expect("the update fits");

    assert_eq!(part_payloads, vec![build_keystroke_payload()]);
}

#[test]
fn an_update_reads_back_every_field_it_was_written_with() {
    let structure_json_bytes = serde_json::to_vec(&build_painted_frame()).expect("encodes");
    let new_row_bytes = build_encoded_row(5, 0x61);
    let window_pane_header = PaneHeader {
        cursor: PaneCursor {
            row_index: 40_000,
            column_index: 300,
            is_visible: false,
            is_blinking: true,
            shape: Some(PaneCursorShape::Underline),
        },
        view_top_row_index: u64::MAX,
        retained_line_count: 10_000,
        pane_window: Some(PaneWindow {
            column_count: u16::MAX,
            row_count: 4,
            view_row_offset: 12,
            is_reset: true,
        }),
    };
    let bare_pane_header = PaneHeader {
        cursor: PaneCursor {
            row_index: 0,
            column_index: 0,
            is_visible: true,
            is_blinking: false,
            shape: Some(PaneCursorShape::Bar),
        },
        view_top_row_index: 3,
        retained_line_count: 3,
        pane_window: None,
    };
    let bare_pane_id = PaneId::from_uuid(Uuid::from_u128(9));
    let pane_updates = [
        PaneUpdate {
            pane_id: build_pane_id(),
            pane_header: window_pane_header,
            row_instructions: vec![
                RowInstruction::Keep { row_count: 2 },
                RowInstruction::Copy {
                    source_row_index: 3,
                    row_count: 1,
                },
                RowInstruction::New {
                    encoded_row_bytes: new_row_bytes.as_slice(),
                },
            ],
        },
        PaneUpdate {
            pane_id: bare_pane_id,
            pane_header: bare_pane_header,
            row_instructions: Vec::new(),
        },
    ];

    let part_payloads = encode_painted_update(
        u64::MAX,
        Some(&structure_json_bytes),
        PlacementPreviewUpdate::Unchanged,
        &pane_updates,
    )
    .expect("the update fits");

    assert_eq!(part_payloads.len(), 1);
    assert_eq!(
        decode_painted_update_part(&part_payloads[0]),
        PaintedUpdatePart {
            update_opening: Some(PaintedUpdateOpening {
                update_sequence: u64::MAX,
                painted_frame: Some(Box::new(build_painted_frame())),
                placement_preview_update: PlacementPreviewUpdate::Unchanged,
            }),
            is_last_part: true,
            pane_sections: vec![
                PaneSection {
                    pane_id: build_pane_id(),
                    pane_header: Some(window_pane_header),
                    row_instructions: vec![
                        RowInstruction::Keep { row_count: 2 },
                        RowInstruction::Copy {
                            source_row_index: 3,
                            row_count: 1,
                        },
                        RowInstruction::New {
                            encoded_row_bytes: new_row_bytes.clone(),
                        },
                    ],
                },
                PaneSection {
                    pane_id: bare_pane_id,
                    pane_header: Some(bare_pane_header),
                    row_instructions: Vec::new(),
                },
            ],
        }
    );
}

#[test]
fn an_update_over_one_part_splits_between_rows_and_goes_on_in_a_section_with_no_header() {
    let encoded_rows: Vec<Vec<u8>> = (0..6u8)
        .map(|row_index| build_encoded_row(40, row_index))
        .collect();
    let pane_update = PaneUpdate {
        pane_id: build_pane_id(),
        pane_header: build_window_pane_header(),
        row_instructions: encoded_rows
            .iter()
            .map(|encoded_row_bytes| RowInstruction::New {
                encoded_row_bytes: encoded_row_bytes.as_slice(),
            })
            .collect(),
    };

    let part_payloads = encode_painted_update_in_parts(
        3,
        None,
        PlacementPreviewUpdate::Unchanged,
        &[pane_update],
        140,
    )
    .expect("each row fits a part");

    assert_eq!(
        part_payloads
            .iter()
            .map(|part_payload| part_payload.len())
            .collect::<Vec<_>>(),
        vec![114, 103, 103]
    );
    let build_new_rows = |first_row_index: usize| {
        encoded_rows[first_row_index..first_row_index + 2]
            .iter()
            .map(|encoded_row_bytes| RowInstruction::New {
                encoded_row_bytes: encoded_row_bytes.clone(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        part_payloads
            .iter()
            .map(|part_payload| decode_painted_update_part(part_payload))
            .collect::<Vec<_>>(),
        vec![
            PaintedUpdatePart {
                update_opening: Some(PaintedUpdateOpening {
                    update_sequence: 3,
                    painted_frame: None,
                    placement_preview_update: PlacementPreviewUpdate::Unchanged,
                }),
                is_last_part: false,
                pane_sections: vec![PaneSection {
                    pane_id: build_pane_id(),
                    pane_header: Some(build_window_pane_header()),
                    row_instructions: build_new_rows(0),
                }],
            },
            PaintedUpdatePart {
                update_opening: None,
                is_last_part: false,
                pane_sections: vec![PaneSection {
                    pane_id: build_pane_id(),
                    pane_header: None,
                    row_instructions: build_new_rows(2),
                }],
            },
            PaintedUpdatePart {
                update_opening: None,
                is_last_part: true,
                pane_sections: vec![PaneSection {
                    pane_id: build_pane_id(),
                    pane_header: None,
                    row_instructions: build_new_rows(4),
                }],
            },
        ]
    );
}

#[test]
fn json_that_does_not_fit_one_part_is_refused() {
    let structure_json_bytes = [b' '; 50];

    assert_eq!(
        encode_painted_update_in_parts(
            1,
            Some(&structure_json_bytes),
            PlacementPreviewUpdate::Unchanged,
            &[],
            60,
        ),
        Err(PaintedUpdateEncodeError::StructureTooLarge {
            structure_byte_count: 50,
            placement_preview_byte_count: 0,
        })
    );
}

#[test]
fn a_row_that_does_not_fit_one_part_is_refused() {
    let encoded_row_bytes = build_encoded_row(50, 0x61);
    let pane_update = PaneUpdate {
        pane_id: build_pane_id(),
        pane_header: build_window_pane_header(),
        row_instructions: vec![RowInstruction::New {
            encoded_row_bytes: encoded_row_bytes.as_slice(),
        }],
    };

    assert_eq!(
        encode_painted_update_in_parts(
            1,
            None,
            PlacementPreviewUpdate::Unchanged,
            &[pane_update],
            60,
        ),
        Err(PaintedUpdateEncodeError::RowTooLarge { row_byte_count: 50 })
    );
}

#[test]
fn a_binary_frame_of_a_kind_this_build_does_not_know_reads_as_unknown() {
    assert_eq!(
        decode_session_binary_frame(vec![9, 1, 2]),
        Ok(SessionBinaryFrame::Unknown { frame_kind: 9 })
    );
}

#[test]
fn a_cursor_shape_code_this_build_does_not_know_reads_as_no_shape() {
    let mut payload_bytes = build_keystroke_payload();
    let shape_byte_index = 5 + 16 + 4;
    assert_eq!(payload_bytes[shape_byte_index], 0x00);
    payload_bytes[shape_byte_index] = 9;

    let painted_update_part = decode_painted_update_part(&payload_bytes);

    assert_eq!(
        painted_update_part.pane_sections[0]
            .pane_header
            .map(|pane_header| pane_header.cursor.shape),
        Some(None)
    );
}

#[test]
fn each_malformed_part_names_its_fault() {
    let keystroke_payload = build_keystroke_payload();
    let section_flags_index = 5 + 16;
    let build_payload_with_byte = |byte_index: usize, byte_value: u8| {
        let mut payload_bytes = keystroke_payload.clone();
        payload_bytes[byte_index] = byte_value;
        payload_bytes
    };
    let mut wide_cursor_row = keystroke_payload[..section_flags_index + 1].to_vec();
    wide_cursor_row.extend_from_slice(&[0xf0, 0xa2, 0x04]);
    wide_cursor_row.extend_from_slice(&keystroke_payload[section_flags_index + 2..]);
    let mut trailing_payload = keystroke_payload.clone();
    trailing_payload.push(0x00);
    let mut malformed_structure = vec![PAINTED_UPDATE_FRAME_KIND, 0x03, 0x01, 0x01, b'{'];
    malformed_structure.push(0x00);
    let mut continuing_section = vec![PAINTED_UPDATE_FRAME_KIND, 0x02, 0x01];
    continuing_section.extend_from_slice(build_pane_id().get_uuid().as_bytes());
    continuing_section.extend_from_slice(&[0x02, 0x00]);
    let keep_index = keystroke_payload.len() - 8;
    let mut eleven_byte_sequence = vec![PAINTED_UPDATE_FRAME_KIND, 0x03];
    eleven_byte_sequence.extend_from_slice(&[0x80; 10]);
    eleven_byte_sequence.push(0x00);

    let malformed_part_cases: Vec<(Vec<u8>, SessionBinaryFrameDecodeError)> = vec![
        (Vec::new(), SessionBinaryFrameDecodeError::MissingFrameKind),
        (
            build_payload_with_byte(1, 0x0b),
            SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "part_flags",
                flag_bits: 0x0b,
            },
        ),
        (
            continuing_section,
            SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "section_flags",
                flag_bits: 0x02,
            },
        ),
        (
            build_payload_with_byte(section_flags_index, 0x05),
            SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "section_flags",
                flag_bits: 0x05,
            },
        ),
        (
            build_payload_with_byte(section_flags_index + 3, 0x04),
            SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "cursor_flags",
                flag_bits: 0x04,
            },
        ),
        (
            wide_cursor_row,
            SessionBinaryFrameDecodeError::ValueOutOfRange {
                field_name: "cursor_row_index",
                decoded_value: 70_000,
            },
        ),
        (
            build_payload_with_byte(keep_index, 0x03),
            SessionBinaryFrameDecodeError::UnknownRowInstruction { instruction_tag: 3 },
        ),
        (
            build_payload_with_byte(keep_index + 1, 0x00),
            SessionBinaryFrameDecodeError::ValueOutOfRange {
                field_name: "row_count",
                decoded_value: 0,
            },
        ),
        (
            keystroke_payload[..keystroke_payload.len() - 1].to_vec(),
            SessionBinaryFrameDecodeError::Truncated,
        ),
        (
            eleven_byte_sequence,
            SessionBinaryFrameDecodeError::VarintTooLong,
        ),
        (
            trailing_payload,
            SessionBinaryFrameDecodeError::TrailingBytes {
                trailing_byte_count: 1,
            },
        ),
        (
            malformed_structure,
            SessionBinaryFrameDecodeError::MalformedStructure {
                error_detail: "EOF while parsing an object at line 1 column 1".to_string(),
            },
        ),
    ];

    for (payload_bytes, expected_error) in malformed_part_cases {
        assert_eq!(
            decode_session_binary_frame(payload_bytes.clone()),
            Err(expected_error),
            "payload {payload_bytes:02x?}"
        );
    }
    assert_eq!(
        SessionBinaryFrameDecodeError::VarintTooLong.to_string(),
        "a varint of the painted update runs past 10 bytes or past the range of a u64"
    );
}

#[test]
fn a_part_cut_at_any_byte_reads_as_truncated() {
    let keystroke_payload = build_keystroke_payload();

    for kept_byte_count in 1..keystroke_payload.len() {
        assert_eq!(
            decode_session_binary_frame(keystroke_payload[..kept_byte_count].to_vec()),
            Err(SessionBinaryFrameDecodeError::Truncated),
            "the first {kept_byte_count} bytes"
        );
    }
}

#[test]
fn an_update_carries_a_replaced_or_cleared_placement_preview_and_reads_it_back() {
    let placement_preview = PanePlacementPreview::Available(Box::new(
        crate::placement::tests::build_cross_tab_test_snapshot(),
    ));
    let placement_preview_json_bytes =
        serde_json::to_vec(&placement_preview).expect("the preview encodes");

    let replaced_payloads = encode_painted_update(
        2,
        None,
        PlacementPreviewUpdate::Replaced(&placement_preview_json_bytes),
        &[],
    )
    .expect("the update fits");
    let cleared_payloads = encode_painted_update(3, None, PlacementPreviewUpdate::Cleared, &[])
        .expect("the update fits");

    assert_eq!(replaced_payloads.len(), 1);
    assert_eq!(replaced_payloads[0][1], 0x07);
    assert_eq!(
        decode_painted_update_part(&replaced_payloads[0]).update_opening,
        Some(PaintedUpdateOpening {
            update_sequence: 2,
            painted_frame: None,
            placement_preview_update: PlacementPreviewUpdate::Replaced(Box::new(placement_preview)),
        })
    );
    assert_eq!(
        cleared_payloads,
        vec![vec![
            PAINTED_UPDATE_FRAME_KIND,
            0x07,
            0x03,
            0x00,
            0x00,
            0x00
        ]]
    );
    assert_eq!(
        decode_painted_update_part(&cleared_payloads[0])
            .update_opening
            .map(|update_opening| update_opening.placement_preview_update),
        Some(PlacementPreviewUpdate::Cleared)
    );
}

#[test]
fn a_placement_preview_that_does_not_fit_one_part_is_refused() {
    let placement_preview_json_bytes = [b' '; 50];

    assert_eq!(
        encode_painted_update_in_parts(
            1,
            None,
            PlacementPreviewUpdate::Replaced(&placement_preview_json_bytes),
            &[],
            60,
        ),
        Err(PaintedUpdateEncodeError::StructureTooLarge {
            structure_byte_count: 0,
            placement_preview_byte_count: 50,
        })
    );
}

#[test]
fn a_placement_preview_flag_on_a_part_that_opens_no_update_is_refused() {
    assert_eq!(
        decode_session_binary_frame(vec![PAINTED_UPDATE_FRAME_KIND, 0x06, 0x00]),
        Err(SessionBinaryFrameDecodeError::UnknownFlags {
            field_name: "part_flags",
            flag_bits: 0x06,
        })
    );
}

#[test]
fn a_placement_preview_that_does_not_decode_is_refused() {
    let payload_bytes = vec![
        PAINTED_UPDATE_FRAME_KIND,
        0x07,
        0x01,
        0x00,
        0x01,
        b'{',
        0x00,
    ];

    assert!(matches!(
        decode_session_binary_frame(payload_bytes),
        Err(SessionBinaryFrameDecodeError::MalformedStructure { .. })
    ));
}
