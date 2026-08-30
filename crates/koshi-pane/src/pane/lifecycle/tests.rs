//! Tests for the pane lifecycle state machine, covering all valid and invalid
//! transitions between states (Spawning, Running, Exited, Closing, Removed) and
//! the events that drive them (ProcessStarted, ProcessExited, CloseRequested,
//! Cleaned, Respawn).

use std::time::{Duration, SystemTime};

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::PluginId;

use super::{PaneLifecycle, PaneLifecycleEvent};
use crate::error::InvalidTransition;
use crate::pane::state::PaneKind;

/// One instance of each lifecycle state. The payloads differ from the ones in
/// `all_events()`: `Exited` carries `code: Some(7)` at `UNIX_EPOCH + 1s`, and
/// `Closing` carries `since: UNIX_EPOCH + 2s`.
fn all_states() -> [PaneLifecycle; 5] {
    [
        PaneLifecycle::Spawning,
        PaneLifecycle::Running,
        PaneLifecycle::Exited {
            code: Some(7),
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        },
        PaneLifecycle::Closing {
            since: SystemTime::UNIX_EPOCH + Duration::from_secs(2),
        },
        PaneLifecycle::Removed,
    ]
}

/// One instance of each lifecycle event. The payloads differ from the ones in
/// `all_states()`: `ProcessExited` carries `code: Some(3)` at
/// `UNIX_EPOCH + 10s`, and `CloseRequested` carries `since: UNIX_EPOCH + 20s`.
fn all_events() -> [PaneLifecycleEvent; 5] {
    [
        PaneLifecycleEvent::ProcessStarted,
        PaneLifecycleEvent::ProcessExited {
            code: Some(3),
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        },
        PaneLifecycleEvent::CloseRequested {
            since: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
        },
        PaneLifecycleEvent::Cleaned,
        PaneLifecycleEvent::Respawn,
    ]
}

/// The state that a legal `from` × `event` pair reaches, with the payload
/// taken from `event`. `None` for every illegal pair.
fn expected_next(from: PaneLifecycle, event: PaneLifecycleEvent) -> Option<PaneLifecycle> {
    match (from, event) {
        (PaneLifecycle::Spawning, PaneLifecycleEvent::ProcessStarted) => {
            Some(PaneLifecycle::Running)
        }
        (
            PaneLifecycle::Spawning | PaneLifecycle::Running | PaneLifecycle::Exited { .. },
            PaneLifecycleEvent::CloseRequested { since },
        ) => Some(PaneLifecycle::Closing { since }),
        (PaneLifecycle::Running, PaneLifecycleEvent::ProcessExited { code, at }) => {
            Some(PaneLifecycle::Exited { code, at })
        }
        (PaneLifecycle::Exited { .. }, PaneLifecycleEvent::Respawn) => {
            Some(PaneLifecycle::Spawning)
        }
        (PaneLifecycle::Closing { .. }, PaneLifecycleEvent::Cleaned) => {
            Some(PaneLifecycle::Removed)
        }
        _ => None,
    }
}

#[test]
fn spawning_advances_to_running_when_the_process_starts() {
    let next =
        PaneLifecycle::Spawning.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal);

    assert_eq!(next, Ok(PaneLifecycle::Running));
}

#[test]
fn a_spawning_pane_can_be_closed_before_it_runs() {
    let since = SystemTime::UNIX_EPOCH;

    let next = PaneLifecycle::Spawning.transition(
        PaneLifecycleEvent::CloseRequested { since },
        PaneKind::Terminal,
    );

    assert_eq!(next, Ok(PaneLifecycle::Closing { since }));
}

#[test]
fn a_running_pane_exits_carrying_its_code_and_time() {
    let at = SystemTime::UNIX_EPOCH;

    let next = PaneLifecycle::Running.transition(
        PaneLifecycleEvent::ProcessExited { code: Some(2), at },
        PaneKind::Terminal,
    );

    assert_eq!(next, Ok(PaneLifecycle::Exited { code: Some(2), at }));
}

#[test]
fn a_running_pane_starts_closing_on_request() {
    let since = SystemTime::UNIX_EPOCH;

    let next = PaneLifecycle::Running.transition(
        PaneLifecycleEvent::CloseRequested { since },
        PaneKind::Terminal,
    );

    assert_eq!(next, Ok(PaneLifecycle::Closing { since }));
}

#[test]
fn a_held_exited_pane_can_later_be_closed() {
    let exited = PaneLifecycle::Exited {
        code: Some(0),
        at: SystemTime::UNIX_EPOCH,
    };
    let since = SystemTime::UNIX_EPOCH + Duration::from_secs(4);

    let next = exited.transition(
        PaneLifecycleEvent::CloseRequested { since },
        PaneKind::Terminal,
    );

    // `Closing` carries the request time, not the exit time.
    assert_eq!(next, Ok(PaneLifecycle::Closing { since }));
}

#[test]
fn a_closing_pane_is_removed_once_cleaned() {
    let closing = PaneLifecycle::Closing {
        since: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        closing.transition(PaneLifecycleEvent::Cleaned, PaneKind::Terminal),
        Ok(PaneLifecycle::Removed)
    );
}

