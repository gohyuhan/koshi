use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tile_core::ids::{PaneId, PluginId};
use tile_core::process::{ShellKind, SpawnSpec};

use super::PaneRegistry;
use crate::error::PaneRegistryError;
use crate::pane::lifecycle::PaneLifecycle;
use crate::pane::policy::{PaneClosePolicy, PaneExitPolicy};
use crate::pane::state::{PaneKind, PaneRecord};

/// A minimal terminal-pane record. Timestamps use `UNIX_EPOCH` so tests stay
/// deterministic; per-test fields are tweaked by the caller.
fn terminal_record(id: PaneId) -> PaneRecord {
    PaneRecord {
        id,
        kind: PaneKind::Terminal,
        title: None,
        command: None,
        cwd: None,
        close_policy: PaneClosePolicy::Force,
        exit_policy: PaneExitPolicy::CloseOnExit,
        env: BTreeMap::new(),
        lifecycle: PaneLifecycle::Spawning,
        created_at: SystemTime::UNIX_EPOCH,
        exited_at: None,
        exit_code: None,
    }
}

#[test]
fn a_new_registry_is_empty() {
    let registry = PaneRegistry::new();

    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert_eq!(registry.list().count(), 0);
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
    original.title = Some("original".to_owned());
    let mut clash = terminal_record(id);
    clash.title = Some("clash".to_owned());

    registry.insert(original).expect("first insert");
    let rejected = registry.insert(clash);

    assert_eq!(rejected, Err(PaneRegistryError::DuplicateId(id)));
    // The first record is untouched — a rejected insert never overwrites.
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(id).unwrap().title.as_deref(), Some("original"));
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
fn get_mut_edits_a_record_in_place() {
    let mut registry = PaneRegistry::new();
    let id = PaneId::new();
    registry.insert(terminal_record(id)).expect("insert");

    registry.get_mut(id).expect("present").title = Some("renamed".to_owned());

    assert_eq!(registry.get(id).unwrap().title.as_deref(), Some("renamed"));
    assert_eq!(registry.get_mut(PaneId::new()), None);
}

#[test]
fn list_yields_every_record() {
    let mut registry = PaneRegistry::new();
    let mut ids: Vec<PaneId> = (0..3).map(|_| PaneId::new()).collect();
    for &id in &ids {
        registry.insert(terminal_record(id)).expect("insert");
    }

    // `list` order is the map's, so compare as sorted sets.
    let mut listed: Vec<PaneId> = registry.list().map(|record| record.id).collect();
    listed.sort();
    ids.sort();

    assert_eq!(listed, ids);
    assert_eq!(registry.len(), 3);
}

#[test]
fn a_pane_record_survives_a_serde_round_trip() {
    let mut env = BTreeMap::new();
    env.insert("EDITOR".to_owned(), "nvim".to_owned());

    let record = PaneRecord {
        id: PaneId::new(),
        kind: PaneKind::Terminal,
        title: Some("editor".to_owned()),
        command: Some(SpawnSpec {
            program: PathBuf::from("/bin/bash"),
            args: vec!["-l".to_owned()],
            cwd: Some(PathBuf::from("/home/u")),
            env: env.clone(),
            shell_kind: ShellKind::Bash,
        }),
        cwd: Some(PathBuf::from("/home/u")),
        close_policy: PaneClosePolicy::Graceful {
            timeout: Duration::from_secs(3),
        },
        exit_policy: PaneExitPolicy::HoldOnExit,
        env,
        lifecycle: PaneLifecycle::Exited {
            code: Some(0),
            at: SystemTime::UNIX_EPOCH,
        },
        created_at: SystemTime::UNIX_EPOCH,
        exited_at: Some(SystemTime::UNIX_EPOCH),
        exit_code: Some(0),
    };

    let json = serde_json::to_string(&record).expect("serialize");
    let restored: PaneRecord = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(record, restored);
}

#[test]
fn a_plugin_pane_kind_survives_a_serde_round_trip() {
    let record = PaneRecord {
        kind: PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
        ..terminal_record(PaneId::new())
    };

    let json = serde_json::to_string(&record).expect("serialize");
    let restored: PaneRecord = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(record, restored);
}
