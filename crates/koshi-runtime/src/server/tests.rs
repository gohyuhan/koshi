//! Tests for the server half.
//!
//! Construction defaults, the held service handles, and the wired event inbox.
//! A session with one tab and one pane. The two doors — commands in via
//! `submit_command`, events out via `subscribe` — with the identity every
//! attached client carries, and a detach that leaves the server healthy with
//! its panes alive. Frame delivery: a subscriber paused by a dropped critical
//! event is handed a fresh frame or dropped, and a due render hands every
//! client its own frame behind any bytes queued for that client's own
//! terminal. A freed slot on a queue that missed a frame hands that client
//! alone a frame, and a subscriber owed a frame is offered none until its
//! writing thread takes the owed-frame mark. The restart checks, an accepted
//! or refused restart request, and the announcements that wait for every
//! client to hold the session's last frame. The state that crosses an image
//! swap: what `carry_out` writes and what `resume` puts back. The view a client
//! leaves behind, taken back by the token the next attach presents.

use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::command::{
    ClosePaneArgs, Command, CommandSource, NewPaneArgs, NewPanePlacement, PanePlacementAnchor,
    PanePlacementTarget, PlacePaneArgs, TiledPlacementTarget, ToggleLockModeArgs,
};
use koshi_core::event::{
    InputModeChanged, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved, PtyResized,
    SubscriberLagged, TabCreated,
};
use koshi_core::geometry::{Direction, FloatingPanePosition, PaneArea, Point};
use koshi_core::ids::{CommandId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;
use koshi_ipc::protocol::ConnectionToken;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneRecord;
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::{ClientOrigin, ClientRegistry, FloatingPaneView};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;
use crate::runtime::bus::SUBSCRIBER_QUEUE_CAPACITY;
use crate::runtime::command::tests::{
    attach_client_through_server, build_floating_new_pane_args, build_tab, create_floating_pane,
    create_session_with_attached_client, get_command_outcome,
};
use crate::runtime::event::{AttachAccepted, SessionEnding};
use crate::runtime::render_schedule::tests::{
    mark_changed_views_then_compute_frame_wait, DEFAULT_FRAME_INTERVAL_DURATION,
};
use crate::runtime::saved_view::SavedView;

impl Server {
    /// Every host-write byte that `delivery_receiver`, a subscription that
    /// views `client_id`, hands the thread that writes its connection, read
    /// the way that thread reads it: after each delivery it reads, it takes
    /// every byte that waits, and a [`Delivery::HostWrite`] clears the queued
    /// wake first. Every other delivery read is dropped. `None` when no byte
    /// is taken, which a subscription the bus removed matches too.
    pub(crate) fn take_host_writes(
        &self,
        client_id: ClientId,
        delivery_receiver: &DeliveryReceiver,
    ) -> Option<Vec<u8>> {
        let writer_handoff = self
            .find_subscriber_id_by_client_id(client_id)
            .and_then(|subscriber_id| self.event_bus.find_writer_handoff(subscriber_id))?;
        let mut taken_host_write_bytes: Option<Vec<u8>> = None;
        for delivery in delivery_receiver.try_iter() {
            if delivery == Delivery::HostWrite {
                writer_handoff.clear_queued_wake();
            }
            let host_write_bytes = writer_handoff.take_host_write_bytes();
            if !host_write_bytes.is_empty() {
                taken_host_write_bytes
                    .get_or_insert_with(Vec::new)
                    .extend(host_write_bytes);
            }
        }
        taken_host_write_bytes
    }

    /// Borrow what this session and its clients' writing threads share about
    /// the session's last frame.
    pub(crate) fn get_ending_notice(&self) -> &Arc<crate::runtime::event::EndingNotice> {
        self.event_bus.get_ending_notice()
    }

    /// Borrow the event bus.
    pub(crate) fn get_event_bus(&self) -> &EventBus {
        &self.event_bus
    }
}

const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};
/// The viewport of a second, out-of-process client. It differs from
/// [`TEST_VIEWPORT_SIZE`].
const REMOTE_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 30,
};

/// A server bootstrapped with one session, one tab, and one shell pane, plus
/// its client id.
fn boot_server() -> (Server, ClientId) {
    let (mut server, _event_sender) = build_test_server_with_event_sender();
    let client_id = create_session_with_attached_client(
        &mut server,
        SessionId::new(),
        TEST_VIEWPORT_SIZE,
        SystemTime::now(),
    )
    .expect("bootstrap");
    (server, client_id)
}

#[test]
fn close_undriven_panes_uses_reported_exit_status_and_unobserved_fallback() {
    for (reported_exit_status, expected_exit_code) in
        [(Some(ExitStatus::ExitCode(7)), 7), (None, -1)]
    {
        let (mut server, _) = boot_server();
        let pane_id = *server.live_pane_ids.iter().next().expect("one pane");
        server.live_pane_ids.remove(&pane_id);
        let exit_status_by_pane_id = reported_exit_status
            .map(|exit_status| HashMap::from([(pane_id, exit_status)]))
            .unwrap_or_default();

        let exit_events =
            server.close_undriven_panes(HashSet::from([pane_id]), exit_status_by_pane_id);

        assert_eq!(
            exit_events.first(),
            Some(&Event::PaneProcessExited(PaneProcessExited {
                pane_id,
                exit_code: Some(expected_exit_code),
                signal: None,
            }))
        );
        assert!(server.get_session_for_pane(pane_id).is_none());
    }
}

#[test]
fn close_undriven_panes_removes_a_floating_pane_whose_child_is_gone() {
    let (mut server, client_id) = boot_server();
    let floating_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let booted_client = server
        .list_sessions()
        .values()
        .next()
        .expect("the booted session")
        .clients
        .get_client_by_id(client_id)
        .expect("the booted client");
    let tab_id = booted_client.get_active_tab_id();
    let tiled_pane_id = booted_client
        .get_focused_pane_id(tab_id)
        .expect("the booted client focuses the tab's pane");
    server.live_pane_ids.remove(&floating_pane_id);

    let exit_events =
        server.close_undriven_panes(HashSet::from([floating_pane_id]), HashMap::new());

    // The floating pane took the client's input when it opened; its removal
    // returns the input to the tiled pane.
    assert_eq!(
        exit_events,
        vec![
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: floating_pane_id,
                exit_code: Some(-1),
                signal: None,
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: floating_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: floating_pane_id,
                tab_id: None,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: tiled_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    assert!(server.get_session_for_pane(floating_pane_id).is_none());
    let session = server
        .list_sessions()
        .values()
        .next()
        .expect("the booted session");
    assert_eq!(session.floating_set.list_members(), &[]);
}

#[test]
fn a_floating_pane_is_shown_only_to_a_client_that_shows_it() {
    let (mut server, client_id) = boot_server();
    let floating_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let later_client_id =
        attach_client_through_server(&mut server, client_id, TEST_VIEWPORT_SIZE, None);

    assert!(server.is_pane_shown_to_client(client_id, floating_pane_id));
    assert!(!server.is_pane_shown_to_client(later_client_id, floating_pane_id));

    assert!(server
        .get_client_mut(client_id)
        .expect("the booted client")
        .minimize_floating_pane(floating_pane_id));

    assert!(!server.is_pane_shown_to_client(client_id, floating_pane_id));
}

/// Each subscription as its subscriber and the client it views, in
/// subscription order.
fn list_subscription_pairs(server: &Server) -> Vec<(SubscriberId, ClientId)> {
    server
        .subscriptions
        .iter()
        .map(|frame_subscription| {
            (
                frame_subscription.subscriber_id,
                frame_subscription.client_id,
            )
        })
        .collect()
}

/// Publish critical events until every subscriber's queue overflows and pauses
/// it, so exactly one event was dropped for each.
fn pause_subscribers(server: &mut Server) {
    while !server.event_bus.has_desynced_subscribers() {
        server.event_bus.publish(&Event::Quit(QuitCause::Requested));
    }
}

fn build_test_server_with_event_sender() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let (runtime_event_sender, inbox_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender.clone()),
    )));
    let server = Server::from_runtime_parts(pty_backend, inbox_receiver);
    (server, runtime_event_sender)
}

#[test]
fn a_new_server_starts_with_no_sessions_or_engines() {
    let (server, _event_sender) = build_test_server_with_event_sender();

    assert!(server.list_sessions().is_empty());
    assert!(server.list_terminal_engines().is_empty());
    assert!(server.get_ipc_server().is_none());
}

#[test]
fn accessors_return_the_constructed_services() {
    let (server, _event_sender) = build_test_server_with_event_sender();

    assert_eq!(Arc::strong_count(server.get_pty_backend()), 1);
    assert_eq!(server.get_event_bus().count_subscribers(), 0);
}

#[test]
fn inbox_delivers_events_to_the_receiver() {
    let (server, runtime_event_sender) = build_test_server_with_event_sender();

    runtime_event_sender
        .send(RuntimeEvent::Quit)
        .expect("send to inbox");

    let received_runtime_event = server.get_inbox_receiver().try_recv();
    let Ok(RuntimeEvent::Quit) = received_runtime_event else {
        panic!("expected Quit, got {received_runtime_event:?}");
    };
}

#[test]
fn holds_one_session_with_one_tab_and_pane() {
    let (mut server, _inbox_sender) = build_test_server_with_event_sender();

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
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
        .expect("pane registers");
    session.tabs.insert(
        tab_id,
        build_tab(tab_id, "shell", 0, LayoutNode::Pane(pane_id)),
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

    assert_eq!(session.panes.count_pane_records(), 1);
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
fn a_fresh_server_has_no_quit_flag_set() {
    let (server, _event_sender) = build_test_server_with_event_sender();

    assert!(!server.is_quit_requested());
}

#[test]
fn every_attached_client_is_local_with_its_own_generated_label() {
    let (mut server, bootstrapped_client_id) = boot_server();
    let session_id = *server.list_sessions().keys().next().expect("session");
    let attached_client_id = attach_client_through_server(
        &mut server,
        bootstrapped_client_id,
        TEST_VIEWPORT_SIZE,
        None,
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
    let (mut server, bootstrapped_client_id) = boot_server();
    let session_id = *server.list_sessions().keys().next().expect("session");
    let active_tab_id = server.list_sessions()[&session_id]
        .clients
        .get_client_by_id(bootstrapped_client_id)
        .expect("client record")
        .get_active_tab_id();

    // A second client attaches, then detaches again.
    let attached_client_id = ClientId::new();
    let emitted_events = server.handle_client_attach(
        session_id,
        attached_client_id,
        TEST_VIEWPORT_SIZE,
        None,
        active_tab_id,
        None,
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
            tab_id: Some(active_tab_id),
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
            .count_pane_records(),
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
            .count_pane_records(),
        1
    );
    assert!(server.has_active_panes());
}

#[test]
fn submit_command_dispatches_against_live_state() {
    let (mut server, client_id) = boot_server();

    let command_id = CommandId::new();
    let command_result = server.submit_command(CommandEnvelope::from_parts(
        command_id,
        CommandSource::KeyBinding { client_id },
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
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);

    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::InputModeChanged(InputModeChanged {
            client_id,
            lock_mode: LockMode::Locked,
        }))]
    );
}

#[test]
fn publish_events_delivers_out_of_command_events_to_subscribers() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let published_events = vec![Event::Quit(QuitCause::Requested)];

    server.publish_events(&published_events);

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn publish_events_remembers_out_of_command_events_in_the_recent_events_ring() {
    let (mut server, _client_id) = boot_server();
    let tab_id = TabId::new();

    server.publish_events(&[Event::LayoutChanged(koshi_core::event::LayoutChanged {
        tab_id,
    })]);

    // The ring is process-wide, and every test in this binary writes to it. The
    // record is found by this tab's own id.
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
    let (mut server, client_id) = boot_server();

    let (_delivery_receiver, _) = server.subscribe(client_id, 60);

    assert_eq!(
        server
            .subscriptions
            .iter()
            .map(|frame_subscription| frame_subscription.client_id)
            .collect::<Vec<ClientId>>(),
        vec![client_id]
    );
}

#[test]
fn a_subscriber_whose_receiver_is_gone_loses_its_recorded_client_too() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    drop(delivery_receiver);

    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert_eq!(list_subscription_pairs(&server), Vec::new());
}

