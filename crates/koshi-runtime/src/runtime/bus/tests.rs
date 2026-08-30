//! Tests for [`EventBus`]: subscribers receive distinct ids and published
//! events in order over their own queues, a dropped receiver ends its
//! subscription, a lossy event that does not fit a full queue is dropped for
//! that subscriber only, and a critical one that does not fit desyncs the
//! subscriber until a snapshot resyncs it. The quit and the restart each end
//! the stream: each reaches a desynced subscriber as well as a live one, and
//! each raises the bus's ending notice for the queues they do not fit, which
//! keeps the first ending raised. Then the painted frame: it lands on a live
//! subscriber's queue, is refused by a desynced one, and on a full queue is
//! refused while the subscriber stays live. Then a round's mouse answers, which
//! ride the same queue and desync the subscriber when they do not fit. Then
//! bytes for the terminal a subscriber's client runs in and the session a
//! subscriber's client moves to, which ride it the same way, and which are each
//! refused for an unknown subscriber and for a desynced one.
//!
//! Then the two wire conversions: the filter an attaching client sent becomes
//! the bus's own, and one queue item becomes the frame that client is sent.

use koshi_core::command::CopyTarget;
use koshi_core::event::{
    CommandRejected, ConfigReloaded, Copied, Event, InputModeChanged, KeybindingMatched,
    LayoutChanged, MouseDragged, MousePressed, MouseReleased, MouseScrolled, MouseSelectChanged,
    PaneClosing, PaneCommandFinished, PaneCommandStarted, PaneCreated, PaneEnterPressed,
    PaneFocused, PaneMouseForwarded, PaneOutputUpdated, PaneProcessExited, PaneRemoved,
    PaneResumed, PaneScrollbackTruncated, PaneSuppressed, PaneTyped, PluginEvent, PluginInstalled,
    PluginMouseInput, PtyResized, RejectReason, SelectionChanged, SubmittedLinePayload, TabClosed,
    TabCreated, TabFocused, TabMoved, TerminalTooSmallCause, TerminalTooSmallEntered,
    TerminalTooSmallExited, TypedPayload,
};
use koshi_core::geometry::{Direction, PaneArea, Point, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, ScrollDirection};
use koshi_core::process::PtySize;
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{ClientSnapshot, PluginUiSnapshot, SessionSnapshot, TabSnapshot};
use std::time::SystemTime;

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
                gap: 0,
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
fn a_new_bus_has_raised_no_ending() {
    let bus = EventBus::new();
    assert_eq!(bus.ending_notice().raised(), None);
}

#[test]
fn the_default_filter_is_every_event() {
    assert_eq!(EventFilter::default(), EventFilter::All);
}

#[test]
fn publishing_to_a_bus_with_no_subscribers_removes_nobody() {
    let tab = TabId::new();
    let mut bus = EventBus::new();

    let removed = bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(removed, Vec::new());
    assert_eq!(bus.subscriber_count(), 0);
    assert!(!bus.has_desynced());
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
fn every_dropped_receiver_is_returned_in_subscription_order() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (first_id, first) = bus.subscribe(EventFilter::All);
    let (keep_id, keep) = bus.subscribe(EventFilter::All);
    let (third_id, third) = bus.subscribe(EventFilter::All);
    drop(first);
    drop(third);

    let removed = bus.publish(&Event::TabCreated(TabCreated { tab_id: tab }));

    assert_eq!(removed, vec![first_id, third_id]);
    assert!(bus.contains(keep_id));
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))]
    );
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
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }));
            SUBSCRIBER_QUEUE_CAPACITY
        ]
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
    assert_eq!(
        full.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }));
            SUBSCRIBER_QUEUE_CAPACITY
        ]
    );
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
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }));
            SUBSCRIBER_QUEUE_CAPACITY
        ]
    );
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
fn a_live_subscriber_whose_queue_is_full_misses_the_restart() {
    // The queue is bounded, so the restart is dropped like any other event that
    // does not fit. The ending notice holds it instead, which is what the
    // client's writing thread reads rather than waiting on the queue.
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    bus.publish(&Event::Restarting);

    assert_eq!(
        bus.ending_notice().raised(),
        Some(SessionEnding::Restarting)
    );
    assert_eq!(bus.desynced(), vec![id]);
    let taken = rx.try_iter().collect::<Vec<_>>();
    assert_eq!(taken.len(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(
        taken
            .iter()
            .all(|delivery| *delivery
                == Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }))),
        "the queue must hold its backlog and nothing else"
    );
}

