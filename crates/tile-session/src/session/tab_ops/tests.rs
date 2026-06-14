use std::time::SystemTime;

use tile_core::event::Event;
use tile_core::geometry::{Size, SplitDirection};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use tile_pane::pane::lifecycle::PaneLifecycle;
use tile_pane::pane::state::PaneRecord;

use super::{close_tab, focus_tab, move_tab, new_tab, rename_tab, TabTarget};
use crate::client::{Client, ClientRegistry};
use crate::session::state::{Session, Tab};

const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A single-pane tab named `"code"` at display position `index`.
fn single_pane_tab(tab_id: TabId, pane: PaneId, index: usize) -> Tab {
    Tab::new(tab_id, "code".to_owned(), index, pane)
}

/// A tab split left/right between `left` and `right` at display `index`.
fn two_pane_tab(tab_id: TabId, left: PaneId, right: PaneId, index: usize) -> Tab {
    let mut tab = Tab::new(tab_id, "code".to_owned(), index, left);
    tab.layout = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    ));
    tab
}

/// A client viewing `tab_id`, no per-tab focus recorded yet.
fn client_on(tab_id: TabId) -> Client {
    Client::new(
        ClientId::new(),
        SessionId::new(),
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

/// A session holding the given clients, tabs, and (registered) panes.
fn session_with(clients: Vec<Client>, tabs: Vec<Tab>, panes: Vec<PaneId>) -> Session {
    let mut registry = ClientRegistry::new();
    for client in clients {
        registry.attach(client);
    }
    let mut session = Session::new(SessionId::new(), "main".to_owned(), registry);
    for tab in tabs {
        session.tabs.insert(tab.id, tab);
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
    (session_with(vec![], tabs, panes.to_vec()), ids)
}

/// Four single-pane tabs at indices 0, 1, 2, 3.
fn four_tab_session() -> (Session, [TabId; 4]) {
    let ids = [TabId::new(), TabId::new(), TabId::new(), TabId::new()];
    let panes = [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    let tabs: Vec<Tab> = (0..4)
        .map(|i| single_pane_tab(ids[i], panes[i], i))
        .collect();
    (session_with(vec![], tabs, panes.to_vec()), ids)
}

// --- new_tab ---------------------------------------------------------------

#[test]
fn new_tab_registers_a_spawning_pane_and_emits_created_then_pane_created() {
    let mut session = session_with(vec![], vec![], vec![]);

    let events = new_tab(&mut session, "logs".to_owned(), SystemTime::UNIX_EPOCH);

    assert_eq!(session.tabs.len(), 1);
    let tab = session.tabs.values().next().unwrap();
    assert_eq!(tab.name, "logs");
    assert_eq!(tab.index, 0);

    // TabCreated then PaneCreated, both naming the same new tab.
    let (tab_id, pane_id) = match events.as_slice() {
        [Event::TabCreated(created), Event::PaneCreated(pane)] => {
            assert_eq!(created.tab_id, pane.tab_id);
            (created.tab_id, pane.pane_id)
        }
        other => panic!("unexpected events: {other:?}"),
    };
    assert_eq!(tab.id, tab_id);
    assert_eq!(tab.layout.leaf_panes(), vec![pane_id]);
    assert_eq!(
        *session.panes.get(pane_id).unwrap().lifecycle(),
        PaneLifecycle::Spawning
    );
}

#[test]
fn new_tab_appends_after_existing_tabs_without_moving_the_client() {
    let existing = TabId::new();
    let pane = PaneId::new();
    let client = client_on(existing);
    let client_id = client.id();
    let mut session = session_with(
        vec![client],
        vec![single_pane_tab(existing, pane, 0)],
        vec![pane],
    );

    let _ = new_tab(&mut session, "second".to_owned(), SystemTime::UNIX_EPOCH);

    assert_eq!(session.tabs.len(), 2);
    let new = session.tabs.values().find(|t| t.name == "second").unwrap();
    assert_eq!(new.index, 1);
    // Creating a tab does not move the client onto it.
    assert_eq!(
        session.clients.get(client_id).unwrap().active_tab(),
        existing
    );
}

// --- close_tab -------------------------------------------------------------

#[test]
fn close_tab_emits_a_close_remove_pair_per_pane_then_tab_closed() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb1, pb2) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![],
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
        vec![],
        vec![
            single_pane_tab(a, pa, 0),
            single_pane_tab(b, pb, 1),
            single_pane_tab(c, pc, 2),
        ],
        vec![pa, pb, pc],
    );

    let _ = close_tab(&mut session, b); // remove the middle tab

    assert_eq!(session.tabs[&a].index, 0);
    assert_eq!(session.tabs[&c].index, 1); // was 2, densified to 1
}

#[test]
fn close_tab_moves_a_viewing_client_to_the_nearest_tab() {
    let (a, b, c) = (TabId::new(), TabId::new(), TabId::new());
    let (pa, pb, pc) = (PaneId::new(), PaneId::new(), PaneId::new());
    let client = client_on(b); // viewing the middle tab
    let client_id = client.id();
    let mut session = session_with(
        vec![client],
        vec![
            single_pane_tab(a, pa, 0),
            single_pane_tab(b, pb, 1),
            single_pane_tab(c, pc, 2),
        ],
        vec![pa, pb, pc],
    );

    let events = close_tab(&mut session, b);

    // nearest to index 1 is the previous tab (a, index 0).
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TabFocused(t) if t.tab_id == a)));
}

