//! Tests for [`EventBus`]: subscribers receive distinct ids and published
//! events in order over their own queues, a dropped receiver ends its
//! subscription, a lossy event that does not fit a full queue is dropped for
//! that subscriber only, and a critical one that does not fit desyncs the
//! subscriber until a snapshot resyncs it.

use koshi_core::event::{Event, LayoutChanged, PaneOutputUpdated, TabCreated};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{ClientSnapshot, PluginUiSnapshot, SessionSnapshot, TabSnapshot};

use super::*;

/// A minimal frame to resync from: one empty tab, no panes, no plugin UI.
fn snapshot() -> Box<RenderSnapshot> {
    let tab = TabId::new();
    Box::new(RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::from("session"),
            active_tab: TabSnapshot {
                id: tab,
                name: String::from("tab"),
                layout_solved: Vec::new(),
                effective_size: Size { cols: 80, rows: 24 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata: Vec::new(),
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport: Size { cols: 80, rows: 24 },
            active_tab: tab,
            focused_pane: None,
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
        plugin_ui: PluginUiSnapshot::default(),
    })
}

/// Fill every subscriber queue to capacity with `TabCreated` events for `tab`.
fn fill_to_capacity(bus: &mut EventBus, tab: TabId) {
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
}

#[test]
fn a_new_bus_has_no_subscribers() {
    let bus = EventBus::new();
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn subscribers_receive_distinct_ids() {
    let mut bus = EventBus::new();
    let (first, _first_rx) = bus.subscribe(EventFilter::All);
    let (second, _second_rx) = bus.subscribe(EventFilter::All);

    assert_ne!(first, second);
    assert_eq!(bus.subscribers[0].id, first);
    assert_eq!(bus.subscribers[1].id, second);
}

#[test]
fn a_subscriber_receives_published_events_in_order() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab })),
            Delivery::Event(Event::LayoutChanged(LayoutChanged { tab_id: tab })),
        ]
    );
}

#[test]
fn every_subscriber_receives_its_own_copy() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (_first_id, first) = bus.subscribe(EventFilter::All);
    let (_second_id, second) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(
        first.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
    assert_eq!(
        second.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
}

#[test]
fn a_dropped_receiver_is_removed_on_the_next_publish() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (_keep_id, keep) = bus.subscribe(EventFilter::All);
    let (dropped_id, dropped) = bus.subscribe(EventFilter::All);
    drop(dropped);
    assert_eq!(bus.subscriber_count(), 2);

    let removed = bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(removed, vec![dropped_id]);
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );

    let removed = bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(removed, Vec::new());
}

#[test]
fn a_lossy_event_that_does_not_fit_is_dropped_and_the_subscriber_stays_live() {
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    bus.publish(&Event::PaneOutputUpdated(PaneOutputUpdated {
        pane_id: pane,
    }));

    assert!(!bus.has_desynced());
    assert_eq!(bus.desynced(), Vec::new());

    // The queue holds exactly the earlier events; the overflowing one is gone.
    let backlog: Vec<_> = rx.try_iter().collect();
    assert_eq!(backlog.len(), SUBSCRIBER_QUEUE_CAPACITY);
    assert_eq!(
        backlog[SUBSCRIBER_QUEUE_CAPACITY - 1],
        Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }))
    );

    // Delivery never paused: the next event lands on the drained queue.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::LayoutChanged(LayoutChanged {
            tab_id: tab
        }))]
    );
    assert!(bus.contains(id));
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_critical_event_that_does_not_fit_desyncs_that_subscriber_only() {
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut bus = EventBus::new();
    let (full_id, full) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    let (fresh_id, fresh) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    assert!(bus.has_desynced());
    assert_eq!(bus.desynced(), vec![full_id]);

    // The fresh subscriber is untouched and still receiving.
    assert_eq!(
        fresh.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::LayoutChanged(LayoutChanged {
            tab_id: tab
        }))]
    );
    assert!(bus.contains(fresh_id));

    // Draining the desynced queue does not resume it: nothing further arrives,
    // critical or lossy, until a snapshot lands.
    let backlog: Vec<_> = full.try_iter().collect();
    assert_eq!(backlog.len(), SUBSCRIBER_QUEUE_CAPACITY);
    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    bus.publish(&Event::PaneOutputUpdated(PaneOutputUpdated {
        pane_id: pane,
    }));
    assert_eq!(full.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.desynced(), vec![full_id]);
    assert_eq!(bus.subscriber_count(), 2);
}

#[test]
fn a_resync_onto_a_still_full_queue_fails_and_leaves_the_subscriber_desynced() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);

    assert!(!bus.try_resync(id, snapshot()));

    assert_eq!(bus.desynced(), vec![id]);
    // Nothing was queued: the backlog is still exactly the pre-gap events.
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
}

