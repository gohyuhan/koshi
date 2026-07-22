//! Tests for the server half: construction defaults, the held service
//! handles, the wired event inbox, a session with one tab and one pane, and
//! the two doors — commands in via `submit_command`, events out via
//! `subscribe` — including that detaching a client leaves the server healthy
//! with its panes alive.

use std::sync::mpsc;
use std::time::SystemTime;

use koshi_core::command::{Command, CommandSource};
use koshi_core::event::{InputMode, InputModeChanged};
use koshi_core::geometry::Direction;
use koshi_core::ids::{CommandId, TabId};
use koshi_core::process::PtySize;
use koshi_pane::pane::state::PaneRecord;
use koshi_session::client::ClientRegistry;
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
        Direction::Right,
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
        Command::ToggleLockMode,
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
    let rx = server.subscribe(EventFilter::All);

    let _ = server.submit_command(CommandEnvelope::new(
        CommandId::new(),
        CommandSource::KeyBinding { client_id },
        SystemTime::now(),
        Command::ToggleLockMode,
    ));

    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![Event::InputModeChanged(InputModeChanged {
            client_id,
            mode: InputMode::Locked,
        })]
    );
}

#[test]
fn publish_events_delivers_out_of_command_events_to_subscribers() {
    let (mut server, _client_id) = booted_server();
    let rx = server.subscribe(EventFilter::All);
    let events = vec![Event::Quit];

    server.publish_events(&events);

    assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Event::Quit]);
}

#[test]
fn constructor_seeds_the_app_config_with_the_given_default_new_pane_direction() {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();

    // Every other test in this crate seeds `Direction::Right`; a different
    // value here proves the constructor actually honors its parameter rather
    // than a hardcoded default.
    let rt = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx,
        Direction::Down,
    );

    assert_eq!(rt.config.layout.new_pane_direction, Direction::Down);
}