#[test]
fn resyncing_hands_a_paused_subscriber_a_frame_of_the_client_it_views() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    // Drain the queue: the frame fits on the next pass.
    let _delivery_backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged(Instant::now());

    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 1,
            },
        }]
    );
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(subscriber_id, client_id)]
    );
}

/// A paused subscriber whose client is the tab's only viewer and reports
/// [`PaneArea::Starving`] is resynced with a frame carrying every pane
/// suppressed, and keeps its subscription.
#[test]
fn resyncing_a_starving_sole_viewer_keeps_its_subscription() {
    let (mut server, client_id) = boot_server();
    let _ = server.handle_client_resize(
        client_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Starving),
        None,
    );
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _delivery_backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    assert!(
        expected_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    );
    assert_eq!(
        expected_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
        Size {
            column_count: 0,
            row_count: 0
        }
    );

    server.resync_lagged(Instant::now());

    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(subscriber_id, client_id)]
    );
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 1,
            },
        }]
    );
}

#[test]
fn resyncing_with_nobody_paused_delivers_nothing() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;

    server.resync_lagged(Instant::now());

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(subscriber_id, client_id)]
    );
    assert!(server.event_bus.has_subscriber(subscriber_id));
}

#[test]
fn a_resync_blocked_by_a_full_queue_retries_with_a_newer_frame() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, writer_handoff) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);

    // The queue is still full, so the frame does not fit and the subscriber
    // stays paused.
    server.resync_lagged(Instant::now());
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );

    // Change the state the frame reports, then make room the way the writing
    // thread does: it reads the backlog and takes the owed-frame mark.
    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));
    let _backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();
    assert!(writer_handoff.take_owed_frame_mark());
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    assert_eq!(
        expected_render_snapshot.client_snapshot.lock_mode,
        koshi_core::lock::LockMode::Locked
    );

    server.resync_lagged(Instant::now());

    // The retry built a new frame: it carries the mode set after the first
    // attempt failed, not the one that was current then. The count covers the
    // event that triggered the pause plus the withheld mode change.
    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 2,
            },
        }]
    );
}

#[test]
fn one_unresyncable_subscriber_does_not_block_the_others_frame() {
    let (mut server, client_id) = boot_server();
    let (kept_receiver, _) = server.subscribe(client_id, 60);
    let kept_subscriber_id = server.subscriptions[0].subscriber_id;
    // Straight off the bus, so nothing records which client it views.
    let (orphan_subscriber_id, orphan_receiver, _) = server.event_bus.subscribe();
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![kept_subscriber_id, orphan_subscriber_id]
    );
    let _good_backlog: Vec<Delivery> = kept_receiver.try_iter().collect();
    let _orphan_backlog: Vec<Delivery> = orphan_receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged(Instant::now());

    assert_eq!(
        kept_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id: kept_subscriber_id,
                dropped_event_count: 1,
            },
        }]
    );
    assert!(!server.event_bus.has_subscriber(orphan_subscriber_id));
    assert_eq!(
        orphan_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(kept_subscriber_id, client_id)]
    );
}

#[test]
fn a_gone_receiver_costs_only_its_own_recorded_client() {
    let (mut server, client_id) = boot_server();
    let (kept_receiver, _) = server.subscribe(client_id, 60);
    let (gone_receiver, _) = server.subscribe(client_id, 60);
    let kept_subscriber_id = server.subscriptions[0].subscriber_id;
    let gone_subscriber_id = server.subscriptions[1].subscriber_id;
    drop(gone_receiver);

    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    assert!(!server.event_bus.has_subscriber(gone_subscriber_id));
    assert!(server.event_bus.has_subscriber(kept_subscriber_id));
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(kept_subscriber_id, client_id)]
    );
    assert_eq!(
        kept_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn a_paused_subscriber_that_views_no_client_is_unsubscribed() {
    let (mut server, _client_id) = boot_server();
    // Straight off the bus, so nothing records which client it views.
    let (subscriber_id, delivery_receiver, _) = server.event_bus.subscribe();
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _drained_deliveries: Vec<Delivery> = delivery_receiver.try_iter().collect();

    server.resync_lagged(Instant::now());

    assert_eq!(server.event_bus.count_subscribers(), 0);
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_paused_subscriber_whose_client_is_gone_is_unsubscribed() {
    let (mut server, _client_id) = boot_server();
    // No session holds this id, so no frame can ever be built for it.
    let (delivery_receiver, _) = server.subscribe(ClientId::new(), 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();

    server.resync_lagged(Instant::now());

    assert_eq!(server.event_bus.count_subscribers(), 0);
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(list_subscription_pairs(&server), Vec::new());
}

#[test]
fn pushing_frames_serves_every_client() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_client_through_server(&mut server, local_client_id, REMOTE_VIEWPORT_SIZE, None);
    let (local_receiver, _) = server.subscribe(local_client_id, 60);
    let (remote_receiver, _) = server.subscribe(remote_client_id, 60);
    let local_frame = server.build_snapshot(local_client_id).expect("frame");
    let remote_frame = server.build_snapshot(remote_client_id).expect("frame");
    assert_eq!(
        local_frame.client_snapshot.viewport_size,
        TEST_VIEWPORT_SIZE
    );
    assert_eq!(
        remote_frame.client_snapshot.viewport_size,
        REMOTE_VIEWPORT_SIZE
    );

    server.push_frames(Instant::now());

    assert_eq!(
        local_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(local_frame))]
    );
    assert_eq!(
        remote_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(remote_frame))]
    );
}

#[test]
fn a_host_write_queues_its_wake_ahead_of_the_next_frame() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, writer_handoff) = server.subscribe(client_id, 60);
    // An OSC 52 copy of "hello", as `copy_to_clipboard` queues it.
    assert!(server.queue_host_write(client_id, &HostWrite::from_clipboard_copy("c", b"hello")));
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.push_frames(Instant::now());

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::HostWrite,
            Delivery::Frame(Box::new(expected_render_snapshot)),
        ]
    );
    assert_eq!(
        writer_handoff.take_host_write_bytes(),
        b"\x1b]52;c;aGVsbG8=\x07"
    );
}

#[test]
fn a_host_write_for_a_client_with_no_subscriber_is_dropped() {
    let (mut server, client_id) = boot_server();

    assert!(!server.queue_host_write(client_id, &HostWrite::from_clipboard_copy("c", b"hello")));

    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    assert_eq!(server.take_host_writes(client_id, &delivery_receiver), None);
}

#[test]
fn a_detach_leaves_the_waiting_bytes_to_the_connection_writer_and_adds_none() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_client_through_server(&mut server, local_client_id, TEST_VIEWPORT_SIZE, None);
    let (delivery_receiver, writer_handoff) = server.subscribe(remote_client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    assert!(server.queue_host_write(
        remote_client_id,
        &HostWrite::from_clipboard_copy("c", b"one")
    ));
    assert!(server.queue_host_write(
        remote_client_id,
        &HostWrite::from_clipboard_copy("p", b"two")
    ));

    let _ = server.handle_client_detach(remote_client_id);

    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert!(!server.queue_host_write(
        remote_client_id,
        &HostWrite::from_clipboard_copy("c", b"three")
    ));
    assert_eq!(
        delivery_receiver
            .try_iter()
            .filter(|delivery| *delivery == Delivery::HostWrite)
            .count(),
        1
    );
    assert_eq!(
        writer_handoff.take_host_write_bytes(),
        b"\x1b]52;c;b25l\x07\x1b]52;p;dHdv\x07"
    );
}

#[test]
fn pushing_frames_serves_no_client_that_detached() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_client_through_server(&mut server, local_client_id, TEST_VIEWPORT_SIZE, None);
    let (remote_receiver, _) = server.subscribe(remote_client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;

    // The detach takes the subscription with the client record, so the push
    // finds nobody to build a frame for.
    let _ = server.handle_client_detach(remote_client_id);
    server.push_frames(Instant::now());

    assert_eq!(list_subscription_pairs(&server), Vec::new());
    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert_eq!(
        remote_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_frame_for_a_gone_receiver_costs_that_subscription_its_recorded_client() {
    let (mut server, client_id) = boot_server();
    let (kept_receiver, _) = server.subscribe(client_id, 60);
    let (gone_receiver, _) = server.subscribe(client_id, 60);
    let kept_subscriber_id = server.subscriptions[0].subscriber_id;
    let gone_subscriber_id = server.subscriptions[1].subscriber_id;
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    drop(gone_receiver);

    server.push_frames(Instant::now());

    assert!(!server.event_bus.has_subscriber(gone_subscriber_id));
    assert!(server.event_bus.has_subscriber(kept_subscriber_id));
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(kept_subscriber_id, client_id)]
    );
    assert_eq!(
        kept_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected_render_snapshot))]
    );
}

#[test]
fn a_frame_blocked_by_a_full_queue_leaves_the_subscription_in_place() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    // Free one slot and spend it on the resync frame: the subscriber is live
    // again, with a queue that is full again.
    let _oldest_delivery: Delivery = delivery_receiver.recv().expect("queued event");
    let resync_time = Instant::now();
    server.resync_lagged(resync_time);
    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    server.mark_client_view_changed(client_id);
    let frame_time = resync_time + DEFAULT_FRAME_INTERVAL_DURATION;

    server.push_frames(frame_time);

    assert_eq!(
        list_subscription_pairs(&server),
        vec![(subscriber_id, client_id)]
    );
    assert!(server.event_bus.has_subscriber(subscriber_id));
    let mut expected_deliveries =
        vec![Delivery::Event(Event::Quit(QuitCause::Requested)); SUBSCRIBER_QUEUE_CAPACITY - 1];
    expected_deliveries.push(Delivery::Snapshot {
        render_snapshot: Box::new(server.build_snapshot(client_id).expect("frame")),
        lag_report: SubscriberLagged {
            subscriber_id,
            dropped_event_count: 1,
        },
    });
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        expected_deliveries,
        "the frame did not fit, so none was queued"
    );
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, frame_time),
        Some(Duration::ZERO),
        "the frame that did not fit leaves the view change in place"
    );
}

#[test]
fn a_paused_subscriber_whose_view_changed_sets_no_wakeup() {
    let (mut server, client_id) = boot_server();
    let (_delivery_receiver, _) = server.subscribe(client_id, 60);
    pause_subscribers(&mut server);
    let current_time = Instant::now();

    assert_eq!(
        mark_changed_views_then_compute_frame_wait(&mut server, client_id, current_time),
        Some(Duration::ZERO)
    );
    assert_eq!(server.compute_next_render_wakeup(current_time), None);
}

