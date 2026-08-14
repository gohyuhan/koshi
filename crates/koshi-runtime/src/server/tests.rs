//! Tests for the server half: construction defaults, the held service
//! handles, the wired event inbox, a session with one tab and one pane, and
//! the two doors — commands in via `submit_command`, events out via
//! `subscribe` — including the identity every attached client carries, that
//! detaching a client leaves the server healthy with its panes alive, that a
//! subscriber paused by a dropped critical event is handed a fresh frame or
//! dropped, and that a due render hands every client its own frame, behind any
//! bytes queued for that client's own terminal.

use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use koshi_core::command::{Command, CommandSource, ToggleLockModeArgs};
use koshi_core::event::{EventClass, InputMode, InputModeChanged, PaneFocused, SubscriberLagged};
use koshi_core::ids::{CommandId, TabId};
use koshi_core::process::PtySize;
use koshi_pane::pane::state::PaneRecord;
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::{ClientOrigin, ClientRegistry};
use koshi_session::session::state::Tab;
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;
use crate::placeholder::{NullSnapshotProvider, NullStorage};
use crate::runtime::event::SessionEnding;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };
/// The viewport of a second, out-of-process client, sized apart from
/// [`VIEWPORT`] so a frame names which client it was built for.
const REMOTE_VIEWPORT: Size = Size {
    cols: 100,
    rows: 30,
};

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

/// Attach a second client at `viewport`, viewing the tab `first` views, and
/// hand back its id.
fn attach_second_client(server: &mut Server, first: ClientId, viewport: Size) -> ClientId {
    let session_id = *server.sessions().keys().next().expect("session");
    let active_tab = server.sessions()[&session_id]
        .clients
        .get(first)
        .expect("client record")
        .active_tab();
    let second = ClientId::new();
    let _ =
        server.handle_client_attach(session_id, second, viewport, active_tab, SystemTime::now());
    second
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
fn every_attached_client_is_local_with_its_own_generated_label() {
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
    assert_eq!(attached.origin(), ClientOrigin::Local);

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
    // Same size, so nothing reflows; the joining client still lands on the
    // tab's pane, which is the one event a same-size attach carries.
    let landed_on = server.sessions()[&session_id].tabs[&active_tab]
        .layout()
        .leaf_panes()
        .first()
        .copied()
        .expect("the tab holds one pane");
    assert_eq!(
        events,
        vec![Event::PaneFocused(PaneFocused {
            client_id: second,
            tab_id: active_tab,
            pane_id: landed_on,
            prior_pane: None,
        })],
        "same-size attach reflows nothing"
    );
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
fn pushing_frames_serves_every_client() {
    let (mut server, local) = booted_server();
    let remote = attach_second_client(&mut server, local, REMOTE_VIEWPORT);
    let local_rx = server.subscribe(local, EventFilter::All);
    let remote_rx = server.subscribe(remote, EventFilter::All);
    let local_frame = server.build_snapshot(local).expect("frame");
    let remote_frame = server.build_snapshot(remote).expect("frame");
    assert_eq!(local_frame.client.viewport, VIEWPORT);
    assert_eq!(remote_frame.client.viewport, REMOTE_VIEWPORT);

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
    let rx = server.subscribe(client_id, EventFilter::All);
    // An OSC 52 copy of "hello", as `copy_to_clipboard` queues it.
    let bytes = b"\x1b]52;c;aGVsbG8=\x07".to_vec();
    server.queue_host_write(client_id, &bytes);
    let expected = server.build_snapshot(client_id).expect("frame");

    server.push_frames();

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            Delivery::HostWrite(bytes),
            Delivery::Frame(Box::new(expected)),
        ]
    );
    assert_eq!(server.host_writes.get(&client_id), None);
}

#[test]
fn a_detach_drops_the_bytes_queued_for_that_clients_terminal() {
    let (mut server, local) = booted_server();
    let remote = attach_second_client(&mut server, local, VIEWPORT);
    server.queue_host_write(remote, b"\x1b]52;c;aGVsbG8=\x07");

    let _ = server.handle_client_detach(remote);

    assert_eq!(server.host_writes.get(&remote), None);
}

