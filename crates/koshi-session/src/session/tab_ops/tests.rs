//! Tests for tab operations: creation, deletion, focus, and reordering.
//!
//! Holds fixtures that build test sessions, tabs, clients, and panes, and
//! exercises [`commit_new_tab`], [`commit_profile_tab`], [`close_tab`],
//! [`focus_tab`], [`resolve_tab_target`] and [`move_tab`] over the state
//! configurations each one can be called in, asserting the events emitted and
//! the state left behind.

use super::*;

use std::path::PathBuf;
use std::time::SystemTime;

use koshi_core::geometry::{Size, SplitDirection};
use koshi_core::ids::SessionId;
use koshi_layout::tree::SplitNode;
use koshi_pane::pane::lifecycle::PaneLifecycle;
use koshi_pane::pane::state::PaneRecord;

use crate::client::{ClientOrigin, ClientRegistry};
use crate::error::SessionConsistencyError;
use crate::session::lifecycle::SessionLifecycle;
use crate::session::state::tests::build_default_floating_member;

const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A single-pane tab named `"code"` at display position `tab_index`.
fn build_single_pane_tab(tab_id: TabId, pane_id: PaneId, tab_index: usize) -> Tab {
    Tab::from_root_pane(tab_id, "code".to_owned(), tab_index, pane_id)
}

/// A tab split left/right between `left_pane_id` and `right_pane_id` at display
/// `tab_index`.
fn build_two_pane_tab(
    tab_id: TabId,
    left_pane_id: PaneId,
    right_pane_id: PaneId,
    tab_index: usize,
) -> Tab {
    let mut tab = Tab::from_root_pane(tab_id, "code".to_owned(), tab_index, left_pane_id);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    )));
    tab
}

/// A client of `session_id` viewing `tab_id`, no per-tab focus recorded yet.
/// Its `session_id` is the one [`Session::validate_session_consistency`] demands of an attached
/// client.
fn build_client_on(session_id: SessionId, tab_id: TabId) -> Client {
    Client::from_attachment(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        TEST_VIEWPORT_SIZE,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// A `Spawning` terminal pane record for `pane_id`.
fn build_pane_record(pane_id: PaneId) -> PaneRecord {
    PaneRecord::from_terminal_pane(pane_id)
}

/// A session holding the given tabs and (registered) panes, with no clients
/// attached yet. Attach clients afterward with [`Session::attach_client`] so
/// each carries the session's own id.
fn build_session_with(tabs: Vec<Tab>, pane_ids: Vec<PaneId>) -> Session {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    for tab in tabs {
        session.tabs.insert(tab.get_tab_id(), tab);
    }
    for pane_id in pane_ids {
        let _ = session
            .panes
            .register_pane_record(build_pane_record(pane_id));
    }
    session
}

/// Three single-pane tabs at indices 0, 1, 2.
fn build_three_tab_session() -> (Session, [TabId; 3]) {
    let tab_ids = [TabId::new(), TabId::new(), TabId::new()];
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];
    let tabs = vec![
        build_single_pane_tab(tab_ids[0], pane_ids[0], 0),
        build_single_pane_tab(tab_ids[1], pane_ids[1], 1),
        build_single_pane_tab(tab_ids[2], pane_ids[2], 2),
    ];
    (build_session_with(tabs, pane_ids.to_vec()), tab_ids)
}

/// Four single-pane tabs at indices 0, 1, 2, 3.
fn build_four_tab_session() -> (Session, [TabId; 4]) {
    let tab_ids = [TabId::new(), TabId::new(), TabId::new(), TabId::new()];
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new()];
    let tabs: Vec<Tab> = (0..4)
        .map(|tab_index| build_single_pane_tab(tab_ids[tab_index], pane_ids[tab_index], tab_index))
        .collect();
    (build_session_with(tabs, pane_ids.to_vec()), tab_ids)
}

#[test]
fn fixtures_build_a_consistent_session() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(second_tab_id, second_pane_id, 1),
        ],
        vec![first_pane_id, second_pane_id],
    );
    session.attach_client(build_client_on(session.session_id, first_tab_id));

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

// --- commit_new_tab ---------------------------------------------------------

#[test]
fn commit_new_tab_registers_a_running_pane_and_emits_created_then_pane_created() {
    let mut session = build_session_with(vec![], vec![]);
    let (new_tab_id, new_pane_id) = (TabId::new(), PaneId::new());

    let (previous_active_tab_id, emitted_events) = commit_new_tab(
        &mut session,
        new_tab_id,
        new_pane_id,
        "logs".to_owned(),
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(previous_active_tab_id, None);
    assert_eq!(session.tabs.len(), 1);
    let tab = &session.tabs[&new_tab_id];
    assert_eq!(tab.get_tab_name(), "logs");
    assert_eq!(tab.get_tab_index(), 0);
    assert_eq!(
        tab.get_layout_tree().list_leaf_pane_ids(),
        vec![new_pane_id]
    );

    match emitted_events.as_slice() {
        [Event::TabCreated(tab_created), Event::PaneCreated(pane_created)] => {
            assert_eq!(tab_created.tab_id, new_tab_id);
            assert_eq!(pane_created.tab_id, Some(new_tab_id));
            assert_eq!(pane_created.pane_id, new_pane_id);
        }
        unexpected_events => panic!("unexpected events: {unexpected_events:?}"),
    }
    // The child was spawned before the commit, so the pane enters `Running`.
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(new_pane_id)
            .unwrap()
            .get_lifecycle(),
        PaneLifecycle::Running
    );
}