/// Publish one `TabCreated` event [`SUBSCRIBER_QUEUE_CAPACITY`] times, which
/// fills the queue of every subscriber whose queue was empty and leaves it
/// live. Returns the event.
fn fill_subscriber_queues(server: &mut Server) -> Event {
    let tab_created_event = Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    });
    for _ in 0..SUBSCRIBER_QUEUE_CAPACITY {
        server.event_bus.publish(&tab_created_event);
    }
    tab_created_event
}

#[test]
fn freed_slots_on_two_full_queues_give_each_client_one_frame_and_owe_nothing_more() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_client_through_server(&mut server, local_client_id, REMOTE_VIEWPORT_SIZE, None);
    let (local_receiver, local_writer_handoff) = server.subscribe(local_client_id, 60);
    let (remote_receiver, remote_writer_handoff) = server.subscribe(remote_client_id, 60);
    let tab_created_delivery = Delivery::Event(fill_subscriber_queues(&mut server));
    // The first frames find both queues full: both frames are dropped, and
    // both subscribers are owed a frame.
    let render_time = Instant::now();
    server.push_frames(render_time);
    let local_frame = server.build_snapshot(local_client_id).expect("frame");
    let remote_frame = server.build_snapshot(remote_client_id).expect("frame");

    // Each writing thread reads one delivery, takes its mark, and wakes the
    // session, remote first. Each wake runs one pass of the serve loop, one
    // frame interval after the last.
    let wakes = [
        (&remote_receiver, &remote_writer_handoff),
        (&local_receiver, &local_writer_handoff),
    ];
    for (pass_number, (delivery_receiver, writer_handoff)) in (1..).zip(wakes) {
        assert_eq!(delivery_receiver.recv(), Ok(tab_created_delivery.clone()));
        assert!(writer_handoff.take_owed_frame_mark());
        let control_flow = server.handle_runtime_event(RuntimeEvent::QueueRoomFreed);
        assert_eq!(control_flow, std::ops::ControlFlow::Continue(()));
        server.push_due_frames(render_time + DEFAULT_FRAME_INTERVAL_DURATION * pass_number);
    }

    assert!(
        !remote_writer_handoff.take_owed_frame_mark(),
        "the local wake offers the full remote queue no frame"
    );
    assert!(!local_writer_handoff.take_owed_frame_mark());
    let mut expected_local_deliveries =
        vec![tab_created_delivery.clone(); SUBSCRIBER_QUEUE_CAPACITY - 1];
    expected_local_deliveries.push(Delivery::Frame(Box::new(local_frame)));
    assert_eq!(
        local_receiver.try_iter().collect::<Vec<_>>(),
        expected_local_deliveries
    );
    let mut expected_remote_deliveries = vec![tab_created_delivery; SUBSCRIBER_QUEUE_CAPACITY - 1];
    expected_remote_deliveries.push(Delivery::Frame(Box::new(remote_frame)));
    assert_eq!(
        remote_receiver.try_iter().collect::<Vec<_>>(),
        expected_remote_deliveries
    );
}

#[test]
fn a_freed_slot_taken_before_the_frame_owes_the_subscriber_a_frame_again() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, writer_handoff) = server.subscribe(client_id, 60);
    let tab_created_event = fill_subscriber_queues(&mut server);
    let tab_created_delivery = Delivery::Event(tab_created_event.clone());
    let frame_time = Instant::now();
    server.push_frames(frame_time);
    assert_eq!(delivery_receiver.recv(), Ok(tab_created_delivery.clone()));
    assert!(writer_handoff.take_owed_frame_mark());
    // An event takes the freed slot before the session reads the wake.
    server.event_bus.publish(&tab_created_event);

    let _ = server.handle_runtime_event(RuntimeEvent::QueueRoomFreed);
    server.push_due_frames(frame_time);

    assert!(writer_handoff.take_owed_frame_mark());
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![tab_created_delivery; SUBSCRIBER_QUEUE_CAPACITY]
    );
}

#[test]
fn a_freed_slot_for_a_paused_subscriber_takes_the_resync_snapshot_and_no_frame() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, writer_handoff) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    let frame_time = Instant::now();
    server.resync_lagged(frame_time);
    let _oldest_delivery: Delivery = delivery_receiver.recv().expect("queued event");
    assert!(writer_handoff.take_owed_frame_mark());
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    let _ = server.handle_runtime_event(RuntimeEvent::QueueRoomFreed);
    server.push_due_frames(frame_time);

    let mut expected_deliveries =
        vec![Delivery::Event(Event::Quit(QuitCause::Requested)); SUBSCRIBER_QUEUE_CAPACITY - 1];
    expected_deliveries.push(Delivery::Snapshot {
        render_snapshot: Box::new(expected_render_snapshot),
        lag_report: SubscriberLagged {
            subscriber_id,
            dropped_event_count: 1,
        },
    });
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        expected_deliveries
    );
    assert!(!writer_handoff.take_owed_frame_mark());
}

#[test]
fn a_render_offers_a_subscriber_owed_a_frame_nothing_until_its_writer_takes_the_mark() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, writer_handoff) = server.subscribe(client_id, 60);
    let tab_created_delivery = Delivery::Event(fill_subscriber_queues(&mut server));
    let frame_time = Instant::now();
    server.push_frames(frame_time);
    // The writing thread reads one delivery and has not taken its mark yet.
    assert_eq!(delivery_receiver.recv(), Ok(tab_created_delivery.clone()));

    server.push_frames(frame_time);
    let owed_deliveries: Vec<Delivery> = delivery_receiver.try_iter().collect();
    assert!(writer_handoff.take_owed_frame_mark());
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    server.push_frames(frame_time);

    assert_eq!(
        owed_deliveries,
        vec![tab_created_delivery; SUBSCRIBER_QUEUE_CAPACITY - 1]
    );
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected_render_snapshot))]
    );
}

#[test]
fn a_paused_subscriber_owed_a_frame_is_not_resynced_until_its_writer_takes_the_mark() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, writer_handoff) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    server.resync_lagged(Instant::now());
    // The writing thread reads one delivery and has not taken its mark yet.
    let _oldest_delivery: Delivery = delivery_receiver.recv().expect("queued event");

    server.resync_lagged(Instant::now());

    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested)); SUBSCRIBER_QUEUE_CAPACITY - 1]
    );
    assert!(writer_handoff.take_owed_frame_mark());
}

#[test]
fn a_push_queues_nothing_for_a_gone_client_and_drops_a_gone_receivers_subscription() {
    let (mut server, client_id) = boot_server();
    // No session holds this id, so no frame can ever be built for it.
    let unknown_client_id = ClientId::new();
    let (unknown_client_receiver, _) = server.subscribe(unknown_client_id, 60);
    let (gone_receiver, _) = server.subscribe(client_id, 60);
    let unknown_client_subscriber_id = server.subscriptions[0].subscriber_id;
    let gone_subscriber_id = server.subscriptions[1].subscriber_id;
    drop(gone_receiver);
    let frame_time = Instant::now();

    server.push_frames(frame_time);

    assert_eq!(
        unknown_client_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert!(!server.event_bus.has_subscriber(gone_subscriber_id));
    assert_eq!(
        list_subscription_pairs(&server),
        vec![(unknown_client_subscriber_id, unknown_client_id)]
    );
    assert_eq!(
        server.compute_next_render_wakeup(frame_time),
        None,
        "a client the session shows no frame for is owed none"
    );
}

#[test]
fn constructor_starts_on_an_empty_app_layer_and_the_built_in_defaults() {
    let (runtime_event_sender, inbox_receiver) = mpsc::channel();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));

    let server = Server::from_runtime_parts(pty_backend, inbox_receiver);

    // The constructor reads nothing from disk and holds no settings.
    // `load_startup_config` loads the first `koshi.kdl`.
    assert_eq!(server.app_layer, PartialKoshiConfig::default());
    assert_eq!(server.config, ServerConfig::default());
    assert_eq!(server.client_config, ClientConfig::default());
}

/// The one session a booted server holds, and the tab and root pane its client
/// is looking at.
fn get_booted_parts(server: &Server, client_id: ClientId) -> (SessionId, TabId, PaneId) {
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
        .get_active_tab_id();
    let pane_id = session.tabs[&tab_id]
        .list_focus_mru()
        .first()
        .copied()
        .expect("the tab's root pane");
    (session_id, tab_id, pane_id)
}

/// The view a booted client files: `booted_tab_id` active with `root_pane_id`
/// focused, no zoom, `scroll_offset_by_pane_id` as its scroll positions, and no
/// floating pane view.
fn build_booted_saved_view(
    booted_tab_id: TabId,
    root_pane_id: PaneId,
    scroll_offset_by_pane_id: HashMap<PaneId, usize>,
) -> SavedView {
    SavedView {
        active_tab_id: booted_tab_id,
        focus_history_by_tab_id: HashMap::from([(booted_tab_id, vec![root_pane_id])]),
        zoomed_pane_id_by_tab_id: HashMap::new(),
        scroll_offset_by_pane_id,
        floating_pane_view_by_pane_id: HashMap::new(),
        shown_floating_pane_ids: Vec::new(),
    }
}

/// Add a tab named `second` to `session_id`, holding one new pane record.
/// Returns the tab and its pane.
fn add_additional_tab(server: &mut Server, session_id: SessionId) -> (TabId, PaneId) {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session = server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session");
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
        .expect("a fresh pane id");
    let tab_index = session.tabs.len();
    session.tabs.insert(
        tab_id,
        build_tab(tab_id, "second", tab_index, LayoutNode::Pane(pane_id)),
    );
    (tab_id, pane_id)
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

// A binary that the kernel refuses to exec fails the check before the swap
// starts.
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
    let stranded_pane_id = PaneId::new();
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
            pane_id: stranded_pane_id,
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
            "pane {stranded_pane_id} has no terminal descriptor, so its terminal cannot cross the swap"
        ))
    );
    assert_eq!(can_carry_panes(&panes[..1]), Ok(()));
    // A session holding no pane holds no restart back either.
    assert_eq!(can_carry_panes(&[]), Ok(()));
}

// Windows keeps every pane's pseudoconsole in the supervisor process, which
// outlives the swap. The pane check refuses no pane on Windows.
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
fn a_restart_refusal_reads_as_its_sentence() {
    assert_eq!(
        RestartRefusal::UnfitProgramFile {
            refusal_reason: "the binary at /x is not executable".to_string(),
        }
        .to_string(),
        "the binary at /x is not executable"
    );
    assert_eq!(
        RestartRefusal::PaneNotReady {
            refusal_reason: "pane 3 has no terminal descriptor".to_string(),
        }
        .to_string(),
        "pane 3 has no terminal descriptor"
    );
    assert_eq!(
        RestartRefusal::ImageReplacementUnsupported.to_string(),
        "this koshi cannot replace its own image, so it cannot restart"
    );
}

#[test]
fn only_a_pane_not_ready_can_end_while_the_program_file_stays_the_same() {
    assert_eq!(
        [
            RestartRefusal::UnfitProgramFile {
                refusal_reason: String::new(),
            },
            RestartRefusal::PaneNotReady {
                refusal_reason: String::new(),
            },
            RestartRefusal::ImageReplacementUnsupported,
        ]
        .map(|restart_refusal| restart_refusal.can_end_without_file_change()),
        [false, true, false]
    );
}

