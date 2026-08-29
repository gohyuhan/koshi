//! Tests for the recent-events ring: order, the capacity bound, and what a
//! record carries.
//!
//! The ring is process-wide and the test binary runs its tests on many
//! threads, so every test here takes [`SERIAL`] first and clears the ring.
//!
//! [`the_ring_answers_after_a_thread_died_holding_it`] poisons [`RING`], and
//! that poisoning lasts for the rest of the binary. Every lock taken here
//! recovers it; a `RING.lock().unwrap()` added later would fail depending on
//! test order.

use super::*;

use std::sync::{Mutex as StdMutex, MutexGuard};

use koshi_core::event::{PaneCreated, PaneTyped, TypedPayload};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

/// Held for the length of one test, so two tests never share the ring.
static SERIAL: StdMutex<()> = StdMutex::new(());

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

    let held = recent();
    let names: Vec<&str> = held.iter().map(|record| record.name.as_ref()).collect();
    assert_eq!(names, ["PaneCreated", "Quit", "Restarting"]);
}

#[test]
fn a_record_carries_the_ids_its_event_named() {
    let _guard = exclusive();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    record(&Event::PaneCreated(PaneCreated { pane_id, tab_id }));

    let held = recent();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].name, "PaneCreated");
    assert_eq!(held[0].pane, Some(pane_id));
    assert_eq!(held[0].tab, Some(tab_id));
    assert_eq!(held[0].client, None);
}

#[test]
fn a_typed_character_leaves_the_character_behind() {
    let _guard = exclusive();
    let pane_id = PaneId::new();

    record(&Event::PaneTyped(PaneTyped {
        pane_id,
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        payload: TypedPayload::SafePublic('z'),
        timestamp: SystemTime::now(),
    }));

    let held = recent();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].name, "PaneTyped");
    assert_eq!(held[0].pane, Some(pane_id));
    let encoded = format!("{:?}", held[0]);
    assert!(!encoded.contains('z'), "{encoded}");
}

#[test]
fn a_full_ring_drops_exactly_the_oldest_record() {
    let _guard = exclusive();
    let oldest = PaneId::new();
    let newest = PaneId::new();

    record(&Event::PaneCreated(PaneCreated {
        pane_id: oldest,
        tab_id: TabId::new(),
    }));
    for _ in 1..CAPACITY {
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

// A thread that dies while it holds the ring poisons that lock. A bug report
// must still be answerable: recording and reading both recover the poisoned
// lock instead of panicking.
#[test]
fn the_ring_answers_after_a_thread_died_holding_it() {
    let _guard = exclusive();
    record(&Event::Quit);

    // Silence the default hook so the deliberate panic below stays quiet.
    let saved = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let died = std::thread::spawn(|| {
        let _held = RING.lock().expect("the ring is not poisoned yet");
        panic!("the thread holding the ring died");
    });
    assert!(died.join().is_err(), "the spawned thread must have died");
    std::panic::set_hook(saved);
    assert!(RING.is_poisoned(), "the lock must be poisoned");

    record(&Event::Restarting);

    let held = recent();
    let names: Vec<&str> = held.iter().map(|record| record.name.as_ref()).collect();
    assert_eq!(names, ["Quit", "Restarting"]);
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
    // Neither event names an id, so a torn record would show one.
    assert!(
        held.iter()
            .all(|event| event.pane.is_none() && event.tab.is_none()),
        "{held:?}"
    );
}
