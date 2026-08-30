//! Tests for `PaneRegistry`: insertion, lookup, removal, in-place edits, and
//! serialization round-trips of records and of the registry itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::{PaneId, PluginId};
use koshi_core::process::{ShellKind, SpawnSpec};

use super::PaneRegistry;
use crate::error::PaneRegistryError;
use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::policy::{PaneClosePolicy, PaneExitPolicy};
use crate::pane::state::{PaneKind, PaneRecord};

/// A terminal record for `id` with `close_policy = Force` and
/// `created_at = UNIX_EPOCH`.
fn terminal_record(id: PaneId) -> PaneRecord {
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record.close_policy = PaneClosePolicy::Force;
    record
}

#[test]
fn a_new_registry_is_empty() {
    let registry = PaneRegistry::new();

    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert_eq!(registry.list().count(), 0);
}

#[test]
fn new_and_default_build_the_same_empty_registry() {
    assert_eq!(PaneRegistry::new(), PaneRegistry::default());
}

#[test]
fn an_inserted_record_can_be_looked_up() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();

    registry.insert(terminal_record(id)).expect("first insert");

    assert!(!registry.is_empty());
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(id), Some(&terminal_record(id)));
    assert_eq!(registry.get(PaneId::new()), None);
}

#[test]
fn inserting_a_duplicate_id_is_rejected_and_keeps_the_original() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();

    let mut original = terminal_record(id);
    original.cwd = Some(PathBuf::from("/original"));
    let mut clash = terminal_record(id);
    clash.cwd = Some(PathBuf::from("/clash"));

    registry.insert(original).expect("first insert");
    let rejected = registry.insert(clash);

    assert_eq!(
        rejected,
        Err(PaneRegistryError::DuplicateId {
            id,
            kind: PaneKind::Terminal
        })
    );
    // The first record is untouched: a rejected insert never overwrites.
    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry.get(id).unwrap().cwd.as_deref(),
        Some(Path::new("/original"))
    );
}

#[test]
fn a_duplicate_insert_reports_the_kind_of_the_record_it_turned_away() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();
    let plugin_id = PluginId::new();
    registry.insert(terminal_record(id)).expect("first insert");

    let rejected = registry.insert(PaneRecord::new_with_kind(
        id,
        PaneKind::Plugin { plugin_id },
        SystemTime::UNIX_EPOCH,
    ));

    // The error carries the kind of the rejected record, not the kind of the
    // record already registered.
    assert_eq!(
        rejected,
        Err(PaneRegistryError::DuplicateId {
            id,
            kind: PaneKind::Plugin { plugin_id }
        })
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(id), Some(&terminal_record(id)));
}

#[test]
fn a_duplicate_id_error_is_recoverable_and_classified_by_pane_kind() {
    // The error's domain follows the clashing pane's kind.
    let terminal = PaneRegistryError::DuplicateId {
        id: PaneId::new(),
        kind: PaneKind::Terminal,
    };
    assert_eq!(terminal.category(), DomainCategory::Terminal);
    assert_eq!(terminal.severity(), Severity::Recoverable);

    let plugin = PaneRegistryError::DuplicateId {
        id: PaneId::new(),
        kind: PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    };
    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(plugin.severity(), Severity::Recoverable);
}

#[test]
fn removing_a_record_deletes_it() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();
    registry.insert(terminal_record(id)).expect("insert");

    let removed = registry.remove(id);

    assert_eq!(removed, Some(terminal_record(id)));
    assert!(registry.is_empty());
    assert_eq!(registry.get(id), None);
    // Removing an absent id is a no-op, not an error.
    assert_eq!(registry.remove(id), None);
}

#[test]
fn removing_one_record_leaves_the_others_in_place() {
    let mut registry = PaneRegistry::new();
    let kept = PaneId::new();
    let dropped = PaneId::new();
    registry.insert(terminal_record(kept)).expect("insert kept");
    registry
        .insert(terminal_record(dropped))
        .expect("insert dropped");

    assert_eq!(registry.remove(dropped), Some(terminal_record(dropped)));

    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(kept), Some(&terminal_record(kept)));
    assert_eq!(registry.get(dropped), None);
}

