//! Tests for the session model in `state.rs`: [`Session`] and [`Tab`]
//! construction, a tab's focus history, the start lock, the lifecycle moves
//! that attaching, detaching and stopping make, the tab size, the shared
//! floating viewport, the tab and floating cell sizes, the floating set and its
//! size solve, the serialized shape, and every invariant
//! [`Session::validate_session_consistency`] checks.

use super::*;
use std::collections::HashMap;
use std::num::NonZeroU16;
use std::time::SystemTime;

use koshi_core::command::{GridPosition, Selection, SelectionKind};
use koshi_core::event::Event;
use koshi_core::geometry::{AxisPercent, PaneArea, Point, SplitDirection};
use koshi_core::lock::LockMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;

use crate::client::{ClientOrigin, FloatingPanePosition, FloatingPaneView};
use crate::session::pane_ops::NewPaneSpec;
use crate::session::tab_ops::{close_tab, commit_new_tab};

/// A local client with id `client_id` in `session_id`, viewing `tab_id` at
/// `viewport_size` and reporting `pane_area`.
fn build_test_client(
    client_id: ClientId,
    session_id: SessionId,
    tab_id: TabId,
    viewport_size: Size,
    pane_area: Option<PaneArea>,
) -> Client {
    Client::from_attachment(
        client_id,
        session_id,
        SystemTime::UNIX_EPOCH,
        viewport_size,
        pane_area,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// Attach a client viewing `tab_id` with the given viewport, reporting no pane
/// area, and return the id it attached under.
fn attach_viewer(
    session: &mut Session,
    tab_id: TabId,
    column_count: u16,
    row_count: u16,
) -> ClientId {
    attach_viewer_with_pane_area(session, tab_id, column_count, row_count, None)
}

/// Attach a client viewing `tab_id` with the given viewport, reporting
/// `pane_area`, and return the id it attached under.
fn attach_viewer_with_pane_area(
    session: &mut Session,
    tab_id: TabId,
    column_count: u16,
    row_count: u16,
    pane_area: Option<PaneArea>,
) -> ClientId {
    let client_id = ClientId::new();
    session.attach_client(build_test_client(
        client_id,
        session.session_id,
        tab_id,
        Size {
            column_count,
            row_count,
        },
        pane_area,
    ));
    client_id
}

/// A session with no tabs and no clients.
fn build_empty_session() -> Session {
    Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Create a tab through [`commit_new_tab`] with freshly minted ids, no focus
/// client, and an empty spec, and return the events it emitted.
fn commit_test_tab(session: &mut Session, tab_name: String) -> Vec<Event> {
    commit_new_tab(
        session,
        TabId::new(),
        PaneId::new(),
        tab_name,
        None,
        NewPaneSpec::default(),
    )
    .1
}

/// The id of the tab a [`commit_new_tab`] call created, read off its
/// `TabCreated`.
fn get_created_tab_id(emitted_events: &[Event]) -> TabId {
    emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::TabCreated(tab_created) => Some(tab_created.tab_id),
            _ => None,
        })
        .expect("commit_new_tab emits a TabCreated event")
}

/// The id of the pane a [`commit_new_tab`] call created, read off its
/// `PaneCreated`.
fn get_created_pane_id(emitted_events: &[Event]) -> PaneId {
    emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(pane_created) => Some(pane_created.pane_id),
            _ => None,
        })
        .expect("commit_new_tab emits a PaneCreated event")
}

#[test]
fn tab_size_takes_the_per_axis_minimum_across_viewers() {
    let tab_id = TabId::new();
    let other_tab_id = TabId::new();
    let mut session = build_empty_session();

    // Two clients view `tab_id` with opposite aspect ratios.
    attach_viewer(&mut session, tab_id, 80, 5);
    attach_viewer(&mut session, tab_id, 40, 24);
    // A client on a different tab does not count.
    attach_viewer(&mut session, other_tab_id, 10, 1);

    // The full-viewport minimum is 40x5; minus the tabline and hint rows it
    // leaves 40x3.
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 40,
            row_count: 3
        })
    );
}

#[test]
fn tab_size_is_none_without_an_attached_viewer() {
    let tab_id = TabId::new();
    let session = build_empty_session();

    assert_eq!(session.get_tab_size(tab_id), None);
}

#[test]
fn find_tab_by_pane_id_finds_the_tab_holding_each_leaf_and_none_for_other_panes() {
    let mut session = build_empty_session();
    let (first_pane_id, second_pane_id, third_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let first_tab = Tab::from_root_pane(TabId::new(), "a".to_owned(), 0, first_pane_id);
    let mut second_tab = Tab::from_root_pane(TabId::new(), "b".to_owned(), 1, second_pane_id);
    second_tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(second_pane_id),
            LayoutNode::Pane(third_pane_id),
        ],
    )));
    let (first_tab_id, second_tab_id) = (first_tab.get_tab_id(), second_tab.get_tab_id());
    session.tabs.insert(first_tab_id, first_tab);
    session.tabs.insert(second_tab_id, second_tab);
    let floating_pane_id = register_floating_pane(&mut session);

    for (pane_id, expected_tab_id) in [
        (first_pane_id, Some(first_tab_id)),
        (second_pane_id, Some(second_tab_id)),
        (third_pane_id, Some(second_tab_id)),
        (floating_pane_id, None),
        (PaneId::new(), None),
    ] {
        assert_eq!(
            session.find_tab_by_pane_id(pane_id).map(Tab::get_tab_id),
            expected_tab_id
        );
    }
}

#[test]
fn shared_floating_viewport_takes_the_per_axis_minimum_across_every_tab() {
    let mut session = build_empty_session();

    // Two clients view two different tabs, with opposite aspect ratios.
    attach_viewer(&mut session, TabId::new(), 80, 5);
    attach_viewer(&mut session, TabId::new(), 40, 24);

    // The full-viewport minimum is 40x5; minus the tabline and hint rows it
    // leaves 40x3.
    assert_eq!(
        session.get_shared_floating_viewport(),
        Some(Size {
            column_count: 40,
            row_count: 3
        })
    );
}

#[test]
fn shared_floating_viewport_leaves_a_starving_client_out() {
    let mut session = build_empty_session();
    let roomy_pane_area = Size {
        column_count: 120,
        row_count: 40,
    };

    attach_viewer_with_pane_area(
        &mut session,
        TabId::new(),
        120,
        42,
        Some(PaneArea::Reported(roomy_pane_area)),
    );
    attach_viewer_with_pane_area(&mut session, TabId::new(), 10, 3, Some(PaneArea::Starving));

    assert_eq!(
        session.get_shared_floating_viewport(),
        Some(roomy_pane_area)
    );
}

#[test]
fn shared_floating_viewport_is_none_without_a_contributing_client() {
    let mut session = build_empty_session();
    assert_eq!(session.get_shared_floating_viewport(), None);

    attach_viewer_with_pane_area(&mut session, TabId::new(), 80, 24, Some(PaneArea::Starving));
    attach_viewer_with_pane_area(&mut session, TabId::new(), 40, 24, Some(PaneArea::Starving));

    assert_eq!(session.get_shared_floating_viewport(), None);
}

#[test]
fn a_new_session_stores_the_supplied_creation_time() {
    let created_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234);
    let session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        created_at,
        ClientRegistry::new(),
    );

    assert_eq!(session.created_at, created_at);
}

#[test]
fn a_session_placement_revision_advances_once_and_refuses_wraparound() {
    let mut session = build_empty_session();

    assert_eq!(session.get_placement_revision(), 0);
    assert!(session.can_advance_placement_revision());
    assert!(session.advance_placement_revision());
    assert_eq!(session.get_placement_revision(), 1);

    session.placement_revision = u64::MAX;
    assert!(!session.can_advance_placement_revision());
    assert!(!session.advance_placement_revision());
    assert_eq!(session.get_placement_revision(), u64::MAX);
}

#[test]
fn tab_size_with_exactly_one_viewer_returns_its_own_reserved_size() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer(&mut session, tab_id, 100, 30);

    // With one viewer the result is that viewer's own size minus the tabline
    // and hint rows.
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 100,
            row_count: 28
        })
    );
}

#[test]
fn tab_size_saturates_at_zero_rows_below_the_tabline_and_hint_rows() {
    // A viewport with fewer rows than the tabline and hint rows saturates the
    // row count at `0`.
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer(&mut session, tab_id, 80, 1);

    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 0
        })
    );
}

#[test]
fn tab_size_leaves_a_starving_viewer_out() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer(&mut session, tab_id, 80, 24);
    attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, Some(PaneArea::Starving));

    // Only the viewer that reported a pane area counts: 80x24 minus the
    // tabline and hint rows.
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn tab_size_with_a_zero_reported_area_is_zero() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer(&mut session, tab_id, 80, 24);
    attach_viewer_with_pane_area(
        &mut session,
        tab_id,
        80,
        24,
        Some(PaneArea::Reported(Size {
            column_count: 0,
            row_count: 0,
        })),
    );

    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 0,
            row_count: 0
        })
    );
}

#[test]
fn tab_size_is_none_when_every_viewer_is_starving() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, Some(PaneArea::Starving));
    attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, Some(PaneArea::Starving));

    assert_eq!(session.get_tab_size(tab_id), None);
}

#[test]
fn tab_size_takes_the_minimum_of_a_reported_and_an_unreported_attach_viewer() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(
        &mut session,
        tab_id,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 60,
            row_count: 30,
        })),
    );
    attach_viewer(&mut session, tab_id, 80, 24);

    // 60x30 reported against 80x22 from the unreported viewer.
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 60,
            row_count: 22
        })
    );
}

#[test]
fn tab_size_takes_the_element_wise_minimum_of_two_reports() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(
        &mut session,
        tab_id,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 100,
            row_count: 20,
        })),
    );
    attach_viewer_with_pane_area(
        &mut session,
        tab_id,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 60,
            row_count: 30,
        })),
    );

    // The narrower report gives the columns, the shorter one gives the rows.
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 60,
            row_count: 20
        })
    );
}

