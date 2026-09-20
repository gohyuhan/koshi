//! Tests for the server half.
//!
//! Construction defaults, the held service handles, and the wired event inbox.
//! A session with one tab and one pane. The two doors — commands in via
//! `submit_command`, events out via `subscribe` — with the identity every
//! attached client carries, and a detach that leaves the server healthy with
//! its panes alive. Frame delivery: a subscriber paused by a dropped critical
//! event is handed a fresh frame or dropped, and a due render hands every
//! client its own frame behind any bytes queued for that client's own
//! terminal. The restart checks, an accepted or refused restart request, and
//! the announcements that wait for every client to hold the session's last
//! frame. The state that crosses an image swap: what `carry_out` writes and
//! what `resume` puts back. The view a client leaves behind, taken back by the
//! token the next attach presents.

use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use koshi_core::command::{Command, CommandSource, NewPaneArgs, ToggleLockModeArgs};
use koshi_core::event::{EventClass, InputModeChanged, PaneFocused, PtyResized, SubscriberLagged};
use koshi_core::geometry::{Direction, PaneArea};
use koshi_core::ids::{CommandId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;
use koshi_ipc::protocol::ConnectionToken;
use koshi_layout::mode::LayoutMode;
use koshi_pane::pane::state::PaneRecord;
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::{ClientOrigin, ClientRegistry};
use koshi_session::session::state::Tab;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;
use crate::runtime::event::{AttachAccepted, SessionEnding};
use crate::runtime::saved_view::SavedView;

const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};
/// The viewport of a second, out-of-process client, sized apart from
/// [`TEST_VIEWPORT_SIZE`] so a frame names which client it was built for.
const REMOTE_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 30,
};

/// A server bootstrapped with one session, one tab, and one shell pane, plus
/// its client id.
fn booted_server() -> (Server, ClientId) {
    let (mut server, _event_sender) = build_test_server_with_event_sender();
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
        .expect("bootstrap");
    (server, client_id)
}

/// Publish critical events until every subscriber's queue overflows and pauses
/// it, so exactly one event was dropped for each.
fn pause_subscribers(server: &mut Server) {
    while !server.event_bus.has_desynced_subscribers() {
        server.event_bus.publish(&Event::Quit(QuitCause::Requested));
    }
}

/// Attach an additional client at `viewport`, viewing the tab `reference_client_id` views, and
/// hand back its id.
fn attach_additional_client(
    server: &mut Server,
    reference_client_id: ClientId,
    viewport_size: Size,
) -> ClientId {
    let session_id = *server.list_sessions().keys().next().expect("session");
    let active_tab_id = server.list_sessions()[&session_id]
        .clients
        .get_client_by_id(reference_client_id)
        .expect("client record")
        .get_active_tab();
    let attached_client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        attached_client_id,
        viewport_size,
        None,
        active_tab_id,
        SystemTime::now(),
        false,
    );
    attached_client_id
}

fn build_test_server_with_event_sender() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (event_sender, event_receiver) = mpsc::channel();
    let server = Server::from_runtime_parts(pty_backend, event_receiver, event_sender.clone());
    (server, event_sender)
}

#[test]
fn a_new_server_starts_with_no_sessions_or_engines() {
    let (server, _event_sender) = build_test_server_with_event_sender();

    assert!(server.list_sessions().is_empty());
    assert!(server.list_terminal_engines().is_empty());
    assert!(server.ipc_server().is_none());
}

#[test]
fn accessors_return_the_constructed_services() {
    let (server, _event_sender) = build_test_server_with_event_sender();

    assert_eq!(Arc::strong_count(server.get_pty_backend()), 1);
    assert_eq!(server.event_bus().subscriber_count(), 0);
}

#[test]
fn inbox_delivers_events_to_the_receiver() {
    let (server, event_sender) = build_test_server_with_event_sender();

    event_sender
        .send(RuntimeEvent::Timer)
        .expect("send to inbox");

    assert!(matches!(
        server.inbox_rx().try_recv(),
        Ok(RuntimeEvent::Timer)
    ));
}

#[test]
fn holds_one_session_with_one_tab_and_pane() {
    let (mut server, _tx) = build_test_server_with_event_sender();

    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let mut session = Session::from_identity_and_client_registry(
        session_id,
        "main".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id, SystemTime::now()))
        .expect("pane registers");
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "shell".to_string(), 0, pane_id),
    );

    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );

    assert_eq!(server.list_sessions().len(), 1);
    let session = server
        .list_sessions()
        .get(&session_id)
        .expect("session present");
    assert_eq!(session.session_id, session_id);

    assert_eq!(session.tabs.len(), 1);
    assert_eq!(
        session.tabs.get(&tab_id).expect("tab present").get_tab_id(),
        tab_id
    );

    assert_eq!(session.panes.pane_record_count(), 1);
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane_id)
            .expect("pane present")
            .get_pane_id(),
        pane_id
    );

    assert_eq!(server.list_terminal_engines().len(), 1);
    assert!(server.list_terminal_engines().contains_key(&pane_id));
}

#[test]
fn a_fresh_server_has_no_draining_or_quit_flags_set() {
    let (server, _event_sender) = build_test_server_with_event_sender();

    assert!(!server.is_draining());
    assert!(!server.is_quit_requested());
}

#[test]
fn every_attached_client_is_local_with_its_own_generated_label() {
    let (mut server, bootstrapped_client_id) = booted_server();
    let session_id = *server.list_sessions().keys().next().expect("session");
    let active_tab_id = server.list_sessions()[&session_id]
        .clients
        .get_client_by_id(bootstrapped_client_id)
        .expect("client record")
        .get_active_tab();

    let attached_client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        attached_client_id,
        TEST_VIEWPORT_SIZE,
        None,
        active_tab_id,
        SystemTime::now(),
        false,
    );

    let clients = &server.list_sessions()[&session_id].clients;
    let bootstrapped_client = clients
        .get_client_by_id(bootstrapped_client_id)
        .expect("bootstrapped client");
    let attached_client = clients
        .get_client_by_id(attached_client_id)
        .expect("attached client");

    assert_eq!(bootstrapped_client.get_origin(), ClientOrigin::Local);
    assert_eq!(attached_client.get_origin(), ClientOrigin::Local);

    // Both labels are generated as `C-<adjective>-<noun>`, and the attaching
    // client never takes the label the bootstrapped one already holds.
    for client_label in [bootstrapped_client.get_label(), attached_client.get_label()] {
        let label_parts: Vec<&str> = client_label.split('-').collect();
        assert_eq!(
            label_parts.len(),
            3,
            "not C-<adjective>-<noun>: {client_label}"
        );
        assert_eq!(label_parts[0], "C");
    }
    assert_ne!(bootstrapped_client.get_label(), attached_client.get_label());
}

#[test]
fn detaching_a_client_leaves_the_server_healthy_with_panes_alive() {
    let (mut server, bootstrapped_client_id) = booted_server();
    let session_id = *server.list_sessions().keys().next().expect("session");
    let active_tab_id = server.list_sessions()[&session_id]
        .clients
        .get_client_by_id(bootstrapped_client_id)
        .expect("client record")
        .get_active_tab();

    // A second client attaches, then detaches again.
    let attached_client_id = ClientId::new();
    let emitted_events = server.handle_client_attach(
        session_id,
        attached_client_id,
        TEST_VIEWPORT_SIZE,
        None,
        active_tab_id,
        SystemTime::now(),
        false,
    );
    // Same size, so nothing reflows; the joining client still lands on the
    // tab's pane, which is the one event a same-size attach carries.
    let focused_pane_id = server.list_sessions()[&session_id].tabs[&active_tab_id]
        .get_layout_tree()
        .list_leaf_pane_ids()
        .first()
        .copied()
        .expect("the tab holds one pane");
    assert_eq!(
        emitted_events,
        vec![Event::PaneFocused(PaneFocused {
            client_id: attached_client_id,
            tab_id: active_tab_id,
            pane_id: focused_pane_id,
            previous_pane_id: None,
        })],
        "same-size attach reflows nothing"
    );
    let _ = server.handle_client_detach(attached_client_id);

    // The server still holds the session, its pane, and its engine; the
    // remaining client still renders.
    assert_eq!(server.list_sessions().len(), 1);
    assert_eq!(
        server.list_sessions()[&session_id]
            .panes
            .pane_record_count(),
        1
    );
    assert!(server.has_active_panes());
    assert_eq!(server.list_terminal_engines().len(), 1);
    assert_eq!(
        server
            .build_snapshot(bootstrapped_client_id)
            .expect("frame")
            .client_snapshot
            .client_id,
        bootstrapped_client_id
    );
    assert!(server.build_snapshot(attached_client_id).is_none());

    // Even the first client detaching removes only the view: the session and
    // its pane live on.
    let _ = server.handle_client_detach(bootstrapped_client_id);
    assert_eq!(server.list_sessions().len(), 1);
    assert_eq!(
        server.list_sessions()[&session_id]
            .panes
            .pane_record_count(),
        1
    );
    assert!(server.has_active_panes());
}