#[test]
fn a_taken_restart_raises_the_flag_and_changes_nothing_else() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    let (response_sender, response_receiver) = mpsc::channel();

    let _ = server.handle_runtime_event(RuntimeEvent::IpcRestart { response_sender });

    response_receiver
        .try_recv()
        .expect("the dispatcher answers the restart");
    assert!(server.is_restart_requested());
    assert!(!server.is_quit_requested());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
    assert_eq!(server.live_pane_ids.len(), 1);
}

#[test]
fn a_restart_taken_back_lowers_the_flag_and_the_next_one_is_taken_again() {
    let (mut server, _client_id) = boot_server();
    let _ = server.handle_runtime_event(RuntimeEvent::IpcRestart {
        response_sender: mpsc::channel().0,
    });
    assert!(server.is_restart_requested());

    server.cancel_restart();

    assert!(!server.is_restart_requested());
    assert!(!server.is_quit_requested());
    let _ = server.handle_runtime_event(RuntimeEvent::IpcRestart {
        response_sender: mpsc::channel().0,
    });
    assert!(server.is_restart_requested());
}

#[test]
fn an_attach_claiming_a_carried_client_keeps_its_id_zoom_focus_and_tab() {
    let (mut server, client_id) = boot_server();
    let (session_id, tab_id, pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    {
        let client = server.get_client_mut(client_id).expect("the booted client");
        client.update_focused_pane(tab_id, pane_id);
        client.zoom_pane(tab_id, pane_id);
        client.set_scroll_offset(pane_id, 7);
        // The client was looking at the second tab when the image was replaced.
        client.update_active_tab_id(additional_tab_id);
    }
    let resume_token = server.saved_view_store.mint_resume_token(client_id);
    server.client_ids_awaiting_reconnect.insert(client_id);

    let attachment = server
        .handle_ipc_attach(
            Some(client_id),
            Some(resume_token),
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session hands the record back");

    assert_eq!(attachment.client_id, client_id);
    assert_eq!(attachment.session_id, session_id);
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("the same record");
    assert_eq!(client.get_active_tab_id(), additional_tab_id);
    assert_eq!(client.get_focused_pane_id(tab_id), Some(pane_id));
    assert_eq!(client.get_zoomed_pane_id(tab_id), Some(pane_id));
    assert_eq!(client.get_scroll_offset(pane_id), 7);
    assert_eq!(client.get_viewport_size(), REMOTE_VIEWPORT_SIZE);
}

/// The attach reply hands back the report the session recorded.
#[test]
fn handle_ipc_attach_echoes_the_stored_pane_area() {
    let (mut server, _client_id) = boot_server();

    let starving_attachment = server
        .handle_ipc_attach(
            None,
            None,
            REMOTE_VIEWPORT_SIZE,
            Some(PaneArea::Starving),
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session mints a client");
    assert_eq!(starving_attachment.pane_area, Some(PaneArea::Starving));

    let unreported_attachment = server
        .handle_ipc_attach(
            None,
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session mints a client");
    assert_eq!(unreported_attachment.pane_area, None);
}

#[test]
fn an_attach_claiming_a_client_this_session_does_not_hold_mints_a_new_one() {
    let (mut server, client_id) = boot_server();
    let (session_id, tab_id, _pane_id) = get_booted_parts(&server, client_id);
    let stranger_client_id = ClientId::new();

    let attachment = server
        .handle_ipc_attach(
            Some(stranger_client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session mints a client instead of refusing");

    assert_ne!(attachment.client_id, stranger_client_id);
    assert_ne!(attachment.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 2);
    let minted_client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(attachment.client_id)
        .expect("the minted record");
    assert_eq!(minted_client.get_active_tab_id(), tab_id);
}

#[test]
fn an_attach_claiming_a_client_a_connection_is_streaming_for_mints_a_new_one() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    // The first attach takes the record with its token and holds its queue.
    // The record is in use when the second attach names it.
    let resume_token = server.saved_view_store.mint_resume_token(client_id);
    let held_attachment = server
        .handle_ipc_attach(
            Some(client_id),
            Some(resume_token),
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the first attach takes the record");
    assert_eq!(held_attachment.client_id, client_id);

    let newly_minted_attachment = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the second attach mints a client instead of refusing");

    assert_ne!(newly_minted_attachment.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 2);
    // The client already streaming keeps its record and its own subscription:
    // a second caller naming the same id takes neither.
    let viewed_client_ids: Vec<ClientId> = server
        .subscriptions
        .iter()
        .map(|frame_subscription| frame_subscription.client_id)
        .collect();
    assert_eq!(
        viewed_client_ids.len(),
        2,
        "each attach holds one subscription"
    );
    assert_eq!(
        viewed_client_ids
            .iter()
            .filter(|&&viewed_client_id| viewed_client_id == client_id)
            .count(),
        1,
        "the claimed record is streamed for by exactly one connection"
    );
    assert_eq!(
        viewed_client_ids
            .iter()
            .filter(|&&viewed_client_id| viewed_client_id == newly_minted_attachment.client_id)
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
    let (mut server, client_id) = boot_server();
    let (session_id, tab_id, _pane_id) = get_booted_parts(&server, client_id);

    let attachment = server
        .handle_ipc_attach(
            None,
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session mints a client");

    assert_ne!(attachment.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 2);
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(attachment.client_id)
            .expect("the minted record")
            .get_active_tab_id(),
        tab_id
    );
}

/// A second pane in the tab the booted client views, split rightward from that
/// tab's root pane, so zooming one pane changes the size the tab's panes solve
/// to. Returns the new pane's id.
fn split_booted_pane(server: &mut Server, client_id: ClientId, root_pane_id: PaneId) -> PaneId {
    let session_id = *server
        .session_by_id
        .keys()
        .next()
        .expect("the booted session");
    let command_result = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: None,
                tab_id: None,
                direction: Direction::Right,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    ));
    assert_eq!(
        get_command_outcome(&command_result),
        Ok(vec![
            "PaneCreated",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized",
            "PtyResized"
        ]),
        "the split ran"
    );
    let added_pane_ids: Vec<PaneId> = server.session_by_id[&session_id]
        .panes
        .list_pane_records()
        .map(PaneRecord::get_pane_id)
        .filter(|&pane_id| pane_id != root_pane_id)
        .collect();
    assert_eq!(added_pane_ids.len(), 1, "the split added exactly one pane");
    added_pane_ids[0]
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
            None,
            attached_at,
            false,
            60,
            None,
        )
        .expect("the session mints a client")
}

#[test]
fn an_attach_presenting_no_token_still_mints_one_and_files_no_view() {
    let (mut server, _client_id) = boot_server();
    let now = SystemTime::now();

    let attachment = attach_with_token(&mut server, None, now);

    assert_eq!(
        attachment.resume_token.expose_secret().len(),
        64,
        "a minted token is 32 random bytes written as hex"
    );
    // Nothing has detached, so the token this attach minted takes back nothing.
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&attachment.resume_token, now),
        None
    );
}

#[test]
fn a_token_takes_back_the_tab_focus_zoom_and_scroll_the_client_left() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root_pane_id);
    let (additional_tab_id, additional_pane_id) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    // 600 newlines on a 24-row viewport retain at least 576 lines, so the offset
    // of 500 the view files stands inside the split pane's history.
    server.handle_pty_output(split_pane_id, &b"\n".repeat(600));
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.update_focused_pane(additional_tab_id, additional_pane_id);
        client.zoom_pane(booted_tab_id, root_pane_id);
        client.set_scroll_offset(split_pane_id, 500);
        client.update_active_tab_id(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    assert_ne!(returning_attachment.client_id, leaving_attachment.client_id);
    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_active_tab_id(), additional_tab_id);
    assert_eq!(
        client.get_focused_pane_id(booted_tab_id),
        Some(root_pane_id)
    );
    assert_eq!(
        client.get_focused_pane_id(additional_tab_id),
        Some(additional_pane_id)
    );
    assert_eq!(
        client.get_layout_mode(booted_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    assert_eq!(client.get_layout_mode(additional_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_scroll_offset(split_pane_id), 500);
}

#[test]
fn a_token_whose_focused_pane_moved_to_another_tab_keeps_the_focus_the_attach_gave() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root_pane_id);
    let (additional_tab_id, _additional_pane_id) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, split_pane_id);
        client.zoom_pane(booted_tab_id, split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let placement_result = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::PlacePane(PlacePaneArgs {
            source_pane_id: Some(split_pane_id),
            placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Split {
                destination_tab_id: additional_tab_id,
                anchor: PanePlacementAnchor::Tab,
                direction: Direction::Right,
            }),
        }),
    ));
    assert_eq!(
        get_command_outcome(&placement_result),
        Ok(vec![
            "PanePlacementCommitted",
            "LayoutChanged",
            "LayoutChanged",
            "TabFocused",
            "PaneFocused",
            "PaneFocused"
        ]),
        "the placement ran"
    );

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(
        client.get_focused_pane_id(booted_tab_id),
        Some(root_pane_id)
    );
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
}

#[test]
fn a_restored_view_announces_the_focus_it_puts_back() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root_pane_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session");
        // An attach that restores nothing lands on the tab's first leaf,
        // `root_pane_id`. The leaving client focuses `split_pane_id`.
        session
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
            .expect("the client that is about to leave")
            .update_focused_pane(booted_tab_id, split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let (watcher_receiver, _) = server.subscribe(client_id, 60);

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let pane_focused_events: Vec<PaneFocused> = watcher_receiver
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::Event(Event::PaneFocused(pane_focused)) => Some(pane_focused),
            _ => None,
        })
        .filter(|pane_focused| pane_focused.client_id == returning_attachment.client_id)
        .collect();
    assert_eq!(
        pane_focused_events,
        vec![
            PaneFocused {
                client_id: returning_attachment.client_id,
                tab_id: Some(booted_tab_id),
                pane_id: root_pane_id,
                previous_pane_id: None,
            },
            PaneFocused {
                client_id: returning_attachment.client_id,
                tab_id: Some(booted_tab_id),
                pane_id: split_pane_id,
                previous_pane_id: Some(root_pane_id),
            },
        ],
        "the attach lands on the tab's first leaf, then the restored view moves the focus back"
    );
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(returning_attachment.client_id)
            .expect("the client the token attached")
            .list_focus_history(booted_tab_id),
        [split_pane_id, root_pane_id]
    );
}

#[test]
fn a_token_takes_back_the_floating_view_and_the_floating_pane_that_takes_the_input() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    let pinned_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(leaving_attachment.client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let minimized_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(leaving_attachment.client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let focused_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(leaving_attachment.client_id),
        build_floating_new_pane_args(None, None, false),
    );
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.pin_floating_pane(pinned_pane_id, Point { column: 4, row: 2 });
        assert!(client.minimize_floating_pane(minimized_pane_id));
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let (watcher_receiver, _) = server.subscribe(client_id, 60);

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let session = &server.session_by_id[&session_id];
    let client = session
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(
        client.list_floating_pane_views(),
        &HashMap::from([
            (
                pinned_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Pinned(Point { column: 4, row: 2 }),
                    is_minimized: false,
                },
            ),
            (
                minimized_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Default,
                    is_minimized: true,
                },
            ),
            (
                focused_pane_id,
                FloatingPaneView {
                    position: FloatingPanePosition::Default,
                    is_minimized: false,
                },
            ),
        ])
    );
    assert_eq!(
        client.list_shown_floating_pane_ids(),
        [pinned_pane_id, focused_pane_id]
    );
    assert_eq!(client.get_focused_floating_pane_id(), Some(focused_pane_id));
    assert_eq!(session.validate_session_consistency(), Ok(()));
    assert_eq!(
        server.find_typed_pane(returning_attachment.client_id),
        Some(focused_pane_id)
    );
    let pane_focused_events: Vec<PaneFocused> = watcher_receiver
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::Event(Event::PaneFocused(pane_focused)) => Some(pane_focused),
            _ => None,
        })
        .filter(|pane_focused| pane_focused.client_id == returning_attachment.client_id)
        .collect();
    assert_eq!(
        pane_focused_events,
        vec![
            PaneFocused {
                client_id: returning_attachment.client_id,
                tab_id: Some(booted_tab_id),
                pane_id: root_pane_id,
                previous_pane_id: None,
            },
            PaneFocused {
                client_id: returning_attachment.client_id,
                tab_id: None,
                pane_id: focused_pane_id,
                previous_pane_id: Some(root_pane_id),
            },
        ],
        "the attach lands on the tab's pane, then the restored floating pane takes the input"
    );
}

