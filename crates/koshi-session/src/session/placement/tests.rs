//! Tests for atomic cross-tab placement commit, undock commit and client-view
//! repair.

use super::*;

use std::time::SystemTime;

use koshi_core::constant::MAX_FLOATING_PANES_PER_SESSION;
use koshi_core::event::{Event, LayoutChanged, PaneFocused, TabClosed, TabFocused};
use koshi_core::geometry::{PaneArea, Point, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::solver::{PaneSizing, MIN_PANE_SIZE};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::{Client, ClientOrigin, ClientRegistry, FloatingPanePosition, FloatingPaneView};
use crate::error::FloatingSetError;
use crate::session::state::tests::build_default_floating_member;
use crate::session::state::{Session, Tab};

const TEST_PANE_SIZING: PaneSizing = PaneSizing {
    minimum_size: MIN_PANE_SIZE,
    gap_cell_count: 0,
};

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
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
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

/// A session whose tab `tab_id` holds `staying_pane_id | undocked_pane_id`,
/// with `client_id` attached to the tab and focused on `undocked_pane_id`.
fn build_undock_session(
    tab_id: TabId,
    staying_pane_id: PaneId,
    undocked_pane_id: PaneId,
    client_id: ClientId,
) -> Session {
    let mut session = build_session();
    register_pane(&mut session, staying_pane_id);
    register_pane(&mut session, undocked_pane_id);
    register_tab(&mut session, tab_id, &[staying_pane_id, undocked_pane_id]);
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(build_horizontal_split(staying_pane_id, undocked_pane_id));
    attach_client(&mut session, client_id, tab_id, undocked_pane_id);
    session
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
            TEST_PANE_SIZING,
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
        TEST_PANE_SIZING,
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
                tab_id: Some(destination_tab_id),
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
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(source_pane_id)
            .map(PaneRecord::get_pane_id),
        Some(source_pane_id)
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("acting client");
    assert_eq!(client.get_active_tab_id(), destination_tab_id);
    assert_eq!(
        client.get_focused_pane_id(destination_tab_id),
        Some(source_pane_id)
    );
    assert_eq!(client.get_zoomed_pane_id(source_tab_id), None);
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
        TEST_PANE_SIZING,
    )
    .expect("placement commits");

    assert_eq!(
        session
            .clients
            .get_client_by_id(background_client_id)
            .expect("background client")
            .get_focused_pane_id(source_tab_id),
        Some(surviving_source_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(background_client_id)
            .expect("background client")
            .get_zoomed_pane_id(source_tab_id),
        None
    );
    assert_eq!(
        emitted_events
            .iter()
            .filter(|event| {
                **event
                    == Event::PaneFocused(PaneFocused {
                        client_id: background_client_id,
                        tab_id: Some(source_tab_id),
                        pane_id: surviving_source_pane_id,
                        previous_pane_id: Some(source_pane_id),
                    })
            })
            .count(),
        1
    );
}

#[test]
fn transfer_ranks_the_source_tabs_panes_at_its_size_after_the_acting_client_leaves() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let surviving_source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let acting_client_id = ClientId::new();
    let viewer_client_id = ClientId::new();
    let mut session = build_session();
    for pane_id in [
        source_pane_id,
        surviving_source_pane_id,
        destination_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
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
    // An 80x2 pane area hides every pane of the tab it views: a pane needs 3
    // rows. The viewer's 80x22 pane area shows every pane.
    let mut acting_client = Client::from_attachment(
        acting_client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 2,
        })),
        source_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    acting_client.update_focused_pane(source_tab_id, source_pane_id);
    session.attach_client(acting_client);
    attach_client(
        &mut session,
        viewer_client_id,
        source_tab_id,
        source_pane_id,
    );

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
        TEST_PANE_SIZING,
    )
    .expect("placement commits");

    let mut repaired_client_ids = vec![acting_client_id, viewer_client_id];
    repaired_client_ids.sort();
    let mut expected_events = vec![
        Event::LayoutChanged(LayoutChanged {
            tab_id: destination_tab_id,
        }),
        Event::LayoutChanged(LayoutChanged {
            tab_id: source_tab_id,
        }),
        Event::TabFocused(TabFocused {
            client_id: acting_client_id,
            tab_id: destination_tab_id,
            previous_tab_id: source_tab_id,
        }),
        Event::PaneFocused(PaneFocused {
            client_id: acting_client_id,
            tab_id: Some(destination_tab_id),
            pane_id: source_pane_id,
            previous_pane_id: None,
        }),
    ];
    expected_events.extend(repaired_client_ids.into_iter().map(|client_id| {
        Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(source_tab_id),
            pane_id: surviving_source_pane_id,
            previous_pane_id: Some(source_pane_id),
        })
    }));
    assert_eq!(
        emitted_events, expected_events,
        "the source tab is ranked at the viewer's 80x22 once the acting client left it"
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(viewer_client_id)
            .expect("viewer client")
            .get_focused_pane_id(source_tab_id),
        Some(surviving_source_pane_id)
    );
}

