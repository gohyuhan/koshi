//! Tests for tab operations: creation, deletion, focus, and reordering.
//!
//! Holds fixtures that build test sessions, tabs, clients, and panes, and
//! exercises [`commit_new_tab`], [`commit_profile_tab`], [`close_tab`],
//! [`focus_tab`], [`resolve_tab_target`] and [`move_tab`] over the state
//! configurations each one can be called in, asserting the events emitted and
//! the state left behind.

use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::{
    Event, PaneClosing, PaneCreated, PaneFocused, PaneRemoved, TabClosed, TabCreated, TabFocused,
    TabMoved,
};
use koshi_core::geometry::{Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneRecord;

use super::{
    close_tab, commit_new_tab, commit_profile_tab, focus_tab, move_tab, resolve_tab_target,
    ProfileTab, TabTarget,
};
use crate::client::{Client, ClientOrigin, ClientRegistry};
use crate::error::SessionConsistencyError;
use crate::session::lifecycle::SessionLifecycle;
use crate::session::pane_ops::NewPaneSpec;
use crate::session::state::{Session, Tab};

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A single-pane tab named `"code"` at display position `index`.
fn single_pane_tab(tab_id: TabId, pane: PaneId, index: usize) -> Tab {
    Tab::new(tab_id, "code".to_owned(), index, pane)
}

/// A tab split left/right between `left` and `right` at display `index`.
fn two_pane_tab(tab_id: TabId, left: PaneId, right: PaneId, index: usize) -> Tab {
    let mut tab = Tab::new(tab_id, "code".to_owned(), index, left);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(left), LayoutNode::Pane(right)],
    )));
    tab
}

/// A client of `session_id` viewing `tab_id`, no per-tab focus recorded yet.
/// Its `session_id` is the one [`Session::validate`] demands of an attached
/// client.
fn client_on(session_id: SessionId, tab_id: TabId) -> Client {
    Client::new(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// A `Spawning` terminal-pane record stamped `UNIX_EPOCH`.
fn pane_record(id: PaneId) -> PaneRecord {
    PaneRecord::new(id, SystemTime::UNIX_EPOCH)
}

/// A session holding the given tabs and (registered) panes, with no clients
/// attached yet. Attach clients afterward with [`Session::attach_client`] so
/// each carries the session's own id.
fn session_with(tabs: Vec<Tab>, panes: Vec<PaneId>) -> Session {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    for tab in tabs {
        session.tabs.insert(tab.id(), tab);
    }
    for pane in panes {
        let _ = session.panes.insert(pane_record(pane));
    }
    session
}

/// Three single-pane tabs at indices 0, 1, 2.
fn three_tab_session() -> (Session, [TabId; 3]) {
    let ids = [TabId::new(), TabId::new(), TabId::new()];
    let panes = [PaneId::new(), PaneId::new(), PaneId::new()];
    let tabs = vec![
        single_pane_tab(ids[0], panes[0], 0),
        single_pane_tab(ids[1], panes[1], 1),
        single_pane_tab(ids[2], panes[2], 2),
    ];
    (session_with(tabs, panes.to_vec()), ids)
}

/// Four single-pane tabs at indices 0, 1, 2, 3.
fn four_tab_session() -> (Session, [TabId; 4]) {
    let ids = [TabId::new(), TabId::new(), TabId::new(), TabId::new()];
    let panes = [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    let tabs: Vec<Tab> = (0..4)
        .map(|i| single_pane_tab(ids[i], panes[i], i))
        .collect();
    (session_with(tabs, panes.to_vec()), ids)
}

#[test]
fn fixtures_build_a_consistent_session() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    session.attach_client(client_on(session.id, a));

    assert_eq!(session.validate(), Ok(()));
}

// --- commit_new_tab ---------------------------------------------------------

#[test]
fn commit_new_tab_registers_a_running_pane_and_emits_created_then_pane_created() {
    let mut session = session_with(vec![], vec![]);
    let (new_tab_id, new_pane_id) = (TabId::new(), PaneId::new());

    let (prev, events) = commit_new_tab(
        &mut session,
        new_tab_id,
        new_pane_id,
        "logs".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(prev, None);
    assert_eq!(session.tabs.len(), 1);
    let tab = &session.tabs[&new_tab_id];
    assert_eq!(tab.name(), "logs");
    assert_eq!(tab.index(), 0);
    assert_eq!(tab.layout().leaf_panes(), vec![new_pane_id]);

    match events.as_slice() {
        [Event::TabCreated(created), Event::PaneCreated(pane)] => {
            assert_eq!(created.tab_id, new_tab_id);
            assert_eq!(pane.tab_id, new_tab_id);
            assert_eq!(pane.pane_id, new_pane_id);
        }
        other => panic!("unexpected events: {other:?}"),
    }
    // The child was spawned before the commit, so the pane enters `Running`.
    assert_eq!(
        *session.panes.get(new_pane_id).unwrap().lifecycle(),
        PaneLifecycle::Running
    );
}

#[test]
fn commit_new_tab_first_tab_transitions_the_session_to_running() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);

    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "first".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
}

#[test]
fn commit_new_tab_after_the_first_leaves_the_session_running() {
    let mut session = session_with(vec![], vec![]);
    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "first".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);

    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "second".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
}

