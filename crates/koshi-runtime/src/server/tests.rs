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

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::command::{
    Command, CommandSource, NewPaneArgs, NewPanePlacement, ToggleLockModeArgs,
};
use koshi_core::event::{
    InputModeChanged, PaneClosing, PaneCreated, PaneFocused, PaneProcessExited, PaneRemoved,
    PtyResized, SubscriberLagged,
};
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

impl Server {
    /// Take every byte queued for `client_id`'s outer terminal, or `None` when
    /// nothing is queued.
    pub(crate) fn take_host_writes(&mut self, client_id: ClientId) -> Option<Vec<u8>> {
        self.host_write_bytes_by_client_id.remove(&client_id)
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
    let client_id = server
        .bootstrap_local(SessionId::new(), TEST_VIEWPORT_SIZE, SystemTime::now())
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
    let command_result = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Floating {
                size: None,
                at: None,
                is_pinned: false,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        }),
    ));
    let CommandResult::Ok { emitted_events, .. } = command_result else {
        panic!("the floating pane was created, got {command_result:?}");
    };
    let Some(Event::PaneCreated(PaneCreated {
        pane_id: floating_pane_id,
        tab_id: None,
    })) = emitted_events.first().cloned()
    else {
        panic!("a floating PaneCreated comes first, got {emitted_events:?}");
    };
    server.live_pane_ids.remove(&floating_pane_id);

    let exit_events =
        server.close_undriven_panes(HashSet::from([floating_pane_id]), HashMap::new());

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
        .get_active_tab_id();
    let attached_client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        attached_client_id,
        viewport_size,
        None,
        active_tab_id,
        None,
        SystemTime::now(),
        false,
    );
    attached_client_id
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
    let active_tab_id = server.list_sessions()[&session_id]
        .clients
        .get_client_by_id(bootstrapped_client_id)
        .expect("client record")
        .get_active_tab_id();

    let attached_client_id = ClientId::new();
    let _ = server.handle_client_attach(
        session_id,
        attached_client_id,
        TEST_VIEWPORT_SIZE,
        None,
        active_tab_id,
        None,
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
    let inbox_receiver = server.subscribe(client_id);

    let _ = server.submit_command(CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));

    assert_eq!(
        inbox_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::InputModeChanged(InputModeChanged {
            client_id,
            lock_mode: LockMode::Locked,
        }))]
    );
}

#[test]
fn publish_events_delivers_out_of_command_events_to_subscribers() {
    let (mut server, client_id) = boot_server();
    let inbox_receiver = server.subscribe(client_id);
    let published_events = vec![Event::Quit(QuitCause::Requested)];

    server.publish_events(&published_events);

    assert_eq!(
        inbox_receiver.try_iter().collect::<Vec<_>>(),
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

    let _event_receiver = server.subscribe(client_id);

    assert_eq!(server.subscriptions.len(), 1);
    assert_eq!(server.subscriptions[0].1, client_id);
}

#[test]
fn a_subscriber_whose_receiver_is_gone_loses_its_recorded_client_too() {
    let (mut server, client_id) = boot_server();
    let inbox_receiver = server.subscribe(client_id);
    let (subscriber_id, _) = server.subscriptions[0];
    drop(inbox_receiver);

    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert_eq!(server.subscriptions, Vec::new());
}

#[test]
fn resyncing_hands_a_paused_subscriber_a_frame_of_the_client_it_views() {
    let (mut server, client_id) = boot_server();
    let inbox_receiver = server.subscribe(client_id);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    // Drain the queue: the frame fits on the next pass.
    let _delivery_backlog: Vec<Delivery> = inbox_receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged();

    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(
        inbox_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Snapshot {
            render_snapshot: Box::new(expected_render_snapshot),
            lag_report: SubscriberLagged {
                subscriber_id,
                dropped_event_count: 1,
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
    let (mut server, client_id) = boot_server();
    let _ = server.handle_client_resize(
        client_id,
        TEST_VIEWPORT_SIZE,
        Some(PaneArea::Starving),
        None,
    );
    let inbox_receiver = server.subscribe(client_id);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _delivery_backlog: Vec<Delivery> = inbox_receiver.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    let delivered_deliveries: Vec<Delivery> = inbox_receiver.try_iter().collect();
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
            .is_every_pane_suppressed
    );
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
        Size {
            column_count: 0,
            row_count: 0
        }
    );
}

#[test]
fn resyncing_with_nobody_paused_delivers_nothing() {
    let (mut server, client_id) = boot_server();
    let inbox_receiver = server.subscribe(client_id);
    let (subscriber_id, _) = server.subscriptions[0];

    server.resync_lagged();

    assert_eq!(
        inbox_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    assert!(server.event_bus.has_subscriber(subscriber_id));
}

#[test]
fn a_resync_blocked_by_a_full_queue_retries_with_a_newer_frame() {
    let (mut server, client_id) = boot_server();
    let delivery_receiver = server.subscribe(client_id);
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
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    ));
    let _backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();
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
    let kept_receiver = server.subscribe(client_id);
    let (kept_subscriber_id, _) = server.subscriptions[0];
    // Straight off the bus, so nothing records which client it views.
    let (orphan_subscriber_id, orphan_receiver) = server.event_bus.subscribe();
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![kept_subscriber_id, orphan_subscriber_id]
    );
    let _good_backlog: Vec<Delivery> = kept_receiver.try_iter().collect();
    let _orphan_backlog: Vec<Delivery> = orphan_receiver.try_iter().collect();
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.resync_lagged();

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
    assert_eq!(server.subscriptions, vec![(kept_subscriber_id, client_id)]);
}