#[test]
fn pushing_frames_serves_no_client_that_detached() {
    let (mut server, local) = booted_server();
    let remote = attach_second_client(&mut server, local, VIEWPORT);
    let remote_rx = server.subscribe(remote, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];

    // The detach takes the subscription with the client record, so the push
    // finds nobody to build a frame for.
    let _ = server.handle_client_detach(remote);
    server.push_frames();

    assert_eq!(server.subscriptions, Vec::new());
    assert!(!server.event_bus.contains(subscriber_id));
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
    let expected = server.build_snapshot(client_id).expect("frame");
    drop(gone);

    server.push_frames();

    assert!(!server.event_bus.contains(gone_id));
    assert!(server.event_bus.contains(keep_id));
    assert_eq!(server.subscriptions, vec![(keep_id, client_id)]);
    assert_eq!(
        keep.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Frame(Box::new(expected))]
    );
}

#[test]
fn a_frame_blocked_by_a_full_queue_leaves_the_subscription_in_place() {
    let (mut server, client_id) = booted_server();
    let rx = server.subscribe(client_id, EventFilter::All);
    let (subscriber_id, _) = server.subscriptions[0];
    pause_subscribers(&mut server);
    // Free one slot and spend it on the resync frame: the subscriber is live
    // again, with a queue that is full again.
    let _oldest: Delivery = rx.recv().expect("queued event");
    server.resync_lagged();
    assert_eq!(server.event_bus.desynced(), Vec::new());

    server.push_frames();

    assert_eq!(server.subscriptions, vec![(subscriber_id, client_id)]);
    assert!(server.event_bus.contains(subscriber_id));
    let delivered: Vec<Delivery> = rx.try_iter().collect();
    assert!(
        !delivered
            .iter()
            .any(|item| matches!(item, Delivery::Frame(_))),
        "the frame did not fit, so none was queued"
    );
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

/// The one session a booted server holds, and the tab and root pane its client
/// is looking at.
fn booted_parts(server: &Server, client_id: ClientId) -> (SessionId, TabId, PaneId) {
    let session_id = *server.sessions.keys().next().expect("the booted session");
    let session = &server.sessions[&session_id];
    let tab_id = session
        .clients
        .get(client_id)
        .expect("the booted client")
        .active_tab();
    let pane_id = session.tabs[&tab_id]
        .focus_mru()
        .first()
        .copied()
        .expect("the tab's root pane");
    (session_id, tab_id, pane_id)
}

/// A second tab in the booted session, holding a pane of its own, so a test can
/// leave a client on a tab that is not the session's first.
fn add_second_tab(server: &mut Server, session_id: SessionId) -> TabId {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session = server.sessions.get_mut(&session_id).expect("the session");
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::UNIX_EPOCH))
        .expect("a fresh pane id");
    let index = session.tabs.len();
    session.tabs.insert(
        tab_id,
        Tab::new(tab_id, "second".to_string(), index, pane_id),
    );
    tab_id
}

/// A file at `path` holding nothing, carrying `mode` on Unix.
#[cfg(unix)]
fn write_binary(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, b"").expect("the stand-in binary is written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("the stand-in binary takes the mode asked for");
}

