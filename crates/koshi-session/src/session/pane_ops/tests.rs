//! Tests for the pane state ops.
//!
//! For the NewPane commit, each test builds a one-pane session, prepares a
//! split candidate layout tree with [`split_leaf`] (as the runtime does), and applies it
//! with [`commit_new_pane`], asserting the emitted events, the post-split
//! layout tree, the registered pane, and the client's focus. Fit preflight and
//! source-pane resolution lives in the runtime and is covered by the runtime's
//! tests.

use super::*;

use std::collections::BTreeMap;
use std::time::SystemTime;

use koshi_core::constant::MAX_FLOATING_PANES_PER_SESSION;
use koshi_core::geometry::{Direction, Point, Size, SplitDirection};
use koshi_core::ids::SessionId;
use koshi_core::process::ShellKind;
use koshi_layout::edit::split_leaf;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::SplitNode;
use koshi_pane::pane::lifecycle::PaneLifecycle;

use crate::client::{Client, ClientOrigin, ClientRegistry};
use crate::session::state::tests::build_default_floating_member;
use crate::session::state::Tab;

const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A session with one tab holding a single leaf `pane_id`, plus one attached client
/// viewing that tab with `pane_id` focused. Returns the session and its ids.
fn build_single_pane_session() -> (Session, TabId, PaneId, ClientId) {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "code".to_owned(), 0, pane_id),
    );
    let _ = session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id));

    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        TEST_VIEWPORT_SIZE,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);

    (session, tab_id, pane_id, client_id)
}

/// Mint the new pane's id and build the candidate layout tree for splitting `source_pane_id` in
/// `tab_id`, exactly as the runtime does before committing.
fn prepare_split_candidate(
    session: &Session,
    tab_id: TabId,
    source_pane_id: PaneId,
    direction: Direction,
) -> (PaneId, LayoutNode) {
    let new_pane_id = PaneId::new();
    let candidate_layout_tree = split_leaf(
        session
            .tabs
            .get(&tab_id)
            .expect("tab id identifies the registered tab")
            .get_layout_tree(),
        source_pane_id,
        new_pane_id,
        direction,
    )
    .expect("source_pane_id is a leaf");
    (new_pane_id, candidate_layout_tree)
}

#[test]
fn commit_with_an_unknown_tab_registers_nothing_and_emits_nothing() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        TabId::new(),
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(previous_tab_id, None);
    assert_eq!(session_events, Vec::new());
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(new_pane_id)
            .map(PaneRecord::get_pane_id),
        None
    );
    assert_eq!(session.panes.count_pane_records(), 1);
    assert_eq!(
        session
            .tabs
            .get(&tab_id)
            .expect("tab id identifies the registered tab")
            .get_layout_tree(),
        &LayoutNode::Pane(source_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_emits_events_swaps_the_tree_and_focuses_the_new_pane() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );
    assert_eq!(
        session_events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: new_pane_id,
                previous_pane_id: Some(source_pane_id),
            }),
        ]
    );

    // The candidate layout tree was swapped in: a horizontal split, source pane first.
    assert_eq!(
        session
            .tabs
            .get(&tab_id)
            .expect("tab id identifies the registered tab")
            .get_layout_tree(),
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(source_pane_id),
                LayoutNode::Pane(new_pane_id),
            ],
        ))
    );

    // The new pane is registered `Running`, focused, and at the front of the
    // tab's focus history.
    assert_eq!(session.panes.count_pane_records(), 2);
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(new_pane_id)
            .expect("pane record")
            .get_lifecycle(),
        PaneLifecycle::Running,
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(tab_id),
        Some(new_pane_id),
    );
    assert_eq!(
        session
            .tabs
            .get(&tab_id)
            .expect("tab id identifies the registered tab")
            .list_focus_mru()
            .first(),
        Some(&new_pane_id),
    );
}