#[test]
fn submit_command_dispatches_against_live_state() {
    let (mut server, client_id) = booted_server();

    let command_id = CommandId::new();
    let command_result = server.submit_command(CommandEnvelope::from_parts(
        command_id,
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    match command_result {
        CommandResult::Ok {
            command_id: applied_command_id,
            emitted_events,
        } => {
            assert_eq!(applied_command_id, command_id);
            assert_eq!(emitted_events.len(), 1, "the toggle emits one event");
        }
        CommandResult::Rejected { .. } => panic!("toggle-lock must apply, never reject"),
    }
    assert_eq!(
        server
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .clients
            .get_client_by_id(client_id)
            .expect("client record")
            .get_lock_mode(),
        koshi_core::lock::LockMode::Locked
    );
}

#[test]
fn a_subscriber_receives_the_events_a_command_emits() {
    let (mut server, client_id) = booted_server();
    let event_receiver = server.subscribe(client_id, EventFilter::All);

    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::InputModeChanged(InputModeChanged {
            client_id,
            lock_mode: LockMode::Locked,
        }))]
    );
}

#[test]
fn publish_events_delivers_out_of_command_events_to_subscribers() {
    let (mut server, client_id) = booted_server();
    let event_receiver = server.subscribe(client_id, EventFilter::All);
    let published_events = vec![Event::Quit(QuitCause::Requested)];

    server.publish_events(&published_events);

    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn publish_events_remembers_out_of_command_events_in_the_recent_events_ring() {
    let (mut server, _client_id) = booted_server();
    let tab_id = TabId::new();

    server.publish_events(&[Event::LayoutChanged(koshi_core::event::LayoutChanged {
        tab_id,
    })]);

    // The ring is process-wide and every test in this binary writes to it, so
    // the record is found by this tab's own id rather than by position.
    let recent_event_records = recent_events::list_recent_events();
    let recorded_event = recent_event_records
        .iter()
        .find(|event_record| event_record.tab_id == Some(tab_id))
        .expect("the published event is remembered");
    assert_eq!(recorded_event.event_name, "LayoutChanged");
    assert_eq!(recorded_event.pane_id, None);
}

#[test]
fn subscribing_records_which_client_the_subscriber_views() {
    let (mut server, client_id) = booted_server();

    let _event_receiver = server.subscribe(client_id, EventFilter::All);

    assert_eq!(server.subscriptions.len(), 1);
    assert_eq!(server.subscriptions[0].1, client_id);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_loses_its_recorded_client_too() {
    let (mut server, client_id) = booted_server();
    let event_receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    drop(event_receiver);

    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert_eq!(server.subscriptions, Vec::new());
}

#[test]
fn resyncing_hands_a_paused_subscriber_a_frame_of_the_client_it_views() {
    let (mut server, client_id) = booted_server();
    let event_receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    // Make room, so the frame fits on the next pass. What the queue already
    // holds is the bus's own concern.
    let _delivery_backlog: Vec<Delivery> = event_receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged();

    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 1,
                event_class: EventClass::Critical,
            },
        }]
    );
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
}

/// A paused subscriber whose client is the tab's only viewer and reports
/// [`PaneArea::Starving`] is resynced with a frame carrying every pane
/// suppressed, and keeps its subscription.
#[test]
fn resyncing_a_starving_sole_viewer_keeps_its_subscription() {
    let (mut server, client_id) = booted_server();
    let _ = server.handle_client_resize(client_id, TEST_VIEWPORT_SIZE, Some(PaneArea::Starving));
    let event_receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _delivery_backlog: Vec<Delivery> = event_receiver.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    let delivered_deliveries: Vec<Delivery> = event_receiver.try_iter().collect();
    let [Delivery::Snapshot {
        render_snapshot, ..
    }] = delivered_deliveries.as_slice()
    else {
        panic!("one resync frame, got {delivered_deliveries:?}");
    };
    assert!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed
    );
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 0,
            row_count: 0
        }
    );
}

#[test]
fn resyncing_with_nobody_paused_delivers_nothing() {
    let (mut server, client_id) = booted_server();
    let event_receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];

    server.resync_lagged();

    assert_eq!(
        event_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    assert!(server.event_bus.has_subscriber(subscriber_id));
}

#[test]
fn a_resync_blocked_by_a_full_queue_retries_with_a_newer_frame() {
    let (mut server, client_id) = booted_server();
    let receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);

    // The queue is still full, so the frame does not fit and the subscriber
    // stays paused.
    server.resync_lagged();
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );

    // Change the state the frame reports, then make room.
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));
    let _backlog: Vec<Delivery> = receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    assert_eq!(
        expected_render_snapshot.client_snapshot.lock_mode,
        koshi_core::lock::LockMode::Locked
    );

    server.resync_lagged();

    // The retry built a new frame: it carries the mode set after the first
    // attempt failed, not the one that was current then. The count covers the
    // event that triggered the pause plus the withheld mode change.
    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 2,
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
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![good_id, orphan_id]
    );
    let _good_backlog: Vec<Delivery> = good.try_iter().collect();
    let _orphan_backlog: Vec<Delivery> = orphan.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged();

    assert_eq!(
        good.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id: good_id,
                dropped_event_count: 1,
                event_class: EventClass::Critical,
            },
        }]
    );
    assert!(!server.event_bus.has_subscriber(orphan_id));
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

    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    assert!(!server.event_bus.has_subscriber(gone_id));
    assert!(server.event_bus.has_subscriber(keep_id));
    assert_eq!(server.subscriptions, vec![(keep_id, client_id)]);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn a_paused_subscriber_that_views_no_client_is_unsubscribed() {
    let (mut server, _client_id) = booted_server();
    // Straight off the bus, so nothing records which client it views.
    let (subscriber_id, receiver) = server.event_bus.subscribe(EventFilter::All);
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _backlog: Vec<Delivery> = receiver.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.subscriber_count(), 0);
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_paused_subscriber_whose_client_is_gone_is_unsubscribed() {
    let (mut server, _client_id) = booted_server();
    // No session holds this id, so no frame can ever be built for it.
    let receiver = server.subscribe(ClientId::new(), EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _backlog: Vec<Delivery> = receiver.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.subscriber_count(), 0);
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(server.subscriptions, Vec::new());
}

#[test]
fn pushing_frames_serves_every_client() {
    let (mut server, local) = booted_server();
    let remote = attach_additional_client(&mut server, local, REMOTE_VIEWPORT_SIZE);
    let local_rx = server.subscribe(local, EventFilter::All);
    let remote_rx = server.subscribe(remote, EventFilter::All);
    let local_frame = server.build_snapshot(local).expect("frame");
    let remote_frame = server.build_snapshot(remote).expect("frame");
    assert_eq!(
        local_frame.client_snapshot.viewport_size,
        TEST_VIEWPORT_SIZE
    );
    assert_eq!(
        remote_frame.client_snapshot.viewport_size,
        REMOTE_VIEWPORT_SIZE
    );

    server.push_frames();

    assert_eq!(
        local_rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(local_frame))]
    );
    assert_eq!(
        remote_rx.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(remote_frame))]
    );
}

#[test]
fn a_due_render_hands_a_clients_queued_host_bytes_to_its_subscriber_before_its_frame() {
    let (mut server, client_id) = booted_server();
    let receiver = server.subscribe(client_id, EventFilter::All);
    // An OSC 52 copy of "hello", as `copy_to_clipboard` queues it.
    let queued_host_bytes = b"\x1b]52;c;aGVsbG8=\x07".to_vec();
    server.queue_host_write(client_id, &queued_host_bytes);
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.push_frames();

    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::HostWrite(queued_host_bytes),
            Delivery::Frame(Box::new(expected_render_snapshot)),
        ]
    );
    assert_eq!(server.host_write_bytes_by_client_id.get(&client_id), None);
}

#[test]
fn a_detach_drops_the_bytes_queued_for_that_clients_terminal() {
    let (mut server, local) = booted_server();
    let remote = attach_additional_client(&mut server, local, TEST_VIEWPORT_SIZE);
    server.queue_host_write(remote, b"\x1b]52;c;aGVsbG8=\x07");

    let _ = server.handle_client_detach(remote);

    assert_eq!(server.host_write_bytes_by_client_id.get(&remote), None);
}

