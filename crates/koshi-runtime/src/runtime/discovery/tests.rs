//! Tests for building the discovery overview from live session state.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_core::client::ClientOrigin;
use koshi_core::command::{Command, CommandEnvelope, CommandSource};
use koshi_core::discovery::PaneLifecycle;
use koshi_core::geometry::{Direction, PaneArea, Size};
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

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

const VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A bare runtime with stub services and no sessions. The sender is returned
/// so the inbox stays open.
fn build_test_runtime() -> (Server, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (event_sender, event_receiver) = mpsc::channel();
    let server = Server::from_runtime_parts(pty_backend, event_receiver, event_sender.clone());
    (server, event_sender)
}

#[test]
fn no_session_yields_no_overview() {
    let (server, _event_sender) = build_test_runtime();

    assert_eq!(server.build_overview(), None);
}

#[test]
fn bootstrapped_session_reports_its_exact_rows() {
    let (mut server, _event_sender) = build_test_runtime();
    let session_id = SessionId::new();
    let creation_time = SystemTime::UNIX_EPOCH;
    let client_id = server
        .bootstrap_local(session_id, VIEWPORT_SIZE, creation_time)
        .expect("bootstrap");

    let overview = server.build_overview().expect("one session is running");
    let session = &server.list_sessions()[&session_id];

    assert_eq!(overview.session.session_id, session_id);
    assert_eq!(overview.session.session_name, session.session_name);
    assert_eq!(overview.session.created_at, creation_time);
    assert_eq!(overview.session.attached_client_ids, vec![client_id]);
    assert_eq!(overview.session.pane_count, 1);

    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    assert_eq!(overview.tabs.len(), 1);
    assert_eq!(overview.tabs[0].tab_id, tab.get_tab_id());
    assert_eq!(overview.tabs[0].session_id, session_id);
    assert_eq!(overview.tabs[0].tab_name, tab.get_tab_name());
    assert_eq!(overview.tabs[0].tab_index, 0);
    assert_eq!(overview.tabs[0].active_pane_id, Some(pane_id));
    assert_eq!(overview.tabs[0].pane_count, 1);

    assert_eq!(overview.panes.len(), 1);
    assert_eq!(overview.panes[0].pane_id, pane_id);
    assert_eq!(overview.panes[0].tab_id, tab.get_tab_id());
    assert_eq!(overview.panes[0].session_id, session_id);
    assert_eq!(overview.panes[0].lifecycle, PaneLifecycle::Running);
    assert_eq!(overview.panes[0].focused_by_client_ids, vec![client_id]);

    assert_eq!(overview.clients.len(), 1);
    assert_eq!(overview.clients[0].client_id, client_id);
    assert_eq!(overview.clients[0].session_id, session_id);
    assert_eq!(overview.clients[0].attached_at, creation_time);
    assert_eq!(overview.clients[0].viewport_size, VIEWPORT_SIZE);
    assert_eq!(overview.clients[0].active_tab_id, tab.get_tab_id());
    assert_eq!(overview.clients[0].focused_pane_id, Some(pane_id));
    assert_eq!(overview.clients[0].lock_mode, LockMode::Normal);
}

