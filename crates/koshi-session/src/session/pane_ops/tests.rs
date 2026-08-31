//! Tests for the pane state ops.
//!
//! For the NewPane commit, each test builds a one-pane session, prepares a
//! split candidate with [`split_leaf`] (as the runtime does), and applies it
//! with [`commit_new_pane`], asserting the emitted events, the post-split
//! layout tree, the registered pane, and the client's focus. Fit preflight and
//! source resolution live in the runtime and are covered by the runtime's
//! tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use koshi_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused, TabFocused};
use koshi_core::geometry::{Direction, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_layout::edit::split_leaf;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneRecord;

use super::{commit_new_pane, NewPaneSpec};
use crate::client::{Client, ClientOrigin, ClientRegistry};
use crate::session::state::{Session, Tab};

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A session with one tab holding a single leaf `pane`, plus one attached client
/// viewing that tab with `pane` focused. Returns the session and the ids.
fn session_one_pane() -> (Session, TabId, PaneId, ClientId) {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "code".to_owned(), 0, pane));
    let _ = session
        .panes
        .insert(PaneRecord::new(pane, SystemTime::UNIX_EPOCH));

    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane);
    session.attach_client(client);

    (session, tab_id, pane, client_id)
}

/// Mint the new pane's id and build the candidate tree for splitting `source` in
/// `tab`, exactly as the runtime does before committing.
fn prepared(
    session: &Session,
    tab: TabId,
    source: PaneId,
    direction: Direction,
) -> (PaneId, LayoutNode) {
    let new_id = PaneId::new();
    let candidate = split_leaf(
        session.tabs.get(&tab).expect("tab").layout(),
        source,
        new_id,
        direction,
    )
    .expect("source is a leaf");
    (new_id, candidate)
}

#[test]
fn commit_with_an_unknown_tab_registers_nothing_and_emits_nothing() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (previous, events) = commit_new_pane(
        &mut session,
        new_id,
        TabId::new(),
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(previous, None);
    assert_eq!(events, Vec::new());
    assert_eq!(session.panes.get(new_id).map(PaneRecord::id), None);
    assert_eq!(session.panes.len(), 1);
    assert_eq!(
        session.tabs.get(&tab).expect("tab").layout(),
        &LayoutNode::Pane(source)
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn commit_emits_events_swaps_the_tree_and_focuses_the_new_pane() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PaneFocused(PaneFocused {
                client_id: client,
                tab_id: tab,
                pane_id: new_id,
                prior_pane: Some(source),
            }),
        ]
    );

    // The candidate tree was swapped in: a horizontal split, source first.
    assert_eq!(
        session.tabs.get(&tab).expect("tab").layout(),
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![LayoutNode::Pane(source), LayoutNode::Pane(new_id),],
        ))
    );

    // The new pane is registered `Running`, focused, and at the front of the
    // tab's focus history.
    assert_eq!(session.panes.len(), 2);
    assert_eq!(
        *session.panes.get(new_id).expect("record").lifecycle(),
        PaneLifecycle::Running,
    );
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .focused_pane(tab),
        Some(new_id),
    );
    assert_eq!(
        session.tabs.get(&tab).expect("tab").focus_mru().first(),
        Some(&new_id),
    );
}