#[test]
fn pushing_frames_serves_no_client_that_detached() {
    let (mut server, local) = booted_server();
    let remote = attach_additional_client(&mut server, local, TEST_VIEWPORT_SIZE);
    let remote_rx = server.subscribe(remote, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];

    // The detach takes the subscription with the client record, so the push
    // finds nobody to build a frame for.
    let _ = server.handle_client_detach(remote);
    server.push_frames();

    assert_eq!(server.subscriptions, Vec::new());
    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert_eq!(
        remote_rx.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_frame_for_a_gone_receiver_costs_that_subscription_its_recorded_client() {
    let (mut server, client_id) = booted_server();
    let keep = server.subscribe(client_id, EventFilter::All);
    let gone = server.subscribe(client_id, EventFilter::All);
    let (keep_id, _) = server.subscriptions[0];
    let (gone_id, _) = server.subscriptions[1];
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    drop(gone);

    server.push_frames();

    assert!(!server.event_bus.has_subscriber(gone_id));
    assert!(server.event_bus.has_subscriber(keep_id));
    assert_eq!(server.subscriptions, vec![(keep_id, client_id)]);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected_render_snapshot))]
    );
}

#[test]
fn a_frame_blocked_by_a_full_queue_leaves_the_subscription_in_place() {
    let (mut server, client_id) = booted_server();
    let receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    // Free one slot and spend it on the resync frame: the subscriber is live
    // again, with a queue that is full again.
    let _oldest: Delivery = receiver.recv().expect("queued event");
    server.resync_lagged();
    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());

    server.push_frames();

    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    assert!(server.event_bus.has_subscriber(subscriber_id));
    let delivered: Vec<Delivery> = receiver.try_iter().collect();
    assert!(
        !delivered
            .iter()
            .any(|delivery| matches!(delivery, Delivery::Frame(_))),
        "the frame did not fit, so none was queued"
    );
}

#[test]
fn constructor_starts_on_an_empty_app_layer_and_the_built_in_defaults() {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (tx, inbox_rx) = mpsc::channel();

    let server = Server::from_runtime_parts(pty_backend, inbox_rx, tx);

    // Nothing is read from disk here, so the constructor holds no settings of
    // its own: `load_startup_config` puts the first real `koshi.kdl` in.
    assert_eq!(server.app_layer, PartialKoshiConfig::default());
    assert_eq!(server.config, ServerConfig::default());
    assert_eq!(server.client_config, ClientConfig::default());
}

/// The one session a booted server holds, and the tab and root pane its client
/// is looking at.
fn booted_parts(server: &Server, client_id: ClientId) -> (SessionId, TabId, PaneId) {
    let session_id = *server
        .session_by_id
        .keys()
        .next()
        .expect("the booted session");
    let session = &server.session_by_id[&session_id];
    let tab_id = session
        .clients
        .get_client_by_id(client_id)
        .expect("the booted client")
        .get_active_tab();
    let pane_id = session.tabs[&tab_id]
        .list_focus_mru()
        .first()
        .copied()
        .expect("the tab's root pane");
    (session_id, tab_id, pane_id)
}

/// A second tab in the booted session, holding a pane of its own, so a test can
/// leave a client on a tab that is not the session's first.
fn add_additional_tab(server: &mut Server, session_id: SessionId) -> TabId {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session = server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session");
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            pane_id,
            SystemTime::UNIX_EPOCH,
        ))
        .expect("a fresh pane id");
    let tab_index = session.tabs.len();
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "second".to_string(), tab_index, pane_id),
    );
    tab_id
}

/// A file at `binary_file_path` holding nothing, carrying `file_mode` on Unix.
#[cfg(unix)]
fn write_binary(binary_file_path: &Path, file_mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(binary_file_path, b"").expect("the stand-in binary is written");
    std::fs::set_permissions(binary_file_path, std::fs::Permissions::from_mode(file_mode))
        .expect("the stand-in binary takes the mode asked for");
}

#[test]
fn a_readable_binary_this_machine_could_run_passes_the_binary_check() {
    let binary_directory =
        std::env::temp_dir().join(format!("koshi-restart-ok-{}", std::process::id()));
    std::fs::create_dir_all(&binary_directory).expect("the directory is created");
    let executable_path = binary_directory.join("koshi");
    #[cfg(unix)]
    write_binary(&executable_path, 0o755);
    #[cfg(not(unix))]
    std::fs::write(&executable_path, b"").expect("the stand-in binary is written");

    assert_eq!(is_binary_runnable(&executable_path), Ok(()));

    let _ = std::fs::remove_dir_all(&binary_directory);
}

#[test]
fn a_binary_that_cannot_be_read_fails_the_binary_check_naming_the_path() {
    let binary_directory =
        std::env::temp_dir().join(format!("koshi-restart-gone-{}", std::process::id()));
    std::fs::create_dir_all(&binary_directory).expect("the directory is created");
    let executable_path = binary_directory.join("koshi");
    let metadata_error = std::fs::metadata(&executable_path).expect_err("nothing is at that path");

    assert_eq!(
        is_binary_runnable(&executable_path),
        Err(format!(
            "the binary at {} could not be read: {metadata_error}",
            executable_path.display()
        ))
    );

    let _ = std::fs::remove_dir_all(&binary_directory);
}

// A binary the kernel would refuse to exec must be caught before the swap
// starts, so the session never tears itself down for a restart that cannot run.
#[cfg(unix)]
#[test]
fn a_binary_with_no_execute_bit_fails_the_binary_check_naming_the_path() {
    let binary_directory =
        std::env::temp_dir().join(format!("koshi-restart-noexec-{}", std::process::id()));
    std::fs::create_dir_all(&binary_directory).expect("the directory is created");
    let executable_path = binary_directory.join("koshi");
    write_binary(&executable_path, 0o644);

    assert_eq!(
        is_binary_runnable(&executable_path),
        Err(format!(
            "the binary at {} is not executable",
            executable_path.display()
        ))
    );

    let _ = std::fs::remove_dir_all(&binary_directory);
}

#[cfg(unix)]
#[test]
fn a_pane_whose_terminal_exposes_no_descriptor_fails_the_pane_check_naming_it() {
    let carried_pane_id = PaneId::new();
    let stranded = PaneId::new();
    let panes = [
        CarriedPtyPane {
            pane_id: carried_pane_id,
            terminal_fd: Some(9),
            process_id: 51234,
            pty_size: PtySize {
                column_count: 80,
                row_count: 24,
            },
            exit_status: None,
        },
        CarriedPtyPane {
            pane_id: stranded,
            terminal_fd: None,
            process_id: 51235,
            pty_size: PtySize {
                column_count: 80,
                row_count: 24,
            },
            exit_status: None,
        },
    ];

    assert_eq!(
        can_carry_panes(&panes),
        Err(format!(
            "pane {stranded} has no terminal descriptor, so its terminal cannot cross the swap"
        ))
    );
    assert_eq!(can_carry_panes(&panes[..1]), Ok(()));
    // A session holding no pane holds no restart back either.
    assert_eq!(can_carry_panes(&[]), Ok(()));
}

// Windows keeps every pane's pseudoconsole in the supervisor process, which
// outlives the swap, so the pane check has nothing to refuse there.
#[cfg(windows)]
#[test]
fn no_pane_holds_a_restart_back_on_windows() {
    let panes = [CarriedPtyPane {
        pane_id: PaneId::new(),
        process_id: 51234,
        pty_size: PtySize {
            column_count: 80,
            row_count: 24,
        },
        exit_status: None,
    }];

    assert_eq!(can_carry_panes(&panes), Ok(()));
    assert_eq!(can_carry_panes(&[]), Ok(()));
}

#[test]
fn a_restart_is_refused_while_no_check_is_installed_and_leaves_the_flag_down() {
    let (mut server, _tx) = build_test_server_with_event_sender();

    assert_eq!(
        server.handle_ipc_restart(),
        Err("this koshi cannot replace its own image, so it cannot restart".to_string())
    );
    assert!(!server.is_restart_requested());
}

#[test]
fn a_restart_the_check_refuses_leaves_the_flag_down() {
    let (mut server, _tx) = build_test_server_with_event_sender();
    server.set_restart_check(Arc::new(|| {
        Err("the binary at /x is not executable".to_string())
    }));

    assert_eq!(
        server.handle_ipc_restart(),
        Err("the binary at /x is not executable".to_string())
    );
    assert!(!server.is_restart_requested());
}

#[test]
fn a_restart_the_check_passes_raises_the_flag_and_changes_nothing_else() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.set_restart_check(Arc::new(|| Ok(())));

    assert_eq!(server.handle_ipc_restart(), Ok(()));

    assert!(server.is_restart_requested());
    assert!(!server.is_quit_requested());
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 1);
    assert_eq!(server.pty_handle_by_pane_id.len(), 1);
}

#[test]
fn a_restart_taken_back_lowers_the_flag_and_the_next_one_is_accepted_again() {
    // A swap the session abandoned before anything irreversible happened puts
    // the session back on its feet in this same process, so the event loop must
    // stop asking for the swap and the next restart request must still work.
    let (mut server, _client_id) = booted_server();
    server.set_restart_check(Arc::new(|| Ok(())));
    assert_eq!(server.handle_ipc_restart(), Ok(()));
    assert!(server.is_restart_requested());

    server.cancel_restart();

    assert!(!server.is_restart_requested());
    assert!(!server.is_quit_requested());
    assert_eq!(server.handle_ipc_restart(), Ok(()));
    assert!(server.is_restart_requested());
}