#[test]
fn tab_size_does_not_depend_on_where_the_starving_viewer_attached() {
    let tab_id = TabId::new();

    let mut starving_first_session = build_empty_session();
    attach_viewer_with_pane_area(
        &mut starving_first_session,
        tab_id,
        80,
        24,
        Some(PaneArea::Starving),
    );
    attach_viewer(&mut starving_first_session, tab_id, 80, 24);

    let mut starving_second_session = build_empty_session();
    attach_viewer(&mut starving_second_session, tab_id, 80, 24);
    attach_viewer_with_pane_area(
        &mut starving_second_session,
        tab_id,
        80,
        24,
        Some(PaneArea::Starving),
    );

    assert_eq!(
        starving_first_session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
    assert_eq!(
        starving_second_session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn detaching_a_starving_viewer_leaves_tab_size_unchanged() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    attach_viewer(&mut session, tab_id, 80, 24);
    let starving_client_id =
        attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, Some(PaneArea::Starving));
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );

    session.detach_client(starving_client_id);

    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn detaching_the_smallest_viewer_lets_tab_size_grow() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    let smallest_client_id = attach_viewer_with_pane_area(
        &mut session,
        tab_id,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 60,
            row_count: 30,
        })),
    );
    attach_viewer(&mut session, tab_id, 120, 40);
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 60,
            row_count: 30
        })
    );

    session.detach_client(smallest_client_id);

    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 120,
            row_count: 38
        })
    );
}

#[test]
fn attach_client_returns_the_client_it_displaced_on_reattach() {
    // A re-attach under the same id replaces the client record in place and
    // returns the one it displaced.
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    let client_id = ClientId::new();
    let initial_client = build_test_client(
        client_id,
        session.session_id,
        tab_id,
        Size {
            column_count: 0,
            row_count: 0,
        },
        None,
    );
    assert_eq!(
        session
            .attach_client(initial_client)
            .map(|client| client.get_client_id()),
        None
    );

    let reattached_client = build_test_client(
        client_id,
        session.session_id,
        tab_id,
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
    );
    let displaced_client = session.attach_client(reattached_client);

    assert_eq!(
        displaced_client.map(|client| client.get_viewport_size()),
        Some(Size {
            column_count: 0,
            row_count: 0
        })
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .map(Client::get_viewport_size),
        Some(Size {
            column_count: 40,
            row_count: 10
        })
    );
    assert_eq!(session.clients.count_clients(), 1);
}

#[test]
fn attaching_a_client_before_any_tab_leaves_the_session_starting() {
    // `ClientAttached` moves only a `Detaching` session to `Running`. A session
    // that has not created its first tab is `Starting` and rejects the event.
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);

    attach_viewer(&mut session, tab_id, 0, 0);

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detach_client_returns_the_exact_record_it_removed() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    let client_id = attach_viewer_with_pane_area(&mut session, tab_id, 12, 3, None);

    let removed_client = session.detach_client(client_id);

    assert_eq!(
        removed_client.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .map(Client::get_client_id),
        None
    );
    assert_eq!(session.clients.count_clients(), 0);
}

#[test]
fn detach_client_on_an_unattached_id_returns_none() {
    let mut session = build_empty_session();

    assert_eq!(
        session
            .detach_client(ClientId::new())
            .map(|client| client.get_client_id()),
        None
    );
}

#[test]
fn request_session_stop_is_idempotent_once_already_stopping() {
    let mut session = build_empty_session();

    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    // `StopRequested` from `Stopping` is rejected; the session stays
    // `Stopping`.
    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn complete_session_stop_before_a_stop_was_requested_is_a_noop() {
    // `StopCompleted` is only legal from `Stopping`; calling it on a fresh
    // (`Starting`) session is an illegal transition the wrapper swallows.
    let mut session = build_empty_session();

    session.complete_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_the_last_client_of_a_stopping_session_does_not_revert_it() {
    // A `Stopping` session that loses its last client stays `Stopping`; it does
    // not fall back to `Detaching`.
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    let client_id = attach_viewer_with_pane_area(&mut session, tab_id, 0, 0, None);
    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    let removed_client = session.detach_client(client_id);

    assert_eq!(
        removed_client.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn detaching_the_last_client_of_a_starting_session_leaves_it_starting() {
    // `LastClientDetached` moves only a `Running` session to `Detaching`. A
    // session that has not created its first tab is `Starting` and rejects it.
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    let client_id = attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, None);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);

    let removed_client = session.detach_client(client_id);

    assert_eq!(
        removed_client.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(session.clients.count_clients(), 0);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_again_with_no_clients_left_keeps_the_session_detaching() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");
    let client_id = attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, None);

    session.detach_client(client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);

    // The registry is already empty: the second detach fires
    // `LastClientDetached` again, and `Detaching` rejects it and does not move.
    let removed_client = session.detach_client(ClientId::new());

    assert_eq!(removed_client.map(|client| client.get_client_id()), None);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn completing_a_stop_twice_leaves_the_session_stopped() {
    let mut session = build_empty_session();
    session.request_session_stop();
    session.complete_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);

    // `Stopped` is terminal and rejects every event, including a repeat.
    session.complete_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);
}

/// Drive a new session to `Detaching`: create its first tab, attach one viewer
/// of `tab_id`, then detach it.
fn build_detached_session(tab_id: TabId) -> Session {
    let mut session = build_empty_session();
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");
    let client_id = attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, None);
    session.detach_client(client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
    session
}

#[test]
fn request_session_stop_moves_a_detaching_session_to_stopping() {
    let mut session = build_detached_session(TabId::new());

    session.request_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn complete_session_stop_on_a_detaching_session_leaves_it_detaching() {
    // `StopCompleted` is legal only from `Stopping`; `Detaching` rejects it.
    let mut session = build_detached_session(TabId::new());

    session.complete_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn attaching_a_client_to_a_stopped_session_registers_it_without_reviving_it() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    session.request_session_stop();
    session.complete_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);

    let client_id = attach_viewer_with_pane_area(&mut session, tab_id, 80, 24, None);

    assert_eq!(session.clients.count_clients(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .map(Client::get_client_id),
        Some(client_id)
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);
}

#[test]
fn tab_size_counts_a_client_only_once_it_switches_onto_the_tab() {
    let tab_id = TabId::new();
    let other_tab_id = TabId::new();
    let mut session = build_empty_session();
    let client_id = attach_viewer_with_pane_area(&mut session, other_tab_id, 100, 30, None);
    assert_eq!(session.get_tab_size(tab_id), None);

    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the viewer is attached")
        .update_active_tab_id(tab_id);

    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 100,
            row_count: 28
        })
    );
    assert_eq!(session.get_tab_size(other_tab_id), None);
}

#[test]
fn tab_size_clamps_a_report_larger_than_the_viewport() {
    let tab_id = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(
        &mut session,
        tab_id,
        80,
        24,
        Some(PaneArea::Reported(Size {
            column_count: u16::MAX,
            row_count: u16::MAX,
        })),
    );

    // A report is clamped per axis to the viewport. The tabline and hint rows
    // come off only when no report exists: 80x24, not 80x22.
    assert_eq!(
        session.get_tab_size(tab_id),
        Some(Size {
            column_count: 80,
            row_count: 24
        })
    );
}

#[test]
fn an_empty_session_survives_a_serde_round_trip() {
    let session = build_empty_session();

    let serialized_session_json = serde_json::to_string(&session).expect("the session writes out");
    let decoded_session: Session =
        serde_json::from_str(&serialized_session_json).expect("the session reads back");

    assert_eq!(decoded_session.session_id, session.session_id);
    assert_eq!(decoded_session.session_name, "s");
    assert_eq!(decoded_session.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*decoded_session.get_lifecycle(), SessionLifecycle::Starting);
    assert!(!decoded_session.should_start_locked);
    assert_eq!(decoded_session.tabs.len(), 0);
    assert_eq!(decoded_session.panes.count_pane_records(), 0);
    assert_eq!(decoded_session.clients.count_clients(), 0);
}

