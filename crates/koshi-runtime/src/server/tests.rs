//! Tests for the server half: construction defaults, the held service
//! handles, the wired event inbox, a session with one tab and one pane, and
//! the two doors — commands in via `submit_command`, events out via
//! `subscribe` — including the identity every attached client carries, that
//! detaching a client leaves the server healthy with its panes alive, and that
//! a subscriber paused by a dropped critical event is handed a fresh frame or
//! dropped.

use std::sync::mpsc;
use std::time::SystemTime;

use koshi_core::command::{Command, CommandSource, ToggleLockModeArgs};
use koshi_core::event::{EventClass, InputMode, InputModeChanged, SubscriberLagged};
use koshi_core::ids::{CommandId, TabId};
use koshi_core::process::PtySize;
use koshi_pane::pane::state::PaneRecord;
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::{AuthorityTier, ClientOrigin, ClientRegistry};
use koshi_session::session::state::Tab;
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;
use crate::placeholder::{NullSnapshotProvider, NullStorage};

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A server bootstrapped with one session, one tab, and one shell pane, plus
/// its client id.
fn booted_server() -> (Server, ClientId) {
    let (mut server, _tx) = new_server();
    let client_id = server
        .bootstrap_local(SessionId::new(), VIEWPORT, SystemTime::now())
        .expect("bootstrap");
    (server, client_id)
}

/// Publish critical events until every subscriber's queue overflows and pauses
/// it, so exactly one event was dropped for each.
fn pause_subscribers(server: &mut Server) {
    while !server.event_bus.has_desynced() {
        server.event_bus.publish(&Event::Quit);
    }
}

fn new_server() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let server = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
    );
    (server, tx)
}

#[test]
fn a_new_server_starts_with_no_sessions_or_engines() {
    let (rt, _tx) = new_server();

    assert!(rt.sessions().is_empty());
    assert!(rt.terminal_engines().is_empty());
    assert!(rt.ipc_server().is_none());
}

#[test]
fn accessors_return_the_constructed_services() {
    let (rt, _tx) = new_server();

    assert_eq!(Arc::strong_count(rt.pty_backend()), 1);
    assert_eq!(Arc::strong_count(rt.snapshot_provider()), 1);
    assert_eq!(Arc::strong_count(rt.storage()), 1);
    let _ = rt.event_bus();
}

#[test]
fn inbox_delivers_events_to_the_receiver() {
    let (rt, tx) = new_server();

    tx.send(RuntimeEvent::Timer).expect("send to inbox");

    assert!(matches!(rt.inbox_rx().try_recv(), Ok(RuntimeEvent::Timer)));
}

#[test]
fn holds_one_session_with_one_tab_and_pane() {
    let (mut rt, _tx) = new_server();

    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let mut session = Session::new(
        session_id,
        "main".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::now()))
        .expect("pane registers");
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "shell".to_string(), 0, pane_id));

    rt.sessions.insert(session_id, session);
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 80, rows: 24 }));

    assert_eq!(rt.sessions().len(), 1);
    let session = rt.sessions().get(&session_id).expect("session present");
    assert_eq!(session.id, session_id);

    assert_eq!(session.tabs.len(), 1);
    assert_eq!(session.tabs.get(&tab_id).expect("tab present").id(), tab_id);

    assert_eq!(session.panes.len(), 1);
    assert_eq!(
        session.panes.get(pane_id).expect("pane present").id(),
        pane_id
    );

    assert_eq!(rt.terminal_engines().len(), 1);
    assert!(rt.terminal_engines().contains_key(&pane_id));
}

#[test]
fn a_fresh_server_has_no_draining_or_quit_flags_set() {
    let (rt, _tx) = new_server();

    assert!(!rt.is_draining());
    assert!(!rt.quit_requested());
}

#[test]
fn every_attached_client_is_a_local_admin_with_its_own_generated_label() {
    let (mut server, first) = booted_server();
    let session_id = *server.sessions().keys().next().expect("session");
    let active_tab = server.sessions()[&session_id]
        .clients
        .get(first)
        .expect("client record")
        .active_tab();

    let second = ClientId::new();
    let _ =
        server.handle_client_attach(session_id, second, VIEWPORT, active_tab, SystemTime::now());

    let clients = &server.sessions()[&session_id].clients;
    let bootstrapped = clients.get(first).expect("bootstrapped client");
    let attached = clients.get(second).expect("attached client");

    assert_eq!(bootstrapped.origin(), ClientOrigin::Local);
    assert_eq!(bootstrapped.tier(), AuthorityTier::Admin);
    assert_eq!(attached.origin(), ClientOrigin::Local);
    assert_eq!(attached.tier(), AuthorityTier::Admin);

    // Both labels are generated as `C-<adjective>-<noun>`, and the attaching
    // client never takes the label the bootstrapped one already holds.
    for label in [bootstrapped.label(), attached.label()] {
        let pieces: Vec<&str> = label.split('-').collect();
        assert_eq!(pieces.len(), 3, "not C-<adjective>-<noun>: {label}");
        assert_eq!(pieces[0], "C");
    }
    assert_ne!(bootstrapped.label(), attached.label());
}

