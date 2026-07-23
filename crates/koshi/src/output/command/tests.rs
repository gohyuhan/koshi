//! Tests for created-id command output.

use koshi_core::event::{Event, PaneCreated, TabCreated};
use koshi_core::ids::{PaneId, TabId};
use uuid::Uuid;

use super::*;

fn id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

#[test]
fn a_new_pane_prints_one_pane_id_line() {
    let pane_id = PaneId::from_uuid(id());
    let events = [Event::PaneCreated(PaneCreated {
        pane_id,
        tab_id: TabId::from_uuid(id()),
    })];

    assert_eq!(
        render_created_events(&events),
        format!("[PANE ID]: {pane_id}\n")
    );
}

#[test]
fn a_new_tab_prints_tab_then_root_pane() {
    let tab_id = TabId::from_uuid(id());
    let pane_id = PaneId::from_uuid(id());
    let events = [
        Event::TabCreated(TabCreated { tab_id }),
        Event::PaneCreated(PaneCreated { pane_id, tab_id }),
    ];

    assert_eq!(
        render_created_events(&events),
        format!("[TAB ID]: {tab_id}\n[PANE ID]: {pane_id}\n")
    );
}

#[test]
fn unrelated_events_print_nothing() {
    assert_eq!(render_created_events(&[Event::Quit]), "");
}

#[test]
fn created_ids_keep_their_event_order() {
    let tab_id = TabId::from_uuid(id());
    let pane_id = PaneId::from_uuid(id());
    let events = [
        Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        Event::Quit,
        Event::TabCreated(TabCreated { tab_id }),
    ];

    assert_eq!(
        render_created_events(&events),
        format!("[PANE ID]: {pane_id}\n[TAB ID]: {tab_id}\n")
    );
}