#[test]
fn a_token_drops_a_floating_pane_closed_since_the_detach_and_moves_its_input() {
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, _root_pane_id) = get_booted_parts(&server, client_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    let staying_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(leaving_attachment.client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let closing_pane_id = create_floating_pane(
        &mut server,
        CommandSource::from_key_binding(leaving_attachment.client_id),
        build_floating_new_pane_args(None, None, false),
    );
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let _ = server.handle_child_exit(closing_pane_id, ExitStatus::ExitCode(0));

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let session = &server.session_by_id[&session_id];
    let client = session
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(
        client.list_floating_pane_views(),
        &HashMap::from([(
            staying_pane_id,
            FloatingPaneView {
                position: FloatingPanePosition::Default,
                is_minimized: false,
            },
        )])
    );
    assert_eq!(client.list_shown_floating_pane_ids(), [staying_pane_id]);
    assert_eq!(client.get_focused_floating_pane_id(), Some(staying_pane_id));
    assert_eq!(session.validate_session_consistency(), Ok(()));
    assert_eq!(
        server.find_typed_pane(returning_attachment.client_id),
        Some(staying_pane_id)
    );
}

#[test]
fn a_token_whose_pane_lost_its_history_comes_back_at_the_live_bottom() {
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root_pane_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    server.handle_pty_output(split_pane_id, &b"\n".repeat(600));
    server
        .get_client_mut(leaving_attachment.client_id)
        .expect("the client that is about to leave")
        .set_scroll_offset(split_pane_id, 500);
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
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

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_scroll_offset(split_pane_id), 0);
    assert!(!client.is_view_held(split_pane_id));
}

#[test]
fn the_same_token_twice_takes_the_view_back_once() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root_pane_id);
        client.update_active_tab_id(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let resume_token = leaving_attachment.resume_token;

    let restored_attachment =
        attach_with_token(&mut server, Some(resume_token.clone()), detached_at);
    let newly_minted_attachment = attach_with_token(&mut server, Some(resume_token), detached_at);

    let clients = &server.session_by_id[&session_id].clients;
    let restored_client = clients
        .get_client_by_id(restored_attachment.client_id)
        .expect("the restored client");
    assert_eq!(restored_client.get_active_tab_id(), additional_tab_id);
    assert_eq!(
        restored_client.get_layout_mode(booted_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: root_pane_id
        }
    );
    let newly_minted_client = clients
        .get_client_by_id(newly_minted_attachment.client_id)
        .expect("the newly minted client");
    assert_eq!(newly_minted_client.get_active_tab_id(), booted_tab_id);
    assert_eq!(
        newly_minted_client.get_layout_mode(booted_tab_id),
        LayoutMode::Tiled
    );
}

#[test]
fn a_token_presented_one_second_after_its_24_hours_takes_nothing_back() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root_pane_id);
        client.update_active_tab_id(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at + Duration::from_secs(24 * 60 * 60 + 1),
    );

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the minted client");
    assert_eq!(client.get_active_tab_id(), booted_tab_id);
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
}

#[test]
fn a_view_whose_tab_was_closed_while_it_stood_comes_back_on_the_first_tab() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, _root) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    server
        .get_client_mut(leaving_attachment.client_id)
        .expect("the client that is about to leave")
        .update_active_tab_id(additional_tab_id);
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .tabs
        .remove(&additional_tab_id);

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(returning_attachment.client_id)
            .expect("the client the token attached")
            .get_active_tab_id(),
        booted_tab_id
    );
}

#[test]
fn a_view_whose_zoomed_pane_was_closed_while_it_stood_comes_back_tiled() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root_pane_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.zoom_pane(booted_tab_id, split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let close_result = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(split_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    ));
    assert_eq!(
        get_command_outcome(&close_result),
        Ok(vec![
            "PaneClosing",
            "PaneRemoved",
            "LayoutChanged",
            "PaneFocused",
            "PtyResized"
        ]),
        "the close ran"
    );

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
    assert_eq!(
        client.get_focused_pane_id(booted_tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn a_view_whose_focused_pane_was_closed_while_it_stood_comes_back_unfocused_there() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, additional_pane_id) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.update_focused_pane(additional_tab_id, additional_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let close_result = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(additional_pane_id),
            should_force_close: true,
            should_kill_process_tree: false,
        }),
    ));
    assert_eq!(
        get_command_outcome(&close_result),
        Ok(vec!["PaneClosing", "PaneRemoved", "TabClosed"]),
        "the close ran"
    );

    let returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the client the token attached");
    assert_eq!(client.get_focused_pane_id(additional_tab_id), None);
    assert_eq!(
        client.get_focused_pane_id(booted_tab_id),
        Some(root_pane_id)
    );
}

#[test]
fn taking_a_zoom_back_resizes_the_tabs_panes() {
    let (mut server, client_id) = boot_server();
    let (_, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let _split = split_booted_pane(&mut server, client_id, root_pane_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    // A tab's panes are solved once per viewer, and each pane takes its
    // smallest rect across them. The client whose zoom comes back is the tab's
    // only viewer, and it stays tiled until the restore zooms it.
    let _ = server.handle_client_detach(client_id);
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.zoom_pane(booted_tab_id, root_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let (delivery_receiver, _) = server.subscribe(ClientId::new(), 60);

    let _returning_attachment = attach_with_token(
        &mut server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    let pty_resized_events: Vec<Event> = delivery_receiver
        .try_iter()
        .filter_map(|delivery| match delivery {
            Delivery::Event(event @ Event::PtyResized(_)) => Some(event),
            _ => None,
        })
        .collect();
    // The zoomed pane fills the tab: 80x24 less the tabline and hint rows is
    // 80x22, less the one-cell border on each side is 78x20.
    assert_eq!(
        pty_resized_events,
        vec![Event::PtyResized(PtyResized {
            pane_id: root_pane_id,
            pty_size: PtySize {
                column_count: 78,
                row_count: 20
            },
        })]
    );
}

#[test]
fn a_client_the_restart_grace_still_holds_files_no_view_and_keeps_its_token() {
    let (mut server, client_id) = boot_server();
    let (_, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    {
        let client = server.get_client_mut(client_id).expect("the booted client");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.set_scroll_offset(root_pane_id, 500);
    }
    let resume_token = server.saved_view_store.mint_resume_token(client_id);
    let now = SystemTime::now();
    server.client_ids_awaiting_reconnect.insert(client_id);

    server.save_client_view(client_id, now);

    assert_eq!(
        server.saved_view_store.take_saved_view(&resume_token, now),
        None
    );
    // The hash still stands, so the restart path decides this record's fate.
    server.client_ids_awaiting_reconnect.remove(&client_id);
    server.save_client_view(client_id, now);
    assert_eq!(
        server.saved_view_store.take_saved_view(&resume_token, now),
        Some(build_booted_saved_view(
            booted_tab_id,
            root_pane_id,
            HashMap::from([(root_pane_id, 500)])
        ))
    );
}

#[test]
fn a_detach_with_no_view_filed_leaves_its_token_taking_nothing_back() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .get_client_mut(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root_pane_id);
        client.update_active_tab_id(additional_tab_id);
    }
    let now = SystemTime::now();
    // The `core:detach` and `core:quit` path: the record goes with no view
    // filed for it.
    let _ = server.handle_client_detach(leaving_attachment.client_id);

    let returning_attachment =
        attach_with_token(&mut server, Some(leaving_attachment.resume_token), now);

    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(returning_attachment.client_id)
        .expect("the minted client");
    assert_eq!(client.get_active_tab_id(), booted_tab_id);
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_scroll_offset(root_pane_id), 0);
}

#[test]
fn an_attach_that_finds_no_session_spends_no_token() {
    let (mut server, client_id) = boot_server();
    let (_, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let now = SystemTime::now();
    let leaving_attachment = attach_with_token(&mut server, None, now);
    server
        .get_client_mut(leaving_attachment.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root_pane_id, 42);
    server.save_client_view(leaving_attachment.client_id, now);

    // The process is past its last session, so there is nothing to attach to.
    server.session_by_id.clear();
    let refused_attachment = server.handle_ipc_attach(
        None,
        Some(leaving_attachment.resume_token.clone()),
        TEST_VIEWPORT_SIZE,
        None,
        None,
        now,
        false,
        60,
        None,
    );

    assert!(refused_attachment.is_none(), "no session is left to join");
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&leaving_attachment.resume_token, now),
        Some(build_booted_saved_view(
            booted_tab_id,
            root_pane_id,
            HashMap::from([(root_pane_id, 42)])
        )),
        "the refused attach read no token and spent none"
    );
}

#[test]
fn a_connection_that_never_reached_its_stream_files_no_view() {
    let (mut server, client_id) = boot_server();
    let (_, _booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let now = SystemTime::now();
    let undelivered_attachment = attach_with_token(&mut server, None, now);
    server
        .get_client_mut(undelivered_attachment.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root_pane_id, 12);

    server
        .saved_view_store
        .forget_client_resume_token(undelivered_attachment.client_id);
    server.save_client_view(undelivered_attachment.client_id, now);

    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&undelivered_attachment.resume_token, now),
        None,
        "the token never reached that client, so its view is not filed"
    );
}

#[test]
fn a_client_the_session_no_longer_holds_files_no_view_and_drops_its_token() {
    let (mut server, client_id) = boot_server();
    let (_, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let detached_attachment = attach_with_token(&mut server, None, SystemTime::now());
    let now = SystemTime::now();
    let _ = server.handle_client_detach(detached_attachment.client_id);

    server.save_client_view(detached_attachment.client_id, now);

    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&detached_attachment.resume_token, now),
        None
    );
    // The store still takes the next mint and files against it.
    let next_attachment = attach_with_token(&mut server, None, now);
    server
        .get_client_mut(next_attachment.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root_pane_id, 500);
    server.save_client_view(next_attachment.client_id, now);
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&next_attachment.resume_token, now),
        Some(build_booted_saved_view(
            booted_tab_id,
            root_pane_id,
            HashMap::from([(root_pane_id, 500)])
        ))
    );
}

#[test]
fn a_resumed_server_starts_with_every_carried_client_awaiting_its_own_attach() {
    let (mut server, client_id) = boot_server();
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");

    let resumed_server = resume_carried_server(body, HashMap::new());

    assert_eq!(
        resumed_server.client_ids_awaiting_reconnect,
        HashSet::from([client_id])
    );
    assert!(!resumed_server.is_restart_requested());
}

