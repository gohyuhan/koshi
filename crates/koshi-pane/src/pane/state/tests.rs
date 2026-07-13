//! Tests for the pane metadata record: creation defaults and lifecycle
//! ownership through `update_lifecycle`.

use std::collections::BTreeMap;
use std::time::SystemTime;

use koshi_core::ids::PaneId;

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
