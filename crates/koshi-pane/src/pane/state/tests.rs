//! Tests for the pane metadata record: creation defaults and lifecycle
//! ownership through `update_lifecycle`.

use std::collections::BTreeMap;
use std::time::SystemTime;

use koshi_core::error::DomainCategory;
use koshi_core::ids::{PaneId, PluginId};

use super::{PaneKind, PaneRecord};
use crate::error::InvalidTransition;
use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::policy::{PaneClosePolicy, PaneExitPolicy};

#[test]
fn a_new_record_starts_spawning_with_empty_metadata() {
    let id = PaneId::new();

    let record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);

    assert_eq!(record.id(), id);
    assert_eq!(record.kind(), &PaneKind::Terminal);
    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);
    assert_eq!(record.title, None);
    assert_eq!(record.command, None);
    assert_eq!(record.cwd, None);
    assert_eq!(record.close_policy, PaneClosePolicy::default());
    assert_eq!(record.exit_policy, PaneExitPolicy::CloseOnExit);
    assert_eq!(record.env, BTreeMap::new());
    assert_eq!(record.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(record.exited_at, None);
    assert_eq!(record.exit_code, None);
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