#[test]
fn commit_new_tab_first_tab_transitions_the_session_to_running() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);

    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "first".to_owned(),
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn commit_new_tab_after_the_first_leaves_the_session_running() {
    let mut session = build_session_with(vec![], vec![]);
    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "first".to_owned(),
        None,
        NewPaneSpec::default(),
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        PaneId::new(),
        "second".to_owned(),
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn commit_new_tab_on_a_stopping_session_leaves_it_stopping() {
    // `FirstTabCreated` is legal only from `Starting`. A session already
    // winding down keeps `Stopping`, and the tab is still created.
    let mut session = build_session_with(vec![], vec![]);
    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
    let (tab_id, pane_id) = (TabId::new(), PaneId::new());

    let (previous_active_tab_id, emitted_events) = commit_new_tab(
        &mut session,
        tab_id,
        pane_id,
        "logs".to_owned(),
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
    assert_eq!(previous_active_tab_id, None);
    assert_eq!(
        emitted_events,
        vec![
            Event::TabCreated(TabCreated { tab_id }),
            Event::PaneCreated(PaneCreated {
                pane_id,
                tab_id: Some(tab_id)
            }),
        ]
    );
    assert_eq!(session.tabs[&tab_id].get_tab_index(), 0);
}

#[test]
fn commit_new_tab_appends_after_existing_tabs() {
    let existing_tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(existing_tab_id, pane_id, 0)],
        vec![pane_id],
    );
    let new_tab_id = TabId::new();

    let _ = commit_new_tab(
        &mut session,
        new_tab_id,
        PaneId::new(),
        "second".to_owned(),
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(session.tabs.len(), 2);
    assert_eq!(session.tabs[&new_tab_id].get_tab_index(), 1);
}

#[test]
fn commit_new_tab_switches_the_focus_client_and_emits_focus_events() {
    let existing_tab_id = TabId::new();
    let existing_pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(existing_tab_id, existing_pane_id, 0)],
        vec![existing_pane_id],
    );
    let client = build_client_on(session.session_id, existing_tab_id);
    let client_id = client.get_client_id();
    session.attach_client(client);
    let (new_tab_id, new_pane_id) = (TabId::new(), PaneId::new());

    let (previous_active_tab_id, emitted_events) = commit_new_tab(
        &mut session,
        new_tab_id,
        new_pane_id,
        "second".to_owned(),
        Some(client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(previous_active_tab_id, Some(existing_tab_id));
    let client = session.clients.get_client_by_id(client_id).unwrap();
    assert_eq!(client.get_active_tab_id(), new_tab_id);
    assert_eq!(client.get_focused_pane_id(new_tab_id), Some(new_pane_id));
    assert_eq!(session.tabs[&new_tab_id].list_focus_mru(), &[new_pane_id]);

    match emitted_events.as_slice() {
        [Event::TabCreated(tab_created), Event::PaneCreated(pane_created), Event::TabFocused(tab_focused), Event::PaneFocused(pane_focused)] =>
        {
            assert_eq!(tab_created.tab_id, new_tab_id);
            assert_eq!(pane_created.pane_id, new_pane_id);
            assert_eq!(pane_created.tab_id, Some(new_tab_id));
            assert_eq!(tab_focused.client_id, client_id);
            assert_eq!(tab_focused.tab_id, new_tab_id);
            assert_eq!(tab_focused.previous_tab_id, existing_tab_id);
            assert_eq!(pane_focused.client_id, client_id);
            assert_eq!(pane_focused.tab_id, Some(new_tab_id));
            assert_eq!(pane_focused.pane_id, new_pane_id);
            assert_eq!(pane_focused.previous_pane_id, None);
        }
        unexpected_events => panic!("unexpected events: {unexpected_events:?}"),
    }
}

#[test]
fn commit_new_tab_does_not_move_other_clients() {
    let existing_tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(existing_tab_id, pane_id, 0)],
        vec![pane_id],
    );
    let focused_client = build_client_on(session.session_id, existing_tab_id);
    let bystander_client = build_client_on(session.session_id, existing_tab_id);
    let focused_client_id = focused_client.get_client_id();
    let bystander_client_id = bystander_client.get_client_id();
    session.attach_client(focused_client);
    session.attach_client(bystander_client);
    let new_tab_id = TabId::new();

    let _ = commit_new_tab(
        &mut session,
        new_tab_id,
        PaneId::new(),
        "second".to_owned(),
        Some(focused_client_id),
        NewPaneSpec::default(),
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(focused_client_id)
            .unwrap()
            .get_active_tab_id(),
        new_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(bystander_client_id)
            .unwrap()
            .get_active_tab_id(),
        existing_tab_id
    );
}

#[test]
fn commit_new_tab_with_a_stale_focus_client_moves_no_view() {
    let existing_tab_id = TabId::new();
    let existing_pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(existing_tab_id, existing_pane_id, 0)],
        vec![existing_pane_id],
    );
    let client = build_client_on(session.session_id, existing_tab_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let new_tab_id = TabId::new();
    let new_pane_id = PaneId::new();
    let (previous_active_tab_id, emitted_events) = commit_new_tab(
        &mut session,
        new_tab_id,
        new_pane_id,
        "second".to_owned(),
        Some(ClientId::new()),
        NewPaneSpec::default(),
    );

    assert_eq!(previous_active_tab_id, None);
    assert_eq!(
        emitted_events,
        vec![
            Event::TabCreated(TabCreated { tab_id: new_tab_id }),
            Event::PaneCreated(PaneCreated {
                pane_id: new_pane_id,
                tab_id: Some(new_tab_id),
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        existing_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(new_tab_id),
        None
    );
}

#[test]
fn commit_new_tab_records_the_new_pane_spec_on_the_root_pane() {
    let mut session = build_session_with(vec![], vec![]);
    let new_pane_id = PaneId::new();
    let new_pane_spec = NewPaneSpec {
        working_directory: Some(PathBuf::from("/srv")),
        spawn_spec: None,
    };

    let _ = commit_new_tab(
        &mut session,
        TabId::new(),
        new_pane_id,
        "logs".to_owned(),
        None,
        new_pane_spec,
    );

    let pane_record = session.panes.get_pane_record_by_id(new_pane_id).unwrap();
    assert_eq!(pane_record.working_directory, Some(PathBuf::from("/srv")));
    assert_eq!(pane_record.spawn_spec, None);
}

// --- close_tab -------------------------------------------------------------

#[test]
fn close_tab_emits_a_close_remove_pair_per_pane_then_tab_closed() {
    let (surviving_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (surviving_pane_id, first_closed_pane_id, second_closed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(surviving_tab_id, surviving_pane_id, 0),
            build_two_pane_tab(
                closed_tab_id,
                first_closed_pane_id,
                second_closed_pane_id,
                1,
            ),
        ],
        vec![
            surviving_pane_id,
            first_closed_pane_id,
            second_closed_pane_id,
        ],
    );

    let emitted_events = close_tab(&mut session, closed_tab_id);

    assert_eq!(
        session.panes.get_pane_record_by_id(first_closed_pane_id),
        None
    );
    assert_eq!(
        session.panes.get_pane_record_by_id(second_closed_pane_id),
        None
    );
    assert_eq!(session.panes.count_pane_records(), 1);
    assert!(!session.tabs.contains_key(&closed_tab_id));

    // One close/remove pair per pane in layout order, then the tab. The surviving tab
    // survives, so no `Quit`.
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: first_closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: first_closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: second_closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: second_closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
        ]
    );
}

#[test]
fn close_tab_renumbers_survivors_densely() {
    let (first_tab_id, closed_tab_id, third_tab_id) = (TabId::new(), TabId::new(), TabId::new());
    let (first_pane_id, closed_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
            build_single_pane_tab(third_tab_id, third_pane_id, 2),
        ],
        vec![first_pane_id, closed_pane_id, third_pane_id],
    );

    let _ = close_tab(&mut session, closed_tab_id); // remove the middle tab

    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&third_tab_id].get_tab_index(), 1); // was 2, densified to 1
}

