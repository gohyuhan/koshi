//! Pane-module integration tests: driving a `PaneRecord` across the whole
//! lifecycle state machine (`state` + `lifecycle` together), including that
//! `Exited` never returns to a live state, terminality of `Removed`, and that a
//! plugin pane threads its kind into a rejected-transition error.

use std::time::{Duration, SystemTime};

use koshi_core::ids::{PaneId, PluginId};

use crate::error::InvalidTransitionError;
use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::state::{PaneKind, PaneRecord};

#[test]
fn a_pane_walks_from_spawning_to_removed_one_event_at_a_time() {
    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new(), SystemTime::UNIX_EPOCH);
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Spawning);

    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Running);

    let exited_at = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at,
        })
        .expect("ProcessExited is legal from Running");
    assert_eq!(
        pane_record.get_lifecycle(),
        &PaneLifecycle::Exited {
            exit_code: Some(0),
            exited_at
        }
    );

    let close_requested_at = SystemTime::UNIX_EPOCH + Duration::from_secs(9);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested { close_requested_at })
        .expect("CloseRequested is legal from Exited");
    assert_eq!(
        pane_record.get_lifecycle(),
        &PaneLifecycle::Closing { close_requested_at }
    );

    pane_record
        .update_lifecycle(PaneLifecycleEvent::Cleaned)
        .expect("Cleaned is legal from Closing");
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Removed);

    // Removed is terminal: a further event is rejected and the state holds.
    let rejected = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
    assert_eq!(
        rejected,
        Err(InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Removed,
            lifecycle_event: PaneLifecycleEvent::ProcessStarted,
            pane_kind: PaneKind::Terminal,
        })
    );
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Removed);
}

#[test]
fn an_exited_pane_only_moves_on_to_closing() {
    let exited_at = SystemTime::UNIX_EPOCH + Duration::from_secs(2);
    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new(), SystemTime::UNIX_EPOCH);

    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("ProcessStarted is legal from Spawning");
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: None,
            exited_at,
        })
        .expect("ProcessExited is legal from Running");

    // `Exited` has exactly one way out. Neither restarting the child nor
    // finishing a cleanup that was never requested is legal.
    for lifecycle_event in [
        PaneLifecycleEvent::ProcessStarted,
        PaneLifecycleEvent::Cleaned,
    ] {
        assert_eq!(
            pane_record.update_lifecycle(lifecycle_event),
            Err(InvalidTransitionError {
                previous_lifecycle: PaneLifecycle::Exited {
                    exit_code: None,
                    exited_at
                },
                lifecycle_event,
                pane_kind: PaneKind::Terminal,
            })
        );
        assert_eq!(
            pane_record.get_lifecycle(),
            &PaneLifecycle::Exited {
                exit_code: None,
                exited_at
            }
        );
    }

    let close_requested_at = SystemTime::UNIX_EPOCH + Duration::from_secs(3);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested { close_requested_at })
        .expect("CloseRequested is legal from Exited");

    assert_eq!(
        pane_record.get_lifecycle(),
        &PaneLifecycle::Closing { close_requested_at }
    );
}

#[test]
fn a_pane_can_be_closed_before_its_process_ever_starts() {
    let close_requested_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
    let mut pane_record = PaneRecord::from_terminal_pane(PaneId::new(), SystemTime::UNIX_EPOCH);

    pane_record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested { close_requested_at })
        .expect("CloseRequested is legal from Spawning");

    assert_eq!(
        pane_record.get_lifecycle(),
        &PaneLifecycle::Closing { close_requested_at }
    );
}

#[test]
fn a_rejected_transition_on_a_plugin_pane_reports_the_plugin_kind() {
    let plugin_kind = PaneKind::Plugin {
        plugin_id: PluginId::new(),
    };
    let mut pane_record =
        PaneRecord::from_pane_kind(PaneId::new(), plugin_kind, SystemTime::UNIX_EPOCH);

    let rejected = pane_record.update_lifecycle(PaneLifecycleEvent::Cleaned);

    assert_eq!(
        rejected,
        Err(InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Spawning,
            lifecycle_event: PaneLifecycleEvent::Cleaned,
            pane_kind: plugin_kind,
        })
    );
    assert_eq!(pane_record.get_lifecycle(), &PaneLifecycle::Spawning);
}