/// A whole session survives being written out and read back: its identity and
/// lifecycle, its tabs with their nested layout trees, its pane registry, and
/// the view state each attached client keeps to itself.
#[test]
fn a_session_with_tabs_panes_and_clients_survives_a_serde_round_trip() {
    let first_tab_id = TabId::new();
    let second_tab_id = TabId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let third_pane_id = PaneId::new();
    let fourth_pane_id = PaneId::new();

    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "carried".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");

    // The first tab splits left and right, and its right half splits again:
    // the first pane fills the left, the second and third panes share the
    // right.
    let first_tab_layout = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(first_pane_id),
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![
                    LayoutNode::Pane(second_pane_id),
                    LayoutNode::Pane(third_pane_id),
                ],
            )),
        ],
    ));
    let mut first_tab = Tab::from_root_pane(first_tab_id, "one".to_owned(), 0, first_pane_id);
    first_tab.update_layout(first_tab_layout.clone());
    first_tab.record_focus_mru(second_pane_id);
    session.tabs.insert(first_tab_id, first_tab);
    session.tabs.insert(
        second_tab_id,
        Tab::from_root_pane(second_tab_id, "two".to_owned(), 1, fourth_pane_id),
    );

    for pane_id in [first_pane_id, second_pane_id, third_pane_id, fourth_pane_id] {
        session
            .panes
            .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
            .expect("each pane id is registered once");
    }

    let highlight_selection = Selection {
        selection_kind: SelectionKind::Block,
        anchor: GridPosition {
            row_index: 12,
            column_index: 4,
        },
        cursor: GridPosition {
            row_index: 40,
            column_index: 9,
        },
    };

    // The first client watches the first tab zoomed on the second pane,
    // scrolled up in it, locked, grabbing the mouse, with a highlight up.
    let zoomed_client_id = ClientId::new();
    let mut zoomed_client = Client::from_attachment(
        zoomed_client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 100,
            row_count: 30,
        },
        None,
        first_tab_id,
        ClientOrigin::Local,
        "C-brave-otter".to_owned(),
        3,
    );
    zoomed_client.update_focused_pane(first_tab_id, second_pane_id);
    zoomed_client.update_focused_pane(second_tab_id, fourth_pane_id);
    zoomed_client.zoom_pane(first_tab_id, second_pane_id);
    zoomed_client.set_scroll_offset(second_pane_id, 7);
    zoomed_client.update_lock_mode(LockMode::Locked);
    zoomed_client.toggle_mouse_selection();
    zoomed_client.set_selection(second_pane_id, highlight_selection);
    session.attach_client(zoomed_client);

    // The second client watches the second tab, tiled, scrolled up in a
    // different pane: no field is the same for both.
    let tiled_client_id = ClientId::new();
    let mut tiled_client = Client::from_attachment(
        tiled_client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        second_tab_id,
        ClientOrigin::Local,
        "C-calm-heron".to_owned(),
        5,
    );
    tiled_client.update_focused_pane(first_tab_id, first_pane_id);
    tiled_client.set_scroll_offset(third_pane_id, 3);
    session.attach_client(tiled_client);
    assert!(session.advance_placement_revision());
    assert!(session
        .clients
        .get_client_mut_by_id(zoomed_client_id)
        .expect("zoomed client is attached")
        .advance_placement_revision());

    let serialized_session_json = serde_json::to_string(&session).expect("the session writes out");
    let decoded_session: Session =
        serde_json::from_str(&serialized_session_json).expect("the session reads back");

    assert_eq!(decoded_session.session_id, session.session_id);
    assert_eq!(decoded_session.session_name, "carried");
    assert_eq!(decoded_session.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*decoded_session.get_lifecycle(), SessionLifecycle::Running);
    assert_eq!(decoded_session.get_placement_revision(), 1);
    assert_eq!(
        decoded_session
            .clients
            .get_client_by_id(zoomed_client_id)
            .expect("zoomed client is carried")
            .get_placement_revision(),
        1
    );

    assert_eq!(decoded_session.tabs.len(), 2);
    let recovered_first_tab = decoded_session
        .tabs
        .get(&first_tab_id)
        .expect("the first tab is carried");
    assert_eq!(recovered_first_tab.get_tab_id(), first_tab_id);
    assert_eq!(recovered_first_tab.get_tab_name(), "one");
    assert_eq!(recovered_first_tab.get_tab_index(), 0);
    assert_eq!(*recovered_first_tab.get_layout_tree(), first_tab_layout);
    assert_eq!(
        recovered_first_tab.list_focus_mru(),
        [second_pane_id].as_slice()
    );
    let recovered_second_tab = decoded_session
        .tabs
        .get(&second_tab_id)
        .expect("the second tab is carried");
    assert_eq!(recovered_second_tab.get_tab_index(), 1);
    assert_eq!(
        *recovered_second_tab.get_layout_tree(),
        LayoutNode::Pane(fourth_pane_id)
    );

    assert_eq!(decoded_session.panes.count_pane_records(), 4);
    for pane_id in [first_pane_id, second_pane_id, third_pane_id, fourth_pane_id] {
        let pane_record = decoded_session
            .panes
            .get_pane_record_by_id(pane_id)
            .expect("the pane record is carried");
        assert_eq!(pane_record.get_pane_id(), pane_id);
        assert_eq!(*pane_record.get_lifecycle(), PaneLifecycle::Spawning);
    }

    assert_eq!(decoded_session.clients.count_clients(), 2);
    let recovered_zoomed_client = decoded_session
        .clients
        .get_client_by_id(zoomed_client_id)
        .expect("the first client is carried");
    assert_eq!(recovered_zoomed_client.get_session_id(), session.session_id);
    assert_eq!(
        recovered_zoomed_client.get_attached_at(),
        SystemTime::UNIX_EPOCH
    );
    assert_eq!(recovered_zoomed_client.get_origin(), ClientOrigin::Local);
    assert_eq!(recovered_zoomed_client.get_label(), "C-brave-otter");
    assert_eq!(recovered_zoomed_client.get_color_index(), 3);
    assert_eq!(
        recovered_zoomed_client.get_viewport_size(),
        Size {
            column_count: 100,
            row_count: 30
        }
    );
    assert_eq!(recovered_zoomed_client.get_active_tab_id(), first_tab_id);
    assert_eq!(recovered_zoomed_client.get_lock_mode(), LockMode::Locked);
    assert!(recovered_zoomed_client.is_mouse_selection_enabled());
    assert_eq!(
        recovered_zoomed_client.get_focused_pane_id(first_tab_id),
        Some(second_pane_id)
    );
    assert_eq!(
        recovered_zoomed_client.get_focused_pane_id(second_tab_id),
        Some(fourth_pane_id)
    );
    assert_eq!(
        recovered_zoomed_client.get_zoomed_pane_id(first_tab_id),
        Some(second_pane_id)
    );
    assert_eq!(
        recovered_zoomed_client.get_zoomed_pane_id(second_tab_id),
        None
    );
    assert_eq!(recovered_zoomed_client.get_scroll_offset(second_pane_id), 7);
    assert_eq!(recovered_zoomed_client.get_scroll_offset(third_pane_id), 0);
    assert_eq!(
        recovered_zoomed_client.get_selection(second_pane_id),
        Some(highlight_selection)
    );
    assert_eq!(recovered_zoomed_client.get_selection(first_pane_id), None);

    let recovered_tiled_client = decoded_session
        .clients
        .get_client_by_id(tiled_client_id)
        .expect("the second client is carried");
    assert_eq!(recovered_tiled_client.get_label(), "C-calm-heron");
    assert_eq!(recovered_tiled_client.get_color_index(), 5);
    assert_eq!(
        recovered_tiled_client.get_viewport_size(),
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    assert_eq!(recovered_tiled_client.get_active_tab_id(), second_tab_id);
    assert_eq!(recovered_tiled_client.get_lock_mode(), LockMode::Normal);
    assert!(!recovered_tiled_client.is_mouse_selection_enabled());
    assert_eq!(
        recovered_tiled_client.get_focused_pane_id(first_tab_id),
        Some(first_pane_id)
    );
    assert_eq!(
        recovered_tiled_client.get_focused_pane_id(second_tab_id),
        None
    );
    assert_eq!(
        recovered_tiled_client.get_zoomed_pane_id(first_tab_id),
        None
    );
    assert_eq!(recovered_tiled_client.get_scroll_offset(third_pane_id), 3);
    assert_eq!(recovered_tiled_client.get_scroll_offset(second_pane_id), 0);
}

#[test]
fn attaching_a_client_to_a_stopping_session_registers_it_without_reviving_it() {
    // `ClientAttached` moves only a `Detaching` session to `Running`. A client
    // that attaches while the session is `Stopping` is still registered, and
    // the lifecycle stays `Stopping`.
    let tab_id = TabId::new();
    let mut session = build_empty_session();
    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    let client_id = ClientId::new();
    let client = build_test_client(
        client_id,
        session.session_id,
        tab_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
    );
    let displaced_client = session.attach_client(client);

    assert_eq!(displaced_client.map(|client| client.get_client_id()), None);
    assert_eq!(session.clients.count_clients(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .map(Client::get_client_id),
        Some(client_id)
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn tab_cell_size_uses_the_oldest_measured_viewer_and_changes_on_detach() {
    let tab_id = TabId::new();
    let other_tab_id = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "images".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let mut clients = [
        build_test_client(
            ClientId::new(),
            session.session_id,
            tab_id,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
        ),
        build_test_client(
            ClientId::new(),
            session.session_id,
            tab_id,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
        ),
        build_test_client(
            ClientId::new(),
            session.session_id,
            other_tab_id,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
        ),
    ];
    clients.sort_by_key(Client::get_client_id);
    let first_client_id = clients[0].get_client_id();
    let second_client_id = clients[1].get_client_id();
    clients[0].update_active_tab_id(tab_id);
    clients[1].update_active_tab_id(tab_id);
    clients[2].update_active_tab_id(other_tab_id);
    clients[1].update_cell_size(PixelCellSize::from_pixel_dimensions(12, 24));
    clients[2].update_cell_size(PixelCellSize::from_pixel_dimensions(8, 16));
    for client in clients {
        session.clients.attach_client(client);
    }
    assert_eq!(
        session.get_tab_cell_size(tab_id),
        PixelCellSize::from_pixel_dimensions(12, 24)
    );
    session
        .clients
        .get_client_mut_by_id(first_client_id)
        .expect("client")
        .update_cell_size(PixelCellSize::from_pixel_dimensions(10, 20));
    assert_eq!(
        session.get_tab_cell_size(tab_id),
        PixelCellSize::from_pixel_dimensions(10, 20)
    );
    session.clients.detach_client(first_client_id);
    assert_eq!(
        session.get_tab_cell_size(tab_id),
        PixelCellSize::from_pixel_dimensions(12, 24)
    );
    session.clients.detach_client(second_client_id);
    assert_eq!(session.get_tab_cell_size(tab_id), None);
    assert_eq!(
        session.get_tab_cell_size(other_tab_id),
        PixelCellSize::from_pixel_dimensions(8, 16)
    );
}

#[test]
fn floating_cell_size_uses_the_earliest_measured_client_whichever_tab_it_views() {
    let mut session = build_empty_session();
    let (db_tab_id, web_tab_id) = (TabId::new(), TabId::new());
    // carol attaches first and reports no cell size; alice views `db` with
    // 10x20 px cells; bob attaches last and views `web` with 8x16 px cells.
    let carol_client = Client::from_attachment(
        ClientId::new(),
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        web_tab_id,
        ClientOrigin::Local,
        "C-carol".to_string(),
        0,
    );
    let mut alice_client = Client::from_attachment(
        ClientId::new(),
        session.session_id,
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        db_tab_id,
        ClientOrigin::Local,
        "C-alice".to_string(),
        1,
    );
    alice_client.update_cell_size(PixelCellSize::from_pixel_dimensions(10, 20));
    let mut bob_client = Client::from_attachment(
        ClientId::new(),
        session.session_id,
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        web_tab_id,
        ClientOrigin::Local,
        "C-bob".to_string(),
        2,
    );
    bob_client.update_cell_size(PixelCellSize::from_pixel_dimensions(8, 16));
    let alice_client_id = alice_client.get_client_id();
    let bob_client_id = bob_client.get_client_id();
    for client in [bob_client, carol_client, alice_client] {
        session.attach_client(client);
    }

    assert_eq!(
        session.get_floating_cell_size(),
        PixelCellSize::from_pixel_dimensions(10, 20)
    );
    session.detach_client(alice_client_id);
    assert_eq!(
        session.get_floating_cell_size(),
        PixelCellSize::from_pixel_dimensions(8, 16)
    );
    session.detach_client(bob_client_id);
    assert_eq!(session.get_floating_cell_size(), None);
}

#[test]
fn a_new_session_starts_empty() {
    let session_id = SessionId::new();
    let session = Session::from_identity_and_client_registry(
        session_id,
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(session.session_id, session_id);
    assert_eq!(session.session_name, "main");
    assert!(session.tabs.is_empty());
    assert_eq!(session.panes.count_pane_records(), 0);
    assert!(!session.clients.has_clients());
}

#[test]
fn a_new_tab_owns_its_layout_and_starts_unfocused() {
    let tab_id = TabId::new();
    let root_pane_id = PaneId::new();
    let tab = Tab::from_root_pane(tab_id, "code".to_owned(), 0, root_pane_id);

    assert_eq!(tab.get_tab_id(), tab_id);
    assert_eq!(tab.get_tab_name(), "code");
    assert_eq!(tab.get_tab_index(), 0);
    // A fresh tab shows exactly its root pane, no focus yet. It carries no
    // layout mode of its own: whether a pane is zoomed belongs to a client's
    // view, not to the tab.
    assert_eq!(*tab.get_layout_tree(), LayoutNode::Pane(root_pane_id));
    assert!(tab.list_focus_mru().is_empty());
}

#[test]
fn a_tab_index_can_be_reassigned() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());

    tab.update_tab_index(3);

    assert_eq!(tab.get_tab_index(), 3);
}

#[test]
fn record_focus_orders_newest_first() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (oldest_pane_id, middle_pane_id, newest_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());

    tab.record_focus_mru(oldest_pane_id);
    tab.record_focus_mru(middle_pane_id);
    tab.record_focus_mru(newest_pane_id);

    assert_eq!(
        tab.list_focus_mru().to_vec(),
        vec![newest_pane_id, middle_pane_id, oldest_pane_id]
    );
}

#[test]
fn re_focusing_moves_to_front_without_duplicating() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());

    tab.record_focus_mru(first_pane_id);
    tab.record_focus_mru(second_pane_id);
    tab.record_focus_mru(first_pane_id);

    // The first pane returns to the front; it is not stored twice.
    assert_eq!(
        tab.list_focus_mru().to_vec(),
        vec![first_pane_id, second_pane_id]
    );
}