#[test]
fn a_check_installed_again_replaces_the_one_before_it() {
    // The session installs the check again on every server it serves with, so
    // a session put back after a failed swap answers the next restart through
    // the check it was given then, not the one it started with.
    let (mut server, _client_id) = booted_server();
    server.set_restart_check(Arc::new(|| Err("the first check".to_string())));
    assert_eq!(
        server.handle_ipc_restart(),
        Err("the first check".to_string())
    );

    server.set_restart_check(Arc::new(|| Err("the second check".to_string())));

    assert_eq!(
        server.handle_ipc_restart(),
        Err("the second check".to_string())
    );
    assert!(!server.is_restart_requested());
}

#[test]
fn an_attach_claiming_a_carried_client_keeps_its_id_zoom_focus_and_tab() {
    let (mut server, client_id) = booted_server();
    let (session_id, tab_id, pane_id) = booted_parts(&server, client_id);
    let additional_tab_id = add_additional_tab(&mut server, session_id);
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client");
        client.update_focused_pane(tab_id, pane_id);
        client.zoom_pane(tab_id, pane_id);
        client.set_scroll_offset(pane_id, 7);
        // The client was looking at the second tab when the image was replaced.
        client.update_active_tab(additional_tab_id);
    }
    server.client_ids_awaiting_reconnect.insert(client_id);

    let accepted = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the session hands the record back");

    assert_eq!(accepted.client_id, client_id);
    assert_eq!(accepted.session_id, session_id);
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 1);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("the same record");
    assert_eq!(client.get_active_tab(), additional_tab_id);
    assert_eq!(client.get_focused_pane(tab_id), Some(pane_id));
    assert_eq!(client.get_zoomed_pane(tab_id), Some(pane_id));
    assert_eq!(client.get_scroll_offset(pane_id), 7);
    assert_eq!(client.get_viewport_size(), REMOTE_VIEWPORT_SIZE);
}

/// The attach reply hands back the report the session recorded.
#[test]
fn handle_ipc_attach_echoes_the_stored_pane_area() {
    let (mut server, _client_id) = booted_server();

    let starving = server
        .handle_ipc_attach(
            None,
            None,
            REMOTE_VIEWPORT_SIZE,
            Some(PaneArea::Starving),
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the session mints a client");
    assert_eq!(starving.pane_area, Some(PaneArea::Starving));

    let unreported = server
        .handle_ipc_attach(
            None,
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the session mints a client");
    assert_eq!(unreported.pane_area, None);
}

#[test]
fn an_attach_claiming_a_client_this_session_does_not_hold_mints_a_new_one() {
    let (mut server, client_id) = booted_server();
    let (session_id, tab_id, _pane_id) = booted_parts(&server, client_id);
    let stranger = ClientId::new();

    let accepted = server
        .handle_ipc_attach(
            Some(stranger),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the session mints a client instead of refusing");

    assert_ne!(accepted.client_id, stranger);
    assert_ne!(accepted.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 2);
    let minted = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(accepted.client_id)
        .expect("the minted record");
    assert_eq!(minted.get_active_tab(), tab_id);
}

#[test]
fn an_attach_claiming_a_client_a_connection_is_streaming_for_mints_a_new_one() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    // The first attach takes the record and holds its queue, so the record is
    // in use when the second attach names it.
    let held = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            TEST_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the first attach takes the record");
    assert_eq!(held.client_id, client_id);

    let newly_minted_client = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the second attach mints a client instead of refusing");

    assert_ne!(newly_minted_client.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 2);
    // The client already streaming keeps its record and its own subscription:
    // a second caller naming the same id takes neither.
    let viewed: Vec<ClientId> = server
        .subscriptions
        .iter()
        .map(|&(_, client)| client)
        .collect();
    assert_eq!(viewed.len(), 2, "each attach holds one subscription");
    assert_eq!(
        viewed.iter().filter(|&&held| held == client_id).count(),
        1,
        "the claimed record is streamed for by exactly one connection"
    );
    assert_eq!(
        viewed
            .iter()
            .filter(|&&held| held == newly_minted_client.client_id)
            .count(),
        1,
        "and the minted record by exactly one other"
    );
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("the record the first attach took")
            .get_viewport_size(),
        TEST_VIEWPORT_SIZE,
        "the second attach must not move the first client's viewport"
    );
}

#[test]
fn an_attach_naming_no_client_to_come_back_as_mints_one_on_the_first_tab() {
    let (mut server, client_id) = booted_server();
    let (session_id, tab_id, _pane_id) = booted_parts(&server, client_id);

    let accepted = server
        .handle_ipc_attach(
            None,
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the session mints a client");

    assert_ne!(accepted.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 2);
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(accepted.client_id)
            .expect("the minted record")
            .get_active_tab(),
        tab_id
    );
}

/// A second pane in the tab the booted client views, split rightward from that
/// tab's root pane, so zooming one pane changes the size the tab's panes solve
/// to. Returns the new pane's id.
fn split_booted_pane(server: &mut Server, client_id: ClientId, root: PaneId) -> PaneId {
    let session_id = *server
        .session_by_id
        .keys()
        .next()
        .expect("the booted session");
    let command_result = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    ));
    assert!(
        matches!(command_result, CommandResult::Ok { .. }),
        "the split ran, got {command_result:?}"
    );
    let added: Vec<PaneId> = server.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .filter(|&pane_id| pane_id != root)
        .collect();
    assert_eq!(added.len(), 1, "the split added exactly one pane");
    added[0]
}

/// [`add_additional_tab`], also handing back the pane it put in that tab.
fn add_additional_tab_with_pane(server: &mut Server, session_id: SessionId) -> (TabId, PaneId) {
    let existing_pane_ids: HashSet<PaneId> = server.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .collect();
    let tab_id = add_additional_tab(server, session_id);
    let pane_id = server.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .find(|pane_id| !existing_pane_ids.contains(pane_id))
        .expect("the second tab's pane");
    (tab_id, pane_id)
}

/// Attach over `handle_ipc_attach` presenting `resume_token` at `attached_at`,
/// and hand back what the session minted.
fn attach_with_token(
    server: &mut Server,
    resume_token: Option<ConnectionToken>,
    attached_at: SystemTime,
) -> AttachAccepted {
    server
        .handle_ipc_attach(
            None,
            resume_token,
            TEST_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            attached_at,
            false,
        )
        .expect("the session mints a client")
}

#[test]
fn an_attach_presenting_no_token_still_mints_one_and_files_no_view() {
    let (mut server, _client_id) = booted_server();
    let now = SystemTime::now();

    let accepted = attach_with_token(&mut server, None, now);

    assert_eq!(
        accepted.resume_token.expose().len(),
        64,
        "a minted token is 32 random bytes written as hex"
    );
    // Nothing has detached, so the token this attach minted takes back nothing.
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&accepted.resume_token, now),
        None
    );
}

#[test]
fn a_token_takes_back_the_tab_focus_zoom_and_scroll_the_client_left() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root);
    let (additional_tab_id, additional_pane_id) =
        add_additional_tab_with_pane(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    // 600 newlines on a 24-row viewport retain at least 576 lines, so the offset
    // of 500 the view files stands inside the split pane's history.
    server.handle_pty_output(split_pane_id, &b"\n".repeat(600));
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root);
        client.update_focused_pane(additional_tab_id, additional_pane_id);
        client.zoom_pane(booted_tab_id, root);
        client.set_scroll_offset(split_pane_id, 500);
        client.update_active_tab(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);

    let back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    assert_ne!(back.client_id, leaving.client_id);
    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(back.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_active_tab(), additional_tab_id);
    assert_eq!(client.get_focused_pane(booted_tab_id), Some(root));
    assert_eq!(
        client.get_focused_pane(additional_tab_id),
        Some(additional_pane_id)
    );
    assert_eq!(
        client.get_layout_mode(booted_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root
        }
    );
    assert_eq!(client.get_layout_mode(additional_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_scroll_offset(split_pane_id), 500);
}