#[test]
fn commit_switches_a_client_from_another_tab_and_reports_the_previous() {
    // Two tabs; the client is viewing tab A but the split lands in tab B.
    let tab_a = TabId::new();
    let tab_b = TabId::new();
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let client_id = ClientId::new();
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .tabs
        .insert(tab_a, Tab::new(tab_a, "a".to_owned(), 0, pane_a));
    session
        .tabs
        .insert(tab_b, Tab::new(tab_b, "b".to_owned(), 1, pane_b));
    let _ = session
        .panes
        .insert(PaneRecord::new(pane_a, SystemTime::UNIX_EPOCH));
    let _ = session
        .panes
        .insert(PaneRecord::new(pane_b, SystemTime::UNIX_EPOCH));
    let mut client = Client::new(
        client_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        None,
        tab_a,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_a, pane_a);
    session.attach_client(client);

    let new_id = PaneId::new();
    let candidate = split_leaf(
        session.tabs.get(&tab_b).expect("tab b").layout(),
        pane_b,
        new_id,
        Direction::Right,
    )
    .expect("source is a leaf");

    let (previous, events) = commit_new_pane(
        &mut session,
        new_id,
        tab_b,
        candidate,
        Some(client_id),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    // The client was not viewing tab B, so it is switched there, tab A comes
    // back as the previous tab, and the client focuses the new pane.
    // `TabFocused` is emitted ahead of `PaneCreated`.
    assert_eq!(previous, Some(tab_a));
    assert_eq!(
        session.clients.get(client_id).expect("client").active_tab(),
        tab_b
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_b),
        Some(new_id),
    );
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: tab_b,
                prior_tab: tab_a,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab_b,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab_b }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: tab_b,
                pane_id: new_id,
                prior_pane: None,
            }),
        ]
    );
}

#[test]
fn commit_without_a_focus_client_emits_no_focus_event() {
    let (mut session, tab, source, _client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
    // No focus was claimed, so nothing entered the tab's focus history.
    assert_eq!(session.panes.len(), 2);
    assert!(session.tabs.get(&tab).expect("tab").focus_mru().is_empty());
}

#[test]
fn commit_leaves_the_session_consistent() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn commit_stamps_the_supplied_created_at_on_the_new_pane_record() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);
    let created_at = SystemTime::UNIX_EPOCH + Duration::from_secs(4321);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        created_at,
    );

    assert_eq!(
        session.panes.get(new_id).expect("record").created_at(),
        created_at
    );
}

#[test]
fn commit_with_the_default_spec_leaves_the_new_pane_without_a_cwd_or_command() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    let record = session.panes.get(new_id).expect("record");
    assert_eq!(record.cwd, None);
    assert_eq!(record.command, None);
}

#[test]
fn commit_puts_the_new_pane_ahead_of_the_existing_focus_history() {
    let (mut session, tab, source, client) = session_one_pane();
    session
        .tabs
        .get_mut(&tab)
        .expect("tab")
        .record_focus_mru(source);
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session.tabs.get(&tab).expect("tab").focus_mru(),
        [new_id, source].as_slice()
    );
}

/// With no acting client, every attached client's zoom of the split tab drops,
/// including the zoom of a client that is watching a different tab.
#[test]
fn commit_with_no_acting_client_drops_the_tabs_zoom_for_a_client_on_another_tab() {
    let (mut session, tab, source, _client) = session_one_pane();
    let other_tab = TabId::new();
    let other_pane = PaneId::new();
    session.tabs.insert(
        other_tab,
        Tab::new(other_tab, "other".to_owned(), 1, other_pane),
    );
    let _ = session
        .panes
        .insert(PaneRecord::new(other_pane, SystemTime::UNIX_EPOCH));

    let onlooker_id = ClientId::new();
    let mut onlooker = Client::new(
        onlooker_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        None,
        other_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    onlooker.update_focused_pane(other_tab, other_pane);
    onlooker.zoom_pane(tab, source);
    session.attach_client(onlooker);

    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session
            .clients
            .get(onlooker_id)
            .expect("onlooker")
            .layout_mode(tab),
        LayoutMode::Tiled
    );
    // The onlooker keeps the tab it was watching and the focus it had there.
    assert_eq!(
        session
            .clients
            .get(onlooker_id)
            .expect("onlooker")
            .active_tab(),
        other_tab
    );
    assert_eq!(
        session
            .clients
            .get(onlooker_id)
            .expect("onlooker")
            .focused_pane(other_tab),
        Some(other_pane)
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn commit_records_name_cwd_and_command_on_the_new_pane() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);
    let cwd = PathBuf::from("/work");
    let command = SpawnSpec {
        program: PathBuf::from("/usr/bin/htop"),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Other("htop".to_owned()),
    };

    let _ = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec {
            cwd: Some(cwd.clone()),
            command: Some(command.clone()),
        },
        SystemTime::UNIX_EPOCH,
    );
    let record = session.panes.get(new_id).expect("record");
    assert_eq!(record.cwd.as_deref(), Some(cwd.as_path()));
    assert_eq!(record.command.as_ref(), Some(&command));
}

