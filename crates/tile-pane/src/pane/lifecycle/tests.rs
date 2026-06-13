use std::time::SystemTime;

use tile_core::error::{DomainCategory, DomainError, Severity};

use super::{PaneLifecycle, PaneLifecycleEvent};
use crate::error::InvalidTransition;

/// Every lifecycle state, with a fixed payload where one is carried, so a test
/// can sweep the whole state space.
fn all_states() -> [PaneLifecycle; 5] {
    [
        PaneLifecycle::Spawning,
        PaneLifecycle::Running,
        PaneLifecycle::Exited {
            code: Some(0),
            at: SystemTime::UNIX_EPOCH,
        },
        PaneLifecycle::Closing {
            since: SystemTime::UNIX_EPOCH,
        },
        PaneLifecycle::Removed,
    ]
}

/// Every driving event, with fixed payloads, for the same exhaustive sweep.
fn all_events() -> [PaneLifecycleEvent; 5] {
    [
        PaneLifecycleEvent::ProcessStarted,
        PaneLifecycleEvent::ProcessExited {
            code: Some(0),
            at: SystemTime::UNIX_EPOCH,
        },
        PaneLifecycleEvent::CloseRequested {
            since: SystemTime::UNIX_EPOCH,
        },
        PaneLifecycleEvent::Cleaned,
        PaneLifecycleEvent::Respawn,
    ]
}

/// The transitions the spec permits. Mirrors the `transition` match so the
/// sweep can assert nothing outside this set is ever accepted.
fn is_allowed(from: PaneLifecycle, event: PaneLifecycleEvent) -> bool {
    matches!(
        (from, event),
        (PaneLifecycle::Spawning, PaneLifecycleEvent::ProcessStarted)
            | (
                PaneLifecycle::Spawning,
                PaneLifecycleEvent::CloseRequested { .. }
            )
            | (
                PaneLifecycle::Running,
                PaneLifecycleEvent::ProcessExited { .. }
            )
            | (
                PaneLifecycle::Running,
                PaneLifecycleEvent::CloseRequested { .. }
            )
            | (
                PaneLifecycle::Exited { .. },
                PaneLifecycleEvent::CloseRequested { .. }
            )
            | (PaneLifecycle::Closing { .. }, PaneLifecycleEvent::Cleaned)
            | (PaneLifecycle::Exited { .. }, PaneLifecycleEvent::Respawn)
    )
}

#[test]
fn spawning_advances_to_running_when_the_process_starts() {
    let next = PaneLifecycle::Spawning.transition(PaneLifecycleEvent::ProcessStarted);

    assert_eq!(next, Ok(PaneLifecycle::Running));
}

#[test]
fn a_spawning_pane_can_be_closed_before_it_runs() {
    let since = SystemTime::UNIX_EPOCH;

    let next = PaneLifecycle::Spawning.transition(PaneLifecycleEvent::CloseRequested { since });

    // A close can arrive before the child reports started; honour it rather
    // than forcing the pane to run first.
    assert_eq!(next, Ok(PaneLifecycle::Closing { since }));
}

#[test]
fn a_running_pane_exits_carrying_its_code_and_time() {
    let at = SystemTime::UNIX_EPOCH;

    let next =
        PaneLifecycle::Running.transition(PaneLifecycleEvent::ProcessExited { code: Some(2), at });

    assert_eq!(next, Ok(PaneLifecycle::Exited { code: Some(2), at }));
}

#[test]
fn a_running_pane_starts_closing_on_request() {
    let since = SystemTime::UNIX_EPOCH;

    let next = PaneLifecycle::Running.transition(PaneLifecycleEvent::CloseRequested { since });

    assert_eq!(next, Ok(PaneLifecycle::Closing { since }));
}

#[test]
fn a_held_exited_pane_can_later_be_closed() {
    let exited = PaneLifecycle::Exited {
        code: Some(0),
        at: SystemTime::UNIX_EPOCH,
    };
    let since = SystemTime::UNIX_EPOCH;

    let next = exited.transition(PaneLifecycleEvent::CloseRequested { since });

    // The close discards the stale exit payload and adopts the request's time.
    assert_eq!(next, Ok(PaneLifecycle::Closing { since }));
}

