//! Tests for conversion of saved sessions, images, and placement errors.

use super::*;
use crate::resume::read_resume_body;

/// Every path in `migrated_json` whose value `decoded_json` does not hold at the
/// same path. `.name` extends a path into an object field and `[index]` into an
/// array item; a path stops at the first value that differs.
fn list_json_paths_lost_by_decoding(
    migrated_json: &Value,
    decoded_json: &Value,
    json_path: &str,
) -> Vec<String> {
    match (migrated_json, decoded_json) {
        (Value::Object(migrated_fields), Value::Object(decoded_fields)) => migrated_fields
            .iter()
            .flat_map(|(field_name, migrated_field)| {
                let field_path = format!("{json_path}.{field_name}");
                match decoded_fields.get(field_name) {
                    Some(decoded_field) => {
                        list_json_paths_lost_by_decoding(migrated_field, decoded_field, &field_path)
                    }
                    None => vec![field_path],
                }
            })
            .collect(),
        (Value::Array(migrated_items), Value::Array(decoded_items))
            if migrated_items.len() == decoded_items.len() =>
        {
            migrated_items
                .iter()
                .zip(decoded_items)
                .enumerate()
                .flat_map(|(item_index, (migrated_item, decoded_item))| {
                    list_json_paths_lost_by_decoding(
                        migrated_item,
                        decoded_item,
                        &format!("{json_path}[{item_index}]"),
                    )
                })
                .collect()
        }
        _ if migrated_json == decoded_json => Vec::new(),
        _ => vec![json_path.to_string()],
    }
}

#[test]
fn every_migrated_field_of_each_released_fixture_survives_decoding() {
    let mut lost_json_paths = Vec::new();
    for (source_resume_format, body_field_name, fixture_text) in [
        (2, "body", include_str!("../fixtures/format_two.json")),
        (3, "body", include_str!("../fixtures/format_three.json")),
        (4, "raw_body", include_str!("../fixtures/format_four.json")),
    ] {
        let body_field_names = get_resume_body_field_names(source_resume_format);
        let fixture_fields =
            parse_unique_json_object(fixture_text, "fixture").expect("read the fixture");
        let body_fields = parse_unique_json_object(fixture_fields[body_field_name].get(), "body")
            .expect("read the body");
        let sessions = parse_unique_json_object(
            body_fields[body_field_names.sessions_field_name].get(),
            "sessions",
        )
        .expect("read the sessions");
        for (session_key, raw_session) in sessions {
            let migrated_session_bytes =
                migrate_previous_session_json(source_resume_format, &session_key, raw_session)
                    .expect("migrate the session");
            let migrated_session: Value =
                serde_json::from_slice(&migrated_session_bytes).expect("read the migrated session");
            let decoded_session: Session = serde_json::from_slice(&migrated_session_bytes)
                .expect("decode the migrated session");
            lost_json_paths.extend(list_json_paths_lost_by_decoding(
                &migrated_session,
                &serde_json::to_value(&decoded_session).expect("encode the decoded session"),
                &format!("format {source_resume_format} session"),
            ));
        }
        let mut ancillary_pane_fields = BTreeMap::new();
        for &field_name in body_field_names.ancillary_pane_field_names {
            if let Some(raw_fields) = body_fields.get(field_name) {
                ancillary_pane_fields.insert(
                    field_name,
                    parse_unique_json_object(raw_fields.get(), field_name)
                        .expect("read the pane fields"),
                );
            }
        }
        let pane_states = parse_unique_json_object(
            body_fields[body_field_names.pane_states_field_name].get(),
            "panes",
        )
        .expect("read the pane states");
        for (pane_key, raw_pane_state) in pane_states {
            let migrated_pane_bytes =
                if source_resume_format >= OLDEST_FORMAT_WITH_CURRENT_PANE_STATE {
                    raw_pane_state.get().as_bytes().to_vec()
                } else {
                    migrate_previous_pane_json(
                        source_resume_format,
                        &pane_key,
                        raw_pane_state,
                        &ancillary_pane_fields,
                    )
                    .expect("migrate the pane")
                };
            let migrated_pane: Value =
                serde_json::from_slice(&migrated_pane_bytes).expect("read the migrated pane");
            let decoded_pane: CarriedPaneState =
                serde_json::from_slice(&migrated_pane_bytes).expect("decode the migrated pane");
            lost_json_paths.extend(list_json_paths_lost_by_decoding(
                &migrated_pane,
                &serde_json::to_value(&decoded_pane).expect("encode the decoded pane"),
                &format!("format {source_resume_format} pane"),
            ));
        }
    }

    assert_eq!(lost_json_paths, Vec::<String>::new());
}

