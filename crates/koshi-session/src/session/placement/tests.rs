//! Tests for atomic cross-tab placement commit, undock commit, dock commit
//! refusals and client-view repair.

use super::*;

use std::time::SystemTime;

use koshi_core::command::PanePlacementAnchor;
use koshi_core::constant::MAX_FLOATING_PANES_PER_SESSION;
use koshi_core::event::{
    Event, LayoutChanged, PaneFocused, PaneMinimizedChanged, TabClosed, TabFocused,
};
use koshi_core::geometry::{
    Direction, FloatingPanePosition, PaneArea, Point, Rect, Size, SplitDirection,
    DEFAULT_FLOATING_PANE_SIZE,
};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::{Client, ClientOrigin, FloatingPaneView};
use crate::error::FloatingSetError;
use crate::session::state::tests::{build_default_floating_member, build_empty_session, build_tab};
use crate::session::state::Session;

/// The [`Event::PaneFocused`] events of `emitted_events` that name
/// `client_id`, in emitted order.
fn list_pane_focused_events_of_client(
    emitted_events: &[Event],
    client_id: ClientId,
) -> Vec<&Event> {
    emitted_events
        .iter()
        .filter(|event| {
            matches!(event, Event::PaneFocused(pane_focused) if pane_focused.client_id == client_id)
        })
        .collect()
}

fn register_pane(session: &mut Session, pane_id: PaneId) {
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
        .expect("pane id is unique");
}

fn register_tab(session: &mut Session, tab_id: TabId, pane_ids: &[PaneId]) {
    let root_pane_id = pane_ids[0];
    let mut tab = build_tab(
        tab_id,
        "tab",
        session.tabs.len(),
        LayoutNode::Pane(root_pane_id),
    );
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

/// Add a new floating pane to `session`, minimized for every attached client,
/// and give `client_id`'s input to it. Returns the floating pane's id.
fn focus_new_floating_pane(session: &mut Session, client_id: ClientId) -> PaneId {
    let floating_pane_id = PaneId::new();
    register_pane(session, floating_pane_id);
    session
        .add_floating_member(build_default_floating_member(floating_pane_id), &[])
        .expect("the floating set has room");
    assert!(session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client is attached")
        .focus_floating_pane(floating_pane_id));
    floating_pane_id
}

/// A session whose tab `tab_id` holds `staying_pane_id | undocked_pane_id`,
/// with `client_id` attached to the tab and focused on `undocked_pane_id`.
fn build_undock_session(
    tab_id: TabId,
    staying_pane_id: PaneId,
    undocked_pane_id: PaneId,
    client_id: ClientId,
) -> Session {
    let mut session = build_empty_session();
    register_pane(&mut session, staying_pane_id);
    register_pane(&mut session, undocked_pane_id);
    register_tab(&mut session, tab_id, &[staying_pane_id, undocked_pane_id]);
    let _ = session.update_tab_layout(
        tab_id,
        build_horizontal_split(staying_pane_id, undocked_pane_id),
    );
    attach_client(&mut session, client_id, tab_id, undocked_pane_id);
    session
}

#[test]
fn commit_rejects_equal_source_and_destination_tabs_without_mutation() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();
    let mut session = build_empty_session();
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
    let mut session = build_empty_session();
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
    let mut session = build_empty_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, surviving_source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, surviving_source_pane_id],
    );
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(source_pane_id, surviving_source_pane_id),
    );
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
    let mut session = build_empty_session();
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
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(source_pane_id, surviving_source_pane_id),
    );
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
    // The viewer types in a floating pane: its input is not on the moved
    // pane, so it stays in the source tab.
    let _ = focus_new_floating_pane(&mut session, viewer_client_id);

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
    let mut session = build_empty_session();
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
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(source_pane_id, surviving_source_pane_id),
    );
    register_tab(
        &mut session,
        destination_tab_id,
        &[swapped_destination_pane_id, surviving_destination_pane_id],
    );
    let _ = session.update_tab_layout(
        destination_tab_id,
        build_horizontal_split(swapped_destination_pane_id, surviving_destination_pane_id),
    );
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
    let mut session = build_empty_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, surviving_source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, surviving_source_pane_id],
    );
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(source_pane_id, surviving_source_pane_id),
    );
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
    let mut session = build_empty_session();
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
    let mut session = build_empty_session();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, remaining_source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, remaining_source_pane_id],
    );
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(source_pane_id, remaining_source_pane_id),
    );
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