#[test]
fn closing_the_grace_window_detaches_only_the_clients_that_never_came_back() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    let absent_client_id =
        attach_client_through_server(&mut server, client_id, TEST_VIEWPORT_SIZE, None);
    server.client_ids_awaiting_reconnect.insert(client_id);
    server
        .client_ids_awaiting_reconnect
        .insert(absent_client_id);
    let resume_token = server.saved_view_store.mint_resume_token(client_id);
    // One of the two came back before the window closed.
    let _held = server
        .handle_ipc_attach(
            Some(client_id),
            Some(resume_token),
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the record is handed back");

    server.handle_drop_unclaimed_clients(SystemTime::now());

    let clients = &server.session_by_id[&session_id].clients;
    assert_eq!(clients.count_clients(), 1);
    assert_eq!(
        clients
            .get_client_by_id(client_id)
            .map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(
        clients
            .get_client_by_id(absent_client_id)
            .map(|client| client.get_client_id()),
        None
    );
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn closing_the_grace_window_with_nobody_awaited_detaches_nobody() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);

    let emitted_events = server.handle_drop_unclaimed_clients(SystemTime::now());

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
}

#[test]
fn closing_the_grace_window_keeps_a_session_left_with_no_client_under_auto_close() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    server.config.should_auto_close_session = true;
    server.client_ids_awaiting_reconnect.insert(client_id);

    server.handle_drop_unclaimed_clients(SystemTime::now());

    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 0);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
    assert!(!server.is_quit_requested());
}

#[test]
fn a_quit_applied_before_the_swap_is_carried_to_the_next_image() {
    // A quit lands after the clients are told the session is restarting. The
    // swap runs to the end, and the carried state holds the quit for the next
    // image.
    let (mut server, _client_id) = boot_server();
    server.is_quit_requested = true;

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.carried_quit,
        Some(CarriedQuit::Graceful),
        "the carried state records the quit and its kind"
    );

    let resumed_server = resume_carried_server(body, HashMap::new());
    assert!(resumed_server.is_quit_requested());
    assert!(
        !resumed_server.should_shutdown_immediately,
        "a graceful quit stays graceful across the swap"
    );
}

#[test]
fn a_zero_grace_quit_is_still_zero_grace_after_the_swap() {
    // `request_quit` sets the flag and the kind together, and the carried state
    // holds both.
    let (mut server, _client_id) = boot_server();
    server.is_quit_requested = true;
    server.should_shutdown_immediately = true;

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(body.carried_quit, Some(CarriedQuit::Immediate));

    let resumed_server = resume_carried_server(body, HashMap::new());
    assert!(resumed_server.is_quit_requested());
    assert!(resumed_server.should_shutdown_immediately);
}

#[test]
fn a_session_that_still_expects_a_client_back_is_not_ended_by_a_carried_quit() {
    // The clients were told to come back: the quit waits for them. The grace
    // window that empties the set bounds the wait.
    let (mut server, client_id) = boot_server();
    let (_session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    server.is_quit_requested = true;
    server.client_ids_awaiting_reconnect.insert(ClientId::new());

    assert!(
        server.is_awaiting_client(),
        "a carried record is still unclaimed"
    );

    server.handle_drop_unclaimed_clients(SystemTime::now());

    assert!(
        !server.is_awaiting_client(),
        "the window closing is what lets the quit through"
    );
}

#[test]
fn a_swap_with_no_quit_behind_it_comes_back_serving() {
    let (mut server, _client_id) = boot_server();
    assert!(!server.is_quit_requested());

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(body.carried_quit, None);

    let resumed_server = resume_carried_server(body, HashMap::new());
    assert!(!resumed_server.is_quit_requested());
}

#[test]
fn a_detach_that_lands_while_a_client_is_awaited_leaves_its_record_alone() {
    // The connection of a client that was told the session is restarting ends,
    // and its detach arrives while the grace window holds that record. The
    // record stays until the window closes.
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    server.client_ids_awaiting_reconnect.insert(client_id);

    let emitted_events = server.handle_client_detach(client_id);

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert!(server.client_ids_awaiting_reconnect.contains(&client_id));

    // The window closing is what detaches it, and it takes the record with it.
    server.handle_drop_unclaimed_clients(SystemTime::now());

    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 0);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn the_restart_announcement_waits_for_every_client_to_hold_the_frame() {
    // `announce_restarting` returns once no client writing thread is left
    // running.
    let (mut server, _client_id) = boot_server();
    let ending_notice = Arc::clone(server.get_ending_notice());
    ending_notice.record_writer_started();
    let writer_ending_notice = Arc::clone(&ending_notice);
    let wait_started_at = Instant::now();
    let writer_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        writer_ending_notice.record_writer_ended();
    });

    server.announce_restarting();
    let waited_duration = wait_started_at.elapsed();

    assert_eq!(
        ending_notice.get_session_ending(),
        Some(SessionEnding::Restarting),
        "the notice must name the frame the clients are told"
    );
    assert_eq!(
        ending_notice.wait_until_writers_end(Instant::now()),
        0,
        "the call must return only once no writing thread is left"
    );
    assert!(
        waited_duration >= Duration::from_millis(150),
        "the call returned after {waited_duration:?}, before the writing thread ended"
    );
    writer_thread.join().expect("the writing thread ends");
}

#[test]
fn the_restart_announcement_gives_up_on_a_client_that_never_takes_the_frame() {
    // A client that stopped reading its socket leaves its writing thread
    // blocked inside the write. `announce_restarting` stops waiting for that
    // thread after `CLIENT_NOTIFICATION_TIMEOUT_DURATION`.
    let (mut server, _client_id) = boot_server();
    let ending_notice = Arc::clone(server.get_ending_notice());
    ending_notice.record_writer_started();

    let wait_started_at = Instant::now();
    server.announce_restarting();
    let waited_duration = wait_started_at.elapsed();

    assert_eq!(
        ending_notice.wait_until_writers_end(Instant::now()),
        1,
        "the writing thread that never ends must still be counted"
    );
    assert!(
        waited_duration >= CLIENT_NOTIFICATION_TIMEOUT_DURATION,
        "the call returned after {waited_duration:?}, before the limit"
    );
    assert!(
        waited_duration < CLIENT_NOTIFICATION_TIMEOUT_DURATION * 3,
        "the call waited {waited_duration:?}, well past the limit"
    );
}

#[test]
fn the_quit_announcement_tells_the_clients_the_session_ended() {
    // `announce_quit` publishes the quit to every subscriber and records the
    // quit frame on the ending notice.
    let (mut server, _client_id) = boot_server();
    let (_, delivery_receiver, _) = server.event_bus.subscribe();

    server.announce_quit();

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
    assert_eq!(
        server.get_ending_notice().get_session_ending(),
        Some(SessionEnding::Quit),
        "the notice must name the frame the clients are told"
    );
}

#[test]
fn the_quit_announcement_leaves_a_published_quit_as_the_only_one() {
    // Closing the last tab publishes the quit itself, which raises the notice.
    // The stream's last frame goes out once.
    let (mut server, _client_id) = boot_server();
    let (_, delivery_receiver, _) = server.event_bus.subscribe();
    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    server.announce_quit();

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn a_quit_announced_after_a_restart_keeps_the_restart_as_the_last_frame() {
    // The notice keeps the frame it was raised with first: the quit publishes
    // nothing.
    let (mut server, _client_id) = boot_server();
    let (_, delivery_receiver, _) = server.event_bus.subscribe();

    server.announce_restarting();
    server.announce_quit();

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Restarting)],
        "the restart frame must be the only one published"
    );
    assert_eq!(
        server.get_ending_notice().get_session_ending(),
        Some(SessionEnding::Restarting),
        "the notice must keep the frame the clients were told"
    );
}

#[test]
fn a_second_restart_taken_leaves_one_swap_asked_for() {
    // Two `koshi update` runs can reach one session before its loop reads the
    // flag. Each request is answered on its own, and the loop still exits into
    // exactly one swap.
    let (mut server, _client_id) = boot_server();
    let (first_response_sender, first_response_receiver) = mpsc::channel();
    let (second_response_sender, second_response_receiver) = mpsc::channel();

    let _ = server.handle_runtime_event(RuntimeEvent::IpcRestart {
        response_sender: first_response_sender,
    });
    let _ = server.handle_runtime_event(RuntimeEvent::IpcRestart {
        response_sender: second_response_sender,
    });

    first_response_receiver
        .try_recv()
        .expect("the first is answered");
    second_response_receiver
        .try_recv()
        .expect("the second is answered");
    assert!(server.is_restart_requested());
    server.cancel_restart();
    assert!(
        !server.is_restart_requested(),
        "one cancel takes the restart back, whatever the count of requests"
    );
}