#[test]
fn a_restored_view_announces_the_focus_it_puts_back() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session");
        session
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave")
            .update_focused_pane(booted_tab_id, root);
        // The tab's most recent focus is the split pane, so an attach that
        // restores nothing lands on `split_pane_id`.
        session
            .tabs
            .get_mut(&booted_tab_id)
            .expect("the tab")
            .record_focus_mru(split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    let watcher = server.subscribe(client_id, EventFilter::All);

    let back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    let focused: Vec<PaneFocused> = watcher
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::Event(Event::PaneFocused(payload)) => Some(payload),
            _ => None,
        })
        .filter(|payload| payload.client_id == back.client_id)
        .collect();
    assert_eq!(
        focused,
        vec![
            PaneFocused {
                client_id: back.client_id,
                tab_id: booted_tab_id,
                pane_id: split_pane_id,
                previous_pane_id: None,
            },
            PaneFocused {
                client_id: back.client_id,
                tab_id: booted_tab_id,
                pane_id: root,
                previous_pane_id: Some(split_pane_id),
            },
        ],
        "the attach lands on the tab's most recent pane, then the restored view moves the focus back"
    );
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(back.client_id)
            .expect("the client the token attached")
            .get_focused_pane(booted_tab_id),
        Some(root)
    );
}

#[test]
fn a_token_whose_pane_lost_its_history_comes_back_at_the_live_bottom() {
    let (mut server, client_id) = booted_server();
    let (session_id, _booted_tab_id, root) = booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    server.handle_pty_output(split_pane_id, &b"\n".repeat(600));
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(leaving.client_id)
        .expect("the client that is about to leave")
        .set_scroll_offset(split_pane_id, 500);
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    // `CSI 3 J` — what `clear` sends when terminfo names `E3` — drops every
    // retained line of the split pane while the view stands.
    server.handle_pty_output(split_pane_id, b"\x1b[3J");
    assert_eq!(
        server.terminal_engine_by_pane_id[&split_pane_id]
            .get_terminal_state()
            .get_scrollback()
            .get_retained_line_count(),
        0
    );

    let back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(back.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_scroll_offset(split_pane_id), 0);
    assert!(!client.is_view_held(split_pane_id));
}

#[test]
fn the_same_token_twice_takes_the_view_back_once() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let additional_tab_id = add_additional_tab(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root);
        client.update_active_tab(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    let token = leaving.resume_token;

    let restored_client = attach_with_token(&mut server, Some(token.clone()), detached_at);
    let newly_minted_client = attach_with_token(&mut server, Some(token), detached_at);

    let clients = &server.session_by_id[&session_id].clients;
    let restored = clients
        .get_client_by_id(restored_client.client_id)
        .expect("the restored client");
    assert_eq!(restored.get_active_tab(), additional_tab_id);
    assert_eq!(
        restored.get_layout_mode(booted_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root
        }
    );
    let plain = clients
        .get_client_by_id(newly_minted_client.client_id)
        .expect("the newly minted client");
    assert_eq!(plain.get_active_tab(), booted_tab_id);
    assert_eq!(plain.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
}

#[test]
fn a_token_presented_121_seconds_after_the_detach_takes_nothing_back() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let additional_tab_id = add_additional_tab(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root);
        client.update_active_tab(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);

    let back = attach_with_token(
        &mut server,
        Some(leaving.resume_token),
        detached_at + Duration::from_secs(121),
    );

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(back.client_id)
        .expect("the minted client");
    assert_eq!(client.get_active_tab(), booted_tab_id);
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
}

#[test]
fn a_view_whose_tab_was_closed_while_it_stood_comes_back_on_the_first_tab() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, _root) = booted_parts(&server, client_id);
    let additional_tab_id = add_additional_tab(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(leaving.client_id)
        .expect("the client that is about to leave")
        .update_active_tab(additional_tab_id);
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .tabs
        .remove(&additional_tab_id);

    let back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(back.client_id)
            .expect("the client the token attached")
            .get_active_tab(),
        booted_tab_id
    );
}

#[test]
fn a_view_whose_zoomed_pane_was_closed_while_it_stood_comes_back_tiled() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root);
        client.zoom_pane(booted_tab_id, split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .panes
        .remove_pane_record(split_pane_id);

    let back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(back.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_focused_pane(booted_tab_id), Some(root));
}

#[test]
fn a_view_whose_focused_pane_was_closed_while_it_stood_comes_back_unfocused_there() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let (additional_tab_id, additional_pane_id) =
        add_additional_tab_with_pane(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root);
        client.update_focused_pane(additional_tab_id, additional_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .panes
        .remove_pane_record(additional_pane_id);

    let back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(back.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_focused_pane(additional_tab_id), None);
    assert_eq!(client.get_focused_pane(booted_tab_id), Some(root));
}

#[test]
fn taking_a_zoom_back_resizes_the_tabs_panes() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let _split = split_booted_pane(&mut server, client_id, root);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    // A tab's panes are solved once per viewer and each pane takes its smallest
    // rect across them, so the client whose zoom comes back is left as the
    // tab's only viewer, and it is left tiled until the restore zooms it.
    let _ = server.handle_client_detach(client_id);
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root);
        client.zoom_pane(booted_tab_id, root);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);
    let receiver = server.subscribe(ClientId::new(), EventFilter::All);

    let _back = attach_with_token(&mut server, Some(leaving.resume_token), detached_at);

    let resized: Vec<Event> = receiver
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::Event(event @ Event::PtyResized(_)) => Some(event),
            _ => None,
        })
        .collect();
    // The zoomed pane fills the tab: 80x24 less the tabline and hint rows is
    // 80x22, less the one-cell border on each side is 78x20.
    assert_eq!(
        resized,
        vec![Event::PtyResized(PtyResized {
            pane_id: root,
            pty_size: PtySize {
                column_count: 78,
                row_count: 20
            },
        })]
    );
}

#[test]
fn a_client_the_restart_grace_still_holds_files_no_view_and_keeps_its_token() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client");
        client.update_focused_pane(booted_tab_id, root);
        client.set_scroll_offset(root, 500);
    }
    let token = server.saved_view_store.mint_resume_token(client_id);
    let now = SystemTime::now();
    server.client_ids_awaiting_reconnect.insert(client_id);

    server.save_client_view(client_id, now);

    assert_eq!(server.saved_view_store.take_saved_view(&token, now), None);
    // The hash still stands, so the restart path decides this record's fate.
    server.client_ids_awaiting_reconnect.remove(&client_id);
    server.save_client_view(client_id, now);
    assert_eq!(
        server.saved_view_store.take_saved_view(&token, now),
        Some(SavedView {
            active_tab_id: booted_tab_id,
            focused_pane_id_by_tab_id: HashMap::from([(booted_tab_id, root)]),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::from([(root, 500)]),
        })
    );
}

#[test]
fn a_claim_that_wins_keeps_its_record_and_drops_the_presented_tokens_view() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let additional_tab_id = add_additional_tab(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root);
        client.set_scroll_offset(root, 500);
        client.update_active_tab(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving.client_id, detached_at);
    let _ = server.handle_client_detach(leaving.client_id);

    // The booted client's record is still held and no connection streams for
    // it, so the claim wins and the token names a view of another client.
    let back = server
        .handle_ipc_attach(
            Some(client_id),
            Some(leaving.resume_token.clone()),
            TEST_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            detached_at,
            false,
        )
        .expect("the session hands the record back");

    assert_eq!(back.client_id, client_id);
    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("the record the claim took");
    assert_eq!(client.get_active_tab(), booted_tab_id);
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_scroll_offset(root), 0);
    // The token is spent either way, so presenting it again takes nothing.
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&leaving.resume_token, detached_at),
        None
    );
}

#[test]
fn a_detach_with_no_view_filed_leaves_its_token_taking_nothing_back() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let additional_tab_id = add_additional_tab(&mut server, session_id);
    let leaving = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root);
        client.update_active_tab(additional_tab_id);
    }
    let now = SystemTime::now();
    // The `core:detach` and `core:quit` path: the record goes with no view
    // filed for it.
    let _ = server.handle_client_detach(leaving.client_id);

    let back = attach_with_token(&mut server, Some(leaving.resume_token), now);

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(back.client_id)
        .expect("the minted client");
    assert_eq!(client.get_active_tab(), booted_tab_id);
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_scroll_offset(root), 0);
}

#[test]
fn an_attach_that_finds_no_session_spends_no_token() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let now = SystemTime::now();
    let leaving = attach_with_token(&mut server, None, now);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(leaving.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root, 42);
    server.save_client_view(leaving.client_id, now);

    // The process is past its last session, so there is nothing to attach to.
    server.session_by_id.clear();
    let refused = server.handle_ipc_attach(
        None,
        Some(leaving.resume_token.clone()),
        TEST_VIEWPORT_SIZE,
        None,
        EventFilter::All,
        now,
        false,
    );

    assert!(refused.is_none(), "no session is left to join");
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&leaving.resume_token, now),
        Some(SavedView {
            active_tab_id: booted_tab_id,
            focused_pane_id_by_tab_id: HashMap::from([(booted_tab_id, root)]),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::from([(root, 42)]),
        }),
        "the refused attach read no token and spent none"
    );
}

