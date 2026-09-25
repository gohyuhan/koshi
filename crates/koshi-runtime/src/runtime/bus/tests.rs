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

use koshi_core::command::{CopyTarget, PanePlacementAnchor, PanePlacementTarget};
use koshi_core::event::{
    CommandRejected, ConfigReloaded, Copied, Event, InputModeChanged, KeybindingMatched,
    LayoutChanged, MouseDragged, MousePressed, MouseReleased, MouseScrolled, MouseSelectChanged,
    PaneClosing, PaneCommandFinished, PaneCommandStarted, PaneCreated, PaneEnterPressed,
    PaneFocused, PaneMouseForwarded, PaneOutputUpdated, PanePlacementCommitted, PaneProcessExited,
    PaneRemoved, PaneResumed, PaneScrollbackTruncated, PaneSuppressed, PaneTyped, PluginEvent,
    PluginInstalled, PluginMouseInput, PtyResized, QuitCause, RejectReason, SelectionChanged,
    SubmittedLinePayload, TabClosed, TabCreated, TabFocused, TabMoved, TerminalTooSmallCause,
    TerminalTooSmallEntered, TerminalTooSmallExited, TypedPayload,
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
fn build_test_render_snapshot() -> Box<RenderSnapshot> {
    let tab_id = TabId::new();
    Box::new(RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                effective_cell_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id: tab_id,
            focused_pane_id: None,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    })
}

/// Fill every subscriber queue to capacity with `TabCreated` events for `tab`.
fn fill_to_capacity(bus: &mut EventBus, tab_id: TabId) {
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(&Event::TabCreated(TabCreated { tab_id }));
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
    assert_eq!(bus.ending_notice().get_session_ending(), None);
}

#[test]
fn the_default_filter_is_every_event() {
    assert_eq!(EventFilter::default(), EventFilter::All);
}

#[test]
fn publishing_to_a_bus_with_no_subscribers_removes_nobody() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();

    let removed_subscriber_ids = bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert_eq!(removed_subscriber_ids, Vec::new());
    assert_eq!(bus.subscriber_count(), 0);
    assert!(!bus.has_desynced_subscribers());
}

#[test]
fn subscribers_receive_distinct_ids() {
    let mut bus = EventBus::new();
    let (first_subscriber_id, _first_receiver) = bus.subscribe(EventFilter::All);
    let (second_subscriber_id, _second_receiver) = bus.subscribe(EventFilter::All);

    assert_ne!(first_subscriber_id, second_subscriber_id);
    assert_eq!(bus.subscribers[0].subscriber_id, first_subscriber_id);
    assert_eq!(bus.subscribers[1].subscriber_id, second_subscriber_id);
}

#[test]
fn a_subscriber_receives_published_events_in_order() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (_subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id })),
            Delivery::Event(Event::LayoutChanged(LayoutChanged { tab_id })),
        ]
    );
}

#[test]
fn every_subscriber_receives_its_own_copy() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (_first_subscriber_id, first_receiver) = bus.subscribe(EventFilter::All);
    let (_second_subscriber_id, second_receiver) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert_eq!(
        first_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
    assert_eq!(
        second_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
}

#[test]
fn a_dropped_receiver_is_removed_on_the_next_publish() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (_keep_subscriber_id, keep_receiver) = bus.subscribe(EventFilter::All);
    let (dropped_subscriber_id, dropped_receiver) = bus.subscribe(EventFilter::All);
    drop(dropped_receiver);
    assert_eq!(bus.subscriber_count(), 2);

    let removed_subscriber_ids = bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert_eq!(removed_subscriber_ids, vec![dropped_subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        keep_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );

    let removed_subscriber_ids = bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert_eq!(removed_subscriber_ids, Vec::new());
}

#[test]
fn every_dropped_receiver_is_returned_in_subscription_order() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (first_subscriber_id, first_receiver) = bus.subscribe(EventFilter::All);
    let (keep_subscriber_id, keep_receiver) = bus.subscribe(EventFilter::All);
    let (third_subscriber_id, third_receiver) = bus.subscribe(EventFilter::All);
    drop(first_receiver);
    drop(third_receiver);

    let removed_subscriber_ids = bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert_eq!(
        removed_subscriber_ids,
        vec![first_subscriber_id, third_subscriber_id]
    );
    assert!(bus.has_subscriber(keep_subscriber_id));
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        keep_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
}

