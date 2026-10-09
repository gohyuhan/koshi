//! Tests for `PaneRegistry`: insertion, lookup, removal, in-place edits, and
//! serialization round-trips of records and of the registry itself.

use super::*;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use koshi_core::process::{ShellKind, SpawnSpec};

use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::policy::PaneClosePolicy;

/// A terminal pane record for `pane_id` with `close_policy = Force`.
fn build_terminal_pane_record(pane_id: PaneId) -> PaneRecord {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record.close_policy = PaneClosePolicy::Force;
    pane_record
}

#[test]
fn new_pane_registry_starts_with_no_records() {
    let registry = PaneRegistry::new();

    assert_eq!(registry.count_pane_records(), 0);
    assert_eq!(registry.list_pane_records().count(), 0);
}

#[test]
fn pane_registry_new_matches_the_default_empty_registry() {
    assert_eq!(PaneRegistry::new(), PaneRegistry::default());
}

#[test]
fn registered_pane_record_is_found_by_pane_id() {
    let mut registry = PaneRegistry::new();
    let pane_id = PaneId::new();

    registry
        .register_pane_record(build_terminal_pane_record(pane_id))
        .expect("first insert");

    assert_eq!(registry.count_pane_records(), 1);
    assert_eq!(
        registry.get_pane_record_by_id(pane_id),
        Some(&build_terminal_pane_record(pane_id))
    );
    assert_eq!(registry.get_pane_record_by_id(PaneId::new()), None);
}

#[test]
fn inserting_a_duplicate_pane_id_is_rejected_and_keeps_the_original() {
    let mut registry = PaneRegistry::new();
    let pane_id = PaneId::new();

    let mut original_pane_record = build_terminal_pane_record(pane_id);
    original_pane_record.working_directory = Some(PathBuf::from("/original"));
    let mut conflicting_pane_record = build_terminal_pane_record(pane_id);
    conflicting_pane_record.working_directory = Some(PathBuf::from("/clash"));

    registry
        .register_pane_record(original_pane_record)
        .expect("first insert");
    let rejected_registration = registry.register_pane_record(conflicting_pane_record);

    assert_eq!(
        rejected_registration,
        Err(PaneRegistryError::DuplicateId { pane_id })
    );
    // The first pane record is untouched: a rejected insert never overwrites.
    assert_eq!(registry.count_pane_records(), 1);
    assert_eq!(
        registry
            .get_pane_record_by_id(pane_id)
            .unwrap()
            .working_directory
            .as_deref(),
        Some(Path::new("/original"))
    );
}

#[test]
fn remove_pane_record_returns_and_deletes_the_record() {
    let mut registry = PaneRegistry::new();
    let pane_id = PaneId::new();
    registry
        .register_pane_record(build_terminal_pane_record(pane_id))
        .expect("insert");

    let removed_record = registry.remove_pane_record(pane_id);

    assert_eq!(removed_record, Some(build_terminal_pane_record(pane_id)));
    assert_eq!(registry.count_pane_records(), 0);
    assert_eq!(registry.get_pane_record_by_id(pane_id), None);
    // Removing an absent pane ID is a no-op, not an error.
    assert_eq!(registry.remove_pane_record(pane_id), None);
}

#[test]
fn remove_pane_record_keeps_other_records() {
    let mut registry = PaneRegistry::new();
    let retained_pane_id = PaneId::new();
    let removed_pane_id = PaneId::new();
    registry
        .register_pane_record(build_terminal_pane_record(retained_pane_id))
        .expect("insert kept");
    registry
        .register_pane_record(build_terminal_pane_record(removed_pane_id))
        .expect("insert dropped");

    assert_eq!(
        registry.remove_pane_record(removed_pane_id),
        Some(build_terminal_pane_record(removed_pane_id))
    );

    assert_eq!(registry.count_pane_records(), 1);
    assert_eq!(
        registry.get_pane_record_by_id(retained_pane_id),
        Some(&build_terminal_pane_record(retained_pane_id))
    );
    assert_eq!(registry.get_pane_record_by_id(removed_pane_id), None);
}

#[test]
fn mutable_pane_record_lookup_updates_the_registered_record() {
    let mut registry = PaneRegistry::new();
    let pane_id = PaneId::new();
    registry
        .register_pane_record(build_terminal_pane_record(pane_id))
        .expect("insert");

    registry
        .get_pane_record_mut_by_id(pane_id)
        .expect("present")
        .working_directory = Some(PathBuf::from("/edited"));

    assert_eq!(
        registry
            .get_pane_record_by_id(pane_id)
            .unwrap()
            .working_directory
            .as_deref(),
        Some(Path::new("/edited"))
    );
    assert_eq!(registry.get_pane_record_mut_by_id(PaneId::new()), None);
}

