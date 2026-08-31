//! Tests for the pane metadata record: creation defaults, lifecycle
//! ownership through `update_lifecycle`, and the serialized form.

use std::time::SystemTime;

use koshi_core::error::DomainCategory;
use koshi_core::ids::{PaneId, PluginId};

use super::{PaneKind, PaneRecord};
use crate::error::InvalidTransition;
use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::policy::{PaneClosePolicy, PaneExitPolicy};

/// The JSON form of a fresh terminal record with the nil uuid as its id and
/// `UNIX_EPOCH` as its creation time.
const FRESH_TERMINAL_RECORD_JSON: &str = r#"{"id":"00000000-0000-0000-0000-000000000000","kind":"Terminal","command":null,"cwd":null,"close_policy":{"Graceful":{"timeout":3}},"exit_policy":"CloseOnExit","lifecycle":"Spawning","created_at":{"secs_since_epoch":0,"nanos_since_epoch":0}}"#;

/// The `PaneId` whose uuid is all zeros.
fn nil_pane_id() -> PaneId {
    serde_json::from_str(r#""00000000-0000-0000-0000-000000000000""#).expect("a valid uuid")
}

/// The `PluginId` whose uuid is all ones.
fn fixed_plugin_id() -> PluginId {
    serde_json::from_str(r#""11111111-1111-1111-1111-111111111111""#).expect("a valid uuid")
}

#[test]
fn a_new_record_starts_spawning_with_empty_metadata() {
    let id = PaneId::new();

    let record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);

    assert_eq!(record.id(), id);
    assert_eq!(record.kind(), &PaneKind::Terminal);
    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);
    assert_eq!(record.command, None);
    assert_eq!(record.cwd, None);
    assert_eq!(record.close_policy, PaneClosePolicy::default());
    assert_eq!(record.exit_policy, PaneExitPolicy::CloseOnExit);
    assert_eq!(record.created_at(), SystemTime::UNIX_EPOCH);
}

#[test]
fn new_and_new_with_kind_terminal_build_the_same_record() {
    let id = PaneId::new();

    assert_eq!(
        PaneRecord::new(id, SystemTime::UNIX_EPOCH),
        PaneRecord::new_with_kind(id, PaneKind::Terminal, SystemTime::UNIX_EPOCH)
    );
}

#[test]
fn a_rejected_lifecycle_event_leaves_the_record_unchanged() {
    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);

    // `Cleaned` is illegal from `Spawning`: the record reports the rejection…
    let rejected = record.update_lifecycle(PaneLifecycleEvent::Cleaned);

    assert_eq!(
        rejected,
        Err(InvalidTransition {
            from: PaneLifecycle::Spawning,
            event: PaneLifecycleEvent::Cleaned,
            kind: PaneKind::Terminal,
        })
    );
    // …and stays exactly where it was.
    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);
}

#[test]
fn an_accepted_lifecycle_event_advances_the_record() {
    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);

    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");

    assert_eq!(record.lifecycle(), &PaneLifecycle::Running);
}

#[test]
fn a_record_accepts_a_legal_event_after_a_rejected_one() {
    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);

    let rejected = record.update_lifecycle(PaneLifecycleEvent::Cleaned);
    let accepted = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);

    assert_eq!(
        rejected,
        Err(InvalidTransition {
            from: PaneLifecycle::Spawning,
            event: PaneLifecycleEvent::Cleaned,
            kind: PaneKind::Terminal,
        })
    );
    assert_eq!(accepted, Ok(()));
    assert_eq!(record.lifecycle(), &PaneLifecycle::Running);
}

#[test]
fn a_plugin_record_carries_the_plugin_kind_and_its_domain() {
    let plugin_id = PluginId::new();

    let record = PaneRecord::new_with_kind(
        PaneId::new(),
        PaneKind::Plugin { plugin_id },
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(record.kind(), &PaneKind::Plugin { plugin_id });
    assert_eq!(record.kind().domain_category(), DomainCategory::Plugin);
    // A plugin pane still starts life in `Spawning`, like a terminal one.
    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);
}

#[test]
fn a_plugin_record_advances_on_a_legal_event() {
    let plugin_kind = PaneKind::Plugin {
        plugin_id: PluginId::new(),
    };
    let mut record = PaneRecord::new_with_kind(PaneId::new(), plugin_kind, SystemTime::UNIX_EPOCH);

    let accepted = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);

    assert_eq!(accepted, Ok(()));
    assert_eq!(record.lifecycle(), &PaneLifecycle::Running);
    assert_eq!(record.kind(), &plugin_kind);
}

#[test]
fn pane_kinds_serialize_as_a_variant_name_or_a_plugin_id_object() {
    assert_eq!(
        serde_json::to_string(&PaneKind::Terminal).expect("serialize"),
        r#""Terminal""#
    );
    assert_eq!(
        serde_json::to_string(&PaneKind::Plugin {
            plugin_id: fixed_plugin_id()
        })
        .expect("serialize"),
        r#"{"Plugin":{"plugin_id":"11111111-1111-1111-1111-111111111111"}}"#
    );
}

#[test]
fn a_fresh_record_serializes_under_its_field_names() {
    let record = PaneRecord::new(nil_pane_id(), SystemTime::UNIX_EPOCH);

    assert_eq!(
        serde_json::to_string(&record).expect("serialize"),
        FRESH_TERMINAL_RECORD_JSON
    );
}

#[test]
fn a_record_deserializes_from_its_field_names() {
    let restored: PaneRecord =
        serde_json::from_str(FRESH_TERMINAL_RECORD_JSON).expect("deserialize");

    assert_eq!(
        restored,
        PaneRecord::new(nil_pane_id(), SystemTime::UNIX_EPOCH)
    );
}

#[test]
fn a_record_ignores_an_unknown_field_when_deserializing() {
    let json = FRESH_TERMINAL_RECORD_JSON.replacen("{", r#"{"unknown":1,"#, 1);

    let restored: PaneRecord = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(
        restored,
        PaneRecord::new(nil_pane_id(), SystemTime::UNIX_EPOCH)
    );
}

/// Stored JSON that carries an `env` map decodes to the same record as JSON
/// without one. `env` is not a field of `PaneRecord`, so it is skipped.
#[test]
fn a_record_carrying_a_stored_env_map_still_deserializes() {
    let json = FRESH_TERMINAL_RECORD_JSON.replacen(
        r#""lifecycle""#,
        r#""env":{"EDITOR":"vi"},"lifecycle""#,
        1,
    );

    let restored: PaneRecord = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(
        restored,
        PaneRecord::new(nil_pane_id(), SystemTime::UNIX_EPOCH)
    );
}

#[test]
fn a_record_without_a_lifecycle_fails_to_deserialize() {
    let json = FRESH_TERMINAL_RECORD_JSON.replacen(r#""lifecycle":"Spawning","#, "", 1);

    let error = serde_json::from_str::<PaneRecord>(&json).expect_err("missing field");

    assert_eq!(
        error.to_string(),
        "missing field `lifecycle` at line 1 column 217"
    );
}