#[test]
fn a_lossy_event_that_does_not_fit_is_dropped_and_the_subscriber_stays_live() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    bus.publish(&Event::PaneOutputUpdated(PaneOutputUpdated { pane_id }));

    assert!(!bus.has_desynced_subscribers());
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());

    // The queue holds exactly the earlier events; the overflowing one is gone.
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id })); SUBSCRIBER_QUEUE_CAPACITY]
    );

    // Delivery never paused: the next event lands on the drained queue.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::LayoutChanged(LayoutChanged {
            tab_id
        }))]
    );
    assert!(bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_critical_event_that_does_not_fit_desyncs_that_subscriber_only() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut bus = EventBus::new();
    let (full_subscriber_id, full_subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    let (fresh_subscriber_id, fresh_subscriber_receiver) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));

    assert!(bus.has_desynced_subscribers());
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![full_subscriber_id]);

    // The fresh subscriber is untouched and still receiving.
    assert_eq!(
        fresh_subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::LayoutChanged(LayoutChanged {
            tab_id
        }))]
    );
    assert!(bus.has_subscriber(fresh_subscriber_id));

    // Draining the desynced queue does not resume it: nothing further arrives,
    // critical or lossy, until a snapshot lands.
    assert_eq!(
        full_subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id })); SUBSCRIBER_QUEUE_CAPACITY]
    );
    bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    bus.publish(&Event::PaneOutputUpdated(PaneOutputUpdated { pane_id }));
    assert_eq!(
        full_subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![full_subscriber_id]);
    assert_eq!(bus.subscriber_count(), 2);
}

#[test]
fn a_resync_onto_a_still_full_queue_fails_and_leaves_the_subscriber_desynced() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);

    assert!(!bus.try_resync(subscriber_id, build_test_render_snapshot()));

    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    // Nothing was queued: the backlog is still exactly the pre-gap events.
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id })); SUBSCRIBER_QUEUE_CAPACITY]
    );
}

#[test]
fn a_resync_queues_the_snapshot_behind_the_backlog_and_ahead_of_live_events() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));

    // Leave two pre-gap events on the queue so the ordering is visible.
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY - 2 {
        assert_eq!(
            subscriber_receiver.recv().unwrap(),
            Delivery::Event(Event::TabCreated(TabCreated { tab_id }))
        );
    }

    let render_snapshot = build_test_render_snapshot();
    assert!(bus.try_resync(subscriber_id, render_snapshot.clone()));
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::TabCreated(TabCreated { tab_id })),
            Delivery::Event(Event::TabCreated(TabCreated { tab_id })),
            Delivery::Snapshot {
                render_snapshot,
                lag_report: SubscriberLagged {
                    subscriber_id,
                    dropped_event_count: 1,
                    event_class: EventClass::Critical,
                },
            },
            Delivery::Event(Event::LayoutChanged(LayoutChanged { tab_id })),
        ]
    );
}

