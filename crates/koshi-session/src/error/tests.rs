//! Tests for the session domain errors: their `Display` wording and their
//! equality.
//!
//! The `Display` of an id-bearing variant embeds a random UUID. Each test of
//! such a variant builds the expected message from the same ids, interpolated
//! the same way, and gives every id field a different id: a message with two
//! fields swapped fails. [`SessionConsistencyError::DuplicateTabIndex`],
//! [`SessionConsistencyError::TooManyFloatingPanes`] and
//! [`FloatingSetError::TooManyPanes`] carry no id and are checked against a fixed
//! literal.

use super::*;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

use crate::session::lifecycle::{SessionLifecycle, SessionLifecycleEvent};

#[test]
fn invalid_transition_display_names_the_state_and_event() {
    let transition_error = InvalidTransition {
        previous_lifecycle: SessionLifecycle::Running,
        lifecycle_event: SessionLifecycleEvent::StopCompleted,
    };
    assert_eq!(
        transition_error.to_string(),
        "illegal session lifecycle transition from Running on StopCompleted"
    );
}

#[test]
fn duplicate_tab_index_display_names_the_index() {
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { tab_index: 7 }.to_string(),
        "multiple tabs claim bar index 7"
    );
}

#[test]
fn pane_not_in_registry_display_names_the_tab_and_pane() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let consistency_error = SessionConsistencyError::PaneNotInRegistry { tab_id, pane_id };
    assert_eq!(
        consistency_error.to_string(),
        format!("tab {tab_id:?} layout references pane {pane_id:?} with no registry record")
    );
}

#[test]
fn orphaned_pane_record_display_names_the_pane() {
    let pane_id = PaneId::new();
    let consistency_error = SessionConsistencyError::OrphanedPaneRecord { pane_id };
    assert_eq!(
        consistency_error.to_string(),
        format!("pane {pane_id:?} is absent from every layout and from the floating panes")
    );
}

#[test]
fn focus_pane_not_in_registry_display_names_client_pane_and_tab() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let consistency_error = SessionConsistencyError::FocusPaneNotInRegistry {
        client_id,
        tab_id,
        pane_id,
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("client {client_id:?} focuses pane {pane_id:?} (tab {tab_id:?}) with no registry record")
    );
}

#[test]
fn pane_in_multiple_layouts_display_lists_every_tab() {
    let pane_id = PaneId::new();
    let tab_ids = vec![TabId::new(), TabId::new()];
    let consistency_error = SessionConsistencyError::PaneInMultipleLayouts {
        pane_id,
        tab_ids: tab_ids.clone(),
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("pane {pane_id:?} appears as a layout leaf in tabs {tab_ids:?}")
    );
}

#[test]
fn pane_in_multiple_layouts_display_of_one_tab_twice_repeats_that_tab() {
    // The same tab twice is how one tree holding a pane at two positions is
    // reported: the list carries one entry per leaf, not one per tab.
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let consistency_error = SessionConsistencyError::PaneInMultipleLayouts {
        pane_id,
        tab_ids: vec![tab_id, tab_id],
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("pane {pane_id:?} appears as a layout leaf in tabs [{tab_id:?}, {tab_id:?}]")
    );
}

#[test]
fn pane_in_multiple_layouts_display_of_an_empty_tab_list_shows_empty_brackets() {
    let pane_id = PaneId::new();
    let consistency_error = SessionConsistencyError::PaneInMultipleLayouts {
        pane_id,
        tab_ids: Vec::new(),
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("pane {pane_id:?} appears as a layout leaf in tabs []")
    );
}

#[test]
fn invalid_transition_display_names_a_second_state_and_event_pair() {
    // A different pair through the same template: the state comes first, the
    // event second.
    let transition_error = InvalidTransition {
        previous_lifecycle: SessionLifecycle::Starting,
        lifecycle_event: SessionLifecycleEvent::ClientAttached,
    };
    assert_eq!(
        transition_error.to_string(),
        "illegal session lifecycle transition from Starting on ClientAttached"
    );
}

#[test]
fn duplicate_tab_index_display_names_index_zero() {
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex { tab_index: 0 }.to_string(),
        "multiple tabs claim bar index 0"
    );
}