#[test]
fn the_placed_pane_takes_over_the_acting_clients_input_from_a_focused_floating_pane() {
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let (staying_pane_id, placed_pane_id, destination_pane_id, floating_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
    for pane_id in [
        staying_pane_id,
        placed_pane_id,
        destination_pane_id,
        floating_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[staying_pane_id]);
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(staying_pane_id, placed_pane_id),
    );
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
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id,
                pane_id: floating_pane_id,
                is_minimized: true,
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
    assert_eq!(client.list_shown_floating_pane_ids(), &[] as &[PaneId]);
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
        Some(FloatingPaneView {
            position: FloatingPanePosition::Moved(top_left_cell),
            is_minimized: false,
        })
    );
    assert_eq!(client.list_shown_floating_pane_ids(), &[undocked_pane_id]);
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(undocked_pane_id)
    );
    assert_eq!(client.get_focused_pane_id(tab_id), Some(staying_pane_id));
    assert_eq!(client.get_zoomed_pane_id(tab_id), None);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undock_moves_a_background_clients_tab_focus_and_gives_it_a_minimized_view() {
    // The background client remembers the undocked pane in the tab, and its
    // input is on a floating pane: it does not follow the undocked pane.
    let tab_id = TabId::new();
    let (staying_pane_id, undocked_pane_id) = (PaneId::new(), PaneId::new());
    let (acting_client_id, background_client_id) = (ClientId::new(), ClientId::new());
    let mut session =
        build_undock_session(tab_id, staying_pane_id, undocked_pane_id, acting_client_id);
    attach_client(&mut session, background_client_id, tab_id, undocked_pane_id);
    let background_floating_pane_id = focus_new_floating_pane(&mut session, background_client_id);

    let emitted_events = commit_undock_placement(
        &mut session,
        tab_id,
        build_default_floating_member(undocked_pane_id),
        (acting_client_id, FloatingPanePosition::Default),
    )
    .expect("undock commits");

    let background_focus_events =
        list_pane_focused_events_of_client(&emitted_events, background_client_id);
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
    assert_eq!(
        background_client.get_focused_floating_pane_id(),
        Some(background_floating_pane_id)
    );
    assert_eq!(
        background_client.list_shown_floating_pane_ids(),
        &[background_floating_pane_id]
    );
    assert_eq!(
        background_client.get_floating_pane_view(undocked_pane_id),
        Some(FloatingPaneView {
            position: FloatingPanePosition::Default,
            is_minimized: true,
        })
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undock_shows_the_pane_for_a_client_whose_input_is_on_it_at_the_spot_it_held_there() {
    // Both clients view `staying | undocked` with their input on `undocked`.
    // The other client follows the pane: it shows the floating pane on top,
    // at the top-left cell the pane held in its own view, with its input on
    // it.
    let tab_id = TabId::new();
    let (staying_pane_id, undocked_pane_id) = (PaneId::new(), PaneId::new());
    let (acting_client_id, following_client_id) = (ClientId::new(), ClientId::new());
    let mut session =
        build_undock_session(tab_id, staying_pane_id, undocked_pane_id, acting_client_id);
    attach_client(&mut session, following_client_id, tab_id, undocked_pane_id);
    let (_, following_top_left_cell) =
        find_lifted_floating_pane_rect(&session, following_client_id, tab_id, undocked_pane_id)
            .expect("the following client draws the pane");

    let emitted_events = commit_undock_placement(
        &mut session,
        tab_id,
        build_default_floating_member(undocked_pane_id),
        (acting_client_id, FloatingPanePosition::Default),
    )
    .expect("undock commits");

    let following_focus_events =
        list_pane_focused_events_of_client(&emitted_events, following_client_id);
    assert_eq!(
        following_focus_events,
        vec![
            &Event::PaneFocused(PaneFocused {
                client_id: following_client_id,
                tab_id: Some(tab_id),
                pane_id: staying_pane_id,
                previous_pane_id: Some(undocked_pane_id),
            }),
            &Event::PaneFocused(PaneFocused {
                client_id: following_client_id,
                tab_id: None,
                pane_id: undocked_pane_id,
                previous_pane_id: Some(staying_pane_id),
            }),
        ]
    );
    let following_client = session
        .clients
        .get_client_by_id(following_client_id)
        .expect("following client");
    assert_eq!(
        following_client.get_floating_pane_view(undocked_pane_id),
        Some(FloatingPaneView {
            position: FloatingPanePosition::Moved(following_top_left_cell),
            is_minimized: false,
        })
    );
    assert_eq!(
        following_client.get_focused_floating_pane_id(),
        Some(undocked_pane_id)
    );
    assert_eq!(
        following_client.get_focused_pane_id(tab_id),
        Some(staying_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_stack_member_drawn_on_its_header_lifts_at_the_rect_it_opens_into() {
    // An `80x22` tab that is the stack `[open, collapsed]`, viewed by a client
    // whose input is on `open`: `collapsed` is drawn on its header on row 21.
    let tab_id = TabId::new();
    let (open_member_pane_id, collapsed_member_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
    for pane_id in [open_member_pane_id, collapsed_member_pane_id] {
        register_pane(&mut session, pane_id);
    }
    register_tab(
        &mut session,
        tab_id,
        &[open_member_pane_id, collapsed_member_pane_id],
    );
    let _ = session.update_tab_layout(
        tab_id,
        LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![open_member_pane_id, collapsed_member_pane_id],
            0,
        )),
    );
    attach_client(&mut session, client_id, tab_id, open_member_pane_id);
    let build_cells_size = |column_count: u16, row_count: u16| FloatingPaneSize {
        width: FloatingPaneDimension::Cells(NonZeroU16::new(column_count).expect("nonzero")),
        height: FloatingPaneDimension::Cells(NonZeroU16::new(row_count).expect("nonzero")),
    };

    // Opened, `collapsed` takes rows 1 to 21, below the header of `open`.
    assert_eq!(
        find_lifted_floating_pane_rect(&session, client_id, tab_id, collapsed_member_pane_id),
        Some((build_cells_size(80, 21), Point { column: 0, row: 1 }))
    );
    assert_eq!(
        find_lifted_floating_pane_rect(&session, client_id, tab_id, open_member_pane_id),
        Some((build_cells_size(80, 21), Point { column: 0, row: 0 }))
    );
}

#[test]
fn undocking_the_only_pane_of_a_tab_closes_the_tab_and_moves_its_viewer() {
    let (undock_tab_id, landing_tab_id) = (TabId::new(), TabId::new());
    let (undocked_pane_id, landing_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
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

    let mut last_pane_session = build_empty_session();
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
            ),
            Err(expected_error)
        );
        assert_eq!(
            serde_json::to_string(&session).expect("session serializes"),
            serialized_session_before
        );
    }
}

/// A session whose tab `tab_id` holds `tiled_pane_id`, with `client_id`
/// attached to the tab and showing and focusing the floating pane
/// `floating_pane_id`.
fn build_dock_session(
    tab_id: TabId,
    tiled_pane_id: PaneId,
    floating_pane_id: PaneId,
    client_id: ClientId,
) -> Session {
    let mut session = build_empty_session();
    register_pane(&mut session, tiled_pane_id);
    register_pane(&mut session, floating_pane_id);
    register_tab(&mut session, tab_id, &[tiled_pane_id]);
    attach_client(&mut session, client_id, tab_id, tiled_pane_id);
    session
        .add_floating_member(build_default_floating_member(floating_pane_id), &[])
        .expect("the floating set has room");
    assert!(session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client is attached")
        .focus_floating_pane(floating_pane_id));
    session
}

#[test]
fn dock_refusals_change_nothing() {
    let tab_id = TabId::new();
    let (tiled_pane_id, floating_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let docked_tree = build_horizontal_split(tiled_pane_id, floating_pane_id);
    let repeating_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(tiled_pane_id),
            LayoutNode::Pane(floating_pane_id),
            LayoutNode::Pane(floating_pane_id),
        ],
    ));
    assert_eq!(
        build_dock_session(tab_id, tiled_pane_id, floating_pane_id, client_id)
            .validate_session_consistency(),
        Ok(())
    );

    let refusal_cases = [
        (
            tiled_pane_id,
            tab_id,
            docked_tree.clone(),
            client_id,
            PlacementCommitError::FloatingSourceNotFound,
        ),
        (
            floating_pane_id,
            TabId::new(),
            docked_tree.clone(),
            client_id,
            PlacementCommitError::DestinationTabNotFound,
        ),
        (
            floating_pane_id,
            tab_id,
            repeating_tree,
            client_id,
            PlacementCommitError::PaneOwnershipConflict,
        ),
        (
            floating_pane_id,
            tab_id,
            LayoutNode::Pane(floating_pane_id),
            client_id,
            PlacementCommitError::PaneOwnershipConflict,
        ),
        (
            floating_pane_id,
            tab_id,
            docked_tree,
            ClientId::new(),
            PlacementCommitError::ActingClientNotFound,
        ),
    ];
    for (source_pane_id, destination_tab_id, destination_tree, acting_client_id, expected_error) in
        refusal_cases
    {
        let mut session = build_dock_session(tab_id, tiled_pane_id, floating_pane_id, client_id);
        let serialized_session_before =
            serde_json::to_string(&session).expect("session serializes");

        assert_eq!(
            commit_dock_placement(
                &mut session,
                source_pane_id,
                destination_tab_id,
                destination_tree,
                acting_client_id,
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
fn docking_into_another_tab_solves_the_tab_the_acting_client_left_at_its_other_viewers_size() {
    let (source_tab_id, destination_tab_id) = (TabId::new(), TabId::new());
    let (source_tab_pane_id, destination_tab_pane_id, floating_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let (acting_client_id, viewer_client_id) = (ClientId::new(), ClientId::new());
    let mut session = build_empty_session();
    for pane_id in [
        source_tab_pane_id,
        destination_tab_pane_id,
        floating_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[source_tab_pane_id]);
    register_tab(&mut session, destination_tab_id, &[destination_tab_pane_id]);
    // The acting client's 80x10 pane area is the smaller of the two viewers
    // of the source tab. The viewer reports none: 80x22.
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
            row_count: 10,
        })),
        source_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    acting_client.update_focused_pane(source_tab_id, source_tab_pane_id);
    session.attach_client(acting_client);
    attach_client(
        &mut session,
        viewer_client_id,
        source_tab_id,
        source_tab_pane_id,
    );
    session
        .add_floating_member(build_default_floating_member(floating_pane_id), &[])
        .expect("the floating set has room");
    assert!(session
        .clients
        .get_client_mut_by_id(acting_client_id)
        .expect("the acting client is attached")
        .focus_floating_pane(floating_pane_id));
    assert_eq!(session.update_tab_solve(source_tab_id), Vec::<Event>::new());
    assert_eq!(
        session.tabs[&source_tab_id].get_solved_tab_size(),
        Size {
            column_count: 80,
            row_count: 10,
        }
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));

    let emitted_events = commit_dock_placement(
        &mut session,
        floating_pane_id,
        destination_tab_id,
        build_horizontal_split(destination_tab_pane_id, floating_pane_id),
        acting_client_id,
    )
    .expect("dock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged {
                tab_id: destination_tab_id,
            }),
            Event::TabFocused(TabFocused {
                client_id: acting_client_id,
                tab_id: destination_tab_id,
                previous_tab_id: source_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: acting_client_id,
                tab_id: Some(destination_tab_id),
                pane_id: floating_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    assert_eq!(
        [source_tab_id, destination_tab_id]
            .map(|tab_id| session.tabs[&tab_id].get_solved_tab_size()),
        [
            Size {
                column_count: 80,
                row_count: 22,
            },
            Size {
                column_count: 80,
                row_count: 10,
            },
        ]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn undocking_a_stack_member_collapses_the_stack_to_its_survivor() {
    let tab_id = TabId::new();
    let (left_pane_id, staying_member_pane_id, undocked_member_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
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
    let _ = session.update_tab_layout(
        tab_id,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(left_pane_id),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![undocked_member_pane_id, staying_member_pane_id],
                    0,
                )),
            ],
        )),
    );
    attach_client(&mut session, client_id, tab_id, undocked_member_pane_id);

    let emitted_events = commit_undock_placement(
        &mut session,
        tab_id,
        build_default_floating_member(undocked_member_pane_id),
        (client_id, FloatingPanePosition::Default),
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
    let mut session = build_empty_session();
    for pane_id in [
        left_pane_id,
        middle_pane_id,
        moved_pane_id,
        destination_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[left_pane_id]);
    let _ = session.update_tab_layout(
        source_tab_id,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(left_pane_id),
                LayoutNode::Pane(middle_pane_id),
                LayoutNode::Pane(moved_pane_id),
            ],
        )),
    );
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