#[test]
fn a_resync_queues_the_snapshot_behind_the_backlog_and_ahead_of_live_events() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    // Leave two pre-gap events on the queue so the ordering is visible.
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY - 2 {
        assert_eq!(
            rx.recv().unwrap(),
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }))
        );
    }

    let frame = snapshot();
    assert!(bus.try_resync(id, frame.clone()));
    assert_eq!(bus.desynced(), Vec::new());

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab })),
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab })),
            Delivery::Snapshot {
                snapshot: frame,
                lagged: SubscriberLagged {
                    subscriber_id: id,
                    dropped_count: 1,
                    event_class: EventClass::Critical,
                },
            },
            Delivery::Event(Event::LayoutChanged(LayoutChanged { tab_id: tab })),
        ]
    );
}

#[test]
fn the_dropped_count_holds_the_trigger_plus_withheld_critical_events_only() {
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    // The trigger, then three withheld critical events and two withheld lossy
    // ones: 1 + 3 = 4.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    for _ in 0..3 {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    for _ in 0..2 {
        bus.publish(&Event::PaneOutputUpdated(PaneOutputUpdated {
            pane_id: pane,
        }));
    }

    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    let frame = snapshot();
    assert!(bus.try_resync(id, frame.clone()));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            snapshot: frame,
            lagged: SubscriberLagged {
                subscriber_id: id,
                dropped_count: 4,
                event_class: EventClass::Critical,
            },
        }]
    );
}

#[test]
fn a_desynced_subscriber_whose_receiver_is_gone_is_removed_by_the_resync() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    drop(rx);

    assert!(!bus.try_resync(id, snapshot()));

    assert!(!bus.contains(id));
    assert_eq!(bus.subscriber_count(), 0);
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn a_resync_of_a_live_or_unknown_subscriber_does_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert!(!bus.try_resync(id, snapshot()));
    assert!(!bus.try_resync(SubscriberId::new(), snapshot()));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_second_desync_counts_from_one_again() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    // First gap: the trigger plus two more withheld critical events.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    for _ in 0..2 {
        bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    }
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(bus.try_resync(id, snapshot()));
    assert_eq!(rx.try_iter().count(), 1);

    // Second gap, on a queue refilled from scratch: the count restarts.
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    let frame = snapshot();
    assert!(bus.try_resync(id, frame.clone()));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            snapshot: frame,
            lagged: SubscriberLagged {
                subscriber_id: id,
                dropped_count: 1,
                event_class: EventClass::Critical,
            },
        }]
    );
}

#[test]
fn a_resync_of_a_just_resynced_subscriber_does_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(bus.try_resync(id, snapshot()));
    assert_eq!(rx.try_iter().count(), 1);

    assert!(!bus.try_resync(id, snapshot()));

    // No second frame was queued, and the subscriber is still live.
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.desynced(), Vec::new());
    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
}

#[test]
fn every_desynced_subscriber_is_listed_in_subscription_order() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (first_id, first) = bus.subscribe(EventFilter::All);
    let (second_id, second) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    let (live_id, _live) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    assert_eq!(bus.desynced(), vec![first_id, second_id]);
    assert!(bus.contains(live_id));
    assert_eq!(bus.subscriber_count(), 3);

    // Resyncing the first leaves the second listed, still in order.
    assert_eq!(first.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(bus.try_resync(first_id, snapshot()));
    assert_eq!(bus.desynced(), vec![second_id]);
    assert_eq!(second.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
}

#[test]
fn a_desynced_subscriber_whose_receiver_is_gone_survives_every_publish() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    drop(rx);

    // Withholding means no send, so no publish ever learns the receiver is
    // gone: the resync is the only thing that reaps it.
    let removed = bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(removed, Vec::new());
    assert!(bus.contains(id));
    assert_eq!(bus.desynced(), vec![id]);

    assert!(!bus.try_resync(id, snapshot()));

    assert!(!bus.contains(id));
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn unsubscribing_an_unknown_id_changes_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    bus.unsubscribe(SubscriberId::new());

    assert!(bus.contains(id));
    assert_eq!(bus.subscriber_count(), 1);
    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
}

#[test]
fn unsubscribe_removes_that_subscriber_only() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (gone_id, gone) = bus.subscribe(EventFilter::All);
    let (keep_id, keep) = bus.subscribe(EventFilter::All);

    bus.unsubscribe(gone_id);

    assert!(!bus.contains(gone_id));
    assert!(bus.contains(keep_id));
    assert_eq!(bus.subscriber_count(), 1);

    bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));
    assert_eq!(gone.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
}