#[test]
fn mutable_pane_record_lookup_updates_lifecycle_for_get_and_remove() {
    let mut registry = PaneRegistry::new();
    let pane_id = PaneId::new();
    registry
        .register_pane_record(build_terminal_pane_record(pane_id))
        .expect("insert");

    registry
        .get_pane_record_mut_by_id(pane_id)
        .expect("present")
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");

    assert_eq!(
        registry
            .get_pane_record_by_id(pane_id)
            .expect("present")
            .get_lifecycle(),
        &PaneLifecycle::Running
    );
    assert_eq!(
        registry
            .remove_pane_record(pane_id)
            .expect("present")
            .get_lifecycle(),
        &PaneLifecycle::Running
    );
}

#[test]
fn list_pane_records_returns_records_in_pane_id_order() {
    let mut registry = PaneRegistry::new();
    let mut pane_ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    pane_ids.sort_unstable();

    // Insert the highest pane ID first; `list` must still walk the lowest pane ID first.
    for &pane_id in pane_ids.iter().rev() {
        registry
            .register_pane_record(build_terminal_pane_record(pane_id))
            .expect("insert");
    }

    let listed_records: Vec<PaneRecord> = registry.list_pane_records().cloned().collect();
    let expected_records: Vec<PaneRecord> = pane_ids
        .iter()
        .map(|&pane_id| build_terminal_pane_record(pane_id))
        .collect();

    assert_eq!(listed_records, expected_records);
    assert_eq!(registry.count_pane_records(), 3);
}

#[test]
fn pane_registry_allows_registering_a_removed_pane_id_again() {
    let mut registry = PaneRegistry::new();
    let pane_id = PaneId::new();
    registry
        .register_pane_record(build_terminal_pane_record(pane_id))
        .expect("first insert");
    registry.remove_pane_record(pane_id).expect("present");

    // The pane ID is free again: a fresh pane record registers under it without error.
    registry
        .register_pane_record(build_terminal_pane_record(pane_id))
        .expect("reinsert");
    assert_eq!(registry.count_pane_records(), 1);
    assert_eq!(
        registry.get_pane_record_by_id(pane_id),
        Some(&build_terminal_pane_record(pane_id))
    );
}

#[test]
fn a_pane_record_survives_a_serde_round_trip() {
    let mut environment_variables = BTreeMap::new();
    environment_variables.insert("EDITOR".to_owned(), "nvim".to_owned());

    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new());
    pane_record.spawn_spec = Some(SpawnSpec {
        program: PathBuf::from("/bin/bash"),
        arguments: vec!["-l".to_owned()],
        working_directory: Some(PathBuf::from("/home/u")),
        environment_variables,
        shell_kind: ShellKind::Bash,
    });
    pane_record.working_directory = Some(PathBuf::from("/home/u"));
    pane_record.close_policy = PaneClosePolicy::Graceful {
        timeout_duration: Duration::from_secs(3),
    };
    // Drive to `Exited { exit_code: Some(0), .. }` through legal events.
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at: SystemTime::UNIX_EPOCH,
        })
        .expect("ProcessExited is legal from Running");

    let record_json = serde_json::to_string(&pane_record).expect("serialize");
    let restored_record: PaneRecord = serde_json::from_str(&record_json).expect("deserialize");

    assert_eq!(pane_record, restored_record);
}

#[test]
fn empty_pane_registry_serializes_with_no_records() {
    assert_eq!(
        serde_json::to_string(&PaneRegistry::new()).expect("serialize"),
        r#"{"pane_record_by_id":{}}"#
    );
}

#[test]
fn pane_registry_round_trip_preserves_registered_records() {
    let mut registry = PaneRegistry::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    registry
        .register_pane_record(build_terminal_pane_record(first_pane_id))
        .expect("insert first");
    registry
        .register_pane_record(PaneRecord::from_terminal_pane(second_pane_id))
        .expect("insert second");

    let registry_json = serde_json::to_string(&registry).expect("serialize");
    let restored_registry: PaneRegistry =
        serde_json::from_str(&registry_json).expect("deserialize");

    assert_eq!(restored_registry, registry);
    assert_eq!(restored_registry.count_pane_records(), 2);
    assert_eq!(
        restored_registry.get_pane_record_by_id(first_pane_id),
        registry.get_pane_record_by_id(first_pane_id)
    );
    assert_eq!(
        restored_registry.get_pane_record_by_id(second_pane_id),
        registry.get_pane_record_by_id(second_pane_id)
    );
}
