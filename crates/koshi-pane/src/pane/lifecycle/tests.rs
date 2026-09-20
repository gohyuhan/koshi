//! Tests for the pane lifecycle state machine, covering all valid and invalid
//! transitions between states (Spawning, Running, Exited, Closing, Removed) and
//! the events that drive them (ProcessStarted, ProcessExited, CloseRequested,
//! Cleaned).

use std::time::{Duration, SystemTime};

use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::ids::PluginId;

use super::{PaneLifecycle, PaneLifecycleEvent};
use crate::error::InvalidTransitionError;
use crate::pane::state::PaneKind;

/// One instance of each lifecycle state. The payloads differ from the ones in
/// `list_lifecycle_events()`: `Exited` carries `exit_code: Some(7)` and
/// `exited_at = UNIX_EPOCH + 1s`; `Closing` carries `close_requested_at = UNIX_EPOCH + 2s`.
fn list_lifecycle_states() -> [PaneLifecycle; 5] {
    [
        PaneLifecycle::Spawning,
        PaneLifecycle::Running,
        PaneLifecycle::Exited {
            exit_code: Some(7),
            exited_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        },
        PaneLifecycle::Closing {
            close_requested_at: SystemTime::UNIX_EPOCH + Duration::from_secs(2),
        },
        PaneLifecycle::Removed,
    ]
}

/// One instance of each lifecycle event. The payloads differ from the ones in
/// `list_lifecycle_states()`: `ProcessExited` carries `exit_code: Some(3)` and
/// `exited_at = UNIX_EPOCH + 10s`; `CloseRequested` carries `close_requested_at = UNIX_EPOCH + 20s`.
fn list_lifecycle_events() -> [PaneLifecycleEvent; 4] {
    [
        PaneLifecycleEvent::ProcessStarted,
        PaneLifecycleEvent::ProcessExited {
            exit_code: Some(3),
            exited_at: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        },
        PaneLifecycleEvent::CloseRequested {
            close_requested_at: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
        },
        PaneLifecycleEvent::Cleaned,
    ]
}

/// The state that a legal previous-lifecycle × lifecycle-event pair reaches,
/// with the payload taken from the lifecycle event. `None` for every illegal pair.
fn compute_expected_lifecycle(
    previous_lifecycle: PaneLifecycle,
    lifecycle_event: PaneLifecycleEvent,
) -> Option<PaneLifecycle> {
    match (previous_lifecycle, lifecycle_event) {
        (PaneLifecycle::Spawning, PaneLifecycleEvent::ProcessStarted) => {
            Some(PaneLifecycle::Running)
        }
        (
            PaneLifecycle::Spawning | PaneLifecycle::Running | PaneLifecycle::Exited { .. },
            PaneLifecycleEvent::CloseRequested { close_requested_at },
        ) => Some(PaneLifecycle::Closing { close_requested_at }),
        (
            PaneLifecycle::Running,
            PaneLifecycleEvent::ProcessExited {
                exit_code,
                exited_at,
            },
        ) => Some(PaneLifecycle::Exited {
            exit_code,
            exited_at,
        }),
        (PaneLifecycle::Closing { .. }, PaneLifecycleEvent::Cleaned) => {
            Some(PaneLifecycle::Removed)
        }
        _ => None,
    }
}

#[test]
fn spawning_advances_to_running_when_the_process_starts() {
    let transition_result =
        PaneLifecycle::Spawning.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal);

    assert_eq!(transition_result, Ok(PaneLifecycle::Running));
}

#[test]
fn a_spawning_pane_can_be_closed_before_it_runs() {
    let close_requested_at = SystemTime::UNIX_EPOCH;

    let transition_result = PaneLifecycle::Spawning.transition(
        PaneLifecycleEvent::CloseRequested { close_requested_at },
        PaneKind::Terminal,
    );

    assert_eq!(
        transition_result,
        Ok(PaneLifecycle::Closing { close_requested_at })
    );
}