#[test]
fn migrate_cell_extra_preserves_placeholder_and_native_image_coordinates() {
    let mut released_cell_extra = serde_json::json!({
        "combining": ['\u{0301}'],
        "image_placeholder": {
            "image_id": 7,
            "placement_id": 8,
            "row": 3,
            "column": 4,
            "image_id_msb": 1
        },
        "image_fragments": [
            {"source": 9, "row": 5, "column": 6},
            {"source": 10, "row": 7, "column": 8}
        ]
    });

    migrate_cell_extra(&mut released_cell_extra).expect("migrate released cell metadata");

    assert_eq!(
        released_cell_extra,
        serde_json::json!({
            "combining": ['\u{0301}'],
            "image_placeholder": {
                "image_id": 7,
                "placement_id": 8,
                "source_row": 3,
                "source_column": 4,
                "image_id_msb": 1
            },
            "image_fragments": [
                {"image_source_id": 9, "source_row_index": 5, "source_column_index": 6},
                {"image_source_id": 10, "source_row_index": 7, "source_column_index": 8}
            ]
        })
    );
    let mut migrated_cell = serde_json::to_value(koshi_terminal::grid::state::Cell::build_blank())
        .expect("encode a blank cell");
    migrated_cell["combining"] = released_cell_extra;
    let decoded_cell: koshi_terminal::grid::state::Cell =
        serde_json::from_value(migrated_cell.clone()).expect("decode the migrated cell");
    assert_eq!(
        serde_json::to_value(&decoded_cell).expect("encode the decoded cell"),
        migrated_cell
    );
}

#[test]
fn format_three_terminal_defaults_equal_the_current_terminal_defaults() {
    let current_terminal_fields = serde_json::to_value(
        koshi_terminal::state::TerminalState::from_pty_size(koshi_core::process::PtySize {
            row_count: 1,
            column_count: 1,
        }),
    )
    .expect("encode the current terminal defaults");

    for (field_name, default_value) in build_format_three_terminal_defaults() {
        assert_eq!(
            current_terminal_fields[field_name], default_value,
            "{field_name}"
        );
    }
}

#[test]
fn resume_readers_reject_session_ids_repeated_with_different_letter_case() {
    let fixture_json: Value = serde_json::from_str(include_str!("../fixtures/format_three.json"))
        .expect("parse released resume fixture");
    let previous_sessions = fixture_json["body"]["sessions"]
        .as_object()
        .expect("released sessions");
    assert_eq!(previous_sessions.len(), 1);
    let (session_key, previous_session) = previous_sessions.iter().next().expect("one session");
    let session_id = serde_json::from_value::<SessionId>(Value::String(session_key.clone()))
        .expect("released session id");
    let uppercase_session_key = session_key.to_ascii_uppercase();
    assert_ne!(uppercase_session_key, *session_key);
    let previous_session_json =
        serde_json::to_string(previous_session).expect("encode released session");
    let previous_body_json = format!(
        r#"{{"sessions":{{"{session_key}":{previous_session_json},"{uppercase_session_key}":{previous_session_json}}},"engines":{{}}}}"#
    );

    let previous_error = migrate_resume_body(3, &previous_body_json)
        .expect_err("equivalent released session keys must be refused");
    match previous_error {
        StorageError::Corrupt { detail } => assert_eq!(
            detail,
            format!("resume body has duplicate session id {session_id}")
        ),
        unexpected_storage_error => {
            panic!("expected corrupt resume body, got {unexpected_storage_error:?}")
        }
    }

    let migrated_body = migrate_resume_body(3, &fixture_json["body"].to_string())
        .expect("migrate released session");
    let migrated_session = &migrated_body.session_by_id[&session_id];
    let migrated_session_json =
        serde_json::to_string(migrated_session).expect("encode current session");
    let current_body_prefix = format!(
        r#"{{"session_by_id":{{"{session_key}":{migrated_session_json},"{uppercase_session_key}":"#
    );
    let duplicate_session_end_column = current_body_prefix.len() + migrated_session_json.len() + 1;
    let current_body_json = format!("{current_body_prefix}{migrated_session_json}")
        + r#"},"carried_pane_state_by_pane_id":{},"carried_quit":null}"#;
    let current_raw_body = RawValue::from_string(current_body_json).expect("current body JSON");
    let current_error = read_resume_body(RESUME_FORMAT, &current_raw_body)
        .expect_err("equivalent current session keys must be refused");
    match current_error {
        StorageError::Corrupt { detail } => assert_eq!(
            detail,
            format!(
                "resume body is unreadable: duplicate session id {session_id} at line 1 column {duplicate_session_end_column}"
            )
        ),
        unexpected_storage_error => {
            panic!("expected corrupt resume body, got {unexpected_storage_error:?}")
        }
    }
}