#[test]
fn a_gone_receiver_costs_only_its_own_recorded_client() {
    let (mut server, client_id) = boot_server();
    let kept_receiver = server.subscribe(client_id);
    let gone_receiver = server.subscribe(client_id);
    let (kept_subscriber_id, _) = server.subscriptions[0];
    let (gone_subscriber_id, _) = server.subscriptions[1];
    drop(gone_receiver);

    server.publish_events(&[Event::Quit(QuitCause::Requested)]);

    assert!(!server.event_bus.has_subscriber(gone_subscriber_id));
    assert!(server.event_bus.has_subscriber(kept_subscriber_id));
    assert_eq!(server.subscriptions, vec![(kept_subscriber_id, client_id)]);
    assert_eq!(
        kept_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit(QuitCause::Requested))]
    );
}

#[test]
fn a_paused_subscriber_that_views_no_client_is_unsubscribed() {
    let (mut server, _client_id) = boot_server();
    // Straight off the bus, so nothing records which client it views.
    let (subscriber_id, delivery_receiver) = server.event_bus.subscribe();
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _drained_deliveries: Vec<Delivery> = delivery_receiver.try_iter().collect();

    server.resync_lagged();

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
    let delivery_receiver = server.subscribe(ClientId::new());
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    assert_eq!(
        server.event_bus.list_desynced_subscriber_ids(),
        vec![subscriber_id]
    );
    let _backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();

    server.resync_lagged();

    assert_eq!(server.event_bus.count_subscribers(), 0);
    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
    assert_eq!(server.subscriptions, Vec::new());
}

#[test]
fn pushing_frames_serves_every_client() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_additional_client(&mut server, local_client_id, REMOTE_VIEWPORT_SIZE);
    let local_receiver = server.subscribe(local_client_id);
    let remote_receiver = server.subscribe(remote_client_id);
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

    server.push_frames();

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
fn a_due_render_hands_a_clients_queued_host_bytes_to_its_subscriber_before_its_frame() {
    let (mut server, client_id) = boot_server();
    let delivery_receiver = server.subscribe(client_id);
    // An OSC 52 copy of "hello", as `copy_to_clipboard` queues it.
    let queued_host_bytes = b"\x1b]52;c;aGVsbG8=\x07".to_vec();
    server.queue_host_write(client_id, &queued_host_bytes);
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");

    server.push_frames();

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::HostWrite(queued_host_bytes),
            Delivery::Frame(Box::new(expected_render_snapshot)),
        ]
    );
    assert_eq!(server.host_write_bytes_by_client_id.get(&client_id), None);
}

