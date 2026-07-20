//! Pane-module integration tests: driving a `PaneRecord` across the whole
//! lifecycle state machine (`state` + `lifecycle` together), including the
//! respawn loop, terminality of `Removed`, and that a plugin pane threads its
//! kind into a rejected-transition error.

use std::time::{Duration, SystemTime};

use koshi_core::ids::{PaneId, PluginId};

use crate::error::InvalidTransition;
use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::state::{PaneKind, PaneRecord};

#[test]
fn a_pane_walks_from_spawning_to_removed_one_event_at_a_time() {
    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);
    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);

    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");
    assert_eq!(record.lifecycle(), &PaneLifecycle::Running);

    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited { code: Some(0), at })
        .expect("ProcessExited is legal from Running");
    assert_eq!(
        record.lifecycle(),
        &PaneLifecycle::Exited { code: Some(0), at }
    );

    // The lifecycle move records the exit inside its own state variant; the
    // record's separate `exit_code`/`exited_at` fields are set elsewhere and
    // stay untouched here.
    assert_eq!(record.exit_code, None);
    assert_eq!(record.exited_at, None);

    let since = SystemTime::UNIX_EPOCH + Duration::from_secs(9);
    record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested { since })
        .expect("CloseRequested is legal from Exited");
    assert_eq!(record.lifecycle(), &PaneLifecycle::Closing { since });

    record
        .update_lifecycle(PaneLifecycleEvent::Cleaned)
        .expect("Cleaned is legal from Closing");
    assert_eq!(record.lifecycle(), &PaneLifecycle::Removed);

    // Removed is terminal: a further event is rejected and the state holds.
    let rejected = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    assert_eq!(
        rejected,
        Err(InvalidTransition {
            from: PaneLifecycle::Removed,
            event: PaneLifecycleEvent::ProcessStarted,
            kind: PaneKind::Terminal,
        })
    );
    assert_eq!(record.lifecycle(), &PaneLifecycle::Removed);
}

#[test]
fn an_exited_pane_can_respawn_back_to_spawning() {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);

    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited { code: None, at })
        .expect("ProcessExited is legal from Running");
    record
        .update_lifecycle(PaneLifecycleEvent::Respawn)
        .expect("Respawn is legal from Exited");

    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);
}

#[test]
fn a_pane_can_be_closed_before_its_process_ever_starts() {
    let since = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let mut record = PaneRecord::new(PaneId::new(), SystemTime::UNIX_EPOCH);

    record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested { since })
        .expect("CloseRequested is legal from Spawning");

    assert_eq!(record.lifecycle(), &PaneLifecycle::Closing { since });
}

#[test]
fn a_rejected_transition_on_a_plugin_pane_reports_the_plugin_kind() {
    let plugin_kind = PaneKind::Plugin {
        plugin_id: PluginId::new(),
    };
    let mut record =
        PaneRecord::new_with_kind(PaneId::new(), plugin_kind.clone(), SystemTime::UNIX_EPOCH);

    let rejected = record.update_lifecycle(PaneLifecycleEvent::Cleaned);

    assert_eq!(
        rejected,
        Err(InvalidTransition {
            from: PaneLifecycle::Spawning,
            event: PaneLifecycleEvent::Cleaned,
            kind: plugin_kind,
        })
    );
    assert_eq!(record.lifecycle(), &PaneLifecycle::Spawning);
}
