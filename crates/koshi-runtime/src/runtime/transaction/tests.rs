//! Tests for [`TransactionScope`]: the buffer accumulates events in emission
//! order, and `commit` seals the batch into an applied result with one freshly
//! minted event id per buffered event, keyed to the command, delivering the
//! batch to the bus's subscribers.

use std::collections::HashSet;

use koshi_core::command::CommandResult;
use koshi_core::event::{Event, LayoutChanged, TabCreated, TabFocused};
use koshi_core::ids::{ClientId, CommandId, TabId};

use super::*;
use crate::runtime::bus::EventFilter;

#[test]
fn a_new_scope_buffers_no_events() {
    let scope = TransactionScope::new();
    assert!(scope.events().is_empty());
}

#[test]
fn emit_appends_in_call_order() {
    let tab = TabId::new();
    let prior = TabId::new();
    let client_id = ClientId::new();
    let mut scope = TransactionScope::new();
    scope.emit(Event::TabCreated(TabCreated { tab_id: tab }));
    scope.emit(Event::TabFocused(TabFocused {
        client_id,
        tab_id: tab,
        prior_tab: prior,
    }));
    scope.emit(Event::Quit);

    assert_eq!(
        scope.events(),
        &[
            Event::TabCreated(TabCreated { tab_id: tab }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: tab,
                prior_tab: prior,
            }),
            Event::Quit,
        ]
    );
}

#[test]
fn commit_mints_one_unique_id_per_event_keyed_to_the_command() {
    let command_id = CommandId::new();
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let mut scope = TransactionScope::new();
    scope.emit(Event::TabCreated(TabCreated { tab_id: tab }));
    scope.emit(Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    match scope.commit(command_id, &mut bus) {
        CommandResult::Ok {
            command_id: applied,
            emitted_events,
        } => {
            assert_eq!(applied, command_id);
            assert_eq!(emitted_events.len(), 2);
            let unique: HashSet<_> = emitted_events.iter().collect();
            assert_eq!(unique.len(), 2);
        }
        CommandResult::Rejected { .. } => panic!("commit must apply, never reject"),
    }
}

#[test]
fn committing_an_empty_scope_applies_with_no_events() {
    let command_id = CommandId::new();
    let mut bus = EventBus::new();
    let scope = TransactionScope::new();

    assert_eq!(
        scope.commit(command_id, &mut bus),
        CommandResult::Ok {
            command_id,
            emitted_events: Vec::new(),
        }
    );
}

#[test]
fn commit_delivers_the_batch_to_a_subscriber_in_emission_order() {
    let command_id = CommandId::new();
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let rx = bus.subscribe(EventFilter::All);
    let mut scope = TransactionScope::new();
    scope.emit(Event::TabCreated(TabCreated { tab_id: tab }));
    scope.emit(Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    let _ = scope.commit(command_id, &mut bus);

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Event::TabCreated(TabCreated { tab_id: tab }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
}

#[test]
fn two_scopes_commit_independently_with_no_shared_state() {
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let command_a = CommandId::new();
    let command_b = CommandId::new();

    let mut scope_a = TransactionScope::new();
    scope_a.emit(Event::TabCreated(TabCreated { tab_id: tab_a }));

    let mut scope_b = TransactionScope::new();
    scope_b.emit(Event::TabCreated(TabCreated { tab_id: tab_b }));
    scope_b.emit(Event::LayoutChanged(LayoutChanged { tab_id: tab_b }));

    // Scope A's buffer is untouched by scope B's later emits.
    assert_eq!(
        scope_a.events(),
        &[Event::TabCreated(TabCreated { tab_id: tab_a })]
    );

    let mut bus = EventBus::new();
    let result_a = scope_a.commit(command_a, &mut bus);
    let result_b = scope_b.commit(command_b, &mut bus);

    // Each result carries its own command id and its own event count — no
    // cross-instance bleed.
    match (result_a, result_b) {
        (
            CommandResult::Ok {
                command_id: applied_a,
                emitted_events: events_a,
            },
            CommandResult::Ok {
                command_id: applied_b,
                emitted_events: events_b,
            },
        ) => {
            assert_eq!(applied_a, command_a);
            assert_eq!(applied_b, command_b);
            assert_eq!(events_a.len(), 1);
            assert_eq!(events_b.len(), 2);
            let all_ids: HashSet<_> = events_a.iter().chain(events_b.iter()).collect();
            assert_eq!(
                all_ids.len(),
                3,
                "every minted id across both scopes is unique"
            );
        }
        _ => panic!("commit must apply, never reject"),
    }
}