#[test]
fn publishing_the_quit_raises_the_ending_notice_and_delivers_it() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    let removed = bus.publish(&Event::Quit);

    assert_eq!(removed, Vec::new());
    assert_eq!(bus.ending_notice().raised(), Some(SessionEnding::Quit));
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit)]
    );
    assert_eq!(bus.desynced(), Vec::new());
    assert!(bus.contains(id));
}

#[test]
fn the_ending_notice_keeps_the_first_ending_it_was_raised_with() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::Restarting);
    bus.publish(&Event::Quit);

    assert_eq!(
        bus.ending_notice().raised(),
        Some(SessionEnding::Restarting)
    );
    // Both events still ride the queue; only the notice is set once.
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::Restarting),
            Delivery::Event(Event::Quit),
        ]
    );
}

#[test]
fn a_desynced_subscriber_is_told_the_session_is_restarting() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);
    // The client drains its backlog, so the queue has room again while the
    // subscriber is still awaiting its snapshot.
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    bus.publish(&Event::Restarting);

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Restarting)]
    );
}

#[test]
fn a_desynced_subscriber_is_told_the_session_quit() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    bus.publish(&Event::Quit);

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit)]
    );
}

#[test]
fn a_last_frame_that_does_not_fit_a_desynced_queue_is_counted() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));

    // The queue is still full, so the restart does not fit either: the count
    // holds the event that desynced the subscriber plus this one.
    bus.publish(&Event::Restarting);

    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    let frame = snapshot();
    assert!(bus.try_resync(id, frame.clone()));
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            snapshot: frame,
            lagged: SubscriberLagged {
                subscriber_id: id,
                dropped_count: 2,
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
fn unsubscribing_a_desynced_subscriber_clears_it_from_the_desynced_list() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, _rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);

    bus.unsubscribe(id);

    assert!(!bus.contains(id));
    assert_eq!(bus.desynced(), Vec::new());
    assert!(!bus.has_desynced());
    assert_eq!(bus.subscriber_count(), 0);
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

#[test]
fn a_frame_lands_on_a_live_subscribers_queue() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    let frame = snapshot();

    assert!(bus.try_send_frame(id, frame.clone()));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(frame)]
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_frame_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_frame(SubscriberId::new(), snapshot()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_frame_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    assert!(!bus.try_send_frame(id, snapshot()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_frame_that_does_not_fit_is_refused_and_leaves_the_subscriber_live() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    assert!(!bus.try_send_frame(id, snapshot()));

    // A refused frame is not a gap in the stream: the next frame supersedes it,
    // so the subscriber keeps receiving instead of pausing for a snapshot.
    assert!(!bus.has_desynced());
    assert_eq!(bus.desynced(), Vec::new());
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }));
            SUBSCRIBER_QUEUE_CAPACITY
        ]
    );

    let frame = snapshot();
    assert!(bus.try_send_frame(id, frame.clone()));
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(frame)]
    );
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_frame() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    drop(rx);

    assert!(!bus.try_send_frame(id, snapshot()));

    assert!(!bus.contains(id));
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn an_answer_lands_on_a_live_subscribers_queue() {
    let pane = PaneId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_answer(
        id,
        9,
        vec![
            MouseAnswer::Scrolled {
                pane,
                top: Some(41),
            },
            MouseAnswer::Resized {
                pane,
                side: Direction::Up,
                step: -1,
                applied: 3,
            },
        ]
    ));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::MouseAnswer {
            request_id: 9,
            answers: vec![
                MouseAnswer::Scrolled {
                    pane,
                    top: Some(41),
                },
                MouseAnswer::Resized {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    applied: 3,
                },
            ],
        }]
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_round_with_nothing_to_report_lands_as_an_empty_list() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_answer(id, 1, Vec::new()));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::MouseAnswer {
            request_id: 1,
            answers: Vec::new(),
        }]
    );
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn an_answer_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_answer(SubscriberId::new(), 4, Vec::new()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn an_answer_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    assert!(!bus.try_send_answer(id, 5, Vec::new()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn an_answer_that_does_not_fit_desyncs_the_subscriber_and_a_resync_follows() {
    // A lost answer leaves the viewer's drag anchor where it was, so it may not
    // pass silently: the desync it causes is what puts a fresh frame on the
    // queue.
    let tab = TabId::new();
    let pane = PaneId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    assert!(!bus.try_send_answer(
        id,
        7,
        vec![MouseAnswer::Scrolled {
            pane,
            top: Some(12),
        }]
    ));

    assert_eq!(bus.desynced(), vec![id]);

    // The event published next is withheld, not delivered on top of the gap,
    // and counted: 1 for the lost answer plus 1 for it.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    let frame = snapshot();
    assert!(bus.try_resync(id, frame.clone()));

    let queued: Vec<_> = rx.try_iter().collect();
    assert_eq!(
        queued,
        vec![Delivery::Snapshot {
            snapshot: frame,
            lagged: SubscriberLagged {
                subscriber_id: id,
                dropped_count: 2,
                event_class: EventClass::Critical,
            },
        }]
    );
    assert_eq!(
        wire_event(&queued[0]),
        Some(SessionEvent::Resync { dropped_count: 2 })
    );
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_answer() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    drop(rx);

    assert!(!bus.try_send_answer(id, 2, Vec::new()));

    assert!(!bus.contains(id));
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn a_host_write_reaches_the_subscriber_as_the_bytes_it_queued() {
    // An OSC 52 copy of "hello", the sequence a clipboard write queues.
    let bytes = b"\x1b]52;c;aGVsbG8=\x07".to_vec();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_host_write(id, bytes.clone()));

    let queued: Vec<Delivery> = rx.try_iter().collect();
    assert_eq!(queued, vec![Delivery::HostWrite(bytes.clone())]);
    assert_eq!(
        wire_event(&queued[0]),
        Some(SessionEvent::HostWrite { bytes })
    );
    assert_eq!(bus.desynced(), Vec::new());
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_full_queue_desyncs_the_subscriber_and_drops_the_host_write() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    assert!(!bus.try_send_host_write(id, b"\x1b]52;c;aGVsbG8=\x07".to_vec()));

    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(bus.subscriber_count(), 1);
    // The backlog that filled the queue, and nothing else: the bytes are gone,
    // and the desync is what puts a fresh frame on the queue.
    assert_eq!(
        rx.try_iter().collect::<Vec<Delivery>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }));
            SUBSCRIBER_QUEUE_CAPACITY
        ]
    );
}