#[test]
fn a_detach_drops_the_bytes_queued_for_that_clients_terminal() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_additional_client(&mut server, local_client_id, TEST_VIEWPORT_SIZE);
    server.queue_host_write(remote_client_id, b"\x1b]52;c;aGVsbG8=\x07");

    let _ = server.handle_client_detach(remote_client_id);

    assert_eq!(
        server.host_write_bytes_by_client_id.get(&remote_client_id),
        None
    );
}

#[test]
fn pushing_frames_serves_no_client_that_detached() {
    let (mut server, local_client_id) = boot_server();
    let remote_client_id =
        attach_additional_client(&mut server, local_client_id, TEST_VIEWPORT_SIZE);
    let remote_receiver = server.subscribe(remote_client_id);
    let (subscriber_id, _) = server.subscriptions[0];

    // The detach takes the subscription with the client record, so the push
    // finds nobody to build a frame for.
    let _ = server.handle_client_detach(remote_client_id);
    server.push_frames();

    assert_eq!(server.subscriptions, Vec::new());
    assert!(!server.event_bus.has_subscriber(subscriber_id));
    assert_eq!(
        remote_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_frame_for_a_gone_receiver_costs_that_subscription_its_recorded_client() {
    let (mut server, client_id) = boot_server();
    let kept_receiver = server.subscribe(client_id);
    let gone_receiver = server.subscribe(client_id);
    let (kept_subscriber_id, _) = server.subscriptions[0];
    let (gone_subscriber_id, _) = server.subscriptions[1];
    let expected_render_snapshot = server.build_snapshot(client_id).expect("frame");
    drop(gone_receiver);

    server.push_frames();

    assert!(!server.event_bus.has_subscriber(gone_subscriber_id));
    assert!(server.event_bus.has_subscriber(kept_subscriber_id));
    assert_eq!(server.subscriptions, vec![(kept_subscriber_id, client_id)]);
    assert_eq!(
        kept_receiver.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected_render_snapshot))]
    );
}

#[test]
fn a_frame_blocked_by_a_full_queue_leaves_the_subscription_in_place() {
    let (mut server, client_id) = boot_server();
    let delivery_receiver = server.subscribe(client_id);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    // Free one slot and spend it on the resync frame: the subscriber is live
    // again, with a queue that is full again.
    let _oldest_delivery: Delivery = delivery_receiver.recv().expect("queued event");
    server.resync_lagged();
    assert_eq!(server.event_bus.list_desynced_subscriber_ids(), Vec::new());

    server.push_frames();

    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    assert!(server.event_bus.has_subscriber(subscriber_id));
    let queued_deliveries: Vec<Delivery> = delivery_receiver.try_iter().collect();
    assert!(
        !queued_deliveries
            .iter()
            .any(|delivery| matches!(delivery, Delivery::Frame(_))),
        "the frame did not fit, so none was queued"
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
        Tab::from_root_pane(tab_id, "second".to_string(), tab_index, pane_id),
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
fn a_restart_is_refused_while_no_check_is_installed_and_leaves_the_flag_down() {
    let (mut server, _inbox_sender) = build_test_server_with_event_sender();

    assert_eq!(
        server.handle_ipc_restart(),
        Err(RestartRefusal::ImageReplacementUnsupported)
    );
    assert!(!server.is_restart_requested());
}

#[test]
fn a_restart_the_check_refuses_leaves_the_flag_down() {
    let (mut server, _inbox_sender) = build_test_server_with_event_sender();
    server.set_restart_check(Arc::new(|| {
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the binary at /x is not executable".to_string(),
        })
    }));

    assert_eq!(
        server.handle_ipc_restart(),
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the binary at /x is not executable".to_string(),
        })
    );
    assert!(!server.is_restart_requested());
}

#[test]
fn a_restart_the_check_passes_raises_the_flag_and_changes_nothing_else() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    server.set_restart_check(Arc::new(|| Ok(())));

    assert_eq!(server.handle_ipc_restart(), Ok(()));

    assert!(server.is_restart_requested());
    assert!(!server.is_quit_requested());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
    assert_eq!(server.live_pane_ids.len(), 1);
}