#[test]
fn the_dropped_count_holds_the_trigger_plus_withheld_critical_events_only() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    // The trigger, then three withheld critical events and two withheld lossy
    // ones: 1 + 3 = 4.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    for _ in 0..3 {
        bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    }
    for _ in 0..2 {
        bus.publish(&Event::PaneOutputUpdated(PaneOutputUpdated { pane_id }));
    }

    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    let render_snapshot = build_test_render_snapshot();
    assert!(bus.try_resync(subscriber_id, render_snapshot.clone()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot,
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 4,
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
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    bus.publish(&Event::Restarting);

    assert_eq!(
        bus.ending_notice().get_session_ending(),
        Some(SessionEnding::Restarting)
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    let queued_deliveries = subscriber_receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(queued_deliveries.len(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(
        queued_deliveries
            .iter()
            .all(|delivery| *delivery == Delivery::Event(Event::TabCreated(TabCreated { tab_id }))),
        "the queue must hold its backlog and nothing else"
    );
}

#[test]
fn publishing_the_quit_raises_the_ending_notice_and_delivers_it() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    let removed_subscriber_ids = bus.publish(&Event::Quit(QuitCause::Requested));

    assert_eq!(removed_subscriber_ids, Vec::new());
    assert_eq!(
        bus.ending_notice().get_session_ending(),
        Some(SessionEnding::Quit)
    );
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
    assert!(bus.has_subscriber(subscriber_id));
}

#[test]
fn the_ending_notice_keeps_the_first_ending_it_was_raised_with() {
    let mut bus = EventBus::new();
    let (_subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::Restarting);
    bus.publish(&Event::Quit(QuitCause::Requested));

    assert_eq!(
        bus.ending_notice().get_session_ending(),
        Some(SessionEnding::Restarting)
    );
    // Both events still ride the queue; only the notice is set once.
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::Event(Event::Restarting),
            Delivery::Event(Event::Quit(QuitCause::Requested)),
        ]
    );
}

#[test]
fn a_desynced_subscriber_is_told_the_session_is_restarting() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    // The client drains its backlog, so the queue has room again while the
    // subscriber is still awaiting its snapshot.
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    bus.publish(&Event::Restarting);

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Restarting)]
    );
}

#[test]
fn a_desynced_subscriber_is_told_the_session_quit() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    bus.publish(&Event::Quit(QuitCause::Requested));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn a_last_frame_that_does_not_fit_a_desynced_queue_is_counted() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));

    // The queue is still full, so the restart does not fit either: the count
    // holds the event that desynced the subscriber plus this one.
    bus.publish(&Event::Restarting);

    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    let render_snapshot = build_test_render_snapshot();
    assert!(bus.try_resync(subscriber_id, render_snapshot.clone()));
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot,
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 2,
                event_class: EventClass::Critical,
            },
        }]
    );
}

#[test]
fn a_desynced_subscriber_whose_receiver_is_gone_is_removed_by_the_resync() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    drop(subscriber_receiver);

    assert!(!bus.try_resync(subscriber_id, build_test_render_snapshot()));

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 0);
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn a_resync_of_a_live_or_unknown_subscriber_does_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert!(!bus.try_resync(subscriber_id, build_test_render_snapshot()));
    assert!(!bus.try_resync(SubscriberId::new(), build_test_render_snapshot()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_second_desync_counts_from_one_again() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    // First gap: the trigger plus two more withheld critical events.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    for _ in 0..2 {
        bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    }
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    assert!(bus.try_resync(subscriber_id, build_test_render_snapshot()));
    assert_eq!(subscriber_receiver.try_iter().count(), 1);

    // Second gap, on a queue refilled from scratch: the count restarts.
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    let render_snapshot = build_test_render_snapshot();
    assert!(bus.try_resync(subscriber_id, render_snapshot.clone()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot,
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 1,
                event_class: EventClass::Critical,
            },
        }]
    );
}

#[test]
fn a_resync_of_a_just_resynced_subscriber_does_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
    assert!(bus.try_resync(subscriber_id, build_test_render_snapshot()));
    assert_eq!(subscriber_receiver.try_iter().count(), 1);

    assert!(!bus.try_resync(subscriber_id, build_test_render_snapshot()));

    // No second frame was queued, and the subscriber is still live.
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
    bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
}

#[test]
fn every_desynced_subscriber_is_listed_in_subscription_order() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (first_subscriber_id, first_receiver) = bus.subscribe(EventFilter::All);
    let (second_subscriber_id, second_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    let (live_subscriber_id, _live_receiver) = bus.subscribe(EventFilter::All);

    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));

    assert_eq!(
        bus.list_desynced_subscriber_ids(),
        vec![first_subscriber_id, second_subscriber_id]
    );
    assert!(bus.has_subscriber(live_subscriber_id));
    assert_eq!(bus.subscriber_count(), 3);

    // Resyncing the first leaves the second listed, still in order.
    assert_eq!(first_receiver.try_iter().count(), SUBSCRIBER_QUEUE_CAPACITY);
    assert!(bus.try_resync(first_subscriber_id, build_test_render_snapshot()));
    assert_eq!(
        bus.list_desynced_subscriber_ids(),
        vec![second_subscriber_id]
    );
    assert_eq!(
        second_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );
}