#[test]
fn close_tab_moves_a_viewing_client_to_the_nearest_tab() {
    let (first_tab_id, closed_tab_id, third_tab_id) = (TabId::new(), TabId::new(), TabId::new());
    let (first_pane_id, closed_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
            build_single_pane_tab(third_tab_id, third_pane_id, 2),
        ],
        vec![first_pane_id, closed_pane_id, third_pane_id],
    );
    let client = build_client_on(session.session_id, closed_tab_id); // viewing the middle tab
    let client_id = client.get_client_id();
    session.attach_client(client);

    let emitted_events = close_tab(&mut session, closed_tab_id);

    // The nearest tab to index 1 is `first_tab_id`, at index 0.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        first_tab_id
    );
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: first_tab_id,
                previous_tab_id: closed_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(first_tab_id),
                pane_id: first_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(first_tab_id),
        Some(first_pane_id)
    );
}

#[test]
fn close_tab_at_the_first_index_moves_viewers_to_the_next_tab() {
    // Closing index 0 has no previous tab to fall back on, so
    // `nearest_surviving_tab` takes the smallest index above it.
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[0]);
    let closed_pane_id = get_only_pane_id(&session, tab_ids[0]);
    let landed_pane_id = get_only_pane_id(&session, tab_ids[1]);

    let emitted_events = close_tab(&mut session, tab_ids[0]);

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[1]
    );
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(tab_ids[0]),
            }),
            Event::TabClosed(TabClosed { tab_id: tab_ids[0] }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: tab_ids[1],
                previous_tab_id: tab_ids[0],
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_ids[1]),
                pane_id: landed_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_ids[1]),
        Some(landed_pane_id)
    );
    // The survivors close ranks behind the gone first tab.
    assert_eq!(session.tabs[&tab_ids[1]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 1);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn close_tab_leaves_a_non_viewing_clients_active_tab() {
    let (viewed_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (viewed_pane_id, closed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(viewed_tab_id, viewed_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
        ],
        vec![viewed_pane_id, closed_pane_id],
    );
    let client = build_client_on(session.session_id, viewed_tab_id); // viewing the first tab, not the closed second tab
    let client_id = client.get_client_id();
    session.attach_client(client);

    let emitted_events = close_tab(&mut session, closed_tab_id);

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        viewed_tab_id
    );
    // No client was viewing the closed tab, so nothing is refocused.
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
        ]
    );
}

#[test]
fn close_tab_drops_the_focus_and_zoom_a_non_viewing_client_held_in_it() {
    // A client sitting on another tab still carries per-tab state for the tab
    // that closes. Both entries go, so nothing points at the gone pane.
    let (viewed_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (viewed_pane_id, closed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(viewed_tab_id, viewed_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
        ],
        vec![viewed_pane_id, closed_pane_id],
    );
    let mut client = build_client_on(session.session_id, viewed_tab_id);
    client.update_focused_pane(closed_tab_id, closed_pane_id);
    client.zoom_pane(closed_tab_id, closed_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);
    assert_eq!(session.validate_session_consistency(), Ok(()));

    let _ = close_tab(&mut session, closed_tab_id);

    let client = session.clients.get_client_by_id(client_id).unwrap();
    assert_eq!(client.get_active_tab_id(), viewed_tab_id);
    assert_eq!(client.get_focused_pane_id(closed_tab_id), None);
    assert_eq!(client.get_zoomed_pane_id(closed_tab_id), None);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn close_tab_emits_pane_events_for_a_leaf_missing_from_the_registry() {
    // A layout leaf with no registry pane record (a desync) still gets its
    // close/remove pair, so the runtime tears down whatever it holds for it.
    let (surviving_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (surviving_pane_id, first_closed_pane_id, unregistered_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(surviving_tab_id, surviving_pane_id, 0),
            build_two_pane_tab(closed_tab_id, first_closed_pane_id, unregistered_pane_id, 1),
        ],
        vec![surviving_pane_id, first_closed_pane_id], // the unregistered pane is a leaf of the closed tab
    );

    let emitted_events = close_tab(&mut session, closed_tab_id);

    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: first_closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: first_closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: unregistered_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: unregistered_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
        ]
    );
    assert_eq!(
        session.panes.get_pane_record_by_id(first_closed_pane_id),
        None
    );
    assert_eq!(session.panes.count_pane_records(), 1);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn closing_the_last_tab_quits() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, pane_id, 0)],
        vec![pane_id],
    );

    let emitted_events = close_tab(&mut session, tab_id);

    assert!(session.tabs.is_empty());
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id }),
            Event::PaneRemoved(PaneRemoved {
                pane_id,
                tab_id: Some(tab_id)
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn closing_an_unknown_tab_is_a_noop() {
    let mut session = build_session_with(vec![], vec![]);
    let emitted_events = close_tab(&mut session, TabId::new());
    assert!(emitted_events.is_empty());
}

// --- focus_tab -------------------------------------------------------------

/// The single pane of a single-pane tab.
fn get_only_pane_id(session: &Session, tab_id: TabId) -> PaneId {
    session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids()[0]
}

/// Attach a fresh client viewing `tab` and return its id.
fn attach_client_on(session: &mut Session, tab_id: TabId) -> ClientId {
    let client = build_client_on(session.session_id, tab_id);
    let client_id = client.get_client_id();
    session.attach_client(client);
    client_id
}

/// Builds the target of one focus case from the three tab ids of its session.
type TabTargetFromTabIds = fn([TabId; 3]) -> TabTarget;

#[test]
fn focus_tab_moves_the_client_to_the_resolved_tab_and_lands_on_its_pane() {
    // (start tab position, target, landing tab position)
    let focus_cases: [(usize, TabTargetFromTabIds, usize); 7] = [
        (0, |tab_ids| TabTarget::Id(tab_ids[2]), 2),
        (0, |_| TabTarget::Index(1), 1),
        (0, |_| TabTarget::Index(2), 2),
        (0, |_| TabTarget::Next, 1),
        (2, |_| TabTarget::Previous, 1),
        (2, |_| TabTarget::Next, 0),
        (0, |_| TabTarget::Previous, 2),
    ];

    for (start_tab_index, build_tab_target, landing_tab_index) in focus_cases {
        let (mut session, tab_ids) = build_three_tab_session();
        let start_tab_id = tab_ids[start_tab_index];
        let landing_tab_id = tab_ids[landing_tab_index];
        let client_id = attach_client_on(&mut session, start_tab_id);
        let landed_pane_id = get_only_pane_id(&session, landing_tab_id);
        let tab_target = build_tab_target(tab_ids);

        let emitted_events = focus_tab(&mut session, client_id, tab_target);

        assert_eq!(
            emitted_events,
            vec![
                Event::TabFocused(TabFocused {
                    client_id,
                    tab_id: landing_tab_id,
                    previous_tab_id: start_tab_id,
                }),
                Event::PaneFocused(PaneFocused {
                    client_id,
                    tab_id: Some(landing_tab_id),
                    pane_id: landed_pane_id,
                    previous_pane_id: None,
                }),
            ],
            "{tab_target:?} from tab {start_tab_index}"
        );
        let client = session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached");
        assert_eq!(
            client.get_active_tab_id(),
            landing_tab_id,
            "{tab_target:?} from tab {start_tab_index}"
        );
        assert_eq!(
            client.get_focused_pane_id(landing_tab_id),
            Some(landed_pane_id),
            "{tab_target:?} from tab {start_tab_index}"
        );
    }
}

#[test]
fn focusing_the_already_active_tab_is_a_noop() {
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[1]);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Id(tab_ids[1]));

    assert!(emitted_events.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[1]
    );
}