#[test]
fn a_restart_taken_back_lowers_the_flag_and_the_next_one_is_accepted_again() {
    let (mut server, _client_id) = boot_server();
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
    let (mut server, _client_id) = boot_server();
    server.set_restart_check(Arc::new(|| {
        Err(RestartRefusal::PaneNotReady {
            refusal_reason: "the first check".to_string(),
        })
    }));
    assert_eq!(
        server.handle_ipc_restart(),
        Err(RestartRefusal::PaneNotReady {
            refusal_reason: "the first check".to_string(),
        })
    );

    server.set_restart_check(Arc::new(|| {
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the second check".to_string(),
        })
    }));

    assert_eq!(
        server.handle_ipc_restart(),
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the second check".to_string(),
        })
    );
    assert!(!server.is_restart_requested());
}

#[test]
fn an_attach_claiming_a_carried_client_keeps_its_id_zoom_focus_and_tab() {
    let (mut server, client_id) = boot_server();
    let (session_id, tab_id, pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
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
        client.update_active_tab_id(additional_tab_id);
    }
    server.client_ids_awaiting_reconnect.insert(client_id);

    let attachment = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
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
    // The first attach takes the record and holds its queue, so the record is
    // in use when the second attach names it.
    let held_attachment = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
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
        )
        .expect("the second attach mints a client instead of refusing");

    assert_ne!(newly_minted_attachment.client_id, client_id);
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 2);
    // The client already streaming keeps its record and its own subscription:
    // a second caller naming the same id takes neither.
    let viewed_client_ids: Vec<ClientId> = server
        .subscriptions
        .iter()
        .map(|&(_, client)| client)
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
    assert!(
        matches!(command_result, CommandResult::Ok { .. }),
        "the split ran, got {command_result:?}"
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
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
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
        session
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
            .expect("the client that is about to leave")
            .update_focused_pane(booted_tab_id, root_pane_id);
        // The tab's most recent focus is the split pane, so an attach that
        // restores nothing lands on `split_pane_id`.
        session
            .tabs
            .get_mut(&booted_tab_id)
            .expect("the tab")
            .record_focus_mru(split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let watcher_receiver = server.subscribe(client_id);

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
                pane_id: split_pane_id,
                previous_pane_id: None,
            },
            PaneFocused {
                client_id: returning_attachment.client_id,
                tab_id: Some(booted_tab_id),
                pane_id: root_pane_id,
                previous_pane_id: Some(split_pane_id),
            },
        ],
        "the attach lands on the tab's most recent pane, then the restored view moves the focus back"
    );
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(returning_attachment.client_id)
            .expect("the client the token attached")
            .get_focused_pane_id(booted_tab_id),
        Some(root_pane_id)
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
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(leaving_attachment.client_id)
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
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
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
fn a_token_presented_121_seconds_after_the_detach_takes_nothing_back() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
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
        detached_at + Duration::from_secs(121),
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
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(leaving_attachment.client_id)
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
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.zoom_pane(booted_tab_id, split_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .panes
        .remove_pane_record(split_pane_id);

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
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.update_focused_pane(additional_tab_id, additional_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .panes
        .remove_pane_record(additional_pane_id);

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
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let _split = split_booted_pane(&mut server, client_id, root_pane_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    // A tab's panes are solved once per viewer, and each pane takes its
    // smallest rect across them. The client whose zoom comes back is the tab's
    // only viewer, and it stays tiled until the restore zooms it.
    let _ = server.handle_client_detach(client_id);
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.update_focused_pane(booted_tab_id, root_pane_id);
        client.zoom_pane(booted_tab_id, root_pane_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);
    let delivery_receiver = server.subscribe(ClientId::new());

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
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(client_id)
            .expect("the booted client");
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
        Some(SavedView {
            active_tab_id: booted_tab_id,
            focused_pane_id_by_tab_id: HashMap::from([(booted_tab_id, root_pane_id)]),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::from([(root_pane_id, 500)]),
        })
    );
}

#[test]
fn a_claim_that_wins_keeps_its_record_and_drops_the_presented_tokens_view() {
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let (additional_tab_id, _) = add_additional_tab(&mut server, session_id);
    let leaving_attachment = attach_with_token(&mut server, None, SystemTime::now());
    {
        let client = server
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
            .expect("the client that is about to leave");
        client.zoom_pane(booted_tab_id, root_pane_id);
        client.set_scroll_offset(root_pane_id, 500);
        client.update_active_tab_id(additional_tab_id);
    }
    let detached_at = SystemTime::now();
    server.save_client_view(leaving_attachment.client_id, detached_at);
    let _ = server.handle_client_detach(leaving_attachment.client_id);

    // The booted client's record is still held and no connection streams for
    // it, so the claim wins and the token names a view of another client.
    let returning_attachment = server
        .handle_ipc_attach(
            Some(client_id),
            Some(leaving_attachment.resume_token.clone()),
            TEST_VIEWPORT_SIZE,
            None,
            None,
            detached_at,
            false,
        )
        .expect("the session hands the record back");

    assert_eq!(returning_attachment.client_id, client_id);
    let client = server.session_by_id[&session_id]
        .clients
        .get_client_by_id(client_id)
        .expect("the record the claim took");
    assert_eq!(client.get_active_tab_id(), booted_tab_id);
    assert_eq!(client.get_layout_mode(booted_tab_id), LayoutMode::Tiled);
    assert_eq!(client.get_scroll_offset(root_pane_id), 0);
    // The token is spent either way, so presenting it again takes nothing.
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&leaving_attachment.resume_token, detached_at),
        None
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
            .session_by_id
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_client_mut_by_id(leaving_attachment.client_id)
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
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let now = SystemTime::now();
    let leaving_attachment = attach_with_token(&mut server, None, now);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(leaving_attachment.client_id)
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
    );

    assert!(refused_attachment.is_none(), "no session is left to join");
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&leaving_attachment.resume_token, now),
        Some(SavedView {
            active_tab_id: booted_tab_id,
            focused_pane_id_by_tab_id: HashMap::from([(booted_tab_id, root_pane_id)]),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::from([(root_pane_id, 42)]),
        }),
        "the refused attach read no token and spent none"
    );
}