#[test]
fn a_dead_pane_respawns_back_to_spawning() {
    let exited = PaneLifecycle::Exited {
        code: Some(1),
        at: SystemTime::UNIX_EPOCH,
    };

    // `Spawning` carries no payload: the exit code and time are gone.
    assert_eq!(
        exited.transition(PaneLifecycleEvent::Respawn, PaneKind::Terminal),
        Ok(PaneLifecycle::Spawning)
    );
}

#[test]
fn a_respawned_pane_runs_through_the_normal_start_path() {
    let exited = PaneLifecycle::Exited {
        code: Some(1),
        at: SystemTime::UNIX_EPOCH,
    };

    // Respawn lands in `Spawning`; `ProcessStarted` then moves it to `Running`.
    let spawning = exited
        .transition(PaneLifecycleEvent::Respawn, PaneKind::Terminal)
        .unwrap();
    let running = spawning.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal);

    assert_eq!(running, Ok(PaneLifecycle::Running));
}

#[test]
fn a_close_during_spawn_wins_over_a_late_child_exit() {
    // The pane is closed while `Spawning`; the child then exits anyway.
    let since = SystemTime::UNIX_EPOCH;
    let closing = PaneLifecycle::Spawning
        .transition(
            PaneLifecycleEvent::CloseRequested { since },
            PaneKind::Terminal,
        )
        .unwrap();
    assert_eq!(closing, PaneLifecycle::Closing { since });

    // The late exit is rejected; the state stays `Closing`.
    let late_exit = PaneLifecycleEvent::ProcessExited {
        code: Some(0),
        at: since,
    };
    assert_eq!(
        closing.transition(late_exit, PaneKind::Terminal),
        Err(InvalidTransition {
            from: closing,
            event: late_exit,
            kind: PaneKind::Terminal,
        })
    );

    // The close still completes to `Removed`.
    assert_eq!(
        closing.transition(PaneLifecycleEvent::Cleaned, PaneKind::Terminal),
        Ok(PaneLifecycle::Removed)
    );
}

#[test]
fn a_second_close_request_while_closing_is_rejected() {
    let closing = PaneLifecycle::Closing {
        since: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    };
    let event = PaneLifecycleEvent::CloseRequested {
        since: SystemTime::UNIX_EPOCH + Duration::from_secs(2),
    };

    assert_eq!(
        closing.transition(event, PaneKind::Terminal),
        Err(InvalidTransition {
            from: closing,
            event,
            kind: PaneKind::Terminal,
        })
    );
}

#[test]
fn a_running_pane_rejects_a_second_process_start() {
    assert_eq!(
        PaneLifecycle::Running.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal),
        Err(InvalidTransition {
            from: PaneLifecycle::Running,
            event: PaneLifecycleEvent::ProcessStarted,
            kind: PaneKind::Terminal,
        })
    );
}

#[test]
fn a_running_pane_cannot_respawn() {
    assert_eq!(
        PaneLifecycle::Running.transition(PaneLifecycleEvent::Respawn, PaneKind::Terminal),
        Err(InvalidTransition {
            from: PaneLifecycle::Running,
            event: PaneLifecycleEvent::Respawn,
            kind: PaneKind::Terminal,
        })
    );
}

#[test]
fn a_removed_pane_rejects_every_event() {
    let from = PaneLifecycle::Removed;

    for event in all_events() {
        assert_eq!(
            from.transition(event, PaneKind::Terminal),
            Err(InvalidTransition {
                from,
                event,
                kind: PaneKind::Terminal
            }),
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
        from.transition(event, PaneKind::Terminal),
        Err(InvalidTransition {
            from,
            event,
            kind: PaneKind::Terminal
        })
    );
}

#[test]
fn an_exited_pane_cannot_skip_the_close_transaction() {
    let from = PaneLifecycle::Exited {
        code: Some(0),
        at: SystemTime::UNIX_EPOCH,
    };
    // `Cleaned` is legal only from `Closing`.
    let event = PaneLifecycleEvent::Cleaned;

    assert_eq!(
        from.transition(event, PaneKind::Terminal),
        Err(InvalidTransition {
            from,
            event,
            kind: PaneKind::Terminal
        })
    );
}

#[test]
fn an_exited_pane_is_never_silently_removed() {
    let from = PaneLifecycle::Exited {
        code: Some(0),
        at: SystemTime::UNIX_EPOCH,
    };

    // No single event moves `Exited` to `Removed`. The path is
    // `Exited` -> `CloseRequested` -> `Closing` -> `Cleaned` -> `Removed`.
    for event in all_events() {
        assert_ne!(
            from.transition(event, PaneKind::Terminal),
            Ok(PaneLifecycle::Removed)
        );
    }
}

#[test]
fn only_the_specified_transitions_are_accepted() {
    for from in all_states() {
        for event in all_events() {
            let expected = match expected_next(from, event) {
                Some(next) => Ok(next),
                None => Err(InvalidTransition {
                    from,
                    event,
                    kind: PaneKind::Terminal,
                }),
            };

            assert_eq!(
                from.transition(event, PaneKind::Terminal),
                expected,
                "{from:?} on {event:?}"
            );
        }
    }
}

#[test]
fn exactly_seven_transitions_are_legal() {
    let accepted = all_states()
        .into_iter()
        .flat_map(|from| all_events().into_iter().map(move |event| (from, event)))
        .filter(|&(from, event)| from.transition(event, PaneKind::Terminal).is_ok())
        .count();

    assert_eq!(accepted, 7);
}

#[test]
fn an_exit_code_passes_through_unchanged_at_the_i32_bounds() {
    let at = SystemTime::UNIX_EPOCH;

    for code in [i32::MIN, -1, 0, 1, i32::MAX] {
        assert_eq!(
            PaneLifecycle::Running.transition(
                PaneLifecycleEvent::ProcessExited {
                    code: Some(code),
                    at
                },
                PaneKind::Terminal,
            ),
            Ok(PaneLifecycle::Exited {
                code: Some(code),
                at
            }),
            "exit code {code}"
        );
    }
}

#[test]
fn an_invalid_transition_is_recoverable_and_classified_by_pane_kind() {
    // The error's domain follows the pane's kind.
    let terminal = PaneLifecycle::Removed
        .transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal)
        .unwrap_err();
    assert_eq!(terminal.category(), DomainCategory::Terminal);
    assert_eq!(terminal.severity(), Severity::Recoverable);

    let plugin = PaneLifecycle::Removed
        .transition(
            PaneLifecycleEvent::ProcessStarted,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
        )
        .unwrap_err();
    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(plugin.severity(), Severity::Recoverable);
}

