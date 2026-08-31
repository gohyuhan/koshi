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

/// Log `events` through a thread-local JSON subscriber and return everything
/// written. Empty output means every event was left out of the file.
fn captured(events: &[Event]) -> String {
    let (_guard, logs) = with_test_writer();
    for event in events {
        log_event(event);
    }
    logs.contents()
}

/// A `PaneTyped` carrying the printable character `'x'`.
fn typed_a_character() -> Event {
    Event::PaneTyped(PaneTyped {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        payload: TypedPayload::SafePublic('x'),
        timestamp: std::time::SystemTime::UNIX_EPOCH,
    })
}

#[test]
fn pane_created_is_one_info_line_carrying_its_pane_and_tab_ids() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let out = captured(&[Event::PaneCreated(PaneCreated { pane_id, tab_id })]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "wrong level: {out}");
    assert!(out.contains(r#""message":"pane created""#), "{out}");
    assert!(out.contains(&format!(r#""pane_id":"{pane_id}""#)), "{out}");
    assert!(out.contains(&format!(r#""tab_id":"{tab_id}""#)), "{out}");
}

// Two events committed together write two lines, in the order they were
// committed.
#[test]
fn a_new_pane_writes_its_created_line_before_its_focused_line() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let out = captured(&[
        Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        Event::PaneFocused(PaneFocused {
            client_id: ClientId::new(),
            tab_id,
            pane_id,
            prior_pane: None,
        }),
    ]);

    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "expected exactly two lines: {out}");
    assert!(lines[0].contains(r#""message":"pane created""#), "{out}");
    assert!(lines[1].contains(r#""message":"pane focused""#), "{out}");
}

#[test]
fn config_reload_is_logged_at_info_naming_its_session() {
    let session_id = SessionId::new();

    let applied = captured(&[Event::ConfigReloaded(ConfigReloaded { session_id })]);

    assert_eq!(
        applied.lines().count(),
        1,
        "expected exactly one line: {applied}"
    );
    assert!(applied.contains(r#""level":"INFO""#), "{applied}");
    assert!(
        applied.contains(r#""message":"config reloaded""#),
        "{applied}"
    );
    assert!(
        applied.contains(&format!(r#""session_id":"{session_id}""#)),
        "{applied}"
    );
}

// The rejection line is written where the rejection is built, in
// `koshi-runtime`; the event writes nothing.
#[test]
fn command_rejected_writes_nothing_because_the_rejection_itself_is_logged() {
    let out = captured(&[Event::CommandRejected(CommandRejected {
        id: CommandId::new(),
        reason: RejectReason::MinSize,
    })]);

    assert_eq!(out, "", "the rejection would be logged twice: {out}");
}

#[test]
fn subscriber_lag_is_a_warning_carrying_the_drop_count() {
    let subscriber_id = SubscriberId::new();

    let out = captured(&[Event::SubscriberLagged(SubscriberLagged {
        subscriber_id,
        dropped_count: 12,
        event_class: EventClass::Lossy,
    })]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"WARN""#), "{out}");
    assert!(
        out.contains(r#""message":"subscriber queue overflowed; events dropped""#),
        "{out}"
    );
    assert!(
        out.contains(&format!(r#""subscriber_id":"{subscriber_id}""#)),
        "{out}"
    );
    assert!(out.contains(r#""dropped_count":12"#), "{out}");
    assert!(out.contains(r#""event_class":"Lossy""#), "{out}");
}

#[test]
fn plugin_install_is_info_and_a_failed_load_is_a_warning() {
    let plugin_id = PluginId::new();

    let installed = captured(&[Event::Plugin(PluginEvent::Installed(PluginInstalled {
        plugin_id,
    }))]);
    assert_eq!(
        installed.lines().count(),
        1,
        "expected exactly one line: {installed}"
    );
    assert!(installed.contains(r#""level":"INFO""#), "{installed}");
    assert!(
        installed.contains(r#""message":"plugin installed""#),
        "{installed}"
    );
    assert!(
        installed.contains(&format!(r#""plugin_id":"{plugin_id}""#)),
        "{installed}"
    );

    let failed = captured(&[Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
        plugin_id,
        reason: "wasm module has no `koshi` export".to_string(),
    }))]);
    assert_eq!(
        failed.lines().count(),
        1,
        "expected exactly one line: {failed}"
    );
    assert!(failed.contains(r#""level":"WARN""#), "{failed}");
    assert!(
        failed.contains(r#""message":"plugin failed to load; continuing without it""#),
        "{failed}"
    );
    assert!(
        failed.contains(&format!(r#""plugin_id":"{plugin_id}""#)),
        "{failed}"
    );
    assert!(
        failed.contains(r#""reason":"wasm module has no `koshi` export""#),
        "{failed}"
    );
}

// The line carries the byte count and the target, never the copied text.
#[test]
fn copied_records_the_byte_count_and_target_only() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();

    let out = captured(&[Event::Copied(Copied {
        client_id,
        pane_id,
        target: CopyTarget::Osc52,
        byte_len: 41,
    })]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"copied""#), "{out}");
    assert!(
        out.contains(&format!(r#""client_id":"{client_id}""#)),
        "{out}"
    );
    assert!(out.contains(&format!(r#""pane_id":"{pane_id}""#)), "{out}");
    assert!(out.contains(r#""byte_len":41"#), "{out}");
    assert!(out.contains(r#""target":"Osc52""#), "{out}");
}

#[test]
fn input_mode_change_is_info_naming_the_mode_now_in_effect() {
    let client_id = ClientId::new();

    let out = captured(&[Event::InputModeChanged(InputModeChanged {
        client_id,
        mode: LockMode::Locked,
    })]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"input mode changed""#), "{out}");
    assert!(
        out.contains(&format!(r#""client_id":"{client_id}""#)),
        "{out}"
    );
    assert!(out.contains(r#""mode":"Locked""#), "{out}");
}

#[test]
fn mouse_select_change_is_info_naming_the_state_now_in_effect() {
    let client_id = ClientId::new();

    let out = captured(&[Event::MouseSelectChanged(MouseSelectChanged {
        client_id,
        on: true,
    })]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"mouse select changed""#), "{out}");
    assert!(
        out.contains(&format!(r#""client_id":"{client_id}""#)),
        "{out}"
    );
    assert!(out.contains(r#""on":true"#), "{out}");
}

// Every written event is `info` or `warn`. `CommandRejected` writes nothing.
#[test]
fn no_event_is_ever_logged_as_an_error() {
    let out = captured(&[
        Event::PaneCreated(PaneCreated {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        }),
        Event::ConfigReloaded(ConfigReloaded {
            session_id: SessionId::new(),
        }),
        Event::CommandRejected(CommandRejected {
            id: CommandId::new(),
            reason: RejectReason::TargetGone,
        }),
        Event::SubscriberLagged(SubscriberLagged {
            subscriber_id: SubscriberId::new(),
            dropped_count: 1,
            event_class: EventClass::Critical,
        }),
        Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
            plugin_id: PluginId::new(),
            reason: "unreadable".to_string(),
        })),
        Event::Quit,
        Event::Restarting,
    ]);

    assert_eq!(out.lines().count(), 6, "expected six lines: {out}");
    assert!(
        !out.contains(r#""level":"ERROR""#),
        "an event was logged as an error: {out}"
    );
}

#[test]
fn restarting_is_info_saying_the_session_swaps_its_image() {
    let out = captured(&[Event::Restarting]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(
        out.contains(r#""message":"session restarting into the binary on disk""#),
        "{out}"
    );
}

// One event per reason the file leaves it out; together they write nothing.
#[test]
fn events_that_fire_faster_than_a_person_acts_write_nothing() {
    let out = captured(&[
        // Terminal content ticking over as a pane prints.
        Event::PaneOutputUpdated(PaneOutputUpdated {
            pane_id: PaneId::new(),
        }),
        // One per pane per frame while a window edge is dragged.
        Event::PtyResized(PtyResized {
            pane_id: PaneId::new(),
            size: PtySize { cols: 80, rows: 24 },
        }),
        // One per keystroke, and it carries the character.
        typed_a_character(),
        // One per keystroke that resolves to a command.
        Event::KeybindingMatched(KeybindingMatched {
            client_id: ClientId::new(),
            command_id: CommandId::new(),
        }),
        // One per wheel notch.
        Event::MouseScrolled(MouseScrolled {
            client_id: ClientId::new(),
            pane: Some(PaneId::new()),
            position: Point { x: 4, y: 9 },
            direction: ScrollDirection::Down,
        }),
        // Announces the close that `PaneRemoved` completes.
        Event::PaneClosing(PaneClosing {
            pane_id: PaneId::new(),
        }),
    ]);

    assert_eq!(
        out, "",
        "a high-frequency event reached the log file: {out}"
    );
}

#[test]
fn a_closed_pane_is_recorded_once_by_the_removal_not_the_announcement() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let out = captured(&[
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
    ]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"pane removed""#), "{out}");
    assert!(out.contains(&format!(r#""pane_id":"{pane_id}""#)), "{out}");
    assert!(out.contains(&format!(r#""tab_id":"{tab_id}""#)), "{out}");
}

// `exit_code: None` leaves the field off the line; it is not written as
// `null`.
#[test]
fn a_pane_exit_writes_its_code_as_a_number_and_omits_an_absent_one() {
    let pane_id = PaneId::new();

    let with_code = captured(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: Some(0),
    })]);
    assert_eq!(
        with_code.lines().count(),
        1,
        "expected exactly one line: {with_code}"
    );
    assert!(with_code.contains(r#""level":"INFO""#), "{with_code}");
    assert!(
        with_code.contains(r#""message":"pane process exited""#),
        "{with_code}"
    );
    assert!(
        with_code.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{with_code}"
    );
    assert!(with_code.contains(r#""exit_code":0"#), "{with_code}");

    let negative = captured(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: Some(-1),
    })]);
    assert!(negative.contains(r#""exit_code":-1"#), "{negative}");

    let signalled = captured(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id,
        exit_code: None,
    })]);
    assert_eq!(
        signalled.lines().count(),
        1,
        "expected exactly one line: {signalled}"
    );
    assert!(
        signalled.contains(r#""message":"pane process exited""#),
        "{signalled}"
    );
    assert!(
        signalled.contains(&format!(r#""pane_id":"{pane_id}""#)),
        "{signalled}"
    );
    assert!(!signalled.contains("exit_code"), "{signalled}");
}

// `Some(0)` is the only code that logs at info. Every other code, and
// `None` for a signal-terminated child, logs at warn.
#[test]
fn a_pane_exit_is_info_only_when_the_program_exited_zero() {
    let pane_id = PaneId::new();
    let cases = [(Some(0), "INFO"), (Some(127), "WARN"), (None, "WARN")];

    for (exit_code, level) in cases {
        let out = captured(&[Event::PaneProcessExited(PaneProcessExited {
            pane_id,
            exit_code,
        })]);

        assert_eq!(out.lines().count(), 1, "{exit_code:?}: {out}");
        assert!(
            out.contains(&format!(r#""level":"{level}""#)),
            "{exit_code:?} must log at {level}: {out}"
        );
        assert!(
            out.contains(r#""message":"pane process exited""#),
            "{exit_code:?}: {out}"
        );
        assert!(
            out.contains(&format!(r#""pane_id":"{pane_id}""#)),
            "{exit_code:?}: {out}"
        );
    }
}

// `prior_pane` and `prior_tab` are on the events and are not written.
#[test]
fn each_focus_and_tab_lifecycle_fact_writes_its_own_message_and_ids() {
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let prior_pane = PaneId::new();
    let prior_tab = TabId::new();

    let focused_pane = captured(&[Event::PaneFocused(PaneFocused {
        client_id,
        tab_id,
        pane_id,
        prior_pane: Some(prior_pane),
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
        !focused_pane.contains(&prior_pane.to_string()),
        "the pane focus left behind is not written: {focused_pane}"
    );

    let created_tab = captured(&[Event::TabCreated(TabCreated { tab_id })]);
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

    let closed_tab = captured(&[Event::TabClosed(TabClosed { tab_id })]);
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

    let focused_tab = captured(&[Event::TabFocused(TabFocused {
        client_id,
        tab_id,
        prior_tab,
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
        !focused_tab.contains(&prior_tab.to_string()),
        "the tab focus left behind is not written: {focused_tab}"
    );
}

// A tab dragged from slot 0 to slot 3 writes `old_index` 0 and `new_index` 3.
#[test]
fn a_tab_move_records_the_slot_it_left_and_the_slot_it_landed_on() {
    let tab_id = TabId::new();

    let out = captured(&[Event::TabMoved(TabMoved {
        tab_id,
        old_index: 0,
        new_index: 3,
    })]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"tab moved""#), "{out}");
    assert!(out.contains(r#""old_index":0"#), "{out}");
    assert!(out.contains(r#""new_index":3"#), "{out}");
    assert!(out.contains(&format!(r#""tab_id":"{tab_id}""#)), "{out}");
}

// Entering writes the size, the pane area and the cause; leaving writes the
// size. An absent pane area is written as the string `None`.
#[test]
fn the_too_small_pair_says_which_way_it_went_and_the_size_it_happened_at() {
    let client_id = ClientId::new();

    let entered = captured(&[Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
        client_id,
        size: Size { cols: 10, rows: 3 },
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
    assert!(entered.contains(r#""cols":10"#), "{entered}");
    assert!(entered.contains(r#""rows":3"#), "{entered}");
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
        captured(&[Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
            client_id,
            size: Size { cols: 10, rows: 3 },
            pane_area: None,
            cause: TerminalTooSmallCause::Regions,
        })]);
    assert!(
        entered_without_area.contains(r#""pane_area":"None""#),
        "{entered_without_area}"
    );

    let exited = captured(&[Event::TerminalTooSmallExited(TerminalTooSmallExited {
        client_id,
        size: Size { cols: 80, rows: 24 },
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
    assert!(exited.contains(r#""cols":80"#), "{exited}");
    assert!(exited.contains(r#""rows":24"#), "{exited}");
    assert!(
        exited.contains(&format!(r#""client_id":"{client_id}""#)),
        "{exited}"
    );
}

#[test]
fn quitting_writes_one_info_line_saying_the_session_is_ending() {
    let out = captured(&[Event::Quit]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"session quitting""#), "{out}");
}

// Each plugin event writes its own message. `LoadFailed` and `Broken` are
// warnings; the rest are info.
#[test]
fn each_plugin_lifecycle_fact_writes_its_own_message_at_its_own_level() {
    let plugin_id = PluginId::new();
    let cases = [
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
                reason: "manifest names no entry point".to_string(),
            })),
            "WARN",
            "plugin marked broken and disabled",
        ),
    ];

    for (event, level, message) in cases {
        let out = captured(std::slice::from_ref(&event));
        assert_eq!(out.lines().count(), 1, "{message}: {out}");
        assert!(
            out.contains(&format!(r#""level":"{level}""#)),
            "{message}: {out}"
        );
        assert!(
            out.contains(&format!(r#""message":"{message}""#)),
            "{message}: {out}"
        );
        assert!(
            out.contains(&format!(r#""plugin_id":"{plugin_id}""#)),
            "{message}: {out}"
        );
    }

    // `Broken` writes its `reason` on the line.
    let broken = captured(&[Event::Plugin(PluginEvent::Broken(PluginBroken {
        plugin_id,
        reason: "manifest names no entry point".to_string(),
    }))]);
    assert!(
        broken.contains(r#""reason":"manifest names no entry point""#),
        "{broken}"
    );
}

// The remaining silent variants. With
// `events_that_fire_faster_than_a_person_acts_write_nothing` and
// `command_rejected_writes_nothing_because_the_rejection_itself_is_logged`,
// every silent arm of `log_event` is covered.
#[test]
fn the_remaining_silent_events_write_nothing() {
    let out = captured(&[
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
            line: SubmittedLinePayload::SafePublic("ls -la".to_string()),
            timestamp: std::time::SystemTime::UNIX_EPOCH,
        }),
        // One per click and per step of a drag.
        Event::MousePressed(MousePressed {
            client_id: ClientId::new(),
            pane: Some(PaneId::new()),
            position: Point { x: 1, y: 2 },
            button: MouseButton::Left,
        }),
        Event::MouseReleased(MouseReleased {
            client_id: ClientId::new(),
            pane: Some(PaneId::new()),
            position: Point { x: 1, y: 2 },
            button: MouseButton::Left,
        }),
        Event::MouseDragged(MouseDragged {
            client_id: ClientId::new(),
            pane: None,
            position: Point { x: 3, y: 4 },
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

    assert_eq!(out, "", "an event kept out of the file reached it: {out}");
}