#[test]
fn a_desynced_subscriber_whose_receiver_is_gone_survives_every_publish() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    drop(subscriber_receiver);

    // Withholding means no send, so no publish ever learns the receiver is
    // gone: the resync is the only thing that reaps it.
    let removed_subscriber_ids = bus.publish(&Event::TabCreated(TabCreated { tab_id }));

    assert_eq!(removed_subscriber_ids, Vec::new());
    assert!(bus.has_subscriber(subscriber_id));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);

    assert!(!bus.try_resync(subscriber_id, build_test_render_snapshot()));

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn unsubscribing_an_unknown_id_changes_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    bus.unsubscribe(SubscriberId::new());

    assert!(bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 1);
    bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
}

#[test]
fn unsubscribing_a_desynced_subscriber_clears_it_from_the_desynced_list() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, _subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);

    bus.unsubscribe(subscriber_id);

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
    assert!(!bus.has_desynced_subscribers());
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn unsubscribe_removes_that_subscriber_only() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (gone_subscriber_id, gone_receiver) = bus.subscribe(EventFilter::All);
    let (keep_subscriber_id, keep_receiver) = bus.subscribe(EventFilter::All);

    bus.unsubscribe(gone_subscriber_id);

    assert!(!bus.has_subscriber(gone_subscriber_id));
    assert!(bus.has_subscriber(keep_subscriber_id));
    assert_eq!(bus.subscriber_count(), 1);

    bus.publish(&Event::TabCreated(TabCreated { tab_id }));
    assert_eq!(
        gone_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(
        keep_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id }))]
    );
}

#[test]
fn a_frame_lands_on_a_live_subscribers_queue() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    let render_snapshot = build_test_render_snapshot();

    assert!(bus.try_send_frame(subscriber_id, render_snapshot.clone()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(render_snapshot)]
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_frame_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_frame(SubscriberId::new(), build_test_render_snapshot()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_frame_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    assert!(!bus.try_send_frame(subscriber_id, build_test_render_snapshot()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_frame_that_does_not_fit_is_refused_and_leaves_the_subscriber_live() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    assert!(!bus.try_send_frame(subscriber_id, build_test_render_snapshot()));

    // A refused frame is not a gap in the stream: the next frame supersedes it,
    // so the subscriber keeps receiving instead of pausing for a snapshot.
    assert!(!bus.has_desynced_subscribers());
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id })); SUBSCRIBER_QUEUE_CAPACITY]
    );

    let render_snapshot = build_test_render_snapshot();
    assert!(bus.try_send_frame(subscriber_id, render_snapshot.clone()));
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(render_snapshot)]
    );
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_frame() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    drop(subscriber_receiver);

    assert!(!bus.try_send_frame(subscriber_id, build_test_render_snapshot()));

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn an_answer_lands_on_a_live_subscribers_queue() {
    let pane_id = PaneId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_answer(
        subscriber_id,
        9,
        vec![
            MouseAnswer::Scrolled {
                pane_id,
                top_row_number: Some(41),
            },
            MouseAnswer::Resized {
                pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                applied_cell_count: 3,
            },
        ]
    ));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::MouseAnswer {
            request_id: 9,
            mouse_answers: vec![
                MouseAnswer::Scrolled {
                    pane_id,
                    top_row_number: Some(41),
                },
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    applied_cell_count: 3,
                },
            ],
        }]
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_round_with_nothing_to_report_lands_as_an_empty_list() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_answer(subscriber_id, 1, Vec::new()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::MouseAnswer {
            request_id: 1,
            mouse_answers: Vec::new(),
        }]
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn an_answer_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_answer(SubscriberId::new(), 4, Vec::new()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn an_answer_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    assert!(!bus.try_send_answer(subscriber_id, 5, Vec::new()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn an_answer_that_does_not_fit_desyncs_the_subscriber_and_a_resync_follows() {
    // A lost answer leaves the viewer's drag anchor where it was, so it may not
    // pass silently: the desync it causes is what puts a fresh frame on the
    // queue.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    assert!(!bus.try_send_answer(
        subscriber_id,
        7,
        vec![MouseAnswer::Scrolled {
            pane_id,
            top_row_number: Some(12),
        }]
    ));

    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);

    // The event published next is withheld, not delivered on top of the gap,
    // and counted: 1 for the lost answer plus 1 for it.
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    let render_snapshot = build_test_render_snapshot();
    assert!(bus.try_resync(subscriber_id, render_snapshot.clone()));

    let queued_deliveries: Vec<_> = subscriber_receiver.try_iter().collect();
    assert_eq!(
        queued_deliveries,
        vec![Delivery::Snapshot {
            render_snapshot,
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 2,
                event_class: EventClass::Critical,
            },
        }]
    );
    assert_eq!(
        wire_event(&queued_deliveries[0]),
        Some(SessionEvent::Resync {
            dropped_event_count: 2
        })
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_answer() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    drop(subscriber_receiver);

    assert!(!bus.try_send_answer(subscriber_id, 2, Vec::new()));

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 0);
}

