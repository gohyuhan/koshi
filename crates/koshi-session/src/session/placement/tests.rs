//! Tests for atomic cross-tab placement commit and client-view repair.

use super::*;

use std::time::SystemTime;

use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabClosed, TabFocused};
use koshi_core::geometry::SplitDirection;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::{Client, ClientOrigin, ClientRegistry};
use crate::session::state::{Session, Tab};

fn build_session() -> Session {
    Session::from_identity_and_client_registry(
        SessionId::new(),
        "session".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

fn register_pane(session: &mut Session, pane_id: PaneId) {
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            pane_id,
            SystemTime::UNIX_EPOCH,
        ))
        .expect("pane id is unique");
}

fn register_tab(session: &mut Session, tab_id: TabId, pane_ids: &[PaneId]) {
    let root_pane_id = pane_ids[0];
    let mut tab = Tab::from_root_pane(tab_id, "tab".to_string(), session.tabs.len(), root_pane_id);
    for &pane_id in pane_ids.iter().skip(1) {
        tab.record_focus_mru(pane_id);
    }
    session.tabs.insert(tab_id, tab);
}

fn attach_client(session: &mut Session, client_id: ClientId, tab_id: TabId, pane_id: PaneId) {
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        koshi_core::geometry::Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
}

fn build_horizontal_split(first_pane_id: PaneId, second_pane_id: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Pane(second_pane_id),
        ],
    ))
}

#[test]
fn commit_rejects_equal_source_and_destination_tabs_without_mutation() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, pane_id);
    register_tab(&mut session, tab_id, &[pane_id]);
    attach_client(&mut session, client_id, tab_id, pane_id);
    let serialized_session_before = serde_json::to_string(&session).expect("session serializes");

    assert_eq!(
        commit_cross_tab_placement(
            &mut session,
            tab_id,
            tab_id,
            pane_id,
            CrossTabPlacement {
                source_tree: None,
                destination_tree: LayoutNode::Pane(pane_id),
            },
            client_id,
        ),
        Err(PlacementCommitError::SameTab)
    );
    assert_eq!(
        serde_json::to_string(&session).expect("session serializes"),
        serialized_session_before
    );
}

#[test]
fn transferring_a_sole_source_pane_closes_only_the_empty_tab() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(&mut session, source_tab_id, &[source_pane_id]);
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    attach_client(&mut session, client_id, source_tab_id, source_pane_id);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("acting client")
        .zoom_pane(source_tab_id, source_pane_id);

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        source_pane_id,
        CrossTabPlacement {
            source_tree: None,
            destination_tree: build_horizontal_split(destination_pane_id, source_pane_id),
        },
        client_id,
    )
    .expect("placement commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged {
                tab_id: destination_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: destination_tab_id,
                previous_tab_id: source_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: destination_tab_id,
                pane_id: source_pane_id,
                previous_pane_id: None,
            }),
            Event::TabClosed(TabClosed {
                tab_id: source_tab_id,
            }),
        ]
    );
    assert!(!session.tabs.contains_key(&source_tab_id));
    assert_eq!(
        session.tabs[&destination_tab_id]
            .get_layout_tree()
            .list_leaf_pane_ids(),
        vec![destination_pane_id, source_pane_id]
    );
    assert!(session
        .panes
        .get_pane_record_by_id(source_pane_id)
        .is_some());
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("acting client");
    assert_eq!(client.get_active_tab(), destination_tab_id);
    assert_eq!(
        client.get_focused_pane(destination_tab_id),
        Some(source_pane_id)
    );
    assert_eq!(client.get_zoomed_pane(source_tab_id), None);
}