#[test]
fn a_connection_that_never_reached_its_stream_files_no_view() {
    let (mut server, client_id) = boot_server();
    let (session_id, _booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
    let now = SystemTime::now();
    let undelivered_attachment = attach_with_token(&mut server, None, now);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(undelivered_attachment.client_id)
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
    let (session_id, booted_tab_id, root_pane_id) = get_booted_parts(&server, client_id);
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
        .session_by_id
        .get_mut(&session_id)
        .expect("the session")
        .clients
        .get_client_mut_by_id(next_attachment.client_id)
        .expect("the client that just attached")
        .set_scroll_offset(root_pane_id, 500);
    server.save_client_view(next_attachment.client_id, now);
    assert_eq!(
        server
            .saved_view_store
            .take_saved_view(&next_attachment.resume_token, now),
        Some(SavedView {
            active_tab_id: booted_tab_id,
            focused_pane_id_by_tab_id: HashMap::from([(booted_tab_id, root_pane_id)]),
            zoomed_pane_id_by_tab_id: HashMap::new(),
            scroll_offset_by_pane_id: HashMap::from([(root_pane_id, 500)]),
        })
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
    let absent_client_id = ClientId::new();
    server.handle_client_attach(
        session_id,
        absent_client_id,
        TEST_VIEWPORT_SIZE,
        None,
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(client_id)
            .expect("the booted client")
            .get_active_tab_id(),
        None,
        SystemTime::now(),
        false,
    );
    server.client_ids_awaiting_reconnect.insert(client_id);
    server
        .client_ids_awaiting_reconnect
        .insert(absent_client_id);
    // One of the two came back before the window closed.
    let _held = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            TEST_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
        )
        .expect("the record is handed back");

    server.handle_drop_unclaimed_clients(Instant::now());

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

    let emitted_events = server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 1);
}