#[test]
fn focus_mru_is_capped_dropping_the_oldest() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let focus_history_entry_count = MAX_TAB_FOCUS_MRU_ENTRY_COUNT as usize;

    // Record one more distinct pane than the cap allows.
    let pane_ids: Vec<PaneId> = (0..=focus_history_entry_count)
        .map(|_| PaneId::new())
        .collect();
    for &pane_id in &pane_ids {
        tab.record_focus_mru(pane_id);
    }

    // Newest first, with the first-recorded pane evicted: every other pane keeps
    // its place in recording order.
    let surviving_newest_first_pane_ids: Vec<PaneId> =
        pane_ids[1..].iter().rev().copied().collect();
    assert_eq!(
        tab.list_focus_mru().to_vec(),
        surviving_newest_first_pane_ids
    );
}

#[test]
fn focus_mru_at_exactly_the_cap_evicts_nothing() {
    // The boundary just below the eviction case above: recording exactly
    // `MAX_TAB_FOCUS_MRU_ENTRY_COUNT` distinct panes keeps every one of them.
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let focus_history_entry_count = MAX_TAB_FOCUS_MRU_ENTRY_COUNT as usize;

    let pane_ids: Vec<PaneId> = (0..focus_history_entry_count)
        .map(|_| PaneId::new())
        .collect();
    for &pane_id in &pane_ids {
        tab.record_focus_mru(pane_id);
    }

    let newest_first_pane_ids: Vec<PaneId> = pane_ids.iter().rev().copied().collect();
    assert_eq!(tab.list_focus_mru().to_vec(), newest_first_pane_ids);
}

#[test]
fn re_recording_an_existing_pane_at_the_cap_moves_it_front_without_evicting() {
    // Re-recording an entry a full history already holds evicts nothing: the
    // duplicate is dropped before the length is checked.
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let focus_history_entry_count = MAX_TAB_FOCUS_MRU_ENTRY_COUNT as usize;
    let pane_ids: Vec<PaneId> = (0..focus_history_entry_count)
        .map(|_| PaneId::new())
        .collect();
    for &pane_id in &pane_ids {
        tab.record_focus_mru(pane_id);
    }
    // The re-recorded pane comes from the middle of the history. The back entry
    // is the one the cap evicts on its own.
    let middle_pane_id = pane_ids[focus_history_entry_count / 2];

    tab.record_focus_mru(middle_pane_id);

    // `middle_pane_id` moves to the front and every other pane keeps its order behind it.
    let mut expected_focus_history_pane_ids: Vec<PaneId> = pane_ids.iter().rev().copied().collect();
    expected_focus_history_pane_ids.retain(|&pane_id| pane_id != middle_pane_id);
    expected_focus_history_pane_ids.insert(0, middle_pane_id);
    assert_eq!(
        tab.list_focus_mru().to_vec(),
        expected_focus_history_pane_ids
    );
}

#[test]
fn recording_the_same_pane_twice_keeps_one_entry() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let pane_id = PaneId::new();

    tab.record_focus_mru(pane_id);
    tab.record_focus_mru(pane_id);

    assert_eq!(tab.list_focus_mru().to_vec(), vec![pane_id]);
}

#[test]
fn remove_focus_mru_drops_only_the_named_pane() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (oldest_pane_id, middle_pane_id, newest_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    tab.record_focus_mru(oldest_pane_id);
    tab.record_focus_mru(middle_pane_id);
    tab.record_focus_mru(newest_pane_id); // newest first: [newest, middle, oldest]

    tab.remove_focus_mru(middle_pane_id);

    assert_eq!(
        tab.list_focus_mru().to_vec(),
        vec![newest_pane_id, oldest_pane_id]
    );
}

#[test]
fn remove_focus_mru_for_a_pane_never_recorded_is_a_noop() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let recorded_pane_id = PaneId::new();
    tab.record_focus_mru(recorded_pane_id);

    tab.remove_focus_mru(PaneId::new());

    assert_eq!(tab.list_focus_mru().to_vec(), vec![recorded_pane_id]);
}

#[test]
fn remove_focus_mru_on_an_empty_history_is_a_noop() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());

    tab.remove_focus_mru(PaneId::new());

    assert!(tab.list_focus_mru().is_empty());
}

#[test]
fn the_starting_lock_is_taken_once() {
    let mut session = build_empty_session();
    session.should_start_locked = true;

    assert!(session.take_start_lock(), "the first read takes the lock");
    assert!(!session.take_start_lock(), "the second read finds none");
    assert!(!session.should_start_locked);
}

#[test]
fn a_session_seeded_without_the_lock_marker_has_no_starting_lock() {
    let mut session = build_empty_session();

    assert!(!session.take_start_lock());
}

#[test]
fn the_starting_lock_survives_a_serde_round_trip() {
    let mut session = build_empty_session();
    session.should_start_locked = true;

    let serialized_session_json = serde_json::to_string(&session).expect("serialize");
    let restored_session: Session =
        serde_json::from_str(&serialized_session_json).expect("deserialize");

    assert!(
        restored_session.should_start_locked,
        "a session server that restarts before any client attaches still locks the first one"
    );
}

#[test]
fn recovery_notice_survives_serialization_and_absent_field_reads_as_hidden() {
    let mut session = build_empty_session();
    session.is_recovery_notice_visible = true;
    let serialized_session = serde_json::to_value(&session).expect("serialize session");
    assert_eq!(
        serialized_session["is_recovery_notice_visible"],
        serde_json::Value::Bool(true)
    );
    let restored_session: Session =
        serde_json::from_value(serialized_session.clone()).expect("restore session");
    assert!(restored_session.is_recovery_notice_visible);

    let mut session_without_notice_field = serialized_session;
    session_without_notice_field
        .as_object_mut()
        .expect("session object")
        .remove("is_recovery_notice_visible");
    let restored_session: Session = serde_json::from_value(session_without_notice_field)
        .expect("restore a session without the notice field");
    assert!(!restored_session.is_recovery_notice_visible);
}

#[test]
fn the_starting_lock_is_stored_as_a_plain_json_bool() {
    // Pins the stored shape: the member is named `should_start_locked` and holds a
    // JSON boolean.
    let mut session = build_empty_session();
    session.should_start_locked = true;

    let serialized_session = serde_json::to_value(&session).expect("serialize");

    assert_eq!(
        serialized_session["should_start_locked"],
        serde_json::Value::Bool(true)
    );
}

#[test]
fn a_tab_survives_a_serde_round_trip() {
    let root_pane_id = PaneId::new();
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 2, root_pane_id);
    tab.record_focus_mru(root_pane_id);

    let serialized_tab_json = serde_json::to_string(&tab).expect("serialize");
    let restored_tab: Tab = serde_json::from_str(&serialized_tab_json).expect("deserialize");

    assert_eq!(tab, restored_tab);
}

#[test]
fn a_tabs_name_is_stored_as_a_plain_json_string() {
    // Pins the stored shape: the member is named `tab_name` and holds a JSON
    // string, not a nested object.
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 2, PaneId::new());

    let serialized_tab = serde_json::to_value(&tab).expect("serialize");

    assert_eq!(
        serialized_tab["tab_name"],
        serde_json::Value::String("code".to_owned())
    );
}

#[test]
fn a_fresh_session_is_starting() {
    let session = build_empty_session();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_the_last_client_moves_the_session_to_detaching_and_keeps_its_tabs() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    // Attaching to a running session leaves it running.
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    session.detach_client(client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
    // The tab and its pane stay alive.
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        vec![tab_id]
    );
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane_id)
            .expect("the pane stays")
            .get_pane_id(),
        pane_id
    );
}

#[test]
fn re_attaching_resumes_a_detached_session() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);

    let detached_client_id = attach_viewer(&mut session, tab_id, 80, 24);
    session.detach_client(detached_client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);

    attach_viewer(&mut session, tab_id, 80, 24);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_one_of_several_clients_keeps_the_session_running() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);

    let first_client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let second_client_id = attach_viewer(&mut session, tab_id, 80, 24);

    session.detach_client(first_client_id);
    // One client remains: the session is still running.
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    session.detach_client(second_client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn requesting_then_completing_a_stop_walks_to_stopped() {
    let mut session = build_empty_session();
    let _ = commit_test_tab(&mut session, "code".to_owned());

    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    session.complete_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);
}

/// A `Running` pane record registered in `session`, returned by id. It is live
/// and a valid layout leaf: on its own it trips no pane-level consistency
/// check.
fn register_live_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("Spawning -> Running is a legal transition");
    session
        .panes
        .register_pane_record(pane_record)
        .expect("a fresh pane id is unique");
    pane_id
}

/// A `Removed` pane record registered in `session`, returned by id.
fn register_removed_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            close_requested_at: SystemTime::UNIX_EPOCH,
        })
        .expect("Spawning -> Closing is a legal transition");
    pane_record
        .update_lifecycle(PaneLifecycleEvent::Cleaned)
        .expect("Closing -> Removed is a legal transition");
    session
        .panes
        .register_pane_record(pane_record)
        .expect("a fresh pane id is unique");
    pane_id
}

/// A `Closing` pane record registered in `session`, returned by id. It still
/// holds a record and is still a legal layout leaf.
fn register_closing_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            close_requested_at: SystemTime::UNIX_EPOCH,
        })
        .expect("Spawning -> Closing is a legal transition");
    session
        .panes
        .register_pane_record(pane_record)
        .expect("a fresh pane id is unique");
    pane_id
}

/// An `Exited` pane record registered in `session`, returned by id. It holds
/// exit code `Some(0)` at `UNIX_EPOCH`. It is not `Removed`: the orphan check
/// fires when it is a leaf nowhere.
fn register_exited_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("Spawning -> Running is a legal transition");
    pane_record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at: SystemTime::UNIX_EPOCH,
        })
        .expect("Running -> Exited is a legal transition");
    session
        .panes
        .register_pane_record(pane_record)
        .expect("a fresh pane id is unique");
    pane_id
}

