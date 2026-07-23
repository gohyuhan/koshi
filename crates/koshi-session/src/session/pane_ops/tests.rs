//! Tests for the pane state ops.
//!
//! For the NewPane commit, each test builds a one-pane session, prepares a
//! split candidate with [`split_leaf`] (as the runtime does), and applies it
//! with [`commit_new_pane`], asserting the emitted events, the post-split
//! layout tree, the registered pane, and the client's focus. Fit preflight and
//! source resolution belong to the runtime (which builds the candidate and
//! spawns before committing), so they are covered by the runtime's tests, not
//! here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::{Event, LayoutChanged, PaneCreated, PaneFocused, TabFocused};
use koshi_core::geometry::{Direction, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::process::{ShellKind, SpawnSpec};
use koshi_layout::edit::split_leaf;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneRecord;

use super::{commit_new_pane, NewPaneSpec};
use crate::client::{Client, ClientRegistry};
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
        tab_id,
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
            vec![
                LayoutChild::new(LayoutNode::Pane(source)),
                LayoutChild::new(LayoutNode::Pane(new_id)),
            ],
        ))
    );

    // The new pane is registered `Running` (its process is already live),
    // focused, and at the front of MRU.
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
        tab_a,
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

    // The client wasn't viewing tab B, so it is switched there (its old tab A is
    // reported for the caller to reflow) and focuses the new pane; TabFocused is
    // emitted first, before the pane appears.
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

/// A split nobody owns — an external caller naming a tab, with no client to act
/// for — drops every zoom of that tab, so the new pane is actually seen. Leaving
/// the zooms up would add the pane underneath them: a zoomed viewer would never
/// see it, and with no acting client nothing focuses it either.
#[test]
fn commit_with_no_acting_client_drops_every_zoom_of_the_tab() {
    let (mut session, tab, source, client) = session_one_pane();
    session
        .clients
        .get_mut(client)
        .expect("client")
        .zoom_pane(tab, source);

    let (new_id, candidate) = prepared(&session, tab, source, Direction::Right);

    // `focus: None` — the caller designated no client (an already-viewed tab
    // needs no adoption, so the runtime passes none through).
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

/// Splitting drops the zoom of the client that split — and only that client's.
/// The new pane lands in the tiled view it was sized against, and the splitter
/// sees it; a second client zoomed on a pane of the same tab is untouched,
/// because one client splitting is not a reason to change another's view.
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
        tab,
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