#[test]
fn closing_the_grace_window_keeps_a_session_left_with_no_client_under_auto_close() {
    let (mut server, client_id) = boot_server();
    let (session_id, _tab_id, _pane_id) = get_booted_parts(&server, client_id);
    server.config.should_auto_close_session = true;
    server.client_ids_awaiting_reconnect.insert(client_id);

    server.handle_drop_unclaimed_clients(Instant::now());

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

    server.handle_drop_unclaimed_clients(Instant::now());

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
    server.handle_drop_unclaimed_clients(Instant::now());

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
    let writer_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        writer_ending_notice.record_writer_ended();
    });

    let wait_started_at = Instant::now();
    server.announce_restarting();
    let waited_duration = wait_started_at.elapsed();

    assert_eq!(
        ending_notice.get_session_ending(),
        Some(SessionEnding::Restarting),
        "the notice must name the frame the clients are told"
    );
    assert_eq!(
        ending_notice.count_running_writers(),
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
        ending_notice.count_running_writers(),
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
    let (_, delivery_receiver) = server.event_bus.subscribe();

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
    let (_, delivery_receiver) = server.event_bus.subscribe();
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
    let (_, delivery_receiver) = server.event_bus.subscribe();

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
fn an_attach_claiming_a_client_whose_tab_is_gone_mints_a_new_one_and_leaves_that_record_awaited() {
    // Another client closes the tab that this client was viewing while this
    // client is away. The attach mints a fresh client on the first tab, and the
    // record keeps waiting for the grace window.
    let (mut server, client_id) = boot_server();
    let (session_id, booted_tab_id, _booted_pane_id) = get_booted_parts(&server, client_id);
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

    let attachment = server
        .handle_ipc_attach(
            Some(client_id),
            None,
            REMOTE_VIEWPORT_SIZE,
            None,
            None,
            SystemTime::now(),
            false,
        )
        .expect("the session mints a client instead of refusing");

    assert_ne!(attachment.client_id, client_id);
    assert_eq!(attachment.session_id, session_id);
    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 2);
    assert_eq!(
        server.session_by_id[&session_id]
            .clients
            .get_client_by_id(attachment.client_id)
            .expect("the minted record")
            .get_active_tab_id(),
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
    let (mut server, _client_id) = boot_server();
    let check_run_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted_check_run_count = Arc::clone(&check_run_count);
    server.set_restart_check(Arc::new(move || {
        counted_check_run_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }));

    assert_eq!(server.handle_ipc_restart(), Ok(()));
    assert_eq!(server.handle_ipc_restart(), Ok(()));

    assert_eq!(
        check_run_count.load(std::sync::atomic::Ordering::SeqCst),
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
    // The second request is answered with what the check says now, and the
    // swap the first one asked for stays asked for.
    let (mut server, _client_id) = boot_server();
    server.set_restart_check(Arc::new(|| Ok(())));
    assert_eq!(server.handle_ipc_restart(), Ok(()));

    server.set_restart_check(Arc::new(|| {
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the binary at /x is not executable".to_string(),
        })
    }));

    assert_eq!(
        server.handle_ipc_restart(),
        Err(RestartRefusal::UnfitProgramFile {
            refusal_reason: "the binary at /x is not executable".to_string(),
        })
    );
    assert!(server.is_restart_requested());
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

    server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(server.session_by_id[&session_id].clients.count_clients(), 0);
    assert!(server.client_ids_awaiting_reconnect.is_empty());
}

#[test]
fn a_session_switch_reaches_every_subscriber_that_views_the_client() {
    let (mut server, client_id) = boot_server();
    let first_receiver = server.subscribe(client_id);
    let second_receiver = server.subscribe(client_id);
    let onlooker_receiver = server.subscribe(ClientId::new());
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
        onlooker_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_session_switch_for_a_client_no_subscriber_views_is_held_by_nobody() {
    let (mut server, client_id) = boot_server();
    let delivery_receiver = server.subscribe(client_id);

    assert!(!server.send_switch(ClientId::new(), SessionId::new()));

    assert_eq!(
        delivery_receiver.try_iter().collect::<Vec<_>>(),
        Vec::<Delivery>::new()
    );
}

#[test]
fn a_session_switch_offered_to_a_paused_subscriber_is_held_by_nobody() {
    let (mut server, client_id) = boot_server();
    let delivery_receiver = server.subscribe(client_id);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    let _backlog: Vec<Delivery> = delivery_receiver.try_iter().collect();

    assert!(!server.send_switch(client_id, SessionId::new()));

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
fn queued_host_bytes_go_behind_whatever_is_already_queued() {
    let (mut server, client_id) = boot_server();

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

/// A server resumed from `body` over a fake PTY backend whose output reaches
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