/// The stack `[target_pane_id, docked_pane_id]` with `target_pane_id` active:
/// the tree [`place_floating_pane_in_tab`](koshi_layout::placement::place_floating_pane_in_tab)
/// builds for a dock onto a plain pane.
fn build_stack_dock_tree(target_pane_id: PaneId, docked_pane_id: PaneId) -> LayoutNode {
    LayoutNode::Split(SplitNode::from_stacked_pane_ids(
        vec![target_pane_id, docked_pane_id],
        0,
    ))
}

#[test]
fn docking_onto_the_stack_of_the_focused_pane_gives_the_docked_pane_its_focus_and_input() {
    let tab_id = TabId::new();
    let (tiled_pane_id, floating_pane_id) = (PaneId::new(), PaneId::new());
    let (acting_client_id, viewer_client_id) = (ClientId::new(), ClientId::new());
    let mut session = build_empty_session();
    register_pane(&mut session, tiled_pane_id);
    register_pane(&mut session, floating_pane_id);
    register_tab(&mut session, tab_id, &[tiled_pane_id]);
    attach_client(&mut session, acting_client_id, tab_id, tiled_pane_id);
    attach_client(&mut session, viewer_client_id, tab_id, tiled_pane_id);
    session
        .add_floating_member(build_default_floating_member(floating_pane_id), &[])
        .expect("the floating set has room");
    assert!(session
        .clients
        .get_client_mut_by_id(acting_client_id)
        .expect("the acting client is attached")
        .focus_floating_pane(floating_pane_id));

    let emitted_events = commit_dock_placement(
        &mut session,
        floating_pane_id,
        tab_id,
        build_stack_dock_tree(tiled_pane_id, floating_pane_id),
        acting_client_id,
    )
    .expect("dock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id: acting_client_id,
                tab_id: Some(tab_id),
                pane_id: floating_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    let layout_tree = session.tabs[&tab_id].get_layout_tree();
    let acting_client = session
        .clients
        .get_client_by_id(acting_client_id)
        .expect("the acting client is attached");
    assert_eq!(
        acting_client.get_focused_pane_id(tab_id),
        Some(floating_pane_id)
    );
    assert_eq!(
        acting_client.get_active_focused_pane_id(),
        Some(floating_pane_id)
    );
    assert!(
        !layout_tree.is_pane_collapsed(floating_pane_id, acting_client.list_focus_history(tab_id))
    );
    // The other client keeps its focus on the stack's first member, and its
    // view keeps that member open.
    let viewer_client = session
        .clients
        .get_client_by_id(viewer_client_id)
        .expect("the viewer is attached");
    assert_eq!(
        viewer_client.get_focused_pane_id(tab_id),
        Some(tiled_pane_id)
    );
    assert!(!layout_tree.is_pane_collapsed(tiled_pane_id, viewer_client.list_focus_history(tab_id)));
    assert!(
        layout_tree.is_pane_collapsed(floating_pane_id, viewer_client.list_focus_history(tab_id))
    );
    assert_eq!(session.tabs[&tab_id].list_focus_mru(), [floating_pane_id]);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn docking_onto_the_stack_of_the_typed_in_pane_reports_the_focus_once() {
    let tab_id = TabId::new();
    let (tiled_pane_id, floating_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
    register_pane(&mut session, tiled_pane_id);
    register_pane(&mut session, floating_pane_id);
    register_tab(&mut session, tab_id, &[tiled_pane_id]);
    attach_client(&mut session, client_id, tab_id, tiled_pane_id);
    session
        .add_floating_member(build_default_floating_member(floating_pane_id), &[])
        .expect("the floating set has room");

    let emitted_events = commit_dock_placement(
        &mut session,
        floating_pane_id,
        tab_id,
        build_stack_dock_tree(tiled_pane_id, floating_pane_id),
        client_id,
    )
    .expect("dock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: floating_pane_id,
                previous_pane_id: Some(tiled_pane_id),
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached")
            .get_active_focused_pane_id(),
        Some(floating_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn docking_minimizes_the_acting_clients_shown_floating_panes_and_gives_it_the_docked_pane() {
    let tab_id = TabId::new();
    let (tiled_pane_id, docked_pane_id, shown_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
    for pane_id in [tiled_pane_id, docked_pane_id, shown_pane_id] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, tab_id, &[tiled_pane_id]);
    attach_client(&mut session, client_id, tab_id, tiled_pane_id);
    for floating_pane_id in [docked_pane_id, shown_pane_id] {
        session
            .add_floating_member(build_default_floating_member(floating_pane_id), &[])
            .expect("the floating set has room");
    }
    assert!(session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client is attached")
        .focus_floating_pane(shown_pane_id));

    let emitted_events = commit_dock_placement(
        &mut session,
        docked_pane_id,
        tab_id,
        build_stack_dock_tree(tiled_pane_id, docked_pane_id),
        client_id,
    )
    .expect("dock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id,
                pane_id: shown_pane_id,
                is_minimized: true,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: docked_pane_id,
                previous_pane_id: Some(shown_pane_id),
            }),
        ]
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");
    assert_eq!(client.get_focused_pane_id(tab_id), Some(docked_pane_id));
    assert_eq!(client.get_focused_floating_pane_id(), None);
    assert_eq!(client.list_shown_floating_pane_ids(), &[] as &[PaneId]);
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn docking_into_another_tab_onto_the_stack_of_its_remembered_focus_focuses_the_docked_pane() {
    let (source_tab_id, destination_tab_id) = (TabId::new(), TabId::new());
    let (source_tab_pane_id, destination_tab_pane_id, floating_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_empty_session();
    for pane_id in [
        source_tab_pane_id,
        destination_tab_pane_id,
        floating_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[source_tab_pane_id]);
    register_tab(&mut session, destination_tab_id, &[destination_tab_pane_id]);
    attach_client(&mut session, client_id, source_tab_id, source_tab_pane_id);
    session
        .add_floating_member(build_default_floating_member(floating_pane_id), &[])
        .expect("the floating set has room");
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client is attached");
    client.update_focused_pane(destination_tab_id, destination_tab_pane_id);
    assert!(client.focus_floating_pane(floating_pane_id));

    let emitted_events = commit_dock_placement(
        &mut session,
        floating_pane_id,
        destination_tab_id,
        build_stack_dock_tree(destination_tab_pane_id, floating_pane_id),
        client_id,
    )
    .expect("dock commits");

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
                pane_id: floating_pane_id,
                previous_pane_id: Some(floating_pane_id),
            }),
        ]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached")
            .get_active_focused_pane_id(),
        Some(floating_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn docking_moves_a_client_whose_input_is_on_the_pane_to_its_tab_and_leaves_other_clients_alone() {
    // `htop` floats. The follower types in `htop` from the other tab and also
    // shows `logs`. The staying client views the destination tab and types
    // in `logs`. Docking `htop` next to `vim` moves the follower onto `htop`
    // in the destination tab; the staying client keeps its input and view.
    let (destination_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let (vim_pane_id, shell_pane_id, htop_pane_id, logs_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let (acting_client_id, following_client_id, staying_client_id) =
        (ClientId::new(), ClientId::new(), ClientId::new());
    let mut session = build_empty_session();
    for pane_id in [vim_pane_id, shell_pane_id, htop_pane_id, logs_pane_id] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, destination_tab_id, &[vim_pane_id]);
    register_tab(&mut session, other_tab_id, &[shell_pane_id]);
    attach_client(
        &mut session,
        acting_client_id,
        destination_tab_id,
        vim_pane_id,
    );
    attach_client(
        &mut session,
        following_client_id,
        other_tab_id,
        shell_pane_id,
    );
    attach_client(
        &mut session,
        staying_client_id,
        destination_tab_id,
        vim_pane_id,
    );
    for floating_pane_id in [htop_pane_id, logs_pane_id] {
        session
            .add_floating_member(build_default_floating_member(floating_pane_id), &[])
            .expect("the floating set has room");
    }
    let following_client = session
        .clients
        .get_client_mut_by_id(following_client_id)
        .expect("the following client is attached");
    assert!(following_client.focus_floating_pane(logs_pane_id));
    assert!(following_client.focus_floating_pane(htop_pane_id));
    assert!(session
        .clients
        .get_client_mut_by_id(staying_client_id)
        .expect("the staying client is attached")
        .focus_floating_pane(logs_pane_id));

    let emitted_events = commit_dock_placement(
        &mut session,
        htop_pane_id,
        destination_tab_id,
        build_horizontal_split(vim_pane_id, htop_pane_id),
        acting_client_id,
    )
    .expect("dock commits");

    assert_eq!(
        emitted_events,
        vec![
            Event::LayoutChanged(LayoutChanged {
                tab_id: destination_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: acting_client_id,
                tab_id: Some(destination_tab_id),
                pane_id: htop_pane_id,
                previous_pane_id: Some(vim_pane_id),
            }),
            Event::TabFocused(TabFocused {
                client_id: following_client_id,
                tab_id: destination_tab_id,
                previous_tab_id: other_tab_id,
            }),
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id: following_client_id,
                pane_id: logs_pane_id,
                is_minimized: true,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: following_client_id,
                tab_id: Some(destination_tab_id),
                pane_id: htop_pane_id,
                previous_pane_id: Some(htop_pane_id),
            }),
        ]
    );
    let following_client = session
        .clients
        .get_client_by_id(following_client_id)
        .expect("the following client is attached");
    assert_eq!(following_client.get_active_tab_id(), destination_tab_id);
    assert_eq!(
        following_client.get_focused_pane_id(destination_tab_id),
        Some(htop_pane_id)
    );
    assert_eq!(
        following_client.get_focused_pane_id(other_tab_id),
        Some(shell_pane_id)
    );
    assert_eq!(following_client.get_focused_floating_pane_id(), None);
    assert_eq!(
        following_client.list_shown_floating_pane_ids(),
        &[] as &[PaneId]
    );
    let staying_client = session
        .clients
        .get_client_by_id(staying_client_id)
        .expect("the staying client is attached");
    assert_eq!(staying_client.get_active_tab_id(), destination_tab_id);
    assert_eq!(
        staying_client.get_focused_pane_id(destination_tab_id),
        Some(vim_pane_id)
    );
    assert_eq!(
        staying_client.get_focused_floating_pane_id(),
        Some(logs_pane_id)
    );
    assert_eq!(
        staying_client.list_shown_floating_pane_ids(),
        &[logs_pane_id]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_swap_moves_each_client_whose_input_is_on_a_moved_pane_with_that_pane() {
    // The source tab holds `left | moved` and the destination tab
    // `right | swapped`. The acting client and the source follower type in
    // `moved`; the swap follower types in `swapped`. The swap puts `swapped`
    // where `moved` was. The source clients follow `moved` to the destination
    // tab, the swap follower follows `swapped` to the source tab, and each
    // client's focus in the tab it left goes to the pane now at the spot it
    // left.
    let (source_tab_id, destination_tab_id) = (TabId::new(), TabId::new());
    let (left_pane_id, moved_pane_id, right_pane_id, swapped_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let (acting_client_id, source_following_client_id, swap_following_client_id) =
        (ClientId::new(), ClientId::new(), ClientId::new());
    let mut session = build_empty_session();
    for pane_id in [left_pane_id, moved_pane_id, right_pane_id, swapped_pane_id] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[left_pane_id, moved_pane_id]);
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(left_pane_id, moved_pane_id),
    );
    register_tab(
        &mut session,
        destination_tab_id,
        &[right_pane_id, swapped_pane_id],
    );
    let _ = session.update_tab_layout(
        destination_tab_id,
        build_horizontal_split(right_pane_id, swapped_pane_id),
    );
    attach_client(&mut session, acting_client_id, source_tab_id, moved_pane_id);
    attach_client(
        &mut session,
        source_following_client_id,
        source_tab_id,
        moved_pane_id,
    );
    attach_client(
        &mut session,
        swap_following_client_id,
        destination_tab_id,
        swapped_pane_id,
    );

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        moved_pane_id,
        CrossTabPlacement {
            source_tree: Some(build_horizontal_split(left_pane_id, swapped_pane_id)),
            destination_tree: build_horizontal_split(right_pane_id, moved_pane_id),
        },
        acting_client_id,
    )
    .expect("placement commits");

    let mut source_client_ids = vec![acting_client_id, source_following_client_id];
    source_client_ids.sort();
    let mut expected_events = vec![
        Event::LayoutChanged(LayoutChanged {
            tab_id: destination_tab_id,
        }),
        Event::LayoutChanged(LayoutChanged {
            tab_id: source_tab_id,
        }),
    ];
    for client_id in [acting_client_id, source_following_client_id] {
        expected_events.push(Event::TabFocused(TabFocused {
            client_id,
            tab_id: destination_tab_id,
            previous_tab_id: source_tab_id,
        }));
        expected_events.push(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(destination_tab_id),
            pane_id: moved_pane_id,
            previous_pane_id: None,
        }));
    }
    expected_events.push(Event::TabFocused(TabFocused {
        client_id: swap_following_client_id,
        tab_id: source_tab_id,
        previous_tab_id: destination_tab_id,
    }));
    expected_events.push(Event::PaneFocused(PaneFocused {
        client_id: swap_following_client_id,
        tab_id: Some(source_tab_id),
        pane_id: swapped_pane_id,
        previous_pane_id: None,
    }));
    expected_events.extend(source_client_ids.into_iter().map(|client_id| {
        Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(source_tab_id),
            pane_id: swapped_pane_id,
            previous_pane_id: Some(moved_pane_id),
        })
    }));
    expected_events.push(Event::PaneFocused(PaneFocused {
        client_id: swap_following_client_id,
        tab_id: Some(destination_tab_id),
        pane_id: moved_pane_id,
        previous_pane_id: Some(swapped_pane_id),
    }));
    assert_eq!(emitted_events, expected_events);
    for client_id in [acting_client_id, source_following_client_id] {
        let client = session
            .clients
            .get_client_by_id(client_id)
            .expect("the source client is attached");
        assert_eq!(client.get_active_tab_id(), destination_tab_id);
        assert_eq!(client.get_active_focused_pane_id(), Some(moved_pane_id));
    }
    let swap_following_client = session
        .clients
        .get_client_by_id(swap_following_client_id)
        .expect("the swap follower is attached");
    assert_eq!(swap_following_client.get_active_tab_id(), source_tab_id);
    assert_eq!(
        swap_following_client.get_active_focused_pane_id(),
        Some(swapped_pane_id)
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn list_following_client_ids_lists_every_other_client_whose_input_is_on_the_pane() {
    // On `vim`: the acting client, a client typing in `vim`. Not on `vim`: a
    // client focused on `vim` in the tab but typing in a floating pane, and a
    // client typing in `shell`.
    let tab_id = TabId::new();
    let (vim_pane_id, shell_pane_id) = (PaneId::new(), PaneId::new());
    let [acting_client_id, typing_client_id, floating_client_id, shell_client_id] = [
        ClientId::new(),
        ClientId::new(),
        ClientId::new(),
        ClientId::new(),
    ];
    let mut session = build_empty_session();
    register_pane(&mut session, vim_pane_id);
    register_pane(&mut session, shell_pane_id);
    register_tab(&mut session, tab_id, &[vim_pane_id, shell_pane_id]);
    let _ = session.update_tab_layout(tab_id, build_horizontal_split(vim_pane_id, shell_pane_id));
    for client_id in [acting_client_id, typing_client_id, floating_client_id] {
        attach_client(&mut session, client_id, tab_id, vim_pane_id);
    }
    attach_client(&mut session, shell_client_id, tab_id, shell_pane_id);
    let _ = focus_new_floating_pane(&mut session, floating_client_id);

    assert_eq!(
        list_following_client_ids(&session, vim_pane_id, acting_client_id),
        vec![typing_client_id]
    );
    assert_eq!(
        list_moving_client_ids(&session, vim_pane_id, acting_client_id),
        vec![acting_client_id, typing_client_id]
    );
    let mut following_client_ids = vec![acting_client_id, typing_client_id];
    following_client_ids.sort();
    let mut moving_client_ids = vec![shell_client_id];
    moving_client_ids.extend(following_client_ids);
    assert_eq!(
        list_moving_client_ids(&session, vim_pane_id, shell_client_id),
        moving_client_ids,
        "the acting client comes first, then the followers in client id order"
    );
}

/// Attach `client_id` viewing `tab_id`, focused on `pane_id`, with a pane
/// area and a viewport of `pane_area_size`.
fn attach_client_with_pane_area(
    session: &mut Session,
    client_id: ClientId,
    tab_id: TabId,
    pane_id: PaneId,
    pane_area_size: Size,
) {
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        pane_area_size,
        Some(PaneArea::Reported(pane_area_size)),
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
}

#[test]
fn tab_size_after_clients_move_counts_arriving_clients_and_staying_viewers_only() {
    // The tab is viewed by an `80x22` client that leaves and a `100x30`
    // client that stays. A `90x20` client arrives from the landing tab, where
    // a `60x10` client stays.
    let (tab_id, landing_tab_id) = (TabId::new(), TabId::new());
    let (tab_pane_id, landing_pane_id) = (PaneId::new(), PaneId::new());
    let [leaving_client_id, staying_client_id, arriving_client_id, landing_viewer_client_id] = [
        ClientId::new(),
        ClientId::new(),
        ClientId::new(),
        ClientId::new(),
    ];
    let mut session = build_empty_session();
    register_pane(&mut session, tab_pane_id);
    register_pane(&mut session, landing_pane_id);
    register_tab(&mut session, tab_id, &[tab_pane_id]);
    register_tab(&mut session, landing_tab_id, &[landing_pane_id]);
    for (client_id, client_tab_id, pane_id, column_count, row_count) in [
        (leaving_client_id, tab_id, tab_pane_id, 80, 22),
        (staying_client_id, tab_id, tab_pane_id, 100, 30),
        (arriving_client_id, landing_tab_id, landing_pane_id, 90, 20),
        (
            landing_viewer_client_id,
            landing_tab_id,
            landing_pane_id,
            60,
            10,
        ),
    ] {
        attach_client_with_pane_area(
            &mut session,
            client_id,
            client_tab_id,
            pane_id,
            Size {
                column_count,
                row_count,
            },
        );
    }

    assert_eq!(
        compute_tab_size_after_clients_move(
            &session,
            tab_id,
            None,
            &[arriving_client_id],
            &[leaving_client_id],
        ),
        Some(Size {
            column_count: 90,
            row_count: 20,
        })
    );
    assert_eq!(
        compute_tab_size_after_clients_move(
            &session,
            tab_id,
            Some(landing_tab_id),
            &[arriving_client_id],
            &[leaving_client_id],
        ),
        Some(Size {
            column_count: 60,
            row_count: 10,
        }),
        "a client of the landing tab counts"
    );
    assert_eq!(
        compute_tab_size_after_clients_move(
            &session,
            tab_id,
            None,
            &[],
            &[leaving_client_id, staying_client_id],
        ),
        None,
        "no client views the tab after the move"
    );
}

#[test]
fn a_client_moved_by_a_swap_is_not_repaired_in_the_tab_it_moves_into() {
    // The source tab holds `left | moved`, the destination tab `right |
    // swapped`. The acting client types in `moved` and remembers `swapped`
    // in the destination tab. The swap follower types in `swapped` and
    // remembers `moved` in the source tab. Each one moves into the tab where
    // it remembered the departed pane, focuses the pane it follows there, and
    // gets no second focus change in that tab.
    let (source_tab_id, destination_tab_id) = (TabId::new(), TabId::new());
    let (left_pane_id, moved_pane_id, right_pane_id, swapped_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let (acting_client_id, swap_following_client_id) = (ClientId::new(), ClientId::new());
    let mut session = build_empty_session();
    for pane_id in [left_pane_id, moved_pane_id, right_pane_id, swapped_pane_id] {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, source_tab_id, &[left_pane_id, moved_pane_id]);
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(left_pane_id, moved_pane_id),
    );
    register_tab(
        &mut session,
        destination_tab_id,
        &[right_pane_id, swapped_pane_id],
    );
    let _ = session.update_tab_layout(
        destination_tab_id,
        build_horizontal_split(right_pane_id, swapped_pane_id),
    );
    attach_client(&mut session, acting_client_id, source_tab_id, moved_pane_id);
    let _ = session
        .clients
        .get_client_mut_by_id(acting_client_id)
        .expect("the acting client is attached")
        .update_focused_pane(destination_tab_id, swapped_pane_id);
    attach_client(
        &mut session,
        swap_following_client_id,
        destination_tab_id,
        swapped_pane_id,
    );
    let _ = session
        .clients
        .get_client_mut_by_id(swap_following_client_id)
        .expect("the swap follower is attached")
        .update_focused_pane(source_tab_id, moved_pane_id);

    let emitted_events = commit_cross_tab_placement(
        &mut session,
        source_tab_id,
        destination_tab_id,
        moved_pane_id,
        CrossTabPlacement {
            source_tree: Some(build_horizontal_split(left_pane_id, swapped_pane_id)),
            destination_tree: build_horizontal_split(right_pane_id, moved_pane_id),
        },
        acting_client_id,
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
                client_id: acting_client_id,
                tab_id: destination_tab_id,
                previous_tab_id: source_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: acting_client_id,
                tab_id: Some(destination_tab_id),
                pane_id: moved_pane_id,
                previous_pane_id: Some(swapped_pane_id),
            }),
            Event::TabFocused(TabFocused {
                client_id: swap_following_client_id,
                tab_id: source_tab_id,
                previous_tab_id: destination_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id: swap_following_client_id,
                tab_id: Some(source_tab_id),
                pane_id: swapped_pane_id,
                previous_pane_id: Some(moved_pane_id),
            }),
            Event::PaneFocused(PaneFocused {
                client_id: acting_client_id,
                tab_id: Some(source_tab_id),
                pane_id: swapped_pane_id,
                previous_pane_id: Some(moved_pane_id),
            }),
            Event::PaneFocused(PaneFocused {
                client_id: swap_following_client_id,
                tab_id: Some(destination_tab_id),
                pane_id: moved_pane_id,
                previous_pane_id: Some(swapped_pane_id),
            }),
        ]
    );
    let acting_client = session
        .clients
        .get_client_by_id(acting_client_id)
        .expect("the acting client is attached");
    assert_eq!(
        acting_client.list_focus_history(destination_tab_id),
        [moved_pane_id]
    );
    let swap_following_client = session
        .clients
        .get_client_by_id(swap_following_client_id)
        .expect("the swap follower is attached");
    assert_eq!(
        swap_following_client.list_focus_history(source_tab_id),
        [swapped_pane_id]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

fn build_size(column_count: u16, row_count: u16) -> Size {
    Size {
        column_count,
        row_count,
    }
}

#[test]
fn placement_tab_sizes_count_every_client_typing_in_the_source_pane() {
    let mut session = build_empty_session();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    let alice_client_id = ClientId::new();
    let bob_client_id = ClientId::new();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(&mut session, source_tab_id, &[source_pane_id]);
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    attach_client_with_pane_area(
        &mut session,
        alice_client_id,
        source_tab_id,
        source_pane_id,
        build_size(100, 30),
    );
    attach_client_with_pane_area(
        &mut session,
        bob_client_id,
        source_tab_id,
        source_pane_id,
        build_size(60, 18),
    );

    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            destination_tab_id,
            None,
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(60, 18)),
            source_tab_size: None,
        },
        "Bob types in the moved pane, so he moves with it; the source tab closes"
    );
    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            destination_tab_id,
            Some(destination_pane_id),
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(60, 18)),
            source_tab_size: None,
        },
        "a swap keeps the source tab, and no client then views it"
    );
}