#[test]
fn commit_new_tab_on_a_stopping_session_leaves_it_stopping() {
    // `FirstTabCreated` is legal only from `Starting`. A session already
    // winding down keeps `Stopping`, and the tab is still created.
    let mut session = session_with(vec![], vec![]);
    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
    let (tab_id, pane_id) = (TabId::new(), PaneId::new());

    let (prev, events) = commit_new_tab(
        &mut session,
        tab_id,
        pane_id,
        "logs".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
    assert_eq!(prev, None);
    assert_eq!(
        events,
        vec![
            Event::TabCreated(TabCreated { tab_id }),
            Event::PaneCreated(PaneCreated { pane_id, tab_id }),
        ]
    );
    assert_eq!(session.tabs[&tab_id].index(), 0);
}

#[test]
fn commit_new_tab_appends_after_existing_tabs() {
    let existing = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(existing, pane, 0)], vec![pane]);
    let new_tab_id = TabId::new();

    let _ = commit_new_tab(
        &mut session,
        new_tab_id,
        PaneId::new(),
        "second".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(session.tabs.len(), 2);
    assert_eq!(session.tabs[&new_tab_id].index(), 1);
}

#[test]
fn commit_new_tab_switches_the_focus_client_and_emits_focus_events() {
    let existing = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(existing, pane, 0)], vec![pane]);
    let client = client_on(session.id, existing);
    let client_id = client.id();
    session.attach_client(client);
    let (new_tab_id, new_pane_id) = (TabId::new(), PaneId::new());

    let (prev, events) = commit_new_tab(
        &mut session,
        new_tab_id,
        new_pane_id,
        "second".to_owned(),
        Some(client_id),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(prev, Some(existing));
    let client = session.clients.get(client_id).unwrap();
    assert_eq!(client.active_tab(), new_tab_id);
    assert_eq!(client.focused_pane(new_tab_id), Some(new_pane_id));
    assert_eq!(session.tabs[&new_tab_id].focus_mru(), &[new_pane_id]);

    match events.as_slice() {
        [Event::TabCreated(created), Event::PaneCreated(pane_created), Event::TabFocused(tab_focused), Event::PaneFocused(pane_focused)] =>
        {
            assert_eq!(created.tab_id, new_tab_id);
            assert_eq!(pane_created.pane_id, new_pane_id);
            assert_eq!(pane_created.tab_id, new_tab_id);
            assert_eq!(tab_focused.client_id, client_id);
            assert_eq!(tab_focused.tab_id, new_tab_id);
            assert_eq!(tab_focused.prior_tab, existing);
            assert_eq!(pane_focused.client_id, client_id);
            assert_eq!(pane_focused.tab_id, new_tab_id);
            assert_eq!(pane_focused.pane_id, new_pane_id);
            assert_eq!(pane_focused.prior_pane, None);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn commit_new_tab_does_not_move_other_clients() {
    let existing = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(existing, pane, 0)], vec![pane]);
    let focused = client_on(session.id, existing);
    let bystander = client_on(session.id, existing);
    let focused_id = focused.id();
    let bystander_id = bystander.id();
    session.attach_client(focused);
    session.attach_client(bystander);
    let new_tab_id = TabId::new();

    let _ = commit_new_tab(
        &mut session,
        new_tab_id,
        PaneId::new(),
        "second".to_owned(),
        Some(focused_id),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        session.clients.get(focused_id).unwrap().active_tab(),
        new_tab_id
    );
    assert_eq!(
        session.clients.get(bystander_id).unwrap().active_tab(),
        existing
    );
}

#[test]
fn commit_new_tab_with_a_stale_focus_client_moves_no_view() {
    let existing = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(existing, pane, 0)], vec![pane]);
    let client = client_on(session.id, existing);
    let client_id = client.id();
    session.attach_client(client);

    let new_tab_id = TabId::new();
    let new_pane_id = PaneId::new();
    let (prev, events) = commit_new_tab(
        &mut session,
        new_tab_id,
        new_pane_id,
        "second".to_owned(),
        Some(ClientId::new()),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(prev, None);
    assert_eq!(
        events,
        vec![
            Event::TabCreated(TabCreated { tab_id: new_tab_id }),
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: new_tab_id,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        existing
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .unwrap()
            .focused_pane(new_tab_id),
        None
    );
}

#[test]
fn commit_new_tab_records_the_spec_on_the_root_pane() {
    let mut session = session_with(vec![], vec![]);
    let new_pane_id = PaneId::new();
    let spec = NewPaneSpec {
        cwd: Some(PathBuf::from("/srv")),
        command: None,
    };

    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        new_pane_id,
        "logs".to_owned(),
        None,
        spec,
        SystemTime::UNIX_EPOCH,
    );

    let record = session.panes.get(new_pane_id).unwrap();
    assert_eq!(record.cwd, Some(PathBuf::from("/srv")));
    assert_eq!(record.command, None);
}

// --- close_tab -------------------------------------------------------------

#[test]
fn close_tab_emits_a_close_remove_pair_per_pane_then_tab_closed() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb1, pb2) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), two_pane_tab(b, pb1, pb2, 1)],
        vec![pa, pb1, pb2],
    );

    let events = close_tab(&mut session, b);

    assert_eq!(session.panes.get(pb1), None);
    assert_eq!(session.panes.get(pb2), None);
    assert_eq!(session.panes.len(), 1);
    assert!(!session.tabs.contains_key(&b));

    // One close/remove pair per pane in layout order, then the tab. Tab `a`
    // survives, so no `Quit`.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb1 }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb1,
                tab_id: b,
            }),
            Event::PaneClosing(PaneClosing { pane_id: pb2 }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb2,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
        ]
    );
}

#[test]
fn close_tab_renumbers_survivors_densely() {
    let (a, b, c) = (TabId::new(), TabId::new(), TabId::new());
    let (pa, pb, pc) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            single_pane_tab(a, pa, 0),
            single_pane_tab(b, pb, 1),
            single_pane_tab(c, pc, 2),
        ],
        vec![pa, pb, pc],
    );

    let _ = close_tab(&mut session, b); // remove the middle tab

    assert_eq!(session.tabs[&a].index(), 0);
    assert_eq!(session.tabs[&c].index(), 1); // was 2, densified to 1
}