#[test]
fn focusing_an_out_of_range_index_is_a_noop() {
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[0]);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Index(9));

    assert!(emitted_events.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[0]
    );
}

#[test]
fn focusing_the_index_one_past_the_last_tab_is_a_noop() {
    // Three tabs fill 0..=2; index 3 is the first slot that does not exist.
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[0]);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Index(3));

    assert!(emitted_events.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[0]
    );
}

#[test]
fn focus_tab_with_no_tabs_left_is_a_noop_not_a_panic() {
    // Closing the last tab leaves the client's `active_tab_id` naming a tab that
    // is gone and `session.tabs` empty. Next/Previous must not divide by the zero
    // tab count, and Index/Id must find nothing.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, pane_id, 0)],
        vec![pane_id],
    );
    let client_id = attach_client_on(&mut session, tab_id);
    let _ = close_tab(&mut session, tab_id);
    assert!(session.tabs.is_empty());

    assert!(focus_tab(&mut session, client_id, TabTarget::Next).is_empty());
    assert!(focus_tab(&mut session, client_id, TabTarget::Previous).is_empty());
    assert!(focus_tab(&mut session, client_id, TabTarget::Index(0)).is_empty());
    assert!(focus_tab(&mut session, client_id, TabTarget::Id(tab_id)).is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_id
    );
}

#[test]
fn focusing_an_unknown_id_is_a_noop() {
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[0]);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Id(TabId::new()));

    assert!(emitted_events.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[0]
    );
}

#[test]
fn focus_tab_for_an_unattached_client_is_a_noop() {
    let (mut session, tab_ids) = build_three_tab_session();
    let attached_client_id = attach_client_on(&mut session, tab_ids[0]);

    let emitted_events = focus_tab(&mut session, ClientId::new(), TabTarget::Id(tab_ids[2]));

    assert!(emitted_events.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(attached_client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[0]
    );
}

#[test]
fn focus_tab_preserves_per_tab_pane_focus() {
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[0]);
    let focused_pane_id = PaneId::new();
    session
        .clients
        .get_client_mut_by_id(client_id)
        .unwrap()
        .update_focused_pane(tab_ids[2], focused_pane_id);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Id(tab_ids[2]));

    // Switching tabs leaves the recorded pane focus intact, and lands on no
    // other pane, so only the tab switch is reported.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_ids[2]),
        Some(focused_pane_id)
    );
    assert_eq!(
        emitted_events,
        vec![Event::TabFocused(TabFocused {
            client_id,
            tab_id: tab_ids[2],
            previous_tab_id: tab_ids[0],
        })]
    );
}

#[test]
fn focus_tab_lands_on_the_tabs_most_recent_pane() {
    // A client that never focused a pane in the tab it switches to lands on
    // that tab's focus history head, not on the first leaf in layout order.
    let (first_tab_id, target_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, left_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_two_pane_tab(target_tab_id, left_pane_id, right_pane_id, 1),
        ],
        vec![first_pane_id, left_pane_id, right_pane_id],
    );
    session
        .tabs
        .get_mut(&target_tab_id)
        .unwrap()
        .record_focus_mru(right_pane_id);
    let client_id = attach_client_on(&mut session, first_tab_id);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Id(target_tab_id));

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(target_tab_id),
        Some(right_pane_id)
    );
    assert_eq!(
        emitted_events,
        vec![
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: target_tab_id,
                previous_tab_id: first_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(target_tab_id),
                pane_id: right_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn focus_tab_lands_on_the_first_leaf_without_a_focus_history() {
    // With no focus history to read, the landing pane is the first leaf in
    // layout order.
    let (first_tab_id, target_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, left_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_two_pane_tab(target_tab_id, left_pane_id, right_pane_id, 1),
        ],
        vec![first_pane_id, left_pane_id, right_pane_id],
    );
    let client_id = attach_client_on(&mut session, first_tab_id);
    assert!(session.tabs[&target_tab_id].list_focus_mru().is_empty());

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(target_tab_id));

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(target_tab_id),
        Some(left_pane_id)
    );
    // The landing is recorded as the tab's most recent focus.
    assert_eq!(
        session.tabs[&target_tab_id].list_focus_mru(),
        &[left_pane_id]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn focus_tab_skips_a_landing_candidate_with_no_registry_record() {
    // The left pane is a layout leaf the registry does not hold, so the landing walk
    // passes over it and takes the right pane.
    let (first_tab_id, target_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, left_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_two_pane_tab(target_tab_id, left_pane_id, right_pane_id, 1),
        ],
        vec![first_pane_id, right_pane_id],
    );
    let client_id = attach_client_on(&mut session, first_tab_id);

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(target_tab_id));

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(target_tab_id),
        Some(right_pane_id)
    );
}

#[test]
fn focus_tab_skips_a_history_entry_that_is_not_a_leaf() {
    // A focus history entry naming a pane the tab's layout no longer holds is
    // passed over, so the landing never focuses a pane outside the tab.
    let (first_tab_id, target_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, target_pane_id, stale_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(target_tab_id, target_pane_id, 1),
        ],
        vec![first_pane_id, target_pane_id],
    );
    let _ = session
        .panes
        .register_pane_record(build_pane_record(stale_pane_id));
    session
        .tabs
        .get_mut(&target_tab_id)
        .unwrap()
        .record_focus_mru(stale_pane_id);
    let client_id = attach_client_on(&mut session, first_tab_id);

    let _ = focus_tab(&mut session, client_id, TabTarget::Id(target_tab_id));

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(target_tab_id),
        Some(target_pane_id)
    );
}

#[test]
fn focus_tab_lands_on_nothing_when_no_leaf_has_a_record() {
    // Every leaf of the target tab is missing from the registry: the switch
    // still happens, and it reports the tab switch alone.
    let (first_tab_id, target_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, target_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(target_tab_id, target_pane_id, 1),
        ],
        vec![first_pane_id],
    );
    let client_id = attach_client_on(&mut session, first_tab_id);

    let emitted_events = focus_tab(&mut session, client_id, TabTarget::Id(target_tab_id));

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(target_tab_id),
        None
    );
    assert_eq!(
        emitted_events,
        vec![Event::TabFocused(TabFocused {
            client_id,
            tab_id: target_tab_id,
            previous_tab_id: first_tab_id,
        })]
    );
}

