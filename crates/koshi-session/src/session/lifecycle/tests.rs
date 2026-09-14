//! Tests for the session lifecycle state machine.
//!
//! Verifies that [`SessionLifecycle::transition`] accepts exactly seven
//! valid transitions and rejects all others. Tests enumerate the full
//! state × lifecycle-event matrix with the exact outcome of every pair, walk one
//! session from `Starting` to `Stopped`, and pin the stored form of every
//! session state, session event and [`TabLifecycle`] state to its bare
//! variant name.

use koshi_core::error::{DomainCategory, DomainError, Severity};

use super::{SessionLifecycle, SessionLifecycleEvent, TabLifecycle};
use crate::error::InvalidTransition;

/// Every session lifecycle state and event, for exhaustive sweeps.
const SESSION_LIFECYCLE_STATES: [SessionLifecycle; 5] = [
    SessionLifecycle::Starting,
    SessionLifecycle::Running,
    SessionLifecycle::Detaching,
    SessionLifecycle::Stopping,
    SessionLifecycle::Stopped,
];

const SESSION_LIFECYCLE_EVENTS: [SessionLifecycleEvent; 5] = [
    SessionLifecycleEvent::FirstTabCreated,
    SessionLifecycleEvent::LastClientDetached,
    SessionLifecycleEvent::ClientAttached,
    SessionLifecycleEvent::StopRequested,
    SessionLifecycleEvent::StopCompleted,
];

#[test]
fn the_first_tab_starts_the_session() {
    assert_eq!(
        SessionLifecycle::Starting.transition(SessionLifecycleEvent::FirstTabCreated),
        Ok(SessionLifecycle::Running)
    );
}

#[test]
fn losing_the_last_client_parks_a_running_session() {
    assert_eq!(
        SessionLifecycle::Running.transition(SessionLifecycleEvent::LastClientDetached),
        Ok(SessionLifecycle::Detaching)
    );
}

#[test]
fn attaching_a_client_resumes_a_detached_session() {
    assert_eq!(
        SessionLifecycle::Detaching.transition(SessionLifecycleEvent::ClientAttached),
        Ok(SessionLifecycle::Running)
    );
}

#[test]
fn a_running_session_can_be_asked_to_stop() {
    assert_eq!(
        SessionLifecycle::Running.transition(SessionLifecycleEvent::StopRequested),
        Ok(SessionLifecycle::Stopping)
    );
}

#[test]
fn a_detached_session_can_be_asked_to_stop() {
    assert_eq!(
        SessionLifecycle::Detaching.transition(SessionLifecycleEvent::StopRequested),
        Ok(SessionLifecycle::Stopping)
    );
}

#[test]
fn a_session_can_stop_before_its_first_tab() {
    assert_eq!(
        SessionLifecycle::Starting.transition(SessionLifecycleEvent::StopRequested),
        Ok(SessionLifecycle::Stopping)
    );
}

#[test]
fn a_stopping_session_completes_to_stopped() {
    assert_eq!(
        SessionLifecycle::Stopping.transition(SessionLifecycleEvent::StopCompleted),
        Ok(SessionLifecycle::Stopped)
    );
}

#[test]
fn exactly_seven_transitions_are_legal() {
    let legal_transition_count = SESSION_LIFECYCLE_STATES
        .iter()
        .flat_map(|&session_lifecycle| {
            SESSION_LIFECYCLE_EVENTS
                .iter()
                .map(move |&lifecycle_event| session_lifecycle.transition(lifecycle_event))
        })
        .filter(|transition_result| transition_result.is_ok())
        .count();
    assert_eq!(legal_transition_count, 7);
}

#[test]
fn stopped_is_terminal() {
    for &lifecycle_event in &SESSION_LIFECYCLE_EVENTS {
        assert_eq!(
            SessionLifecycle::Stopped.transition(lifecycle_event),
            Err(InvalidTransition {
                previous_lifecycle: SessionLifecycle::Stopped,
                lifecycle_event,
            }),
            "Stopped must reject {lifecycle_event:?}"
        );
    }
}