#[test]
fn a_closing_pane_is_removed_once_cleaned() {
    let closing = PaneLifecycle::Closing {
        since: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        closing.transition(PaneLifecycleEvent::Cleaned),
        Ok(PaneLifecycle::Removed)
    );
}

#[test]
fn a_dead_pane_respawns_back_to_spawning() {
    let exited = PaneLifecycle::Exited {
        code: Some(1),
        at: SystemTime::UNIX_EPOCH,
    };

    // RespawnShell loops a dead pane back into Spawning to recreate its PTY and
    // child, dropping the prior exit payload.
    assert_eq!(
        exited.transition(PaneLifecycleEvent::Respawn),
        Ok(PaneLifecycle::Spawning)
    );
}

#[test]
fn a_respawned_pane_runs_through_the_normal_start_path() {
    let exited = PaneLifecycle::Exited {
        code: Some(1),
        at: SystemTime::UNIX_EPOCH,
    };

    // Respawn reuses the ordinary Spawning -> Running edge; no shortcut to Running.
    let spawning = exited.transition(PaneLifecycleEvent::Respawn).unwrap();
    let running = spawning.transition(PaneLifecycleEvent::ProcessStarted);

    assert_eq!(running, Ok(PaneLifecycle::Running));
}

#[test]
fn a_removed_pane_rejects_every_event() {
    let from = PaneLifecycle::Removed;

    for event in all_events() {
        assert_eq!(
            from.transition(event),
            Err(InvalidTransition { from, event }),
            "Removed must stay terminal under {event:?}"
        );
    }
}

#[test]
fn a_spawning_pane_cannot_exit_before_it_runs() {
    let from = PaneLifecycle::Spawning;
    let event = PaneLifecycleEvent::ProcessExited {
        code: Some(1),
        at: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        from.transition(event),
        Err(InvalidTransition { from, event })
    );
}

#[test]
fn an_exited_pane_cannot_skip_the_close_transaction() {
    let from = PaneLifecycle::Exited {
        code: Some(0),
        at: SystemTime::UNIX_EPOCH,
    };
    // `Cleaned` is what drives Closing -> Removed; from Exited it is illegal,
    // so a dead pane can never reach Removed without first Closing.
    let event = PaneLifecycleEvent::Cleaned;

    assert_eq!(
        from.transition(event),
        Err(InvalidTransition { from, event })
    );
}

#[test]
fn an_exited_pane_is_never_silently_removed() {
    let from = PaneLifecycle::Exited {
        code: Some(0),
        at: SystemTime::UNIX_EPOCH,
    };

    // Acceptance signal: Exited is retained (HoldOnExit) — no event takes it
    // straight to Removed; only an explicit close, then cleanup, does.
    for event in all_events() {
        assert_ne!(from.transition(event), Ok(PaneLifecycle::Removed));
    }
}

#[test]
fn only_the_specified_transitions_are_accepted() {
    for from in all_states() {
        for event in all_events() {
            let result = from.transition(event);

            if is_allowed(from, event) {
                assert!(result.is_ok(), "{from:?} on {event:?} should be allowed");
            } else {
                assert_eq!(
                    result,
                    Err(InvalidTransition { from, event }),
                    "{from:?} on {event:?} should be rejected"
                );
            }
        }
    }
}

#[test]
fn exactly_seven_transitions_are_legal() {
    let accepted = all_states()
        .into_iter()
        .flat_map(|from| all_events().into_iter().map(move |event| (from, event)))
        .filter(|&(from, event)| from.transition(event).is_ok())
        .count();

    assert_eq!(accepted, 7);
}

#[test]
fn an_invalid_transition_is_a_recoverable_terminal_error() {
    let err = PaneLifecycle::Removed
        .transition(PaneLifecycleEvent::ProcessStarted)
        .unwrap_err();

    assert_eq!(err.category(), DomainCategory::Terminal);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn lifecycle_events_survive_a_serde_round_trip() {
    for event in all_events() {
        let json = serde_json::to_string(&event).expect("serialize");
        let restored: PaneLifecycleEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(event, restored);
    }
}