#[test]
fn close_tab_moves_a_viewing_client_to_the_nearest_tab() {
    let (a, b, c) = (TabId::new(), TabId::new(), TabId::new());
    let (pa, pb, pc) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            single_pane_tab(a, pa, 0),
            single_pane_tab(b, pb, 1),
            single_pane_tab(c, pc, 2),
        ],
        vec![pa, pb, pc],
    );
    let client = client_on(session.id, b); // viewing the middle tab
    let client_id = client.id();
    session.attach_client(client);

    let events = close_tab(&mut session, b);

    // nearest to index 1 is the previous tab (a, index 0).
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: a,
                prior_tab: b,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: a,
                pane_id: pa,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(a),
        Some(pa)
    );
}

#[test]
fn close_tab_at_the_first_index_moves_viewers_to_the_next_tab() {
    // Closing index 0 has no previous tab to fall back on, so
    // `nearest_surviving_tab` takes the smallest index above it.
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);
    let closed_pane = only_pane(&session, ids[0]);
    let landed = only_pane(&session, ids[1]);

    let events = close_tab(&mut session, ids[0]);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[1]);
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane,
                tab_id: ids[0],
            }),
            Event::TabClosed(TabClosed { tab_id: ids[0] }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[1],
                prior_tab: ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[1],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[1]),
        Some(landed)
    );
    // The survivors close ranks behind the gone first tab.
    assert_eq!(session.tabs[&ids[1]].index(), 0);
    assert_eq!(session.tabs[&ids[2]].index(), 1);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn close_tab_leaves_a_non_viewing_clients_active_tab() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    let client = client_on(session.id, a); // viewing a, not the closed b
    let client_id = client.id();
    session.attach_client(client);

    let events = close_tab(&mut session, b);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    // No client was viewing `b`, so nothing is refocused.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
        ]
    );
}

#[test]
fn close_tab_drops_the_focus_and_zoom_a_non_viewing_client_held_in_it() {
    // A client sitting on another tab still carries per-tab state for the tab
    // that closes. Both entries go, so nothing points at the gone pane.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    let mut client = client_on(session.id, a);
    client.update_focused_pane(b, pb);
    client.zoom_pane(b, pb);
    let client_id = client.id();
    session.attach_client(client);
    assert_eq!(session.validate(), Ok(()));

    let _ = close_tab(&mut session, b);

    let client = session.clients.get(client_id).unwrap();
    assert_eq!(client.active_tab(), a);
    assert_eq!(client.focused_pane(b), None);
    assert_eq!(client.zoomed_pane(b), None);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn close_tab_emits_pane_events_for_a_leaf_missing_from_the_registry() {
    // A layout leaf with no registry record (a desync) still gets its
    // close/remove pair, so the runtime tears down whatever it holds for it.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb1, pb2) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), two_pane_tab(b, pb1, pb2, 1)],
        vec![pa, pb1], // pb2 is a leaf of `b` but was never registered
    );

    let events = close_tab(&mut session, b);

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb1 }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb1,
                tab_id: b,
            }),
            Event::PaneClosing(PaneClosing { pane_id: pb2 }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb2,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
        ]
    );
    assert_eq!(session.panes.get(pb1), None);
    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn closing_the_last_tab_quits() {
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);

    let events = close_tab(&mut session, a);

    assert!(session.tabs.is_empty());
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pa }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pa,
                tab_id: a,
            }),
            Event::TabClosed(TabClosed { tab_id: a }),
            Event::Quit,
        ]
    );
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn closing_an_unknown_tab_is_a_noop() {
    let mut session = session_with(vec![], vec![]);
    let events = close_tab(&mut session, TabId::new());
    assert!(events.is_empty());
}

// --- focus_tab -------------------------------------------------------------

/// The single pane of a single-pane tab.
fn only_pane(session: &Session, tab: TabId) -> PaneId {
    session.tabs[&tab].layout().leaf_panes()[0]
}

/// Attach a fresh client viewing `tab` and return its id.
fn attach_client_on(session: &mut Session, tab: TabId) -> ClientId {
    let client = client_on(session.id, tab);
    let client_id = client.id();
    session.attach_client(client);
    client_id
}

#[test]
fn focus_tab_by_id_switches_active_and_emits() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let landed = only_pane(&session, ids[2]);
    let events = focus_tab(&mut session, client_id, TabTarget::Id(ids[2]));

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[2]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[2],
                prior_tab: ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[2],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[2]),
        Some(landed)
    );
}

#[test]
fn focus_tab_by_index_switches_to_that_position() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let landed = only_pane(&session, ids[1]);
    let events = focus_tab(&mut session, client_id, TabTarget::Index(1));

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[1]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[1],
                prior_tab: ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[1],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[1]),
        Some(landed)
    );
}

#[test]
fn focus_next_steps_to_the_following_tab() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let landed = only_pane(&session, ids[1]);
    let events = focus_tab(&mut session, client_id, TabTarget::Next);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[1]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[1],
                prior_tab: ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[1],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[1]),
        Some(landed)
    );
}

#[test]
fn focus_prev_steps_to_the_preceding_tab() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[2]);

    let landed = only_pane(&session, ids[1]);
    let events = focus_tab(&mut session, client_id, TabTarget::Prev);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[1]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[1],
                prior_tab: ids[2],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[1],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[1]),
        Some(landed)
    );
}

#[test]
fn focus_next_wraps_from_last_to_first() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[2]); // last

    let landed = only_pane(&session, ids[0]);
    let events = focus_tab(&mut session, client_id, TabTarget::Next);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[0]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[0],
                prior_tab: ids[2],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[0],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[0]),
        Some(landed)
    );
}

