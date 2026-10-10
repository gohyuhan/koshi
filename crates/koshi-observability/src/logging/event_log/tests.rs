//! Tests for turning a committed runtime event into a log line.
//!
//! Coverage: the level each outcome gets, the ids and values a line carries,
//! the order lines come out in, the promise that no event is ever an error,
//! and one case per reason an event is left out of the file.

use super::*;

use koshi_core::command::PanePlacementTarget;
use koshi_core::event::{
    ConfigReloaded, FloatingPaneMoved, InputModeChanged, LayoutChanged, MouseSelectChanged,
    PaneClosing, PaneCommandFinished, PaneCommandStarted, PaneCreated, PaneFocused,
    PaneMinimizedChanged, PanePinChanged, PanePlacementCommitted, PaneProcessExited, PaneRemoved,
    PtyResized, SelectionChanged, TabClosed, TabCreated, TabFocused, TabMoved,
    TerminalTooSmallCause, TerminalTooSmallEntered,
};
use koshi_core::geometry::{PaneArea, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;

use koshi_core::log::LogLevel;

use crate::logging::{capture_logs_at_level, resolve_maximum_tracing_level, with_test_writer};

/// Log `runtime_events` through a thread-local JSON subscriber and return everything
/// written. Empty output means every event was left out of the file.
fn capture_event_logs(runtime_events: &[Event]) -> String {
    let (_subscriber_guard, captured_logs) = with_test_writer();
    for runtime_event in runtime_events {
        log_event(runtime_event);
    }
    captured_logs.contents()
}

#[test]
fn pane_created_is_one_info_line_carrying_its_pane_and_tab_ids() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let log_output = capture_event_logs(&[Event::PaneCreated(PaneCreated {
        pane_id,
        tab_id: Some(tab_id),
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(
        log_output.contains(r#""level":"INFO""#),
        "wrong level: {log_output}"
    );
    assert!(
        log_output.contains(r#""message":"pane created""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{log_output}"
    );
}

// Two events committed together write two lines, in the order they were
// committed.
#[test]
fn a_new_pane_writes_its_created_line_before_its_focused_line() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let log_output = capture_event_logs(&[
        Event::PaneCreated(PaneCreated {
            pane_id,
            tab_id: Some(tab_id),
        }),
        Event::PaneFocused(PaneFocused {
            client_id: ClientId::new(),
            tab_id: Some(tab_id),
            pane_id,
            previous_pane_id: None,
        }),
    ]);

    let log_lines: Vec<&str> = log_output.lines().collect();
    assert_eq!(
        log_lines.len(),
        2,
        "expected exactly two lines: {log_output}"
    );
    assert!(
        log_lines[0].contains(r#""message":"pane created""#),
        "{log_output}"
    );
    assert!(
        log_lines[1].contains(r#""message":"pane focused""#),
        "{log_output}"
    );
}

#[test]
fn config_reload_is_logged_at_info_naming_its_session() {
    let session_id = SessionId::new();

    let config_log_output =
        capture_event_logs(&[Event::ConfigReloaded(ConfigReloaded { session_id })]);

    assert_eq!(
        config_log_output.lines().count(),
        1,
        "expected exactly one line: {config_log_output}"
    );
    assert!(
        config_log_output.contains(r#""level":"INFO""#),
        "{config_log_output}"
    );
    assert!(
        config_log_output.contains(r#""message":"config reloaded""#),
        "{config_log_output}"
    );
    assert!(
        config_log_output.contains(&format!(r#""session_id":"{session_id}""#)),
        "{config_log_output}"
    );
}

// The rejection line is written where the rejection is built, in
// `koshi-runtime`; the event writes nothing.
#[test]
fn input_mode_change_is_info_naming_the_mode_now_in_effect() {
    let client_id = ClientId::new();

    let log_output = capture_event_logs(&[Event::InputModeChanged(InputModeChanged {
        client_id,
        lock_mode: LockMode::Locked,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"input mode changed""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""client_id":"{client_id}""#)),
        "{log_output}"
    );
    assert!(log_output.contains(r#""mode":"Locked""#), "{log_output}");
}

#[test]
fn mouse_select_change_is_info_naming_the_state_now_in_effect() {
    let client_id = ClientId::new();

    let log_output = capture_event_logs(&[Event::MouseSelectChanged(MouseSelectChanged {
        client_id,
        is_enabled: true,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"mouse select changed""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""client_id":"{client_id}""#)),
        "{log_output}"
    );
    assert!(log_output.contains(r#""is_enabled":true"#), "{log_output}");
}

#[test]
fn a_floating_pane_move_writes_one_info_line_with_the_client_pane_and_cell() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let log_output = capture_event_logs(&[Event::FloatingPaneMoved(FloatingPaneMoved {
        client_id,
        pane_id,
        top_left_cell: Point { column: 70, row: 2 },
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"floating pane moved""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""client_id":"{client_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{log_output}"
    );
    assert!(log_output.contains(r#""column":70,"#), "{log_output}");
    assert!(log_output.contains(r#""row":2}"#), "{log_output}");
}

#[test]
fn a_floating_pane_pin_change_writes_one_info_line_naming_the_state_now_in_effect() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let log_output = capture_event_logs(&[Event::PanePinChanged(PanePinChanged {
        client_id,
        pane_id,
        is_pinned: false,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"floating pane pin changed""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""client_id":"{client_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{log_output}"
    );
    assert!(log_output.contains(r#""is_pinned":false"#), "{log_output}");
}

#[test]
fn a_floating_pane_minimize_change_writes_one_info_line_naming_the_state_now_in_effect() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let log_output = capture_event_logs(&[Event::PaneMinimizedChanged(PaneMinimizedChanged {
        client_id,
        pane_id,
        is_minimized: true,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"floating pane minimize changed""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""client_id":"{client_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(r#""is_minimized":true"#),
        "{log_output}"
    );
}

// Every written event is `info` or `warn`.
#[test]
fn no_event_is_ever_logged_as_an_error() {
    let log_output = capture_event_logs(&[
        Event::PaneCreated(PaneCreated {
            pane_id: PaneId::new(),
            tab_id: Some(TabId::new()),
        }),
        Event::ConfigReloaded(ConfigReloaded {
            session_id: SessionId::new(),
        }),
        Event::Quit(QuitCause::Requested),
        Event::Restarting,
    ]);

    assert_eq!(
        log_output.lines().count(),
        4,
        "expected four lines: {log_output}"
    );
    assert!(
        !log_output.contains(r#""level":"ERROR""#),
        "an event was logged as an error: {log_output}"
    );
}

#[test]
fn restarting_is_info_saying_the_session_swaps_its_image() {
    let log_output = capture_event_logs(&[Event::Restarting]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"session restarting into the binary on disk""#),
        "{log_output}"
    );
}

// One event per reason the file leaves it out; together they write nothing.
#[test]
fn events_that_fire_faster_than_a_person_acts_write_nothing() {
    let log_output = capture_event_logs(&[
        // One per pane per frame while a window edge is dragged.
        Event::PtyResized(PtyResized {
            pane_id: PaneId::new(),
            pty_size: PtySize {
                column_count: 80,
                row_count: 24,
            },
        }),
        // Announces the close that `PaneRemoved` completes.
        Event::PaneClosing(PaneClosing {
            pane_id: PaneId::new(),
        }),
    ]);

    assert_eq!(
        log_output, "",
        "a high-frequency event reached the log file: {log_output}"
    );
}

#[test]
fn a_closed_pane_is_recorded_once_by_the_removal_not_the_announcement() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let log_output = capture_event_logs(&[
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved {
            pane_id,
            tab_id: Some(tab_id),
        }),
    ]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"pane removed""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{log_output}"
    );
}

// A `None` leaves its field off the line; it is not written as `null`. A
// signaled exit carries `signal` and no `exit_code`; a coded exit carries
// `exit_code` and no `signal`.
#[test]
fn a_pane_exit_writes_its_code_as_a_number_and_omits_an_absent_one() {
    let pane_id = PaneId::new();

    let successful_exit_log = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: Some(0),
        signal: None,
    })]);
    assert_eq!(
        successful_exit_log.lines().count(),
        1,
        "expected exactly one line: {successful_exit_log}"
    );
    assert!(
        successful_exit_log.contains(r#""level":"INFO""#),
        "{successful_exit_log}"
    );
    assert!(
        successful_exit_log.contains(r#""message":"pane process exited""#),
        "{successful_exit_log}"
    );
    assert!(
        successful_exit_log.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{successful_exit_log}"
    );
    assert!(
        successful_exit_log.contains(r#""exit_code":0"#),
        "{successful_exit_log}"
    );
    assert!(
        !successful_exit_log.contains("signal"),
        "{successful_exit_log}"
    );

    let negative_exit_log = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: Some(-1),
        signal: None,
    })]);
    assert!(
        negative_exit_log.contains(r#""exit_code":-1}"#),
        "{negative_exit_log}"
    );

    let signaled_exit_log = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: None,
        signal: Some(9),
    })]);
    assert_eq!(
        signaled_exit_log.lines().count(),
        1,
        "expected exactly one line: {signaled_exit_log}"
    );
    assert!(
        signaled_exit_log.contains(r#""message":"pane process exited""#),
        "{signaled_exit_log}"
    );
    assert!(
        signaled_exit_log.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{signaled_exit_log}"
    );
    assert!(
        !signaled_exit_log.contains("exit_code"),
        "{signaled_exit_log}"
    );
    assert!(
        signaled_exit_log.contains(r#""signal":9}"#),
        "{signaled_exit_log}"
    );
}

// `Some(0)` is the only code that logs at info. Every other code, and
// `None` for a signal-terminated child, logs at warn.
#[test]
fn a_pane_exit_is_info_only_when_the_program_exited_zero() {
    let pane_id = PaneId::new();
    let exit_level_cases = [
        (Some(0), None, "INFO"),
        (Some(127), None, "WARN"),
        (Some(-1), None, "WARN"),
        (None, Some(9), "WARN"),
        (None, Some(0), "WARN"),
        (Some(0), Some(9), "WARN"),
    ];

    for (exit_code, signal, expected_log_level) in exit_level_cases {
        let log_output = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
            pane_id,
            exit_code,
            signal,
        })]);

        assert_eq!(log_output.lines().count(), 1, "{exit_code:?}: {log_output}");
        assert!(
            log_output.contains(&format!(r#""level":"{expected_log_level}""#)),
            "{exit_code:?} must log at {expected_log_level}: {log_output}"
        );
        assert!(
            log_output.contains(r#""message":"pane process exited""#),
            "{exit_code:?}: {log_output}"
        );
        assert!(
            log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
            "{exit_code:?}: {log_output}"
        );
    }
}

// `previous_pane_id` and `previous_tab_id` are on the events and are not written.
#[test]
fn each_focus_and_tab_lifecycle_fact_writes_its_own_message_and_ids() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let previous_pane_id = PaneId::new();
    let previous_tab_id = TabId::new();

    let focused_pane = capture_event_logs(&[Event::PaneFocused(PaneFocused {
        client_id,
        tab_id: Some(tab_id),
        pane_id,
        previous_pane_id: Some(previous_pane_id),
    })]);
    assert_eq!(
        focused_pane.lines().count(),
        1,
        "expected exactly one line: {focused_pane}"
    );
    assert!(focused_pane.contains(r#""level":"INFO""#), "{focused_pane}");
    assert!(
        focused_pane.contains(r#""message":"pane focused""#),
        "{focused_pane}"
    );
    assert!(
        focused_pane.contains(&format!(r#""client_id":"{client_id}""#)),
        "{focused_pane}"
    );
    assert!(
        focused_pane.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{focused_pane}"
    );
    assert!(
        focused_pane.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{focused_pane}"
    );
    assert!(
        !focused_pane.contains(&previous_pane_id.to_string()),
        "the pane focus left behind is not written: {focused_pane}"
    );

    let created_tab = capture_event_logs(&[Event::TabCreated(TabCreated { tab_id })]);
    assert_eq!(
        created_tab.lines().count(),
        1,
        "expected exactly one line: {created_tab}"
    );
    assert!(created_tab.contains(r#""level":"INFO""#), "{created_tab}");
    assert!(
        created_tab.contains(r#""message":"tab created""#),
        "{created_tab}"
    );
    assert!(
        created_tab.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{created_tab}"
    );

    let closed_tab = capture_event_logs(&[Event::TabClosed(TabClosed { tab_id })]);
    assert_eq!(
        closed_tab.lines().count(),
        1,
        "expected exactly one line: {closed_tab}"
    );
    assert!(closed_tab.contains(r#""level":"INFO""#), "{closed_tab}");
    assert!(
        closed_tab.contains(r#""message":"tab closed""#),
        "{closed_tab}"
    );
    assert!(
        closed_tab.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{closed_tab}"
    );

    let focused_tab = capture_event_logs(&[Event::TabFocused(TabFocused {
        client_id,
        tab_id,
        previous_tab_id,
    })]);
    assert_eq!(
        focused_tab.lines().count(),
        1,
        "expected exactly one line: {focused_tab}"
    );
    assert!(focused_tab.contains(r#""level":"INFO""#), "{focused_tab}");
    assert!(
        focused_tab.contains(r#""message":"tab focused""#),
        "{focused_tab}"
    );
    assert!(
        focused_tab.contains(&format!(r#""client_id":"{client_id}""#)),
        "{focused_tab}"
    );
    assert!(
        focused_tab.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{focused_tab}"
    );
    assert!(
        !focused_tab.contains(&previous_tab_id.to_string()),
        "the tab focus left behind is not written: {focused_tab}"
    );
}

// A tab dragged from slot 0 to slot 3 writes `previous_tab_index` 0 and `new_tab_index` 3.
#[test]
fn a_tab_move_records_the_slot_it_left_and_the_slot_it_landed_on() {
    let tab_id = TabId::new();

    let log_output = capture_event_logs(&[Event::TabMoved(TabMoved {
        tab_id,
        previous_tab_index: 0,
        new_tab_index: 3,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"tab moved""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(r#""previous_tab_index":0"#),
        "{log_output}"
    );
    assert!(log_output.contains(r#""new_tab_index":3}"#), "{log_output}");
    assert!(
        log_output.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{log_output}"
    );
}

// Entering writes the size, the pane area and the cause. An absent pane area
// is written as the string `None`.
#[test]
fn terminal_too_small_writes_the_size_the_pane_area_and_the_cause() {
    let client_id = ClientId::new();

    let entered = capture_event_logs(&[Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
        client_id,
        viewport_size: Size {
            column_count: 10,
            row_count: 3,
        },
        pane_area: Some(PaneArea::Starving),
        cause: TerminalTooSmallCause::Regions,
    })]);
    assert_eq!(
        entered.lines().count(),
        1,
        "expected exactly one line: {entered}"
    );
    assert!(entered.contains(r#""level":"INFO""#), "{entered}");
    assert!(
        entered.contains(r#""message":"terminal too small; panes hidden""#),
        "{entered}"
    );
    assert!(entered.contains(r#""column_count":10,"#), "{entered}");
    assert!(entered.contains(r#""row_count":3,"#), "{entered}");
    assert!(
        entered.contains(r#""pane_area":"Some(Starving)""#),
        "{entered}"
    );
    assert!(entered.contains(r#""cause":"Regions""#), "{entered}");
    assert!(
        entered.contains(&format!(r#""client_id":"{client_id}""#)),
        "{entered}"
    );

    let entered_without_area =
        capture_event_logs(&[Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
            client_id,
            viewport_size: Size {
                column_count: 10,
                row_count: 3,
            },
            pane_area: None,
            cause: TerminalTooSmallCause::Regions,
        })]);
    assert!(
        entered_without_area.contains(r#""pane_area":"None""#),
        "{entered_without_area}"
    );
}

// A quit writes one line whatever its cause. Only a last-tab close that a
// failed exit started logs at warn; a requested quit, a close a command made,
// and a close a clean exit made log at info.
#[test]
fn quitting_writes_one_line_at_warn_only_when_a_failed_exit_ended_the_session() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let quit_cases = [
        (QuitCause::Requested, "INFO", "requested"),
        (
            QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            },
            "INFO",
            "last-tab-closed",
        ),
        (
            QuitCause::LastTabClosed {
                tab_id,
                pane_exit: Some(PaneProcessExited {
                    pane_id,
                    exit_code: Some(0),
                    signal: None,
                }),
            },
            "INFO",
            "last-tab-closed",
        ),
        (
            QuitCause::LastTabClosed {
                tab_id,
                pane_exit: Some(PaneProcessExited {
                    pane_id,
                    exit_code: Some(127),
                    signal: None,
                }),
            },
            "WARN",
            "last-tab-closed",
        ),
        (
            QuitCause::LastTabClosed {
                tab_id,
                pane_exit: Some(PaneProcessExited {
                    pane_id,
                    exit_code: None,
                    signal: Some(9),
                }),
            },
            "WARN",
            "last-tab-closed",
        ),
    ];

    for (quit_cause, expected_log_level, expected_cause) in quit_cases {
        let log_output = capture_event_logs(&[Event::Quit(quit_cause)]);

        assert_eq!(
            log_output.lines().count(),
            1,
            "{quit_cause:?}: expected exactly one line: {log_output}"
        );
        assert!(
            log_output.contains(&format!(r#""level":"{expected_log_level}""#)),
            "{quit_cause:?} must log at {expected_log_level}: {log_output}"
        );
        assert!(
            log_output.contains(r#""message":"session quitting""#),
            "{quit_cause:?}: {log_output}"
        );
        assert!(
            log_output.contains(&format!(r#""cause":"{expected_cause}""#)),
            "{quit_cause:?}: {log_output}"
        );
    }
}

// A requested quit names no tab and no pane. A last-tab close names its tab,
// and the pane exit that emptied it when one did, with the exit's own fields.
#[test]
fn a_quit_line_names_the_tab_and_the_pane_exit_that_ended_the_session() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let requested_log = capture_event_logs(&[Event::Quit(QuitCause::Requested)]);
    assert!(!requested_log.contains("tab_id"), "{requested_log}");
    assert!(!requested_log.contains("pane_id"), "{requested_log}");

    let closed_by_command_log = capture_event_logs(&[Event::Quit(QuitCause::LastTabClosed {
        tab_id,
        pane_exit: None,
    })]);
    assert!(
        closed_by_command_log.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{closed_by_command_log}"
    );
    assert!(
        !closed_by_command_log.contains("pane_id"),
        "{closed_by_command_log}"
    );

    let closed_by_exit_log = capture_event_logs(&[Event::Quit(QuitCause::LastTabClosed {
        tab_id,
        pane_exit: Some(PaneProcessExited {
            pane_id,
            exit_code: Some(127),
            signal: None,
        }),
    })]);
    assert!(
        closed_by_exit_log.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{closed_by_exit_log}"
    );
    assert!(
        closed_by_exit_log.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{closed_by_exit_log}"
    );
    assert!(
        closed_by_exit_log.contains(r#""exit_code":127}"#),
        "{closed_by_exit_log}"
    );
    assert!(
        !closed_by_exit_log.contains("signal"),
        "{closed_by_exit_log}"
    );

    let closed_by_signal_log = capture_event_logs(&[Event::Quit(QuitCause::LastTabClosed {
        tab_id,
        pane_exit: Some(PaneProcessExited {
            pane_id,
            exit_code: None,
            signal: Some(9),
        }),
    })]);
    assert!(
        closed_by_signal_log.contains(r#""signal":9}"#),
        "{closed_by_signal_log}"
    );
    assert!(
        !closed_by_signal_log.contains("exit_code"),
        "{closed_by_signal_log}"
    );
}

// At the shipped default cutoff, `warning`, a failed exit and the quit it
// caused are written, and a clean exit and its quit are not.
#[test]
fn at_the_warning_cutoff_only_a_failed_exit_and_the_quit_it_caused_are_written() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let failed_exit = PaneProcessExited {
        pane_id,
        exit_code: Some(127),
        signal: None,
    };
    let clean_exit = PaneProcessExited {
        pane_id,
        exit_code: Some(0),
        signal: None,
    };

    let failing_exit_log = {
        let (_subscriber_guard, captured_logs) =
            capture_logs_at_level(resolve_maximum_tracing_level(LogLevel::Warning));
        log_event(&Event::PaneProcessExited(failed_exit));
        log_event(&Event::Quit(QuitCause::LastTabClosed {
            tab_id,
            pane_exit: Some(failed_exit),
        }));
        captured_logs.contents()
    };
    assert_eq!(failing_exit_log.lines().count(), 2, "{failing_exit_log}");
    assert!(
        failing_exit_log.contains(r#""message":"pane process exited""#),
        "{failing_exit_log}"
    );
    assert!(
        failing_exit_log.contains(r#""message":"session quitting""#),
        "{failing_exit_log}"
    );

    let clean_exit_log = {
        let (_subscriber_guard, captured_logs) =
            capture_logs_at_level(resolve_maximum_tracing_level(LogLevel::Warning));
        log_event(&Event::PaneProcessExited(clean_exit));
        log_event(&Event::Quit(QuitCause::LastTabClosed {
            tab_id,
            pane_exit: Some(clean_exit),
        }));
        log_event(&Event::Quit(QuitCause::Requested));
        captured_logs.contents()
    };
    assert_eq!(clean_exit_log, "", "{clean_exit_log}");
}

// The remaining silent variants. With
// `events_that_fire_faster_than_a_person_acts_write_nothing`, every silent arm
// of `log_event` is covered.
#[test]
fn the_remaining_silent_events_write_nothing() {
    let log_output = capture_event_logs(&[
        // One per pane per frame while a window edge is dragged; the splits and
        // closes behind the change already have their own lines.
        Event::LayoutChanged(LayoutChanged {
            tab_id: TabId::new(),
        }),
        // What the shell inside a pane is doing, not koshi's own state.
        Event::PaneCommandStarted(PaneCommandStarted {
            pane_id: PaneId::new(),
        }),
        Event::PaneCommandFinished(PaneCommandFinished {
            pane_id: PaneId::new(),
            exit_code: Some(1),
        }),
        // One per step of a mouse drag across the screen.
        Event::SelectionChanged(SelectionChanged {
            client_id: ClientId::new(),
            pane_id: PaneId::new(),
            selection: None,
        }),
    ]);

    assert_eq!(
        log_output, "",
        "an event kept out of the file reached it: {log_output}"
    );
}

#[test]
fn a_floating_pane_line_carries_no_tab_id_field() {
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let log_output = capture_event_logs(&[
        Event::PaneCreated(PaneCreated {
            pane_id,
            tab_id: None,
        }),
        Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: None,
            pane_id,
            previous_pane_id: None,
        }),
        Event::PaneRemoved(PaneRemoved {
            pane_id,
            tab_id: None,
        }),
    ]);

    assert_eq!(
        log_output.lines().count(),
        3,
        "expected exactly three lines: {log_output}"
    );
    for log_line in log_output.lines() {
        assert!(
            log_line.contains(&format!(r#""pane_id":"{pane_id}""#)),
            "{log_line}"
        );
        assert!(!log_line.contains("tab_id"), "{log_line}");
    }
}

#[test]
fn a_placement_from_a_floating_pane_logs_only_its_destination_tab() {
    let source_pane_id = PaneId::new();
    let destination_tab_id = TabId::new();

    let log_output = capture_event_logs(&[Event::PanePlacementCommitted(PanePlacementCommitted {
        command_id: CommandId::new(),
        source_pane_id,
        source_tab_id: None,
        destination_tab_id: Some(destination_tab_id),
        placement_target: PanePlacementTarget::Swap {
            target_pane_id: PaneId::new(),
        },
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""source_pane_id":"{source_pane_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""destination_tab_id":"{destination_tab_id}""#)),
        "{log_output}"
    );
    assert!(!log_output.contains("source_tab_id"), "{log_output}");
}