#[test]
fn a_connection_that_never_reached_its_stream_files_no_view() {
    let (mut server, client_id) = booted_server();
    let (session_id, _booted_tab_id, root) = booted_parts(&server, client_id);
    let now = SystemTime::now();
    let undelivered = attach_with_token(&mut server, None, now);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(undelivered.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root, 12);

    server
        .saved_view_store
        .forget_client_resume_token(undelivered.client_id);
    server.save_client_view(undelivered.client_id, now);

    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&undelivered.resume_token, now),
        None,
        "the token never reached that client, so its view is not filed"
    );
}

#[test]
fn a_client_the_session_no_longer_holds_files_no_view_and_drops_its_token() {
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, root) = booted_parts(&server, client_id);
    let gone = attach_with_token(&mut server, None, SystemTime::now());
    let now = SystemTime::now();
    let _ = server.handle_client_detach(gone.client_id);

    server.save_client_view(gone.client_id, now);

    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&gone.resume_token, now),
        None
    );
    // The store still takes the next mint and files against it.
    let newly_attached_client = attach_with_token(&mut server, None, now);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(newly_attached_client.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root, 500);
    server.save_client_view(newly_attached_client.client_id, now);
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&newly_attached_client.resume_token, now),
        Some(SavedView {
            active_tab_id: booted_tab_id,
            focused_pane_id_by_tab_id: HashMap::from([(booted_tab_id, root)]),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::from([(root, 500)]),
        })
    );
}

#[test]
fn a_resumed_server_starts_with_every_carried_client_awaiting_its_own_attach() {
    let (mut server, client_id) = booted_server();
    let (_header, carried_session_bytes) = server.carry_out(&[]).expect("a session to carry");
    let (tx, inbox_rx) = mpsc::channel();

    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        inbox_rx,
        tx,
        carried_session_bytes,
        HashMap::new(),
        HashMap::new(),
    );

    assert_eq!(
        resumed.client_ids_awaiting_reconnect,
        HashSet::from([client_id])
    );
    assert!(!resumed.is_restart_requested());
}

#[test]
fn closing_the_grace_window_detaches_only_the_clients_that_never_came_back() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    let absent = ClientId::new();
    server.handle_client_attach(
        session_id,
        absent,
        TEST_VIEWPORT_SIZE,
        None,
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("the booted client")
            .get_active_tab(),
        SystemTime::now(),
        false,
    );
    server.client_ids_awaiting_reconnect.insert(client_id);
    server.client_ids_awaiting_reconnect.insert(absent);
    // One of the two came back before the window closed.
    let _held = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            TEST_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the record is handed back");

    server.handle_drop_unclaimed_clients(Instant::now());

    let clients = &server.session_by_id[&session_id].clients;
    assert_eq!(clients.client_count(), 1);
    assert_eq!(
        clients
            .get_client_by_id(client_id)
            .map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(
        clients
            .get_client_by_id(absent)
            .map(|client| client.get_client_id()),
        None
    );
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn closing_the_grace_window_with_nobody_awaited_detaches_nobody() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);

    let events = server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(events, Vec::new());
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 1);
}

#[test]
fn a_quit_applied_before_the_swap_is_carried_to_the_next_image() {
    // A quit can land after the clients were told the session is restarting.
    // They are already waiting for the next socket by then, so the swap runs to
    // the end and the next image ends once it has them back — each one reads a
    // real quit instead of a session that stopped answering.
    let (mut server, _client_id) = booted_server();
    server.is_quit_requested = true;

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.carried_quit,
        Some(CarriedQuit::Graceful),
        "the carried state records the quit and its kind"
    );

    let (tx, receiver) = mpsc::channel();
    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        receiver,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );
    assert!(resumed.is_quit_requested());
    assert!(
        !resumed.should_shutdown_immediately,
        "a graceful quit stays graceful across the swap"
    );
}

#[test]
fn a_zero_grace_quit_is_still_zero_grace_after_the_swap() {
    // `request_quit` sets the flag and the kind together. Carrying only the
    // flag would turn a caller's zero-grace teardown into a graceful one in the
    // next image, so the kind travels with it.
    let (mut server, _client_id) = booted_server();
    server.is_quit_requested = true;
    server.should_shutdown_immediately = true;

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(body.carried_quit, Some(CarriedQuit::Immediate));

    let (tx, receiver) = mpsc::channel();
    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        receiver,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );
    assert!(resumed.is_quit_requested());
    assert!(resumed.should_shutdown_immediately);
}

#[test]
fn a_session_that_still_expects_a_client_back_is_not_ended_by_a_carried_quit() {
    // The clients were told to come back, so the quit waits for them: ending
    // first leaves each one polling a socket that never answers. The window
    // that empties the set is what bounds the wait.
    let (mut server, client_id) = booted_server();
    let (_session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.is_quit_requested = true;
    server.client_ids_awaiting_reconnect.insert(ClientId::new());

    assert!(
        server.awaits_a_client(),
        "a carried record is still unclaimed"
    );

    server.handle_drop_unclaimed_clients(Instant::now());

    assert!(
        !server.awaits_a_client(),
        "the window closing is what lets the quit through"
    );
}

#[test]
fn a_swap_with_no_quit_behind_it_comes_back_serving() {
    let (mut server, _client_id) = booted_server();
    assert!(!server.is_quit_requested());

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(body.carried_quit, None);

    let (tx, receiver) = mpsc::channel();
    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        receiver,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );
    assert!(!resumed.is_quit_requested());
}

#[test]
fn a_detach_that_lands_while_a_client_is_awaited_leaves_its_record_alone() {
    // The connection of a client that was told the session is restarting ends,
    // so its detach arrives while the grace window still owns that record. The
    // record has to stay until the window closes, or the client that comes back
    // finds nothing to claim.
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.client_ids_awaiting_reconnect.insert(client_id);

    let events = server.handle_client_detach(client_id);

    assert_eq!(events, Vec::new());
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 1);
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert!(server.client_ids_awaiting_reconnect.contains(&client_id));

    // The window closing is what detaches it, and it takes the record with it.
    server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 0);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn the_restart_announcement_waits_for_every_client_to_hold_the_frame() {
    // The image is replaced right after this call, and nothing joins the client
    // writing threads. A call that returned early would leave a client whose
    // frame was still on its way, and that client would read end of stream and
    // report the session dead.
    let (mut server, _client_id) = booted_server();
    let notice = Arc::clone(server.ending_notice());
    notice.record_writer_started();
    let counted = Arc::clone(&notice);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        counted.record_writer_ended();
    });

    let started = Instant::now();
    server.announce_restarting();
    let waited = started.elapsed();

    assert_eq!(
        notice.get_session_ending(),
        Some(SessionEnding::Restarting),
        "the notice must name the frame the clients are told"
    );
    assert_eq!(
        notice.count_running_writers(),
        0,
        "the call must return only once no writing thread is left"
    );
    assert!(
        waited >= Duration::from_millis(150),
        "the call returned after {waited:?}, before the writing thread ended"
    );
    writer.join().expect("the writing thread ends");
}

#[test]
fn the_restart_announcement_gives_up_on_a_client_that_never_takes_the_frame() {
    // A client that stopped reading its socket leaves its writing thread
    // blocked inside the write. Waiting on that thread without a limit would
    // hold the image swap open until that client came back.
    let (mut server, _client_id) = booted_server();
    let notice = Arc::clone(server.ending_notice());
    notice.record_writer_started();

    let started = Instant::now();
    server.announce_restarting();
    let waited = started.elapsed();

    assert_eq!(
        notice.count_running_writers(),
        1,
        "the writing thread that never ends must still be counted"
    );
    assert!(
        waited >= CLIENT_NOTIFICATION_TIMEOUT_DURATION,
        "the call returned after {waited:?}, before the limit"
    );
    assert!(
        waited < CLIENT_NOTIFICATION_TIMEOUT_DURATION * 3,
        "the call waited {waited:?}, well past the limit"
    );
}

