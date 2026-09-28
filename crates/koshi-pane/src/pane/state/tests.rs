//! Tests for the pane metadata pane record: creation defaults, lifecycle
//! ownership through `update_lifecycle`, and the serialized form.

use koshi_core::ids::PaneId;

use super::PaneRecord;
use crate::error::InvalidTransitionError;
use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::policy::PaneClosePolicy;

/// The JSON form of a fresh terminal pane record with the nil uuid as its id.
const FRESH_TERMINAL_RECORD_JSON: &str = r#"{"pane_id":"00000000-0000-0000-0000-000000000000","spawn_spec":null,"working_directory":null,"close_policy":{"Graceful":{"timeout_duration":3}},"lifecycle":"Spawning"}"#;

/// The `PaneId` whose uuid is all zeros.
fn build_nil_pane_id() -> PaneId {
    serde_json::from_str(r#""00000000-0000-0000-0000-000000000000""#).expect("a valid uuid")
}

#[test]
fn a_new_record_starts_spawning_with_empty_metadata() {
    let pane_id = PaneId::new();

    let pane_record = PaneRecord::from_terminal_pane(pane_id);

    assert_eq!(pane_record.get_pane_id(), pane_id);
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Spawning);
    assert_eq!(pane_record.spawn_spec, None);
    assert_eq!(pane_record.working_directory, None);
    assert_eq!(pane_record.close_policy, PaneClosePolicy::default());
}

#[test]
fn a_rejected_lifecycle_event_leaves_the_record_unchanged() {
    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new());
    pane_record.working_directory = Some(std::path::PathBuf::from("/workspace"));
    pane_record.close_policy = PaneClosePolicy::Force;
    let pane_record_before_rejection = pane_record.clone();

    // `Cleaned` is illegal from `Spawning`: the pane record reports the rejection…
    let rejected = pane_record.update_lifecycle(PaneLifecycleEvent::Cleaned);

    assert_eq!(
        rejected,
        Err(InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Spawning,
            lifecycle_event: PaneLifecycleEvent::Cleaned,
        })
    );
    // …and keeps every field exactly where it was.
    assert_eq!(pane_record, pane_record_before_rejection);
}

#[test]
fn an_accepted_lifecycle_event_advances_the_record() {
    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new());

    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");

    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Running);
}

#[test]
fn a_record_accepts_a_legal_event_after_a_rejected_one() {
    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new());

    let rejected = pane_record.update_lifecycle(PaneLifecycleEvent::Cleaned);
    let accepted = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);

    assert_eq!(
        rejected,
        Err(InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Spawning,
            lifecycle_event: PaneLifecycleEvent::Cleaned,
        })
    );
    assert_eq!(accepted, Ok(()));
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Running);
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
        r#""lifecycle":"Spawning""#,
        r#""exit_policy":"CloseOnExit","lifecycle":"Spawning","created_at":{"secs_since_epoch":0,"nanos_since_epoch":0}"#,
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
        r#""lifecycle""#,
        r#""env":{"EDITOR":"vi"},"lifecycle""#,
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
fn a_record_without_a_lifecycle_fails_to_deserialize() {
    let pane_record_json = FRESH_TERMINAL_RECORD_JSON.replacen(r#","lifecycle":"Spawning""#, "", 1);

    let deserialization_error =
        serde_json::from_str::<PaneRecord>(&pane_record_json).expect_err("missing field");

    assert_eq!(
        deserialization_error.to_string(),
        "missing field `lifecycle` at line 1 column 144"
    );
}
