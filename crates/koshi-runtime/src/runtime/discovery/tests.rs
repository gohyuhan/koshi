//! Tests for building the discovery overview from live session state.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::discovery::PaneState;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{CommandId, SessionId};
use koshi_core::lock::LockMode;
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A bare runtime with stub services and no sessions. The sender is returned
/// so the inbox stays open.
fn new_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        Direction::Right,
    );
    (runtime, tx)
}

#[test]
fn no_session_yields_no_overview() {
    let (runtime, _tx) = new_runtime();

    assert_eq!(runtime.build_overview(), None);
}

#[test]
fn bootstrapped_session_reports_its_exact_rows() {
    let (mut runtime, _tx) = new_runtime();
    let session_id = SessionId::new();
    let now = SystemTime::UNIX_EPOCH;
    let client_id = runtime
        .bootstrap_local(session_id, VIEWPORT, now)
        .expect("bootstrap");

    let overview = runtime.build_overview().expect("one session is running");
    let session = &runtime.sessions()[&session_id];

    assert_eq!(overview.session.id, session_id);
    assert_eq!(overview.session.name, session.name);
    assert_eq!(overview.session.created_at, now);
    assert_eq!(overview.session.attached_clients, vec![client_id]);
    assert_eq!(overview.session.pane_count, 1);

    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.layout().leaf_panes()[0];
    assert_eq!(overview.tabs.len(), 1);
    assert_eq!(overview.tabs[0].id, tab.id());
    assert_eq!(overview.tabs[0].name, tab.name());
    assert_eq!(overview.tabs[0].index, 0);
    assert_eq!(overview.tabs[0].active_pane, Some(pane_id));
    assert_eq!(overview.tabs[0].pane_count, 1);

    assert_eq!(overview.panes.len(), 1);
    assert_eq!(overview.panes[0].id, pane_id);
    assert_eq!(overview.panes[0].tab_id, tab.id());
    assert_eq!(overview.panes[0].session_id, session_id);
    assert_eq!(overview.panes[0].state, PaneState::Running);
    assert_eq!(overview.panes[0].focused_by_clients, vec![client_id]);
    assert_eq!(overview.panes[0].layout_rect, None);

    assert_eq!(overview.clients.len(), 1);
    assert_eq!(overview.clients[0].id, client_id);
    assert_eq!(overview.clients[0].session_id, session_id);
    assert_eq!(overview.clients[0].attached_at, now);
    assert_eq!(overview.clients[0].viewport_size, VIEWPORT);
    assert_eq!(overview.clients[0].active_tab, tab.id());
    assert_eq!(overview.clients[0].focused_pane, Some(pane_id));
    assert_eq!(overview.clients[0].lock_state, LockMode::Normal);
}

#[test]
fn a_command_pane_reports_its_argv_program_first() {
    let (mut runtime, _tx) = new_runtime();
    let session_id = SessionId::new();
    let client_id = runtime
        .bootstrap_local(session_id, VIEWPORT, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let root_pane = runtime.sessions()[&session_id]
        .tabs
        .values()
        .next()
        .expect("one tab")
        .layout()
        .leaf_panes()[0];

    let spec = SpawnSpec {
        program: "/bin/echo".into(),
        args: vec!["hello".to_string(), "world".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("echo".to_string()),
    };
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client_id),
        SystemTime::UNIX_EPOCH,
        Command::RunCommandPane(koshi_core::command::RunCommandPaneArgs {
            command: spec,
            cwd: None,
            source: Some(root_pane),
            direction: Some(Direction::Right),
            stacked: false,
        }),
    );
    let result = runtime.submit_command(envelope);
    assert!(
        matches!(result, koshi_core::command::CommandResult::Ok { .. }),
        "the command pane must split: {result:?}"
    );

    let overview = runtime.build_overview().expect("one session is running");
    let command_pane = overview
        .panes
        .iter()
        .find(|pane| pane.id != root_pane)
        .expect("the split pane is listed");
    assert_eq!(
        command_pane.command,
        Some(vec![
            "/bin/echo".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ]),
    );
    assert_eq!(overview.session.pane_count, 2);
    assert_eq!(overview.tabs[0].pane_count, 2);
}