#[test]
fn a_carried_client_that_never_came_back_is_detached_even_after_its_tab_was_closed() {
    // The grace window closes on a record whose tab was closed while the client
    // was away. The detach removes the record from the session.
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, _booted_pane_id) = get_booted_parts(&server, client_id);
    let (closed_tab_id, _) = add_additional_tab(&mut server, session_id);
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session");
        session
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client")
            .update_active_tab_id(closed_tab_id);
        session.tabs.remove(&closed_tab_id);
    }
    server.client_ids_awaiting_reconnect.insert(client_id);

    server.handle_drop_unclaimed_clients(SystemTime::now());

    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 0);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn after_a_swap_a_carried_client_comes_back_only_with_the_token_its_last_attach_minted() {
    // A viewer names the owner's client id after a swap, first with no token,
    // then with the viewer's own token. Each attach is a fresh client, and the
    // owner's token takes the owner's record back.
    let (mut server, booted_client_id) = boot_server();
    let (session_id, _booted_tab_id, _root_pane_id) = get_booted_parts(&server, booted_client_id);
    let owner_attachment = attach_with_token(&mut server, None, SystemTime::now());
    let viewer_attachment = attach_with_token(&mut server, None, SystemTime::now());
    let pty_size_by_pane_id = server.pty_size_by_pane_id.clone();
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    let mut resumed_server = resume_carried_server(body, pty_size_by_pane_id);

    let tokenless_attachment = resumed_server
        .handle_ipc_attach(
            Some(owner_attachment.client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            true,
            60,
            None,
        )
        .expect("the session mints a client");
    let foreign_token_attachment = resumed_server
        .handle_ipc_attach(
            Some(owner_attachment.client_id),
            Some(viewer_attachment.resume_token),
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            true,
            60,
            None,
        )
        .expect("the session mints a client");

    assert_ne!(tokenless_attachment.client_id, owner_attachment.client_id);
    assert_ne!(
        foreign_token_attachment.client_id,
        owner_attachment.client_id
    );
    assert_eq!(
        resumed_server.client_ids_awaiting_reconnect,
        HashSet::from([booted_client_id, owner_attachment.client_id]),
        "the viewer's token took the viewer's own record over, and the owner's record still waits"
    );

    let owner_returning_attachment = resumed_server
        .handle_ipc_attach(
            Some(owner_attachment.client_id),
            Some(owner_attachment.resume_token),
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session hands the record back");

    assert_eq!(
        owner_returning_attachment.client_id,
        owner_attachment.client_id
    );
    assert_eq!(
        resumed_server.client_ids_awaiting_reconnect,
        HashSet::from([booted_client_id])
    );
    assert_eq!(
        resumed_server.session_by_id[&session_id]
            .clients
            .count_clients(),
        4
    );
}

#[test]
fn after_a_swap_a_carried_client_with_no_token_hash_is_not_handed_back_by_its_id() {
    // The booted client's attach minted no token, the same as every client of
    // a body converted from format 4 or older. Its record waits for the grace
    // window.
    let (mut server, client_id) = boot_server();
    let pty_size_by_pane_id = server.pty_size_by_pane_id.clone();
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    let mut resumed_server = resume_carried_server(body, pty_size_by_pane_id);

    let attachment = resumed_server
        .handle_ipc_attach(
            Some(client_id),
            None,
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session mints a client");

    assert_ne!(attachment.client_id, client_id);
    assert_eq!(
        resumed_server.client_ids_awaiting_reconnect,
        HashSet::from([client_id])
    );
}

#[test]
fn a_view_filed_before_a_swap_comes_back_by_its_token_after_it() {
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, _root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, additional_pane_id) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    server
        .get_client_mut(leaving_attachment.client_id)
        .expect("the client that is about to leave")
        .update_active_tab_id(additional_tab_id);
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let mut pty_size_by_pane_id = server.pty_size_by_pane_id.clone();
    pty_size_by_pane_id.insert(
        additional_pane_id,
        PtySize {
            column_count: 80,
            row_count: 22,
        },
    );
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    let mut resumed_server = resume_carried_server(body, pty_size_by_pane_id);

    let returning_attachment = attach_with_token(
        &mut resumed_server,
        Some(leaving_attachment.resume_token),
        detached_at,
    );

    assert_eq!(
        resumed_server.session_by_id[&session_id]
            .clients
            .get_client_by_id(returning_attachment.client_id)
            .expect("the client the token attached")
            .get_active_tab_id(),
        additional_tab_id
    );
}

#[test]
fn closing_the_grace_window_files_each_unclaimed_view_under_its_token() {
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, _root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, additional_pane_id) = add_additional_tab(&mut server, session_id);
    let away_attachment = attach_with_token(&mut server, None, SystemTime::now());
    server
        .get_client_mut(away_attachment.client_id)
        .expect("the client that does not come back")
        .update_active_tab_id(additional_tab_id);
    let mut pty_size_by_pane_id = server.pty_size_by_pane_id.clone();
    pty_size_by_pane_id.insert(
        additional_pane_id,
        PtySize {
            column_count: 80,
            row_count: 22,
        },
    );
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    let mut resumed_server = resume_carried_server(body, pty_size_by_pane_id);
    let unclaimed_client_deadline = SystemTime::now();

    resumed_server.handle_drop_unclaimed_clients(unclaimed_client_deadline);

    assert_eq!(
        resumed_server.session_by_id[&session_id]
            .clients
            .count_clients(),
        0
    );
    let returning_attachment = attach_with_token(
        &mut resumed_server,
        Some(away_attachment.resume_token),
        unclaimed_client_deadline,
    );
    assert_eq!(
        resumed_server.session_by_id[&session_id]
            .clients
            .get_client_by_id(returning_attachment.client_id)
            .expect("the client the token attached")
            .get_active_tab_id(),
        additional_tab_id
    );
}

#[test]
fn a_carried_client_whose_tab_is_gone_is_taken_over_by_its_token_and_keeps_its_view() {
    // The client's tab was closed while it was away, and its record cannot be
    // handed back. Its token takes the record over: the fresh client lands on
    // the first tab with the zoom the record held there.
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let split_pane_id = split_booted_pane(&mut server, client_id, root_pane_id);
    let (closed_tab_id, _) = add_additional_tab(&mut server, session_id);
    {
        let session = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session");
        let client = session
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client");
        client.zoom_pane(booted_tab_id, split_pane_id);
        client.update_active_tab_id(closed_tab_id);
        session.tabs.remove(&closed_tab_id);
    }
    let resume_token = server.saved_view_store.mint_resume_token(client_id);
    server.client_ids_awaiting_reconnect.insert(client_id);

    let attachment = server
        .handle_ipc_attach(
            Some(client_id),
            Some(resume_token),
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
            60,
            None,
        )
        .expect("the session mints a client");

    assert_ne!(attachment.client_id, client_id);
    assert_eq!(server.client_ids_awaiting_reconnect, HashSet::new());
    let clients = &server.session_by_id[&session_id].clients;
    assert_eq!(clients.count_clients(), 1);
    let client = clients
        .get_client_by_id(attachment.client_id)
        .expect("the minted client");
    assert_eq!(client.get_active_tab_id(), booted_tab_id);
    assert_eq!(
        client.get_zoomed_pane_id(booted_tab_id),
        Some(split_pane_id)
    );
}

#[test]
fn a_token_proven_claim_on_a_client_a_connection_is_streaming_for_takes_that_client_over() {
    // A viewer on another machine dials again with its id and its token before
    // the session read that its first link dropped. The dial takes the
    // streaming record over and does not share its id.
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, _root_pane_id) = get_booted_parts(&server, client_id);
    let first_attachment = attach_with_token(&mut server, None, SystemTime::now());

    let second_attachment = server
        .handle_ipc_attach(
            Some(first_attachment.client_id),
            Some(first_attachment.resume_token.clone()),
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            true,
            60,
            None,
        )
        .expect("the session mints a client");

    assert_ne!(second_attachment.client_id, first_attachment.client_id);
    let clients = &server.session_by_id[&session_id].clients;
    assert_eq!(clients.count_clients(), 2);
    assert_eq!(
        clients
            .get_client_by_id(first_attachment.client_id)
            .map(|client| client.get_client_id()),
        None
    );
}

#[test]
fn a_session_switch_reaches_every_subscriber_that_views_the_client() {
    let (mut server, client_id) = boot_server();
    let (first_receiver, _) = server.subscribe(client_id, 60);
    let (second_receiver, _) = server.subscribe(client_id, 60);
    let (onlooker_receiver, _) = server.subscribe(ClientId::new(), 60);
    let target_session_id = SessionId::new();

    assert!(server.send_switch(client_id, target_session_id, Some(1001)));

    let expected_delivery = Delivery::SwitchTo {
        session_id: target_session_id,
        owner_user_id: Some(1001),
    };
    assert_eq!(
        first_receiver.try_iter().collect::<Vec<_>>(),
        vec![expected_delivery.clone()]
    );
    assert_eq!(
        second_receiver.try_iter().collect::<Vec<_>>(),
        vec![expected_delivery]
    );
    assert_eq!(
        onlooker_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_session_switch_for_a_client_no_subscriber_views_is_held_by_nobody() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);

    assert!(!server.send_switch(ClientId::new(), SessionId::new(), None));

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_session_switch_offered_to_a_paused_subscriber_is_held_by_nobody() {
    let (mut server, client_id) = boot_server();
    let (delivery_receiver, _) = server.subscribe(client_id, 60);
    let subscriber_id = server.subscriptions[0].subscriber_id;
    pause_subscribers(&mut server);
    let _backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();

    assert!(!server.send_switch(client_id, SessionId::new(), None));

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
}

#[test]
fn handing_the_inbox_over_keeps_the_receiver_the_panes_deliver_into() {
    let (server, sender) = build_test_server_with_event_sender();
    sender
        .send(RuntimeEvent::Quit)
        .expect("send before the swap");

    let inbox_receiver = server.into_inbox_receiver();

    sender
        .send(RuntimeEvent::Quit)
        .expect("send after the swap");
    for _ in 0..2 {
        let received_runtime_event = inbox_receiver.try_recv();
        let Ok(RuntimeEvent::Quit) = received_runtime_event else {
            panic!("expected Quit, got {received_runtime_event:?}");
        };
    }
    let Err(mpsc::TryRecvError::Empty) = inbox_receiver.try_recv() else {
        panic!("expected nothing more on the inbox");
    };
}

/// A server resumed from `body` over a fake PTY backend whose output wakes
/// the server's own inbox, with `pty_size_by_pane_id` as the carried PTY sizes,
/// no startup config, and no exit status.
fn resume_carried_server(
    body: ResumeBody,
    pty_size_by_pane_id: HashMap<PaneId, PtySize>,
) -> Server {
    let (runtime_event_sender, inbox_receiver) = mpsc::channel();
    Server::resume(
        Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
            InboxSink::from_event_sender(runtime_event_sender),
        ))),
        inbox_receiver,
        None,
        body,
        pty_size_by_pane_id,
        HashMap::new(),
    )
}

/// One live pane as the PTY backend reports it: no terminal descriptor, so no
/// terminal name is read for it.
fn build_carried_pty_pane(pane_id: PaneId, pty_size: PtySize) -> CarriedPtyPane {
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
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    let session_name = server.session_by_id[&session_id].session_name.clone();

    let (header, body) = server.carry_out(&[]).expect("a session to carry");

    assert_eq!(header.session_id, session_id);
    assert_eq!(header.session_name, session_name);
    assert_eq!(body.session_by_id[&session_id].session_id, session_id);
    assert_eq!(body.session_by_id[&session_id].session_name, session_name);
    assert!(body.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .is_some());
}

#[test]
fn carrying_out_a_server_with_no_session_carries_nothing_and_changes_nothing() {
    let (mut server, _inbox_sender) = build_test_server_with_event_sender();

    assert!(server.carry_out(&[]).is_none());

    assert!(server.list_sessions().is_empty());
    assert!(server.list_terminal_engines().is_empty());
}

