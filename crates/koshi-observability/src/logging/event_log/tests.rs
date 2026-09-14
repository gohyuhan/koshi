//! Tests for turning a committed runtime event into a log line.
//!
//! Coverage: the level each outcome gets, the ids and values a line carries,
//! the order lines come out in, the promise that no event is ever an error,
//! and one case per reason an event is left out of the file.

use super::*;

use koshi_core::command::CopyTarget;
use koshi_core::event::{
    CommandRejected, ConfigReloaded, Copied, EventClass, InputModeChanged, KeybindingMatched,
    LayoutChanged, MouseDragged, MousePressed, MouseReleased, MouseScrolled, MouseSelectChanged,
    PaneClosing, PaneCommandFinished, PaneCommandStarted, PaneCreated, PaneEnterPressed,
    PaneFocused, PaneMouseForwarded, PaneOutputUpdated, PaneProcessExited, PaneRemoved,
    PaneResumed, PaneScrollbackTruncated, PaneSuppressed, PaneTyped, PluginBroken, PluginDisabled,
    PluginDoctorCompleted, PluginEnabled, PluginInstalled, PluginLoadFailed, PluginMouseInput,
    PluginReloaded, PluginUninstalled, PluginUnloaded, PluginUpdated, PtyResized, RejectReason,
    SelectionChanged, SubmittedLinePayload, SubscriberLagged, TabClosed, TabCreated, TabFocused,
    TabMoved, TerminalTooSmallCause, TerminalTooSmallEntered, TerminalTooSmallExited, TypedPayload,
};
use koshi_core::geometry::{PaneArea, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, ScrollDirection};
use koshi_core::process::PtySize;

use crate::logging::with_test_writer;

/// Log `runtime_events` through a thread-local JSON subscriber and return everything
/// written. Empty output means every event was left out of the file.
fn capture_event_logs(runtime_events: &[Event]) -> String {
    let (_subscriber_guard, captured_logs) = with_test_writer();
    for runtime_event in runtime_events {
        log_event(runtime_event);
    }
    captured_logs.contents()
}

/// A `PaneTyped` carrying the printable character `'x'`.
fn build_typed_character_event() -> Event {
    Event::PaneTyped(PaneTyped {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        typed_payload: TypedPayload::SafePublic('x'),
        accepted_at: std::time::SystemTime::UNIX_EPOCH,
    })
}

