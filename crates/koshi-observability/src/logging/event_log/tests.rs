//! Tests for turning a committed runtime event into a log line.
//!
//! Coverage: the level each outcome gets, the ids and values a line carries,
//! the promise that a display name never reaches the file, the promise that no
//! event is ever an error, and one case per reason an event is left out of the
//! file.

use super::*;

use koshi_core::command::CopyTarget;
use koshi_core::event::{
    CommandRejected, ConfigReloaded, Copied, EventClass, InputMode, InputModeChanged,
    KeybindingMatched, LayoutChanged, MouseDragged, MousePressed, MouseReleased, MouseScrolled,
    MouseSelectChanged, PaneClosing, PaneCommandFinished, PaneCommandStarted, PaneCreated,
    PaneEnterPressed, PaneFocused, PaneMouseForwarded, PaneOutputUpdated, PaneProcessExited,
    PaneRemoved, PaneResumed, PaneScrollbackTruncated, PaneSuppressed, PaneTyped, PluginBroken,
    PluginDisabled, PluginDoctorCompleted, PluginEnabled, PluginInstalled, PluginLoadFailed,
    PluginMouseInput, PluginReloaded, PluginUninstalled, PluginUnloaded, PluginUpdated, PtyResized,
    RejectReason, SelectionChanged, SubmittedLinePayload, SubscriberLagged, TabClosed, TabCreated,
    TabFocused, TabMoved, TerminalTooSmallCause, TerminalTooSmallEntered, TerminalTooSmallExited,
    TypedPayload,
};
use koshi_core::geometry::{PaneArea, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
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

/// A `PaneTyped` carrying a printable character, the shape that would leak what
/// the user typed if the event were ever written.
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

// A pane opening is a fact that landed, so it is one info line, and it carries
// both ids needed to tie it back to the tab it happened in.
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

// A reload that applied is info; one that was refused is a warning, because the
// running config is the fallback koshi keeps using. Both name their session.
#[test]
fn config_reload_is_logged_at_info_naming_its_session() {
    let session_id = SessionId::new();

    let applied = captured(&[Event::ConfigReloaded(ConfigReloaded { session_id })]);

    assert!(applied.contains(r#""level":"INFO""#), "{applied}");
    assert!(
        applied.contains(r#""message":"config reloaded""#),
        "{applied}"
    );
    assert!(applied.contains(&session_id.to_string()), "{applied}");
}

// A rejection is written where the rejection is built, which every rejected
// command goes through. Writing it again from the event would put the same
// rejection in the file twice, so the event itself writes nothing.
#[test]
fn command_rejected_writes_nothing_because_the_rejection_itself_is_logged() {
    let out = captured(&[Event::CommandRejected(CommandRejected {
        id: CommandId::new(),
        reason: RejectReason::MinSize,
    })]);

    assert_eq!(out, "", "the rejection would be logged twice: {out}");
}

// A subscriber whose bounded queue overflowed is a warning: dropping is the
// answer koshi already has for a slow subscriber, and it kept running.
#[test]
fn subscriber_lag_is_a_warning_carrying_the_drop_count() {
    let subscriber_id = SubscriberId::new();

    let out = captured(&[Event::SubscriberLagged(SubscriberLagged {
        subscriber_id,
        dropped_count: 12,
        event_class: EventClass::Lossy,
    })]);

    assert!(out.contains(r#""level":"WARN""#), "{out}");
    assert!(
        out.contains(r#""message":"subscriber queue overflowed; events dropped""#),
        "{out}"
    );
    assert!(out.contains(r#""dropped_count":12"#), "{out}");
}

// A plugin that installed is info; one that would not load is a warning, since
// the session runs on without it.
#[test]
fn plugin_install_is_info_and_a_failed_load_is_a_warning() {
    let plugin_id = PluginId::new();

    let installed = captured(&[Event::Plugin(PluginEvent::Installed(PluginInstalled {
        plugin_id,
    }))]);
    assert!(installed.contains(r#""level":"INFO""#), "{installed}");
    assert!(
        installed.contains(r#""message":"plugin installed""#),
        "{installed}"
    );

    let failed = captured(&[Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
        plugin_id,
        reason: "wasm module has no `koshi` export".to_string(),
    }))]);
    assert!(failed.contains(r#""level":"WARN""#), "{failed}");
    assert!(
        failed.contains(r#""message":"plugin failed to load; continuing without it""#),
        "{failed}"
    );
    assert!(
        !failed.contains("wasm module has no"),
        "plugin reason leaked: {failed}"
    );
}

// The copy line records how much was copied and where to, never the text.
#[test]
fn copied_records_the_byte_count_and_target_only() {
    let out = captured(&[Event::Copied(Copied {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        target: CopyTarget::Osc52,
        byte_len: 41,
    })]);

    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"copied""#), "{out}");
    assert!(out.contains(r#""byte_len":41"#), "{out}");
    assert!(out.contains(r#""target":"Osc52""#), "{out}");
}

// Lock mode decides whether a key reaches koshi at all, so the switch is worth
// a line and the line says which mode is now in effect.
#[test]
fn input_mode_change_is_info_naming_the_mode_now_in_effect() {
    let out = captured(&[Event::InputModeChanged(InputModeChanged {
        client_id: ClientId::new(),
        mode: InputMode::Locked,
    })]);

    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"input mode changed""#), "{out}");
    assert!(out.contains(r#""mode":"Locked""#), "{out}");
}

// Mouse select decides whether a click reaches the program in the pane, so the
// switch is worth a line and the line says which way it went.
#[test]
fn mouse_select_change_is_info_naming_the_state_now_in_effect() {
    let out = captured(&[Event::MouseSelectChanged(MouseSelectChanged {
        client_id: ClientId::new(),
        on: true,
    })]);

    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"mouse select changed""#), "{out}");
    assert!(out.contains(r#""on":true"#), "{out}");
}

// The model rule, held as a test: an event is a fact koshi anticipated, so it
// always has a defined outcome and is never reported as an error.
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

    assert!(
        !out.contains(r#""level":"ERROR""#),
        "an event was logged as an error: {out}"
    );
}

// A session that keeps running under a new process image gets its own line: it
// is what explains a jump in the log's process id.
#[test]
fn restarting_is_info_saying_the_session_swaps_its_image() {
    let out = captured(&[Event::Restarting]);

    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(
        out.contains(r#""message":"session restarting into the binary on disk""#),
        "{out}"
    );
}

// One case per reason an event is kept out of the file. Together they must
// write nothing at all: a session of shell output, typing, and mouse motion
// must not put a single line in the log.
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

// The pane a close removes still gets its own line — the fact that completed is
// the one that is written, so a close is recorded exactly once.
#[test]
fn a_closed_pane_is_recorded_once_by_the_removal_not_the_announcement() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    let out = captured(&[
        Event::PaneClosing(PaneClosing { pane_id }),
        Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
    ]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""message":"pane removed""#), "{out}");
}

// An exit code the child reported is written as a number, and a child killed by
// a signal reports none — the field is then left off the line rather than
// written as a null, so a reader never has to tell "exited 0" from "no code".
#[test]
fn a_pane_exit_writes_its_code_as_a_number_and_omits_an_absent_one() {
    let with_code = captured(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id: PaneId::new(),
        exit_code: Some(0),
    })]);
    assert!(with_code.contains(r#""exit_code":0"#), "{with_code}");

    let signalled = captured(&[Event::PaneProcessExited(PaneProcessExited {
        pane_id: PaneId::new(),
        exit_code: None,
    })]);
    assert!(
        signalled.contains(r#""message":"pane process exited""#),
        "{signalled}"
    );
    assert!(!signalled.contains("exit_code"), "{signalled}");
}

// Focus and tab lifecycle: each fact a person can point at gets its own message
// and carries the ids that tie it back to where it happened. The "what it was
// before" fields exist on the events but are not written — a line records the
// state that now holds, not the one it replaced.
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

// A move is only readable with both ends of it, and the two must not be
// swapped: a tab dragged from slot 0 to slot 3 reads `old_index` 0, not 3.
#[test]
fn a_tab_move_records_the_slot_it_left_and_the_slot_it_landed_on() {
    let tab_id = TabId::new();

    let out = captured(&[Event::TabMoved(TabMoved {
        tab_id,
        old_index: 0,
        new_index: 3,
    })]);

    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"tab moved""#), "{out}");
    assert!(out.contains(r#""old_index":0"#), "{out}");
    assert!(out.contains(r#""new_index":3"#), "{out}");
    assert!(out.contains(&format!(r#""tab_id":"{tab_id}""#)), "{out}");
}

// The too-small event records the affected viewport, pane area, and cause.
#[test]
fn the_too_small_pair_says_which_way_it_went_and_the_size_it_happened_at() {
    let client_id = ClientId::new();

    let entered = captured(&[Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
        client_id,
        size: Size { cols: 10, rows: 3 },
        pane_area: Some(PaneArea::Starving),
        cause: TerminalTooSmallCause::Regions,
    })]);
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

    let exited = captured(&[Event::TerminalTooSmallExited(TerminalTooSmallExited {
        client_id,
        size: Size { cols: 80, rows: 24 },
    })]);
    assert!(exited.contains(r#""level":"INFO""#), "{exited}");
    assert!(
        exited.contains(r#""message":"terminal big enough again; panes shown""#),
        "{exited}"
    );
    assert!(exited.contains(r#""cols":80"#), "{exited}");
    assert!(exited.contains(r#""rows":24"#), "{exited}");
}

// The end of a session is the line that explains why the file stops.
#[test]
fn quitting_writes_one_info_line_saying_the_session_is_ending() {
    let out = captured(&[Event::Quit]);

    assert_eq!(out.lines().count(), 1, "expected exactly one line: {out}");
    assert!(out.contains(r#""level":"INFO""#), "{out}");
    assert!(out.contains(r#""message":"session quitting""#), "{out}");
}

// Every plugin lifecycle fact is a deliberate act, so each gets its own line,
// and no two of them read the same. The two that report a plugin koshi could
// not run are warnings; the rest are info.
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

    // The failure reason is payload content, so the log keeps it out.
    let broken = captured(&[Event::Plugin(PluginEvent::Broken(PluginBroken {
        plugin_id,
        reason: "manifest names no entry point".to_string(),
    }))]);
    assert!(
        !broken.contains("manifest names no entry point"),
        "plugin reason leaked: {broken}"
    );
}

// The rest of the events kept out of the file, one per reason. Together with
// `events_that_fire_faster_than_a_person_acts_write_nothing` this covers every
// silent variant, so a session of dragging, clicking, scrolling and printing
// must not put a single line in the log.
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