#[test]
fn commit_with_a_stale_focus_client_claims_no_focus() {
    let (mut session, tab, source, _client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);
    let stale = ClientId::new(); // never attached to this session

    let (_previous, events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(stale),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    // The named client is not attached, so nothing is focused: no PaneFocused,
    // and no focus-MRU entry claims a focus that never happened.
    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
    assert_eq!(session.panes.len(), 2);
    assert!(session.tabs.get(&tab).expect("tab").focus_mru().is_empty());
}

/// A commit that names no acting client drops every attached client's zoom of
/// the split tab.
#[test]
fn commit_with_no_acting_client_drops_every_zoom_of_the_tab() {
    let (mut session, tab, source, client) = session_one_pane();
    session
        .clients
        .get_mut(client)
        .expect("client")
        .zoom_pane(tab, source);

    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .layout_mode(tab),
        LayoutMode::Tiled,
        "the zoom would have hidden the pane the caller just asked for"
    );
}

/// Splitting drops the zoom of the client that split, and only that client's. A
/// second client zoomed on a pane of the same tab keeps its zoom.
#[test]
fn commit_drops_the_splitting_clients_zoom_and_no_others() {
    let (mut session, tab, source, client) = session_one_pane();
    session
        .clients
        .get_mut(client)
        .expect("client")
        .zoom_pane(tab, source);

    let onlooker_id = ClientId::new();
    let mut onlooker = Client::new(
        onlooker_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    onlooker.update_focused_pane(tab, source);
    onlooker.zoom_pane(tab, source);
    session.attach_client(onlooker);

    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .layout_mode(tab),
        LayoutMode::Tiled,
        "the splitting client returns to the tiled view its new pane lives in"
    );
    assert_eq!(
        session
            .clients
            .get(onlooker_id)
            .expect("onlooker")
            .layout_mode(tab),
        LayoutMode::Fullscreen { focused: source },
        "the other client's zoom is not disturbed by someone else's split"
    );
}

/// Committing under a pane id the registry already holds keeps the existing
/// record: its cwd and command stay as they were, and no second record appears.
#[test]
fn committing_a_pane_id_already_registered_keeps_the_original_record() {
    let (mut session, tab, source, client) = session_one_pane();
    session.panes.get_mut(source).expect("record").cwd = Some(PathBuf::from("/original"));
    // The tree is committed unchanged; only the registry path is under test.
    let candidate = session.tabs.get(&tab).expect("tab").layout().clone();

    let (_previous, _events) = commit_new_pane(
        &mut session,
        source,
        tab,
        candidate,
        Some(client),
        NewPaneSpec {
            cwd: Some(PathBuf::from("/replacement")),
            command: None,
        },
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(session.panes.len(), 1);
    assert_eq!(
        session.panes.get(source).expect("record").cwd,
        Some(PathBuf::from("/original"))
    );
}

/// A split in the tab the client is already viewing switches no view, so the
/// caller is handed no previous tab to reflow and no [`Event::TabFocused`] is
/// emitted. Only a client pulled across from another tab reports one.
#[test]
fn commit_reports_no_previous_tab_when_the_client_already_views_the_tab() {
    let (mut session, tab, source, client) = session_one_pane();
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (previous, events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(previous, None);
    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PaneFocused(PaneFocused {
                client_id: client,
                tab_id: tab,
                pane_id: new_id,
                prior_pane: Some(source),
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client).expect("client").active_tab(),
        tab
    );
}

/// Add a second tab holding a single leaf, register that leaf's record, and zoom
/// `client` on it. Returns the new tab and pane ids.
fn second_tab_zoomed_by(session: &mut Session, client: ClientId) -> (TabId, PaneId) {
    let other_tab = TabId::new();
    let other_pane = PaneId::new();
    session.tabs.insert(
        other_tab,
        Tab::new(other_tab, "other".to_owned(), 1, other_pane),
    );
    let _ = session
        .panes
        .insert(PaneRecord::new(other_pane, SystemTime::UNIX_EPOCH));
    session
        .clients
        .get_mut(client)
        .expect("client")
        .zoom_pane(other_tab, other_pane);
    (other_tab, other_pane)
}

/// The splitting client's zoom drops for the split tab only; its zoom of a
/// different tab stays up.
#[test]
fn commit_leaves_the_splitting_clients_zoom_of_another_tab_alone() {
    let (mut session, tab, source, client) = session_one_pane();
    let (other_tab, other_pane) = second_tab_zoomed_by(&mut session, client);
    session
        .clients
        .get_mut(client)
        .expect("client")
        .zoom_pane(tab, source);

    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .layout_mode(tab),
        LayoutMode::Tiled
    );
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .layout_mode(other_tab),
        LayoutMode::Fullscreen {
            focused: other_pane
        }
    );
    assert_eq!(session.validate(), Ok(()));
}

