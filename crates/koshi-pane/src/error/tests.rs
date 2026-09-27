//! Tests for the pane error types: their `Display` text and value equality.

use super::*;

use std::time::SystemTime;

use koshi_core::ids::PaneId;

use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};

#[test]
fn a_duplicate_pane_id_error_names_the_pane_in_its_message() {
    let pane_id = PaneId::new();
    let registry_error = PaneRegistryError::DuplicateId { pane_id };

    assert_eq!(
        registry_error.to_string(),
        format!("pane-{} is already registered", pane_id.get_uuid())
    );
}

#[test]
fn two_duplicate_pane_id_errors_are_equal_only_when_the_id_matches() {
    let pane_id = PaneId::new();
    let duplicate_id_error = PaneRegistryError::DuplicateId { pane_id };

    assert_eq!(
        duplicate_id_error,
        PaneRegistryError::DuplicateId { pane_id }
    );
    assert_ne!(
        duplicate_id_error,
        PaneRegistryError::DuplicateId {
            pane_id: PaneId::new(),
        }
    );
}

#[test]
fn two_invalid_transitions_are_equal_only_when_state_and_event_match() {
    let exited_at = SystemTime::UNIX_EPOCH;
    let base_error = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Running,
        lifecycle_event: PaneLifecycleEvent::ProcessExited {
            exit_code: Some(1),
            exited_at,
        },
    };

    assert_eq!(
        base_error,
        InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Running,
            lifecycle_event: PaneLifecycleEvent::ProcessExited {
                exit_code: Some(1),
                exited_at
            },
        }
    );
    assert_ne!(
        base_error,
        InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Spawning,
            ..base_error
        }
    );
    assert_ne!(
        base_error,
        InvalidTransitionError {
            lifecycle_event: PaneLifecycleEvent::ProcessExited {
                exit_code: Some(2),
                exited_at
            },
            ..base_error
        }
    );
}

#[test]
fn an_invalid_transition_names_the_state_and_event_in_its_message() {
    let transition_error = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Spawning,
        lifecycle_event: PaneLifecycleEvent::Cleaned,
    };

    assert_eq!(
        transition_error.to_string(),
        "illegal pane lifecycle transition from Spawning on Cleaned"
    );
}

#[test]
fn an_invalid_transition_carries_its_payload_in_the_message() {
    let exited_at = SystemTime::UNIX_EPOCH;
    let transition_error = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Running,
        lifecycle_event: PaneLifecycleEvent::ProcessExited {
            exit_code: Some(3),
            exited_at,
        },
    };

    assert_eq!(
        transition_error.to_string(),
        format!(
            "illegal pane lifecycle transition from Running on {:?}",
            PaneLifecycleEvent::ProcessExited {
                exit_code: Some(3),
                exited_at
            }
        )
    );
}
