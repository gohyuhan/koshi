//! Tests for the session domain errors: their `Display` wording and their
//! [`DomainError`] classification.
//!
//! The `Display` of an id-bearing variant embeds a random UUID, so those tests
//! pin the exact wording against the same ids interpolated the same way — this
//! locks the message template and the field order (the ids differ, so a swapped
//! field would change the string and fail), while [`SessionConsistencyError::DuplicateTabIndex`]
//! carries no id and is checked against a fixed literal.

use super::*;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_pane::pane::lifecycle::PaneLifecycle;

use crate::session::lifecycle::{SessionLifecycle, SessionLifecycleEvent};

#[test]
fn invalid_transition_display_names_the_state_and_event() {
    let err = InvalidTransition {
        from: SessionLifecycle::Running,
        event: SessionLifecycleEvent::StopCompleted,
    };
    assert_eq!(
        err.to_string(),
        "illegal session lifecycle transition from Running on StopCompleted"
    );
}

#[test]
fn an_invalid_transition_is_a_recoverable_session_error() {
    let err = InvalidTransition {
        from: SessionLifecycle::Stopped,
        event: SessionLifecycleEvent::FirstTabCreated,
    };
    assert_eq!(err.category(), DomainCategory::Session);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn a_consistency_error_is_a_recoverable_session_error() {
    // The classification is a flat constant, so two unrelated variants prove it
    // is variant-independent.
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { index: 0 }.category(),
        DomainCategory::Session
    );
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { index: 0 }.severity(),
        Severity::Recoverable
    );
    assert_eq!(
        SessionConsistencyError::LingeringRemovedRecord {
            pane: PaneId::new()
        }
        .category(),
        DomainCategory::Session
    );
    assert_eq!(
        SessionConsistencyError::LingeringRemovedRecord {
            pane: PaneId::new()
        }
        .severity(),
        Severity::Recoverable
    );
}

#[test]
fn duplicate_tab_index_display_names_the_index() {
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { index: 7 }.to_string(),
        "multiple tabs claim bar index 7"
    );
}

#[test]
fn pane_not_in_registry_display_names_the_tab_and_pane() {
    let tab = TabId::new();
    let pane = PaneId::new();
    let err = SessionConsistencyError::PaneNotInRegistry { tab, pane };
    assert_eq!(
        err.to_string(),
        format!("tab {tab:?} layout references pane {pane:?} with no registry record")
    );
}

#[test]
fn orphaned_pane_record_display_names_the_pane_and_lifecycle() {
    let pane = PaneId::new();
    let err = SessionConsistencyError::OrphanedPaneRecord {
        pane,
        lifecycle: PaneLifecycle::Running,
    };
    assert_eq!(
        err.to_string(),
        format!("pane {pane:?} is Running but absent from every layout")
    );
}

#[test]
fn focus_pane_not_in_registry_display_names_client_pane_and_tab() {
    let client = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let err = SessionConsistencyError::FocusPaneNotInRegistry { client, tab, pane };
    assert_eq!(
        err.to_string(),
        format!("client {client:?} focuses pane {pane:?} (tab {tab:?}) with no registry record")
    );
}

#[test]
fn pane_in_multiple_layouts_display_lists_every_tab() {
    let pane = PaneId::new();
    let tabs = vec![TabId::new(), TabId::new()];
    let err = SessionConsistencyError::PaneInMultipleLayouts {
        pane,
        tabs: tabs.clone(),
    };
    assert_eq!(
        err.to_string(),
        format!("pane {pane:?} appears as a layout leaf in tabs {tabs:?}")
    );
}
