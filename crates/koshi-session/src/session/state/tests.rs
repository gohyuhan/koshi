//! Tests for [`Session`] helpers.

use super::*;
use std::time::SystemTime;

use koshi_core::command::{GridPosition, Selection, SelectionKind};
use koshi_core::geometry::{PaneArea, SplitDirection};
use koshi_core::lock::LockMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::ClientOrigin;

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
/// area.
fn attach_viewer(session: &mut Session, tab_id: TabId, column_count: u16, row_count: u16) {
    attach_viewer_with_pane_area(session, tab_id, column_count, row_count, None);
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