#[test]
fn an_empty_host_write_reaches_the_subscriber_as_empty_bytes() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_host_write(id, Vec::new()));

    let queued: Vec<Delivery> = rx.try_iter().collect();
    assert_eq!(queued, vec![Delivery::HostWrite(Vec::new())]);
    assert_eq!(
        wire_event(&queued[0]),
        Some(SessionEvent::HostWrite { bytes: Vec::new() })
    );
}

#[test]
fn a_host_write_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_host_write(SubscriberId::new(), b"\x07".to_vec()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn a_host_write_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    assert!(!bus.try_send_host_write(id, b"\x1b]52;c;aGVsbG8=\x07".to_vec()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_host_write() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    drop(rx);

    assert!(!bus.try_send_host_write(id, b"\x1b]52;c;aGVsbG8=\x07".to_vec()));

    assert!(!bus.contains(id));
    assert_eq!(bus.subscriber_count(), 0);
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn a_switch_reaches_the_subscriber_as_the_session_it_names() {
    let session = SessionId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_switch(id, session));

    let queued: Vec<Delivery> = rx.try_iter().collect();
    assert_eq!(queued, vec![Delivery::SwitchTo(session)]);
    assert_eq!(
        wire_event(&queued[0]),
        Some(SessionEvent::SwitchTo {
            session_id: session
        })
    );
    assert_eq!(bus.desynced(), Vec::new());
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_full_queue_desyncs_the_subscriber_and_drops_the_switch() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);

    assert!(!bus.try_send_switch(id, SessionId::new()));

    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(bus.subscriber_count(), 1);
    // The backlog that filled the queue, and nothing else: the switch is gone,
    // and the desync is what puts a fresh frame on the queue.
    assert_eq!(
        rx.try_iter().collect::<Vec<Delivery>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id: tab }));
            SUBSCRIBER_QUEUE_CAPACITY
        ]
    );
}