#[test]
fn close_tab_lands_a_moved_client_on_the_surviving_tabs_recent_pane() {
    // Closing the tab a client views moves it to the nearest survivor and
    // gives it that tab's most recent pane, so the arriving client always has
    // a pane to type into.
    let (surviving_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (left_pane_id, right_pane_id, closed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_two_pane_tab(surviving_tab_id, left_pane_id, right_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
        ],
        vec![left_pane_id, right_pane_id, closed_pane_id],
    );
    session
        .tabs
        .get_mut(&surviving_tab_id)
        .unwrap()
        .record_focus_mru(right_pane_id);
    let client_id = attach_client_on(&mut session, closed_tab_id);

    let emitted_events = close_tab(&mut session, closed_tab_id);

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        surviving_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(surviving_tab_id),
        Some(right_pane_id)
    );
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: surviving_tab_id,
                previous_tab_id: closed_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(surviving_tab_id),
                pane_id: right_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

// --- resolve_tab_target -----------------------------------------------------

#[test]
fn resolve_tab_target_reads_each_variant_against_the_display_order() {
    let (session, tab_ids) = build_three_tab_session(); // indices 0, 1, 2

    assert_eq!(
        resolve_tab_target(&session, tab_ids[0], TabTarget::Id(tab_ids[2])),
        Some(tab_ids[2])
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[0], TabTarget::Id(TabId::new())),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[0], TabTarget::Index(1)),
        Some(tab_ids[1])
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[0], TabTarget::Index(3)),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[0], TabTarget::Next),
        Some(tab_ids[1])
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[2], TabTarget::Next),
        Some(tab_ids[0])
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[2], TabTarget::Previous),
        Some(tab_ids[1])
    );
    assert_eq!(
        resolve_tab_target(&session, tab_ids[0], TabTarget::Previous),
        Some(tab_ids[2])
    );
}

#[test]
fn resolve_tab_target_steps_nowhere_from_an_active_tab_the_session_lost() {
    // `Next` and `Previous` step from `active_tab_id`; an id the session does not
    // hold has no position to step from. `Id` and `Index` never read
    // `active_tab_id`, so they still resolve.
    let (session, tab_ids) = build_three_tab_session();
    let missing_tab_id = TabId::new();

    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Next),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Previous),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Index(0)),
        Some(tab_ids[0])
    );
    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Id(tab_ids[1])),
        Some(tab_ids[1])
    );
}

#[test]
fn resolve_tab_target_in_a_session_with_no_tabs_resolves_to_none() {
    // Zero tabs: every variant resolves to `None`, and `Next`/`Previous` stop
    // before taking the step modulo the tab count.
    let session = build_session_with(vec![], vec![]);
    let missing_tab_id = TabId::new();

    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Next),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Previous),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Index(0)),
        None
    );
    assert_eq!(
        resolve_tab_target(&session, missing_tab_id, TabTarget::Id(missing_tab_id)),
        None
    );
}

// --- move_tab --------------------------------------------------------------

#[test]
fn move_tab_forward_shifts_the_span_back() {
    let (mut session, tab_ids) = build_four_tab_session(); // 0, 1, 2, 3

    let emitted_events = move_tab(&mut session, tab_ids[1], 3); // move the second tab to the end

    assert_eq!(session.tabs[&tab_ids[0]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_ids[3]].get_tab_index(), 2);
    assert_eq!(session.tabs[&tab_ids[1]].get_tab_index(), 3);
    assert_eq!(
        emitted_events,
        vec![Event::TabMoved(TabMoved {
            tab_id: tab_ids[1],
            previous_tab_index: 1,
            new_tab_index: 3,
        })]
    );
}

#[test]
fn move_tab_backward_shifts_the_span_forward() {
    let (mut session, tab_ids) = build_four_tab_session(); // 0, 1, 2, 3

    let emitted_events = move_tab(&mut session, tab_ids[2], 0); // move the third tab to the front

    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[0]].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_ids[1]].get_tab_index(), 2);
    assert_eq!(session.tabs[&tab_ids[3]].get_tab_index(), 3);
    assert_eq!(
        emitted_events,
        vec![Event::TabMoved(TabMoved {
            tab_id: tab_ids[2],
            previous_tab_index: 2,
            new_tab_index: 0,
        })]
    );
}

#[test]
fn move_tab_clamps_an_out_of_bounds_index() {
    let (mut session, tab_ids) = build_four_tab_session();

    let emitted_events = move_tab(&mut session, tab_ids[0], usize::MAX); // clamps to len-1 = 3

    assert_eq!(session.tabs[&tab_ids[1]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_ids[3]].get_tab_index(), 2);
    assert_eq!(session.tabs[&tab_ids[0]].get_tab_index(), 3);
    assert_eq!(
        emitted_events,
        vec![Event::TabMoved(TabMoved {
            tab_id: tab_ids[0],
            previous_tab_index: 0,
            new_tab_index: 3,
        })]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn moving_to_the_same_index_is_a_noop() {
    let (mut session, tab_ids) = build_four_tab_session();

    let emitted_events = move_tab(&mut session, tab_ids[2], 2);

    assert!(emitted_events.is_empty());
    for (tab_index, tab_id) in tab_ids.iter().enumerate() {
        assert_eq!(session.tabs[tab_id].get_tab_index(), tab_index);
    }
}

#[test]
fn moving_an_unknown_tab_is_a_noop() {
    let (mut session, tab_ids) = build_four_tab_session();

    let emitted_events = move_tab(&mut session, TabId::new(), 0);

    assert!(emitted_events.is_empty());
    assert_eq!(session.tabs.len(), 4);
    for (tab_index, tab_id) in tab_ids.iter().enumerate() {
        assert_eq!(session.tabs[tab_id].get_tab_index(), tab_index);
    }
}

#[test]
fn move_tab_in_a_single_tab_session_is_a_noop() {
    // The clamp reads `len - 1`; with one tab that is 0, and every requested
    // index lands back on the slot the tab already holds.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, pane_id, 0)],
        vec![pane_id],
    );

    assert!(move_tab(&mut session, tab_id, 0).is_empty());
    assert!(move_tab(&mut session, tab_id, 7).is_empty());
    assert_eq!(session.tabs[&tab_id].get_tab_index(), 0);
}

#[test]
fn move_tab_with_only_two_tabs_swaps_them() {
    // Two tabs is the smallest count `move_tab` renumbers: the one other tab
    // takes the freed index.
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(second_tab_id, second_pane_id, 1),
        ],
        vec![first_pane_id, second_pane_id],
    );

    let emitted_events = move_tab(&mut session, first_tab_id, 1);

    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 0);
    assert_eq!(session.tabs[&first_tab_id].get_tab_index(), 1);
    assert_eq!(
        emitted_events,
        vec![Event::TabMoved(TabMoved {
            tab_id: first_tab_id,
            previous_tab_index: 0,
            new_tab_index: 1,
        })]
    );
}