#[test]
fn a_signal_killed_pane_exits_with_no_code() {
    let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);

    let next = PaneLifecycle::Running.transition(
        PaneLifecycleEvent::ProcessExited { code: None, at },
        PaneKind::Terminal,
    );

    // `code` stays `None`; the state does not stand in a `0`.
    assert_eq!(next, Ok(PaneLifecycle::Exited { code: None, at }));
}

#[test]
fn lifecycle_events_survive_a_serde_round_trip() {
    for event in all_events() {
        let json = serde_json::to_string(&event).expect("serialize");
        let restored: PaneLifecycleEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(event, restored);
    }
}

#[test]
fn lifecycle_states_survive_a_serde_round_trip() {
    for state in all_states() {
        let json = serde_json::to_string(&state).expect("serialize");
        let restored: PaneLifecycle = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(state, restored);
    }
}

#[test]
fn unit_lifecycle_states_serialize_as_their_variant_names() {
    assert_eq!(
        serde_json::to_string(&PaneLifecycle::Spawning).expect("serialize"),
        r#""Spawning""#
    );
    assert_eq!(
        serde_json::to_string(&PaneLifecycle::Running).expect("serialize"),
        r#""Running""#
    );
    assert_eq!(
        serde_json::to_string(&PaneLifecycle::Removed).expect("serialize"),
        r#""Removed""#
    );
}

#[test]
fn payload_lifecycle_states_serialize_their_fields_with_times_as_seconds_and_nanos() {
    let exited = PaneLifecycle::Exited {
        code: None,
        at: SystemTime::UNIX_EPOCH + Duration::new(5, 40),
    };
    let closing = PaneLifecycle::Closing {
        since: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        serde_json::to_string(&exited).expect("serialize"),
        r#"{"Exited":{"code":null,"at":{"secs_since_epoch":5,"nanos_since_epoch":40}}}"#
    );
    assert_eq!(
        serde_json::to_string(&closing).expect("serialize"),
        r#"{"Closing":{"since":{"secs_since_epoch":0,"nanos_since_epoch":0}}}"#
    );
}

#[test]
fn unit_lifecycle_events_serialize_as_their_variant_names() {
    assert_eq!(
        serde_json::to_string(&PaneLifecycleEvent::ProcessStarted).expect("serialize"),
        r#""ProcessStarted""#
    );
    assert_eq!(
        serde_json::to_string(&PaneLifecycleEvent::Cleaned).expect("serialize"),
        r#""Cleaned""#
    );
    assert_eq!(
        serde_json::to_string(&PaneLifecycleEvent::Respawn).expect("serialize"),
        r#""Respawn""#
    );
}

#[test]
fn an_unknown_lifecycle_state_fails_to_deserialize() {
    let error = serde_json::from_str::<PaneLifecycle>(r#""Zombie""#).expect_err("unknown variant");

    assert_eq!(
        error.to_string(),
        "unknown variant `Zombie`, expected one of `Spawning`, `Running`, `Exited`, `Closing`, `Removed` at line 1 column 8"
    );
}

#[test]
fn an_exited_state_without_a_time_fails_to_deserialize() {
    let error = serde_json::from_str::<PaneLifecycle>(r#"{"Exited":{"code":0}}"#)
        .expect_err("missing field");

    assert_eq!(error.to_string(), "missing field `at` at line 1 column 20");
}