#[test]
fn a_host_write_reaches_the_subscriber_as_the_bytes_it_queued() {
    // An OSC 52 copy of "hello", the sequence a clipboard write queues.
    let clipboard_write_bytes = b"\x1b]52;c;aGVsbG8=\x07".to_vec();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_host_write(subscriber_id, clipboard_write_bytes.clone()));

    let queued_deliveries: Vec<Delivery> = subscriber_receiver.try_iter().collect();
    assert_eq!(
        queued_deliveries,
        vec![Delivery::HostWrite(clipboard_write_bytes.clone())]
    );
    assert_eq!(
        wire_event(&queued_deliveries[0]),
        Some(SessionEvent::HostWrite {
            host_output_bytes: clipboard_write_bytes,
        })
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_full_queue_desyncs_the_subscriber_and_drops_the_host_write() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    assert!(!bus.try_send_host_write(subscriber_id, b"\x1b]52;c;aGVsbG8=\x07".to_vec()));

    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
    // The backlog that filled the queue, and nothing else: the bytes are gone,
    // and the desync is what puts a fresh frame on the queue.
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<Delivery>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id })); SUBSCRIBER_QUEUE_CAPACITY]
    );
}

#[test]
fn an_empty_host_write_reaches_the_subscriber_as_empty_bytes() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_host_write(subscriber_id, Vec::new()));

    let queued_deliveries: Vec<Delivery> = subscriber_receiver.try_iter().collect();
    assert_eq!(queued_deliveries, vec![Delivery::HostWrite(Vec::new())]);
    assert_eq!(
        wire_event(&queued_deliveries[0]),
        Some(SessionEvent::HostWrite {
            host_output_bytes: Vec::new(),
        })
    );
}