#[test]
fn transfer_repairs_a_background_clients_source_focus_and_zoom() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let surviving_source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let acting_client_id = ClientId::new();
    let background_client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, surviving_source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, surviving_source_pane_id],
    );
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .update_layout(build_horizontal_split(
            source_pane_id,
            surviving_source_pane_id,
        ));
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    attach_client(
        &mut session,
        acting_client_id,
        destination_tab_id,
        destination_pane_id,
    );
    attach_client(
        &mut session,
        background_client_id,
        destination_tab_id,
        destination_pane_id,
    );
    let background_client = session
        .clients
        .get_client_mut_by_id(background_client_id)
        .expect("background client");
    background_client.update_focused_pane(source_tab_id, source_pane_id);
    background_client.zoom_pane(source_tab_id, source_pane_id);

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        source_pane_id,
        CrossTabPlacement {
            source_tree: Some(LayoutNode::Pane(surviving_source_pane_id)),
            destination_tree: build_horizontal_split(destination_pane_id, source_pane_id),
        },
        acting_client_id,
    )
    .expect("placement commits");

    assert_eq!(
        session
            .clients
            .get_client_by_id(background_client_id)
            .expect("background client")
            .get_focused_pane(source_tab_id),
        Some(surviving_source_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(background_client_id)
            .expect("background client")
            .get_zoomed_pane(source_tab_id),
        None
    );
    assert_eq!(
        emitted_events
            .iter()
            .filter(|event| {
                **event
                    == Event::PaneFocused(PaneFocused {
                        client_id: background_client_id,
                        tab_id: source_tab_id,
                        pane_id: surviving_source_pane_id,
                        previous_pane_id: Some(source_pane_id),
                    })
            })
            .count(),
        1
    );
}

#[test]
fn transfer_records_layout_fallback_in_tab_focus_history() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let surviving_source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let acting_client_id = ClientId::new();
    let background_client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, surviving_source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, surviving_source_pane_id],
    );
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .update_layout(build_horizontal_split(
            source_pane_id,
            surviving_source_pane_id,
        ));
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .remove_focus_mru(source_pane_id);
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .remove_focus_mru(surviving_source_pane_id);
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    attach_client(
        &mut session,
        acting_client_id,
        destination_tab_id,
        destination_pane_id,
    );
    attach_client(
        &mut session,
        background_client_id,
        destination_tab_id,
        destination_pane_id,
    );
    session
        .clients
        .get_client_mut_by_id(background_client_id)
        .expect("background client")
        .update_focused_pane(source_tab_id, source_pane_id);

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        source_pane_id,
        CrossTabPlacement {
            source_tree: Some(LayoutNode::Pane(surviving_source_pane_id)),
            destination_tree: build_horizontal_split(destination_pane_id, source_pane_id),
        },
        acting_client_id,
    )
    .expect("placement commits");

    assert_eq!(
        session
            .tabs
            .get(&source_tab_id)
            .expect("source tab survives")
            .list_focus_mru(),
        &[surviving_source_pane_id]
    );
    assert_eq!(
        emitted_events
            .iter()
            .filter(|event| {
                **event
                    == Event::PaneFocused(PaneFocused {
                        client_id: background_client_id,
                        tab_id: source_tab_id,
                        pane_id: surviving_source_pane_id,
                        previous_pane_id: Some(source_pane_id),
                    })
            })
            .count(),
        1
    );
}

#[test]
fn commit_rejects_a_prepared_pane_set_that_drops_an_existing_pane() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(&mut session, source_tab_id, &[source_pane_id]);
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    attach_client(&mut session, client_id, source_tab_id, source_pane_id);
    let serialized_session_before = serde_json::to_string(&session).expect("session serializes");

    assert_eq!(
        commit_cross_tab_placement(
            &mut session,
            source_tab_id,
            destination_tab_id,
            source_pane_id,
            CrossTabPlacement {
                source_tree: None,
                destination_tree: LayoutNode::Pane(source_pane_id),
            },
            client_id,
        ),
        Err(PlacementCommitError::PaneOwnershipConflict)
    );
    assert_eq!(
        serde_json::to_string(&session).expect("session serializes"),
        serialized_session_before
    );
}

#[test]
fn commit_rejects_a_prepared_tree_that_moves_more_than_the_source_pane() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let remaining_source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, remaining_source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, remaining_source_pane_id],
    );
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .update_layout(build_horizontal_split(
            source_pane_id,
            remaining_source_pane_id,
        ));
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    attach_client(&mut session, client_id, source_tab_id, source_pane_id);
    let serialized_session_before = serde_json::to_string(&session).expect("session serializes");

    let prepared_destination_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(destination_pane_id),
            LayoutNode::Pane(source_pane_id),
            LayoutNode::Pane(remaining_source_pane_id),
        ],
    ));
    assert_eq!(
        commit_cross_tab_placement(
            &mut session,
            source_tab_id,
            destination_tab_id,
            source_pane_id,
            CrossTabPlacement {
                source_tree: None,
                destination_tree: prepared_destination_tree,
            },
            client_id,
        ),
        Err(PlacementCommitError::PaneOwnershipConflict)
    );
    assert_eq!(
        serde_json::to_string(&session).expect("session serializes"),
        serialized_session_before
    );
}