#[test]
fn placement_tab_sizes_of_a_swap_move_the_clients_typing_in_the_swapped_pane() {
    let mut session = build_empty_session();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let source_neighbor_pane_id = PaneId::new();
    let swapped_pane_id = PaneId::new();
    let destination_neighbor_pane_id = PaneId::new();
    for pane_id in [
        source_pane_id,
        source_neighbor_pane_id,
        swapped_pane_id,
        destination_neighbor_pane_id,
    ] {
        register_pane(&mut session, pane_id);
    }
    register_tab(
        &mut session,
        source_tab_id,
        &[source_pane_id, source_neighbor_pane_id],
    );
    register_tab(
        &mut session,
        destination_tab_id,
        &[swapped_pane_id, destination_neighbor_pane_id],
    );
    let _ = session.update_tab_layout(
        source_tab_id,
        build_horizontal_split(source_pane_id, source_neighbor_pane_id),
    );
    let _ = session.update_tab_layout(
        destination_tab_id,
        build_horizontal_split(swapped_pane_id, destination_neighbor_pane_id),
    );
    let alice_client_id = ClientId::new();
    for (client_id, tab_id, pane_id, pane_area_size) in [
        (
            alice_client_id,
            source_tab_id,
            source_pane_id,
            build_size(100, 30),
        ),
        (
            ClientId::new(),
            source_tab_id,
            source_neighbor_pane_id,
            build_size(90, 28),
        ),
        (
            ClientId::new(),
            destination_tab_id,
            swapped_pane_id,
            build_size(50, 15),
        ),
    ] {
        attach_client_with_pane_area(&mut session, client_id, tab_id, pane_id, pane_area_size);
    }

    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            destination_tab_id,
            None,
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(50, 15)),
            source_tab_size: Some(build_size(90, 28)),
        },
        "a split leaves the swapped pane's client in the destination tab"
    );
    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            destination_tab_id,
            Some(swapped_pane_id),
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(100, 30)),
            source_tab_size: Some(build_size(50, 15)),
        },
        "the client typing in the swapped pane moves to the source tab"
    );
    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            destination_tab_id,
            Some(destination_neighbor_pane_id),
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(50, 15)),
            source_tab_size: Some(build_size(90, 28)),
        },
        "a swap with a pane no client types in moves only the acting client"
    );
}

