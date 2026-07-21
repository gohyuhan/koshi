//! Tests for [`EventBus`]: subscribers receive published events in order over
//! their own queues, a dropped receiver ends its subscription, and a full
//! queue drops the overflowing event for that subscriber only.

use koshi_core::event::{Event, LayoutChanged, TabCreated};
use koshi_core::ids::TabId;

use super::*;

#[test]
fn a_new_bus_has_no_subscribers() {
    let bus = EventBus::new();
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn a_subscriber_receives_published_events_in_order() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let rx = bus.subscribe(EventFilter::All);

    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Event::TabCreated(TabCreated { tab_id: tab }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
}

#[test]
fn every_subscriber_receives_its_own_copy() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let first = bus.subscribe(EventFilter::All);
    let second = bus.subscribe(EventFilter::All);

    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(
        first.try_iter().collect::<Vec<_>>(),
        vec![Event::TabCreated(TabCreated { tab_id: tab })]
    );
    assert_eq!(
        second.try_iter().collect::<Vec<_>>(),
        vec![Event::TabCreated(TabCreated { tab_id: tab })]
    );
}

#[test]
fn a_dropped_receiver_is_removed_on_the_next_publish() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let keep = bus.subscribe(EventFilter::All);
    let dropped = bus.subscribe(EventFilter::All);
    drop(dropped);
    assert_eq!(bus.subscriber_count(), 2);

    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Event::TabCreated(TabCreated { tab_id: tab })]
    );
}

#[test]
fn a_full_queue_drops_the_event_for_that_subscriber_only() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let full = bus.subscribe(EventFilter::All);
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    let fresh = bus.subscribe(EventFilter::All);

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    // The full subscriber holds exactly the capacity of earlier events; the
    // overflowing publish never reached it.
    let full_events: Vec<_> = full.try_iter().collect();
    assert_eq!(full_events.len(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(full_events
        .iter()
        .all(|event| *event == Event::TabCreated(TabCreated { tab_id: tab })));
    // The fresh subscriber still received it.
    assert_eq!(
        fresh.try_iter().collect::<Vec<_>>(),
        vec![Event::LayoutChanged(LayoutChanged { tab_id: tab })]
    );
    // Both subscribers survive: a full queue is not a dead subscriber.
    assert_eq!(bus.subscriber_count(), 2);
}
