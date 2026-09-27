//! Tests for the event vocabulary.
//!
//! Every [`Event`] variant survives a JSON round trip unchanged, in the
//! externally tagged shape, and reports its canonical variant name from
//! `Debug` and from [`Event::get_event_name`].

use super::*;
use crate::command::{GridPosition, PanePlacementAnchor, PanePlacementTarget, SelectionKind};
use crate::geometry::{Direction, PaneArea, Size};
use crate::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use crate::process::PtySize;

/// Roundtrip a value through JSON and assert it survives unchanged.
fn assert_json_roundtrip<Roundtrippable>(roundtrippable_subject: &Roundtrippable)
where
    Roundtrippable: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let serialized_json = serde_json::to_string(roundtrippable_subject).expect("serialize");
    let decoded_roundtrippable_subject: Roundtrippable =
        serde_json::from_str(&serialized_json).expect("deserialize");
    assert_eq!(*roundtrippable_subject, decoded_roundtrippable_subject);
}

#[test]
fn event_lifecycle_variants_round_trip_through_json() {
    assert_json_roundtrip(&Event::PaneCreated(PaneCreated {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::PaneProcessExited(PaneProcessExited {
        pane_id: PaneId::new(),
        exit_code: Some(0),
        signal: None,
    }));
    assert_json_roundtrip(&Event::PaneRemoved(PaneRemoved {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::PtyResized(PtyResized {
        pane_id: PaneId::new(),
        pty_size: PtySize {
            column_count: 80,
            row_count: 24,
        },
    }));
    assert_json_roundtrip(&Event::PaneCommandStarted(PaneCommandStarted {
        pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Event::PaneCommandFinished(PaneCommandFinished {
        pane_id: PaneId::new(),
        exit_code: Some(1),
    }));
    assert_json_roundtrip(&Event::InputModeChanged(InputModeChanged {
        client_id: ClientId::new(),
        lock_mode: LockMode::Locked,
    }));
    assert_json_roundtrip(&Event::MouseSelectChanged(MouseSelectChanged {
        client_id: ClientId::new(),
        is_enabled: true,
    }));
}

#[test]
fn tab_move_small_viewport_and_reload_events_round_trip_through_json() {
    assert_json_roundtrip(&Event::TabMoved(TabMoved {
        tab_id: TabId::new(),
        previous_tab_index: 0,
        new_tab_index: 2,
    }));
    assert_json_roundtrip(&Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
        client_id: ClientId::new(),
        viewport_size: Size {
            column_count: 1,
            row_count: 1,
        },
        pane_area: Some(PaneArea::Reported(Size {
            column_count: 1,
            row_count: 0,
        })),
        cause: TerminalTooSmallCause::Terminal,
    }));
    assert_json_roundtrip(&Event::ConfigReloaded(ConfigReloaded {
        session_id: SessionId::new(),
    }));
}

#[test]
fn terminal_too_small_causes_round_trip_through_json() {
    let other_client_id = ClientId::new();
    for cause in [
        TerminalTooSmallCause::Terminal,
        TerminalTooSmallCause::Regions,
        TerminalTooSmallCause::OtherClient(other_client_id),
    ] {
        assert_json_roundtrip(&cause);
    }
}

#[test]
fn terminal_too_small_event_uses_defaults_for_missing_fields() {
    let client_id = ClientId::new();
    let partial_terminal_too_small_event_json = serde_json::json!({
        "client_id": client_id,
        "viewport_size": { "column_count": 80, "row_count": 24 }
    });

    let terminal_too_small_event: TerminalTooSmallEntered =
        serde_json::from_value(partial_terminal_too_small_event_json)
            .expect("missing optional fields use defaults");
    assert_eq!(terminal_too_small_event.client_id, client_id);
    assert_eq!(
        terminal_too_small_event.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(terminal_too_small_event.pane_area, None);
    assert_eq!(
        terminal_too_small_event.cause,
        TerminalTooSmallCause::Terminal
    );
}

#[test]
fn selection_events_with_and_without_a_selection_round_trip_through_json() {
    assert_json_roundtrip(&Event::SelectionChanged(SelectionChanged {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        selection: Some(Selection {
            selection_kind: SelectionKind::Block,
            anchor: GridPosition {
                row_index: 1,
                column_index: 0,
            },
            cursor: GridPosition {
                row_index: 3,
                column_index: 20,
            },
        }),
    }));
    assert_json_roundtrip(&Event::SelectionChanged(SelectionChanged {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        selection: None,
    }));
}

/// Round-trips the variants the named round-trip tests above leave out, with
/// both `Some` and `None` for `PaneFocused::previous_pane_id`.
#[test]
fn remaining_event_variants_survive_a_json_round_trip() {
    let destination_tab_id = TabId::new();
    assert_json_roundtrip(&Event::PaneClosing(PaneClosing {
        pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Event::PaneFocused(PaneFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        previous_pane_id: Some(PaneId::new()),
    }));
    assert_json_roundtrip(&Event::PaneFocused(PaneFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        previous_pane_id: None,
    }));
    assert_json_roundtrip(&Event::LayoutChanged(LayoutChanged {
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::PanePlacementCommitted(PanePlacementCommitted {
        command_id: CommandId::new(),
        source_pane_id: PaneId::new(),
        source_tab_id: TabId::new(),
        destination_tab_id,
        placement_target: PanePlacementTarget::Split {
            destination_tab_id,
            anchor: PanePlacementAnchor::Pane(PaneId::new()),
            direction: Direction::Down,
        },
    }));
    assert_json_roundtrip(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::TabClosed(TabClosed {
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::TabFocused(TabFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        previous_tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::Quit(QuitCause::Requested));
    assert_json_roundtrip(&Event::Quit(QuitCause::LastTabClosed {
        tab_id: TabId::new(),
        pane_exit: None,
    }));
    assert_json_roundtrip(&Event::Quit(QuitCause::LastTabClosed {
        tab_id: TabId::new(),
        pane_exit: Some(PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: None,
            signal: Some(9),
        }),
    }));
    assert_json_roundtrip(&Event::Restarting);
}

// A serialized exit from before the field existed carries no `signal`.
#[test]
fn a_pane_exit_without_a_signal_field_decodes_with_no_signal() {
    let pane_id = PaneId::new();
    let exit_event_json = format!(
        r#"{{"PaneProcessExited":{{"pane_id":"{}","exit_code":127}}}}"#,
        pane_id.get_uuid()
    );

    let decoded_event: Event =
        serde_json::from_str(&exit_event_json).expect("decodes without signal");

    assert_eq!(
        decoded_event,
        Event::PaneProcessExited(PaneProcessExited {
            pane_id,
            exit_code: Some(127),
            signal: None,
        })
    );
}

// `is_failure` is `false` for exit code `0` with no signal, and `true` for
// every other pair, including the contradictory `Some(0)` beside a signal.
#[test]
fn a_pane_exit_is_a_failure_unless_its_code_is_zero_and_no_signal_is_present() {
    let pane_id = PaneId::new();
    let pane_exit_failure_cases = [
        (Some(0), None, false),
        (Some(1), None, true),
        (Some(-1), None, true),
        (None, Some(9), true),
        (None, Some(0), true),
        (Some(0), Some(9), true),
        (None, None, true),
    ];

    for (exit_code, exit_signal_number, expected_is_failure) in pane_exit_failure_cases {
        let pane_process_exit = PaneProcessExited {
            pane_id,
            exit_code,
            signal: exit_signal_number,
        };
        assert_eq!(
            pane_process_exit.is_failure(),
            expected_is_failure,
            "{pane_process_exit:?}"
        );
    }
}

/// The variant name in a value's `Debug` output: the text before the first
/// `(`, or the whole string for a unit variant.
/// `PaneCreated(PaneCreated { .. })` → `"PaneCreated"`; `Quit(_)` → `"Quit"`.
fn format_debug_variant_name<DebugSubject: std::fmt::Debug>(
    debug_subject: &DebugSubject,
) -> String {
    let debug_text = format!("{debug_subject:?}");
    debug_text
        .split('(')
        .next()
        .unwrap_or(&debug_text)
        .to_string()
}

/// One instance per top-level `Event` variant with its canonical name. The
/// array length is the variant count.
pub(crate) fn list_event_cases() -> [(Event, &'static str); 21] {
    [
        (
            Event::PaneCreated(PaneCreated {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneCreated",
        ),
        (
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: PaneId::new(),
                exit_code: None,
                signal: None,
            }),
            "PaneProcessExited",
        ),
        (
            Event::PaneClosing(PaneClosing {
                pane_id: PaneId::new(),
            }),
            "PaneClosing",
        ),
        (
            Event::PaneRemoved(PaneRemoved {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneRemoved",
        ),
        (
            Event::PaneFocused(PaneFocused {
                client_id: ClientId::new(),
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
                previous_pane_id: None,
            }),
            "PaneFocused",
        ),
        (
            Event::PtyResized(PtyResized {
                pane_id: PaneId::new(),
                pty_size: PtySize {
                    column_count: 80,
                    row_count: 24,
                },
            }),
            "PtyResized",
        ),
        (
            Event::PaneCommandStarted(PaneCommandStarted {
                pane_id: PaneId::new(),
            }),
            "PaneCommandStarted",
        ),
        (
            Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: PaneId::new(),
                exit_code: None,
            }),
            "PaneCommandFinished",
        ),
        (
            Event::LayoutChanged(LayoutChanged {
                tab_id: TabId::new(),
            }),
            "LayoutChanged",
        ),
        (
            Event::PanePlacementCommitted(PanePlacementCommitted {
                command_id: CommandId::new(),
                source_pane_id: PaneId::new(),
                source_tab_id: TabId::new(),
                destination_tab_id: TabId::new(),
                placement_target: PanePlacementTarget::Swap {
                    target_pane_id: PaneId::new(),
                },
            }),
            "PanePlacementCommitted",
        ),
        (
            Event::TabCreated(TabCreated {
                tab_id: TabId::new(),
            }),
            "TabCreated",
        ),
        (
            Event::TabClosed(TabClosed {
                tab_id: TabId::new(),
            }),
            "TabClosed",
        ),
        (
            Event::TabFocused(TabFocused {
                client_id: ClientId::new(),
                tab_id: TabId::new(),
                previous_tab_id: TabId::new(),
            }),
            "TabFocused",
        ),
        (
            Event::TabMoved(TabMoved {
                tab_id: TabId::new(),
                previous_tab_index: 0,
                new_tab_index: 1,
            }),
            "TabMoved",
        ),
        (
            Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                client_id: ClientId::new(),
                viewport_size: Size {
                    column_count: 1,
                    row_count: 1,
                },
                pane_area: Some(PaneArea::Starving),
                cause: TerminalTooSmallCause::Regions,
            }),
            "TerminalTooSmallEntered",
        ),
        (
            Event::ConfigReloaded(ConfigReloaded {
                session_id: SessionId::new(),
            }),
            "ConfigReloaded",
        ),
        (
            Event::InputModeChanged(InputModeChanged {
                client_id: ClientId::new(),
                lock_mode: LockMode::Normal,
            }),
            "InputModeChanged",
        ),
        (
            Event::MouseSelectChanged(MouseSelectChanged {
                client_id: ClientId::new(),
                is_enabled: true,
            }),
            "MouseSelectChanged",
        ),
        (
            Event::SelectionChanged(SelectionChanged {
                client_id: ClientId::new(),
                pane_id: PaneId::new(),
                selection: None,
            }),
            "SelectionChanged",
        ),
        (Event::Quit(QuitCause::Requested), "Quit"),
        (Event::Restarting, "Restarting"),
    ]
}

/// Checks 21 distinct top-level event names against `Debug` and
/// [`Event::get_event_name`].
#[test]
fn event_variants_report_their_canonical_names() {
    let event_cases = list_event_cases();
    let mut event_names = std::collections::BTreeSet::new();
    assert_eq!(event_cases.len(), 21);
    for (event, event_name) in event_cases {
        assert_eq!(format_debug_variant_name(&event), event_name);
        assert_eq!(event.get_event_name(), event_name);
        assert!(
            event_names.insert(event_name),
            "duplicate event name: {event_name}"
        );
    }
    assert_eq!(event_names.len(), 21);
}

#[test]
fn every_event_case_survives_a_json_round_trip() {
    for (event, event_name) in list_event_cases() {
        let event_json = serde_json::to_string(&event).expect("serialize");
        let decoded_event: Event = serde_json::from_str(&event_json).expect("deserialize");
        assert_eq!(decoded_event, event, "{event_name}");
    }
}

#[test]
fn events_encode_externally_tagged() {
    assert_eq!(
        serde_json::to_string(&Event::Quit(QuitCause::Requested)).expect("serialize"),
        r#"{"Quit":"Requested"}"#
    );
    assert_eq!(
        serde_json::to_string(&Event::Restarting).expect("serialize"),
        r#""Restarting""#
    );

    let pane_id = PaneId::new();
    let event_json =
        serde_json::to_string(&Event::PaneClosing(PaneClosing { pane_id })).expect("serialize");
    assert_eq!(
        event_json,
        format!(
            r#"{{"PaneClosing":{{"pane_id":"{}"}}}}"#,
            pane_id.get_uuid()
        )
    );

    let other_client_id = ClientId::new();
    assert_eq!(
        serde_json::to_string(&TerminalTooSmallCause::OtherClient(other_client_id))
            .expect("serialize"),
        format!(r#"{{"OtherClient":"{}"}}"#, other_client_id.get_uuid())
    );
}

#[test]
fn too_small_cause_defaults_to_terminal() {
    assert_eq!(
        TerminalTooSmallCause::default(),
        TerminalTooSmallCause::Terminal
    );
}

#[test]
fn terminal_too_small_event_with_null_pane_area_decodes_as_none() {
    let client_id = ClientId::new();
    let too_small_event_json = serde_json::json!({
        "client_id": client_id,
        "viewport_size": { "column_count": 80, "row_count": 24 },
        "pane_area": null,
        "cause": "Regions"
    });

    let terminal_too_small_event: TerminalTooSmallEntered =
        serde_json::from_value(too_small_event_json).expect("deserialize");
    assert_eq!(terminal_too_small_event.client_id, client_id);
    assert_eq!(
        terminal_too_small_event.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(terminal_too_small_event.pane_area, None);
    assert_eq!(
        terminal_too_small_event.cause,
        TerminalTooSmallCause::Regions
    );
}