#[test]
fn a_command_pane_reports_its_argv_program_first() {
    let (mut server, _event_sender) = build_test_runtime();
    let session_id = SessionId::new();
    let client_id = server
        .bootstrap_local(session_id, VIEWPORT_SIZE, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let root_pane_id = server.list_sessions()[&session_id]
        .tabs
        .values()
        .next()
        .expect("one tab")
        .get_layout_tree()
        .list_leaf_pane_ids()[0];

    let spawn_spec = SpawnSpec {
        program: "/bin/echo".into(),
        arguments: vec!["hello".to_string(), "world".to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("echo".to_string()),
    };
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::UNIX_EPOCH,
        Command::RunCommandPane(koshi_core::command::RunCommandPaneArgs {
            spawn_spec,
            working_directory: None,
            source_pane_id: Some(root_pane_id),
            tab_id: None,
            direction: Direction::Right,
            should_stack: false,
            client_id: None,
        }),
    );
    let command_result = server.submit_command(command_envelope);
    assert!(
        matches!(
            command_result,
            koshi_core::command::CommandResult::Ok { .. }
        ),
        "the command pane must split: {command_result:?}"
    );

    let overview = server.build_overview().expect("one session is running");
    let command_pane = overview
        .panes
        .iter()
        .find(|pane_summary| pane_summary.pane_id != root_pane_id)
        .expect("the split pane is listed");
    assert_eq!(
        command_pane.command_argv,
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
fn a_pane_reports_the_title_its_child_set_and_no_argv_for_a_shell() {
    let (mut server, _event_sender) = build_test_runtime();
    let session_id = SessionId::new();
    server
        .bootstrap_local(session_id, VIEWPORT_SIZE, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    let pane_id = server.list_sessions()[&session_id]
        .tabs
        .values()
        .next()
        .expect("one tab")
        .get_layout_tree()
        .list_leaf_pane_ids()[0];
    let overview_before_title_update = server.build_overview().expect("one session is running");
    assert_eq!(overview_before_title_update.panes[0].pane_title, None);

    // OSC 2 sets the window title, ended here by BEL.
    server.handle_pty_output(pane_id, b"\x1b]2;build watch\x07");

    let overview_after_title_update = server.build_overview().expect("one session is running");
    assert_eq!(
        overview_after_title_update.panes[0].pane_title,
        Some("build watch".to_string())
    );
    assert_eq!(overview_after_title_update.panes[0].command_argv, None);
}

#[test]
fn a_pane_lists_every_client_focused_on_it_in_client_id_order() {
    let (mut server, _event_sender) = build_test_runtime();
    let session_id = SessionId::new();
    let attachment_time = SystemTime::UNIX_EPOCH;
    let seeded_client_id = server
        .bootstrap_local(session_id, VIEWPORT_SIZE, attachment_time)
        .expect("bootstrap");
    let tab_id = server.list_sessions()[&session_id]
        .tabs
        .keys()
        .next()
        .copied()
        .expect("the genesis tab");
    let joining = ClientId::new();
    server.handle_client_attach(
        session_id,
        joining,
        VIEWPORT_SIZE,
        None,
        tab_id,
        attachment_time,
        false,
    );

    let overview = server.build_overview().expect("one session is running");

    let mut sorted_client_ids = vec![seeded_client_id, joining];
    sorted_client_ids.sort();
    assert_eq!(overview.panes.len(), 1);
    assert_eq!(overview.panes[0].focused_by_client_ids, sorted_client_ids);
    assert_eq!(overview.session.attached_client_ids, sorted_client_ids);
}

#[test]
fn the_overview_reports_where_each_client_connected_from() {
    let (mut server, _event_sender) = build_test_runtime();
    let session_id = SessionId::new();
    let attachment_time = SystemTime::UNIX_EPOCH;
    let local_client_id = server
        .bootstrap_local(session_id, VIEWPORT_SIZE, attachment_time)
        .expect("bootstrap");
    let tab_id = server.list_sessions()[&session_id]
        .tabs
        .keys()
        .next()
        .copied()
        .expect("the genesis tab");
    let remote_client_id = ClientId::new();
    server.handle_client_attach(
        session_id,
        remote_client_id,
        VIEWPORT_SIZE,
        None,
        tab_id,
        attachment_time,
        true,
    );

    let overview = server.build_overview().expect("one session is running");
    let find_client_origin = |client_id: ClientId| {
        overview
            .clients
            .iter()
            .find(|client_record| client_record.client_id == client_id)
            .map(|client_record| client_record.origin)
    };

    // `koshi share` reads this row and nothing else to decide whether the
    // client that typed it is on this machine.
    assert_eq!(
        find_client_origin(local_client_id),
        Some(Some(ClientOrigin::Local))
    );
    assert_eq!(
        find_client_origin(remote_client_id),
        Some(Some(ClientOrigin::Remote))
    );
}

/// The overview carries the client's report exactly as it arrived, next to
/// the raw terminal viewport it was reported alongside.
#[test]
fn discovery_reports_the_raw_pane_area() {
    let (mut server, _event_sender) = build_test_runtime();
    let session_id = SessionId::new();
    let attachment_time = SystemTime::UNIX_EPOCH;
    let seeded_client_id = server
        .bootstrap_local(session_id, VIEWPORT_SIZE, attachment_time)
        .expect("bootstrap");
    let tab_id = server.list_sessions()[&session_id]
        .tabs
        .keys()
        .next()
        .copied()
        .expect("the genesis tab");
    let reporting_client_id = ClientId::new();
    let reported_pane_area = PaneArea::Reported(Size {
        column_count: 60,
        row_count: 20,
    });
    server.handle_client_attach(
        session_id,
        reporting_client_id,
        VIEWPORT_SIZE,
        Some(reported_pane_area),
        tab_id,
        attachment_time,
        false,
    );

    let overview = server.build_overview().expect("one session is running");
    let get_client_discovery_row = |client_id: ClientId| {
        overview
            .clients
            .iter()
            .find(|client_record| client_record.client_id == client_id)
            .expect("the client is listed")
    };

    assert_eq!(
        get_client_discovery_row(reporting_client_id).pane_area,
        Some(reported_pane_area)
    );
    assert_eq!(
        get_client_discovery_row(reporting_client_id).viewport_size,
        VIEWPORT_SIZE
    );
    // The seeded client reported nothing, and the row says so.
    assert_eq!(get_client_discovery_row(seeded_client_id).pane_area, None);
    assert_eq!(
        get_client_discovery_row(seeded_client_id).viewport_size,
        VIEWPORT_SIZE
    );
}

/// A fixed UUID ending in `tail`, so tab ids sort in a known order.
fn build_test_uuid_with_suffix(suffix_byte: u8) -> Uuid {
    Uuid::parse_str(&format!(
        "00000000-0000-0000-0000-0000000000{suffix_byte:02}"
    ))
    .expect("literal UUID parses")
}

/// A session named `quiet-lake` with no tabs, no panes and no clients.
fn build_empty_session(session_id: SessionId) -> Session {
    Session::from_identity_and_client_registry(
        session_id,
        "quiet-lake".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Add a tab named `tab_name` at bar position `tab_index`, holding one registered
/// `Spawning` pane, and return that pane's id.
fn register_session_tab_with_pane(
    session: &mut Session,
    tab_id: TabId,
    tab_name: &str,
    tab_index: usize,
) -> PaneId {
    let pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            pane_id,
            SystemTime::UNIX_EPOCH,
        ))
        .expect("a fresh pane id");
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, tab_name.to_string(), tab_index, pane_id),
    );
    pane_id
}

/// Drive `pane_id`'s lifecycle through `lifecycle_events`, in order.
fn advance_pane_lifecycle(
    session: &mut Session,
    pane_id: PaneId,
    lifecycle_events: &[PaneLifecycleEvent],
) {
    let pane_record = session
        .panes
        .get_pane_record_mut_by_id(pane_id)
        .expect("a registered pane");
    for lifecycle_event in lifecycle_events {
        pane_record
            .update_lifecycle(*lifecycle_event)
            .expect("a legal step");
    }
}

#[test]
fn tabs_and_their_panes_come_back_in_tab_bar_order_not_in_id_order() {
    // The tab map is keyed by id, so the lower id is visited first; the tab bar
    // puts it second.
    let session_id = SessionId::new();
    let lower_tab_id = TabId::from_uuid(build_test_uuid_with_suffix(1));
    let higher_tab_id = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let mut session = build_empty_session(session_id);
    let lower_pane_id = register_session_tab_with_pane(&mut session, lower_tab_id, "second", 1);
    let higher_pane_id = register_session_tab_with_pane(&mut session, higher_tab_id, "first", 0);
    let (mut server, _event_sender) = build_test_runtime();
    server.session_by_id.insert(session_id, session);

    let overview = server.build_overview().expect("one session is running");

    let tab_order: Vec<(TabId, usize)> = overview
        .tabs
        .iter()
        .map(|tab_summary| (tab_summary.tab_id, tab_summary.tab_index))
        .collect();
    assert_eq!(tab_order, vec![(higher_tab_id, 0), (lower_tab_id, 1)]);

    let pane_order: Vec<(PaneId, TabId)> = overview
        .panes
        .iter()
        .map(|pane_summary| (pane_summary.pane_id, pane_summary.tab_id))
        .collect();
    assert_eq!(
        pane_order,
        vec![
            (higher_pane_id, higher_tab_id),
            (lower_pane_id, lower_tab_id)
        ],
        "panes follow the tab-bar order of the tabs holding them"
    );
}

#[test]
fn a_registered_pane_no_tab_layout_holds_gets_no_row_but_is_still_counted() {
    let session_id = SessionId::new();
    let mut session = build_empty_session(session_id);
    let held_pane_id = register_session_tab_with_pane(
        &mut session,
        TabId::from_uuid(build_test_uuid_with_suffix(1)),
        "only",
        0,
    );
    let unlisted_pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            unlisted_pane_id,
            SystemTime::UNIX_EPOCH,
        ))
        .expect("a fresh pane id");
    let (mut server, _event_sender) = build_test_runtime();
    server.session_by_id.insert(session_id, session);

    let overview = server.build_overview().expect("one session is running");

    let pane_ids: Vec<PaneId> = overview
        .panes
        .iter()
        .map(|pane_summary| pane_summary.pane_id)
        .collect();
    assert_eq!(pane_ids, vec![held_pane_id]);
    assert_eq!(overview.session.pane_count, 2);
}