#[test]
fn detaching_a_client_leaves_the_server_healthy_with_panes_alive() {
    let (mut server, first) = booted_server();
    let session_id = *server.sessions().keys().next().expect("session");
    let active_tab = server.sessions()[&session_id]
        .clients
        .get(first)
        .expect("client record")
        .active_tab();

    // A second client attaches, then detaches again.
    let second = ClientId::new();
    let events =
        server.handle_client_attach(session_id, second, VIEWPORT, active_tab, SystemTime::now());
    assert_eq!(events, Vec::new(), "same-size attach reflows nothing");
    let _ = server.handle_client_detach(second);

    // The server still holds the session, its pane, and its engine; the
    // remaining client still renders.
    assert_eq!(server.sessions().len(), 1);
    assert_eq!(server.sessions()[&session_id].panes.len(), 1);
    assert!(server.has_active_panes());
    assert_eq!(server.terminal_engines().len(), 1);
    assert!(server.build_snapshot(first).is_some());
    assert!(server.build_snapshot(second).is_none());

    // Even the first client detaching removes only the view: the session and
    // its pane live on.
    let _ = server.handle_client_detach(first);
    assert_eq!(server.sessions().len(), 1);
    assert_eq!(server.sessions()[&session_id].panes.len(), 1);
    assert!(server.has_active_panes());
}

#[test]
fn submit_command_dispatches_against_live_state() {
    let (mut server, client_id) = booted_server();

    let command_id = CommandId::new();
    let result = server.submit_command(CommandEnvelope::new(
        command_id,
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    match result {
        CommandResult::Ok {
            command_id: applied,
            emitted_events,
        } => {
            assert_eq!(applied, command_id);
            assert_eq!(emitted_events.len(), 1, "the toggle emits one event");
        }
        CommandResult::Rejected { .. } => panic!("toggle-lock must apply, never reject"),
    }
    assert_eq!(
        server
            .sessions()
            .values()
            .next()
            .expect("session")
            .clients
            .get(client_id)
            .expect("client record")
            .lock_mode(),
        koshi_core::lock::LockMode::Locked
    );
}

#[test]
fn a_subscriber_receives_the_events_a_command_emits() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);

    let _ = server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::InputModeChanged(InputModeChanged {
            client_id,
            mode: InputMode::Locked,
        }))]
    );
}

#[test]
fn publish_events_delivers_out_of_command_events_to_subscribers() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);
    let events = vec![Event::Quit];

    server.publish_events(&events);

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit)]
    );
}

#[test]
fn subscribing_records_which_client_the_subscriber_views() {
    let (mut server, client_id) = booted_server();

    let _rx = server.subscribe(client_id, EventFilter::All);

    assert_eq!(server.subscriptions.len(), 1);
    assert_eq!(server.subscriptions[0].1, client_id);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_loses_its_recorded_client_too() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    drop(rx);

    server.publish_events(&[Event::Quit]);

    assert!(!server.event_bus.contains(subscriber_id));
    assert_eq!(server.subscriptions, Vec::new());
}

#[test]
fn resyncing_hands_a_paused_subscriber_a_frame_of_the_client_it_views() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(server.event_bus.desynced(), vec![subscriber_id]);
    // Make room, so the frame fits on the next pass. The pre-gap backlog's
    // contents are the bus's own concern.
    let _backlog: Vec<Delivery> = rx.try_iter().collect();
    let expected = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged();

    assert_eq!(server.event_bus.desynced(), Vec::new());
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            snapshot: Box::new(expected),
            lagged: SubscriberLagged {
                subscriber_id,
                dropped_count: 1,
                event_class: EventClass::Critical,
            },
        }]
    );
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
}

#[test]
fn resyncing_with_nobody_paused_delivers_nothing() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];

    server.resync_lagged();

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    assert!(server.event_bus.contains(subscriber_id));
}