#[test]
fn focus_prev_wraps_from_first_to_last() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]); // first

    let landed = only_pane(&session, ids[2]);
    let events = focus_tab(&mut session, client_id, TabTarget::Prev);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[2]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[2],
                prior_tab: ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[2],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[2]),
        Some(landed)
    );
}

#[test]
fn focusing_the_already_active_tab_is_a_noop() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[1]);

    let events = focus_tab(&mut session, client_id, TabTarget::Id(ids[1]));

    assert!(events.is_empty());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[1]);
}

#[test]
fn focusing_an_out_of_range_index_is_a_noop() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let events = focus_tab(&mut session, client_id, TabTarget::Index(9));

    assert!(events.is_empty());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[0]);
}

#[test]
fn focusing_the_index_one_past_the_last_tab_is_a_noop() {
    // Three tabs fill 0..=2; index 3 is the first slot that does not exist.
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let events = focus_tab(&mut session, client_id, TabTarget::Index(3));

    assert!(events.is_empty());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[0]);
}

#[test]
fn focusing_the_last_index_switches_to_the_last_tab() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let landed = only_pane(&session, ids[2]);
    let events = focus_tab(&mut session, client_id, TabTarget::Index(2));

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[2]);
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: ids[2],
                prior_tab: ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: ids[2],
                pane_id: landed,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[2]),
        Some(landed)
    );
}

#[test]
fn focus_tab_with_no_tabs_left_is_a_noop_not_a_panic() {
    // Closing the last tab leaves the client's `active_tab` naming a tab that
    // is gone and `session.tabs` empty. Next/Prev must not divide by the zero
    // tab count, and Index/Id must find nothing.
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);
    let client_id = attach_client_on(&mut session, a);
    let _ = close_tab(&mut session, a);
    assert!(session.tabs.is_empty());

    assert!(focus_tab(&mut session, client_id, TabTarget::Next).is_empty());
    assert!(focus_tab(&mut session, client_id, TabTarget::Prev).is_empty());
    assert!(focus_tab(&mut session, client_id, TabTarget::Index(0)).is_empty());
    assert!(focus_tab(&mut session, client_id, TabTarget::Id(a)).is_empty());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
}

#[test]
fn focusing_an_unknown_id_is_a_noop() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let events = focus_tab(&mut session, client_id, TabTarget::Id(TabId::new()));

    assert!(events.is_empty());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[0]);
}

#[test]
fn focus_tab_for_an_unattached_client_is_a_noop() {
    let (mut session, ids) = three_tab_session();
    let attached = attach_client_on(&mut session, ids[0]);

    let events = focus_tab(&mut session, ClientId::new(), TabTarget::Id(ids[2]));

    assert!(events.is_empty());
    assert_eq!(session.clients.get(attached).unwrap().active_tab(), ids[0]);
}

#[test]
fn focus_tab_preserves_per_tab_pane_focus() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);
    let focused_in_two = PaneId::new();
    session
        .clients
        .get_mut(client_id)
        .unwrap()
        .update_focused_pane(ids[2], focused_in_two);

    let events = focus_tab(&mut session, client_id, TabTarget::Id(ids[2]));

    // Switching tabs leaves the recorded pane focus intact, and lands on no
    // other pane, so only the tab switch is reported.
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[2]),
        Some(focused_in_two)
    );
    assert_eq!(
        events,
        vec![Event::TabFocused(TabFocused {
            client_id,
            tab_id: ids[2],
            prior_tab: ids[0],
        })]
    );
}

#[test]
fn focus_tab_lands_on_the_tabs_most_recent_pane() {
    // A client that never focused a pane in the tab it switches to lands on
    // that tab's focus history head, not on the first leaf in layout order.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, left, right) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), two_pane_tab(b, left, right, 1)],
        vec![pa, left, right],
    );
    session.tabs.get_mut(&b).unwrap().record_focus_mru(right);
    let client_id = attach_client_on(&mut session, a);

    let events = focus_tab(&mut session, client_id, TabTarget::Id(b));

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(b),
        Some(right)
    );
    assert_eq!(
        events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: b,
                prior_tab: a,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: b,
                pane_id: right,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn focus_tab_lands_on_the_first_leaf_without_a_focus_history() {
    // With no focus history to read, the landing pane is the first leaf in
    // layout order.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, left, right) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), two_pane_tab(b, left, right, 1)],
        vec![pa, left, right],
    );
    let client_id = attach_client_on(&mut session, a);
    assert!(session.tabs[&b].focus_mru().is_empty());

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(b));

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(b),
        Some(left)
    );
    // The landing is recorded as the tab's most recent focus.
    assert_eq!(session.tabs[&b].focus_mru(), &[left]);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn focus_tab_skips_a_landing_candidate_with_no_registry_record() {
    // `left` is a layout leaf the registry does not hold, so the landing walk
    // passes over it and takes `right`.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, left, right) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), two_pane_tab(b, left, right, 1)],
        vec![pa, right],
    );
    let client_id = attach_client_on(&mut session, a);

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(b));

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(b),
        Some(right)
    );
}

#[test]
fn focus_tab_skips_a_history_entry_that_is_not_a_leaf() {
    // A focus history entry naming a pane the tab's layout no longer holds is
    // passed over, so the landing never focuses a pane outside the tab.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb, stale) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    let _ = session.panes.insert(pane_record(stale));
    session.tabs.get_mut(&b).unwrap().record_focus_mru(stale);
    let client_id = attach_client_on(&mut session, a);

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(b));

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(b),
        Some(pb)
    );
}

#[test]
fn focus_tab_lands_on_nothing_when_no_leaf_has_a_record() {
    // Every leaf of the target tab is missing from the registry: the switch
    // still happens, and it reports the tab switch alone.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa],
    );
    let client_id = attach_client_on(&mut session, a);

    let events = focus_tab(&mut session, client_id, TabTarget::Id(b));

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(b),
        None
    );
    assert_eq!(
        events,
        vec![Event::TabFocused(TabFocused {
            client_id,
            tab_id: b,
            prior_tab: a,
        })]
    );
}