#[test]
fn carrying_out_sizes_each_pane_by_this_servers_record_and_the_backend_otherwise() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let session_name = server.session_by_id[&session_id].session_name.clone();
    // 80x24 less the tabline and hint rows is 80x22, less the one-cell border
    // on each side is 78x20.
    assert_eq!(
        server.pty_size_by_pane_id[&root_pane_id],
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
    let unrecorded_pane_id = PaneId::new();
    let panes = [
        build_carried_pty_pane(
            root_pane_id,
            PtySize {
                column_count: 1,
                row_count: 1,
            },
        ),
        build_carried_pty_pane(
            unrecorded_pane_id,
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
                pane_id: root_pane_id,
                process_id: 51234,
                row_count: 20,
                column_count: 78,
                terminal_fd: None,
                terminal_name: None,
                exit_status: None,
            },
            CarriedPane {
                pane_id: unrecorded_pane_id,
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
    assert_eq!(body.carried_pane_state_by_pane_id.len(), 1);
    assert!(server.list_sessions().is_empty());
    assert!(server.list_terminal_engines().is_empty());
}

#[test]
fn a_resumed_server_puts_every_carried_engine_back_with_its_undecoded_bytes() {
    let (mut server, client_id) = boot_server();
    let (_session_id, _tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    // "hi" is printed; `ESC [` opens a control sequence that has no final byte
    // yet, so the parser stops there and holds those two bytes.
    server.handle_pty_output(root_pane_id, b"hi\x1b[");
    assert_eq!(
        server.terminal_engine_by_pane_id[&root_pane_id].get_undecoded_terminal_bytes(),
        b"\x1b["
    );
    let carried_size = server.pty_size_by_pane_id[&root_pane_id];

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.carried_pane_state_by_pane_id[&root_pane_id].undecoded_bytes,
        b"\x1b[".to_vec()
    );
    let carried_state = body.carried_pane_state_by_pane_id[&root_pane_id]
        .terminal_state
        .clone();

    let resumed_server = resume_carried_server(body, HashMap::from([(root_pane_id, carried_size)]));

    assert_eq!(resumed_server.terminal_engine_by_pane_id.len(), 1);
    assert_eq!(
        resumed_server.terminal_engine_by_pane_id[&root_pane_id].get_terminal_state(),
        &carried_state
    );
    assert_eq!(
        resumed_server.terminal_engine_by_pane_id[&root_pane_id].get_undecoded_terminal_bytes(),
        b"\x1b["
    );
    assert_eq!(
        resumed_server.pty_size_by_pane_id,
        HashMap::from([(root_pane_id, carried_size)])
    );
    assert_eq!(resumed_server.live_pane_ids, HashSet::from([root_pane_id]));
}

#[test]
fn a_resumed_server_keeps_queued_graphics_events() {
    let (mut server, client_id) = boot_server();
    let (_session_id, _tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    server.handle_pty_output(
        root_pane_id,
        b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\",
    );

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.carried_pane_state_by_pane_id[&root_pane_id]
            .graphics_events
            .len(),
        1
    );

    let mut resumed_server = resume_carried_server(
        body,
        HashMap::from([(
            root_pane_id,
            PtySize {
                column_count: 78,
                row_count: 20,
            },
        )]),
    );

    let terminal_engine = resumed_server
        .terminal_engine_by_pane_id
        .get_mut(&root_pane_id)
        .expect("the terminal engine");
    assert_eq!(
        terminal_engine
            .get_terminal_state()
            .list_image_placements()
            .len(),
        1
    );
    assert_eq!(
        terminal_engine.get_terminal_state().list_image_placements()[0].get_image_anchor(),
        (0, 0)
    );
    assert_eq!(
        terminal_engine.take_graphics_events(),
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
    let (mut server, client_id) = boot_server();
    let (_session_id, _tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let mut graphics_input_bytes = Vec::new();
    for _ in 0..66 {
        graphics_input_bytes
            .extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    }
    server.handle_pty_output(root_pane_id, &graphics_input_bytes);

    let (_header, body) = server.carry_out(&[]).expect("a session to carry");
    assert_eq!(
        body.carried_pane_state_by_pane_id[&root_pane_id]
            .graphics_events
            .len(),
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_BATCH_COUNT
    );

    let mut resumed_server = resume_carried_server(
        body,
        HashMap::from([(
            root_pane_id,
            PtySize {
                column_count: 78,
                row_count: 20,
            },
        )]),
    );
    let graphics_events = resumed_server
        .terminal_engine_by_pane_id
        .get_mut(&root_pane_id)
        .expect("the resumed engine")
        .take_graphics_events();

    assert_eq!(
        graphics_events.len(),
        koshi_terminal::engine::MAX_GRAPHICS_EVENT_BATCH_COUNT
    );
    assert_eq!(
        graphics_events.last(),
        Some(&Err(koshi_terminal::graphics::GraphicsError::QueueFull {
            dropped_event_count: 2
        }))
    );
}

#[test]
fn a_resumed_server_keeps_graphics_inside_a_split_screen_wrapper() {
    let (mut server, client_id) = boot_server();
    let (_session_id, _tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let image_graphics_bytes = b"\x1b]1337;File=inline=1;width=1;height=1:iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\x07";
    let split_byte_index = image_graphics_bytes.len() / 2;
    let screen_wrap = |inner_graphics_bytes: &[u8]| {
        let mut wrapped_graphics_bytes = b"\x1bP".to_vec();
        wrapped_graphics_bytes.extend_from_slice(inner_graphics_bytes);
        wrapped_graphics_bytes.extend_from_slice(b"\x1b\\");
        wrapped_graphics_bytes
    };

    server.handle_pty_output(
        root_pane_id,
        &screen_wrap(&image_graphics_bytes[..split_byte_index]),
    );
    let (_header, body) = server.carry_out(&[]).expect("a session to carry");

    let mut resumed_server = resume_carried_server(
        body,
        HashMap::from([(
            root_pane_id,
            PtySize {
                column_count: 78,
                row_count: 20,
            },
        )]),
    );
    resumed_server.handle_pty_output(
        root_pane_id,
        &screen_wrap(&image_graphics_bytes[split_byte_index..]),
    );

    let graphics_events = resumed_server
        .terminal_engine_by_pane_id
        .get_mut(&root_pane_id)
        .expect("the resumed engine")
        .take_graphics_events();
    assert_eq!(graphics_events.len(), 1);
    let graphics_event = graphics_events
        .into_iter()
        .next()
        .expect("the resumed image event")
        .expect("the resumed image decodes");
    assert_eq!(graphics_event.protocol, GraphicsProtocol::Iterm2);
    assert_eq!(graphics_event.image.pixel_width, 1);
    assert_eq!(graphics_event.image.pixel_height, 1);
    assert_eq!(graphics_event.image.rgba_bytes, vec![0, 0, 0, 255]);
    assert_eq!(graphics_event.action, ImageAction::Display);
    assert_eq!(
        graphics_event.display,
        ImageDisplay {
            requested_width: Some(koshi_terminal::graphics::ImageDimension::Cells(1)),
            requested_height: Some(koshi_terminal::graphics::ImageDimension::Cells(1)),
            ..ImageDisplay::default()
        }
    );
    assert_eq!(graphics_event.anchor, (0, 0));
}

#[cfg(unix)]
#[test]
fn the_pane_check_names_the_first_pane_with_no_terminal_descriptor() {
    let pane_without_terminal_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let panes = [
        build_carried_pty_pane(
            pane_without_terminal_id,
            PtySize {
                column_count: 80,
                row_count: 24,
            },
        ),
        build_carried_pty_pane(
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

/// Every delivery waiting on `delivery_receiver`, and whether its sender is
/// gone once they are read.
fn drain_deliveries(delivery_receiver: &DeliveryReceiver) -> (Vec<Delivery>, bool) {
    let mut waiting_deliveries = Vec::new();
    loop {
        match delivery_receiver.try_recv() {
            Ok(delivery) => waiting_deliveries.push(delivery),
            Err(mpsc::TryRecvError::Empty) => return (waiting_deliveries, false),
            Err(mpsc::TryRecvError::Disconnected) => return (waiting_deliveries, true),
        }
    }
}

/// A session left with one attached client, the one this returns, and
/// `auto-close-session` on.
fn boot_server_with_one_attached_client_under_auto_close() -> (Server, AttachAccepted) {
    let (mut server, genesis_client_id) = boot_server();
    let attachment = attach_with_token(&mut server, None, SystemTime::now());
    let _ = server.handle_client_detach(genesis_client_id);
    server.config.should_auto_close_session = true;
    (server, attachment)
}

#[test]
fn a_returning_client_leaves_its_view_filed_and_its_session_running_under_auto_close() {
    let (mut server, attachment) = boot_server_with_one_attached_client_under_auto_close();
    let (_, booted_tab_id, root_pane_id) = get_booted_parts(&server, attachment.client_id);
    let detached_at = SystemTime::now();

    let control_flow = server.handle_runtime_event(RuntimeEvent::ClientDetached {
        client_id: attachment.client_id,
        detached_at,
        is_streamed: true,
        is_returning: true,
    });

    assert_eq!(control_flow, std::ops::ControlFlow::Continue(()));
    assert_eq!(
        server.session_by_id[&attachment.session_id]
            .clients
            .count_clients(),
        0
    );
    assert!(
        !server.is_quit_requested(),
        "a client that may attach again does not end the session"
    );
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&attachment.resume_token, detached_at),
        Some(build_booted_saved_view(
            booted_tab_id,
            root_pane_id,
            HashMap::new()
        )),
        "the view is filed under the token the attach minted"
    );
}

#[test]
fn a_client_that_does_not_return_ends_its_session_under_auto_close_and_still_files_its_view() {
    let (mut server, attachment) = boot_server_with_one_attached_client_under_auto_close();
    let (_, booted_tab_id, root_pane_id) = get_booted_parts(&server, attachment.client_id);
    let detached_at = SystemTime::now();

    let _ = server.handle_runtime_event(RuntimeEvent::ClientDetached {
        client_id: attachment.client_id,
        detached_at,
        is_streamed: true,
        is_returning: false,
    });

    assert_eq!(
        server.session_by_id[&attachment.session_id]
            .clients
            .count_clients(),
        0
    );
    assert!(server.is_quit_requested());
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&attachment.resume_token, detached_at),
        Some(build_booted_saved_view(
            booted_tab_id,
            root_pane_id,
            HashMap::new()
        ))
    );
}

#[test]
fn a_returning_client_the_attach_reply_never_reached_files_no_view_and_keeps_its_session() {
    let (mut server, attachment) = boot_server_with_one_attached_client_under_auto_close();
    let detached_at = SystemTime::now();

    let _ = server.handle_runtime_event(RuntimeEvent::ClientDetached {
        client_id: attachment.client_id,
        detached_at,
        is_streamed: false,
        is_returning: true,
    });

    assert_eq!(
        server.session_by_id[&attachment.session_id]
            .clients
            .count_clients(),
        0
    );
    assert!(!server.is_quit_requested());
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&attachment.resume_token, detached_at),
        None
    );
}

#[test]
fn a_returning_detach_of_a_client_the_restart_grace_holds_changes_nothing() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    server.client_ids_awaiting_reconnect.insert(client_id);

    assert_eq!(server.handle_returning_client_detach(client_id), Vec::new());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
    assert!(server.client_ids_awaiting_reconnect.contains(&client_id));
}

#[test]
fn a_token_of_a_client_still_attached_takes_its_view_and_removes_that_client() {
    let (mut server, genesis_client_id) = boot_server();
    let (session_id, booted_tab_id, _root_pane_id) = get_booted_parts(&server, genesis_client_id);
    let (additional_tab_id, _additional_pane_id) = add_additional_tab(&mut server, session_id);
    let first_attachment = attach_with_token(&mut server, None, SystemTime::now());
    server
        .get_client_mut(first_attachment.client_id)
        .expect("the first client")
        .update_active_tab_id(additional_tab_id);
    let first_resume_token = first_attachment.resume_token.clone();

    let second_attachment = attach_with_token(
        &mut server,
        Some(first_resume_token.clone()),
        SystemTime::now(),
    );

    let session = &server.session_by_id[&session_id];
    assert_ne!(second_attachment.client_id, first_attachment.client_id);
    assert!(session
        .clients
        .get_client_by_id(first_attachment.client_id)
        .is_none());
    assert_eq!(
        session.clients.count_clients(),
        2,
        "the genesis client and the new one"
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_attachment.client_id)
            .expect("the client the token attached")
            .get_active_tab_id(),
        additional_tab_id
    );
    let (_, is_first_queue_closed) = drain_deliveries(&first_attachment.deliveries);
    assert!(is_first_queue_closed, "the removed client's queue closes");

    // The token is spent: presenting it again takes nothing over.
    let third_attachment =
        attach_with_token(&mut server, Some(first_resume_token), SystemTime::now());
    let session = &server.session_by_id[&session_id];
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_attachment.client_id)
            .map(|client| client.get_active_tab_id()),
        Some(additional_tab_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(third_attachment.client_id)
            .expect("the fresh client")
            .get_active_tab_id(),
        booted_tab_id
    );
}

#[test]
fn a_take_over_keeps_the_session_running_under_auto_close() {
    let (mut server, attachment) = boot_server_with_one_attached_client_under_auto_close();
    let (_, booted_tab_id, _) = get_booted_parts(&server, attachment.client_id);

    let returning_attachment = attach_with_token(
        &mut server,
        Some(attachment.resume_token.clone()),
        SystemTime::now(),
    );

    let session = &server.session_by_id[&attachment.session_id];
    assert_eq!(session.clients.count_clients(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(returning_attachment.client_id)
            .map(|client| client.get_active_tab_id()),
        Some(booted_tab_id)
    );
    assert!(!server.is_quit_requested());
}