#[test]
fn a_host_write_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_host_write(SubscriberId::new(), b"\x07".to_vec()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn a_host_write_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    assert!(!bus.try_send_host_write(subscriber_id, b"\x1b]52;c;aGVsbG8=\x07".to_vec()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_host_write() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    drop(subscriber_receiver);

    assert!(!bus.try_send_host_write(subscriber_id, b"\x1b]52;c;aGVsbG8=\x07".to_vec()));

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 0);
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn a_switch_reaches_the_subscriber_as_the_session_it_names() {
    let session_id = SessionId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_switch(subscriber_id, session_id));

    let queued_deliveries: Vec<Delivery> = subscriber_receiver.try_iter().collect();
    assert_eq!(queued_deliveries, vec![Delivery::SwitchTo(session_id)]);
    assert_eq!(
        wire_event(&queued_deliveries[0]),
        Some(SessionEvent::SwitchTo { session_id })
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_rejected_placement_command_reaches_the_subscriber_with_its_command_id() {
    let command_id = CommandId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(bus.try_send_placement_command_rejection(subscriber_id, command_id));

    let queued_delivery = subscriber_receiver
        .try_recv()
        .expect("the placement rejection reaches its subscriber");
    assert_eq!(
        queued_delivery,
        Delivery::PlacementCommandRejected(command_id)
    );
    assert_eq!(
        wire_event(&queued_delivery),
        Some(SessionEvent::PlacementCommandRejected { command_id })
    );
}

#[test]
fn a_full_queue_desyncs_the_subscriber_and_drops_the_switch() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);

    assert!(!bus.try_send_switch(subscriber_id, SessionId::new()));

    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
    // The backlog that filled the queue, and nothing else: the switch is gone,
    // and the desync is what puts a fresh frame on the queue.
    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<Delivery>>(),
        vec![Delivery::Event(Event::TabCreated(TabCreated { tab_id })); SUBSCRIBER_QUEUE_CAPACITY]
    );
}

#[test]
fn a_switch_for_an_unknown_subscriber_is_refused() {
    let mut bus = EventBus::new();
    let (_subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);

    assert!(!bus.try_send_switch(SubscriberId::new(), SessionId::new()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.subscriber_count(), 1);
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn a_switch_for_a_desynced_subscriber_is_refused_and_queues_nothing() {
    let tab_id = TabId::new();
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    fill_to_capacity(&mut bus, tab_id);
    bus.publish(&Event::LayoutChanged(LayoutChanged { tab_id }));
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    // Drained, so a refusal here cannot be a full queue.
    assert_eq!(
        subscriber_receiver.try_iter().count(),
        SUBSCRIBER_QUEUE_CAPACITY
    );

    assert!(!bus.try_send_switch(subscriber_id, SessionId::new()));

    assert_eq!(
        subscriber_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(bus.list_desynced_subscriber_ids(), vec![subscriber_id]);
    assert_eq!(bus.subscriber_count(), 1);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_is_removed_by_the_switch() {
    let mut bus = EventBus::new();
    let (subscriber_id, subscriber_receiver) = bus.subscribe(EventFilter::All);
    drop(subscriber_receiver);

    assert!(!bus.try_send_switch(subscriber_id, SessionId::new()));

    assert!(!bus.has_subscriber(subscriber_id));
    assert_eq!(bus.subscriber_count(), 0);
    assert_eq!(bus.list_desynced_subscriber_ids(), Vec::new());
}

#[test]
fn the_wire_filter_converts_to_the_bus_filter() {
    assert_eq!(EventFilter::from(EventFilterSpec::All), EventFilter::All);
}

#[test]
fn every_structure_event_converts_to_its_wire_frame() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let tab_id = TabId::new();
    let other_tab_id = TabId::new();

    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneCreated(PaneCreated {
            pane_id,
            tab_id,
        }))),
        Some(SessionEvent::PaneCreated { pane_id, tab_id })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneProcessExited(
            PaneProcessExited {
                pane_id,
                exit_code: Some(130),
                signal: None,
            }
        ))),
        Some(SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: Some(130),
            signal: None,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneClosing(PaneClosing {
            pane_id,
        }))),
        Some(SessionEvent::PaneClosing { pane_id })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneRemoved(PaneRemoved {
            pane_id,
            tab_id,
        }))),
        Some(SessionEvent::PaneRemoved { pane_id, tab_id })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: Some(other_pane_id),
        }))),
        Some(SessionEvent::PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: Some(other_pane_id),
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::LayoutChanged(LayoutChanged {
            tab_id,
        }))),
        Some(SessionEvent::LayoutChanged { tab_id })
    );
    let placement_command_id = CommandId::new();
    let placement_target = PanePlacementTarget::Split {
        destination_tab_id: other_tab_id,
        anchor: PanePlacementAnchor::Pane(other_pane_id),
        direction: Direction::Down,
    };
    assert_eq!(
        wire_event(&Delivery::Event(Event::PanePlacementCommitted(
            PanePlacementCommitted {
                command_id: placement_command_id,
                source_pane_id: pane_id,
                source_tab_id: tab_id,
                destination_tab_id: other_tab_id,
                placement_target: placement_target.clone(),
            }
        ))),
        Some(SessionEvent::PanePlacementCommitted {
            command_id: placement_command_id,
            source_pane_id: pane_id,
            source_tab_id: tab_id,
            destination_tab_id: other_tab_id,
            placement_target,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabCreated(TabCreated { tab_id }))),
        Some(SessionEvent::TabCreated { tab_id })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabClosed(TabClosed { tab_id }))),
        Some(SessionEvent::TabClosed { tab_id })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabFocused(TabFocused {
            client_id,
            tab_id,
            previous_tab_id: other_tab_id,
        }))),
        Some(SessionEvent::TabFocused {
            client_id,
            tab_id,
            previous_tab_id: other_tab_id,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::TabMoved(TabMoved {
            tab_id,
            previous_tab_index: 2,
            new_tab_index: 0,
        }))),
        Some(SessionEvent::TabMoved {
            tab_id,
            previous_tab_index: 2,
            new_tab_index: 0,
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::Quit(QuitCause::Requested))),
        Some(SessionEvent::Quit)
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::Restarting)),
        Some(SessionEvent::Restarting)
    );
}

