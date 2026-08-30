//! Tests for the recent-events ring: order, the capacity bound, and what a
//! record carries.
//!
//! The ring is process-wide. Every test takes [`SERIAL`] first, then clears the
//! ring.
//!
//! [`the_ring_answers_after_a_thread_died_holding_it`] poisons [`RING`] for the
//! rest of the binary; every other lock taken on [`RING`] here recovers it.

use super::*;

use koshi_core::event::{PaneCreated, PaneTyped, TypedPayload};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

/// Held for the length of one test; two tests never hold the ring at once.
static SERIAL: Mutex<()> = Mutex::new(());

/// Take the ring for this test and empty it.
fn exclusive() -> MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    clear();
    guard
}

/// A `PaneCreated` for a fresh pane in a fresh tab.
fn pane_created() -> Event {
    Event::PaneCreated(PaneCreated {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    })
}

/// The event names in `records`, in ring order.
fn names(records: &[RecentEvent]) -> Vec<&str> {
    records.iter().map(|record| record.name.as_ref()).collect()
}

#[test]
fn an_empty_ring_reports_nothing() {
    let _guard = exclusive();

    assert_eq!(recent(), Vec::new());
}

#[test]
fn records_come_back_in_the_order_they_were_made() {
    let _guard = exclusive();

    record(&pane_created());
    record(&Event::Quit);
    record(&Event::Restarting);

    assert_eq!(names(&recent()), ["PaneCreated", "Quit", "Restarting"]);
}

#[test]
fn a_record_carries_the_ids_its_event_named() {
    let _guard = exclusive();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    record(&Event::PaneCreated(PaneCreated { pane_id, tab_id }));

    let held = recent();
    assert_eq!(held.len(), 1);
    assert_eq!(
        held[0],
        RecentEvent {
            at: held[0].at,
            name: "PaneCreated".into(),
            session: None,
            client: None,
            tab: Some(tab_id),
            pane: Some(pane_id),
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
}

#[test]
fn a_typed_character_leaves_the_character_behind() {
    let _guard = exclusive();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let session_id = SessionId::new();
    let client_id = ClientId::new();

    record(&Event::PaneTyped(PaneTyped {
        pane_id,
        tab_id,
        session_id,
        client_id,
        payload: TypedPayload::SafePublic('z'),
        timestamp: SystemTime::now(),
    }));

    let held = recent();
    assert_eq!(held.len(), 1);
    assert_eq!(
        held[0],
        RecentEvent {
            at: held[0].at,
            name: "PaneTyped".into(),
            session: Some(session_id),
            client: Some(client_id),
            tab: Some(tab_id),
            pane: Some(pane_id),
            plugin: None,
            command: None,
            subscriber: None,
        }
    );
    let encoded = format!("{:?}", held[0]);
    assert!(!encoded.contains('z'), "{encoded}");
}

#[test]
fn a_full_ring_drops_exactly_the_oldest_record() {
    let _guard = exclusive();
    let oldest = PaneId::new();
    let second = PaneId::new();
    let newest = PaneId::new();

    record(&Event::PaneCreated(PaneCreated {
        pane_id: oldest,
        tab_id: TabId::new(),
    }));
    record(&Event::PaneCreated(PaneCreated {
        pane_id: second,
        tab_id: TabId::new(),
    }));
    for _ in 2..CAPACITY {
        record(&pane_created());
    }
    assert_eq!(recent().len(), CAPACITY);
    assert_eq!(recent()[0].pane, Some(oldest));

    record(&Event::PaneCreated(PaneCreated {
        pane_id: newest,
        tab_id: TabId::new(),
    }));

    let held = recent();
    assert_eq!(held.len(), CAPACITY);
    assert_ne!(held[0].pane, Some(oldest));
    assert_eq!(held[0].pane, Some(second));
    assert_eq!(held[CAPACITY - 1].pane, Some(newest));
}

#[test]
fn a_record_is_stamped_with_the_wall_clock_at_the_moment_it_was_made() {
    let _guard = exclusive();

    let before = SystemTime::now();
    record(&Event::Quit);
    let after = SystemTime::now();

    let held = recent();
    assert_eq!(held.len(), 1);
    assert!(
        held[0].at >= before && held[0].at <= after,
        "{:?} is outside {before:?}..={after:?}",
        held[0].at
    );
}

#[test]
fn clearing_the_ring_drops_every_record_and_recording_starts_over() {
    let _guard = exclusive();
    record(&pane_created());
    record(&Event::Quit);

    clear();
    assert_eq!(recent(), Vec::new());

    record(&Event::Restarting);
    assert_eq!(names(&recent()), ["Restarting"]);
}

// A thread that dies while holding the ring poisons the lock. `record`,
// `recent` and `clear` all recover it.
#[test]
fn the_ring_answers_after_a_thread_died_holding_it() {
    let _guard = exclusive();
    record(&Event::Quit);

    // `resume_unwind` skips the panic hook; the guard dropped while unwinding
    // poisons the lock.
    let died = std::thread::spawn(|| {
        let _held = RING.lock().expect("the ring is not poisoned yet");
        std::panic::resume_unwind(Box::new("the thread holding the ring died"));
    })
    .join();
    assert_eq!(
        died.unwrap_err().downcast_ref::<&str>(),
        Some(&"the thread holding the ring died")
    );
    assert!(RING.is_poisoned(), "the lock must be poisoned");

    record(&Event::Restarting);
    assert_eq!(names(&recent()), ["Quit", "Restarting"]);

    clear();
    assert_eq!(recent(), Vec::new());
}

#[test]
fn reading_the_ring_twice_gives_the_same_records_both_times() {
    let _guard = exclusive();
    record(&pane_created());
    record(&Event::Quit);

    assert_eq!(recent(), recent());
    assert_eq!(recent().len(), 2);
}

#[test]
fn two_threads_recording_at_once_both_land_and_neither_record_is_torn() {
    let _guard = exclusive();
    let quitter = std::thread::spawn(|| {
        for _ in 0..100 {
            record(&Event::Quit);
        }
    });
    let restarter = std::thread::spawn(|| {
        for _ in 0..100 {
            record(&Event::Restarting);
        }
    });
    quitter.join().expect("the quitting thread finishes");
    restarter.join().expect("the restarting thread finishes");

    let held = recent();
    assert_eq!(held.len(), 200);
    assert_eq!(
        held.iter().filter(|event| event.name == "Quit").count(),
        100
    );
    assert_eq!(
        held.iter()
            .filter(|event| event.name == "Restarting")
            .count(),
        100
    );
    // Neither event names an id; a torn record would show one.
    assert!(
        held.iter()
            .all(|event| event.pane.is_none() && event.tab.is_none()),
        "{held:?}"
    );
}