#[test]
fn move_tab_keeps_the_session_consistent_and_indices_dense() {
    // Reordering must leave the registry contract intact: after a move the
    // indices are still a dense 0..len with no duplicate, and an attached
    // client viewing a moved tab still resolves — `validate` finds nothing.
    let (mut session, tab_ids) = build_four_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[3]); // viewing the tab that moves

    let _ = move_tab(&mut session, tab_ids[3], 0); // fourth tab to the front

    assert_eq!(session.tabs[&tab_ids[3]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[0]].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_ids[1]].get_tab_index(), 2);
    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 3);
    // The client's active tab is unchanged by a reorder — a move shifts
    // positions, not which tab a client views.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_ids[3]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn move_tab_then_close_tab_keeps_indices_dense() {
    // Two reorders back to back: the close renumbers the survivors from the
    // order the move left, not from the order the fixture built.
    let (mut session, tab_ids) = build_four_tab_session();

    let _ = move_tab(&mut session, tab_ids[3], 0); // fourth tab to index 0
    let _ = close_tab(&mut session, tab_ids[0]); // first tab leaves

    assert_eq!(session.tabs.len(), 3);
    assert_eq!(session.tabs[&tab_ids[3]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[1]].get_tab_index(), 1);
    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 2);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

// --- close_and_refocus_tab / focus_tab edge cases ---------------------------

#[test]
fn closing_an_already_closed_tab_is_a_noop_on_the_second_call() {
    let (surviving_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (surviving_pane_id, closed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(surviving_tab_id, surviving_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
        ],
        vec![surviving_pane_id, closed_pane_id],
    );

    let first_close_events = close_tab(&mut session, closed_tab_id);
    assert_eq!(
        first_close_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
        ]
    );
    assert!(!session.tabs.contains_key(&closed_tab_id));

    // Closing the same, now-unknown, id again must not disturb the survivor.
    let second_close_events = close_tab(&mut session, closed_tab_id);

    assert!(second_close_events.is_empty());
    assert!(session.tabs.contains_key(&surviving_tab_id));
    assert_eq!(session.tabs[&surviving_tab_id].get_tab_index(), 0);
}

#[test]
fn close_tab_moves_every_client_that_was_viewing_it() {
    let (surviving_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (surviving_pane_id, closed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(surviving_tab_id, surviving_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
        ],
        vec![surviving_pane_id, closed_pane_id],
    );
    let first_client_id = attach_client_on(&mut session, closed_tab_id);
    let second_client_id = attach_client_on(&mut session, closed_tab_id);

    let emitted_events = close_tab(&mut session, closed_tab_id);

    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .unwrap()
            .get_active_tab_id(),
        surviving_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .unwrap()
            .get_active_tab_id(),
        surviving_tab_id
    );
    // Clients are walked in id order, so the two refocus pairs follow it.
    let (lower_client_id, higher_client_id) = if first_client_id < second_client_id {
        (first_client_id, second_client_id)
    } else {
        (second_client_id, first_client_id)
    };
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(closed_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: closed_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id: lower_client_id,
                tab_id: surviving_tab_id,
                previous_tab_id: closed_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: lower_client_id,
                tab_id: Some(surviving_tab_id),
                pane_id: surviving_pane_id,
                previous_pane_id: None,
            }),
            Event::TabFocused(TabFocused {
                client_id: higher_client_id,
                tab_id: surviving_tab_id,
                previous_tab_id: closed_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: higher_client_id,
                tab_id: Some(surviving_tab_id),
                pane_id: surviving_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .unwrap()
            .get_focused_pane_id(surviving_tab_id),
        Some(surviving_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .unwrap()
            .get_focused_pane_id(surviving_tab_id),
        Some(surviving_pane_id)
    );
}

#[test]
fn close_tab_preserves_a_clients_prior_focus_on_the_tab_it_lands_on() {
    // The client already held a per-tab focus on `surviving_tab_id` before
    // `closed_tab_id` closed and moved it there; that focus stays.
    let (surviving_tab_id, closed_tab_id) = (TabId::new(), TabId::new());
    let (surviving_pane_id, closed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(surviving_tab_id, surviving_pane_id, 0),
            build_single_pane_tab(closed_tab_id, closed_pane_id, 1),
        ],
        vec![surviving_pane_id, closed_pane_id],
    );
    let mut client = build_client_on(session.session_id, closed_tab_id);
    client.update_focused_pane(surviving_tab_id, surviving_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let _ = close_tab(&mut session, closed_tab_id);

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        surviving_tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(surviving_tab_id),
        Some(surviving_pane_id)
    );
}

#[test]
fn closing_the_last_tab_leaves_a_viewing_clients_active_tab_pointing_at_it() {
    // With no surviving tab to send the client to, `close_and_refocus_tab`
    // leaves `active_tab_id` unchanged: it still names the tab id that was just
    // removed from `session.tabs`. The per-tab focus entry for that tab is
    // still pruned. `validate_session_consistency` checks the active tab only
    // in a session that still has tabs.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, pane_id, 0)],
        vec![pane_id],
    );
    let client_id = attach_client_on(&mut session, tab_id);

    let emitted_events = close_tab(&mut session, tab_id);

    assert!(session.tabs.is_empty());
    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id }),
            Event::PaneRemoved(PaneRemoved {
                pane_id,
                tab_id: Some(tab_id)
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_id
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane_id(tab_id),
        None
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_dangling_active_tab_is_still_reported_while_other_tabs_remain() {
    // The zero-tab scoping must not swallow the real corruption case: a
    // client viewing a gone tab while the session still has tabs is an
    // inconsistency and stays reported.
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![
            build_single_pane_tab(first_tab_id, first_pane_id, 0),
            build_single_pane_tab(second_tab_id, second_pane_id, 1),
        ],
        vec![first_pane_id, second_pane_id],
    );
    let client_id = attach_client_on(&mut session, first_tab_id);
    let missing_tab_id = TabId::new();
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("attached")
        .update_active_tab_id(missing_tab_id);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ActiveTabMissing {
            client_id,
            tab_id: missing_tab_id,
        }])
    );
}

#[test]
fn focus_tab_next_with_a_stale_active_tab_is_a_noop_not_a_panic() {
    // A client whose `active_tab_id` is not in `session.tabs`: stepping Next or
    // Previous changes nothing and does not panic. `resolve_tab_target` finds
    // no index for the stale tab.
    let (mut session, tab_ids) = build_three_tab_session();
    let client_id = attach_client_on(&mut session, tab_ids[0]);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .unwrap()
        .update_active_tab_id(TabId::new()); // now points nowhere

    let next_tab_events = focus_tab(&mut session, client_id, TabTarget::Next);
    let previous_tab_events = focus_tab(&mut session, client_id, TabTarget::Previous);

    assert!(next_tab_events.is_empty());
    assert!(previous_tab_events.is_empty());
}