#[test]
fn a_resync_blocked_by_a_full_queue_retries_with_a_newer_frame() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);

    // The queue is still full, so the frame does not fit and the subscriber
    // stays paused.
    server.resync_lagged();
    assert_eq!(server.event_bus.desynced(), vec![subscriber_id]);

    // Change the state the frame reports, then make room.
    let _ = server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));
    let _backlog: Vec<Delivery> = rx.try_iter().collect();
    let expected = server.build_snapshot(client_id).expect("frame");
    assert_eq!(
        expected.client.lock_mode,
        koshi_core::lock::LockMode::Locked
    );

    server.resync_lagged();

    // The retry built a new frame: it carries the mode set after the first
    // attempt failed, not the one that was current then. The count covers the
    // event that triggered the pause plus the withheld mode change.
    assert_eq!(server.event_bus.desynced(), Vec::new());
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            snapshot: Box::new(expected),
            lagged: SubscriberLagged {
                subscriber_id,
                dropped_count: 2,
                event_class: EventClass::Critical,
            },
        }]
    );
}

#[test]
fn one_unresyncable_subscriber_does_not_block_the_others_frame() {
    let (mut server, client_id) = booted_server();
    let good = server.subscribe(client_id, EventFilter::All);
    let (good_id, _) = server.subscriptions[0];
    // Straight off the bus, so nothing records which client it views.
    let (orphan_id, orphan) = server.event_bus.subscribe(EventFilter::All);
    pause_subscribers(&mut server);
    assert_eq!(server.event_bus.desynced(), vec![good_id, orphan_id]);
    let _good_backlog: Vec<Delivery> = good.try_iter().collect();
    let _orphan_backlog: Vec<Delivery> = orphan.try_iter().collect();
    let expected = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged();

    assert_eq!(
        good.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            snapshot: Box::new(expected),
            lagged: SubscriberLagged {
                subscriber_id: good_id,
                dropped_count: 1,
                event_class: EventClass::Critical,
            },
        }]
    );
    assert!(!server.event_bus.contains(orphan_id));
    assert_eq!(
        orphan.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(server.subscriptions, vec![(good_id, client_id)]);
}

#[test]
fn a_gone_receiver_costs_only_its_own_recorded_client() {
    let (mut server, client_id) = booted_server();
    let keep = server.subscribe(client_id, EventFilter::All);
    let gone = server.subscribe(client_id, EventFilter::All);
    let (keep_id, _) = server.subscriptions[0];
    let (gone_id, _) = server.subscriptions[1];
    drop(gone);

    server.publish_events(&[Event::Quit]);

    assert!(!server.event_bus.contains(gone_id));
    assert!(server.event_bus.contains(keep_id));
    assert_eq!(server.subscriptions, vec![(keep_id, client_id)]);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit)]
    );
}

#[test]
fn a_paused_subscriber_that_views_no_client_is_unsubscribed() {
    let (mut server, _client_id) = booted_server();
    // Straight off the bus, so nothing records which client it views.
    let (subscriber_id, rx) = server.event_bus.subscribe(EventFilter::All);
    pause_subscribers(&mut server);
    assert_eq!(server.event_bus.desynced(), vec![subscriber_id]);
    let _backlog: Vec<Delivery> = rx.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.subscriber_count(), 0);
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
}

#[test]
fn a_paused_subscriber_whose_client_is_gone_is_unsubscribed() {
    let (mut server, _client_id) = booted_server();
    // No session holds this id, so no frame can ever be built for it.
    let rx = server.subscribe(ClientId::new(), EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(server.event_bus.desynced(), vec![subscriber_id]);
    let _backlog: Vec<Delivery> = rx.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.subscriber_count(), 0);
    assert_eq!(rx.try_iter().collect::<Vec<_>>(), Vec::<Delivery>::new());
    assert_eq!(server.subscriptions, Vec::new());
}

#[test]
fn constructor_starts_on_an_empty_app_layer_and_the_built_in_defaults() {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();

    let rt = Server::new(pty_backend, snapshot_provider, storage, inbox_rx, tx);

    // Nothing is read from disk here, so the constructor holds no settings of
    // its own: `load_startup_config` puts the first real `koshi.kdl` in.
    assert_eq!(rt.app_layer, PartialKoshiConfig::default());
    assert_eq!(rt.config, ServerConfig::default());
    assert_eq!(rt.client_config, ClientConfig::default());
}