#[test]
fn transfer_returns_a_client_zoomed_on_the_swapped_destination_pane_to_its_tiled_view() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let surviving_source_pane_id = PaneId::new();
    let swapped_destination_pane_id = PaneId::new();
    let surviving_destination_pane_id = PaneId::new();
    let acting_client_id = ClientId::new();
    let background_client_id = ClientId::new();
    let mut session = build_session();
    for pane_id in [
        source_pane_id,
        surviving_source_pane_id,
        swapped_destination_pane_id,
        surviving_destination_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
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
    register_tab(
        &mut session,
        destination_tab_id,
        &[swapped_destination_pane_id, surviving_destination_pane_id],
    );
    session
        .tabs
        .get_mut(&destination_tab_id)
        .expect("destination tab")
        .update_layout(build_horizontal_split(
            swapped_destination_pane_id,
            surviving_destination_pane_id,
        ));
    attach_client(
        &mut session,
        acting_client_id,
        source_tab_id,
        source_pane_id,
    );
    attach_client(
        &mut session,
        background_client_id,
        destination_tab_id,
        swapped_destination_pane_id,
    );
    session
        .clients
        .get_client_mut_by_id(background_client_id)
        .expect("background client")
        .zoom_pane(destination_tab_id, swapped_destination_pane_id);

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        source_pane_id,
        CrossTabPlacement {
            source_tree: Some(build_horizontal_split(
                swapped_destination_pane_id,
                surviving_source_pane_id,
            )),
            destination_tree: build_horizontal_split(source_pane_id, surviving_destination_pane_id),
        },
        acting_client_id,
        TEST_PANE_SIZING,
    )
    .expect("placement commits");

    let background_client = session
        .clients
        .get_client_by_id(background_client_id)
        .expect("background client");
    assert_eq!(
        background_client.get_zoomed_pane_id(destination_tab_id),
        None
    );
    assert_eq!(
        background_client.get_focused_pane_id(destination_tab_id),
        Some(source_pane_id),
        "the placed pane is the destination tab's most recent focus"
    );
    assert_eq!(
        emitted_events
            .iter()
            .filter(|event| {
                **event
                    == Event::PaneFocused(PaneFocused {
                        client_id: background_client_id,
                        tab_id: Some(destination_tab_id),
                        pane_id: source_pane_id,
                        previous_pane_id: Some(swapped_destination_pane_id),
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
        TEST_PANE_SIZING,
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
                        tab_id: Some(source_tab_id),
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
            TEST_PANE_SIZING,
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
            TEST_PANE_SIZING,
        ),
        Err(PlacementCommitError::PaneOwnershipConflict)
    );
    assert_eq!(
        serde_json::to_string(&session).expect("session serializes"),
        serialized_session_before
    );
}

#[test]
fn the_placed_pane_takes_over_the_acting_clients_input_from_a_focused_floating_pane() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let (staying_pane_id, placed_pane_id, destination_pane_id, floating_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_session();
    for pane_id in [
        staying_pane_id,
        placed_pane_id,
        destination_pane_id,
        floating_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[staying_pane_id]);
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .update_layout(build_horizontal_split(staying_pane_id, placed_pane_id));
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    session
        .floating_set
        .add_member(build_default_floating_member(floating_pane_id))
        .expect("the floating set is empty");
    attach_client(&mut session, client_id, source_tab_id, staying_pane_id);
    assert!(session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("acting client")
        .focus_floating_pane(floating_pane_id));

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        placed_pane_id,
        CrossTabPlacement {
            source_tree: Some(LayoutNode::Pane(staying_pane_id)),
            destination_tree: build_horizontal_split(destination_pane_id, placed_pane_id),
        },
        client_id,
        TEST_PANE_SIZING,
    )
    .expect("placement commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged {
                tab_id: destination_tab_id,
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: source_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: destination_tab_id,
                previous_tab_id: source_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(destination_tab_id),
                pane_id: placed_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("acting client");
    assert_eq!(client.get_focused_floating_pane_id(), None);
    assert_eq!(client.get_active_focused_pane_id(), Some(placed_pane_id));
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undock_floats_the_pane_and_hands_its_tab_focus_to_the_survivor() {
    let tab_id = TabId::new();
    let (staying_pane_id, undocked_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_undock_session(tab_id, staying_pane_id, undocked_pane_id, client_id);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("acting client")
        .zoom_pane(tab_id, undocked_pane_id);
    let top_left_cell = Point { column: 7, row: 3 };

    let emitted_events = commit_undock_placement(
        &mut session,
        tab_id,
        build_default_floating_member(undocked_pane_id),
        (client_id, FloatingPanePosition::Moved(top_left_cell)),
        TEST_PANE_SIZING,
    )
    .expect("undock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: staying_pane_id,
                previous_pane_id: Some(undocked_pane_id),
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: None,
                pane_id: undocked_pane_id,
                previous_pane_id: Some(staying_pane_id),
            }),
        ]
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![staying_pane_id]
    );
    assert_eq!(
        session.floating_set.list_members(),
        &[build_default_floating_member(undocked_pane_id)]
    );
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(undocked_pane_id)
            .map(PaneRecord::get_pane_id),
        Some(undocked_pane_id)
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("acting client");
    assert_eq!(
        client.get_floating_pane_view(undocked_pane_id),
        FloatingPaneView {
            position: FloatingPanePosition::Moved(top_left_cell),
            is_minimized: false,
        }
    );
    assert_eq!(client.list_floating_pane_focus_order(), &[undocked_pane_id]);
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(undocked_pane_id)
    );
    assert_eq!(client.get_focused_pane_id(tab_id), Some(staying_pane_id));
    assert_eq!(client.get_zoomed_pane_id(tab_id), None);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undock_moves_a_background_clients_tab_focus_and_stores_no_view_for_it() {
    let tab_id = TabId::new();
    let (staying_pane_id, undocked_pane_id) = (PaneId::new(), PaneId::new());
    let (acting_client_id, background_client_id) = (ClientId::new(), ClientId::new());
    let mut session =
        build_undock_session(tab_id, staying_pane_id, undocked_pane_id, acting_client_id);
    attach_client(&mut session, background_client_id, tab_id, undocked_pane_id);

    let emitted_events = commit_undock_placement(
        &mut session,
        tab_id,
        build_default_floating_member(undocked_pane_id),
        (acting_client_id, FloatingPanePosition::Default),
        TEST_PANE_SIZING,
    )
    .expect("undock commits");

    let background_focus_events: Vec<&Event> = emitted_events
        .iter()
        .filter(|event| {
            matches!(event, Event::PaneFocused(pane_focused) if pane_focused.client_id == background_client_id)
        })
        .collect();
    assert_eq!(
        background_focus_events,
        vec![&Event::PaneFocused(PaneFocused {
            client_id: background_client_id,
            tab_id: Some(tab_id),
            pane_id: staying_pane_id,
            previous_pane_id: Some(undocked_pane_id),
        })]
    );
    let background_client = session
        .clients
        .get_client_by_id(background_client_id)
        .expect("background client");
    assert_eq!(
        background_client.get_focused_pane_id(tab_id),
        Some(staying_pane_id)
    );
    assert_eq!(background_client.get_focused_floating_pane_id(), None);
    assert_eq!(
        background_client.list_floating_pane_focus_order(),
        &[] as &[PaneId]
    );
    assert!(background_client.list_floating_pane_views().is_empty());
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undocking_the_only_pane_of_a_tab_closes_the_tab_and_moves_its_viewer() {
    let (undock_tab_id, landing_tab_id) = (TabId::new(), TabId::new());
    let (undocked_pane_id, landing_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_session();
    register_pane(&mut session, undocked_pane_id);
    register_pane(&mut session, landing_pane_id);
    register_tab(&mut session, undock_tab_id, &[undocked_pane_id]);
    register_tab(&mut session, landing_tab_id, &[landing_pane_id]);
    attach_client(&mut session, client_id, undock_tab_id, undocked_pane_id);

    let emitted_events = commit_undock_placement(
        &mut session,
        undock_tab_id,
        build_default_floating_member(undocked_pane_id),
        (client_id, FloatingPanePosition::Default),
        TEST_PANE_SIZING,
    )
    .expect("undock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::TabClosed(TabClosed {
                tab_id: undock_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id,
                tab_id: landing_tab_id,
                previous_tab_id: undock_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(landing_tab_id),
                pane_id: landing_pane_id,
                previous_pane_id: None,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: None,
                pane_id: undocked_pane_id,
                previous_pane_id: Some(landing_pane_id),
            }),
        ]
    );
    assert!(!session.tabs.contains_key(&undock_tab_id));
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("acting client");
    assert_eq!(client.get_active_tab_id(), landing_tab_id);
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(undocked_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undock_refusals_change_nothing() {
    let tab_id = TabId::new();
    let (staying_pane_id, undocked_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();

    let mut last_pane_session = build_session();
    register_pane(&mut last_pane_session, undocked_pane_id);
    register_tab(&mut last_pane_session, tab_id, &[undocked_pane_id]);
    attach_client(&mut last_pane_session, client_id, tab_id, undocked_pane_id);

    let mut full_floating_set_session =
        build_undock_session(tab_id, staying_pane_id, undocked_pane_id, client_id);
    for _ in 0..MAX_FLOATING_PANES_PER_SESSION {
        full_floating_set_session
            .floating_set
            .add_member(build_default_floating_member(PaneId::new()))
            .expect("the floating set has room");
    }

    let refusal_cases = [
        (
            build_undock_session(tab_id, staying_pane_id, undocked_pane_id, client_id),
            TabId::new(),
            undocked_pane_id,
            client_id,
            PlacementCommitError::SourceTabNotFound,
        ),
        (
            build_undock_session(tab_id, staying_pane_id, undocked_pane_id, client_id),
            tab_id,
            PaneId::new(),
            client_id,
            PlacementCommitError::SourcePaneNotFound,
        ),
        (
            last_pane_session,
            tab_id,
            undocked_pane_id,
            client_id,
            PlacementCommitError::LastTiledPane,
        ),
        (
            build_undock_session(tab_id, staying_pane_id, undocked_pane_id, client_id),
            tab_id,
            undocked_pane_id,
            ClientId::new(),
            PlacementCommitError::ActingClientNotFound,
        ),
        (
            full_floating_set_session,
            tab_id,
            undocked_pane_id,
            client_id,
            PlacementCommitError::FloatingMemberRefused(FloatingSetError::TooManyPanes),
        ),
    ];
    for (mut session, source_tab_id, source_pane_id, acting_client_id, expected_error) in
        refusal_cases
    {
        let serialized_session_before =
            serde_json::to_string(&session).expect("session serializes");

        assert_eq!(
            commit_undock_placement(
                &mut session,
                source_tab_id,
                build_default_floating_member(source_pane_id),
                (acting_client_id, FloatingPanePosition::Default),
                TEST_PANE_SIZING,
            ),
            Err(expected_error)
        );
        assert_eq!(
            serde_json::to_string(&session).expect("session serializes"),
            serialized_session_before
        );
    }
}

#[test]
fn undocking_a_stack_member_collapses_the_stack_to_its_survivor() {
    let tab_id = TabId::new();
    let (left_pane_id, staying_member_pane_id, undocked_member_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_session();
    for pane_id in [
        left_pane_id,
        staying_member_pane_id,
        undocked_member_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(
        &mut session,
        tab_id,
        &[
            left_pane_id,
            staying_member_pane_id,
            undocked_member_pane_id,
        ],
    );
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(left_pane_id),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![undocked_member_pane_id, staying_member_pane_id],
                    0,
                )),
            ],
        )));
    attach_client(&mut session, client_id, tab_id, undocked_member_pane_id);

    let emitted_events = commit_undock_placement(
        &mut session,
        tab_id,
        build_default_floating_member(undocked_member_pane_id),
        (client_id, FloatingPanePosition::Default),
        TEST_PANE_SIZING,
    )
    .expect("undock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: staying_member_pane_id,
                previous_pane_id: Some(undocked_member_pane_id),
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: None,
                pane_id: undocked_member_pane_id,
                previous_pane_id: Some(staying_member_pane_id),
            }),
        ]
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &build_horizontal_split(left_pane_id, staying_member_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn transfer_gives_a_background_clients_source_focus_to_the_spatial_neighbor() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let (left_pane_id, middle_pane_id, moved_pane_id, destination_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let acting_client_id = ClientId::new();
    let background_client_id = ClientId::new();
    let mut session = build_session();
    for pane_id in [
        left_pane_id,
        middle_pane_id,
        moved_pane_id,
        destination_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[left_pane_id]);
    session
        .tabs
        .get_mut(&source_tab_id)
        .expect("source tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(left_pane_id),
                LayoutNode::Pane(middle_pane_id),
                LayoutNode::Pane(moved_pane_id),
            ],
        )));
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
        .update_focused_pane(source_tab_id, moved_pane_id);

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        moved_pane_id,
        CrossTabPlacement {
            source_tree: Some(build_horizontal_split(left_pane_id, middle_pane_id)),
            destination_tree: build_horizontal_split(destination_pane_id, moved_pane_id),
        },
        acting_client_id,
        TEST_PANE_SIZING,
    )
    .expect("placement commits");

    assert_eq!(
        emitted_events.last(),
        Some(&Event::PaneFocused(PaneFocused {
            client_id: background_client_id,
            tab_id: Some(source_tab_id),
            pane_id: middle_pane_id,
            previous_pane_id: Some(moved_pane_id),
        }))
    );
    assert_eq!(
        session.tabs[&source_tab_id].list_focus_mru(),
        &[middle_pane_id]
    );
}