#[test]
fn close_tab_leaves_a_non_viewing_clients_active_tab() {
    let (a, b) = (TabId::new(), TabId::new());
    let (pa, pb) = (PaneId::new(), PaneId::new());
    let client = client_on(a); // viewing a, not the closed b
    let client_id = client.id();
    let mut session = session_with(
        vec![client],
        vec![single_pane_tab(a, pa, 0), single_pane_tab(b, pb, 1)],
        vec![pa, pb],
    );

    let events = close_tab(&mut session, b);

    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), a);
    assert!(!events.iter().any(|e| matches!(e, Event::TabFocused(_))));
}

#[test]
fn closing_the_last_tab_quits() {
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![], vec![single_pane_tab(a, pa, 0)], vec![pa]);

    let events = close_tab(&mut session, a);

    assert!(session.tabs.is_empty());
    assert!(events.iter().any(|e| matches!(e, Event::Quit)));
}

#[test]
fn closing_an_unknown_tab_is_a_noop() {
    let mut session = session_with(vec![], vec![], vec![]);
    let events = close_tab(&mut session, TabId::new());
    assert!(events.is_empty());
}

// --- rename_tab ------------------------------------------------------------

#[test]
fn rename_tab_changes_the_name_and_preserves_layout() {
    let a = TabId::new();
    let (l, r) = (PaneId::new(), PaneId::new());
    let mut session = session_with(vec![], vec![two_pane_tab(a, l, r, 0)], vec![l, r]);
    let layout_before = session.tabs[&a].layout.clone();

    let events = rename_tab(&mut session, a, "build".to_owned());

    assert_eq!(session.tabs[&a].name, "build");
    assert_eq!(session.tabs[&a].layout, layout_before);
    assert!(
        matches!(events.as_slice(), [Event::TabRenamed(t)] if t.tab_id == a && t.name == "build")
    );
}

#[test]
fn renaming_to_the_same_name_is_a_noop() {
    let a = TabId::new();
    let pa = PaneId::new();
    let mut session = session_with(vec![], vec![single_pane_tab(a, pa, 0)], vec![pa]);

    // single_pane_tab names it "code".
    let events = rename_tab(&mut session, a, "code".to_owned());

    assert!(events.is_empty());
}

#[test]
fn renaming_an_unknown_tab_is_a_noop() {
    let mut session = session_with(vec![], vec![], vec![]);
    let events = rename_tab(&mut session, TabId::new(), "x".to_owned());
    assert!(events.is_empty());
}