#[test]
fn a_readable_binary_this_machine_could_run_passes_the_binary_check() {
    let dir = std::env::temp_dir().join(format!("koshi-restart-ok-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the directory is created");
    let exe = dir.join("koshi");
    #[cfg(unix)]
    write_binary(&exe, 0o755);
    #[cfg(not(unix))]
    std::fs::write(&exe, b"").expect("the stand-in binary is written");

    assert_eq!(binary_is_runnable(&exe), Ok(()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_binary_that_cannot_be_read_fails_the_binary_check_naming_the_path() {
    let dir = std::env::temp_dir().join(format!("koshi-restart-gone-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the directory is created");
    let exe = dir.join("koshi");
    let error = std::fs::metadata(&exe).expect_err("nothing is at that path");

    assert_eq!(
        binary_is_runnable(&exe),
        Err(format!(
            "the binary at {} could not be read: {error}",
            exe.display()
        ))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// A binary the kernel would refuse to exec must be caught before the swap
// starts, so the session never tears itself down for a restart that cannot run.
#[cfg(unix)]
#[test]
fn a_binary_with_no_execute_bit_fails_the_binary_check_naming_the_path() {
    let dir = std::env::temp_dir().join(format!("koshi-restart-noexec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the directory is created");
    let exe = dir.join("koshi");
    write_binary(&exe, 0o644);

    assert_eq!(
        binary_is_runnable(&exe),
        Err(format!("the binary at {} is not executable", exe.display()))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_pane_whose_terminal_exposes_no_descriptor_fails_the_pane_check_naming_it() {
    let carried = PaneId::new();
    let stranded = PaneId::new();
    let panes = [
        CarriedPtyPane {
            pane_id: carried,
            terminal_fd: Some(9),
            pid: 51234,
            size: PtySize { cols: 80, rows: 24 },
            exit: None,
        },
        CarriedPtyPane {
            pane_id: stranded,
            terminal_fd: None,
            pid: 51235,
            size: PtySize { cols: 80, rows: 24 },
            exit: None,
        },
    ];

    assert_eq!(
        panes_can_be_carried(&panes),
        Err(format!(
            "pane {stranded} has no terminal descriptor, so its terminal cannot cross the swap"
        ))
    );
    assert_eq!(panes_can_be_carried(&panes[..1]), Ok(()));
    // A session holding no pane holds no restart back either.
    assert_eq!(panes_can_be_carried(&[]), Ok(()));
}

// Windows keeps every pane's pseudoconsole in the supervisor process, which
// outlives the swap, so the pane check has nothing to refuse there.
#[cfg(windows)]
#[test]
fn no_pane_holds_a_restart_back_on_windows() {
    let panes = [CarriedPtyPane {
        pane_id: PaneId::new(),
        pid: 51234,
        size: PtySize { cols: 80, rows: 24 },
        exit: None,
    }];

    assert_eq!(panes_can_be_carried(&panes), Ok(()));
    assert_eq!(panes_can_be_carried(&[]), Ok(()));
}

#[test]
fn a_restart_is_refused_while_no_check_is_installed_and_leaves_the_flag_down() {
    let (mut server, _tx) = new_server();

    assert_eq!(
        server.handle_ipc_restart(),
        Err("this koshi cannot replace its own image, so it cannot restart".to_string())
    );
    assert!(!server.restart_requested());
}

#[test]
fn a_restart_the_check_refuses_leaves_the_flag_down() {
    let (mut server, _tx) = new_server();
    server.set_restart_check(Arc::new(|| {
        Err("the binary at /x is not executable".to_string())
    }));

    assert_eq!(
        server.handle_ipc_restart(),
        Err("the binary at /x is not executable".to_string())
    );
    assert!(!server.restart_requested());
}

#[test]
fn a_restart_the_check_passes_raises_the_flag_and_changes_nothing_else() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.set_restart_check(Arc::new(|| Ok(())));

    assert_eq!(server.handle_ipc_restart(), Ok(()));

    assert!(server.restart_requested());
    assert!(!server.quit_requested());
    assert_eq!(server.sessions[&session_id].clients.len(), 1);
    assert_eq!(server.pty_handles.len(), 1);
}

#[test]
fn a_restart_taken_back_lowers_the_flag_and_the_next_one_is_accepted_again() {
    // A swap the session abandoned before anything irreversible happened puts
    // the session back on its feet in this same process, so the event loop must
    // stop asking for the swap and the next restart request must still work.
    let (mut server, _client_id) = booted_server();
    server.set_restart_check(Arc::new(|| Ok(())));
    assert_eq!(server.handle_ipc_restart(), Ok(()));
    assert!(server.restart_requested());

    server.cancel_restart();

    assert!(!server.restart_requested());
    assert!(!server.quit_requested());
    assert_eq!(server.handle_ipc_restart(), Ok(()));
    assert!(server.restart_requested());
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
    assert!(!server.restart_requested());
}

#[test]
fn an_attach_claiming_a_carried_client_keeps_its_id_zoom_focus_and_tab() {
    let (mut server, client_id) = booted_server();
    let (session_id, tab_id, pane_id) = booted_parts(&server, client_id);
    let second_tab = add_second_tab(&mut server, session_id);
    {
        let client = server
            .sessions
            .get_mut(&session_id)
            .expect("the session")
            .clients
            .get_mut(client_id)
            .expect("the booted client");
        client.update_focused_pane(tab_id, pane_id);
        client.zoom_pane(tab_id, pane_id);
        client.set_scroll_offset(pane_id, 7);
        // The client was looking at the second tab when the image was replaced.
        client.update_active_tab(second_tab);
    }
    server.awaiting_reconnect.insert(client_id);

    let accepted = server
        .handle_ipc_attach(
            Some(client_id),
            REMOTE_VIEWPORT,
            EventFilter::All,
            SystemTime::now(),
        )
        .expect("the session hands the record back");

    assert_eq!(accepted.client_id, client_id);
    assert_eq!(accepted.session_id, session_id);
    assert_eq!(server.sessions[&session_id].clients.len(), 1);
    assert!(server.awaiting_reconnect.is_empty());
    let client = server.sessions[&session_id]
        .clients
        .get(client_id)
        .expect("the same record");
    assert_eq!(client.active_tab(), second_tab);
    assert_eq!(client.focused_pane(tab_id), Some(pane_id));
    assert_eq!(client.zoomed_pane(tab_id), Some(pane_id));
    assert_eq!(client.scroll_offset(pane_id), 7);
    assert_eq!(client.viewport(), REMOTE_VIEWPORT);
}

#[test]
fn an_attach_claiming_a_client_this_session_does_not_hold_mints_a_new_one() {
    let (mut server, client_id) = booted_server();
    let (session_id, tab_id, _pane_id) = booted_parts(&server, client_id);
    let stranger = ClientId::new();

    let accepted = server
        .handle_ipc_attach(
            Some(stranger),
            REMOTE_VIEWPORT,
            EventFilter::All,
            SystemTime::now(),
        )
        .expect("the session mints a client instead of refusing");

    assert_ne!(accepted.client_id, stranger);
    assert_ne!(accepted.client_id, client_id);
    assert_eq!(server.sessions[&session_id].clients.len(), 2);
    let minted = server.sessions[&session_id]
        .clients
        .get(accepted.client_id)
        .expect("the minted record");
    assert_eq!(minted.active_tab(), tab_id);
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
            VIEWPORT,
            EventFilter::All,
            SystemTime::now(),
        )
        .expect("the first attach takes the record");
    assert_eq!(held.client_id, client_id);

    let second = server
        .handle_ipc_attach(
            Some(client_id),
            REMOTE_VIEWPORT,
            EventFilter::All,
            SystemTime::now(),
        )
        .expect("the second attach mints a client instead of refusing");

    assert_ne!(second.client_id, client_id);
    assert_eq!(server.sessions[&session_id].clients.len(), 2);
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
            .filter(|&&held| held == second.client_id)
            .count(),
        1,
        "and the minted record by exactly one other"
    );
    assert_eq!(
        server.sessions[&session_id]
            .clients
            .get(client_id)
            .expect("the record the first attach took")
            .viewport(),
        VIEWPORT,
        "the second attach must not move the first client's viewport"
    );
}

#[test]
fn an_attach_naming_no_client_to_come_back_as_mints_one_on_the_first_tab() {
    let (mut server, client_id) = booted_server();
    let (session_id, tab_id, _pane_id) = booted_parts(&server, client_id);

    let accepted = server
        .handle_ipc_attach(None, REMOTE_VIEWPORT, EventFilter::All, SystemTime::now())
        .expect("the session mints a client");

    assert_ne!(accepted.client_id, client_id);
    assert_eq!(server.sessions[&session_id].clients.len(), 2);
    assert_eq!(
        server.sessions[&session_id]
            .clients
            .get(accepted.client_id)
            .expect("the minted record")
            .active_tab(),
        tab_id
    );
}

#[test]
fn a_resumed_server_starts_with_every_carried_client_awaiting_its_own_attach() {
    let (mut server, client_id) = booted_server();
    let session_id = *server.sessions.keys().next().expect("the booted session");
    let (_header, body) = server.carry_out(session_id, "quiet-lake".to_string(), &[]);
    let (tx, inbox_rx) = mpsc::channel();

    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        inbox_rx,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );

    assert_eq!(resumed.awaiting_reconnect, HashSet::from([client_id]));
    assert!(!resumed.restart_requested());
    assert_eq!(Arc::strong_count(resumed.snapshot_provider()), 1);
    assert_eq!(Arc::strong_count(resumed.storage()), 1);
}

#[test]
fn closing_the_grace_window_detaches_only_the_clients_that_never_came_back() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    let absent = ClientId::new();
    server.handle_client_attach(
        session_id,
        absent,
        VIEWPORT,
        server.sessions[&session_id]
            .clients
            .get(client_id)
            .expect("the booted client")
            .active_tab(),
        SystemTime::now(),
    );
    server.awaiting_reconnect.insert(client_id);
    server.awaiting_reconnect.insert(absent);
    // One of the two came back before the window closed.
    let _held = server
        .handle_ipc_attach(
            Some(client_id),
            VIEWPORT,
            EventFilter::All,
            SystemTime::now(),
        )
        .expect("the record is handed back");

    server.handle_drop_unclaimed_clients(Instant::now());

    let clients = &server.sessions[&session_id].clients;
    assert_eq!(clients.len(), 1);
    assert_eq!(
        clients.get(client_id).map(|client| client.id()),
        Some(client_id)
    );
    assert_eq!(clients.get(absent).map(|client| client.id()), None);
    assert!(server.awaiting_reconnect.is_empty());
}

#[test]
fn closing_the_grace_window_with_nobody_awaited_detaches_nobody() {
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);

    let events = server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(events, Vec::new());
    assert_eq!(server.sessions[&session_id].clients.len(), 1);
}

#[test]
fn a_quit_applied_before_the_swap_is_carried_to_the_next_image() {
    // A quit can land after the clients were told the session is restarting.
    // They are already waiting for the next socket by then, so the swap runs to
    // the end and the next image ends once it has them back — each one reads a
    // real quit instead of a session that stopped answering.
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.quit_requested = true;

    let (_header, body) = server.carry_out(session_id, "quiet-lake".to_string(), &[]);
    assert_eq!(
        body.quit,
        Some(CarriedQuit::Graceful),
        "the carried state records the quit and its kind"
    );

    let (tx, rx) = mpsc::channel();
    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        rx,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );
    assert!(resumed.quit_requested());
    assert!(
        !resumed.immediate_shutdown,
        "a graceful quit stays graceful across the swap"
    );
}

#[test]
fn a_zero_grace_quit_is_still_zero_grace_after_the_swap() {
    // `request_quit` sets the flag and the kind together. Carrying only the
    // flag would turn a caller's zero-grace teardown into a graceful one in the
    // next image, so the kind travels with it.
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.quit_requested = true;
    server.immediate_shutdown = true;

    let (_header, body) = server.carry_out(session_id, "quiet-lake".to_string(), &[]);
    assert_eq!(body.quit, Some(CarriedQuit::Immediate));

    let (tx, rx) = mpsc::channel();
    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        rx,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );
    assert!(resumed.quit_requested());
    assert!(resumed.immediate_shutdown);
}

#[test]
fn a_session_that_still_expects_a_client_back_is_not_ended_by_a_carried_quit() {
    // The clients were told to come back, so the quit waits for them: ending
    // first leaves each one polling a socket that never answers. The window
    // that empties the set is what bounds the wait.
    let (mut server, client_id) = booted_server();
    let (_session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.quit_requested = true;
    server.awaiting_reconnect.insert(ClientId::new());

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
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    assert!(!server.quit_requested());

    let (_header, body) = server.carry_out(session_id, "quiet-lake".to_string(), &[]);
    assert_eq!(body.quit, None);

    let (tx, rx) = mpsc::channel();
    let resumed = Server::resume(
        Arc::new(FakePtyBackend::new()),
        rx,
        tx,
        body,
        HashMap::new(),
        HashMap::new(),
    );
    assert!(!resumed.quit_requested());
}

#[test]
fn a_detach_that_lands_while_a_client_is_awaited_leaves_its_record_alone() {
    // The connection of a client that was told the session is restarting ends,
    // so its detach arrives while the grace window still owns that record. The
    // record has to stay until the window closes, or the client that comes back
    // finds nothing to claim.
    let (mut server, client_id) = booted_server();
    let (session_id, _tab_id, _pane_id) = booted_parts(&server, client_id);
    server.awaiting_reconnect.insert(client_id);

    let events = server.handle_client_detach(client_id);

    assert_eq!(events, Vec::new());
    assert_eq!(server.sessions[&session_id].clients.len(), 1);
    assert_eq!(
        server.sessions[&session_id]
            .clients
            .get(client_id)
            .map(|client| client.id()),
        Some(client_id)
    );
    assert!(server.awaiting_reconnect.contains(&client_id));

    // The window closing is what detaches it, and it takes the record with it.
    server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(server.sessions[&session_id].clients.len(), 0);
    assert!(server.awaiting_reconnect.is_empty());
}

#[test]
fn the_restart_announcement_waits_for_every_client_to_hold_the_frame() {
    // The image is replaced right after this call, and nothing joins the client
    // writing threads. A call that returned early would leave a client whose
    // frame was still on its way, and that client would read end of stream and
    // report the session dead.
    let (mut server, _client_id) = booted_server();
    let notice = Arc::clone(server.ending_notice());
    notice.writer_started();
    let counted = Arc::clone(&notice);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        counted.writer_ended();
    });

    let started = Instant::now();
    server.announce_restarting();
    let waited = started.elapsed();

    assert_eq!(
        notice.raised(),
        Some(SessionEnding::Restarting),
        "the notice must name the frame the clients are told"
    );
    assert_eq!(
        notice.writers_running(),
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
    notice.writer_started();

    let started = Instant::now();
    server.announce_restarting();
    let waited = started.elapsed();

    assert_eq!(
        notice.writers_running(),
        1,
        "the writing thread that never ends must still be counted"
    );
    assert!(
        waited >= CLIENTS_TOLD_LIMIT,
        "the call returned after {waited:?}, before the limit"
    );
    assert!(
        waited < CLIENTS_TOLD_LIMIT * 3,
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
        vec![Delivery::Event(Event::Quit)]
    );
    assert_eq!(
        server.ending_notice().raised(),
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
    server.publish_events(&[Event::Quit]);

    server.announce_quit();

    assert_eq!(
        queue.try_iter().collect::<Vec<_>>(),
        vec![Delivery::Event(Event::Quit)]
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
        server.ending_notice().raised(),
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
    let (session_id, first_tab, _pane_id) = booted_parts(&server, client_id);
    let closed_tab = add_second_tab(&mut server, session_id);
    {
        let session = server.sessions.get_mut(&session_id).expect("the session");
        session
            .clients
            .get_mut(client_id)
            .expect("the booted client")
            .update_active_tab(closed_tab);
        session.tabs.remove(&closed_tab);
    }
    server.awaiting_reconnect.insert(client_id);

    let accepted = server
        .handle_ipc_attach(
            Some(client_id),
            REMOTE_VIEWPORT,
            EventFilter::All,
            SystemTime::now(),
        )
        .expect("the session mints a client instead of refusing");

    assert_ne!(accepted.client_id, client_id);
    assert_eq!(accepted.session_id, session_id);
    assert_eq!(server.sessions[&session_id].clients.len(), 2);
    assert_eq!(
        server.sessions[&session_id]
            .clients
            .get(accepted.client_id)
            .expect("the minted record")
            .active_tab(),
        first_tab
    );
    assert_eq!(
        server.awaiting_reconnect,
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
    assert!(server.restart_requested());
    server.cancel_restart();
    assert!(
        !server.restart_requested(),
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
    assert!(server.restart_requested());
}

#[test]
fn a_carried_client_that_never_came_back_is_detached_even_after_its_tab_was_closed() {
    // The grace window closes on a record whose tab went away while the client
    // was gone. The detach must still take the record off the session rather
    // than leaving it holding a tab nothing can view.
    let (mut server, client_id) = booted_server();
    let (session_id, _first_tab, _pane_id) = booted_parts(&server, client_id);
    let closed_tab = add_second_tab(&mut server, session_id);
    {
        let session = server.sessions.get_mut(&session_id).expect("the session");
        session
            .clients
            .get_mut(client_id)
            .expect("the booted client")
            .update_active_tab(closed_tab);
        session.tabs.remove(&closed_tab);
    }
    server.awaiting_reconnect.insert(client_id);

    server.handle_drop_unclaimed_clients(Instant::now());

    assert_eq!(server.sessions[&session_id].clients.len(), 0);
    assert!(server.awaiting_reconnect.is_empty());
}