/// Every `(session_lifecycle, lifecycle_event)` pair `transition` accepts, with the state it
/// yields. A pair absent from this table is rejected.
const LEGAL_SESSION_LIFECYCLE_TRANSITIONS: [(
    SessionLifecycle,
    SessionLifecycleEvent,
    SessionLifecycle,
); 7] = [
    (
        SessionLifecycle::Starting,
        SessionLifecycleEvent::FirstTabCreated,
        SessionLifecycle::Running,
    ),
    (
        SessionLifecycle::Running,
        SessionLifecycleEvent::LastClientDetached,
        SessionLifecycle::Detaching,
    ),
    (
        SessionLifecycle::Detaching,
        SessionLifecycleEvent::ClientAttached,
        SessionLifecycle::Running,
    ),
    (
        SessionLifecycle::Starting,
        SessionLifecycleEvent::StopRequested,
        SessionLifecycle::Stopping,
    ),
    (
        SessionLifecycle::Running,
        SessionLifecycleEvent::StopRequested,
        SessionLifecycle::Stopping,
    ),
    (
        SessionLifecycle::Detaching,
        SessionLifecycleEvent::StopRequested,
        SessionLifecycle::Stopping,
    ),
    (
        SessionLifecycle::Stopping,
        SessionLifecycleEvent::StopCompleted,
        SessionLifecycle::Stopped,
    ),
];

#[test]
fn every_state_and_event_pair_has_a_fixed_outcome() {
    for session_lifecycle in SESSION_LIFECYCLE_STATES {
        for lifecycle_event in SESSION_LIFECYCLE_EVENTS {
            let expected_lifecycle = match LEGAL_SESSION_LIFECYCLE_TRANSITIONS.iter().find(
                |(previous_lifecycle, transition_event, _)| {
                    *previous_lifecycle == session_lifecycle && *transition_event == lifecycle_event
                },
            ) {
                Some(&(_, _, next_lifecycle)) => Ok(next_lifecycle),
                None => Err(InvalidTransition {
                    previous_lifecycle: session_lifecycle,
                    lifecycle_event,
                }),
            };
            assert_eq!(
                session_lifecycle.transition(lifecycle_event),
                expected_lifecycle,
                "{session_lifecycle:?} on {lifecycle_event:?}"
            );
        }
    }
}

#[test]
fn a_stop_request_is_rejected_once_the_session_is_already_stopping() {
    let stopping = SessionLifecycle::Running
        .transition(SessionLifecycleEvent::StopRequested)
        .expect("a running session accepts a stop request");
    assert_eq!(stopping, SessionLifecycle::Stopping);

    assert_eq!(
        stopping.transition(SessionLifecycleEvent::StopRequested),
        Err(InvalidTransition {
            previous_lifecycle: SessionLifecycle::Stopping,
            lifecycle_event: SessionLifecycleEvent::StopRequested,
        })
    );
}

#[test]
fn a_session_walks_start_to_detach_to_revive_to_stop() {
    let mut session_lifecycle = SessionLifecycle::Starting;
    for (lifecycle_event, expected_lifecycle) in [
        (
            SessionLifecycleEvent::FirstTabCreated,
            SessionLifecycle::Running,
        ),
        (
            SessionLifecycleEvent::LastClientDetached,
            SessionLifecycle::Detaching,
        ),
        (
            SessionLifecycleEvent::ClientAttached,
            SessionLifecycle::Running,
        ),
        (
            SessionLifecycleEvent::StopRequested,
            SessionLifecycle::Stopping,
        ),
        (
            SessionLifecycleEvent::StopCompleted,
            SessionLifecycle::Stopped,
        ),
    ] {
        session_lifecycle = session_lifecycle
            .transition(lifecycle_event)
            .unwrap_or_else(|transition_error| panic!("{transition_error} is a legal step"));
        assert_eq!(session_lifecycle, expected_lifecycle);
    }
}

#[test]
fn an_illegal_transition_reports_its_origin() {
    // Completing a stop that was never requested is illegal.
    let transition_error = SessionLifecycle::Running
        .transition(SessionLifecycleEvent::StopCompleted)
        .expect_err("a running session cannot complete a stop");

    assert_eq!(
        transition_error.previous_lifecycle,
        SessionLifecycle::Running
    );
    assert_eq!(
        transition_error.lifecycle_event,
        SessionLifecycleEvent::StopCompleted
    );
}