#[test]
fn placement_tab_sizes_count_a_closing_tabs_viewers_only_where_they_land() {
    let mut session = build_empty_session();
    let previous_tab_id = TabId::new();
    let source_tab_id = TabId::new();
    let next_tab_id = TabId::new();
    let previous_pane_id = PaneId::new();
    let source_pane_id = PaneId::new();
    let next_pane_id = PaneId::new();
    for (tab_id, pane_id) in [
        (previous_tab_id, previous_pane_id),
        (source_tab_id, source_pane_id),
        (next_tab_id, next_pane_id),
    ] {
        register_pane(&mut session, pane_id);
        register_tab(&mut session, tab_id, &[pane_id]);
    }
    let alice_client_id = ClientId::new();
    let erin_client_id = ClientId::new();
    attach_client_with_pane_area(
        &mut session,
        alice_client_id,
        source_tab_id,
        source_pane_id,
        build_size(100, 30),
    );
    attach_client_with_pane_area(
        &mut session,
        erin_client_id,
        source_tab_id,
        source_pane_id,
        build_size(40, 10),
    );
    let _ = focus_new_floating_pane(&mut session, erin_client_id);

    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            previous_tab_id,
            None,
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(40, 10)),
            source_tab_size: None,
        },
        "the source tab closes, and Erin lands on the previous tab"
    );
    assert_eq!(
        compute_placement_tab_sizes(&session, source_pane_id, next_tab_id, None, alice_client_id,),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(100, 30)),
            source_tab_size: None,
        },
        "Erin lands on the previous tab, not on the destination"
    );
    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            source_pane_id,
            previous_tab_id,
            Some(previous_pane_id),
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(100, 30)),
            source_tab_size: Some(build_size(40, 10)),
        },
        "a swap keeps the source tab, so Erin stays there"
    );
}

