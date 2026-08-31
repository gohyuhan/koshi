//! Tests for the session lifecycle state machine.
//!
//! Verifies that [`SessionLifecycle::transition`] accepts exactly seven
//! valid transitions and rejects all others. Tests enumerate the full
//! state × event matrix with the exact outcome of every pair, walk one
//! session from `Starting` to `Stopped`, and pin the stored form of every
//! session state, session event and [`TabLifecycle`] state to its bare
//! variant name.

use koshi_core::error::{DomainCategory, DomainError, Severity};

use super::{SessionLifecycle, SessionLifecycleEvent, TabLifecycle};
use crate::error::InvalidTransition;

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
        assert_eq!(
            SessionLifecycle::Stopped.transition(event),
            Err(InvalidTransition {
                from: SessionLifecycle::Stopped,
                event,
            }),
            "Stopped must reject {event:?}"
        );
    }
}

/// Every `(state, event)` pair `transition` accepts, with the state it
/// yields. A pair absent from this table is rejected.
const LEGAL: [(SessionLifecycle, SessionLifecycleEvent, SessionLifecycle); 7] = [
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
    for state in STATES {
        for event in EVENTS {
            let expected = match LEGAL
                .iter()
                .find(|(from, on, _)| *from == state && *on == event)
            {
                Some(&(_, _, next)) => Ok(next),
                None => Err(InvalidTransition { from: state, event }),
            };
            assert_eq!(state.transition(event), expected, "{state:?} on {event:?}");
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
            from: SessionLifecycle::Stopping,
            event: SessionLifecycleEvent::StopRequested,
        })
    );
}

#[test]
fn a_session_walks_start_to_detach_to_revive_to_stop() {
    let mut state = SessionLifecycle::Starting;
    for (event, expected) in [
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
        state = state
            .transition(event)
            .unwrap_or_else(|err| panic!("{err} is a legal step"));
        assert_eq!(state, expected);
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

#[test]
fn a_lifecycle_state_is_stored_as_its_bare_variant_name() {
    for (state, json) in [
        (SessionLifecycle::Starting, "\"Starting\""),
        (SessionLifecycle::Running, "\"Running\""),
        (SessionLifecycle::Detaching, "\"Detaching\""),
        (SessionLifecycle::Stopping, "\"Stopping\""),
        (SessionLifecycle::Stopped, "\"Stopped\""),
    ] {
        assert_eq!(serde_json::to_string(&state).expect("serialize"), json);
        assert_eq!(
            serde_json::from_str::<SessionLifecycle>(json).expect("deserialize"),
            state
        );
    }
}

#[test]
fn a_lifecycle_event_is_stored_as_its_bare_variant_name() {
    for (event, json) in [
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
        assert_eq!(serde_json::to_string(&event).expect("serialize"), json);
        assert_eq!(
            serde_json::from_str::<SessionLifecycleEvent>(json).expect("deserialize"),
            event
        );
    }
}

#[test]
fn a_tab_lifecycle_state_is_stored_as_its_bare_variant_name() {
    for (state, json) in [
        (TabLifecycle::Creating, "\"Creating\""),
        (TabLifecycle::Active, "\"Active\""),
        (TabLifecycle::Inactive, "\"Inactive\""),
        (TabLifecycle::Closing, "\"Closing\""),
        (TabLifecycle::Closed, "\"Closed\""),
    ] {
        assert_eq!(serde_json::to_string(&state).expect("serialize"), json);
        assert_eq!(
            serde_json::from_str::<TabLifecycle>(json).expect("deserialize"),
            state
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
