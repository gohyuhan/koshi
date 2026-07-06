//! Tests for tab operations: creation, deletion, renaming, focus, and reordering.
//!
//! This module provides fixtures (helper functions) to construct test sessions,
//! tabs, clients, and panes, and exercises the tab operation functions
//! ([`commit_new_tab`], [`close_tab`], [`rename_tab`], [`focus_tab`],
//! [`move_tab`]) with various state configurations to verify correct event
//! emission and state transitions.

use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::event::Event;
use koshi_core::geometry::{Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneRecord;

use super::{close_tab, commit_new_tab, focus_tab, move_tab, rename_tab, TabTarget};
use crate::client::{Client, ClientRegistry};
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
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    )));
    tab
}

/// A client of `session_id` viewing `tab_id`, no per-tab focus recorded yet.
/// The client's session_id matches the session's own id, ensuring it passes [`Session::validate`] when attached.
fn client_on(session_id: SessionId, tab_id: TabId) -> Client {
    Client::new(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        tab_id,
    )
}

/// A `Spawning` terminal-pane record. Timestamp uses `UNIX_EPOCH` so tests stay
/// deterministic.
fn pane_record(id: PaneId) -> PaneRecord {
    PaneRecord::new(id, SystemTime::UNIX_EPOCH)
}

/// A session holding the given tabs and (registered) panes, with no clients
/// attached yet. Attach clients afterward with [`Session::attach_client`] so
/// each carries the session's own id.
fn session_with(tabs: Vec<Tab>, panes: Vec<PaneId>) -> Session {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
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
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
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

    let (prev, events) = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "second".to_owned(),
        Some(ClientId::new()),
        NewPaneSpec::default(),
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(prev, None);
    assert_eq!(events.len(), 2); // TabCreated + PaneCreated only
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        existing
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
    assert_eq!(record.title, None);
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

    assert!(session.panes.get(pb1).is_none());
    assert!(session.panes.get(pb2).is_none());
    assert!(!session.tabs.contains_key(&b));

    let closing = events
        .iter()
        .filter(|e| matches!(e, Event::PaneClosing(_)))
        .count();
    let removed = events
        .iter()
        .filter(|e| matches!(e, Event::PaneRemoved(_)))
        .count();
    assert_eq!(closing, 2);
    assert_eq!(removed, 2);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TabClosed(t) if t.tab_id == b)));
    // tab `a` survives → no quit.
    assert!(!events.iter().any(|e| matches!(e, Event::Quit)));
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
    assert!(events.iter().any(|e| matches!(
        e,
        Event::TabFocused(t)
            if t.tab_id == a && t.client_id == client_id && t.prior_tab == b
    )));
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
    assert!(!events.iter().any(|e| matches!(e, Event::TabFocused(_))));
}

#[test]
fn closing_the_last_tab_quits() {
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);

    let events = close_tab(&mut session, a);

    assert!(session.tabs.is_empty());
    assert!(events.iter().any(|e| matches!(e, Event::Quit)));
}

#[test]
fn closing_an_unknown_tab_is_a_noop() {
    let mut session = session_with(vec![], vec![]);
    let events = close_tab(&mut session, TabId::new());
    assert!(events.is_empty());
}

// --- rename_tab ------------------------------------------------------------

#[test]
fn rename_tab_changes_the_name_and_preserves_layout() {
    let a = TabId::new();
    let (l, r) = (PaneId::new(), PaneId::new());
    let mut session = session_with(vec![two_pane_tab(a, l, r, 0)], vec![l, r]);
    let layout_before = session.tabs[&a].layout().clone();

    let events = rename_tab(&mut session, a, "build".to_owned());

    assert_eq!(session.tabs[&a].name(), "build");
    assert_eq!(*session.tabs[&a].layout(), layout_before);
    assert!(
        matches!(events.as_slice(), [Event::TabRenamed(t)] if t.tab_id == a && t.name == "build")
    );
}

#[test]
fn renaming_to_the_same_name_is_a_noop() {
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![single_pane_tab(a, pa, 0)], vec![pa]);

    // single_pane_tab names it "code".
    let events = rename_tab(&mut session, a, "code".to_owned());

    assert!(events.is_empty());
}

#[test]
fn renaming_an_unknown_tab_is_a_noop() {
    let mut session = session_with(vec![], vec![]);
    let events = rename_tab(&mut session, TabId::new(), "x".to_owned());
    assert!(events.is_empty());
}

// --- focus_tab -------------------------------------------------------------

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

    let events = focus_tab(&mut session, client_id, TabTarget::Id(ids[2]));

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[2]);
    assert!(matches!(
        events.as_slice(),
        [Event::TabFocused(t)]
            if t.tab_id == ids[2] && t.client_id == client_id && t.prior_tab == ids[0]
    ));
}

#[test]
fn focus_tab_by_index_switches_to_that_position() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]);

    let events = focus_tab(&mut session, client_id, TabTarget::Index(1));

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[1]);
    assert!(matches!(
        events.as_slice(),
        [Event::TabFocused(t)]
            if t.tab_id == ids[1] && t.client_id == client_id && t.prior_tab == ids[0]
    ));
}

#[test]
fn focus_next_wraps_from_last_to_first() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[2]); // last

    let _ = focus_tab(&mut session, client_id, TabTarget::Next);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[0]);
}

#[test]
fn focus_prev_wraps_from_first_to_last() {
    let (mut session, ids) = three_tab_session();
    let client_id = attach_client_on(&mut session, ids[0]); // first

    let _ = focus_tab(&mut session, client_id, TabTarget::Prev);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), ids[2]);
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

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(ids[2]));

    // Switching tabs leaves the recorded pane focus intact.
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(ids[2]),
        Some(focused_in_two)
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
    assert!(matches!(
        events.as_slice(),
        [Event::TabMoved(m)] if m.tab_id == ids[1] && m.old_index == 1 && m.new_index == 3
    ));
}

#[test]
fn move_tab_backward_shifts_the_span_forward() {
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3

    let _ = move_tab(&mut session, ids[2], 0); // move c to the front

    assert_eq!(session.tabs[&ids[2]].index(), 0); // c
    assert_eq!(session.tabs[&ids[0]].index(), 1); // a
    assert_eq!(session.tabs[&ids[1]].index(), 2); // b
    assert_eq!(session.tabs[&ids[3]].index(), 3); // d
}

#[test]
fn move_tab_clamps_an_out_of_bounds_index() {
    let (mut session, ids) = four_tab_session();

    let events = move_tab(&mut session, ids[0], 99); // clamps to len-1 = 3

    assert_eq!(session.tabs[&ids[0]].index(), 3);
    assert!(matches!(events.as_slice(), [Event::TabMoved(m)] if m.new_index == 3));
}

#[test]
fn moving_to_the_same_index_is_a_noop() {
    let (mut session, ids) = four_tab_session();
    let events = move_tab(&mut session, ids[2], 2);
    assert!(events.is_empty());
}

#[test]
fn moving_an_unknown_tab_is_a_noop() {
    let (mut session, _ids) = four_tab_session();
    let events = move_tab(&mut session, TabId::new(), 0);
    assert!(events.is_empty());
}