// --- focus_tab -------------------------------------------------------------

#[test]
fn focus_tab_by_id_switches_active_and_emits() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[0]);

    let events = focus_tab(&session, &mut client, TabTarget::Id(ids[2]));

    assert_eq!(client.active_tab(), ids[2]);
    assert!(matches!(events.as_slice(), [Event::TabFocused(t)] if t.tab_id == ids[2]));
}

#[test]
fn focus_tab_by_index_switches_to_that_position() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[0]);

    let events = focus_tab(&session, &mut client, TabTarget::Index(1));

    assert_eq!(client.active_tab(), ids[1]);
    assert!(matches!(events.as_slice(), [Event::TabFocused(t)] if t.tab_id == ids[1]));
}

#[test]
fn focus_next_wraps_from_last_to_first() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[2]); // last

    let _ = focus_tab(&session, &mut client, TabTarget::Next);

    assert_eq!(client.active_tab(), ids[0]);
}

#[test]
fn focus_prev_wraps_from_first_to_last() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[0]); // first

    let _ = focus_tab(&session, &mut client, TabTarget::Prev);

    assert_eq!(client.active_tab(), ids[2]);
}

#[test]
fn focusing_the_already_active_tab_is_a_noop() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[1]);

    let events = focus_tab(&session, &mut client, TabTarget::Id(ids[1]));

    assert!(events.is_empty());
    assert_eq!(client.active_tab(), ids[1]);
}

#[test]
fn focusing_an_out_of_range_index_is_a_noop() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[0]);

    let events = focus_tab(&session, &mut client, TabTarget::Index(9));

    assert!(events.is_empty());
    assert_eq!(client.active_tab(), ids[0]);
}

#[test]
fn focusing_an_unknown_id_is_a_noop() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[0]);

    let events = focus_tab(&session, &mut client, TabTarget::Id(TabId::new()));

    assert!(events.is_empty());
    assert_eq!(client.active_tab(), ids[0]);
}

#[test]
fn focus_tab_preserves_per_tab_pane_focus() {
    let (session, ids) = three_tab_session();
    let mut client = client_on(ids[0]);
    let focused_in_two = PaneId::new();
    client.update_focused_pane(ids[2], focused_in_two);

    let _ = focus_tab(&session, &mut client, TabTarget::Id(ids[2]));

    // Switching tabs leaves the recorded pane focus intact.
    assert_eq!(client.focused_pane(ids[2]), Some(focused_in_two));
}

// --- move_tab --------------------------------------------------------------

#[test]
fn move_tab_forward_shifts_the_span_back() {
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3

    let events = move_tab(&mut session, ids[1], 3); // move b to the end

    assert_eq!(session.tabs[&ids[0]].index, 0); // a
    assert_eq!(session.tabs[&ids[2]].index, 1); // c
    assert_eq!(session.tabs[&ids[3]].index, 2); // d
    assert_eq!(session.tabs[&ids[1]].index, 3); // b
    assert!(matches!(
        events.as_slice(),
        [Event::TabMoved(m)] if m.tab_id == ids[1] && m.old_index == 1 && m.new_index == 3
    ));
}

#[test]
fn move_tab_backward_shifts_the_span_forward() {
    let (mut session, ids) = four_tab_session(); // a0 b1 c2 d3

    let _ = move_tab(&mut session, ids[2], 0); // move c to the front

    assert_eq!(session.tabs[&ids[2]].index, 0); // c
    assert_eq!(session.tabs[&ids[0]].index, 1); // a
    assert_eq!(session.tabs[&ids[1]].index, 2); // b
    assert_eq!(session.tabs[&ids[3]].index, 3); // d
}

#[test]
fn move_tab_clamps_an_out_of_bounds_index() {
    let (mut session, ids) = four_tab_session();

    let events = move_tab(&mut session, ids[0], 99); // clamps to len-1 = 3

    assert_eq!(session.tabs[&ids[0]].index, 3);
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
