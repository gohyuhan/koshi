//! Tests for building the discovery overview from live session state.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::client::ClientOrigin;
use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::discovery::PaneState;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::ClientRegistry;
use koshi_session::session::state::{Session, Tab};
use koshi_test_support::fake_pty::FakePtyBackend;
use uuid::Uuid;

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
    assert_eq!(overview.tabs[0].session_id, session_id);
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
            tab: None,
            direction: Direction::Right,
            stacked: false,
            client: None,
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

#[test]
fn the_overview_reports_where_each_client_connected_from() {
    let (mut runtime, _tx) = new_runtime();
    let session_id = SessionId::new();
    let now = SystemTime::UNIX_EPOCH;
    let local = runtime
        .bootstrap_local(session_id, VIEWPORT, now)
        .expect("bootstrap");
    let tab = runtime.sessions()[&session_id]
        .tabs
        .keys()
        .next()
        .copied()
        .expect("the genesis tab");
    let remote = ClientId::new();
    runtime.handle_client_attach(session_id, remote, VIEWPORT, tab, now, true);

    let overview = runtime.build_overview().expect("one session is running");
    let origin_of = |id: ClientId| {
        overview
            .clients
            .iter()
            .find(|client| client.id == id)
            .map(|client| client.origin)
    };

    // `koshi share` reads this row and nothing else to decide whether the
    // client that typed it is on this machine.
    assert_eq!(origin_of(local), Some(Some(ClientOrigin::Local)));
    assert_eq!(origin_of(remote), Some(Some(ClientOrigin::Remote)));
}

/// A fixed UUID ending in `tail`, so tab ids sort in a known order.
fn uuid_ending(tail: u8) -> Uuid {
    Uuid::parse_str(&format!("00000000-0000-0000-0000-0000000000{tail:02}"))
        .expect("literal UUID parses")
}

/// A session named `quiet-lake` with no tabs, no panes and no clients.
fn empty_session(session_id: SessionId) -> Session {
    Session::new(
        session_id,
        "quiet-lake".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Add a tab named `name` at bar position `index`, holding one registered
/// `Spawning` pane, and return that pane's id.
fn add_tab_with_pane(session: &mut Session, tab_id: TabId, name: &str, index: usize) -> PaneId {
    let pane_id = PaneId::new();
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::UNIX_EPOCH))
        .expect("a fresh pane id");
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, name.to_string(), index, pane_id));
    pane_id
}

/// Drive `pane_id`'s lifecycle through `events`, in order.
fn advance(session: &mut Session, pane_id: PaneId, events: &[PaneLifecycleEvent]) {
    let record = session.panes.get_mut(pane_id).expect("a registered pane");
    for event in events {
        record.update_lifecycle(*event).expect("a legal step");
    }
}

#[test]
fn tabs_and_their_panes_come_back_in_tab_bar_order_not_in_id_order() {
    // The tab map is keyed by id, so the lower id is visited first; the tab bar
    // puts it second.
    let session_id = SessionId::new();
    let lower = TabId::from_uuid(uuid_ending(1));
    let higher = TabId::from_uuid(uuid_ending(2));
    let mut session = empty_session(session_id);
    let lower_pane = add_tab_with_pane(&mut session, lower, "second", 1);
    let higher_pane = add_tab_with_pane(&mut session, higher, "first", 0);
    let (mut runtime, _tx) = new_runtime();
    runtime.sessions.insert(session_id, session);

    let overview = runtime.build_overview().expect("one session is running");

    let tab_order: Vec<(TabId, usize)> = overview
        .tabs
        .iter()
        .map(|tab| (tab.id, tab.index))
        .collect();
    assert_eq!(tab_order, vec![(higher, 0), (lower, 1)]);

    let pane_order: Vec<(PaneId, TabId)> = overview
        .panes
        .iter()
        .map(|pane| (pane.id, pane.tab_id))
        .collect();
    assert_eq!(
        pane_order,
        vec![(higher_pane, higher), (lower_pane, lower)],
        "panes follow the tab-bar order of the tabs holding them"
    );
}

#[test]
fn each_lifecycle_becomes_its_reported_state_and_a_removed_pane_gets_no_row() {
    let session_id = SessionId::new();
    let mut session = empty_session(session_id);
    let spawning_tab = TabId::from_uuid(uuid_ending(1));
    let exited_tab = TabId::from_uuid(uuid_ending(2));
    let closing_tab = TabId::from_uuid(uuid_ending(3));
    let removed_tab = TabId::from_uuid(uuid_ending(4));
    let spawning = add_tab_with_pane(&mut session, spawning_tab, "spawning", 0);
    let exited = add_tab_with_pane(&mut session, exited_tab, "exited", 1);
    let closing = add_tab_with_pane(&mut session, closing_tab, "closing", 2);
    let removed = add_tab_with_pane(&mut session, removed_tab, "removed", 3);

    let at = SystemTime::UNIX_EPOCH;
    advance(
        &mut session,
        exited,
        &[
            PaneLifecycleEvent::ProcessStarted,
            PaneLifecycleEvent::ProcessExited { code: Some(3), at },
        ],
    );
    advance(
        &mut session,
        closing,
        &[
            PaneLifecycleEvent::ProcessStarted,
            PaneLifecycleEvent::CloseRequested { since: at },
        ],
    );
    advance(
        &mut session,
        removed,
        &[
            PaneLifecycleEvent::ProcessStarted,
            PaneLifecycleEvent::CloseRequested { since: at },
            PaneLifecycleEvent::Cleaned,
        ],
    );

    let (mut runtime, _tx) = new_runtime();
    runtime.sessions.insert(session_id, session);

    let overview = runtime.build_overview().expect("one session is running");

    let rows: Vec<(PaneId, PaneState)> = overview
        .panes
        .iter()
        .map(|pane| (pane.id, pane.state))
        .collect();
    assert_eq!(
        rows,
        vec![
            (spawning, PaneState::Spawning),
            (exited, PaneState::Exited { code: Some(3) }),
            (closing, PaneState::Closing),
        ],
        "a removed pane has left every layout tree, so it produces no row"
    );

    // The registry still holds the removed record, and its tab still counts the
    // layout leaf, so both counts include it.
    assert_eq!(overview.session.pane_count, 4);
    assert_eq!(
        overview
            .tabs
            .iter()
            .find(|tab| tab.id == removed_tab)
            .map(|tab| tab.pane_count),
        Some(1)
    );
}
