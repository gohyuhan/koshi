//! Tests for [`PaneRecord`]: creation defaults and the serialized form.

use super::*;

/// The JSON form of a fresh terminal pane record with the nil uuid as its id.
const FRESH_TERMINAL_RECORD_JSON: &str = r#"{"pane_id":"00000000-0000-0000-0000-000000000000","spawn_spec":null,"working_directory":null}"#;

/// The `PaneId` whose uuid is all zeros.
fn build_nil_pane_id() -> PaneId {
    serde_json::from_str(r#""00000000-0000-0000-0000-000000000000""#).expect("a valid uuid")
}

#[test]
fn a_new_record_has_empty_metadata() {
    let pane_id = PaneId::new();

    let pane_record = PaneRecord::from_terminal_pane(pane_id);

    assert_eq!(pane_record.get_pane_id(), pane_id);
    assert_eq!(pane_record.spawn_spec, None);
    assert_eq!(pane_record.working_directory, None);
}

#[test]
fn a_fresh_record_serializes_under_its_field_names() {
    let pane_record = PaneRecord::from_terminal_pane(build_nil_pane_id());

    assert_eq!(
        serde_json::to_string(&pane_record).expect("serialize"),
        FRESH_TERMINAL_RECORD_JSON
    );
}

#[test]
fn a_record_deserializes_from_its_field_names() {
    let restored_pane_record: PaneRecord =
        serde_json::from_str(FRESH_TERMINAL_RECORD_JSON).expect("deserialize");

    assert_eq!(
        restored_pane_record,
        PaneRecord::from_terminal_pane(build_nil_pane_id())
    );
}

#[test]
fn a_record_ignores_an_unknown_field_when_deserializing() {
    let pane_record_json = FRESH_TERMINAL_RECORD_JSON.replacen("{", r#"{"unknown":1,"#, 1);

    let restored_pane_record: PaneRecord =
        serde_json::from_str(&pane_record_json).expect("deserialize");

    assert_eq!(
        restored_pane_record,
        PaneRecord::from_terminal_pane(build_nil_pane_id())
    );
}

/// Stored JSON that carries a `created_at` timestamp and an `exit_policy`
/// decodes to the same pane record as JSON without them.
#[test]
fn a_record_carrying_a_stored_created_at_and_exit_policy_still_deserializes() {
    let pane_record_json = FRESH_TERMINAL_RECORD_JSON.replacen(
        r#""working_directory":null"#,
        r#""exit_policy":"CloseOnExit","working_directory":null,"created_at":{"secs_since_epoch":0,"nanos_since_epoch":0}"#,
        1,
    );

    let restored_pane_record: PaneRecord =
        serde_json::from_str(&pane_record_json).expect("deserialize");

    assert_eq!(
        restored_pane_record,
        PaneRecord::from_terminal_pane(build_nil_pane_id())
    );
}

/// Stored JSON that carries an `env` map decodes to the same pane record as JSON
/// without one. `env` is not a field of `PaneRecord`, so it is skipped.
#[test]
fn a_record_carrying_a_stored_env_map_still_deserializes() {
    let pane_record_json = FRESH_TERMINAL_RECORD_JSON.replacen(
        r#""working_directory""#,
        r#""env":{"EDITOR":"vi"},"working_directory""#,
        1,
    );

    let restored_pane_record: PaneRecord =
        serde_json::from_str(&pane_record_json).expect("deserialize");

    assert_eq!(
        restored_pane_record,
        PaneRecord::from_terminal_pane(build_nil_pane_id())
    );
}

#[test]
fn a_record_without_a_pane_id_fails_to_deserialize() {
    let pane_record_json = FRESH_TERMINAL_RECORD_JSON.replacen(
        r#""pane_id":"00000000-0000-0000-0000-000000000000","#,
        "",
        1,
    );

    let deserialization_error =
        serde_json::from_str::<PaneRecord>(&pane_record_json).expect_err("missing field");

    assert_eq!(
        deserialization_error.to_string(),
        "missing field `pane_id` at line 1 column 44"
    );
}