#[test]
fn pane_created_is_one_info_line_carrying_its_pane_and_tab_ids() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let log_output = capture_event_logs(&[Event::PaneCreated(PaneCreated { pane_id, tab_id })]);

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
        Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        Event::PaneFocused(PaneFocused {
            client_id: ClientId::new(),
            tab_id,
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
fn command_rejected_writes_nothing_because_the_rejection_itself_is_logged() {
    let log_output = capture_event_logs(&[Event::CommandRejected(CommandRejected {
        command_id: CommandId::new(),
        rejection_reason: RejectReason::MinSize,
    })]);

    assert_eq!(
        log_output, "",
        "the rejection would be logged twice: {log_output}"
    );
}

#[test]
fn subscriber_lag_is_a_warning_carrying_the_drop_count() {
    let subscriber_id = SubscriberId::new();

    let log_output = capture_event_logs(&[Event::SubscriberLagged(SubscriberLagged {
        subscriber_id,
        dropped_event_count: 12,
        event_class: EventClass::Lossy,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"WARN""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"subscriber queue overflowed; events dropped""#),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""subscriber_id":"{subscriber_id}""#)),
        "{log_output}"
    );
    assert!(log_output.contains(r#""dropped_count":12"#), "{log_output}");
    assert!(
        log_output.contains(r#""event_class":"Lossy""#),
        "{log_output}"
    );
}

#[test]
fn plugin_install_is_info_and_a_failed_load_is_a_warning() {
    let plugin_id = PluginId::new();

    let installed_plugin_log_output =
        capture_event_logs(&[Event::Plugin(PluginEvent::Installed(PluginInstalled {
            plugin_id,
        }))]);
    assert_eq!(
        installed_plugin_log_output.lines().count(),
        1,
        "expected exactly one line: {installed_plugin_log_output}"
    );
    assert!(
        installed_plugin_log_output.contains(r#""level":"INFO""#),
        "{installed_plugin_log_output}"
    );
    assert!(
        installed_plugin_log_output.contains(r#""message":"plugin installed""#),
        "{installed_plugin_log_output}"
    );
    assert!(
        installed_plugin_log_output.contains(&format!(r#""plugin_id":"{plugin_id}""#)),
        "{installed_plugin_log_output}"
    );

    let failed_plugin_log_output =
        capture_event_logs(&[Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
            plugin_id,
            failure_reason: "wasm module has no `koshi` export".to_string(),
        }))]);
    assert_eq!(
        failed_plugin_log_output.lines().count(),
        1,
        "expected exactly one line: {failed_plugin_log_output}"
    );
    assert!(
        failed_plugin_log_output.contains(r#""level":"WARN""#),
        "{failed_plugin_log_output}"
    );
    assert!(
        failed_plugin_log_output
            .contains(r#""message":"plugin failed to load; continuing without it""#),
        "{failed_plugin_log_output}"
    );
    assert!(
        failed_plugin_log_output.contains(&format!(r#""plugin_id":"{plugin_id}""#)),
        "{failed_plugin_log_output}"
    );
    assert!(
        failed_plugin_log_output
            .contains(r#""failure_reason":"wasm module has no `koshi` export""#),
        "{failed_plugin_log_output}"
    );
}

// The line carries the byte count and the target, never the copied text.
#[test]
fn copied_records_the_byte_count_and_target_only() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let log_output = capture_event_logs(&[Event::Copied(Copied {
        client_id,
        pane_id,
        clipboard_target: CopyTarget::Osc52,
        byte_count: 41,
    })]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(log_output.contains(r#""message":"copied""#), "{log_output}");
    assert!(
        log_output.contains(&format!(r#""client_id":"{client_id}""#)),
        "{log_output}"
    );
    assert!(
        log_output.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{log_output}"
    );
    assert!(log_output.contains(r#""byte_count":41"#), "{log_output}");
    assert!(
        log_output.contains(r#""clipboard_target":"Osc52""#),
        "{log_output}"
    );
}

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

// Every written event is `info` or `warn`. `CommandRejected` writes nothing.
#[test]
fn no_event_is_ever_logged_as_an_error() {
    let log_output = capture_event_logs(&[
        Event::PaneCreated(PaneCreated {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        }),
        Event::ConfigReloaded(ConfigReloaded {
            session_id: SessionId::new(),
        }),
        Event::CommandRejected(CommandRejected {
            command_id: CommandId::new(),
            rejection_reason: RejectReason::TargetGone,
        }),
        Event::SubscriberLagged(SubscriberLagged {
            subscriber_id: SubscriberId::new(),
            dropped_event_count: 1,
            event_class: EventClass::Critical,
        }),
        Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
            plugin_id: PluginId::new(),
            failure_reason: "unreadable".to_string(),
        })),
        Event::Quit,
        Event::Restarting,
    ]);

    assert_eq!(
        log_output.lines().count(),
        6,
        "expected six lines: {log_output}"
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
        // Terminal content ticking over as a pane prints.
        Event::PaneOutputUpdated(PaneOutputUpdated {
            pane_id: PaneId::new(),
        }),
        // One per pane per frame while a window edge is dragged.
        Event::PtyResized(PtyResized {
            pane_id: PaneId::new(),
            pty_size: PtySize {
                column_count: 80,
                row_count: 24,
            },
        }),
        // One per keystroke, and it carries the character.
        build_typed_character_event(),
        // One per keystroke that resolves to a command.
        Event::KeybindingMatched(KeybindingMatched {
            client_id: ClientId::new(),
            command_id: CommandId::new(),
        }),
        // One per wheel notch.
        Event::MouseScrolled(MouseScrolled {
            client_id: ClientId::new(),
            pane_id: Some(PaneId::new()),
            position: Point { column: 4, row: 9 },
            direction: ScrollDirection::Down,
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
        Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
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

// `exit_code: None` leaves the field off the line; it is not written as
// `null`.
#[test]
fn a_pane_exit_writes_its_code_as_a_number_and_omits_an_absent_one() {
    let pane_id = PaneId::new();

    let successful_exit_log = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: Some(0),
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

    let negative_exit_log = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: Some(-1),
    })]);
    assert!(
        negative_exit_log.contains(r#""exit_code":-1"#),
        "{negative_exit_log}"
    );

    let signaled_exit_log = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: None,
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
}

// `Some(0)` is the only code that logs at info. Every other code, and
// `None` for a signal-terminated child, logs at warn.
#[test]
fn a_pane_exit_is_info_only_when_the_program_exited_zero() {
    let pane_id = PaneId::new();
    let exit_level_cases = [(Some(0), "INFO"), (Some(127), "WARN"), (None, "WARN")];

    for (exit_code, expected_log_level) in exit_level_cases {
        let log_output = capture_event_logs(&[Event::PaneProcessExited(PaneProcessExited {
            pane_id,
            exit_code,
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

// `prior_pane` and `prior_tab` are on the events and are not written.
#[test]
fn each_focus_and_tab_lifecycle_fact_writes_its_own_message_and_ids() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let previous_pane_id = PaneId::new();
    let previous_tab_id = TabId::new();

    let focused_pane = capture_event_logs(&[Event::PaneFocused(PaneFocused {
        client_id,
        tab_id,
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
    assert!(log_output.contains(r#""new_tab_index":3"#), "{log_output}");
    assert!(
        log_output.contains(&format!(r#""tab_id":"{tab_id}""#)),
        "{log_output}"
    );
}

// Entering writes the size, the pane area and the cause; leaving writes the
// size. An absent pane area is written as the string `None`.
#[test]
fn the_too_small_pair_says_which_way_it_went_and_the_size_it_happened_at() {
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
    assert!(entered.contains(r#""column_count":10"#), "{entered}");
    assert!(entered.contains(r#""row_count":3"#), "{entered}");
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

    let exited = capture_event_logs(&[Event::TerminalTooSmallExited(TerminalTooSmallExited {
        client_id,
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
    })]);
    assert_eq!(
        exited.lines().count(),
        1,
        "expected exactly one line: {exited}"
    );
    assert!(exited.contains(r#""level":"INFO""#), "{exited}");
    assert!(
        exited.contains(r#""message":"terminal big enough again; panes shown""#),
        "{exited}"
    );
    assert!(exited.contains(r#""column_count":80"#), "{exited}");
    assert!(exited.contains(r#""row_count":24"#), "{exited}");
    assert!(
        exited.contains(&format!(r#""client_id":"{client_id}""#)),
        "{exited}"
    );
}

#[test]
fn quitting_writes_one_info_line_saying_the_session_is_ending() {
    let log_output = capture_event_logs(&[Event::Quit]);

    assert_eq!(
        log_output.lines().count(),
        1,
        "expected exactly one line: {log_output}"
    );
    assert!(log_output.contains(r#""level":"INFO""#), "{log_output}");
    assert!(
        log_output.contains(r#""message":"session quitting""#),
        "{log_output}"
    );
}

// Each plugin event writes its own message. `LoadFailed` and `Broken` are
// warnings; the rest are info.
#[test]
fn each_plugin_lifecycle_fact_writes_its_own_message_at_its_own_level() {
    let plugin_id = PluginId::new();
    let plugin_event_cases = [
        (
            Event::Plugin(PluginEvent::Uninstalled(PluginUninstalled { plugin_id })),
            "INFO",
            "plugin uninstalled",
        ),
        (
            Event::Plugin(PluginEvent::Enabled(PluginEnabled { plugin_id })),
            "INFO",
            "plugin enabled",
        ),
        (
            Event::Plugin(PluginEvent::Disabled(PluginDisabled { plugin_id })),
            "INFO",
            "plugin disabled",
        ),
        (
            Event::Plugin(PluginEvent::Updated(PluginUpdated { plugin_id })),
            "INFO",
            "plugin updated",
        ),
        (
            Event::Plugin(PluginEvent::Reloaded(PluginReloaded { plugin_id })),
            "INFO",
            "plugin reloaded",
        ),
        (
            Event::Plugin(PluginEvent::Unloaded(PluginUnloaded { plugin_id })),
            "INFO",
            "plugin unloaded",
        ),
        (
            Event::Plugin(PluginEvent::DoctorCompleted(PluginDoctorCompleted {
                plugin_id,
            })),
            "INFO",
            "plugin diagnostic completed",
        ),
        (
            Event::Plugin(PluginEvent::Broken(PluginBroken {
                plugin_id,
                failure_reason: "manifest names no entry point".to_string(),
            })),
            "WARN",
            "plugin marked broken and disabled",
        ),
    ];

    for (plugin_event, expected_log_level, expected_log_message) in plugin_event_cases {
        let log_output = capture_event_logs(std::slice::from_ref(&plugin_event));
        assert_eq!(
            log_output.lines().count(),
            1,
            "{expected_log_message}: {log_output}"
        );
        assert!(
            log_output.contains(&format!(r#""level":"{expected_log_level}""#)),
            "{expected_log_message}: {log_output}"
        );
        assert!(
            log_output.contains(&format!(r#""message":"{expected_log_message}""#)),
            "{expected_log_message}: {log_output}"
        );
        assert!(
            log_output.contains(&format!(r#""plugin_id":"{plugin_id}""#)),
            "{expected_log_message}: {log_output}"
        );
    }

    // `Broken` writes its `failure_reason` on the line.
    let broken_plugin_log =
        capture_event_logs(&[Event::Plugin(PluginEvent::Broken(PluginBroken {
            plugin_id,
            failure_reason: "manifest names no entry point".to_string(),
        }))]);
    assert!(
        broken_plugin_log.contains(r#""failure_reason":"manifest names no entry point""#),
        "{broken_plugin_log}"
    );
}

// The remaining silent variants. With
// `events_that_fire_faster_than_a_person_acts_write_nothing` and
// `command_rejected_writes_nothing_because_the_rejection_itself_is_logged`,
// every silent arm of `log_event` is covered.
#[test]
fn the_remaining_silent_events_write_nothing() {
    let log_output = capture_event_logs(&[
        // One per pane per frame while a window edge is dragged; the splits and
        // closes behind the change already have their own lines.
        Event::LayoutChanged(LayoutChanged {
            tab_id: TabId::new(),
        }),
        Event::PaneSuppressed(PaneSuppressed {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        }),
        Event::PaneResumed(PaneResumed {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        }),
        // Carries the command line the user typed.
        Event::PaneEnterPressed(PaneEnterPressed {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
            session_id: SessionId::new(),
            client_id: ClientId::new(),
            submitted_line: SubmittedLinePayload::SafePublic("ls -la".to_string()),
            accepted_at: std::time::SystemTime::UNIX_EPOCH,
        }),
        // One per click and per step of a drag.
        Event::MousePressed(MousePressed {
            client_id: ClientId::new(),
            pane_id: Some(PaneId::new()),
            position: Point { column: 1, row: 2 },
            button: MouseButton::Left,
        }),
        Event::MouseReleased(MouseReleased {
            client_id: ClientId::new(),
            pane_id: Some(PaneId::new()),
            position: Point { column: 1, row: 2 },
            button: MouseButton::Left,
        }),
        Event::MouseDragged(MouseDragged {
            client_id: ClientId::new(),
            pane_id: None,
            position: Point { column: 3, row: 4 },
            button: MouseButton::Left,
        }),
        Event::PaneMouseForwarded(PaneMouseForwarded {
            pane_id: PaneId::new(),
        }),
        Event::PluginMouseInput(PluginMouseInput {
            plugin_id: PluginId::new(),
        }),
        // What the shell inside a pane is doing, not koshi's own state.
        Event::PaneCommandStarted(PaneCommandStarted {
            pane_id: PaneId::new(),
        }),
        Event::PaneCommandFinished(PaneCommandFinished {
            pane_id: PaneId::new(),
            exit_code: Some(1),
        }),
        // Fires while a pane prints past the end of its buffer.
        Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
            pane_id: PaneId::new(),
            dropped_lines: 500,
            dropped_bytes: 40_000,
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
