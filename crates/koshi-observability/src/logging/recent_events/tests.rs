//! Tests for the recent-events ring: order, the capacity bound, and what a
//! record carries.
//!
//! The ring is process-wide. Every test takes [`RECENT_EVENT_TEST_LOCK`] first, then clears the
//! ring.
//!
//! [`the_ring_answers_after_a_thread_died_holding_it`] poisons [`RECENT_EVENT_RING`] for the
//! rest of the binary; every other lock taken on [`RECENT_EVENT_RING`] here recovers it.

use super::*;

use koshi_core::event::{PaneCreated, PaneTyped, QuitCause, TypedPayload};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

/// Held for the length of one test; two tests never hold the ring at once.
static RECENT_EVENT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Take the ring for this test and empty it.
fn lock_recent_events_for_test() -> MutexGuard<'static, ()> {
    let serialization_guard = RECENT_EVENT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    clear_recent_events();
    serialization_guard
}

/// A `PaneCreated` for a fresh pane in a fresh tab.
fn build_pane_created_event() -> Event {
    Event::PaneCreated(PaneCreated {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    })
}

/// The event names in `recent_events`, in ring order.
fn list_event_names(recent_events: &[RecentEvent]) -> Vec<&str> {
    recent_events
        .iter()
        .map(|recent_event| recent_event.event_name.as_ref())
        .collect()
}

#[test]
fn an_empty_ring_reports_nothing() {
    let _serialization_guard = lock_recent_events_for_test();

    assert_eq!(list_recent_events(), Vec::new());
}

#[test]
fn records_come_back_in_the_order_they_were_made() {
    let _serialization_guard = lock_recent_events_for_test();

    record_event(&build_pane_created_event());
    record_event(&Event::Quit(QuitCause::Requested));
    record_event(&Event::Restarting);

    assert_eq!(
        list_event_names(&list_recent_events()),
        ["PaneCreated", "Quit", "Restarting"]
    );
}