#[test]
fn a_freshly_built_session_is_consistent() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab_id, pane_id);

    session
        .validate_session_consistency()
        .expect("a session built through the normal operations is consistent");
}

#[test]
fn a_layout_leaf_with_no_record_is_reported() {
    let mut session = build_empty_session();
    let ghost_pane_id = PaneId::new();
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, ghost_pane_id);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::PaneNotInRegistry {
            tab_id,
            pane_id: ghost_pane_id,
        }])
    );
}

#[test]
fn a_session_with_no_tabs_panes_or_clients_is_consistent() {
    assert_eq!(build_empty_session().validate_session_consistency(), Ok(()));
}

#[test]
fn a_closing_pane_still_in_the_layout_is_consistent() {
    // Only a `Removed` pane is an illegal leaf. A pane in `Closing` keeps both
    // its leaf and its registry record: neither side reports it.
    let mut session = build_empty_session();
    let closing_pane_id = register_closing_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, closing_pane_id);
    session.tabs.insert(tab.get_tab_id(), tab);

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_removed_pane_left_in_the_layout_is_reported() {
    let mut session = build_empty_session();
    let removed_pane_id = register_removed_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, removed_pane_id);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);

    // A removed pane kept as a leaf breaks two invariants at once: it is an
    // illegal leaf *and* a `Removed` record still in the registry.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::RemovedPaneInLayout {
                tab_id,
                pane_id: removed_pane_id,
            },
            SessionConsistencyError::LingeringRemovedRecord {
                pane_id: removed_pane_id
            },
        ])
    );
}

#[test]
fn a_live_record_in_no_layout_is_reported() {
    let mut session = build_empty_session();
    let orphan_pane_id = register_live_pane(&mut session);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::OrphanedPaneRecord {
            pane_id: orphan_pane_id,
            pane_lifecycle: PaneLifecycle::Running,
        }])
    );
}

#[test]
fn a_removed_record_with_no_layout_is_reported() {
    let mut session = build_empty_session();
    let removed_pane_id = register_removed_pane(&mut session);

    // It is not a leaf: the layout-side check does not also fire.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::LingeringRemovedRecord {
            pane_id: removed_pane_id,
        }])
    );
}

#[test]
fn a_pane_placed_in_two_tabs_is_reported() {
    let mut session = build_empty_session();
    let shared_pane_id = register_live_pane(&mut session);
    let first_tab = Tab::from_root_pane(TabId::new(), "a".to_owned(), 0, shared_pane_id);
    let second_tab = Tab::from_root_pane(TabId::new(), "b".to_owned(), 1, shared_pane_id);
    let (first_tab_id, second_tab_id) = (first_tab.get_tab_id(), second_tab.get_tab_id());
    session.tabs.insert(first_tab_id, first_tab);
    session.tabs.insert(second_tab_id, second_tab);

    // The tabs are listed in the order `Session::tabs` walks them: ascending id.
    let mut holding_tab_ids = vec![first_tab_id, second_tab_id];
    holding_tab_ids.sort();
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::PaneInMultipleLayouts {
            pane_id: shared_pane_id,
            tab_ids: holding_tab_ids,
        }])
    );
}

#[test]
fn a_tab_stored_under_the_wrong_key_is_reported() {
    let mut session = build_empty_session();
    let pane_id = register_live_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, pane_id);
    let tab_id = tab.get_tab_id();
    let wrong_tab_id = TabId::new();
    session.tabs.insert(wrong_tab_id, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::TabKeyMismatch {
            stored_tab_id: wrong_tab_id,
            reported_tab_id: tab_id,
        }])
    );
}

#[test]
fn two_tabs_sharing_a_bar_index_are_reported() {
    let mut session = build_empty_session();
    let first_pane_id = register_live_pane(&mut session);
    let second_pane_id = register_live_pane(&mut session);
    let first_tab = Tab::from_root_pane(TabId::new(), "a".to_owned(), 0, first_pane_id);
    let second_tab = Tab::from_root_pane(TabId::new(), "b".to_owned(), 0, second_pane_id);
    session.tabs.insert(first_tab.get_tab_id(), first_tab);
    session.tabs.insert(second_tab.get_tab_id(), second_tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::DuplicateTabIndex {
            tab_index: 0
        }])
    );
}

#[test]
fn a_client_belonging_to_another_session_is_reported() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);

    let foreign_session_id = SessionId::new();
    let client = build_test_client(
        ClientId::new(),
        foreign_session_id,
        tab_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
    );
    let client_id = client.get_client_id();
    session.attach_client(client);

    // `found_session_id` names the offending client's session, not this
    // session's id.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ClientSessionMismatch {
            client_id,
            found_session_id: foreign_session_id,
        }])
    );
}

#[test]
fn a_client_active_tab_that_does_not_exist_is_reported() {
    let mut session = build_empty_session();
    let _ = commit_test_tab(&mut session, "code".to_owned());

    let phantom_tab_id = TabId::new();
    let client_id = attach_viewer(&mut session, phantom_tab_id, 80, 24);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ActiveTabMissing {
            client_id,
            tab_id: phantom_tab_id,
        }])
    );
}

#[test]
fn every_client_with_a_missing_active_tab_is_reported() {
    // The client walk covers each attached client, not only the first one that
    // trips a check. Clients are reported in ascending id order, the order
    // `ClientRegistry` iterates.
    let mut session = build_empty_session();
    let _ = commit_test_tab(&mut session, "code".to_owned());

    let phantom_tab_id = TabId::new();
    let mut viewer_client_ids = [
        attach_viewer(&mut session, phantom_tab_id, 80, 24),
        attach_viewer(&mut session, phantom_tab_id, 80, 24),
    ];
    viewer_client_ids.sort();

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::ActiveTabMissing {
                client_id: viewer_client_ids[0],
                tab_id: phantom_tab_id,
            },
            SessionConsistencyError::ActiveTabMissing {
                client_id: viewer_client_ids[1],
                tab_id: phantom_tab_id,
            },
        ])
    );
}

#[test]
fn a_client_viewing_a_gone_tab_is_not_reported_once_the_session_has_no_tabs() {
    // Closing the last tab quits the session with no successor tab. Every
    // client's `active_tab_id` names the closed tab until the transport
    // disconnects it. That state is not a violation.
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let client_id = attach_viewer(&mut session, tab_id, 80, 24);

    let _ = close_tab(&mut session, tab_id);

    assert!(session.tabs.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_active_tab_id(),
        tab_id
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_client_focus_on_an_unknown_pane_is_reported() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);

    let ghost_pane_id = PaneId::new();
    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab_id, ghost_pane_id);

    // The tab is real and the pane is not: the registry check and the layout
    // check both fire, in that order, and nothing else does.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client_id,
                tab_id,
                pane_id: ghost_pane_id,
            },
            SessionConsistencyError::FocusTargetMissing {
                client_id,
                tab_id,
                pane_id: ghost_pane_id,
            },
        ])
    );
}

#[test]
fn a_client_focus_in_a_missing_tab_is_reported() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let phantom_tab_id = TabId::new();
    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    // Focus remembered under a tab that is not in the session, on a real pane.
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(phantom_tab_id, pane_id);

    // The pane is real: the registry-side focus check does not fire.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FocusTabMissing {
            client_id,
            tab_id: phantom_tab_id,
        }])
    );
}

#[test]
fn a_focus_in_a_missing_tab_on_a_ghost_pane_reports_the_missing_record_and_the_missing_tab() {
    // Focus naming a pane with no record, remembered under a tab the session no
    // longer holds, trips the registry check and the tab check. The layout
    // check does not fire without a tab to look inside.
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let real_tab_id = get_created_tab_id(&emitted_events);

    let phantom_tab_id = TabId::new();
    let ghost_pane_id = PaneId::new();
    let client_id = attach_viewer(&mut session, real_tab_id, 80, 24);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(phantom_tab_id, ghost_pane_id);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client_id,
                tab_id: phantom_tab_id,
                pane_id: ghost_pane_id,
            },
            SessionConsistencyError::FocusTabMissing {
                client_id,
                tab_id: phantom_tab_id,
            },
        ])
    );
}

#[test]
fn a_client_focus_on_a_pane_outside_its_tab_is_reported() {
    let mut session = build_empty_session();
    let first_emitted_events = commit_test_tab(&mut session, "a".to_owned());
    let first_pane_id = get_created_pane_id(&first_emitted_events);
    let second_emitted_events = commit_test_tab(&mut session, "b".to_owned());
    let second_tab_id = get_created_tab_id(&second_emitted_events);

    let client_id = attach_viewer(&mut session, second_tab_id, 80, 24);
    // Focus recorded for the second tab but pointing at the first tab's pane:
    // a real pane that is not a leaf of the tab it is focused in.
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(second_tab_id, first_pane_id);

    // The pane exists in the registry: this is a target mismatch, not a
    // missing record.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id: second_tab_id,
            pane_id: first_pane_id,
        }])
    );
}

/// A zoom answers to the same rule as a focus: the pane it names must be a live
/// leaf of the tab it is zoomed in.
#[test]
fn a_client_zoom_on_a_pane_outside_its_tab_is_reported() {
    let mut session = build_empty_session();
    let first_emitted_events = commit_test_tab(&mut session, "a".to_owned());
    let first_pane_id = get_created_pane_id(&first_emitted_events);
    let second_emitted_events = commit_test_tab(&mut session, "b".to_owned());
    let second_tab_id = get_created_tab_id(&second_emitted_events);
    let second_pane_id = get_created_pane_id(&second_emitted_events);

    let client_id = attach_viewer(&mut session, second_tab_id, 80, 24);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    // A legitimate focus in the second tab, but a zoom pointing at the first
    // tab's pane.
    client.update_focused_pane(second_tab_id, second_pane_id);
    client.zoom_pane(second_tab_id, first_pane_id);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client_id,
            tab_id: second_tab_id,
            pane_id: first_pane_id,
        }])
    );
}

/// Zoomed on the pane this client has focused, in the tab it is viewing, is a
/// consistent state and reports nothing.
#[test]
fn a_zoom_on_the_focused_pane_is_consistent() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "a".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab_id, pane_id);
    client.zoom_pane(tab_id, pane_id);

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_zoom_on_a_pane_with_no_record_is_reported() {
    // A zoom naming a pane the registry has never heard of is not a live leaf:
    // it is reported even though the tab it is keyed under is real. The
    // client's focus sits on the real pane: no focus check fires alongside.
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let ghost_pane_id = PaneId::new();
    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab_id, pane_id);
    client.zoom_pane(tab_id, ghost_pane_id);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client_id,
            tab_id,
            pane_id: ghost_pane_id,
        }])
    );
}