#[test]
fn an_invalid_transition_is_a_recoverable_session_error() {
    let transition_error = SessionLifecycle::Stopped
        .transition(SessionLifecycleEvent::FirstTabCreated)
        .expect_err("a stopped session rejects every event");

    assert_eq!(transition_error.category(), DomainCategory::Session);
    assert_eq!(transition_error.get_severity(), Severity::Recoverable);
}

#[test]
fn lifecycle_states_survive_a_serde_round_trip() {
    for &session_lifecycle in &SESSION_LIFECYCLE_STATES {
        let lifecycle_json = serde_json::to_string(&session_lifecycle).expect("serialize");
        let restored_lifecycle: SessionLifecycle =
            serde_json::from_str(&lifecycle_json).expect("deserialize");
        assert_eq!(session_lifecycle, restored_lifecycle);
    }
}

#[test]
fn lifecycle_events_survive_a_serde_round_trip() {
    for &lifecycle_event in &SESSION_LIFECYCLE_EVENTS {
        let lifecycle_json = serde_json::to_string(&lifecycle_event).expect("serialize");
        let restored_lifecycle_event: SessionLifecycleEvent =
            serde_json::from_str(&lifecycle_json).expect("deserialize");
        assert_eq!(lifecycle_event, restored_lifecycle_event);
    }
}

#[test]
fn a_lifecycle_state_is_stored_as_its_bare_variant_name() {
    for (session_lifecycle, lifecycle_json) in [
        (SessionLifecycle::Starting, "\"Starting\""),
        (SessionLifecycle::Running, "\"Running\""),
        (SessionLifecycle::Detaching, "\"Detaching\""),
        (SessionLifecycle::Stopping, "\"Stopping\""),
        (SessionLifecycle::Stopped, "\"Stopped\""),
    ] {
        assert_eq!(
            serde_json::to_string(&session_lifecycle).expect("serialize"),
            lifecycle_json
        );
        assert_eq!(
            serde_json::from_str::<SessionLifecycle>(lifecycle_json).expect("deserialize"),
            session_lifecycle
        );
    }
}

#[test]
fn a_lifecycle_event_is_stored_as_its_bare_variant_name() {
    for (lifecycle_event, lifecycle_json) in [
        (
            SessionLifecycleEvent::FirstTabCreated,
            "\"FirstTabCreated\"",
        ),
        (
            SessionLifecycleEvent::LastClientDetached,
            "\"LastClientDetached\"",
        ),
        (SessionLifecycleEvent::ClientAttached, "\"ClientAttached\""),
        (SessionLifecycleEvent::StopRequested, "\"StopRequested\""),
        (SessionLifecycleEvent::StopCompleted, "\"StopCompleted\""),
    ] {
        assert_eq!(
            serde_json::to_string(&lifecycle_event).expect("serialize"),
            lifecycle_json
        );
        assert_eq!(
            serde_json::from_str::<SessionLifecycleEvent>(lifecycle_json).expect("deserialize"),
            lifecycle_event
        );
    }
}

#[test]
fn a_tab_lifecycle_state_is_stored_as_its_bare_variant_name() {
    for (tab_lifecycle, lifecycle_json) in [
        (TabLifecycle::Creating, "\"Creating\""),
        (TabLifecycle::Active, "\"Active\""),
        (TabLifecycle::Inactive, "\"Inactive\""),
        (TabLifecycle::Closing, "\"Closing\""),
        (TabLifecycle::Closed, "\"Closed\""),
    ] {
        assert_eq!(
            serde_json::to_string(&tab_lifecycle).expect("serialize"),
            lifecycle_json
        );
        assert_eq!(
            serde_json::from_str::<TabLifecycle>(lifecycle_json).expect("deserialize"),
            tab_lifecycle
        );
    }
}

#[test]
fn a_lifecycle_state_this_build_does_not_know_is_rejected() {
    let error = serde_json::from_str::<SessionLifecycle>("\"Paused\"")
        .expect_err("`Paused` is not a state this build knows");

    assert_eq!(error.classify(), serde_json::error::Category::Data);
    assert_eq!(
        error.to_string(),
        "unknown variant `Paused`, expected one of `Starting`, `Running`, `Detaching`, `Stopping`, `Stopped` at line 1 column 8"
    );
}
