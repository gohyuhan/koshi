//! Tests for the session domain errors: their `Display` wording and their
//! [`DomainError`] classification.
//!
//! The `Display` of an id-bearing variant embeds a random UUID, so those tests
//! pin the exact wording against the same ids interpolated the same way — this
//! locks the message template and the field order (the ids differ, so a swapped
//! field would change the string and fail), while [`SessionConsistencyError::DuplicateTabIndex`]
//! carries no id and is checked against a fixed literal.

use std::time::SystemTime;

use super::*;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
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

#[test]
fn pane_in_multiple_layouts_display_of_one_tab_twice_repeats_that_tab() {
    // The same tab twice is how one tree holding a pane at two positions is
    // reported: the list carries one entry per leaf, not one per tab.
    let pane = PaneId::new();
    let tab = TabId::new();
    let err = SessionConsistencyError::PaneInMultipleLayouts {
        pane,
        tabs: vec![tab, tab],
    };
    assert_eq!(
        err.to_string(),
        format!("pane {pane:?} appears as a layout leaf in tabs [{tab:?}, {tab:?}]")
    );
}

#[test]
fn pane_in_multiple_layouts_display_of_an_empty_tab_list_shows_empty_brackets() {
    let pane = PaneId::new();
    let err = SessionConsistencyError::PaneInMultipleLayouts {
        pane,
        tabs: Vec::new(),
    };
    assert_eq!(
        err.to_string(),
        format!("pane {pane:?} appears as a layout leaf in tabs []")
    );
}

#[test]
fn invalid_transition_display_names_a_second_state_and_event_pair() {
    // A different pair through the same template: the state comes first, the
    // event second.
    let err = InvalidTransition {
        from: SessionLifecycle::Starting,
        event: SessionLifecycleEvent::ClientAttached,
    };
    assert_eq!(
        err.to_string(),
        "illegal session lifecycle transition from Starting on ClientAttached"
    );
}

#[test]
fn duplicate_tab_index_display_names_index_zero() {
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { index: 0 }.to_string(),
        "multiple tabs claim bar index 0"
    );
}

#[test]
fn duplicate_tab_index_display_names_the_largest_index() {
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { index: usize::MAX }.to_string(),
        format!("multiple tabs claim bar index {}", usize::MAX)
    );
}

#[test]
fn removed_pane_in_layout_display_names_the_tab_and_pane() {
    let tab = TabId::new();
    let pane = PaneId::new();
    let err = SessionConsistencyError::RemovedPaneInLayout { tab, pane };
    assert_eq!(
        err.to_string(),
        format!("tab {tab:?} layout still holds removed pane {pane:?}")
    );
}

#[test]
fn orphaned_pane_record_display_carries_the_exit_code_and_time() {
    // The struct variant renders its own fields, so the exit code is part of
    // the message.
    let pane = PaneId::new();
    let at = SystemTime::UNIX_EPOCH;
    let err = SessionConsistencyError::OrphanedPaneRecord {
        pane,
        lifecycle: PaneLifecycle::Exited { code: Some(2), at },
    };
    assert_eq!(
        err.to_string(),
        format!(
            "pane {pane:?} is Exited {{ code: Some(2), at: {at:?} }} but absent from every layout"
        )
    );
}

#[test]
fn focus_tab_missing_display_names_the_client_and_tab() {
    let client = ClientId::new();
    let tab = TabId::new();
    let err = SessionConsistencyError::FocusTabMissing { client, tab };
    assert_eq!(
        err.to_string(),
        format!("client {client:?} remembers focus in tab {tab:?} that is not in the session")
    );
}

#[test]
fn focus_target_missing_display_names_client_pane_and_tab() {
    let client = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let err = SessionConsistencyError::FocusTargetMissing { client, tab, pane };
    assert_eq!(
        err.to_string(),
        format!("client {client:?} focuses pane {pane:?} absent from tab {tab:?} layout")
    );
}

#[test]
fn zoom_target_missing_display_names_client_pane_and_tab() {
    let client = ClientId::new();
    let tab = TabId::new();
    let pane = PaneId::new();
    let err = SessionConsistencyError::ZoomTargetMissing { client, tab, pane };
    assert_eq!(
        err.to_string(),
        format!("client {client:?} is zoomed on pane {pane:?}, not a live leaf of tab {tab:?}")
    );
}

