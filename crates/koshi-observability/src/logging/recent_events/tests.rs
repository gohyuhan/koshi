//! Tests for the recent-events ring: order, the capacity bound, and what a
//! record carries. Each test records into a ring of its own.

use std::sync::Arc;

use super::*;

use koshi_core::event::{PaneCreated, QuitCause};
use koshi_core::ids::{PaneId, TabId};

/// A `PaneCreated` for a fresh pane in a fresh tab.
fn build_pane_created_event() -> Event {
    Event::PaneCreated(PaneCreated {
        pane_id: PaneId::new(),
        tab_id: Some(TabId::new()),
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
    let recent_event_ring = RecentEventRing::default();

    assert_eq!(recent_event_ring.list_recent_events(), Vec::new());
}

#[test]
fn records_come_back_in_the_order_they_were_made() {
    let recent_event_ring = RecentEventRing::default();

    recent_event_ring.record_event(&build_pane_created_event());
    recent_event_ring.record_event(&Event::Quit(QuitCause::Requested));
    recent_event_ring.record_event(&Event::Restarting);

    assert_eq!(
        list_event_names(&recent_event_ring.list_recent_events()),
        ["PaneCreated", "Quit", "Restarting"]
    );
}

#[test]
fn a_record_carries_the_ids_its_event_named() {
    let recent_event_ring = RecentEventRing::default();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    recent_event_ring.record_event(&Event::PaneCreated(PaneCreated {
        pane_id,
        tab_id: Some(tab_id),
    }));

    let recent_events = recent_event_ring.list_recent_events();
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
            command_id: None,
        }
    );
}

#[test]
fn a_full_ring_drops_exactly_the_oldest_record() {
    let recent_event_ring = RecentEventRing::default();
    let oldest_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let newest_pane_id = PaneId::new();

    recent_event_ring.record_event(&Event::PaneCreated(PaneCreated {
        pane_id: oldest_pane_id,
        tab_id: Some(TabId::new()),
    }));
    recent_event_ring.record_event(&Event::PaneCreated(PaneCreated {
        pane_id: second_pane_id,
        tab_id: Some(TabId::new()),
    }));
    for _ in 2..MAX_RECENT_EVENT_COUNT {
        recent_event_ring.record_event(&build_pane_created_event());
    }
    assert_eq!(
        recent_event_ring.list_recent_events().len(),
        MAX_RECENT_EVENT_COUNT
    );
    assert_eq!(
        recent_event_ring.list_recent_events()[0].pane_id,
        Some(oldest_pane_id)
    );

    recent_event_ring.record_event(&Event::PaneCreated(PaneCreated {
        pane_id: newest_pane_id,
        tab_id: Some(TabId::new()),
    }));

    let recent_events = recent_event_ring.list_recent_events();
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
    let recent_event_ring = RecentEventRing::default();

    let before_record_time = SystemTime::now();
    recent_event_ring.record_event(&Event::Quit(QuitCause::Requested));
    let after_record_time = SystemTime::now();

    let recent_events = recent_event_ring.list_recent_events();
    assert_eq!(recent_events.len(), 1);
    assert!(
        recent_events[0].occurred_at >= before_record_time
            && recent_events[0].occurred_at <= after_record_time,
        "{:?} is outside {before_record_time:?}..={after_record_time:?}",
        recent_events[0].occurred_at
    );
}

// A thread that dies while holding the ring poisons the lock. Recording and
// listing recover the poisoned lock.
#[test]
fn the_ring_answers_after_a_thread_died_holding_it() {
    let recent_event_ring = Arc::new(RecentEventRing::default());
    recent_event_ring.record_event(&Event::Quit(QuitCause::Requested));

    // `resume_unwind` skips the panic hook; the guard dropped while unwinding
    // poisons the lock.
    let dying_thread_ring = Arc::clone(&recent_event_ring);
    let ring_thread_join_result = std::thread::spawn(move || {
        let _recent_event_ring_guard = dying_thread_ring
            .recent_event_records
            .lock()
            .expect("the ring is not poisoned yet");
        std::panic::resume_unwind(Box::new("the thread holding the ring died"));
    })
    .join();
    assert_eq!(
        ring_thread_join_result.unwrap_err().downcast_ref::<&str>(),
        Some(&"the thread holding the ring died")
    );
    assert!(
        recent_event_ring.recent_event_records.is_poisoned(),
        "the lock must be poisoned"
    );

    recent_event_ring.record_event(&Event::Restarting);
    assert_eq!(
        list_event_names(&recent_event_ring.list_recent_events()),
        ["Quit", "Restarting"]
    );
}

#[test]
fn reading_the_ring_twice_gives_the_same_records_both_times() {
    let recent_event_ring = RecentEventRing::default();
    recent_event_ring.record_event(&build_pane_created_event());
    recent_event_ring.record_event(&Event::Quit(QuitCause::Requested));

    assert_eq!(
        recent_event_ring.list_recent_events(),
        recent_event_ring.list_recent_events()
    );
    assert_eq!(recent_event_ring.list_recent_events().len(), 2);
}

#[test]
fn two_threads_recording_at_once_both_land_and_neither_record_is_torn() {
    let recent_event_ring = Arc::new(RecentEventRing::default());
    let quit_thread_ring = Arc::clone(&recent_event_ring);
    let quit_thread = std::thread::spawn(move || {
        for _ in 0..100 {
            quit_thread_ring.record_event(&Event::Quit(QuitCause::Requested));
        }
    });
    let restart_thread_ring = Arc::clone(&recent_event_ring);
    let restart_thread = std::thread::spawn(move || {
        for _ in 0..100 {
            restart_thread_ring.record_event(&Event::Restarting);
        }
    });
    quit_thread.join().expect("the quitting thread finishes");
    restart_thread
        .join()
        .expect("the restarting thread finishes");

    let recent_events = recent_event_ring.list_recent_events();
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
