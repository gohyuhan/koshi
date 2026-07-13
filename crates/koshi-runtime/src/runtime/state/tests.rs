//! Tests for the runtime state container: construction defaults, the held
//! service handles, the wired event inbox, and a session with one tab and one
//! pane.

use std::sync::mpsc;
use std::time::SystemTime;

use koshi_core::geometry::Direction;
use koshi_core::ids::TabId;
use koshi_core::process::PtySize;
use koshi_pane::pane::state::PaneRecord;
use koshi_session::client::ClientRegistry;
use koshi_session::session::state::Tab;
use koshi_test_support::fake_pty::FakePtyBackend;

use super::*;
use crate::placeholder::{NullSnapshotProvider, NullStorage};

fn new_runtime() -> (Runtime, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        TerminalCleanupGuard::new(),
        Direction::Right,
    );
    (runtime, tx)
}

#[test]
fn new_runtime_starts_with_no_sessions_or_engines() {
    let (rt, _tx) = new_runtime();

    assert!(rt.sessions().is_empty());
    assert!(rt.terminal_engines().is_empty());
    assert!(rt.ipc_server().is_none());
}

#[test]
fn accessors_return_the_constructed_services() {
    let (rt, _tx) = new_runtime();

    assert_eq!(Arc::strong_count(rt.pty_backend()), 1);
    assert_eq!(Arc::strong_count(rt.snapshot_provider()), 1);
    assert_eq!(Arc::strong_count(rt.storage()), 1);
    let _ = rt.event_bus();
    let _ = rt.cleanup_guard();
}

#[test]
fn inbox_delivers_events_to_the_receiver() {
    let (rt, tx) = new_runtime();

    tx.send(RuntimeEvent::Timer).expect("send to inbox");

    assert_eq!(rt.inbox_rx().try_recv(), Ok(RuntimeEvent::Timer));
}

#[test]
fn holds_one_session_with_one_tab_and_pane() {
    let (mut rt, _tx) = new_runtime();

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
fn a_fresh_runtime_has_no_draining_or_quit_flags_set() {
    let (rt, _tx) = new_runtime();

    assert!(!rt.is_draining());
    assert!(!rt.quit_requested());
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
    let rt = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx,
        TerminalCleanupGuard::new(),
        Direction::Down,
    );

    assert_eq!(rt.config.layout.new_pane_direction, Direction::Down);
}