/// With no acting client the zoom drop is still scoped to the split tab; a zoom
/// of a different tab stays up.
#[test]
fn commit_with_no_acting_client_leaves_a_zoom_of_another_tab_alone() {
    let (mut session, tab, source, client) = session_one_pane();
    let (other_tab, other_pane) = second_tab_zoomed_by(&mut session, client);
    session
        .clients
        .get_mut(client)
        .expect("client")
        .zoom_pane(tab, source);

    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, _events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .layout_mode(tab),
        LayoutMode::Tiled
    );
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .layout_mode(other_tab),
        LayoutMode::Fullscreen {
            focused: other_pane
        }
    );
    assert_eq!(session.validate(), Ok(()));
}

/// Splitting twice in the same tab registers both panes and leaves the focus
/// history newest first. The split source never enters the history: only the
/// pane each commit creates does.
#[test]
fn a_second_commit_puts_the_newest_pane_at_the_front_of_the_history() {
    let (mut session, tab, source, client) = session_one_pane();

    let (first_new, first_candidate) = prepared(&session, tab, source, Direction::Right);
    let (_previous, _events) = commit_new_pane(
        &mut session,
        first_new,
        tab,
        first_candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    let (second_new, second_candidate) = prepared(&session, tab, first_new, Direction::Down);
    let (_previous, _events) = commit_new_pane(
        &mut session,
        second_new,
        tab,
        second_candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(session.panes.len(), 3);
    assert_eq!(
        session.tabs.get(&tab).expect("tab").focus_mru(),
        [second_new, first_new].as_slice()
    );
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client")
            .focused_pane(tab),
        Some(second_new)
    );
    assert_eq!(session.validate(), Ok(()));
}

/// A client holding no focus in the tab gets `prior_pane: None` on the
/// [`Event::PaneFocused`] the commit emits.
#[test]
fn commit_reports_no_prior_pane_when_the_client_focused_nothing_in_the_tab() {
    let (mut session, tab, source, client) = session_one_pane();
    session
        .clients
        .get_mut(client)
        .expect("client")
        .remove_focused_pane(tab);
    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    let (_previous, events) = commit_new_pane(
        &mut session,
        new_id,
        tab,
        candidate,
        Some(client),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        events,
        vec![
            Event::PaneCreated(PaneCreated {
                pane_id: new_id,
                tab_id: tab,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
            Event::PaneFocused(PaneFocused {
                client_id: client,
                tab_id: tab,
                pane_id: new_id,
                prior_pane: None,
            }),
        ]
    );
}