#[test]
fn a_switch_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_switch(SubscriberId::new(), SessionId::new()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn a_switch_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab = TabId::new();
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id: tab }));
    assert_eq!(bus.desynced(), vec![id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(rx.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);

    assert!(!bus.try_send_switch(id, SessionId::new()));

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(bus.desynced(), vec![id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_switch() {
    let mut bus = EventBus::new();
    let (id, rx) = bus.subscribe(EventFilter::All);
    drop(rx);

    assert!(!bus.try_send_switch(id, SessionId::new()));

    assert!(!bus.contains(id));
    assert_eq!(bus.subscriber_count(), 0);
    assert_eq!(bus.desynced(), Vec::new());
}

#[test]
fn the_wire_filter_converts_to_the_bus_filter() {
    assert_eq!(EventFilter::from(EventFilterSpec::All), EventFilter::All);
}

#[test]
fn every_structure_event_converts_to_its_wire_frame() {
    let client = ClientId::new();
    let pane = PaneId::new();
    let other_pane = PaneId::new();
    let tab = TabId::new();
    let other_tab = TabId::new();

    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneCreated(PaneCreated {
            pane_id: pane,
            tab_id: tab,
        }))),
        Some(SessionEvent::PaneCreated {
            pane_id: pane,
            tab_id: tab,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneProcessExited(
            PaneProcessExited {
                pane_id: pane,
                exit_code: Some(130),
            }
        ))),
        Some(SessionEvent::PaneProcessExited {
            pane_id: pane,
            exit_code: Some(130),
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneClosing(PaneClosing {
            pane_id: pane,
        }))),
        Some(SessionEvent::PaneClosing { pane_id: pane })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneRemoved(PaneRemoved {
            pane_id: pane,
            tab_id: tab,
        }))),
        Some(SessionEvent::PaneRemoved {
            pane_id: pane,
            tab_id: tab,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneFocused(PaneFocused {
            client_id: client,
            tab_id: tab,
            pane_id: pane,
            prior_pane: Some(other_pane),
        }))),
        Some(SessionEvent::PaneFocused {
            client_id: client,
            tab_id: tab,
            pane_id: pane,
            prior_pane: Some(other_pane),
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::LayoutChanged(LayoutChanged {
            tab_id: tab,
        }))),
        Some(SessionEvent::LayoutChanged { tab_id: tab })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabCreated(TabCreated {
            tab_id: tab
        }))),
        Some(SessionEvent::TabCreated { tab_id: tab })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabClosed(TabClosed {
            tab_id: tab
        }))),
        Some(SessionEvent::TabClosed { tab_id: tab })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabFocused(TabFocused {
            client_id: client,
            tab_id: tab,
            prior_tab: other_tab,
        }))),
        Some(SessionEvent::TabFocused {
            client_id: client,
            tab_id: tab,
            prior_tab: other_tab,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabMoved(TabMoved {
            tab_id: tab,
            old_index: 2,
            new_index: 0,
        }))),
        Some(SessionEvent::TabMoved {
            tab_id: tab,
            old_index: 2,
            new_index: 0,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::Quit)),
        Some(SessionEvent::Quit)
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::Restarting)),
        Some(SessionEvent::Restarting)
    );
}

#[test]
fn an_absent_optional_field_stays_absent_on_the_wire() {
    let client = ClientId::new();
    let pane = PaneId::new();
    let tab = TabId::new();

    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneProcessExited(
            PaneProcessExited {
                pane_id: pane,
                exit_code: None,
            }
        ))),
        Some(SessionEvent::PaneProcessExited {
            pane_id: pane,
            exit_code: None,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneFocused(PaneFocused {
            client_id: client,
            tab_id: tab,
            pane_id: pane,
            prior_pane: None,
        }))),
        Some(SessionEvent::PaneFocused {
            client_id: client,
            tab_id: tab,
            pane_id: pane,
            prior_pane: None,
        })
    );
}

