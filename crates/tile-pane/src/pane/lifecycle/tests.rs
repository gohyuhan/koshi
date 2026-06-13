use std::time::SystemTime;

use tile_core::error::{DomainCategory, DomainError, Severity};
use tile_core::ids::PluginId;

use super::{PaneLifecycle, PaneLifecycleEvent};
use crate::error::InvalidTransition;
use crate::pane::state::PaneKind;

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

    // A close can arrive before the child reports started; honour it rather
    // than forcing the pane to run first.
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
    let since = SystemTime::UNIX_EPOCH;

    let next = exited.transition(
        PaneLifecycleEvent::CloseRequested { since },
        PaneKind::Terminal,
    );

    // The close discards the stale exit payload and adopts the request's time.
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

    // RespawnShell loops a dead pane back into Spawning to recreate its PTY and
    // child, dropping the prior exit payload.
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

    // Respawn reuses the ordinary Spawning -> Running edge; no shortcut to Running.
    let spawning = exited
        .transition(PaneLifecycleEvent::Respawn, PaneKind::Terminal)
        .unwrap();
    let running = spawning.transition(PaneLifecycleEvent::ProcessStarted, PaneKind::Terminal);

    assert_eq!(running, Ok(PaneLifecycle::Running));
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
    // `Cleaned` is what drives Closing -> Removed; from Exited it is illegal,
    // so a dead pane can never reach Removed without first Closing.
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

    // Acceptance signal: Exited is retained (HoldOnExit) — no event takes it
    // straight to Removed; only an explicit close, then cleanup, does.
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
            let result = from.transition(event, PaneKind::Terminal);

            if is_allowed(from, event) {
                assert!(result.is_ok(), "{from:?} on {event:?} should be allowed");
            } else {
                assert_eq!(
                    result,
                    Err(InvalidTransition {
                        from,
                        event,
                        kind: PaneKind::Terminal
                    }),
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
        .filter(|&(from, event)| from.transition(event, PaneKind::Terminal).is_ok())
        .count();

    assert_eq!(accepted, 7);
}

#[test]
fn an_invalid_transition_is_recoverable_and_classified_by_pane_kind() {
    // The error's domain follows the pane's kind, so a plugin pane's failure is
    // never mislabelled as a terminal-emulator failure.
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
fn lifecycle_events_survive_a_serde_round_trip() {
    for event in all_events() {
        let json = serde_json::to_string(&event).expect("serialize");
        let restored: PaneLifecycleEvent = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(event, restored);
    }
}
