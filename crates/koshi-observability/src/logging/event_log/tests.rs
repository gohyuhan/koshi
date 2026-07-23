//! Tests for turning a committed runtime event into a log line.
//!
//! Coverage: the level each outcome gets, the ids and values a line carries,
//! the promise that a display name never reaches the file, the promise that no
//! event is ever an error, and one case per reason an event is left out of the
//! file.

use super::*;

use koshi_core::command::CopyTarget;
use koshi_core::event::{
    CommandRejected, ConfigReloadFailed, ConfigReloaded, Copied, EventClass, InputMode,
    InputModeChanged, KeybindingMatched, MouseScrolled, PaneClosing, PaneCreated,
    PaneOutputUpdated, PaneProcessExited, PaneRemoved, PaneTyped, PluginInstalled,
    PluginLoadFailed, PtyResized, RejectReason, SubscriberLagged, TypedPayload,
};
use koshi_core::geometry::Point;
use koshi_core::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
use koshi_core::mouse::ScrollDirection;
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
fn config_reload_is_info_when_it_applied_and_a_warning_when_it_was_refused() {
    let session_id = SessionId::new();

    let applied = captured(&[Event::ConfigReloaded(ConfigReloaded { session_id })]);
    assert!(applied.contains(r#""level":"INFO""#), "{applied}");
    assert!(
        applied.contains(r#""message":"config reloaded""#),
        "{applied}"
    );

    let refused = captured(&[Event::ConfigReloadFailed(ConfigReloadFailed {
        session_id,
        reason: "`<C-a>` is bound twice".to_string(),
    })]);
    assert!(refused.contains(r#""level":"WARN""#), "{refused}");
    assert!(
        refused.contains(r#""message":"config reload failed; keeping the running config""#),
        "{refused}"
    );
    assert!(refused.contains("`<C-a>` is bound twice"), "{refused}");
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

// The model rule, held as a test: an event is a fact koshi anticipated, so it
// always has a defined outcome and is never reported as an error.
#[test]
fn no_event_is_ever_logged_as_an_error() {
    let out = captured(&[
        Event::PaneCreated(PaneCreated {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        }),
        Event::ConfigReloadFailed(ConfigReloadFailed {
            session_id: SessionId::new(),
            reason: "conflicting bindings".to_string(),
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
    ]);

    assert!(
        !out.contains(r#""level":"ERROR""#),
        "an event was logged as an error: {out}"
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
