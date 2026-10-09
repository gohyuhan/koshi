//! Tests for created-id command output and the `kill-session` ending line.

use koshi_core::event::{Event, PaneCreated, QuitCause, TabCreated};
use koshi_core::ids::{PaneId, TabId};
use uuid::Uuid;

use super::*;

fn build_fixed_test_uuid() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

#[test]
fn a_new_pane_prints_one_pane_id_line() {
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let command_events = [Event::PaneCreated(PaneCreated {
        pane_id,
        tab_id: Some(TabId::from_uuid(build_fixed_test_uuid())),
    })];

    assert_eq!(
        render_created_events(&command_events),
        format!("[PANE ID]: {pane_id}\n")
    );
}

#[test]
fn a_new_tab_prints_tab_then_root_pane() {
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let command_events = [
        Event::TabCreated(TabCreated { tab_id }),
        Event::PaneCreated(PaneCreated {
            pane_id,
            tab_id: Some(tab_id),
        }),
    ];

    assert_eq!(
        render_created_events(&command_events),
        format!("[TAB ID]: {tab_id}\n[PANE ID]: {pane_id}\n")
    );
}

#[test]
fn unrelated_events_print_nothing() {
    assert_eq!(
        render_created_events(&[Event::Quit(QuitCause::Requested)]),
        ""
    );
}

#[test]
fn no_events_print_nothing() {
    assert_eq!(render_created_events(&[]), "");
}

#[test]
fn created_ids_keep_their_event_order() {
    let tab_id = TabId::from_uuid(build_fixed_test_uuid());
    let pane_id = PaneId::from_uuid(build_fixed_test_uuid());
    let command_events = [
        Event::PaneCreated(PaneCreated {
            pane_id,
            tab_id: Some(tab_id),
        }),
        Event::Quit(QuitCause::Requested),
        Event::TabCreated(TabCreated { tab_id }),
    ];

    assert_eq!(
        render_created_events(&command_events),
        format!("[PANE ID]: {pane_id}\n[TAB ID]: {tab_id}\n")
    );
}

#[test]
fn a_session_that_quit_with_nothing_left_running_prints_nothing() {
    assert_eq!(
        render_session_ending(&SessionEnding::Quit {
            stopped_process_count: 0
        }),
        ""
    );
}

#[test]
fn a_session_that_quit_names_the_processes_koshi_ended_after_it() {
    assert_eq!(
        render_session_ending(&SessionEnding::Quit {
            stopped_process_count: 1
        }),
        "the session quit; koshi ended 1 process it left running\n"
    );
    assert_eq!(
        render_session_ending(&SessionEnding::Quit {
            stopped_process_count: 2
        }),
        "the session quit; koshi ended 2 processes it left running\n"
    );
}

#[test]
fn a_session_that_did_not_quit_names_the_quit_failure_and_its_process() {
    assert_eq!(
        render_session_ending(&SessionEnding::Stopped {
            quit_failure: "IPC unavailable: the session did not answer in time".to_string(),
            session_process_id: 5000,
            stopped_process_count: 3,
        }),
        "the session did not quit (IPC unavailable: the session did not answer in time); \
         koshi ended its process 5000 and 3 processes under it\n"
    );
    assert_eq!(
        render_session_ending(&SessionEnding::Stopped {
            quit_failure: "session session-0 is not running".to_string(),
            session_process_id: 5000,
            stopped_process_count: 0,
        }),
        "the session did not quit (session session-0 is not running); koshi ended its \
         process 5000 and 0 processes under it\n"
    );
}