#[test]
fn migrate_resume_body_rejects_duplicate_session_map_fields() {
    let previous_body = r#"{"sessions":{},"sessions":{},"engines":{}}"#;

    let migration_error =
        migrate_resume_body(3, previous_body).expect_err("duplicate sessions field is invalid");

    match migration_error {
        StorageError::Corrupt { detail } => {
            assert_eq!(detail, "resume body has duplicate field body.sessions");
        }
        unexpected_storage_error => {
            panic!("expected corrupt resume body, got {unexpected_storage_error:?}")
        }
    }
}

#[test]
fn migrate_decoded_image_keeps_large_byte_array_opaque_until_serialization() {
    let image_bytes = format!("[{}]", vec!["255"; 1024 * 1024].join(","));
    let previous_image = format!(r#"{{"width":512,"height":512,"rgba":{image_bytes}}}"#);
    let mut opaque_byte_arrays = Vec::new();
    let mut decoded_image = parse_json_fragment(&previous_image, &mut opaque_byte_arrays)
        .expect("parse released decoded image");

    assert_eq!(opaque_byte_arrays.len(), 1);
    assert_eq!(opaque_byte_arrays[0].get(), image_bytes);
    assert_eq!(decoded_image["rgba"], serde_json::json!([0]));
    migrate_decoded_image(&mut decoded_image).expect("migrate decoded image fields");

    let migrated_image = serde_json::to_string(&JsonWithOpaqueByteArrays {
        json_value: &decoded_image,
        opaque_byte_arrays: &opaque_byte_arrays,
    })
    .expect("serialize decoded image with raw bytes");
    assert_eq!(
        migrated_image,
        format!(r#"{{"pixel_height":512,"pixel_width":512,"rgba_bytes":{image_bytes}}}"#)
    );
}

#[test]
fn migrate_nested_graphics_error_keeps_its_protocol_and_placement_reason() {
    let mut previous_error = serde_json::json!({
        "PlacementRejected": {
            "protocol": "Kitty",
            "reason": "NoParent"
        }
    });
    migrate_graphics_error(&mut previous_error).expect("migrate nested graphics error");
    let current_error = serde_json::json!({
        "PlacementRejected": {
            "protocol": "Kitty",
            "placement_error": "ParentNotFound"
        }
    });
    assert_eq!(previous_error, current_error);
    serde_json::from_value::<koshi_terminal::graphics::GraphicsError>(previous_error)
        .expect("current graphics error decodes");
}

#[test]
fn migrate_every_previous_image_placement_error_to_its_current_shape() {
    let error_shapes = [
        (
            serde_json::json!("UnsupportedPlacement"),
            serde_json::json!("UnsupportedPlacement"),
        ),
        (
            serde_json::json!("NoParent"),
            serde_json::json!("ParentNotFound"),
        ),
        (
            serde_json::json!("RelativeCycle"),
            serde_json::json!("RelativeCycle"),
        ),
        (
            serde_json::json!("RelativeDepth"),
            serde_json::json!("RelativeDepth"),
        ),
        (
            serde_json::json!("VirtualRelative"),
            serde_json::json!("VirtualRelative"),
        ),
        (
            serde_json::json!({"RelativeOffsetOutOfBounds": {"row": 3, "column": 4}}),
            serde_json::json!({"RelativeOffsetOutOfBounds": {"resolved_row": 3, "resolved_column": 4}}),
        ),
        (
            serde_json::json!({"AnimationFrameNotFound": {"frame": 5}}),
            serde_json::json!({"AnimationFrameNotFound": {"frame_index": 5}}),
        ),
        (
            serde_json::json!("AnimationDataInvalid"),
            serde_json::json!("InvalidAnimationData"),
        ),
        (
            serde_json::json!({"ImageNotFound": {"id": 6, "number": 7}}),
            serde_json::json!({"ImageNotFound": {"image_id": 6, "image_number": 7}}),
        ),
        (
            serde_json::json!({"MissingCellDimensions": {"width": null, "height": null}}),
            serde_json::json!({"MissingCellDimensions": {"requested_width": null, "requested_height": null}}),
        ),
        (
            serde_json::json!({"UnsupportedCellDimensions": {"width": null, "height": null}}),
            serde_json::json!({"UnsupportedCellDimensions": {"requested_width": null, "requested_height": null}}),
        ),
        (
            serde_json::json!({"ZeroSize": {"columns": 8, "rows": 9}}),
            serde_json::json!({"ZeroSize": {"column_count": 8, "row_count": 9}}),
        ),
        (
            serde_json::json!({"SourceOutOfBounds": {"x": 1, "y": 2, "width": 3, "height": 4, "image_width": 5, "image_height": 6}}),
            serde_json::json!({"SourceOutOfBounds": {"source_x": 1, "source_y": 2, "source_pixel_width": 3, "source_pixel_height": 4, "image_pixel_width": 5, "image_pixel_height": 6}}),
        ),
        (
            serde_json::json!({"DimensionsTooLarge": {"columns": 8, "rows": 9}}),
            serde_json::json!({"DimensionsTooLarge": {"column_count": 8, "row_count": 9}}),
        ),
        (
            serde_json::json!({"OutOfBounds": {"row": 1, "column": 2, "columns": 3, "rows": 4, "grid_rows": 5, "grid_columns": 6}}),
            serde_json::json!({"OutOfBounds": {"anchor_row": 1, "anchor_column": 2, "column_count": 3, "row_count": 4, "grid_rows": 5, "grid_columns": 6}}),
        ),
        (
            serde_json::json!({"HistoryOutOfBounds": {"row": 1, "column": 2, "columns": 3, "rows": 4, "first_row": 5, "retained_end": 6}}),
            serde_json::json!({"HistoryOutOfBounds": {"anchor_row": 1, "anchor_column": 2, "column_count": 3, "row_count": 4, "first_row": 5, "retained_end": 6}}),
        ),
        (
            serde_json::json!({"HistoryWidthOutOfBounds": {"row": 1, "column": 2, "columns": 3, "grid_columns": 4}}),
            serde_json::json!({"HistoryWidthOutOfBounds": {"anchor_row": 1, "anchor_column": 2, "column_count": 3, "grid_columns": 4}}),
        ),
        (
            serde_json::json!({"HistoryRangeOverflow": {"total_pushed": 7, "grid_rows": 8}}),
            serde_json::json!({"HistoryRangeOverflow": {"total_pushed_row_count": 7, "grid_rows": 8}}),
        ),
        (
            serde_json::json!({"HistoryRowsExceedCounter": {"retained_rows": 7, "total_pushed": 8}}),
            serde_json::json!({"HistoryRowsExceedCounter": {"retained_row_count": 7, "total_pushed_row_count": 8}}),
        ),
        (
            serde_json::json!("IdentityExhausted"),
            serde_json::json!("IdentityExhausted"),
        ),
        (
            serde_json::json!({"TooManyPlacements": {"count": 7, "limit": 8}}),
            serde_json::json!({"TooManyPlacements": {"placement_count": 7, "placement_limit": 8}}),
        ),
        (
            serde_json::json!({"StorageLimit": {"used_bytes": 7, "requested_bytes": 8, "limit_bytes": 9}}),
            serde_json::json!({"StorageLimit": {"used_byte_count": 7, "requested_byte_count": 8, "byte_limit": 9}}),
        ),
    ];
    for (mut previous_error, current_error) in error_shapes {
        migrate_image_placement_error(&mut previous_error).expect("migrate placement error");
        assert_eq!(previous_error, current_error);
        serde_json::from_value::<koshi_terminal::graphics::ImagePlacementError>(previous_error)
            .expect("current placement error decodes");
    }
}

#[test]
fn migrate_image_display_adds_the_relative_and_suppression_fields_it_lacks() {
    let mut previous_display = serde_json::json!({"z_index": 2});

    migrate_image_display(&mut previous_display).expect("migrate the image display");

    assert_eq!(
        previous_display,
        serde_json::json!({
            "z_index": 2,
            "relative_image_id": null,
            "relative_placement_id": null,
            "relative_column_offset": 0,
            "relative_row_offset": 0,
            "response_suppression_level": 0
        })
    );
}

#[test]
fn migrate_image_display_keeps_the_relative_and_suppression_values_it_has() {
    let mut previous_display = serde_json::json!({
        "relative_image_id": 7,
        "relative_placement_id": 8,
        "relative_offset_x": -3,
        "relative_offset_y": 4,
        "quiet": 2
    });

    migrate_image_display(&mut previous_display).expect("migrate the image display");

    assert_eq!(
        previous_display,
        serde_json::json!({
            "relative_image_id": 7,
            "relative_placement_id": 8,
            "relative_column_offset": -3,
            "relative_row_offset": 4,
            "response_suppression_level": 2
        })
    );
}

#[test]
fn migrate_animation_marks_a_frame_without_gapless_as_not_gapless() {
    let mut previous_animation = serde_json::json!({
        "frames": [
            {"image": {"width": 1, "height": 1, "rgba": [0, 0, 0, 0]}, "delay": 5},
            {"image": {"width": 1, "height": 1, "rgba": [0, 0, 0, 0]}, "delay": 5, "gapless": true}
        ]
    });

    migrate_animation(&mut previous_animation).expect("migrate the animation");

    assert_eq!(
        previous_animation,
        serde_json::json!({
            "frames": [
                {
                    "decoded_image": {"pixel_width": 1, "pixel_height": 1, "rgba_bytes": [0, 0, 0, 0]},
                    "frame_delay": 5,
                    "is_gapless": false
                },
                {
                    "decoded_image": {"pixel_width": 1, "pixel_height": 1, "rgba_bytes": [0, 0, 0, 0]},
                    "frame_delay": 5,
                    "is_gapless": true
                }
            ]
        })
    );
}

#[test]
fn migrate_resume_body_reads_each_format_by_its_own_field_names() {
    for (source_resume_format, resume_body, expected_detail) in [
        (3, r#"{"engines":{}}"#, "body.sessions is missing"),
        (3, r#"{"sessions":{}}"#, "body.engines is missing"),
        (
            4,
            r#"{"sessions":{},"engines":{}}"#,
            "body.session_by_id is missing",
        ),
        (
            4,
            r#"{"session_by_id":{}}"#,
            "body.carried_pane_state_by_pane_id is missing",
        ),
        (
            4,
            r#"{"session_by_id":{},"session_by_id":{},"carried_pane_state_by_pane_id":{}}"#,
            "resume body has duplicate field body.session_by_id",
        ),
    ] {
        match migrate_resume_body(source_resume_format, resume_body) {
            Err(StorageError::Corrupt { detail }) => {
                assert_eq!(detail, expected_detail, "{resume_body}");
            }
            unexpected_result => {
                panic!("expected a corrupt body for {resume_body}, got {unexpected_result:?}")
            }
        }
    }
}

#[test]
fn migrate_resume_body_reads_a_format_four_carried_quit() {
    let migrated_body = migrate_resume_body(
        4,
        r#"{"session_by_id":{},"carried_pane_state_by_pane_id":{},"carried_quit":"Immediate"}"#,
    )
    .expect("an empty format-4 body migrates");

    assert_eq!(migrated_body.carried_quit, Some(CarriedQuit::Immediate));
    assert!(migrated_body.session_by_id.is_empty());
    assert!(migrated_body.carried_pane_state_by_pane_id.is_empty());
}

#[test]
fn migrate_resume_body_refuses_format_four_session_ids_repeated_with_different_letter_case() {
    let fixture_json: Value = serde_json::from_str(include_str!("../fixtures/format_four.json"))
        .expect("parse released resume fixture");
    let saved_sessions = fixture_json["raw_body"]["session_by_id"]
        .as_object()
        .expect("released sessions");
    assert_eq!(saved_sessions.len(), 1);
    let (session_key, saved_session) = saved_sessions.iter().next().expect("one session");
    let session_id = serde_json::from_value::<SessionId>(Value::String(session_key.clone()))
        .expect("released session id");
    let uppercase_session_key = session_key.to_ascii_uppercase();
    assert_ne!(uppercase_session_key, *session_key);
    let saved_session_json = serde_json::to_string(saved_session).expect("encode released session");
    let saved_body_json = format!(
        r#"{{"session_by_id":{{"{session_key}":{saved_session_json},"{uppercase_session_key}":{saved_session_json}}},"carried_pane_state_by_pane_id":{{}}}}"#
    );

    match migrate_resume_body(4, &saved_body_json) {
        Err(StorageError::Corrupt { detail }) => assert_eq!(
            detail,
            format!("resume body has duplicate session id {session_id}")
        ),
        unexpected_result => panic!("expected a corrupt body, got {unexpected_result:?}"),
    }
}

#[test]
fn step_four_adds_the_missing_floating_fields_and_keeps_the_ones_present() {
    let mut resume_body = serde_json::json!({
        "session_by_id": {
            "first": {
                "session_name": "carried",
                "clients": {"client_by_id": {
                    "fresh": {"label": "C-fresh"},
                    "kept": {"label": "C-kept", "floating_pane_focus_order": ["kept-pane"]}
                }}
            },
            "second": {
                "floating_set": {"members": ["kept-member"]},
                "clients": {"client_by_id": {}}
            }
        },
        "carried_pane_state_by_pane_id": {}
    });

    migrate_resume_four_to_five(&mut resume_body).expect("step 4 converts the body");

    assert_eq!(
        resume_body,
        serde_json::json!({
            "session_by_id": {
                "first": {
                    "session_name": "carried",
                    "floating_set": {"members": []},
                    "clients": {"client_by_id": {
                        "fresh": {
                            "label": "C-fresh",
                            "floating_pane_view_by_pane_id": {},
                            "floating_pane_focus_order": [],
                            "focused_floating_pane_id": null
                        },
                        "kept": {
                            "label": "C-kept",
                            "floating_pane_view_by_pane_id": {},
                            "floating_pane_focus_order": ["kept-pane"],
                            "focused_floating_pane_id": null
                        }
                    }}
                },
                "second": {
                    "floating_set": {"members": ["kept-member"]},
                    "clients": {"client_by_id": {}}
                }
            },
            "carried_pane_state_by_pane_id": {}
        })
    );
}

#[test]
fn step_four_names_the_missing_or_misshapen_field() {
    for (resume_body, expected_detail) in [
        (serde_json::json!({}), "body.session_by_id is missing"),
        (
            serde_json::json!({"session_by_id": 7}),
            "body.session_by_id must be an object",
        ),
        (
            serde_json::json!({"session_by_id": {"S": 7}}),
            "body.session_by_id.S must be an object",
        ),
        (
            serde_json::json!({"session_by_id": {"S": {}}}),
            "body.session_by_id.S.clients is missing",
        ),
        (
            serde_json::json!({"session_by_id": {"S": {"clients": 7}}}),
            "session.clients must be an object",
        ),
        (
            serde_json::json!({"session_by_id": {"S": {"clients": {}}}}),
            "session.clients.client_by_id is missing",
        ),
        (
            serde_json::json!({"session_by_id": {"S": {"clients": {"client_by_id": {"C": 7}}}}}),
            "session.clients.client_by_id.C must be an object",
        ),
    ] {
        let mut resume_body = resume_body;
        match migrate_resume_four_to_five(&mut resume_body) {
            Err(StorageError::Corrupt { detail }) => assert_eq!(detail, expected_detail),
            unexpected_result => panic!("expected a corrupt body, got {unexpected_result:?}"),
        }
    }
}