#[test]
fn a_running_pane_exits_carrying_its_code_and_time() {
    let exited_at = SystemTime::UNIX_EPOCH;

    let transition_result = PaneLifecycle::Running.transition(
        PaneLifecycleEvent::ProcessExited {
            exit_code: Some(2),
            exited_at,
        },
        PaneKind::Terminal,
    );

    assert_eq!(
        transition_result,
        Ok(PaneLifecycle::Exited {
            exit_code: Some(2),
            exited_at
        })
    );
}

#[test]
fn a_running_pane_starts_closing_on_request() {
    let close_requested_at = SystemTime::UNIX_EPOCH;

    let transition_result = PaneLifecycle::Running.transition(
        PaneLifecycleEvent::CloseRequested { close_requested_at },
        PaneKind::Terminal,
    );

    assert_eq!(
        transition_result,
        Ok(PaneLifecycle::Closing { close_requested_at })
    );
}

#[test]
fn a_held_exited_pane_can_later_be_closed() {
    let exited = PaneLifecycle::Exited {
        exit_code: Some(0),
        exited_at: SystemTime::UNIX_EPOCH,
    };
    let close_requested_at = SystemTime::UNIX_EPOCH + Duration::from_secs(4);

    let transition_result = exited.transition(
        PaneLifecycleEvent::CloseRequested { close_requested_at },
        PaneKind::Terminal,
    );

    // `Closing` carries the request time, not the exit time.
    assert_eq!(
        transition_result,
        Ok(PaneLifecycle::Closing { close_requested_at })
    );
}

#[test]
fn a_closing_pane_is_removed_once_cleaned() {
    let closing = PaneLifecycle::Closing {
        close_requested_at: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        closing.transition(PaneLifecycleEvent::Cleaned, PaneKind::Terminal),
        Ok(PaneLifecycle::Removed)
    );
}

#[test]
fn a_dead_pane_never_returns_to_a_live_state() {
    let exited = PaneLifecycle::Exited {
        exit_code: Some(1),
        exited_at: SystemTime::UNIX_EPOCH,
    };

    // `CloseRequested` is the only way out of `Exited`. Restarting the child in
    // place is rejected, so the exit code and time stay readable until the
    // close.
    assert_eq!(
        exited.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal),
        Err(InvalidTransitionError {
            previous_lifecycle: exited,
            lifecycle_event: PaneLifecycleEvent::ProcessStarted,
            pane_kind: PaneKind::Terminal,
        })
    );
}