#[test]
fn active_tab_missing_display_names_the_client_and_tab() {
    let client = ClientId::new();
    let tab = TabId::new();
    let err = SessionConsistencyError::ActiveTabMissing { client, tab };
    assert_eq!(
        err.to_string(),
        format!("client {client:?} active tab {tab:?} is not in the session")
    );
}

#[test]
fn lingering_removed_record_display_names_the_pane() {
    let pane = PaneId::new();
    let err = SessionConsistencyError::LingeringRemovedRecord { pane };
    assert_eq!(
        err.to_string(),
        format!("removed pane {pane:?} still has a registry record")
    );
}

#[test]
fn tab_key_mismatch_display_names_the_key_then_the_tabs_own_id() {
    let key = TabId::new();
    let tab_id = TabId::new();
    let err = SessionConsistencyError::TabKeyMismatch { key, tab_id };
    assert_eq!(
        err.to_string(),
        format!("tab stored under key {key:?} reports its own id as {tab_id:?}")
    );
}

#[test]
fn client_session_mismatch_display_names_the_client_and_the_session_it_carries() {
    let client = ClientId::new();
    let found = SessionId::new();
    let err = SessionConsistencyError::ClientSessionMismatch { client, found };
    assert_eq!(
        err.to_string(),
        format!("client {client:?} belongs to session {found:?}, not this one")
    );
}

#[test]
fn lingering_closed_tab_display_names_the_tab() {
    let tab = TabId::new();
    let err = SessionConsistencyError::LingeringClosedTab { tab };
    assert_eq!(
        err.to_string(),
        format!("closed tab {tab:?} still sits in the session's tab map")
    );
}

#[test]
fn orphaned_pane_record_display_carries_a_closing_lifecycle() {
    // Every state but `Removed` reaches this variant, `Closing` included, and
    // the state's own fields land in the message.
    let pane = PaneId::new();
    let since = SystemTime::UNIX_EPOCH;
    let err = SessionConsistencyError::OrphanedPaneRecord {
        pane,
        lifecycle: PaneLifecycle::Closing { since },
    };
    assert_eq!(
        err.to_string(),
        format!("pane {pane:?} is Closing {{ since: {since:?} }} but absent from every layout")
    );
}

#[test]
fn consistency_errors_compare_by_variant_and_by_every_field() {
    // `Session::validate` returns a list callers assert against, so two
    // violations differing in one id must not compare equal.
    let (client, tab, pane) = (ClientId::new(), TabId::new(), PaneId::new());
    let other_pane = PaneId::new();

    assert_eq!(
        SessionConsistencyError::FocusTargetMissing { client, tab, pane },
        SessionConsistencyError::FocusTargetMissing { client, tab, pane }
    );
    assert_ne!(
        SessionConsistencyError::FocusTargetMissing { client, tab, pane },
        SessionConsistencyError::FocusTargetMissing {
            client,
            tab,
            pane: other_pane
        }
    );
    // Same fields, different variant.
    assert_ne!(
        SessionConsistencyError::FocusTargetMissing { client, tab, pane },
        SessionConsistencyError::FocusPaneNotInRegistry { client, tab, pane }
    );
}

#[test]
fn invalid_transitions_compare_by_state_and_by_event() {
    let stopping = InvalidTransition {
        from: SessionLifecycle::Stopping,
        event: SessionLifecycleEvent::ClientAttached,
    };

    assert_eq!(
        stopping,
        InvalidTransition {
            from: SessionLifecycle::Stopping,
            event: SessionLifecycleEvent::ClientAttached,
        }
    );
    assert_ne!(
        stopping,
        InvalidTransition {
            from: SessionLifecycle::Stopped,
            event: SessionLifecycleEvent::ClientAttached,
        }
    );
    assert_ne!(
        stopping,
        InvalidTransition {
            from: SessionLifecycle::Stopping,
            event: SessionLifecycleEvent::StopCompleted,
        }
    );
}
