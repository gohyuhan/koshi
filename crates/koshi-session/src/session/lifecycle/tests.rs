//! Tests for the session lifecycle state machine.
//!
//! Verifies that [`SessionLifecycle::transition`] accepts exactly seven
//! valid transitions and rejects all others. Tests enumerate the full
//! state × event matrix and confirm that serialization round-trips
//! preserve lifecycle states and events.

use koshi_core::error::{DomainCategory, DomainError, Severity};

use super::{SessionLifecycle, SessionLifecycleEvent};

/// Every state and every event, for exhaustive sweeps.
const STATES: [SessionLifecycle; 5] = [
    SessionLifecycle::Starting,
    SessionLifecycle::Running,
    SessionLifecycle::Detaching,
    SessionLifecycle::Stopping,
    SessionLifecycle::Stopped,
];

const EVENTS: [SessionLifecycleEvent; 5] = [
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
    let legal = STATES
        .iter()
        .flat_map(|&state| EVENTS.iter().map(move |&event| state.transition(event)))
        .filter(|outcome| outcome.is_ok())
        .count();
    assert_eq!(legal, 7);
}

#[test]
fn stopped_is_terminal() {
    for &event in &EVENTS {
        assert!(
            SessionLifecycle::Stopped.transition(event).is_err(),
            "Stopped must reject {event:?}"
        );
    }
}

#[test]
fn an_illegal_transition_reports_its_origin() {
    // Completing a stop that was never requested is illegal.
    let err = SessionLifecycle::Running
        .transition(SessionLifecycleEvent::StopCompleted)
        .expect_err("a running session cannot complete a stop");

    assert_eq!(err.from, SessionLifecycle::Running);
    assert_eq!(err.event, SessionLifecycleEvent::StopCompleted);
}

#[test]
fn an_invalid_transition_is_a_recoverable_session_error() {
    let err = SessionLifecycle::Stopped
        .transition(SessionLifecycleEvent::FirstTabCreated)
        .expect_err("a stopped session rejects every event");

    assert_eq!(err.category(), DomainCategory::Session);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn lifecycle_states_survive_a_serde_round_trip() {
    for &state in &STATES {
        let json = serde_json::to_string(&state).expect("serialize");
        let restored: SessionLifecycle = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(state, restored);
    }
}

#[test]
fn lifecycle_events_survive_a_serde_round_trip() {
    for &event in &EVENTS {
        let json = serde_json::to_string(&event).expect("serialize");
        let restored: SessionLifecycleEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, restored);
    }
}