#[test]
fn placement_tab_sizes_within_one_tab_are_the_tab_size() {
    let mut session = build_empty_session();
    let tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    register_pane(&mut session, source_pane_id);
    register_pane(&mut session, target_pane_id);
    register_tab(&mut session, tab_id, &[source_pane_id, target_pane_id]);
    let _ = session.update_tab_layout(
        tab_id,
        build_horizontal_split(source_pane_id, target_pane_id),
    );
    let alice_client_id = ClientId::new();
    attach_client_with_pane_area(
        &mut session,
        alice_client_id,
        tab_id,
        source_pane_id,
        build_size(100, 30),
    );
    attach_client_with_pane_area(
        &mut session,
        ClientId::new(),
        tab_id,
        target_pane_id,
        build_size(80, 20),
    );

    for swapped_pane_id in [None, Some(target_pane_id)] {
        assert_eq!(
            compute_placement_tab_sizes(
                &session,
                source_pane_id,
                tab_id,
                swapped_pane_id,
                alice_client_id,
            ),
            PlacementTabSizes {
                destination_tab_size: Some(build_size(80, 20)),
                source_tab_size: None,
            }
        );
    }
}

#[test]
fn placement_tab_sizes_of_a_floating_source_count_every_client_typing_in_it() {
    let mut session = build_empty_session();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    let source_tab_pane_id = PaneId::new();
    let destination_pane_id = PaneId::new();
    register_pane(&mut session, source_tab_pane_id);
    register_pane(&mut session, destination_pane_id);
    register_tab(&mut session, source_tab_id, &[source_tab_pane_id]);
    register_tab(&mut session, destination_tab_id, &[destination_pane_id]);
    let alice_client_id = ClientId::new();
    let grace_client_id = ClientId::new();
    attach_client_with_pane_area(
        &mut session,
        alice_client_id,
        source_tab_id,
        source_tab_pane_id,
        build_size(100, 30),
    );
    attach_client_with_pane_area(
        &mut session,
        grace_client_id,
        source_tab_id,
        source_tab_pane_id,
        build_size(70, 20),
    );
    attach_client_with_pane_area(
        &mut session,
        ClientId::new(),
        destination_tab_id,
        destination_pane_id,
        build_size(90, 25),
    );
    let floating_pane_id = focus_new_floating_pane(&mut session, alice_client_id);
    assert!(session
        .clients
        .get_client_mut_by_id(grace_client_id)
        .expect("Grace is attached")
        .focus_floating_pane(floating_pane_id));

    assert_eq!(
        compute_placement_tab_sizes(
            &session,
            floating_pane_id,
            destination_tab_id,
            None,
            alice_client_id,
        ),
        PlacementTabSizes {
            destination_tab_size: Some(build_size(70, 20)),
            source_tab_size: None,
        }
    );
}