#[test]
fn a_record_carries_the_ids_its_event_named() {
    let _serialization_guard = lock_recent_events_for_test();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    record_event(&Event::PaneCreated(PaneCreated { pane_id, tab_id }));

    let recent_events = list_recent_events();
    assert_eq!(recent_events.len(), 1);
    assert_eq!(
        recent_events[0],
        RecentEvent {
            occurred_at: recent_events[0].occurred_at,
            event_name: "PaneCreated".into(),
            session_id: None,
            client_id: None,
            tab_id: Some(tab_id),
            pane_id: Some(pane_id),
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
}

#[test]
fn a_typed_character_leaves_the_character_behind() {
    let _serialization_guard = lock_recent_events_for_test();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let session_id = SessionId::new();
    let client_id = ClientId::new();

    record_event(&Event::PaneTyped(PaneTyped {
        pane_id,
        tab_id,
        session_id,
        client_id,
        typed_payload: TypedPayload::SafePublic('z'),
        accepted_at: SystemTime::now(),
    }));

    let recent_events = list_recent_events();
    assert_eq!(recent_events.len(), 1);
    assert_eq!(
        recent_events[0],
        RecentEvent {
            occurred_at: recent_events[0].occurred_at,
            event_name: "PaneTyped".into(),
            session_id: Some(session_id),
            client_id: Some(client_id),
            tab_id: Some(tab_id),
            pane_id: Some(pane_id),
            plugin_id: None,
            command_id: None,
            subscriber_id: None,
        }
    );
    let debug_output = format!("{:?}", recent_events[0]);
    assert!(!debug_output.contains('z'), "{debug_output}");
}

#[test]
fn a_full_ring_drops_exactly_the_oldest_record() {
    let _serialization_guard = lock_recent_events_for_test();
    let oldest_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let newest_pane_id = PaneId::new();

    record_event(&Event::PaneCreated(PaneCreated {
        pane_id: oldest_pane_id,
        tab_id: TabId::new(),
    }));
    record_event(&Event::PaneCreated(PaneCreated {
        pane_id: second_pane_id,
        tab_id: TabId::new(),
    }));
    for _ in 2..MAX_RECENT_EVENT_COUNT {
        record_event(&build_pane_created_event());
    }
    assert_eq!(list_recent_events().len(), MAX_RECENT_EVENT_COUNT);
    assert_eq!(list_recent_events()[0].pane_id, Some(oldest_pane_id));

    record_event(&Event::PaneCreated(PaneCreated {
        pane_id: newest_pane_id,
        tab_id: TabId::new(),
    }));

    let recent_events = list_recent_events();
    assert_eq!(recent_events.len(), MAX_RECENT_EVENT_COUNT);
    assert_ne!(recent_events[0].pane_id, Some(oldest_pane_id));
    assert_eq!(recent_events[0].pane_id, Some(second_pane_id));
    assert_eq!(
        recent_events[MAX_RECENT_EVENT_COUNT - 1].pane_id,
        Some(newest_pane_id)
    );
}

#[test]
fn a_record_is_stamped_with_the_wall_clock_at_the_moment_it_was_made() {
    let _serialization_guard = lock_recent_events_for_test();

    let before_record_time = SystemTime::now();
    record_event(&Event::Quit(QuitCause::Requested));
    let after_record_time = SystemTime::now();

    let recent_events = list_recent_events();
    assert_eq!(recent_events.len(), 1);
    assert!(
        recent_events[0].occurred_at >= before_record_time
            && recent_events[0].occurred_at <= after_record_time,
        "{:?} is outside {before_record_time:?}..={after_record_time:?}",
        recent_events[0].occurred_at
    );
}

#[test]
fn clearing_the_ring_drops_every_record_and_recording_starts_over() {
    let _serialization_guard = lock_recent_events_for_test();
    record_event(&build_pane_created_event());
    record_event(&Event::Quit(QuitCause::Requested));

    clear_recent_events();
    assert_eq!(list_recent_events(), Vec::new());

    record_event(&Event::Restarting);
    assert_eq!(list_event_names(&list_recent_events()), ["Restarting"]);
}

// A thread that dies while holding the ring poisons the lock. Recording,
// listing, and clearing recover the poisoned lock.
#[test]
fn the_ring_answers_after_a_thread_died_holding_it() {
    let _serialization_guard = lock_recent_events_for_test();
    record_event(&Event::Quit(QuitCause::Requested));

    // `resume_unwind` skips the panic hook; the guard dropped while unwinding
    // poisons the lock.
    let ring_thread_join_result = std::thread::spawn(|| {
        let _recent_event_ring_guard = RECENT_EVENT_RING
            .lock()
            .expect("the ring is not poisoned yet");
        std::panic::resume_unwind(Box::new("the thread holding the ring died"));
    })
    .join();
    assert_eq!(
        ring_thread_join_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"the thread holding the ring died")
    );
    assert!(RECENT_EVENT_RING.is_poisoned(), "the lock must be poisoned");

    record_event(&Event::Restarting);
    assert_eq!(
        list_event_names(&list_recent_events()),
        ["Quit", "Restarting"]
    );

    clear_recent_events();
    assert_eq!(list_recent_events(), Vec::new());
}

#[test]
fn reading_the_ring_twice_gives_the_same_records_both_times() {
    let _serialization_guard = lock_recent_events_for_test();
    record_event(&build_pane_created_event());
    record_event(&Event::Quit(QuitCause::Requested));

    assert_eq!(list_recent_events(), list_recent_events());
    assert_eq!(list_recent_events().len(), 2);
}

#[test]
fn two_threads_recording_at_once_both_land_and_neither_record_is_torn() {
    let _serialization_guard = lock_recent_events_for_test();
    let quit_thread = std::thread::spawn(|| {
        for _ in 0..100 {
            record_event(&Event::Quit(QuitCause::Requested));
        }
    });
    let restart_thread = std::thread::spawn(|| {
        for _ in 0..100 {
            record_event(&Event::Restarting);
        }
    });
    quit_thread.join().expect("the quitting thread finishes");
    restart_thread
        .join()
        .expect("the restarting thread finishes");

    let recent_events = list_recent_events();
    assert_eq!(recent_events.len(), 200);
    assert_eq!(
        recent_events
            .iter()
            .filter(|recent_event| recent_event.event_name == "Quit")
            .count(),
        100
    );
    assert_eq!(
        recent_events
            .iter()
            .filter(|recent_event| recent_event.event_name == "Restarting")
            .count(),
        100
    );
    // Neither event names an id; a torn record would show one.
    assert!(
        recent_events.iter().all(|recent_event| {
            recent_event.pane_id.is_none() && recent_event.tab_id.is_none()
        }),
        "{recent_events:?}"
    );
}
