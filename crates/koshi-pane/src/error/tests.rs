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
fn a_duplicate_id_error_names_the_pane_in_its_message() {
    let id = PaneId::new();
    let error = PaneRegistryError::DuplicateId {
        id,
        kind: PaneKind::Terminal,
    };

    // The id renders through its own `Display` (`pane-<uuid>`).
    assert_eq!(
        error.to_string(),
        format!("pane {id} is already registered")
    );
}

#[test]
fn a_duplicate_id_error_takes_its_domain_from_the_pane_kind() {
    let terminal = PaneRegistryError::DuplicateId {
        id: PaneId::new(),
        kind: PaneKind::Terminal,
    };
    let plugin = PaneRegistryError::DuplicateId {
        id: PaneId::new(),
        kind: PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    };

    assert_eq!(terminal.category(), DomainCategory::Terminal);
    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(terminal.severity(), Severity::Recoverable);
    assert_eq!(plugin.severity(), Severity::Recoverable);
}

#[test]
fn two_duplicate_id_errors_are_equal_only_when_id_and_kind_match() {
    let id = PaneId::new();
    let base = PaneRegistryError::DuplicateId {
        id,
        kind: PaneKind::Terminal,
    };

    assert_eq!(
        base,
        PaneRegistryError::DuplicateId {
            id,
            kind: PaneKind::Terminal,
        }
    );
    assert_ne!(
        base,
        PaneRegistryError::DuplicateId {
            id: PaneId::new(),
            kind: PaneKind::Terminal,
        }
    );
}

#[test]
fn an_invalid_transition_names_the_state_and_event_in_its_message() {
    let error = InvalidTransition {
        from: PaneLifecycle::Spawning,
        event: PaneLifecycleEvent::Cleaned,
        kind: PaneKind::Terminal,
    };

    assert_eq!(
        error.to_string(),
        "illegal pane lifecycle transition from Spawning on Cleaned"
    );
}

#[test]
fn an_invalid_transition_carries_its_payload_in_the_message() {
    let at = SystemTime::UNIX_EPOCH;
    let error = InvalidTransition {
        from: PaneLifecycle::Running,
        event: PaneLifecycleEvent::ProcessExited { code: Some(3), at },
        kind: PaneKind::Terminal,
    };

    assert_eq!(
        error.to_string(),
        format!(
            "illegal pane lifecycle transition from Running on {:?}",
            PaneLifecycleEvent::ProcessExited { code: Some(3), at }
        )
    );
}

#[test]
fn an_invalid_transition_takes_its_domain_from_the_pane_kind() {
    let plugin = InvalidTransition {
        from: PaneLifecycle::Removed,
        event: PaneLifecycleEvent::ProcessStarted,
        kind: PaneKind::Plugin {
            plugin_id: PluginId::new(),
        },
    };

    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(plugin.severity(), Severity::Recoverable);
}