/// A session whose tab `tab_id` holds `layout_tree` over `tiled_pane_ids`,
/// viewed by `client_id` with a `pane_area_size` pane area and its input on
/// the first tiled pane, and a floating pane that `client_id` draws at
/// `floating_pane_rect`. Returns the session and the floating pane's id.
fn build_nearest_dock_session(
    tab_id: TabId,
    tiled_pane_ids: &[PaneId],
    layout_tree: LayoutNode,
    client_id: ClientId,
    pane_area_size: Size,
    floating_pane_rect: Rect,
) -> (Session, PaneId) {
    let mut session = build_empty_session();
    for &pane_id in tiled_pane_ids {
        register_pane(&mut session, pane_id);
    }
    register_tab(&mut session, tab_id, tiled_pane_ids);
    let _ = session.update_tab_layout(tab_id, layout_tree);
    attach_client_with_pane_area(
        &mut session,
        client_id,
        tab_id,
        tiled_pane_ids[0],
        pane_area_size,
    );
    let floating_pane_id = PaneId::new();
    register_pane(&mut session, floating_pane_id);
    session
        .add_floating_member(
            FloatingMember {
                pane_id: floating_pane_id,
                desired_size: DEFAULT_FLOATING_PANE_SIZE,
                solved_size: floating_pane_rect.size,
            },
            &[(
                client_id,
                FloatingPanePosition::Moved(floating_pane_rect.origin),
            )],
        )
        .expect("the floating set has room");
    (session, floating_pane_id)
}

/// The rect at `(column, row)` of `column_count` by `row_count` cells.
fn build_rect_at(column: u16, row: u16, column_count: u16, row_count: u16) -> Rect {
    Rect::from_origin_and_size(Point { column, row }, build_size(column_count, row_count))
}