#[test]
fn a_layout_leaf_the_registry_does_not_hold_gets_no_row_but_is_still_counted() {
    let session_id = SessionId::new();
    let mut session = build_empty_session(session_id);
    let registered_pane_id = register_session_tab_with_pane(
        &mut session,
        TabId::from_uuid(build_test_uuid_with_suffix(1)),
        "first",
        0,
    );
    let stray_tab = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let stray_pane_id = PaneId::new();
    session.tabs.insert(
        stray_tab,
        Tab::from_root_pane(stray_tab, "second".to_string(), 1, stray_pane_id),
    );
    let (mut server, _event_sender) = build_test_runtime();
    server.session_by_id.insert(session_id, session);

    let overview = server.build_overview().expect("one session is running");

    let pane_ids: Vec<PaneId> = overview
        .panes
        .iter()
        .map(|pane_summary| pane_summary.pane_id)
        .collect();
    assert_eq!(pane_ids, vec![registered_pane_id]);
    assert_eq!(
        overview
            .tabs
            .iter()
            .find(|tab_summary| tab_summary.tab_id == stray_tab)
            .map(|tab_summary| tab_summary.pane_count),
        Some(1)
    );
    assert_eq!(overview.session.pane_count, 1);
}

#[test]
fn each_lifecycle_becomes_its_reported_state_and_a_removed_pane_gets_no_row() {
    let session_id = SessionId::new();
    let mut session = build_empty_session(session_id);
    let spawning_tab = TabId::from_uuid(build_test_uuid_with_suffix(1));
    let exited_tab = TabId::from_uuid(build_test_uuid_with_suffix(2));
    let closing_tab = TabId::from_uuid(build_test_uuid_with_suffix(3));
    let removed_tab = TabId::from_uuid(build_test_uuid_with_suffix(4));
    let spawning_pane_id =
        register_session_tab_with_pane(&mut session, spawning_tab, "spawning", 0);
    let exited_pane_id = register_session_tab_with_pane(&mut session, exited_tab, "exited", 1);
    let closing_pane_id = register_session_tab_with_pane(&mut session, closing_tab, "closing", 2);
    let removed_pane_id = register_session_tab_with_pane(&mut session, removed_tab, "removed", 3);

    let lifecycle_time = SystemTime::UNIX_EPOCH;
    advance_pane_lifecycle(
        &mut session,
        exited_pane_id,
        &[
            PaneLifecycleEvent::ProcessStarted,
            PaneLifecycleEvent::ProcessExited {
                exit_code: Some(3),
                exited_at: lifecycle_time,
            },
        ],
    );
    advance_pane_lifecycle(
        &mut session,
        closing_pane_id,
        &[
            PaneLifecycleEvent::ProcessStarted,
            PaneLifecycleEvent::CloseRequested {
                close_requested_at: lifecycle_time,
            },
        ],
    );
    advance_pane_lifecycle(
        &mut session,
        removed_pane_id,
        &[
            PaneLifecycleEvent::ProcessStarted,
            PaneLifecycleEvent::CloseRequested {
                close_requested_at: lifecycle_time,
            },
            PaneLifecycleEvent::Cleaned,
        ],
    );

    let (mut server, _event_sender) = build_test_runtime();
    server.session_by_id.insert(session_id, session);

    let overview = server.build_overview().expect("one session is running");

    let pane_lifecycles: Vec<(PaneId, PaneLifecycle)> = overview
        .panes
        .iter()
        .map(|pane_summary| (pane_summary.pane_id, pane_summary.lifecycle))
        .collect();
    assert_eq!(
        pane_lifecycles,
        vec![
            (spawning_pane_id, PaneLifecycle::Spawning),
            (exited_pane_id, PaneLifecycle::Exited { exit_code: Some(3) }),
            (closing_pane_id, PaneLifecycle::Closing),
        ],
        "a pane whose lifecycle is Removed produces no row"
    );

    // The registry still holds the removed pane record, and its tab still holds the
    // layout leaf. Both counts include it.
    assert_eq!(overview.session.pane_count, 4);
    assert_eq!(
        overview
            .tabs
            .iter()
            .find(|tab_summary| tab_summary.tab_id == removed_tab)
            .map(|tab_summary| tab_summary.pane_count),
        Some(1)
    );
}
