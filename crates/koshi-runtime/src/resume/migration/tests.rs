//! Tests for conversion of saved sessions, images, and placement errors.

use super::*;
use crate::resume::read_resume_body;

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