#[test]
fn commit_switches_a_client_from_another_tab_and_reports_the_previous() {
    // Two tabs; the client is viewing tab A but the split lands in tab B.
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.tabs.insert(
        first_tab_id,
        Tab::from_root_pane(first_tab_id, "a".to_owned(), 0, first_pane_id),
    );
    session.tabs.insert(
        second_tab_id,
        Tab::from_root_pane(second_tab_id, "b".to_owned(), 1, second_pane_id),
    );
    let _ = session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(first_pane_id));
    let _ = session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(second_pane_id));
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        TEST_VIEWPORT_SIZE,
        None,
        first_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(first_tab_id, first_pane_id);
    session.attach_client(client);

    let new_pane_id = PaneId::new();
    let candidate_layout_tree = split_leaf(
        session
            .tabs
            .get(&second_tab_id)
            .expect("second tab id identifies the registered tab")
            .get_layout_tree(),
        second_pane_id,
        new_pane_id,
        Direction::Right,
    )
    .expect("source_pane_id is a leaf");

    let (previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        second_tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    // The client was not viewing tab B, so it is switched there, tab A comes
    // back as the previous tab, and the client focuses the new pane.
    // `TabFocused` is emitted ahead of `PaneCreated`.
    assert_eq!(previous_tab_id, Some(first_tab_id));
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_active_tab_id(),
        second_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(second_tab_id),
        Some(new_pane_id),
    );
    assert_eq!(
        session_events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: second_tab_id,
                previous_tab_id: first_tab_id,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(second_tab_id),
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: second_tab_id
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(second_tab_id),
                pane_id: new_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
}

#[test]
fn commit_without_a_focus_client_emits_no_focus_event() {
    let (mut session, tab_id, source_pane_id, _client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        None,
        NewPaneSpec::default(),
    );
    assert_eq!(
        session_events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
    // No focus was claimed, so nothing entered the tab's focus history.
    assert_eq!(session.panes.count_pane_records(), 2);
    assert!(session
        .tabs
        .get(&tab_id)
        .expect("tab id identifies the registered tab")
        .list_focus_mru()
        .is_empty());
}

#[test]
fn commit_leaves_the_session_consistent() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_with_the_default_spec_leaves_the_new_pane_without_a_working_directory_or_spawn_spec() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    let pane_record = session
        .panes
        .get_pane_record_by_id(new_pane_id)
        .expect("pane record");
    assert_eq!(pane_record.working_directory, None);
    assert_eq!(pane_record.spawn_spec, None);
}

#[test]
fn commit_puts_the_new_pane_ahead_of_the_existing_focus_history() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab id identifies the registered tab")
        .record_focus_mru(source_pane_id);
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .tabs
            .get(&tab_id)
            .expect("tab id identifies the registered tab")
            .list_focus_mru(),
        [new_pane_id, source_pane_id].as_slice()
    );
}

/// With no acting client, every attached client's zoom of the split tab drops,
/// including the zoom of a client that is watching a different tab.
#[test]
fn commit_with_no_acting_client_drops_the_tabs_zoom_for_a_client_on_another_tab() {
    let (mut session, tab_id, source_pane_id, _client_id) = build_single_pane_session();
    let other_tab_id = TabId::new();
    let other_pane_id = PaneId::new();
    session.tabs.insert(
        other_tab_id,
        Tab::from_root_pane(other_tab_id, "other".to_owned(), 1, other_pane_id),
    );
    let _ = session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(other_pane_id));

    let onlooker_client_id = ClientId::new();
    let mut onlooker_client = Client::from_attachment(
        onlooker_client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        TEST_VIEWPORT_SIZE,
        None,
        other_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    onlooker_client.update_focused_pane(other_tab_id, other_pane_id);
    onlooker_client.zoom_pane(tab_id, source_pane_id);
    session.attach_client(onlooker_client);

    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(onlooker_client_id)
            .expect("onlooker client")
            .get_layout_mode(tab_id),
        LayoutMode::Tiled
    );
    // The onlooker keeps the tab it was watching and the focus it had there.
    assert_eq!(
        session
            .clients
            .get_client_by_id(onlooker_client_id)
            .expect("onlooker client")
            .get_active_tab_id(),
        other_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(onlooker_client_id)
            .expect("onlooker client")
            .get_focused_pane_id(other_tab_id),
        Some(other_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_records_working_directory_and_spawn_spec_on_the_new_pane() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);
    let working_directory = PathBuf::from("/work");
    let spawn_spec = SpawnSpec {
        program: PathBuf::from("/usr/bin/htop"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("htop".to_owned()),
    };

    let _ = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec {
            working_directory: Some(working_directory.clone()),
            spawn_spec: Some(spawn_spec.clone()),
        },
    );
    let pane_record = session
        .panes
        .get_pane_record_by_id(new_pane_id)
        .expect("pane record");
    assert_eq!(
        pane_record.working_directory.as_deref(),
        Some(working_directory.as_path())
    );
    assert_eq!(pane_record.spawn_spec.as_ref(), Some(&spawn_spec));
}