#[test]
fn the_quit_announcement_tells_the_clients_the_session_ended() {
    // The process tears down right after this call. A client that was never
    // told reads end of stream and reports the session dead, instead of saying
    // the session ended.
    let (mut server, _client_id) = booted_server();
    let (_, queue) = server.event_bus.subscribe(EventFilter::All);

    server.announce_quit();

    assert_eq!(
        queue.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
    assert_eq!(
        server.ending_notice().get_session_ending(),
        Some(SessionEnding::Quit),
        "the notice must name the frame the clients are told"
    );
}

#[test]
fn the_quit_announcement_leaves_a_published_quit_as_the_only_one() {
    // Closing the last tab publishes the quit itself, which raises the notice.
    // The stream's last frame goes out once.
    let (mut server, _client_id) = booted_server();
    let (_, queue) = server.event_bus.subscribe(EventFilter::All);
    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    server.announce_quit();

    assert_eq!(
        queue.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn a_quit_announced_after_a_restart_keeps_the_restart_as_the_last_frame() {
    // The notice keeps the frame it was raised with first, so the quit publishes
    // nothing. Every client read the restart frame and left this stream while
    // `announce_restarting` waited for its writing thread to end, so the session
    // server is what decides where a quit during a swap ends the session.
    let (mut server, _client_id) = booted_server();
    let (_, queue) = server.event_bus.subscribe(EventFilter::All);

    server.announce_restarting();
    server.announce_quit();

    assert_eq!(
        queue.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Restarting)],
        "the restart frame must be the only one published"
    );
    assert_eq!(
        server.ending_notice().get_session_ending(),
        Some(SessionEnding::Restarting),
        "the notice must keep the frame the clients were told"
    );
}

#[test]
fn an_attach_claiming_a_client_whose_tab_is_gone_mints_a_new_one_and_leaves_that_record_awaited() {
    // The tab a client was viewing can be closed by another client while this
    // one is away. Handing the record back would put the client on a tab that
    // no longer exists, so a fresh client is minted on the first tab; the record
    // itself keeps waiting and the grace window decides its fate.
    let (mut server, client_id) = booted_server();
    let (session_id, booted_tab_id, _booted_pane_id) = booted_parts(&server, client_id);
    let closed_tab_id = add_additional_tab(&mut server, session_id);
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session");
        session
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client")
            .update_active_tab(closed_tab_id);
        session.tabs.remove(&closed_tab_id);
    }
    server.client_ids_awaiting_reconnect.insert(client_id);

    let accepted = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            EventFilter::All,
            SystemTime::now(),
            false,
        )
        .expect("the session mints a client instead of refusing");

    assert_ne!(accepted.client_id, client_id);
    assert_eq!(accepted.session_id, session_id);
    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 2);
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(accepted.client_id)
            .expect("the minted record")
            .get_active_tab(),
        booted_tab_id
    );
    assert_eq!(
        server.client_ids_awaiting_reconnect,
        HashSet::from([client_id]),
        "the record nobody could come back as keeps waiting for the grace window"
    );
}

#[test]
fn a_second_restart_request_runs_the_check_again_and_leaves_one_swap_asked_for() {
    // Two `koshi update` runs can reach one session before its loop reads the
    // flag. Each request is answered on its own, and the loop still exits into
    // exactly one swap.
    let (mut server, _client_id) = booted_server();
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&runs);
    server.set_restart_check(Arc::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }));

    assert_eq!(server.handle_ipc_restart(), Ok(()));
    assert_eq!(server.handle_ipc_restart(), Ok(()));

    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "each request is checked on its own"
    );
    assert!(server.is_restart_requested());
    server.cancel_restart();
    assert!(
        !server.is_restart_requested(),
        "one cancel takes the accepted restart back, whatever the count of requests"
    );
}

#[test]
fn a_restart_refused_after_one_was_accepted_leaves_the_swap_asked_for() {
    // The binary on disk can be replaced again between two requests. The second
    // request is answered with what is wrong now, and the swap the first one
    // already won is not taken back by it.
    let (mut server, _client_id) = booted_server();
    server.set_restart_check(Arc::new(|| Ok(())));
    assert_eq!(server.handle_ipc_restart(), Ok(()));

    server.set_restart_check(Arc::new(|| {
        Err("the binary at /x is not executable".to_string())
    }));

    assert_eq!(
        server.handle_ipc_restart(),
        Err("the binary at /x is not executable".to_string())
    );
    assert!(server.is_restart_requested());
}

#[test]
fn a_carried_client_that_never_came_back_is_detached_even_after_its_tab_was_closed() {
    // The grace window closes on a record whose tab went away while the client
    // was gone. The detach must still take the record off the session rather
    // than leaving it holding a tab nothing can view.
    let (mut server, client_id) = booted_server();
    let (session_id, _booted_tab_id, _booted_pane_id) = booted_parts(&server, client_id);
    let closed_tab_id = add_additional_tab(&mut server, session_id);
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session");
        session
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client")
            .update_active_tab(closed_tab_id);
        session.tabs.remove(&closed_tab_id);
    }
    server.client_ids_awaiting_reconnect.insert(client_id);

    server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(server.session_by_id[&session_id].clients.client_count(), 0);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn a_session_switch_reaches_every_subscriber_that_views_the_client() {
    let (mut server, client_id) = booted_server();
    let first_receiver = server.subscribe(client_id, EventFilter::All);
    let second_receiver = server.subscribe(client_id, EventFilter::All);
    let onlooker = server.subscribe(ClientId::new(), EventFilter::All);
    let target_session_id = SessionId::new();

    assert!(server.send_switch(client_id, target_session_id));

    assert_eq!(
        first_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::SwitchTo(target_session_id)]
    );
    assert_eq!(
        second_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::SwitchTo(target_session_id)]
    );
    assert_eq!(
        onlooker.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_session_switch_for_a_client_no_subscriber_views_is_held_by_nobody() {
    let (mut server, client_id) = booted_server();
    let receiver = server.subscribe(client_id, EventFilter::All);

    assert!(!server.send_switch(ClientId::new(), SessionId::new()));

    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_session_switch_offered_to_a_paused_subscriber_is_held_by_nobody() {
    let (mut server, client_id) = booted_server();
    let receiver = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    let _backlog: Vec<Delivery> = receiver.try_iter().collect();

    assert!(!server.send_switch(client_id, SessionId::new()));

    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
}

#[test]
fn queued_host_bytes_go_behind_whatever_is_already_queued() {
    let (mut server, client_id) = booted_server();

    // Two OSC 52 copies, "one" then "two".
    server.queue_host_write(client_id, b"\x1b]52;c;b25l\x07");
    server.queue_host_write(client_id, b"\x1b]52;c;dHdv\x07");

    assert_eq!(
        server.take_host_writes(client_id),
        Some(b"\x1b]52;c;b25l\x07\x1b]52;c;dHdv\x07".to_vec())
    );
    assert_eq!(server.take_host_writes(client_id), None);
}

#[test]
fn handing_the_inbox_over_keeps_the_receiver_the_panes_deliver_into() {
    let (server, tx) = build_test_server_with_event_sender();
    tx.send(RuntimeEvent::Timer).expect("send before the swap");

    let inbox_rx = server.into_inbox_rx();

    tx.send(RuntimeEvent::Timer).expect("send after the swap");
    assert!(matches!(inbox_rx.try_recv(), Ok(RuntimeEvent::Timer)));
    assert!(matches!(inbox_rx.try_recv(), Ok(RuntimeEvent::Timer)));
    assert!(matches!(
        inbox_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

/// One live pane as the PTY backend reports it: no terminal descriptor, so no
/// terminal name is read for it.
fn carried_pty_pane(pane_id: PaneId, pty_size: PtySize) -> CarriedPtyPane {
    CarriedPtyPane {
        pane_id,
        #[cfg(unix)]
        terminal_fd: None,
        process_id: 51234,
        pty_size,
        exit_status: None,
    }
}

#[test]
fn carrying_out_names_the_session_the_body_carries() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    let session_name = server.session_by_id[&session_id].session_name.clone();
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("session exists");
        assert!(session.advance_placement_revision());
        assert!(session
            .clients
            .get_client_mut_by_id(client_id)
            .expect("client exists")
            .advance_placement_revision());
    }

    let (header, body) = server.carry_out(&[]).expect("a session to carry");

    assert_eq!(header.session_id, session_id);
    assert_eq!(header.session_name, session_name);
    assert_eq!(body.session_by_id[&session_id].session_id, session_id);
    assert_eq!(body.session_by_id[&session_id].session_name, session_name);
    assert_eq!(body.session_by_id[&session_id].get_placement_revision(), 1);
    assert_eq!(
        body.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("carried client exists")
            .get_placement_revision(),
        1
    );
}

#[test]
fn carrying_out_a_server_with_no_session_carries_nothing_and_changes_nothing() {
    let (mut server, _tx) = build_test_server_with_event_sender();

    assert!(server.carry_out(&[]).is_none());

    assert!(server.list_sessions().is_empty());
    assert!(server.list_terminal_engines().is_empty());
}

#[test]
fn carrying_out_sizes_each_pane_by_this_servers_record_and_the_backend_otherwise() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, root) = booted_parts(&server, client_id);
    let session_name = server.session_by_id[&session_id].session_name.clone();
    // 80x24 less the tabline and hint rows is 80x22, less the one-cell border
    // on each side is 78x20.
    assert_eq!(
        server.pty_size_by_pane_id[&root],
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
    let unrecorded = PaneId::new();
    let panes = [
        carried_pty_pane(
            root,
            PtySize {
                column_count: 1,
                row_count: 1,
            },
        ),
        carried_pty_pane(
            unrecorded,
            PtySize {
                column_count: 40,
                row_count: 12,
            },
        ),
    ];

    let (header, body) = server.carry_out(&panes).expect("a session to carry");

    assert_eq!(header.resume_format, RESUME_FORMAT);
    assert_eq!(header.session_id, session_id);
    assert_eq!(header.session_name, session_name);
    assert_eq!(
        header.carried_panes,
        vec![
            CarriedPane {
                pane_id: root,
                process_id: 51234,
                row_count: 20,
                column_count: 78,
                terminal_fd: None,
                terminal_name: None,
                exit_status: None,
            },
            CarriedPane {
                pane_id: unrecorded,
                process_id: 51234,
                row_count: 12,
                column_count: 40,
                terminal_fd: None,
                terminal_name: None,
                exit_status: None,
            },
        ]
    );
    // The state moved out of the server and into the body.
    assert_eq!(body.session_by_id.len(), 1);
    assert_eq!(body.terminal_state_by_pane_id.len(), 1);
    assert!(server.list_sessions().is_empty());
    assert!(server.list_terminal_engines().is_empty());
}

#[test]
fn a_resumed_server_puts_every_carried_engine_back_with_its_undecoded_bytes() {
    let (mut server, client_id) = booted_server();
    let (_session_id, _tab_id, root) = booted_parts(&server, client_id);
    // "hi" is printed; `ESC [` opens a control sequence that has no final byte
    // yet, so the parser stops there and holds those two bytes.
    server.handle_pty_output(root, b"hi\x1b[");
    assert_eq!(
        server.terminal_engine_by_pane_id[&root].undecoded_terminal_bytes(),
        b"\x1b["
    );
    let carried_size = server.pty_size_by_pane_id[&root];

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.undecoded_bytes_by_pane_id,
        HashMap::from([(root, b"\x1b[".to_vec())]),
        "only a pane holding bytes has an entry"
    );
    let carried_state = body.terminal_state_by_pane_id[&root].clone();
    let (tx, inbox_rx) = mpsc::channel();

    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        inbox_rx,
        tx,
        body,
        HashMap::from([(root, PtyHandle::from_detached_pane_id(root))]),
        HashMap::from([(root, carried_size)]),
    );

    assert_eq!(resumed.terminal_engine_by_pane_id.len(), 1);
    assert_eq!(
        resumed.terminal_engine_by_pane_id[&root].get_terminal_state(),
        &carried_state
    );
    assert_eq!(
        resumed.terminal_engine_by_pane_id[&root].undecoded_terminal_bytes(),
        b"\x1b["
    );
    assert_eq!(
        resumed.pty_size_by_pane_id,
        HashMap::from([(root, carried_size)])
    );
    assert_eq!(
        resumed.pty_handle_by_pane_id[&root].get_pane_id(),
        root,
        "the handle the caller built is the one the pane keeps"
    );
}