#[test]
fn duplicate_tab_index_display_names_the_largest_index() {
    assert_eq!(
        SessionConsistencyError::DuplicateTabIndex {
            tab_index: usize::MAX
        }
        .to_string(),
        format!("multiple tabs claim bar index {}", usize::MAX)
    );
}

#[test]
fn focus_tab_missing_display_names_the_client_and_tab() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let consistency_error = SessionConsistencyError::FocusTabMissing { client_id, tab_id };
    assert_eq!(
        consistency_error.to_string(),
        format!(
            "client {client_id:?} remembers focus in tab {tab_id:?} that is not in the session"
        )
    );
}

#[test]
fn focus_target_missing_display_names_client_pane_and_tab() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let consistency_error = SessionConsistencyError::FocusTargetMissing {
        client_id,
        tab_id,
        pane_id,
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("client {client_id:?} focuses pane {pane_id:?} absent from tab {tab_id:?} layout")
    );
}

#[test]
fn zoom_target_missing_display_names_client_pane_and_tab() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let consistency_error = SessionConsistencyError::ZoomTargetMissing {
        client_id,
        tab_id,
        pane_id,
    };
    assert_eq!(
        consistency_error.to_string(),
        format!(
            "client {client_id:?} is zoomed on pane {pane_id:?}, not a live leaf of tab {tab_id:?}"
        )
    );
}

#[test]
fn active_tab_missing_display_names_the_client_and_tab() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let consistency_error = SessionConsistencyError::ActiveTabMissing { client_id, tab_id };
    assert_eq!(
        consistency_error.to_string(),
        format!("client {client_id:?} active tab {tab_id:?} is not in the session")
    );
}

#[test]
fn tab_key_mismatch_display_names_the_key_then_the_tabs_own_id() {
    let stored_tab_id = TabId::new();
    let reported_tab_id = TabId::new();
    let consistency_error = SessionConsistencyError::TabKeyMismatch {
        stored_tab_id,
        reported_tab_id,
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("tab stored under key {stored_tab_id:?} reports its own id as {reported_tab_id:?}")
    );
}

#[test]
fn client_session_mismatch_display_names_the_client_and_the_session_it_carries() {
    let client_id = ClientId::new();
    let found_session_id = SessionId::new();
    let consistency_error = SessionConsistencyError::ClientSessionMismatch {
        client_id,
        found_session_id,
    };
    assert_eq!(
        consistency_error.to_string(),
        format!("client {client_id:?} belongs to session {found_session_id:?}, not this one")
    );
}

#[test]
fn consistency_errors_compare_by_variant_and_by_every_field() {
    // Two violations that differ in one id compare unequal.
    let (client_id, tab_id, pane_id) = (ClientId::new(), TabId::new(), PaneId::new());
    let other_pane_id = PaneId::new();

    assert_eq!(
        SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id,
            pane_id,
        },
        SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id,
            pane_id,
        }
    );
    assert_ne!(
        SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id,
            pane_id,
        },
        SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id,
            pane_id: other_pane_id
        }
    );
    // Same fields, different variant.
    assert_ne!(
        SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id,
            pane_id,
        },
        SessionConsistencyError::FocusPaneNotInRegistry {
            client_id,
            tab_id,
            pane_id,
        }
    );
}

#[test]
fn invalid_transitions_compare_by_state_and_by_event() {
    let transition_from_stopping = InvalidTransition {
        previous_lifecycle: SessionLifecycle::Stopping,
        lifecycle_event: SessionLifecycleEvent::ClientAttached,
    };

    assert_eq!(
        transition_from_stopping,
        InvalidTransition {
            previous_lifecycle: SessionLifecycle::Stopping,
            lifecycle_event: SessionLifecycleEvent::ClientAttached,
        }
    );
    assert_ne!(
        transition_from_stopping,
        InvalidTransition {
            previous_lifecycle: SessionLifecycle::Stopped,
            lifecycle_event: SessionLifecycleEvent::ClientAttached,
        }
    );
    assert_ne!(
        transition_from_stopping,
        InvalidTransition {
            previous_lifecycle: SessionLifecycle::Stopping,
            lifecycle_event: SessionLifecycleEvent::StopCompleted,
        }
    );
}

#[test]
fn floating_set_error_display_names_the_repeated_pane_and_the_limit() {
    let pane_id = PaneId::new();

    assert_eq!(
        FloatingSetError::DuplicatePane { pane_id }.to_string(),
        format!("pane-{} is already a floating pane", pane_id.get_uuid())
    );
    assert_eq!(
        FloatingSetError::TooManyPanes.to_string(),
        "a session holds at most 12 floating panes"
    );
}