#[test]
fn a_zoom_in_a_tab_that_is_gone_is_reported() {
    // A zoom entry left under a tab that has since closed points at a real pane
    // through a tab that is no longer in the session. The pane is not a live
    // leaf of that tab: `ZoomTargetMissing` names the gone tab.
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let phantom_tab_id = TabId::new();
    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab_id, pane_id);
    client.zoom_pane(phantom_tab_id, pane_id);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client_id,
            tab_id: phantom_tab_id,
            pane_id,
        }])
    );
}

#[test]
fn a_pane_appearing_twice_in_one_tabs_tree_is_reported() {
    // The multi-layout check also catches a pane that is a leaf twice inside a
    // *single* tab's tree, not only one split across two tabs. Both entries name
    // the same tab id.
    let mut session = build_empty_session();
    let doubled_pane_id = register_live_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "code".to_owned(), 0, doubled_pane_id);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(doubled_pane_id),
            LayoutNode::Pane(doubled_pane_id),
        ],
    )));
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::PaneInMultipleLayouts {
            pane_id: doubled_pane_id,
            tab_ids: vec![tab_id, tab_id],
        }])
    );
}

#[test]
fn an_exited_orphan_record_is_reported() {
    // The orphan check covers `Exited` records, not just live ones: a dead
    // placeholder pane that is a leaf nowhere is reported, and the reported
    // lifecycle is the exact `Exited` state it holds.
    let mut session = build_empty_session();
    let orphan_pane_id = register_exited_pane(&mut session);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::OrphanedPaneRecord {
            pane_id: orphan_pane_id,
            pane_lifecycle: PaneLifecycle::Exited {
                exit_code: Some(0),
                exited_at: SystemTime::UNIX_EPOCH,
            },
        }])
    );
}

#[test]
fn every_violation_is_collected_in_one_pass() {
    let mut session = build_empty_session();
    // A layout leaf with no record.
    let ghost_pane_id = PaneId::new();
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, ghost_pane_id);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);
    // A live record that is a leaf nowhere.
    let orphan_pane_id = register_live_pane(&mut session);

    // Both faults surface from a single call.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::PaneNotInRegistry {
                tab_id,
                pane_id: ghost_pane_id,
            },
            SessionConsistencyError::OrphanedPaneRecord {
                pane_id: orphan_pane_id,
                pane_lifecycle: PaneLifecycle::Running,
            },
        ])
    );
}

#[test]
fn validate_reports_two_orphan_records_in_id_order() {
    // The registry walks in id order: two faults of one kind come out the same
    // way round.
    let mut session = build_empty_session();
    let first_registered_pane_id = register_live_pane(&mut session);
    let second_registered_pane_id = register_live_pane(&mut session);
    let (lower_pane_id, higher_pane_id) = if first_registered_pane_id < second_registered_pane_id {
        (first_registered_pane_id, second_registered_pane_id)
    } else {
        (second_registered_pane_id, first_registered_pane_id)
    };

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::OrphanedPaneRecord {
                pane_id: lower_pane_id,
                pane_lifecycle: PaneLifecycle::Running,
            },
            SessionConsistencyError::OrphanedPaneRecord {
                pane_id: higher_pane_id,
                pane_lifecycle: PaneLifecycle::Running,
            },
        ])
    );
}

#[test]
fn a_restored_focus_history_longer_than_the_cap_still_evicts() {
    // The length is compared in `usize`: a history of 65 536 entries is over
    // the cap and is cut back to it.
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let mut serialized_tab = serde_json::to_value(&tab).expect("a tab serializes");
    let oversized_focus_history: Vec<PaneId> = (0..65_536).map(|_| PaneId::new()).collect();
    serialized_tab["focus_mru"] =
        serde_json::to_value(&oversized_focus_history).expect("the history serializes");
    let mut restored_tab: Tab =
        serde_json::from_value(serialized_tab).expect("the tab deserializes");

    restored_tab.record_focus_mru(PaneId::new());

    assert_eq!(
        restored_tab.list_focus_mru().len(),
        usize::from(MAX_TAB_FOCUS_MRU_ENTRY_COUNT)
    );
}

/// A floating member for `pane_id` asking for the default size, solved to
/// `48x13`.
pub(crate) fn build_default_floating_member(pane_id: PaneId) -> FloatingMember {
    FloatingMember {
        pane_id,
        desired_size: koshi_core::geometry::DEFAULT_FLOATING_PANE_SIZE,
        solved_size: FloatingPaneSizeSolve::Sized(Size {
            column_count: 48,
            row_count: 13,
        }),
    }
}

/// A floating member for `pane_id` asking for 40x10 cells and solved to 40x10.
fn build_floating_member(pane_id: PaneId) -> FloatingMember {
    FloatingMember {
        pane_id,
        desired_size: FloatingPaneSize {
            width: FloatingPaneDimension::Cells(NonZeroU16::new(40).expect("40 is nonzero")),
            height: FloatingPaneDimension::Cells(NonZeroU16::new(10).expect("10 is nonzero")),
        },
        solved_size: FloatingPaneSizeSolve::Sized(Size {
            column_count: 40,
            row_count: 10,
        }),
    }
}

/// A `Running` pane registered in `session` and added as its newest floating
/// member, returned by id.
fn register_floating_pane(session: &mut Session) -> PaneId {
    let pane_id = register_live_pane(session);
    session
        .floating_set
        .add_member(build_floating_member(pane_id))
        .expect("the floating set has room for a fresh pane");
    pane_id
}

/// The attached client `client_id` of `session`, for in-place edits.
fn get_attached_client_mut(session: &mut Session, client_id: ClientId) -> &mut Client {
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client is attached")
}

/// Re-decode the attached client `client_id` of `session` from its JSON, with
/// each `(field_name, field_json)` pair replacing that field.
fn replace_client_fields(
    session: &mut Session,
    client_id: ClientId,
    replaced_fields: &[(&str, serde_json::Value)],
) {
    let client = session
        .clients
        .detach_client(client_id)
        .expect("the client is attached");
    let mut client_json = serde_json::to_value(&client).expect("the client encodes");
    for (field_name, field_json) in replaced_fields {
        client_json[*field_name] = field_json.clone();
    }
    session
        .clients
        .attach_client(serde_json::from_value(client_json).expect("the client decodes"));
}

#[test]
fn add_member_appends_floating_panes_in_creation_order() {
    let mut floating_set = FloatingSet::default();
    let pane_ids = [PaneId::new(), PaneId::new(), PaneId::new()];

    for pane_id in pane_ids {
        floating_set
            .add_member(build_floating_member(pane_id))
            .expect("the floating set has room");
    }

    assert_eq!(
        floating_set.list_members(),
        pane_ids.map(build_floating_member)
    );
}

#[test]
fn has_pane_finds_each_member_and_no_other_pane() {
    let mut floating_set = FloatingSet::default();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    assert!(!floating_set.has_pane(first_pane_id));

    for pane_id in [first_pane_id, second_pane_id] {
        floating_set
            .add_member(build_floating_member(pane_id))
            .expect("the floating set has room");
    }

    assert!(floating_set.has_pane(first_pane_id));
    assert!(floating_set.has_pane(second_pane_id));
    assert!(!floating_set.has_pane(PaneId::new()));
}

#[test]
fn add_member_refuses_a_pane_the_set_already_holds() {
    let mut floating_set = FloatingSet::default();
    let pane_id = PaneId::new();
    floating_set
        .add_member(build_floating_member(pane_id))
        .expect("the floating set is empty");
    let mut repeated_member = build_floating_member(pane_id);
    repeated_member.solved_size = FloatingPaneSizeSolve::Sized(Size {
        column_count: 20,
        row_count: 5,
    });

    assert_eq!(
        floating_set.add_member(repeated_member),
        Err(FloatingSetError::DuplicatePane { pane_id })
    );
    assert_eq!(
        floating_set.list_members(),
        [build_floating_member(pane_id)]
    );
}

#[test]
fn add_member_refuses_a_member_past_the_session_limit() {
    let mut floating_set = FloatingSet::default();
    let pane_ids: Vec<PaneId> = (0..MAX_FLOATING_PANES_PER_SESSION)
        .map(|_| PaneId::new())
        .collect();
    for &pane_id in &pane_ids {
        assert!(!floating_set.is_full());
        floating_set
            .add_member(build_floating_member(pane_id))
            .expect("the floating set has room");
    }

    assert!(floating_set.is_full());
    assert_eq!(
        floating_set.add_member(build_floating_member(PaneId::new())),
        Err(FloatingSetError::TooManyPanes)
    );
    assert_eq!(
        floating_set.add_member(build_floating_member(pane_ids[0])),
        Err(FloatingSetError::DuplicatePane {
            pane_id: pane_ids[0]
        })
    );
    assert_eq!(
        floating_set
            .list_members()
            .iter()
            .map(|floating_member| floating_member.pane_id)
            .collect::<Vec<PaneId>>(),
        pane_ids
    );
}

#[test]
fn a_floating_set_survives_a_serde_round_trip() {
    let mut floating_set = FloatingSet::default();
    let (cell_sized_pane_id, percent_sized_pane_id) = (PaneId::new(), PaneId::new());
    floating_set
        .add_member(build_floating_member(cell_sized_pane_id))
        .expect("the floating set is empty");
    floating_set
        .add_member(FloatingMember {
            pane_id: percent_sized_pane_id,
            desired_size: FloatingPaneSize {
                width: FloatingPaneDimension::Percent(
                    AxisPercent::try_from(60).expect("60 is a percent"),
                ),
                height: FloatingPaneDimension::Percent(
                    AxisPercent::try_from(100).expect("100 is a percent"),
                ),
            },
            solved_size: FloatingPaneSizeSolve::Suppressed,
        })
        .expect("the floating set has room");

    let floating_set_json = serde_json::to_value(&floating_set).expect("the set encodes");

    assert_eq!(
        floating_set_json,
        serde_json::json!({"members": [
            {
                "pane_id": cell_sized_pane_id,
                "desired_size": {"width": {"Cells": 40}, "height": {"Cells": 10}},
                "solved_size": {"Sized": {"column_count": 40, "row_count": 10}}
            },
            {
                "pane_id": percent_sized_pane_id,
                "desired_size": {"width": {"Percent": 60}, "height": {"Percent": 100}},
                "solved_size": "Suppressed"
            }
        ]})
    );
    assert_eq!(
        serde_json::from_value::<FloatingSet>(floating_set_json).expect("the set decodes"),
        floating_set
    );
}