#[test]
fn a_resumed_server_keeps_queued_graphics_events() {
    let (mut server, client_id) = booted_server();
    let (_session_id, _tab_id, root) = booted_parts(&server, client_id);
    server.handle_pty_output(root, b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(body.graphics_events_by_pane_id[&root].len(), 1);

    let (tx, inbox_rx) = mpsc::channel();
    let mut resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        inbox_rx,
        tx,
        body,
        HashMap::from([(root, PtyHandle::from_detached_pane_id(root))]),
        HashMap::from([(
            root,
            PtySize {
                column_count: 78,
                row_count: 20,
            },
        )]),
    );

    let engine = resumed
        .terminal_engine_by_pane_id
        .get_mut(&root)
        .expect("the engine");
    assert_eq!(engine.get_terminal_state().list_image_placements().len(), 1);
    assert_eq!(
        engine.get_terminal_state().list_image_placements()[0].get_image_anchor(),
        (0, 0)
    );
    assert_eq!(
        engine.take_graphics_events(),
        vec![Ok(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: (DecodedImage {
                pixel_width: 1,
                pixel_height: 1,
                rgba_bytes: vec![255, 0, 0, 255],
            })
            .into(),
            animation: None,
            action: ImageAction::TransmitAndDisplay,
            display: ImageDisplay {
                requested_column_count: Some(1),
                requested_row_count: Some(1),
                should_move_cursor: false,
                ..ImageDisplay::default()
            },
            anchor: (0, 0),
        })]
    );
}

#[test]
fn a_resumed_server_keeps_the_graphics_queue_overflow_report() {
    let (mut server, client_id) = booted_server();
    let (_session_id, _tab_id, root) = booted_parts(&server, client_id);
    let mut graphics_input_bytes = Vec::new();
    for _ in 0..66 {
        graphics_input_bytes
            .extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    }
    server.handle_pty_output(root, &graphics_input_bytes);

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.graphics_events_by_pane_id[&root].len(),
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_BATCH_COUNT
    );

    let (tx, inbox_rx) = mpsc::channel();
    let mut resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        inbox_rx,
        tx,
        body,
        HashMap::from([(root, PtyHandle::from_detached_pane_id(root))]),
        HashMap::from([(
            root,
            PtySize {
                column_count: 78,
                row_count: 20,
            },
        )]),
    );
    let events = resumed
        .terminal_engine_by_pane_id
        .get_mut(&root)
        .expect("the resumed engine")
        .take_graphics_events();

    assert_eq!(
        events.len(),
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_BATCH_COUNT
    );
    assert_eq!(
        events.last(),
        Some(&Err(koshi_terminal::graphics::GraphicsError::QueueFull {
            dropped_event_count: 2
        }))
    );
}

#[test]
fn a_resumed_server_keeps_graphics_inside_a_split_screen_wrapper() {
    let (mut server, client_id) = booted_server();
    let (_session_id, _tab_id, root) = booted_parts(&server, client_id);
    let image_graphics_bytes = b"\x1b]1337;File=inline=1;width=1;height=1:iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\x07";
    let split_byte_index = image_graphics_bytes.len() / 2;
    let screen_wrap = |inner_graphics_bytes: &[u8]| {
        let mut wrapped_graphics_bytes = b"\x1bP".to_vec();
        wrapped_graphics_bytes.extend_from_slice(inner_graphics_bytes);
        wrapped_graphics_bytes.extend_from_slice(b"\x1b\\");
        wrapped_graphics_bytes
    };

    server.handle_pty_output(
        root,
        &screen_wrap(&image_graphics_bytes[..split_byte_index]),
    );
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    let transport = body
        .graphics_transport_by_pane_id
        .get(&root)
        .expect("the split wrapper has transport state");
    assert!(transport.screen_inner_transport.is_some());

    let (tx, inbox_rx) = mpsc::channel();
    let mut resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        inbox_rx,
        tx,
        body,
        HashMap::from([(root, PtyHandle::from_detached_pane_id(root))]),
        HashMap::from([(
            root,
            PtySize {
                column_count: 78,
                row_count: 20,
            },
        )]),
    );
    resumed.handle_pty_output(
        root,
        &screen_wrap(&image_graphics_bytes[split_byte_index..]),
    );

    let events = resumed
        .terminal_engine_by_pane_id
        .get_mut(&root)
        .expect("the resumed engine")
        .take_graphics_events();
    assert_eq!(events.len(), 1);
    let event = events
        .into_iter()
        .next()
        .expect("the resumed image event")
        .expect("the resumed image decodes");
    assert_eq!(event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(event.image.pixel_width, 1);
    assert_eq!(event.image.pixel_height, 1);
    assert_eq!(event.image.rgba_bytes, vec![0, 0, 0, 255]);
    assert_eq!(event.action, ImageAction::Display);
    assert_eq!(
        event.display,
        ImageDisplay {
            requested_width: Some(koshi_terminal::graphics::ImageDimension::Cells(1)),
            requested_height: Some(koshi_terminal::graphics::ImageDimension::Cells(1)),
            ..ImageDisplay::default()
        }
    );
    assert_eq!(event.anchor, (0, 0));
}

#[cfg(unix)]
#[test]
fn the_pane_check_names_the_first_pane_with_no_terminal_descriptor() {
    let pane_without_terminal_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let panes = [
        carried_pty_pane(
            pane_without_terminal_id,
            PtySize {
                column_count: 80,
                row_count: 24,
            },
        ),
        carried_pty_pane(
            other_pane_id,
            PtySize {
                column_count: 80,
                row_count: 24,
            },
        ),
    ];

    assert_eq!(
        can_carry_panes(&panes),
        Err(format!(
            "pane {pane_without_terminal_id} has no terminal descriptor, so its terminal cannot cross the swap"
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_binary_only_others_may_execute_passes_the_binary_check() {
    let binary_directory =
        std::env::temp_dir().join(format!("koshi-restart-otherexec-{}", std::process::id()));
    std::fs::create_dir_all(&binary_directory).expect("the directory is created");
    let executable_path = binary_directory.join("koshi");
    // The check tests `mode & 0o111`, so one execute bit anywhere in the mode
    // is enough.
    write_binary(&executable_path, 0o001);

    assert_eq!(is_binary_runnable(&executable_path), Ok(()));

    let _ = std::fs::remove_dir_all(&binary_directory);
}