#[test]
fn too_many_floating_panes_display_names_the_count_and_the_limit() {
    assert_eq!(
        SessionConsistencyError::TooManyFloatingPanes { member_count: 13 }.to_string(),
        "floating panes list 13 panes, more than 12"
    );
}

#[test]
fn floating_member_errors_display_the_pane() {
    let pane_id = PaneId::new();
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());

    assert_eq!(
        SessionConsistencyError::FloatingPaneNotInRegistry { pane_id }.to_string(),
        format!("floating pane {pane_id:?} has no registry record")
    );
    assert_eq!(
        SessionConsistencyError::DuplicateFloatingPane { pane_id }.to_string(),
        format!("floating panes list pane {pane_id:?} more than once")
    );
    assert_eq!(
        SessionConsistencyError::FloatingPaneInLayout {
            pane_id,
            tab_ids: vec![first_tab_id, second_tab_id],
        }
        .to_string(),
        format!(
            "floating pane {pane_id:?} is also a layout leaf in tabs [{first_tab_id:?}, {second_tab_id:?}]"
        )
    );
}

#[test]
fn client_floating_view_errors_display_the_client_and_the_pane() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    assert_eq!(
        SessionConsistencyError::FloatingViewTargetMissing {
            client_id,
            pane_id,
        }
        .to_string(),
        format!("client {client_id:?} stores a floating view of pane {pane_id:?}, which is not floating")
    );
    assert_eq!(
        SessionConsistencyError::FloatingViewMissing { client_id, pane_id }.to_string(),
        format!("client {client_id:?} holds no floating view of floating pane {pane_id:?}")
    );
    assert_eq!(
        SessionConsistencyError::ShownFloatingPaneMissing { client_id, pane_id }.to_string(),
        format!("client {client_id:?} shows pane {pane_id:?} as floating, and it is not floating")
    );
    assert_eq!(
        SessionConsistencyError::DuplicateShownFloatingPane { client_id, pane_id }.to_string(),
        format!("client {client_id:?} shows floating pane {pane_id:?} more than once")
    );
    assert_eq!(
        SessionConsistencyError::FloatingPaneShownStateMismatch {
            client_id,
            pane_id,
            is_minimized: true,
        }
        .to_string(),
        format!(
            "client {client_id:?} view of floating pane {pane_id:?} says is_minimized true, which its shown floating panes contradict"
        )
    );
    assert_eq!(
        SessionConsistencyError::FloatingFocusNotNewestShown {
            client_id,
            focused_pane_id: None,
            newest_shown_pane_id: Some(pane_id),
        }
        .to_string(),
        format!(
            "client {client_id:?} focuses floating pane None, and the newest floating pane it shows is Some({pane_id:?})"
        )
    );
    assert_eq!(
        SessionConsistencyError::UndrawableFloatingPaneShown { client_id, pane_id }.to_string(),
        format!(
            "client {client_id:?} cannot draw floating panes and shows floating pane {pane_id:?}"
        )
    );
}

#[test]
fn tab_solve_errors_display_the_tab_and_the_sizes() {
    let (client_id, tab_id, pane_id) = (ClientId::new(), TabId::new(), PaneId::new());
    let solved_tab_size = Size {
        column_count: 0,
        row_count: 0,
    };
    let tab_size = Size {
        column_count: 80,
        row_count: 22,
    };

    assert_eq!(
        SessionConsistencyError::TabSolveStale { tab_id }.to_string(),
        format!("tab {tab_id:?} stores a solve that its tree does not give")
    );
    assert_eq!(
        SessionConsistencyError::TabSolveSizeStale {
            tab_id,
            solved_tab_size,
            tab_size,
        }
        .to_string(),
        format!(
            "tab {tab_id:?} is solved at {solved_tab_size:?}, and its tab size is {tab_size:?}"
        )
    );
    assert_eq!(
        SessionConsistencyError::SuppressedPaneFocused {
            client_id,
            tab_id,
            pane_id,
        }
        .to_string(),
        format!(
            "client {client_id:?} focuses pane {pane_id:?} of tab {tab_id:?}, which the tab's solve suppresses"
        )
    );
}