/// `cell_count` cells on one axis.
fn build_cells_dimension(cell_count: u16) -> FloatingPaneDimension {
    FloatingPaneDimension::Cells(NonZeroU16::new(cell_count).expect("a nonzero cell count"))
}

/// `percent` percent of one axis.
fn build_percent_dimension(percent: u8) -> FloatingPaneDimension {
    FloatingPaneDimension::Percent(AxisPercent::try_from(percent).expect("a percent from 1 to 100"))
}

/// A floating set holding one fresh pane per `(width, height)` pair, in that
/// order, each last solved as suppressed.
fn build_floating_set_asking_for(
    desired_dimensions: &[(FloatingPaneDimension, FloatingPaneDimension)],
) -> FloatingSet {
    let mut floating_set = FloatingSet::default();
    for &(width, height) in desired_dimensions {
        floating_set
            .add_member(FloatingMember {
                pane_id: PaneId::new(),
                desired_size: FloatingPaneSize { width, height },
                solved_size: FloatingPaneSizeSolve::Suppressed,
            })
            .expect("the floating set has room");
    }
    floating_set
}

/// The solved size of every member of `floating_set`, in creation order.
fn list_solved_sizes(floating_set: &FloatingSet) -> Vec<FloatingPaneSizeSolve> {
    floating_set
        .list_members()
        .iter()
        .map(|floating_member| floating_member.solved_size)
        .collect()
}

#[test]
fn update_member_sizes_resolves_a_percent_against_the_shared_viewport_rounding_down() {
    let mut floating_set = build_floating_set_asking_for(&[(
        build_percent_dimension(60),
        build_percent_dimension(60),
    )]);

    floating_set.update_member_sizes(
        Size {
            column_count: 80,
            row_count: 22,
        },
        Size {
            column_count: 2,
            row_count: 1,
        },
    );

    assert_eq!(
        list_solved_sizes(&floating_set),
        [FloatingPaneSizeSolve::Sized(Size {
            column_count: 48,
            row_count: 13,
        })]
    );
}

#[test]
fn update_member_sizes_cuts_each_member_to_the_shared_viewport_and_raises_it_to_the_minimum() {
    let mut floating_set = build_floating_set_asking_for(&[
        (build_cells_dimension(200), build_cells_dimension(50)),
        (build_cells_dimension(10), build_cells_dimension(3)),
        (build_percent_dimension(1), build_percent_dimension(100)),
    ]);

    // `pane { min-cols 20 min-rows 6 }`: the floating minimum is 22x10.
    floating_set.update_member_sizes(
        Size {
            column_count: 80,
            row_count: 22,
        },
        Size {
            column_count: 20,
            row_count: 6,
        },
    );

    assert_eq!(
        list_solved_sizes(&floating_set),
        [
            FloatingPaneSizeSolve::Sized(Size {
                column_count: 80,
                row_count: 22,
            }),
            FloatingPaneSizeSolve::Sized(Size {
                column_count: 22,
                row_count: 10,
            }),
            FloatingPaneSizeSolve::Sized(Size {
                column_count: 22,
                row_count: 22,
            }),
        ]
    );
}

#[test]
fn update_member_sizes_fits_at_the_floating_minimum_and_suppresses_below_it_on_either_axis() {
    let mut floating_set = build_floating_set_asking_for(&[
        (build_percent_dimension(60), build_percent_dimension(60)),
        (build_cells_dimension(30), build_cells_dimension(12)),
    ]);
    let pane_minimum_size = Size {
        column_count: 20,
        row_count: 6,
    };

    floating_set.update_member_sizes(
        Size {
            column_count: 22,
            row_count: 10,
        },
        pane_minimum_size,
    );
    let floating_pane_minimum = FloatingPaneSizeSolve::Sized(Size {
        column_count: 22,
        row_count: 10,
    });
    assert_eq!(
        list_solved_sizes(&floating_set),
        [floating_pane_minimum, floating_pane_minimum]
    );

    floating_set.update_member_sizes(
        Size {
            column_count: 21,
            row_count: 30,
        },
        pane_minimum_size,
    );
    assert_eq!(
        list_solved_sizes(&floating_set),
        [
            FloatingPaneSizeSolve::Suppressed,
            FloatingPaneSizeSolve::Suppressed
        ]
    );

    floating_set.update_member_sizes(
        Size {
            column_count: 30,
            row_count: 9,
        },
        pane_minimum_size,
    );
    assert_eq!(
        list_solved_sizes(&floating_set),
        [
            FloatingPaneSizeSolve::Suppressed,
            FloatingPaneSizeSolve::Suppressed
        ]
    );
}

#[test]
fn update_member_sizes_restores_the_desired_size_after_a_shrink_and_a_regrow() {
    let desired_size = FloatingPaneSize {
        width: build_percent_dimension(60),
        height: build_percent_dimension(60),
    };
    let mut floating_set =
        build_floating_set_asking_for(&[(desired_size.width, desired_size.height)]);
    let roomy_viewport = Size {
        column_count: 80,
        row_count: 22,
    };
    let pane_minimum_size = Size {
        column_count: 20,
        row_count: 6,
    };
    let solved_on_roomy_viewport = FloatingPaneSizeSolve::Sized(Size {
        column_count: 48,
        row_count: 13,
    });

    floating_set.update_member_sizes(roomy_viewport, pane_minimum_size);
    assert_eq!(list_solved_sizes(&floating_set), [solved_on_roomy_viewport]);
    floating_set.update_member_sizes(
        Size {
            column_count: 21,
            row_count: 30,
        },
        pane_minimum_size,
    );
    assert_eq!(
        list_solved_sizes(&floating_set),
        [FloatingPaneSizeSolve::Suppressed]
    );
    floating_set.update_member_sizes(roomy_viewport, pane_minimum_size);

    assert_eq!(list_solved_sizes(&floating_set), [solved_on_roomy_viewport]);
    assert_eq!(floating_set.list_members()[0].desired_size, desired_size);
}

#[test]
fn update_member_sizes_suppresses_every_member_when_the_floating_minimum_passes_u16_max() {
    let mut floating_set = build_floating_set_asking_for(&[
        (build_cells_dimension(40), build_cells_dimension(10)),
        (build_percent_dimension(100), build_percent_dimension(100)),
    ]);

    // 65534 content columns plus 2 chrome columns need 65536 columns.
    floating_set.update_member_sizes(
        Size {
            column_count: u16::MAX,
            row_count: 22,
        },
        Size {
            column_count: u16::MAX - 1,
            row_count: 6,
        },
    );

    assert_eq!(
        list_solved_sizes(&floating_set),
        [
            FloatingPaneSizeSolve::Suppressed,
            FloatingPaneSizeSolve::Suppressed
        ]
    );
}

#[test]
fn update_member_sizes_fits_a_floating_minimum_of_exactly_u16_max_columns() {
    let mut floating_set = build_floating_set_asking_for(&[
        (build_cells_dimension(40), build_cells_dimension(10)),
        (build_percent_dimension(100), build_percent_dimension(100)),
    ]);

    // 65533 content columns plus 2 chrome columns need 65535 columns.
    floating_set.update_member_sizes(
        Size {
            column_count: u16::MAX,
            row_count: 22,
        },
        Size {
            column_count: u16::MAX - 2,
            row_count: 6,
        },
    );

    assert_eq!(
        list_solved_sizes(&floating_set),
        [
            FloatingPaneSizeSolve::Sized(Size {
                column_count: u16::MAX,
                row_count: 10,
            }),
            FloatingPaneSizeSolve::Sized(Size {
                column_count: u16::MAX,
                row_count: 22,
            })
        ]
    );
}

#[test]
fn remove_floating_member_keeps_the_order_of_the_rest_and_clears_every_client_view() {
    let mut session = build_empty_session();
    let removed_pane_id = register_floating_pane(&mut session);
    let middle_pane_id = register_floating_pane(&mut session);
    let last_pane_id = register_floating_pane(&mut session);
    let tab_id = TabId::new();
    let focusing_client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let pinning_client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let focusing_client = get_attached_client_mut(&mut session, focusing_client_id);
    assert!(focusing_client.focus_floating_pane(middle_pane_id));
    assert!(focusing_client.focus_floating_pane(removed_pane_id));
    let pinning_client = get_attached_client_mut(&mut session, pinning_client_id);
    assert!(pinning_client.focus_floating_pane(removed_pane_id));
    pinning_client.pin_floating_pane(removed_pane_id, Point { column: 3, row: 4 });
    pinning_client.minimize_floating_pane(last_pane_id);

    assert_eq!(
        session.remove_floating_member(removed_pane_id),
        Some(build_floating_member(removed_pane_id))
    );

    assert_eq!(
        session.floating_set.list_members(),
        [
            build_floating_member(middle_pane_id),
            build_floating_member(last_pane_id)
        ]
    );
    let focusing_client = get_attached_client_mut(&mut session, focusing_client_id);
    assert_eq!(
        focusing_client.list_floating_pane_focus_order(),
        [middle_pane_id]
    );
    assert_eq!(focusing_client.get_focused_floating_pane_id(), None);
    let pinning_client = get_attached_client_mut(&mut session, pinning_client_id);
    assert_eq!(
        pinning_client.list_floating_pane_focus_order(),
        Vec::<PaneId>::new()
    );
    assert_eq!(pinning_client.get_focused_floating_pane_id(), None);
    assert_eq!(
        pinning_client.list_floating_pane_views(),
        &HashMap::from([(
            last_pane_id,
            FloatingPaneView {
                position: FloatingPanePosition::Default,
                is_minimized: true,
            },
        )])
    );
}

#[test]
fn remove_floating_member_of_a_pane_that_is_not_floating_changes_nothing() {
    let mut session = build_empty_session();
    let floating_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    assert!(get_attached_client_mut(&mut session, client_id).focus_floating_pane(floating_pane_id));

    assert_eq!(session.remove_floating_member(PaneId::new()), None);

    assert_eq!(
        session.floating_set.list_members(),
        [build_floating_member(floating_pane_id)]
    );
    let client = get_attached_client_mut(&mut session, client_id);
    assert_eq!(client.list_floating_pane_focus_order(), [floating_pane_id]);
    assert_eq!(
        client.get_focused_floating_pane_id(),
        Some(floating_pane_id)
    );
}

#[test]
fn a_floating_member_with_no_layout_leaf_is_consistent() {
    let mut session = build_empty_session();
    let floating_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    let client = get_attached_client_mut(&mut session, client_id);
    assert!(client.focus_floating_pane(floating_pane_id));
    assert!(client.set_floating_pane_position(floating_pane_id, Point { column: 2, row: 1 }));

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_floating_member_with_no_registry_record_is_reported() {
    let mut session = build_empty_session();
    let ghost_pane_id = PaneId::new();
    session
        .floating_set
        .add_member(build_floating_member(ghost_pane_id))
        .expect("the floating set is empty");

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FloatingPaneNotInRegistry {
            pane_id: ghost_pane_id
        }])
    );
}