#[test]
fn close_tab_lands_a_moved_client_on_the_surviving_tabs_recent_pane() {
    // Closing the tab a client views moves it to the nearest survivor and
    // gives it that tab's most recent pane, so the arriving client always has
    // a pane to type into.
    let (a, b) = (TabId::new(), TabId::new());
    let (left, right, pb) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(a, left, right, 0), single_pane_tab(b, pb, 1)],
        vec![left, right, pb],
    );
    session.tabs.get_mut(&a).unwrap().record_focus_mru(right);
    let client_id = attach_client_on(&mut session, b);

    let events = close_tab(&mut session, b);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(a),
        Some(right)
    );
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: a,
                prior_tab: b,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: a,
                pane_id: right,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(session.validate(), Ok(()));
}

// --- resolve_tab_target -----------------------------------------------------

#[test]
fn resolve_tab_target_reads_each_variant_against_the_display_order() {
    let (session, ids) = three_tab_session(); // indices 0, 1, 2

    assert_eq!(
        resolve_tab_target(&session, ids[0], TabTarget::Id(ids[2])),
        Some(ids[2])
    );
    assert_eq!(
        resolve_tab_target(&session, ids[0], TabTarget::Id(TabId::new())),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, ids[0], TabTarget::Index(1)),
        Some(ids[1])
    );
    assert_eq!(
        resolve_tab_target(&session, ids[0], TabTarget::Index(3)),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, ids[0], TabTarget::Next),
        Some(ids[1])
    );
    assert_eq!(
        resolve_tab_target(&session, ids[2], TabTarget::Next),
        Some(ids[0])
    );
    assert_eq!(
        resolve_tab_target(&session, ids[2], TabTarget::Prev),
        Some(ids[1])
    );
    assert_eq!(
        resolve_tab_target(&session, ids[0], TabTarget::Prev),
        Some(ids[2])
    );
}

#[test]
fn resolve_tab_target_steps_nowhere_from_an_active_tab_the_session_lost() {
    // `Next` and `Prev` step from `active_tab`; an id the session does not
    // hold has no position to step from. `Id` and `Index` never read
    // `active_tab`, so they still resolve.
    let (session, ids) = three_tab_session();
    let gone = TabId::new();

    assert_eq!(resolve_tab_target(&session, gone, TabTarget::Next), None);
    assert_eq!(resolve_tab_target(&session, gone, TabTarget::Prev), None);
    assert_eq!(
        resolve_tab_target(&session, gone, TabTarget::Index(0)),
        Some(ids[0])
    );
    assert_eq!(
        resolve_tab_target(&session, gone, TabTarget::Id(ids[1])),
        Some(ids[1])
    );
}

#[test]
fn resolve_tab_target_in_a_session_with_no_tabs_resolves_to_none() {
    // Zero tabs: every variant resolves to `None`, and `Next`/`Prev` stop
    // before taking the step modulo the tab count.
    let session = session_with(vec![], vec![]);
    let gone = TabId::new();

    assert_eq!(resolve_tab_target(&session, gone, TabTarget::Next), None);
    assert_eq!(resolve_tab_target(&session, gone, TabTarget::Prev), None);
    assert_eq!(
        resolve_tab_target(&session, gone, TabTarget::Index(0)),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, gone, TabTarget::Id(gone)),
        None
    );
}

// --- move_tab --------------------------------------------------------------

#[test]
fn move_tab_forward_shifts_the_span_back() {
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3

    let events = move_tab(&mut session, ids[1], 3); // move b to the end

    assert_eq!(session.tabs[&ids[0]].index(), 0); // a
    assert_eq!(session.tabs[&ids[2]].index(), 1); // c
    assert_eq!(session.tabs[&ids[3]].index(), 2); // d
    assert_eq!(session.tabs[&ids[1]].index(), 3); // b
    assert_eq!(
        events,
        vec![Event::TabMoved(TabMoved {
            tab_id: ids[1],
            old_index: 1,
            new_index: 3,
        })]
    );
}

#[test]
fn move_tab_backward_shifts_the_span_forward() {
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3

    let events = move_tab(&mut session, ids[2], 0); // move c to the front

    assert_eq!(session.tabs[&ids[2]].index(), 0); // c
    assert_eq!(session.tabs[&ids[0]].index(), 1); // a
    assert_eq!(session.tabs[&ids[1]].index(), 2); // b
    assert_eq!(session.tabs[&ids[3]].index(), 3); // d
    assert_eq!(
        events,
        vec![Event::TabMoved(TabMoved {
            tab_id: ids[2],
            old_index: 2,
            new_index: 0,
        })]
    );
}