#[test]
fn commit_with_a_stale_focus_client_claims_no_focus() {
    let (mut session, tab_id, source_pane_id, _client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);
    let stale_client_id = ClientId::new(); // never attached to this session

    let (_previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(stale_client_id),
        NewPaneSpec::default(),
    );
    // The named client is not attached, so nothing is focused: no PaneFocused,
    // and no focus-MRU entry claims a focus that never happened.
    assert_eq!(
        session_events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
    assert_eq!(session.panes.count_pane_records(), 2);
    assert!(session
        .tabs
        .get(&tab_id)
        .expect("tab id identifies the registered tab")
        .list_focus_mru()
        .is_empty());
}

/// A commit that names no acting client drops every attached client's zoom of
/// the split tab.
#[test]
fn commit_with_no_acting_client_drops_every_zoom_of_the_tab() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .zoom_pane(tab_id, source_pane_id);

    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(tab_id),
        LayoutMode::Tiled,
        "the zoom would have hidden the pane the caller just asked for"
    );
}

/// Splitting drops the zoom of the client that split, and only that client's. A
/// second client zoomed on a pane of the same tab keeps its zoom.
#[test]
fn commit_drops_the_splitting_clients_zoom_and_no_others() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .zoom_pane(tab_id, source_pane_id);

    let onlooker_client_id = ClientId::new();
    let mut onlooker_client = Client::from_attachment(
        onlooker_client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    onlooker_client.update_focused_pane(tab_id, source_pane_id);
    onlooker_client.zoom_pane(tab_id, source_pane_id);
    session.attach_client(onlooker_client);

    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(tab_id),
        LayoutMode::Tiled,
        "the splitting client returns to the tiled view its new pane lives in"
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(onlooker_client_id)
            .expect("onlooker client")
            .get_layout_mode(tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: source_pane_id
        },
        "the other client's zoom is not disturbed by someone else's split"
    );
}

/// Committing under a pane id the registry already holds keeps the existing
/// pane record: its cwd and command stay as they were, and no second pane record appears.
#[test]
fn committing_a_pane_id_already_registered_keeps_the_original_record() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    session
        .panes
        .get_pane_record_mut_by_id(source_pane_id)
        .expect("pane record")
        .working_directory = Some(PathBuf::from("/original"));
    // The tree is committed unchanged; only the registry path is under test.
    let candidate_layout_tree = session
        .tabs
        .get(&tab_id)
        .expect("tab id identifies the registered tab")
        .get_layout_tree()
        .clone();

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        source_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec {
            working_directory: Some(PathBuf::from("/replacement")),
            spawn_spec: None,
        },
    );

    assert_eq!(session.panes.count_pane_records(), 1);
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(source_pane_id)
            .expect("pane record")
            .working_directory,
        Some(PathBuf::from("/original"))
    );
}

/// A split in the tab the client is already viewing switches no view, so the
/// caller is handed no previous tab to reflow and no [`Event::TabFocused`] is
/// emitted. Only a client pulled across from another tab reports one.
#[test]
fn commit_reports_no_previous_tab_when_the_client_already_views_the_tab() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(previous_tab_id, None);
    assert_eq!(
        session_events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: new_pane_id,
                previous_pane_id: Some(source_pane_id),
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_active_tab_id(),
        tab_id
    );
}

/// Add a second tab holding a single leaf, register that leaf's pane record, and zoom
/// `client_id` on it. Returns the new tab and pane ids.
fn add_second_tab_zoomed_by(session: &mut Session, client_id: ClientId) -> (TabId, PaneId) {
    let other_tab_id = TabId::new();
    let other_pane_id = PaneId::new();
    session.tabs.insert(
        other_tab_id,
        Tab::from_root_pane(other_tab_id, "other".to_owned(), 1, other_pane_id),
    );
    let _ = session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(other_pane_id));
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .zoom_pane(other_tab_id, other_pane_id);
    (other_tab_id, other_pane_id)
}