#[test]
fn get_mut_edits_a_record_in_place() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();
    registry.insert(terminal_record(id)).expect("insert");

    registry.get_mut(id).expect("present").cwd = Some(PathBuf::from("/edited"));

    assert_eq!(
        registry.get(id).unwrap().cwd.as_deref(),
        Some(Path::new("/edited"))
    );
    assert_eq!(registry.get_mut(PaneId::new()), None);
}

#[test]
fn a_lifecycle_step_through_get_mut_is_visible_through_get_and_remove() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();
    registry.insert(terminal_record(id)).expect("insert");

    registry
        .get_mut(id)
        .expect("present")
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");

    assert_eq!(
        registry.get(id).expect("present").lifecycle(),
        &PaneLifecycle::Running
    );
    assert_eq!(
        registry.remove(id).expect("present").lifecycle(),
        &PaneLifecycle::Running
    );
}

#[test]
fn list_yields_every_record() {
    let mut registry = PaneRegistry::new();
    let ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    for &id in &ids {
        registry.insert(terminal_record(id)).expect("insert");
    }

    // `list` has no fixed order: compare sorted by id.
    let mut listed: Vec<PaneRecord> = registry.list().cloned().collect();
    listed.sort_by_key(PaneRecord::id);
    let mut expected: Vec<PaneRecord> = ids.iter().map(|&id| terminal_record(id)).collect();
    expected.sort_by_key(PaneRecord::id);

    assert_eq!(listed, expected);
    assert_eq!(registry.len(), 3);
}

#[test]
fn a_removed_id_can_be_registered_again() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();
    registry.insert(terminal_record(id)).expect("first insert");
    registry.remove(id).expect("present");

    // The id is free again: a fresh record registers under it without error.
    registry.insert(terminal_record(id)).expect("reinsert");
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(id), Some(&terminal_record(id)));
}

#[test]
fn a_pane_record_survives_a_serde_round_trip() {
    let mut env = BTreeMap::new();
    env.insert("EDITOR".to_owned(), "nvim".to_owned());

    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);
    record.command = Some(SpawnSpec {
        program: PathBuf::from("/bin/bash"),
        args: vec!["-l".to_owned()],
        cwd: Some(PathBuf::from("/home/u")),
        env: env.clone(),
        shell_kind: ShellKind::Bash,
    });
    record.cwd = Some(PathBuf::from("/home/u"));
    record.close_policy = PaneClosePolicy::Graceful {
        timeout: Duration::from_secs(3),
    };
    record.exit_policy = PaneExitPolicy::RespawnShell;
    record.env = env;
    // Drive to `Exited { code: Some(0), .. }` through legal events.
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            code: Some(0),
            at: SystemTime::UNIX_EPOCH,
        })
        .expect("ProcessExited is legal from Running");

    let json = serde_json::to_string(&record).expect("serialize");
    let restored: PaneRecord = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(record, restored);
}

#[test]
fn a_plugin_pane_kind_survives_a_serde_round_trip() {
    let record = PaneRecord::new_with_kind(
        PaneId::new(),
        PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
        SystemTime::UNIX_EPOCH,
    );

    let json = serde_json::to_string(&record).expect("serialize");
    let restored: PaneRecord = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(record, restored);
}

#[test]
fn an_empty_registry_serializes_as_an_empty_records_map() {
    assert_eq!(
        serde_json::to_string(&PaneRegistry::new()).expect("serialize"),
        r#"{"records":{}}"#
    );
}

#[test]
fn a_registry_survives_a_serde_round_trip() {
    let mut registry = PaneRegistry::new();
    let terminal_id = PaneId::new();
    let plugin_id = PaneId::new();
    registry
        .insert(terminal_record(terminal_id))
        .expect("insert terminal");
    registry
        .insert(PaneRecord::new_with_kind(
            plugin_id,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
            SystemTime::UNIX_EPOCH,
        ))
        .expect("insert plugin");

    let json = serde_json::to_string(&registry).expect("serialize");
    let restored: PaneRegistry = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored, registry);
    assert_eq!(restored.len(), 2);
    assert_eq!(restored.get(terminal_id), registry.get(terminal_id));
    assert_eq!(restored.get(plugin_id), registry.get(plugin_id));
}