#[test]
fn move_tab_clamps_an_out_of_bounds_index() {
    let (mut session, ids) = four_tab_session();

    let events = move_tab(&mut session, ids[0], usize::MAX); // clamps to len-1 = 3

    assert_eq!(session.tabs[&ids[1]].index(), 0);
    assert_eq!(session.tabs[&ids[2]].index(), 1);
    assert_eq!(session.tabs[&ids[3]].index(), 2);
    assert_eq!(session.tabs[&ids[0]].index(), 3);
    assert_eq!(
        events,
        vec![Event::TabMoved(TabMoved {
            tab_id: ids[0],
            old_index: 0,
            new_index: 3,
        })]
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn moving_to_the_same_index_is_a_noop() {
    let (mut session, ids) = four_tab_session();

    let events = move_tab(&mut session, ids[2], 2);

    assert!(events.is_empty());
    for (position, id) in ids.iter().enumerate() {
        assert_eq!(session.tabs[id].index(), position);
    }
}

#[test]
fn moving_an_unknown_tab_is_a_noop() {
    let (mut session, ids) = four_tab_session();

    let events = move_tab(&mut session, TabId::new(), 0);

    assert!(events.is_empty());
    assert_eq!(session.tabs.len(), 4);
    for (position, id) in ids.iter().enumerate() {
        assert_eq!(session.tabs[id].index(), position);
    }
}

#[test]
fn move_tab_in_a_single_tab_session_is_a_noop() {
    // The clamp reads `len - 1`; with one tab that is 0, and every requested
    // index lands back on the slot the tab already holds.
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);

    assert!(move_tab(&mut session, a, 0).is_empty());
    assert!(move_tab(&mut session, a, 7).is_empty());
    assert_eq!(session.tabs[&a].index(), 0);
}

#[test]
fn move_tab_with_only_two_tabs_swaps_them() {
    // Two is the smallest span move_tab does real work on — the "others"
    // list it builds holds exactly one entry, a boundary the 3- and 4-tab
    // fixtures never exercise.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );

    let events = move_tab(&mut session, a, 1);

    assert_eq!(session.tabs[&b].index(), 0);
    assert_eq!(session.tabs[&a].index(), 1);
    assert_eq!(
        events,
        vec![Event::TabMoved(TabMoved {
            tab_id: a,
            old_index: 0,
            new_index: 1,
        })]
    );
}

#[test]
fn move_tab_keeps_the_session_consistent_and_indices_dense() {
    // Reordering must leave the registry contract intact: after a move the
    // indices are still a dense 0..len with no duplicate, and an attached
    // client viewing a moved tab still resolves — `validate` finds nothing.
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3
    let client_id = attach_client_on(&mut session, ids[3]); // viewing the tab that moves

    let _ = move_tab(&mut session, ids[3], 0); // d to the front

    assert_eq!(session.tabs[&ids[3]].index(), 0);
    assert_eq!(session.tabs[&ids[0]].index(), 1);
    assert_eq!(session.tabs[&ids[1]].index(), 2);
    assert_eq!(session.tabs[&ids[2]].index(), 3);
    // The client's active tab is unchanged by a reorder — a move shifts
    // positions, not which tab a client views.
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[3]);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn move_tab_then_close_tab_keeps_indices_dense() {
    // Two reorders back to back: the close renumbers the survivors from the
    // order the move left, not from the order the fixture built.
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3

    let _ = move_tab(&mut session, ids[3], 0); // d0 a1 b2 c3
    let _ = close_tab(&mut session, ids[0]); // a leaves → d0 b1 c2

    assert_eq!(session.tabs.len(), 3);
    assert_eq!(session.tabs[&ids[3]].index(), 0);
    assert_eq!(session.tabs[&ids[1]].index(), 1);
    assert_eq!(session.tabs[&ids[2]].index(), 2);
    assert_eq!(session.validate(), Ok(()));
}

// --- close_and_refocus_tab / focus_tab edge cases ---------------------------

#[test]
fn closing_an_already_closed_tab_is_a_noop_on_the_second_call() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );

    let first = close_tab(&mut session, b);
    assert_eq!(
        first,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
        ]
    );
    assert!(!session.tabs.contains_key(&b));

    // Closing the same, now-unknown, id again must not disturb the survivor.
    let second = close_tab(&mut session, b);

    assert!(second.is_empty());
    assert!(session.tabs.contains_key(&a));
    assert_eq!(session.tabs[&a].index(), 0);
}

#[test]
fn close_tab_moves_every_client_that_was_viewing_it() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    let first = attach_client_on(&mut session, b);
    let second = attach_client_on(&mut session, b);

    let events = close_tab(&mut session, b);

    assert_eq!(session.clients.get(first).unwrap().active_tab(), a);
    assert_eq!(session.clients.get(second).unwrap().active_tab(), a);
    // Clients are walked in id order, so the two refocus pairs follow it.
    let (lower, higher) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pb }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pb,
                tab_id: b,
            }),
            Event::TabClosed(TabClosed { tab_id: b }),
            Event::TabFocused(TabFocused {
                client_id: lower,
                tab_id: a,
                prior_tab: b,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: lower,
                tab_id: a,
                pane_id: pa,
                prior_pane: None,
            }),
            Event::TabFocused(TabFocused {
                client_id: higher,
                tab_id: a,
                prior_tab: b,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: higher,
                tab_id: a,
                pane_id: pa,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(
        session.clients.get(first).unwrap().focused_pane(a),
        Some(pa)
    );
    assert_eq!(
        session.clients.get(second).unwrap().focused_pane(a),
        Some(pa)
    );
}

#[test]
fn close_tab_preserves_a_clients_prior_focus_on_the_tab_it_lands_on() {
    // The client already held a per-tab focus on `a` before `b` closed and
    // pushed it there; that pre-existing focus must survive, not be reset.
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    let mut client = client_on(session.id, b);
    client.update_focused_pane(a, pa);
    let client_id = client.id();
    session.attach_client(client);

    let _ = close_tab(&mut session, b);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(a),
        Some(pa)
    );
}