/// The splitting client's zoom drops for the split tab only; its zoom of a
/// different tab stays up.
#[test]
fn commit_leaves_the_splitting_clients_zoom_of_another_tab_alone() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (other_tab_id, other_pane_id) = add_second_tab_zoomed_by(&mut session, client_id);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .zoom_pane(tab_id, source_pane_id);

    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(other_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: other_pane_id
        }
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

/// With no acting client the zoom drop is still scoped to the split tab; a zoom
/// of a different tab stays up.
#[test]
fn commit_with_no_acting_client_leaves_a_zoom_of_another_tab_alone() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let (other_tab_id, other_pane_id) = add_second_tab_zoomed_by(&mut session, client_id);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .zoom_pane(tab_id, source_pane_id);

    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(tab_id),
        LayoutMode::Tiled
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(other_tab_id),
        LayoutMode::Fullscreen {
            focused_pane_id: other_pane_id
        }
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

/// Splitting twice in the same tab registers both panes and leaves the focus
/// history newest first. The split source pane never enters the history: only the
/// pane each commit creates does.
#[test]
fn a_second_commit_puts_the_newest_pane_at_the_front_of_the_history() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();

    let (first_new_pane_id, first_candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);
    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        first_new_pane_id,
        tab_id,
        first_candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    let (second_new_pane_id, second_candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, first_new_pane_id, Direction::Down);
    let (_previous_tab_id, _session_events) = commit_new_pane(
        &mut session,
        second_new_pane_id,
        tab_id,
        second_candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(session.panes.count_pane_records(), 3);
    assert_eq!(
        session
            .tabs
            .get(&tab_id)
            .expect("tab id identifies the registered tab")
            .list_focus_mru(),
        [second_new_pane_id, first_new_pane_id].as_slice()
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane_id(tab_id),
        Some(second_new_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

/// A client holding no focus in the tab gets `previous_pane_id: None` on the
/// [`Event::PaneFocused`] the commit emits.
#[test]
fn commit_reports_no_previous_pane_id_when_the_client_has_no_focused_pane() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .remove_focused_pane(tab_id);
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(
        session_events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: new_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
}

#[test]
fn commit_new_pane_takes_over_the_input_from_a_focused_floating_pane() {
    let (mut session, tab_id, source_pane_id, client_id) = build_single_pane_session();
    let floating_member = build_default_floating_member(PaneId::new());
    let floating_pane_id = floating_member.pane_id;
    commit_new_floating_pane(
        &mut session,
        floating_member,
        Some((client_id, FloatingPanePosition::Default)),
        NewPaneSpec::default(),
    )
    .expect("the floating set has room");
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, tab_id, source_pane_id, Direction::Right);

    let (_previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(
        session_events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: new_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");
    assert_eq!(client.get_focused_floating_pane_id(), None);
    assert_eq!(client.get_active_focused_pane_id(), Some(new_pane_id));
    assert_eq!(client.list_floating_pane_focus_order(), [floating_pane_id]);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_new_pane_in_another_tab_takes_over_the_input_from_a_focused_floating_pane() {
    let (mut session, first_tab_id, _first_pane_id, client_id) = build_single_pane_session();
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    session.tabs.insert(
        second_tab_id,
        Tab::from_root_pane(second_tab_id, "b".to_owned(), 1, second_pane_id),
    );
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(second_pane_id))
        .expect("the second pane id is new");
    let floating_member = build_default_floating_member(PaneId::new());
    let floating_pane_id = floating_member.pane_id;
    commit_new_floating_pane(
        &mut session,
        floating_member,
        Some((client_id, FloatingPanePosition::Default)),
        NewPaneSpec::default(),
    )
    .expect("the floating set has room");
    let (new_pane_id, candidate_layout_tree) =
        prepare_split_candidate(&session, second_tab_id, second_pane_id, Direction::Right);

    let (previous_tab_id, session_events) = commit_new_pane(
        &mut session,
        new_pane_id,
        second_tab_id,
        candidate_layout_tree,
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(previous_tab_id, Some(first_tab_id));
    assert_eq!(
        session_events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: second_tab_id,
                previous_tab_id: first_tab_id,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(second_tab_id),
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: second_tab_id
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(second_tab_id),
                pane_id: new_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");
    assert_eq!(client.get_focused_floating_pane_id(), None);
    assert_eq!(client.get_active_focused_pane_id(), Some(new_pane_id));
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_new_floating_pane_for_a_client_that_is_not_attached_stores_no_view() {
    let (mut session, _, _, client_id) = build_single_pane_session();
    let floating_member = build_default_floating_member(PaneId::new());

    let commit_result = commit_new_floating_pane(
        &mut session,
        floating_member,
        Some((
            ClientId::new(),
            FloatingPanePosition::Pinned(Point { column: 5, row: 2 }),
        )),
        NewPaneSpec::default(),
    );

    assert_eq!(
        commit_result,
        Ok(vec![Event::PaneCreated(PaneCreated {
            pane_id: floating_member.pane_id,
            tab_id: None,
        })])
    );
    assert_eq!(session.floating_set.list_members(), [floating_member]);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached")
            .list_floating_pane_views(),
        &std::collections::HashMap::new()
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_new_floating_pane_registers_the_member_and_focuses_it_for_the_designated_client() {
    let (mut session, tab_id, tiled_pane_id, client_id) = build_single_pane_session();
    let floating_member = build_default_floating_member(PaneId::new());
    let floating_pane_id = floating_member.pane_id;

    let commit_result = commit_new_floating_pane(
        &mut session,
        floating_member,
        Some((
            client_id,
            FloatingPanePosition::Pinned(Point { column: 5, row: 2 }),
        )),
        NewPaneSpec::default(),
    );

    assert_eq!(
        commit_result,
        Ok(vec![
            Event::PaneCreated(PaneCreated {
                pane_id: floating_pane_id,
                tab_id: None,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: None,
                pane_id: floating_pane_id,
                previous_pane_id: Some(tiled_pane_id),
            }),
        ])
    );
    assert_eq!(session.floating_set.list_members(), [floating_member]);
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(floating_pane_id)
            .expect("the floating pane is registered")
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");
    assert_eq!(
        client.get_floating_pane_view(floating_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Pinned(Point { column: 5, row: 2 }),
            is_minimized: false,
        }
    );
    assert_eq!(client.list_floating_pane_focus_order(), [floating_pane_id]);
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(floating_pane_id)
    );
    assert_eq!(client.get_focused_pane_id(tab_id), Some(tiled_pane_id));
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(tiled_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_new_floating_pane_at_the_default_position_stores_no_view() {
    let (mut session, _, tiled_pane_id, client_id) = build_single_pane_session();
    let floating_member = build_default_floating_member(PaneId::new());

    let commit_result = commit_new_floating_pane(
        &mut session,
        floating_member,
        Some((client_id, FloatingPanePosition::Default)),
        NewPaneSpec::default(),
    );

    assert_eq!(
        commit_result,
        Ok(vec![
            Event::PaneCreated(PaneCreated {
                pane_id: floating_member.pane_id,
                tab_id: None,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: None,
                pane_id: floating_member.pane_id,
                previous_pane_id: Some(tiled_pane_id),
            }),
        ])
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached")
            .list_floating_pane_views(),
        &std::collections::HashMap::new()
    );
}

#[test]
fn commit_new_floating_pane_into_a_full_floating_set_changes_nothing() {
    let (mut session, _, _, client_id) = build_single_pane_session();
    for _ in 0..MAX_FLOATING_PANES_PER_SESSION {
        commit_new_floating_pane(
            &mut session,
            build_default_floating_member(PaneId::new()),
            None,
            NewPaneSpec::default(),
        )
        .expect("the floating set has room");
    }
    let floating_members = session.floating_set.list_members().to_vec();
    let refused_member = build_default_floating_member(PaneId::new());

    let commit_result = commit_new_floating_pane(
        &mut session,
        refused_member,
        Some((
            client_id,
            FloatingPanePosition::Moved(Point { column: 5, row: 2 }),
        )),
        NewPaneSpec::default(),
    );

    assert_eq!(commit_result, Err(FloatingSetError::TooManyPanes));
    assert_eq!(session.floating_set.list_members(), floating_members);
    assert!(session
        .panes
        .get_pane_record_by_id(refused_member.pane_id)
        .is_none());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached")
            .get_floating_pane_view(refused_member.pane_id),
        FloatingPaneView::default()
    );
}