#[test]
fn a_floating_pane_docks_beside_the_pane_under_its_center_on_that_panes_nearest_edge() {
    // Tab `A | B` fills an `80x22` pane area, with `B` at `(40, 0)`. A `16x10`
    // pane at `(62, 6)` has the center cell `(70, 11)`, 9 cells from the
    // right edge of `B`. At `(2, 1)`, its center cell `(10, 6)` is 6 cells
    // from the top edge of `A`.
    let tab_id = TabId::new();
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let expected_insertions = [
        (
            build_rect_at(62, 6, 16, 10),
            PanePlacementAnchor::Pane(right_pane_id),
            Direction::Right,
        ),
        (
            build_rect_at(2, 1, 16, 10),
            PanePlacementAnchor::Pane(left_pane_id),
            Direction::Up,
        ),
    ];
    for (floating_pane_rect, expected_anchor, expected_direction) in expected_insertions {
        let (session, floating_pane_id) = build_nearest_dock_session(
            tab_id,
            &[left_pane_id, right_pane_id],
            build_horizontal_split(left_pane_id, right_pane_id),
            client_id,
            build_size(80, 22),
            floating_pane_rect,
        );
        let client = session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached");

        assert_eq!(
            find_nearest_dock_target(&session, client, floating_pane_id),
            Some(TiledPlacementTarget::Split {
                destination_tab_id: tab_id,
                anchor: expected_anchor,
                direction: expected_direction,
            }),
            "{floating_pane_rect:?}"
        );
    }
}

#[test]
fn a_floating_pane_centered_over_a_stack_docks_beside_the_whole_stack() {
    // Tab `A | [B, C]` fills an `80x22` pane area. The center cell `(70, 11)`
    // lies over the stack, whose members are no insertion span of their own.
    let tab_id = TabId::new();
    let (left_pane_id, open_member_pane_id, collapsed_member_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let (session, floating_pane_id) = build_nearest_dock_session(
        tab_id,
        &[left_pane_id, open_member_pane_id, collapsed_member_pane_id],
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(left_pane_id),
                LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                    vec![open_member_pane_id, collapsed_member_pane_id],
                    0,
                )),
            ],
        )),
        client_id,
        build_size(80, 22),
        build_rect_at(62, 6, 16, 10),
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");

    assert_eq!(
        find_nearest_dock_target(&session, client, floating_pane_id),
        Some(TiledPlacementTarget::Split {
            destination_tab_id: tab_id,
            anchor: PanePlacementAnchor::Group(vec![open_member_pane_id, collapsed_member_pane_id]),
            direction: Direction::Right,
        })
    );
}

#[test]
fn a_center_cell_outside_the_tab_counts_from_the_nearest_cell_of_the_tab() {
    // The `80x22` tab `A | B` is centered at `(10, 4)` in a `100x30` pane
    // area. A `10x6` pane at `(90, 12)` has the center cell `(95, 15)`, which
    // moves to the tab cell `(79, 11)` on the right edge of `B`. At `(0, 12)`,
    // its center cell `(5, 15)` moves to `(0, 11)` on the left edge of `A`.
    // At `(30, 24)`, its center cell `(35, 27)` moves to `(25, 21)` on the
    // bottom edge of `A`.
    let tab_id = TabId::new();
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let expected_insertions = [
        (
            build_rect_at(90, 12, 10, 6),
            PanePlacementAnchor::Pane(right_pane_id),
            Direction::Right,
        ),
        (
            build_rect_at(0, 12, 10, 6),
            PanePlacementAnchor::Pane(left_pane_id),
            Direction::Left,
        ),
        (
            build_rect_at(30, 24, 10, 6),
            PanePlacementAnchor::Pane(left_pane_id),
            Direction::Down,
        ),
    ];
    for (floating_pane_rect, expected_anchor, expected_direction) in expected_insertions {
        let (session, floating_pane_id) = build_nearest_dock_session(
            tab_id,
            &[left_pane_id, right_pane_id],
            build_horizontal_split(left_pane_id, right_pane_id),
            client_id,
            build_size(100, 30),
            floating_pane_rect,
        );
        let client = session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached");
        assert_eq!(
            client.get_pane_area(),
            Some(build_size(100, 30)),
            "the client draws the tab in a larger pane area"
        );
        assert_eq!(
            session
                .tabs
                .get(&tab_id)
                .expect("the tab exists")
                .get_solved_tab_size(),
            build_size(80, 22)
        );

        assert_eq!(
            find_nearest_dock_target(&session, client, floating_pane_id),
            Some(TiledPlacementTarget::Split {
                destination_tab_id: tab_id,
                anchor: expected_anchor,
                direction: expected_direction,
            }),
            "{floating_pane_rect:?}"
        );
    }
}

#[test]
fn a_center_cell_counts_from_the_top_left_cell_of_a_tab_centered_in_a_larger_pane_area() {
    // The `80x22` tab `A | B` is centered at `(10, 4)` in a `100x30` pane
    // area, with `B` at `(40, 0)` of the tab. A `10x6` pane at `(40, 12)` has
    // the center cell `(45, 15)`, the tab cell `(35, 11)`: 4 cells from the
    // right edge of `A`. At `(15, 10)`, its center cell `(20, 13)` is the tab
    // cell `(10, 9)`: 9 cells from the top edge of `A`.
    let tab_id = TabId::new();
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let expected_insertions = [
        (build_rect_at(40, 12, 10, 6), Direction::Right),
        (build_rect_at(15, 10, 10, 6), Direction::Up),
    ];
    for (floating_pane_rect, expected_direction) in expected_insertions {
        let (session, floating_pane_id) = build_nearest_dock_session(
            tab_id,
            &[left_pane_id, right_pane_id],
            build_horizontal_split(left_pane_id, right_pane_id),
            client_id,
            build_size(100, 30),
            floating_pane_rect,
        );
        let client = session
            .clients
            .get_client_by_id(client_id)
            .expect("the client is attached");

        assert_eq!(
            find_nearest_dock_target(&session, client, floating_pane_id),
            Some(TiledPlacementTarget::Split {
                destination_tab_id: tab_id,
                anchor: PanePlacementAnchor::Pane(left_pane_id),
                direction: expected_direction,
            }),
            "{floating_pane_rect:?}"
        );
    }
}

#[test]
fn a_client_that_draws_the_floating_pane_nowhere_has_no_nearest_dock_target() {
    // A client that reports `Starving` has no floating viewport and draws no
    // floating pane.
    let tab_id = TabId::new();
    let (tiled_pane_id, floating_pane_id) = (PaneId::new(), PaneId::new());
    let client_id = ClientId::new();
    let mut session = build_dock_session(tab_id, tiled_pane_id, floating_pane_id, client_id);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client is attached")
        .update_pane_area(Some(PaneArea::Starving));
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");
    assert_eq!(
        session.find_client_floating_pane_rect(client, floating_pane_id),
        None
    );

    assert_eq!(
        find_nearest_dock_target(&session, client, floating_pane_id),
        None
    );
}