#[test]
fn closing_the_last_tab_leaves_a_viewing_clients_active_tab_pointing_at_it() {
    // With no surviving tab to send the client to, `close_and_refocus_tab`
    // leaves `active_tab` unchanged: it still names the tab id that was just
    // removed from `session.tabs`. The per-tab focus entry for that tab is
    // still pruned. The session is quitting, so this dangling-final reference
    // is legal: `validate()` scopes its active-tab check to sessions that
    // still have tabs.
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);
    let client_id = attach_client_on(&mut session, a);

    let events = close_tab(&mut session, a);

    assert!(session.tabs.is_empty());
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pa }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pa,
                tab_id: a,
            }),
            Event::TabClosed(TabClosed { tab_id: a }),
            Event::Quit,
        ]
    );
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(a),
        None
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn a_dangling_active_tab_is_still_reported_while_other_tabs_remain() {
    // The zero-tab scoping must not swallow the real corruption case: a
    // client viewing a gone tab while the session still has tabs is an
    // inconsistency and stays reported.
    let a = TabId::new();
    let b = TabId::new();
    let pa = PaneId::new();
    let pb = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );
    let client_id = attach_client_on(&mut session, a);
    let gone = TabId::new();
    session
        .clients
        .get_mut(client_id)
        .expect("attached")
        .update_active_tab(gone);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::ActiveTabMissing {
            client: client_id,
            tab: gone,
        }])
    );
}

#[test]
fn focus_tab_next_with_a_stale_active_tab_is_a_noop_not_a_panic() {
    // A client whose `active_tab` no longer exists in `session.tabs` (e.g.
    // an external mutation, or a state built outside the normal ops) must
    // not panic when stepping Next/Prev — `resolve_tab_target` looks up the
    // stale tab's index and finds nothing.
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);
    session
        .clients
        .get_mut(client_id)
        .unwrap()
        .update_active_tab(TabId::new()); // now points nowhere

    let next = focus_tab(&mut session, client_id, TabTarget::Next);
    let prev = focus_tab(&mut session, client_id, TabTarget::Prev);

    assert!(next.is_empty());
    assert!(prev.is_empty());
}

#[test]
fn focus_next_and_prev_on_a_single_tab_session_is_a_noop() {
    // With exactly one tab, wrapping Next/Prev resolves back to the same
    // tab — the already-active-tab guard in `focus_tab` makes this a no-op.
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);
    let client_id = attach_client_on(&mut session, a);

    let next = focus_tab(&mut session, client_id, TabTarget::Next);
    let prev = focus_tab(&mut session, client_id, TabTarget::Prev);

    assert!(next.is_empty());
    assert!(prev.is_empty());
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
}

// --- commit_profile_tab -----------------------------------------------------

/// A two-leaf horizontal split of `left` and `right`, as a profile's tree.
fn two_leaf_layout(left: PaneId, right: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(left), LayoutNode::Pane(right)],
    ))
}

