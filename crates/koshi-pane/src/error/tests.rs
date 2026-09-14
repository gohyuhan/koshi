//! Tests for the pane error types: their `Display` text, the diagnostics domain
//! and severity each reports, and value equality.

use super::*;

use std::time::SystemTime;

use koshi_core::{
    error::{DomainCategory, DomainError, Severity},
    ids::{PaneId, PluginId},
};

use crate::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use crate::pane::state::PaneKind;

#[test]
fn a_duplicate_pane_id_error_names_the_pane_in_its_message() {
    let pane_id = PaneId::new();
    let error = PaneRegistryError::DuplicateId {
        pane_id,
        pane_kind: PaneKind::Terminal,
    };

    assert_eq!(
        error.to_string(),
        format!("pane-{} is already registered", pane_id.get_uuid())
    );
}

#[test]
fn a_duplicate_id_error_takes_its_domain_from_the_pane_kind() {
    let terminal = PaneRegistryError::DuplicateId {
        pane_id: PaneId::new(),
        pane_kind: PaneKind::Terminal,
    };
    let plugin = PaneRegistryError::DuplicateId {
        pane_id: PaneId::new(),
        pane_kind: PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    };

    assert_eq!(terminal.category(), DomainCategory::Terminal);
    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(terminal.get_severity(), Severity::Recoverable);
    assert_eq!(plugin.get_severity(), Severity::Recoverable);
}

#[test]
fn two_duplicate_pane_id_errors_are_equal_only_when_id_and_kind_match() {
    let pane_id = PaneId::new();
    let base = PaneRegistryError::DuplicateId {
        pane_id,
        pane_kind: PaneKind::Terminal,
    };

    assert_eq!(
        base,
        PaneRegistryError::DuplicateId {
            pane_id,
            pane_kind: PaneKind::Terminal,
        }
    );
    assert_ne!(
        base,
        PaneRegistryError::DuplicateId {
            pane_id: PaneId::new(),
            pane_kind: PaneKind::Terminal,
        }
    );
    assert_ne!(
        base,
        PaneRegistryError::DuplicateId {
            pane_id,
            pane_kind: PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
        }
    );
}

#[test]
fn two_invalid_transitions_are_equal_only_when_state_event_and_kind_match() {
    let exited_at = SystemTime::UNIX_EPOCH;
    let base_error = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Running,
        lifecycle_event: PaneLifecycleEvent::ProcessExited {
            exit_code: Some(1),
            exited_at,
        },
        pane_kind: PaneKind::Terminal,
    };

    assert_eq!(
        base_error,
        InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Running,
            lifecycle_event: PaneLifecycleEvent::ProcessExited {
                exit_code: Some(1),
                exited_at
            },
            pane_kind: PaneKind::Terminal,
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
    assert_ne!(
        base_error,
        InvalidTransitionError {
            pane_kind: PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
            ..base_error
        }
    );
}

#[test]
fn an_invalid_transition_names_the_state_and_event_in_its_message() {
    let error = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Spawning,
        lifecycle_event: PaneLifecycleEvent::Cleaned,
        pane_kind: PaneKind::Terminal,
    };

    assert_eq!(
        error.to_string(),
        "illegal pane lifecycle transition from Spawning on Cleaned"
    );
}

#[test]
fn an_invalid_transition_carries_its_payload_in_the_message() {
    let exited_at = SystemTime::UNIX_EPOCH;
    let error = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Running,
        lifecycle_event: PaneLifecycleEvent::ProcessExited {
            exit_code: Some(3),
            exited_at,
        },
        pane_kind: PaneKind::Terminal,
    };

    assert_eq!(
        error.to_string(),
        format!(
            "illegal pane lifecycle transition from Running on {:?}",
            PaneLifecycleEvent::ProcessExited {
                exit_code: Some(3),
                exited_at
            }
        )
    );
}

#[test]
fn an_invalid_transition_takes_its_domain_from_the_pane_kind() {
    let plugin = InvalidTransitionError {
        previous_lifecycle: PaneLifecycle::Removed,
        lifecycle_event: PaneLifecycleEvent::ProcessStarted,
        pane_kind: PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    };

    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(plugin.get_severity(), Severity::Recoverable);
}