#[test]
fn an_absent_optional_field_stays_absent_on_the_wire() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();

    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneProcessExited(
            PaneProcessExited {
                pane_id,
                exit_code: None,
                signal: Some(9),
            }
        ))),
        Some(SessionEvent::PaneProcessExited {
            pane_id,
            exit_code: None,
            signal: Some(9),
        })
    );
    assert_eq!(
        wire_event(&Delivery::Event(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: None,
        }))),
        Some(SessionEvent::PaneFocused {
            client_id,
            tab_id,
            pane_id,
            previous_pane_id: None,
        })
    );
}

#[test]
fn every_event_with_no_wire_spelling_converts_to_nothing() {
    let client_id = ClientId::new();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let session_id = SessionId::new();
    let mouse_position = Point { column: 3, row: 4 };
    let accepted_at = SystemTime::UNIX_EPOCH;

    let non_wire_events = vec![
        Event::PtyResized(PtyResized {
            pane_id,
            pty_size: PtySize {
                column_count: 80,
                row_count: 24,
            },
        }),
        Event::PaneOutputUpdated(PaneOutputUpdated { pane_id }),
        Event::PaneSuppressed(PaneSuppressed { pane_id, tab_id }),
        Event::PaneResumed(PaneResumed { pane_id, tab_id }),
        Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
            client_id,
            viewport_size: Size {
                column_count: 4,
                row_count: 2,
            },
            pane_area: Some(PaneArea::Reported(Size {
                column_count: 4,
                row_count: 0,
            })),
            cause: TerminalTooSmallCause::Terminal,
        }),
        Event::TerminalTooSmallExited(TerminalTooSmallExited {
            client_id,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
        }),
        Event::ConfigReloaded(ConfigReloaded { session_id }),
        Event::InputModeChanged(InputModeChanged {
            client_id,
            lock_mode: LockMode::Locked,
        }),
        Event::MouseSelectChanged(MouseSelectChanged {
            client_id,
            is_enabled: true,
        }),
        Event::KeybindingMatched(KeybindingMatched {
            client_id,
            command_id: CommandId::new(),
        }),
        Event::PaneTyped(PaneTyped {
            pane_id,
            tab_id,
            session_id,
            client_id,
            typed_payload: TypedPayload::SafePublic('k'),
            accepted_at,
        }),
        Event::PaneEnterPressed(PaneEnterPressed {
            pane_id,
            tab_id,
            session_id,
            client_id,
            submitted_line: SubmittedLinePayload::SafePublic("ls -l".to_string()),
            accepted_at,
        }),
        Event::MousePressed(MousePressed {
            client_id,
            pane_id: Some(pane_id),
            position: mouse_position,
            button: MouseButton::Left,
        }),
        Event::MouseReleased(MouseReleased {
            client_id,
            pane_id: Some(pane_id),
            position: mouse_position,
            button: MouseButton::Left,
        }),
        Event::MouseDragged(MouseDragged {
            client_id,
            pane_id: Some(pane_id),
            position: mouse_position,
            button: MouseButton::Left,
        }),
        Event::MouseScrolled(MouseScrolled {
            client_id,
            pane_id: Some(pane_id),
            position: mouse_position,
            direction: ScrollDirection::Up,
        }),
        Event::PaneMouseForwarded(PaneMouseForwarded { pane_id }),
        Event::PluginMouseInput(PluginMouseInput {
            plugin_id: PluginId::new(),
        }),
        Event::PaneCommandStarted(PaneCommandStarted { pane_id }),
        Event::PaneCommandFinished(PaneCommandFinished {
            pane_id,
            exit_code: Some(0),
        }),
        Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
            pane_id,
            dropped_lines: 12,
            dropped_bytes: 340,
        }),
        Event::SubscriberLagged(SubscriberLagged {
            subscriber_id: SubscriberId::new(),
            dropped_event_count: 7,
            event_class: EventClass::Critical,
        }),
        Event::CommandRejected(CommandRejected {
            command_id: CommandId::new(),
            rejection_reason: RejectReason::TargetGone,
        }),
        Event::SelectionChanged(SelectionChanged {
            client_id,
            pane_id,
            selection: None,
        }),
        Event::Copied(Copied {
            client_id,
            pane_id,
            clipboard_target: CopyTarget::Osc52,
            byte_count: 11,
        }),
        Event::Plugin(PluginEvent::Installed(PluginInstalled {
            plugin_id: PluginId::new(),
        })),
    ];

    for runtime_event in non_wire_events {
        let event_name = runtime_event.get_event_name();
        assert_eq!(
            wire_event(&Delivery::Event(runtime_event)),
            None,
            "{event_name} reached the wire"
        );
    }
}