#[test]
fn commit_profile_tab_registers_every_pane_running_and_emits_created_events() {
    let mut session = session_with(vec![], vec![]);
    let tab_id = TabId::new();
    let (p0, p1) = (PaneId::new(), PaneId::new());
    let layout = two_leaf_layout(p0, p1);
    let profile = ProfileTab {
        pane_ids: vec![p0, p1],
        layout: layout.clone(),
        specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focus_leaf: 0,
    };

    let events = commit_profile_tab(
        &mut session,
        tab_id,
        profile,
        "dev".to_owned(),
        None,
        true,
        SystemTime::UNIX_EPOCH,
    );

    // The first tab moves the session from Starting to Running.
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
    // Every pane in the profile is registered and live (each child was already
    // spawned before the commit).
    assert_eq!(
        *session.panes.get(p0).unwrap().lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(
        *session.panes.get(p1).unwrap().lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(session.panes.len(), 2);
    // The tab carries the whole profile tree, not just its single root leaf.
    assert_eq!(*session.tabs[&tab_id].layout(), layout);
    assert_eq!(session.tabs[&tab_id].index(), 0);

    // No focus client, so only creation events: one TabCreated then one
    // PaneCreated per pane, in layout order.
    match events.as_slice() {
        [Event::TabCreated(t), Event::PaneCreated(a), Event::PaneCreated(b)] => {
            assert_eq!(t.tab_id, tab_id);
            assert_eq!(a.pane_id, p0);
            assert_eq!(a.tab_id, tab_id);
            assert_eq!(b.pane_id, p1);
            assert_eq!(b.tab_id, tab_id);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn commit_profile_tab_without_a_client_still_records_the_focus_leaf() {
    // A profile committed with no client — a session started detached — must
    // still put its chosen leaf in the tab's focus history, so the first
    // client to view the tab lands on it, not on layout order.
    let mut session = session_with(vec![], vec![]);
    let tab_id = TabId::new();
    let (p0, p1) = (PaneId::new(), PaneId::new());
    let profile = ProfileTab {
        pane_ids: vec![p0, p1],
        layout: two_leaf_layout(p0, p1),
        specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focus_leaf: 1,
    };

    let _ = commit_profile_tab(
        &mut session,
        tab_id,
        profile,
        "dev".to_owned(),
        None,
        true,
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(session.tabs[&tab_id].focus_mru(), &[p1]);
}

#[test]
fn commit_profile_tab_focuses_the_focus_leaf_and_switches_the_client() {
    let mut session = session_with(vec![], vec![]);
    let start_tab = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        start_tab,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    let client_id = attach_client_on(&mut session, start_tab);

    let tab_id = TabId::new();
    let (p0, p1) = (PaneId::new(), PaneId::new());
    let profile = ProfileTab {
        pane_ids: vec![p0, p1],
        layout: two_leaf_layout(p0, p1),
        specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focus_leaf: 1, // focus the second leaf, not the root
    };

    let events = commit_profile_tab(
        &mut session,
        tab_id,
        profile,
        "dev".to_owned(),
        Some(client_id),
        true,
        SystemTime::UNIX_EPOCH,
    );

    let client = session.clients.get(client_id).unwrap();
    // Active profile tab: the client switches onto it and focuses the chosen leaf.
    assert_eq!(client.active_tab(), tab_id);
    assert_eq!(client.focused_pane(tab_id), Some(p1));
    assert_eq!(session.tabs[&tab_id].focus_mru(), &[p1]);

    // TabCreated, one PaneCreated per pane, then the focus pair naming the leaf.
    assert_eq!(
        events,
        vec![
            Event::TabCreated(TabCreated { tab_id }),
            Event::PaneCreated(PaneCreated {
                pane_id: p0,
                tab_id,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: p1,
                tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id,
                prior_tab: start_tab,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: p1,
                prior_pane: None,
            }),
        ]
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn commit_profile_tab_out_of_range_focus_leaf_focuses_the_root_pane() {
    // `focus_leaf` past the last leaf falls back to the root pane (index 0),
    // never panics and never focuses a pane the profile does not hold.
    let mut session = session_with(vec![], vec![]);
    let start_tab = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        start_tab,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    let client_id = attach_client_on(&mut session, start_tab);

    let tab_id = TabId::new();
    let (p0, p1) = (PaneId::new(), PaneId::new());
    let profile = ProfileTab {
        pane_ids: vec![p0, p1],
        layout: two_leaf_layout(p0, p1),
        specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focus_leaf: 9, // out of range
    };

    let _ = commit_profile_tab(
        &mut session,
        tab_id,
        profile,
        "dev".to_owned(),
        Some(client_id),
        true,
        SystemTime::UNIX_EPOCH,
    );

    let client = session.clients.get(client_id).unwrap();
    assert_eq!(client.focused_pane(tab_id), Some(p0));
    assert_eq!(session.tabs[&tab_id].focus_mru(), &[p0]);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn commit_profile_tab_inactive_records_focus_without_switching_the_view() {
    // An inactive profile tab records the client's starting pane, leaves the
    // client's view where it was, and emits no focus events.
    let mut session = session_with(vec![], vec![]);
    let tab0 = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        tab0,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    let client_id = attach_client_on(&mut session, tab0);

    let tab1 = TabId::new();
    let (p0, p1) = (PaneId::new(), PaneId::new());
    let profile = ProfileTab {
        pane_ids: vec![p0, p1],
        layout: two_leaf_layout(p0, p1),
        specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focus_leaf: 0,
    };

    let events = commit_profile_tab(
        &mut session,
        tab1,
        profile,
        "dev".to_owned(),
        Some(client_id),
        false, // inactive
        SystemTime::UNIX_EPOCH,
    );

    let client = session.clients.get(client_id).unwrap();
    // The view stays on the original tab: an inactive tab does not switch it.
    assert_eq!(client.active_tab(), tab0);
    // But the starting pane is recorded on both the client and the tab history.
    assert_eq!(client.focused_pane(tab1), Some(p0));
    assert_eq!(session.tabs[&tab1].focus_mru(), &[p0]);
    // No TabFocused/PaneFocused while inactive — only the creation events.
    assert_eq!(
        events,
        vec![
            Event::TabCreated(TabCreated { tab_id: tab1 }),
            Event::PaneCreated(PaneCreated {
                pane_id: p0,
                tab_id: tab1,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: p1,
                tab_id: tab1,
            }),
        ]
    );
    assert_eq!(session.tabs[&tab1].index(), 1); // appended after tab0
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn commit_profile_tab_with_a_stale_focus_client_emits_only_creation_events() {
    // An id no client holds records no focus and moves no view, exactly like
    // `None` — but the tab still records its own starting pane.
    let mut session = session_with(vec![], vec![]);
    let tab0 = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        tab0,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );
    let attached = attach_client_on(&mut session, tab0);

    let tab1 = TabId::new();
    let (p0, p1) = (PaneId::new(), PaneId::new());
    let profile = ProfileTab {
        pane_ids: vec![p0, p1],
        layout: two_leaf_layout(p0, p1),
        specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focus_leaf: 1,
    };

    let events = commit_profile_tab(
        &mut session,
        tab1,
        profile,
        "dev".to_owned(),
        Some(ClientId::new()),
        true,
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(
        events,
        vec![
            Event::TabCreated(TabCreated { tab_id: tab1 }),
            Event::PaneCreated(PaneCreated {
                pane_id: p0,
                tab_id: tab1,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: p1,
                tab_id: tab1,
            }),
        ]
    );
    let client = session.clients.get(attached).unwrap();
    assert_eq!(client.active_tab(), tab0);
    assert_eq!(client.focused_pane(tab1), None);
    assert_eq!(session.tabs[&tab1].focus_mru(), &[p1]);
    assert_eq!(session.validate(), Ok(()));
}

#[test]
#[should_panic(expected = "index out of bounds")]
fn commit_profile_tab_with_no_panes_panics() {
    // The tab's root pane is `pane_ids[0]`, so an empty `pane_ids` panics
    // there — the panic the function documents.
    let mut session = session_with(vec![], vec![]);
    let profile = ProfileTab {
        pane_ids: vec![],
        layout: LayoutNode::Pane(PaneId::new()),
        specs: vec![],
        focus_leaf: 0,
    };

    let _ = commit_profile_tab(
        &mut session,
        TabId::new(),
        profile,
        "dev".to_owned(),
        None,
        true,
        SystemTime::UNIX_EPOCH,
    );
}

#[test]
fn a_new_tab_after_a_close_takes_the_freed_index_densely() {
    // Closing the middle of three tabs renumbers the survivors to 0,1; a tab
    // created next lands at the freed dense slot (2) with no duplicate index,
    // and the session stays consistent.
    let (mut session, ids) = three_tab_session(); // a0 b1 c2
    let _ = close_tab(&mut session, ids[1]); // remove the middle → a0 c1
    assert_eq!(session.tabs[&ids[0]].index(), 0);
    assert_eq!(session.tabs[&ids[2]].index(), 1);

    let fresh = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        fresh,
        PaneId::new(),
        "d".to_owned(),
        None,
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(session.tabs[&fresh].index(), 2);
    assert_eq!(session.validate(), Ok(()));
}