#[test]
fn a_removed_pane_left_in_the_floating_set_is_reported() {
    let mut session = build_empty_session();
    let removed_pane_id = register_removed_pane(&mut session);
    session
        .floating_set
        .add_member(build_floating_member(removed_pane_id))
        .expect("the floating set is empty");

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::RemovedPaneInFloatingSet {
                pane_id: removed_pane_id
            },
            SessionConsistencyError::LingeringRemovedRecord {
                pane_id: removed_pane_id
            },
        ])
    );
}

#[test]
fn a_pane_listed_twice_in_the_floating_set_is_reported() {
    let mut session = build_empty_session();
    let repeated_pane_id = register_live_pane(&mut session);
    let repeated_member_json =
        serde_json::to_value(build_floating_member(repeated_pane_id)).expect("the member encodes");
    session.floating_set = serde_json::from_value(serde_json::json!({
        "members": [repeated_member_json.clone(), repeated_member_json]
    }))
    .expect("a saved floating set decodes");

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::DuplicateFloatingPane {
            pane_id: repeated_pane_id
        }])
    );
}

#[test]
fn a_floating_set_past_the_session_limit_is_reported_and_one_at_the_limit_is_not() {
    let mut session = build_empty_session();
    let mut member_jsons: Vec<serde_json::Value> = (0..MAX_FLOATING_PANES_PER_SESSION)
        .map(|_| {
            serde_json::to_value(build_floating_member(register_live_pane(&mut session)))
                .expect("the member encodes")
        })
        .collect();
    session.floating_set =
        serde_json::from_value(serde_json::json!({ "members": member_jsons.clone() }))
            .expect("a saved floating set at the limit decodes");
    assert_eq!(session.validate_session_consistency(), Ok(()));

    member_jsons.push(
        serde_json::to_value(build_floating_member(register_live_pane(&mut session)))
            .expect("the member encodes"),
    );
    session.floating_set = serde_json::from_value(serde_json::json!({ "members": member_jsons }))
        .expect("a saved floating set past the limit decodes");

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::TooManyFloatingPanes {
            member_count: MAX_FLOATING_PANES_PER_SESSION + 1
        }])
    );
}

#[test]
fn a_floating_member_that_is_also_a_layout_leaf_is_reported() {
    let mut session = build_empty_session();
    let pane_id = register_floating_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, pane_id);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FloatingPaneInLayout {
            pane_id,
            tab_ids: vec![tab_id],
        }])
    );
}

#[test]
fn client_views_of_panes_that_are_not_floating_are_reported_in_pane_id_order() {
    let mut session = build_empty_session();
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    let mut stray_pane_ids: [PaneId; 8] = std::array::from_fn(|_| PaneId::new());
    let client = get_attached_client_mut(&mut session, client_id);
    for stray_pane_id in stray_pane_ids {
        client.pin_floating_pane(stray_pane_id, Point { column: 0, row: 0 });
    }
    stray_pane_ids.sort();

    assert_eq!(
        session.validate_session_consistency(),
        Err(Vec::from(stray_pane_ids.map(|stray_pane_id| {
            SessionConsistencyError::FloatingViewTargetMissing {
                client_id,
                pane_id: stray_pane_id,
            }
        })))
    );
}

#[test]
fn a_floating_focus_order_entry_naming_a_pane_that_is_not_floating_is_reported() {
    let mut session = build_empty_session();
    let floating_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    let stray_pane_id = PaneId::new();
    let client = get_attached_client_mut(&mut session, client_id);
    assert!(client.focus_floating_pane(stray_pane_id));
    assert!(client.focus_floating_pane(floating_pane_id));

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FloatingFocusOrderTargetMissing {
                client_id,
                pane_id: stray_pane_id,
            }
        ])
    );
}

#[test]
fn each_repeat_of_a_floating_focus_order_entry_is_reported() {
    let mut session = build_empty_session();
    let floating_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    replace_client_fields(
        &mut session,
        client_id,
        &[(
            "floating_pane_focus_order",
            serde_json::json!([floating_pane_id, floating_pane_id, floating_pane_id]),
        )],
    );

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::DuplicateFloatingFocusOrderEntry {
                client_id,
                pane_id: floating_pane_id,
            },
            SessionConsistencyError::DuplicateFloatingFocusOrderEntry {
                client_id,
                pane_id: floating_pane_id,
            },
        ])
    );
}

#[test]
fn a_floating_focus_on_a_pane_that_is_not_floating_is_reported() {
    let mut session = build_empty_session();
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    let stray_pane_id = PaneId::new();
    assert!(get_attached_client_mut(&mut session, client_id).focus_floating_pane(stray_pane_id));

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FloatingFocusOrderTargetMissing {
                client_id,
                pane_id: stray_pane_id,
            },
            SessionConsistencyError::FocusedFloatingPaneMissing {
                client_id,
                pane_id: stray_pane_id,
            },
        ])
    );
}

#[test]
fn a_floating_focus_that_is_not_last_in_the_focus_order_is_reported() {
    let mut session = build_empty_session();
    let focused_pane_id = register_floating_pane(&mut session);
    let top_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    replace_client_fields(
        &mut session,
        client_id,
        &[
            (
                "floating_pane_focus_order",
                serde_json::json!([focused_pane_id, top_pane_id]),
            ),
            (
                "focused_floating_pane_id",
                serde_json::json!(focused_pane_id),
            ),
        ],
    );

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FocusedFloatingPaneNotOnTop {
            client_id,
            pane_id: focused_pane_id,
        }])
    );
}

#[test]
fn a_floating_focus_on_a_pane_the_client_minimized_is_reported() {
    let mut session = build_empty_session();
    let minimized_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new(), 80, 24);
    let client = get_attached_client_mut(&mut session, client_id);
    assert!(client.focus_floating_pane(minimized_pane_id));
    client.minimize_floating_pane(minimized_pane_id);
    replace_client_fields(
        &mut session,
        client_id,
        &[(
            "focused_floating_pane_id",
            serde_json::json!(minimized_pane_id),
        )],
    );

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FocusedFloatingPaneMinimized {
                client_id,
                pane_id: minimized_pane_id,
            }
        ])
    );
}

#[test]
fn client_focuses_and_zooms_in_missing_tabs_are_reported_in_tab_id_order() {
    let mut session = build_empty_session();
    let pane_id = register_live_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, pane_id);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);
    let client_id = attach_viewer(&mut session, tab_id, 80, 24);
    let mut missing_tab_ids: [TabId; 8] = std::array::from_fn(|_| TabId::new());
    let client = get_attached_client_mut(&mut session, client_id);
    for missing_tab_id in missing_tab_ids {
        client.update_focused_pane(missing_tab_id, pane_id);
        client.zoom_pane(missing_tab_id, pane_id);
    }
    missing_tab_ids.sort();

    let missing_focus_tabs =
        missing_tab_ids.map(|missing_tab_id| SessionConsistencyError::FocusTabMissing {
            client_id,
            tab_id: missing_tab_id,
        });
    let missing_zoom_targets =
        missing_tab_ids.map(
            |missing_tab_id| SessionConsistencyError::ZoomTargetMissing {
                client_id,
                tab_id: missing_tab_id,
                pane_id,
            },
        );
    assert_eq!(
        session.validate_session_consistency(),
        Err(missing_focus_tabs
            .into_iter()
            .chain(missing_zoom_targets)
            .collect())
    );
}

#[test]
fn a_session_json_without_a_floating_set_is_refused() {
    let mut session_json =
        serde_json::to_value(build_empty_session()).expect("the session encodes");
    session_json
        .as_object_mut()
        .expect("a session encodes as a json object")
        .remove("floating_set")
        .expect("the encoded session carries its floating set");

    assert_eq!(
        serde_json::from_value::<Session>(session_json)
            .expect_err("a session without a floating set is refused")
            .to_string(),
        "missing field `floating_set`"
    );
}

/// A floating pane size of `column_count` by `row_count` cells.
fn build_cells_size(column_count: u16, row_count: u16) -> FloatingPaneSize {
    FloatingPaneSize {
        width: FloatingPaneDimension::Cells(
            NonZeroU16::new(column_count).expect("a nonzero column count"),
        ),
        height: FloatingPaneDimension::Cells(
            NonZeroU16::new(row_count).expect("a nonzero row count"),
        ),
    }
}

#[test]
fn solve_floating_pane_size_cuts_to_the_viewport_and_raises_to_the_floating_minimum() {
    let shared_floating_viewport = Size {
        column_count: 80,
        row_count: 22,
    };
    let pane_minimum_size = Size {
        column_count: 2,
        row_count: 1,
    };
    let sixty_percent =
        FloatingPaneDimension::Percent(AxisPercent::try_from(60).expect("60 is a percent"));

    assert_eq!(
        solve_floating_pane_size(
            FloatingPaneSize {
                width: sixty_percent,
                height: sixty_percent,
            },
            shared_floating_viewport,
            pane_minimum_size,
        ),
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 48,
            row_count: 13,
        })
    );
    assert_eq!(
        solve_floating_pane_size(
            build_cells_size(100, 30),
            shared_floating_viewport,
            pane_minimum_size,
        ),
        FloatingPaneSizeSolve::Sized(shared_floating_viewport)
    );
    assert_eq!(
        solve_floating_pane_size(
            build_cells_size(1, 1),
            shared_floating_viewport,
            pane_minimum_size,
        ),
        FloatingPaneSizeSolve::Sized(Size {
            column_count: 4,
            row_count: 5,
        })
    );
    assert_eq!(
        solve_floating_pane_size(
            build_cells_size(40, 10),
            Size {
                column_count: 3,
                row_count: 22,
            },
            pane_minimum_size,
        ),
        FloatingPaneSizeSolve::Suppressed
    );
}

#[test]
fn update_member_desired_size_changes_a_member_and_refuses_any_other_pane() {
    let mut session = build_empty_session();
    let pane_id = register_floating_pane(&mut session);
    let resized_desired_size = build_cells_size(43, 10);

    assert!(session
        .floating_set
        .update_member_desired_size(pane_id, resized_desired_size));
    let resized_member = FloatingMember {
        desired_size: resized_desired_size,
        ..build_floating_member(pane_id)
    };
    assert_eq!(session.floating_set.list_members(), [resized_member]);

    assert!(!session
        .floating_set
        .update_member_desired_size(PaneId::new(), build_cells_size(9, 9)));
    assert_eq!(session.floating_set.list_members(), [resized_member]);
}