#[test]
fn a_frame_converts_to_the_painted_picture() {
    let render_snapshot = build_test_render_snapshot();

    assert_eq!(
        wire_event(&Delivery::Frame(render_snapshot.clone())),
        Some(SessionEvent::Painted {
            frame: Box::new(wire_frame(&render_snapshot)),
        })
    );
}

#[test]
fn a_snapshot_converts_to_a_resync_carrying_the_dropped_count() {
    assert_eq!(
        wire_event(&Delivery::Snapshot {
            render_snapshot: build_test_render_snapshot(),
            lag_report: SubscriberLagged {
                subscriber_id: SubscriberId::new(),
                dropped_event_count: 4,
                event_class: EventClass::Critical,
            },
        }),
        Some(SessionEvent::Resync {
            dropped_event_count: 4
        })
    );
}

#[test]
fn a_round_of_answers_converts_to_the_wire_round_it_answers() {
    let pane_id = PaneId::new();

    assert_eq!(
        wire_event(&Delivery::MouseAnswer {
            request_id: 12,
            mouse_answers: vec![
                MouseAnswer::Scrolled {
                    pane_id,
                    top_row_number: None,
                },
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Left,
                    resize_step: 1,
                    applied_cell_count: 5,
                },
            ],
        }),
        Some(SessionEvent::MouseAnswer {
            request_id: 12,
            mouse_answers: vec![
                MouseAnswer::Scrolled {
                    pane_id,
                    top_row_number: None,
                },
                MouseAnswer::Resized {
                    pane_id,
                    border_side: Direction::Left,
                    resize_step: 1,
                    applied_cell_count: 5,
                },
            ],
        })
    );
    assert_eq!(
        wire_event(&Delivery::MouseAnswer {
            request_id: 13,
            mouse_answers: Vec::new(),
        }),
        Some(SessionEvent::MouseAnswer {
            request_id: 13,
            mouse_answers: Vec::new(),
        })
    );
}