#[test]
fn focus_next_and_previous_on_a_single_tab_session_is_a_noop() {
    // With exactly one tab, wrapping Next/Previous resolves back to the same
    // tab — the already-active-tab guard in `focus_tab` makes this a no-op.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, pane_id, 0)],
        vec![pane_id],
    );
    let client_id = attach_client_on(&mut session, tab_id);

    let next_tab_events = focus_tab(&mut session, client_id, TabTarget::Next);
    let previous_tab_events = focus_tab(&mut session, client_id, TabTarget::Previous);

    assert!(next_tab_events.is_empty());
    assert!(previous_tab_events.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab_id(),
        tab_id
    );
}

// --- commit_profile_tab -----------------------------------------------------

/// A two-leaf horizontal split of `left_pane_id` and `right_pane_id`, as a
/// profile's tree.
fn build_two_leaf_layout(left_pane_id: PaneId, right_pane_id: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(left_pane_id),
            LayoutNode::Pane(right_pane_id),
        ],
    ))
}

#[test]
fn commit_profile_tab_registers_every_pane_running_and_emits_created_events() {
    let mut session = build_session_with(vec![], vec![]);
    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let layout_tree = build_two_leaf_layout(first_pane_id, second_pane_id);
    let profile_tab = ProfileTab {
        pane_ids: vec![first_pane_id, second_pane_id],
        layout_tree: layout_tree.clone(),
        new_pane_specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focused_leaf_index: 0,
    };

    let emitted_events = commit_profile_tab(
        &mut session,
        tab_id,
        profile_tab,
        "dev".to_owned(),
        None,
        true,
    );

    // The first tab moves the session from Starting to Running.
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
    // Every pane in the profile is registered and live (each child was already
    // spawned before the commit).
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(first_pane_id)
            .unwrap()
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(
        *session
            .panes
            .get_pane_record_by_id(second_pane_id)
            .unwrap()
            .get_lifecycle(),
        PaneLifecycle::Running
    );
    assert_eq!(session.panes.count_pane_records(), 2);
    // The tab carries the whole profile tree, not just its single root leaf.
    assert_eq!(*session.tabs[&tab_id].get_layout_tree(), layout_tree);
    assert_eq!(session.tabs[&tab_id].get_tab_index(), 0);

    // No focus client, so only creation events: one TabCreated then one
    // PaneCreated per pane, in layout order.
    match emitted_events.as_slice() {
        [Event::TabCreated(created_tab), Event::PaneCreated(first_created_pane), Event::PaneCreated(second_created_pane)] =>
        {
            assert_eq!(created_tab.tab_id, tab_id);
            assert_eq!(first_created_pane.pane_id, first_pane_id);
            assert_eq!(first_created_pane.tab_id, Some(tab_id));
            assert_eq!(second_created_pane.pane_id, second_pane_id);
            assert_eq!(second_created_pane.tab_id, Some(tab_id));
        }
        unexpected_events => panic!("unexpected events: {unexpected_events:?}"),
    }
}

#[test]
fn commit_profile_tab_without_a_client_still_records_the_focus_leaf() {
    // A profile committed with no client — a session started detached — must
    // still put its chosen leaf in the tab's focus history, so the first
    // client to view the tab lands on it, not on layout order.
    let mut session = build_session_with(vec![], vec![]);
    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let profile_tab = ProfileTab {
        pane_ids: vec![first_pane_id, second_pane_id],
        layout_tree: build_two_leaf_layout(first_pane_id, second_pane_id),
        new_pane_specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focused_leaf_index: 1,
    };

    let _ = commit_profile_tab(
        &mut session,
        tab_id,
        profile_tab,
        "dev".to_owned(),
        None,
        true,
    );

    assert_eq!(session.tabs[&tab_id].list_focus_mru(), &[second_pane_id]);
}