#[test]
fn a_close_during_spawn_wins_over_a_late_child_exit() {
    // The pane is closed while `Spawning`; the child then exits anyway.
    let close_requested_at = SystemTime::UNIX_EPOCH;
    let closing = PaneLifecycle::Spawning
        .transition(
            PaneLifecycleEvent::CloseRequested { close_requested_at },
            PaneKind::Terminal,
        )
        .unwrap();
    assert_eq!(closing, PaneLifecycle::Closing { close_requested_at });

    // The late exit is rejected; the state stays `Closing`.
    let late_exit = PaneLifecycleEvent::ProcessExited {
        exit_code: Some(0),
        exited_at: close_requested_at,
    };
    assert_eq!(
        closing.transition(late_exit, PaneKind::Terminal),
        Err(InvalidTransitionError {
            previous_lifecycle: closing,
            lifecycle_event: late_exit,
            pane_kind: PaneKind::Terminal,
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
        close_requested_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    };
    let lifecycle_event = PaneLifecycleEvent::CloseRequested {
        close_requested_at: SystemTime::UNIX_EPOCH + Duration::from_secs(2),
    };

    assert_eq!(
        closing.transition(lifecycle_event, PaneKind::Terminal),
        Err(InvalidTransitionError {
            previous_lifecycle: closing,
            lifecycle_event,
            pane_kind: PaneKind::Terminal,
        })
    );
}

#[test]
fn a_running_pane_rejects_a_second_process_start() {
    assert_eq!(
        PaneLifecycle::Running.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal),
        Err(InvalidTransitionError {
            previous_lifecycle: PaneLifecycle::Running,
            lifecycle_event: PaneLifecycleEvent::ProcessStarted,
            pane_kind: PaneKind::Terminal,
        })
    );
}

#[test]
fn a_removed_pane_rejects_every_event() {
    let previous_lifecycle = PaneLifecycle::Removed;

    for lifecycle_event in list_lifecycle_events() {
        assert_eq!(
            previous_lifecycle.transition(lifecycle_event, PaneKind::Terminal),
            Err(InvalidTransitionError {
                previous_lifecycle,
                lifecycle_event,
                pane_kind: PaneKind::Terminal
            }),
            "Removed must stay terminal under {lifecycle_event:?}"
        );
    }
}

#[test]
fn a_spawning_pane_cannot_exit_before_it_runs() {
    let previous_lifecycle = PaneLifecycle::Spawning;
    let lifecycle_event = PaneLifecycleEvent::ProcessExited {
        exit_code: Some(1),
        exited_at: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        previous_lifecycle.transition(lifecycle_event, PaneKind::Terminal),
        Err(InvalidTransitionError {
            previous_lifecycle,
            lifecycle_event,
            pane_kind: PaneKind::Terminal
        })
    );
}

#[test]
fn an_exited_pane_cannot_skip_the_close_transaction() {
    let previous_lifecycle = PaneLifecycle::Exited {
        exit_code: Some(0),
        exited_at: SystemTime::UNIX_EPOCH,
    };
    // `Cleaned` is legal only from `Closing`.
    let lifecycle_event = PaneLifecycleEvent::Cleaned;

    assert_eq!(
        previous_lifecycle.transition(lifecycle_event, PaneKind::Terminal),
        Err(InvalidTransitionError {
            previous_lifecycle,
            lifecycle_event,
            pane_kind: PaneKind::Terminal
        })
    );
}

#[test]
fn an_exited_pane_is_never_silently_removed() {
    let previous_lifecycle = PaneLifecycle::Exited {
        exit_code: Some(0),
        exited_at: SystemTime::UNIX_EPOCH,
    };

    // No single event moves `Exited` to `Removed`. The path is
    // `Exited` -> `CloseRequested` -> `Closing` -> `Cleaned` -> `Removed`.
    for lifecycle_event in list_lifecycle_events() {
        assert_ne!(
            previous_lifecycle.transition(lifecycle_event, PaneKind::Terminal),
            Ok(PaneLifecycle::Removed)
        );
    }
}

#[test]
fn only_the_specified_transitions_are_accepted() {
    for previous_lifecycle in list_lifecycle_states() {
        for lifecycle_event in list_lifecycle_events() {
            let expected_lifecycle =
                match compute_expected_lifecycle(previous_lifecycle, lifecycle_event) {
                    Some(next_lifecycle) => Ok(next_lifecycle),
                    None => Err(InvalidTransitionError {
                        previous_lifecycle,
                        lifecycle_event,
                        pane_kind: PaneKind::Terminal,
                    }),
                };

            assert_eq!(
                previous_lifecycle.transition(lifecycle_event, PaneKind::Terminal),
                expected_lifecycle,
                "{previous_lifecycle:?} on {lifecycle_event:?}"
            );
        }
    }
}

#[test]
fn exactly_six_transitions_are_legal() {
    let accepted_transition_count = list_lifecycle_states()
        .into_iter()
        .flat_map(|previous_lifecycle| {
            list_lifecycle_events()
                .into_iter()
                .map(move |lifecycle_event| (previous_lifecycle, lifecycle_event))
        })
        .filter(|&(previous_lifecycle, lifecycle_event)| {
            previous_lifecycle
                .transition(lifecycle_event, PaneKind::Terminal)
                .is_ok()
        })
        .count();

    assert_eq!(accepted_transition_count, 6);
}

#[test]
fn an_exit_code_passes_through_unchanged_at_the_i32_bounds() {
    let exited_at = SystemTime::UNIX_EPOCH;

    for exit_code in [i32::MIN, -1, 0, 1, i32::MAX] {
        assert_eq!(
            PaneLifecycle::Running.transition(
                PaneLifecycleEvent::ProcessExited {
                    exit_code: Some(exit_code),
                    exited_at
                },
                PaneKind::Terminal,
            ),
            Ok(PaneLifecycle::Exited {
                exit_code: Some(exit_code),
                exited_at
            }),
            "exit code {exit_code}"
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
    assert_eq!(terminal.get_severity(), Severity::Recoverable);

    let plugin = PaneLifecycle::Removed
        .transition(
            PaneLifecycleEvent::ProcessStarted,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
        )
        .unwrap_err();
    assert_eq!(plugin.category(), DomainCategory::Plugin);
    assert_eq!(plugin.get_severity(), Severity::Recoverable);
}

#[test]
fn a_signal_killed_pane_exits_with_no_code() {
    let exited_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);

    let transition_result = PaneLifecycle::Running.transition(
        PaneLifecycleEvent::ProcessExited {
            exit_code: None,
            exited_at,
        },
        PaneKind::Terminal,
    );

    // `exit_code` stays `None`; the state does not stand in a `0`.
    assert_eq!(
        transition_result,
        Ok(PaneLifecycle::Exited {
            exit_code: None,
            exited_at
        })
    );
}

#[test]
fn lifecycle_events_survive_a_serde_round_trip() {
    for lifecycle_event in list_lifecycle_events() {
        let lifecycle_json = serde_json::to_string(&lifecycle_event).expect("serialize");
        let restored_lifecycle_event: PaneLifecycleEvent =
            serde_json::from_str(&lifecycle_json).expect("deserialize");

        assert_eq!(lifecycle_event, restored_lifecycle_event);
    }
}

#[test]
fn lifecycle_states_survive_a_serde_round_trip() {
    for lifecycle_state in list_lifecycle_states() {
        let lifecycle_json = serde_json::to_string(&lifecycle_state).expect("serialize");
        let restored_lifecycle_state: PaneLifecycle =
            serde_json::from_str(&lifecycle_json).expect("deserialize");

        assert_eq!(lifecycle_state, restored_lifecycle_state);
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
        exit_code: None,
        exited_at: SystemTime::UNIX_EPOCH + Duration::new(5, 400),
    };
    let closing = PaneLifecycle::Closing {
        close_requested_at: SystemTime::UNIX_EPOCH,
    };

    assert_eq!(
        serde_json::to_string(&exited).expect("serialize"),
        r#"{"Exited":{"exit_code":null,"exited_at":{"secs_since_epoch":5,"nanos_since_epoch":400}}}"#
    );
    assert_eq!(
        serde_json::to_string(&closing).expect("serialize"),
        r#"{"Closing":{"close_requested_at":{"secs_since_epoch":0,"nanos_since_epoch":0}}}"#
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
}

#[test]
fn an_unknown_lifecycle_state_fails_to_deserialize() {
    let deserialization_error =
        serde_json::from_str::<PaneLifecycle>(r#""Zombie""#).expect_err("unknown variant");

    assert_eq!(
        deserialization_error.to_string(),
        "unknown variant `Zombie`, expected one of `Spawning`, `Running`, `Exited`, `Closing`, `Removed` at line 1 column 8"
    );
}

#[test]
fn an_exited_state_without_a_time_fails_to_deserialize() {
    let deserialization_error =
        serde_json::from_str::<PaneLifecycle>(r#"{"Exited":{"exit_code":0}}"#)
            .expect_err("missing field");

    assert_eq!(
        deserialization_error.to_string(),
        "missing field `exited_at` at line 1 column 25"
    );
}