#[test]
fn every_event_with_no_wire_spelling_converts_to_nothing() {
    let client = ClientId::new();
    let pane = PaneId::new();
    let tab = TabId::new();
    let session = SessionId::new();
    let position = Point { x: 3, y: 4 };
    let now = SystemTime::UNIX_EPOCH;

    let refused = vec![
        Event::PtyResized(PtyResized {
            pane_id: pane,
            size: PtySize { cols: 80, rows: 24 },
        }),
        Event::PaneOutputUpdated(PaneOutputUpdated { pane_id: pane }),
        Event::PaneSuppressed(PaneSuppressed {
            pane_id: pane,
            tab_id: tab,
        }),
        Event::PaneResumed(PaneResumed {
            pane_id: pane,
            tab_id: tab,
        }),
        Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
            client_id: client,
            size: Size { cols: 4, rows: 2 },
            pane_area: Some(PaneArea::Reported(Size { cols: 4, rows: 0 })),
            cause: TerminalTooSmallCause::Terminal,
        }),
        Event::TerminalTooSmallExited(TerminalTooSmallExited {
            client_id: client,
            size: Size { cols: 80, rows: 24 },
        }),
        Event::ConfigReloaded(ConfigReloaded {
            session_id: session,
        }),
        Event::InputModeChanged(InputModeChanged {
            client_id: client,
            mode: LockMode::Locked,
        }),
        Event::MouseSelectChanged(MouseSelectChanged {
            client_id: client,
            on: true,
        }),
        Event::KeybindingMatched(KeybindingMatched {
            client_id: client,
            command_id: CommandId::new(),
        }),
        Event::PaneTyped(PaneTyped {
            pane_id: pane,
            tab_id: tab,
            session_id: session,
            client_id: client,
            payload: TypedPayload::SafePublic('k'),
            timestamp: now,
        }),
        Event::PaneEnterPressed(PaneEnterPressed {
            pane_id: pane,
            tab_id: tab,
            session_id: session,
            client_id: client,
            line: SubmittedLinePayload::SafePublic("ls -l".to_string()),
            timestamp: now,
        }),
        Event::MousePressed(MousePressed {
            client_id: client,
            pane: Some(pane),
            position,
            button: MouseButton::Left,
        }),
        Event::MouseReleased(MouseReleased {
            client_id: client,
            pane: Some(pane),
            position,
            button: MouseButton::Left,
        }),
        Event::MouseDragged(MouseDragged {
            client_id: client,
            pane: Some(pane),
            position,
            button: MouseButton::Left,
        }),
        Event::MouseScrolled(MouseScrolled {
            client_id: client,
            pane: Some(pane),
            position,
            direction: ScrollDirection::Up,
        }),
        Event::PaneMouseForwarded(PaneMouseForwarded { pane_id: pane }),
        Event::PluginMouseInput(PluginMouseInput {
            plugin_id: PluginId::new(),
        }),
        Event::PaneCommandStarted(PaneCommandStarted { pane_id: pane }),
        Event::PaneCommandFinished(PaneCommandFinished {
            pane_id: pane,
            exit_code: Some(0),
        }),
        Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
            pane_id: pane,
            dropped_lines: 12,
            dropped_bytes: 340,
        }),
        Event::SubscriberLagged(SubscriberLagged {
            subscriber_id: SubscriberId::new(),
            dropped_count: 7,
            event_class: EventClass::Critical,
        }),
        Event::CommandRejected(CommandRejected {
            id: CommandId::new(),
            reason: RejectReason::TargetGone,
        }),
        Event::SelectionChanged(SelectionChanged {
            client_id: client,
            pane_id: pane,
            selection: None,
        }),
        Event::Copied(Copied {
            client_id: client,
            pane_id: pane,
            target: CopyTarget::Osc52,
            byte_len: 11,
        }),
        Event::Plugin(PluginEvent::Installed(PluginInstalled {
            plugin_id: PluginId::new(),
        })),
    ];

    for event in refused {
        let name = event.name();
        assert_eq!(
            wire_event(&Delivery::Event(event)),
            None,
            "{name} reached the wire"
        );
    }
}

#[test]
fn a_frame_converts_to_the_painted_picture() {
    let frame = snapshot();

    assert_eq!(
        wire_event(&Delivery::Frame(frame.clone())),
        Some(SessionEvent::Painted {
            frame: Box::new(wire_frame(&frame)),
        })
    );
}

#[test]
fn a_snapshot_converts_to_a_resync_carrying_the_dropped_count() {
    assert_eq!(
        wire_event(&Delivery::Snapshot {
            snapshot: snapshot(),
            lagged: SubscriberLagged {
                subscriber_id: SubscriberId::new(),
                dropped_count: 4,
                event_class: EventClass::Critical,
            },
        }),
        Some(SessionEvent::Resync { dropped_count: 4 })
    );
}

#[test]
fn a_round_of_answers_converts_to_the_wire_round_it_answers() {
    let pane = PaneId::new();

    assert_eq!(
        wire_event(&Delivery::MouseAnswer {
            request_id: 12,
            answers: vec![
                MouseAnswer::Scrolled { pane, top: None },
                MouseAnswer::Resized {
                    pane,
                    side: Direction::Left,
                    step: 1,
                    applied: 5,
                },
            ],
        }),
        Some(SessionEvent::MouseAnswer {
            request_id: 12,
            answers: vec![
                MouseAnswer::Scrolled { pane, top: None },
                MouseAnswer::Resized {
                    pane,
                    side: Direction::Left,
                    step: 1,
                    applied: 5,
                },
            ],
        })
    );
    assert_eq!(
        wire_event(&Delivery::MouseAnswer {
            request_id: 13,
            answers: Vec::new(),
        }),
        Some(SessionEvent::MouseAnswer {
            request_id: 13,
            answers: Vec::new(),
        })
    );
}