#[test]
fn commit_profile_tab_focuses_the_focus_leaf_and_switches_the_client() {
    let mut session = build_session_with(vec![], vec![]);
    let start_tab_id = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        start_tab_id,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
    );
    let client_id = attach_client_on(&mut session, start_tab_id);

    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let profile_tab = ProfileTab {
        pane_ids: vec![first_pane_id, second_pane_id],
        layout_tree: build_two_leaf_layout(first_pane_id, second_pane_id),
        new_pane_specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focused_leaf_index: 1, // focus the second leaf, not the root
    };

    let emitted_events = commit_profile_tab(
        &mut session,
        tab_id,
        profile_tab,
        "dev".to_owned(),
        Some(client_id),
        true,
    );

    let client = session.clients.get_client_by_id(client_id).unwrap();
    // Active profile tab: the client switches onto it and focuses the chosen leaf.
    assert_eq!(client.get_active_tab_id(), tab_id);
    assert_eq!(client.get_focused_pane_id(tab_id), Some(second_pane_id));
    assert_eq!(session.tabs[&tab_id].list_focus_mru(), &[second_pane_id]);

    // TabCreated, one PaneCreated per pane, then the focus pair naming the leaf.
    assert_eq!(
        emitted_events,
        vec![
            Event::TabCreated(TabCreated { tab_id }),
            Event::PaneCreated(PaneCreated {
                pane_id: first_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: second_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id,
                previous_tab_id: start_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: second_pane_id,
                previous_pane_id: None,
            }),
        ]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_profile_tab_out_of_range_focus_leaf_focuses_the_root_pane() {
    // `focused_leaf_index` past the last leaf falls back to the root pane (index 0),
    // never panics and never focuses a pane the profile does not hold.
    let mut session = build_session_with(vec![], vec![]);
    let start_tab_id = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        start_tab_id,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
    );
    let client_id = attach_client_on(&mut session, start_tab_id);

    let tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let profile_tab = ProfileTab {
        pane_ids: vec![first_pane_id, second_pane_id],
        layout_tree: build_two_leaf_layout(first_pane_id, second_pane_id),
        new_pane_specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focused_leaf_index: 9, // out of range
    };

    let _ = commit_profile_tab(
        &mut session,
        tab_id,
        profile_tab,
        "dev".to_owned(),
        Some(client_id),
        true,
    );

    let client = session.clients.get_client_by_id(client_id).unwrap();
    assert_eq!(client.get_focused_pane_id(tab_id), Some(first_pane_id));
    assert_eq!(session.tabs[&tab_id].list_focus_mru(), &[first_pane_id]);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_profile_tab_inactive_records_focus_without_switching_the_view() {
    // An inactive profile tab records the client's starting pane, leaves the
    // client's view where it was, and emits no focus events.
    let mut session = build_session_with(vec![], vec![]);
    let first_tab_id = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        first_tab_id,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
    );
    let client_id = attach_client_on(&mut session, first_tab_id);

    let second_tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let profile_tab = ProfileTab {
        pane_ids: vec![first_pane_id, second_pane_id],
        layout_tree: build_two_leaf_layout(first_pane_id, second_pane_id),
        new_pane_specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focused_leaf_index: 0,
    };

    let emitted_events = commit_profile_tab(
        &mut session,
        second_tab_id,
        profile_tab,
        "dev".to_owned(),
        Some(client_id),
        false,
    );

    let client = session.clients.get_client_by_id(client_id).unwrap();
    // The view stays on the original tab: an inactive tab does not switch it.
    assert_eq!(client.get_active_tab_id(), first_tab_id);
    // But the starting pane is recorded on both the client and the tab history.
    assert_eq!(
        client.get_focused_pane_id(second_tab_id),
        Some(first_pane_id)
    );
    assert_eq!(
        session.tabs[&second_tab_id].list_focus_mru(),
        &[first_pane_id]
    );
    // No TabFocused/PaneFocused while inactive — only the creation events.
    assert_eq!(
        emitted_events,
        vec![
            Event::TabCreated(TabCreated {
                tab_id: second_tab_id,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: first_pane_id,
                tab_id: Some(second_tab_id),
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: second_pane_id,
                tab_id: Some(second_tab_id),
            }),
        ]
    );
    assert_eq!(session.tabs[&second_tab_id].get_tab_index(), 1); // appended after the first tab
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn commit_profile_tab_with_a_stale_focus_client_emits_only_creation_events() {
    // An id no client holds records no focus and moves no view, exactly like
    // `None` — but the tab still records its own starting pane.
    let mut session = build_session_with(vec![], vec![]);
    let first_tab_id = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        first_tab_id,
        PaneId::new(),
        "code".to_owned(),
        None,
        NewPaneSpec::default(),
    );
    let attached_client_id = attach_client_on(&mut session, first_tab_id);

    let second_tab_id = TabId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let profile_tab = ProfileTab {
        pane_ids: vec![first_pane_id, second_pane_id],
        layout_tree: build_two_leaf_layout(first_pane_id, second_pane_id),
        new_pane_specs: vec![NewPaneSpec::default(), NewPaneSpec::default()],
        focused_leaf_index: 1,
    };

    let emitted_events = commit_profile_tab(
        &mut session,
        second_tab_id,
        profile_tab,
        "dev".to_owned(),
        Some(ClientId::new()),
        true,
    );

    assert_eq!(
        emitted_events,
        vec![
            Event::TabCreated(TabCreated {
                tab_id: second_tab_id,
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: first_pane_id,
                tab_id: Some(second_tab_id),
            }),
            Event::PaneCreated(PaneCreated {
                pane_id: second_pane_id,
                tab_id: Some(second_tab_id),
            }),
        ]
    );
    let client = session
        .clients
        .get_client_by_id(attached_client_id)
        .unwrap();
    assert_eq!(client.get_active_tab_id(), first_tab_id);
    assert_eq!(client.get_focused_pane_id(second_tab_id), None);
    assert_eq!(
        session.tabs[&second_tab_id].list_focus_mru(),
        &[second_pane_id]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
#[should_panic(expected = "index out of bounds")]
fn commit_profile_tab_with_no_panes_panics() {
    // The tab's root pane is `pane_ids[0]`, so an empty `pane_ids` panics
    // there — the panic the function documents.
    let mut session = build_session_with(vec![], vec![]);
    let profile_tab = ProfileTab {
        pane_ids: vec![],
        layout_tree: LayoutNode::Pane(PaneId::new()),
        new_pane_specs: vec![],
        focused_leaf_index: 0,
    };

    let _ = commit_profile_tab(
        &mut session,
        TabId::new(),
        profile_tab,
        "dev".to_owned(),
        None,
        true,
    );
}

#[test]
fn a_new_tab_after_a_close_takes_the_freed_index_densely() {
    // Closing the middle of three tabs renumbers the survivors to 0,1; a tab
    // created next lands at the freed dense slot (2) with no duplicate index,
    // and the session stays consistent.
    let (mut session, tab_ids) = build_three_tab_session(); // a0 b1 c2
    let _ = close_tab(&mut session, tab_ids[1]); // remove the middle → a0 c1
    assert_eq!(session.tabs[&tab_ids[0]].get_tab_index(), 0);
    assert_eq!(session.tabs[&tab_ids[2]].get_tab_index(), 1);

    let new_tab_id = TabId::new();
    let _ = commit_new_tab(
        &mut session,
        new_tab_id,
        PaneId::new(),
        "d".to_owned(),
        None,
        NewPaneSpec::default(),
    );

    assert_eq!(session.tabs[&new_tab_id].get_tab_index(), 2);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

/// Register `pane_id` and add it to `session`'s floating set, asking for the
/// default size, solved to `48x13`.
fn add_floating_pane(session: &mut Session, pane_id: PaneId) {
    let _ = session
        .panes
        .register_pane_record(build_pane_record(pane_id));
    session
        .floating_set
        .add_member(build_default_floating_member(pane_id))
        .expect("the floating set has room");
}

#[test]
fn closing_the_last_tab_removes_every_floating_pane_in_creation_order_before_the_quit() {
    let tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let (first_floating_pane_id, second_floating_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_session_with(
        vec![build_single_pane_tab(tab_id, tiled_pane_id, 0)],
        vec![tiled_pane_id],
    );
    add_floating_pane(&mut session, first_floating_pane_id);
    add_floating_pane(&mut session, second_floating_pane_id);

    let emitted_events = close_tab(&mut session, tab_id);

    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: tiled_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: tiled_pane_id,
                tab_id: Some(tab_id),
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::PaneClosing(PaneClosing {
                pane_id: first_floating_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: first_floating_pane_id,
                tab_id: None,
            }),
            Event::PaneClosing(PaneClosing {
                pane_id: second_floating_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: second_floating_pane_id,
                tab_id: None,
            }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
    assert_eq!(session.floating_set.list_members(), []);
    assert_eq!(session.panes.count_pane_records(), 0);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn closing_a_tab_that_is_not_the_last_keeps_every_floating_pane() {
    let (mut session, [first_tab_id, ..]) = build_three_tab_session();
    let floating_pane_id = PaneId::new();
    add_floating_pane(&mut session, floating_pane_id);
    let closed_pane_id = get_only_pane_id(&session, first_tab_id);

    let emitted_events = close_tab(&mut session, first_tab_id);

    assert_eq!(
        emitted_events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: closed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: closed_pane_id,
                tab_id: Some(first_tab_id),
            }),
            Event::TabClosed(TabClosed {
                tab_id: first_tab_id,
            }),
        ]
    );
    assert_eq!(
        session
            .floating_set
            .list_members()
            .iter()
            .map(|floating_member| floating_member.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![floating_pane_id]
    );
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(floating_pane_id)
            .map(PaneRecord::get_pane_id),
        Some(floating_pane_id)
    );
}
