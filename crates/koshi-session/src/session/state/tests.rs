//! Tests for [`Session`] helpers.

use super::*;
use std::time::SystemTime;

use koshi_core::command::{GridPosition, Selection, SelectionKind};
use koshi_core::geometry::{PaneArea, SplitDirection};
use koshi_core::lock::LockMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::ClientOrigin;

/// Attach a client viewing `tab` with the given viewport, reporting no pane
/// area.
fn attach_viewer(session: &mut Session, tab_id: TabId, column_count: u16, row_count: u16) {
    attach_viewer_with_pane_area(session, tab_id, column_count, row_count, None);
}

/// Attach a client viewing `tab` with the given viewport, reporting
/// `pane_area`, and return the id it attached under.
fn attach_viewer_with_pane_area(
    session: &mut Session,
    tab_id: TabId,
    column_count: u16,
    row_count: u16,
    pane_area: Option<PaneArea>,
) -> ClientId {
    let client_id = ClientId::new();
    let client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count,
            row_count,
        },
        pane_area,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);
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
fn tab_viewport_takes_the_per_axis_minimum_across_viewers() {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    // Two clients view `tab` with opposite aspect ratios.
    attach_viewer(&mut session, tab, 80, 5);
    attach_viewer(&mut session, tab, 40, 24);
    // A client on a different tab must not count.
    attach_viewer(&mut session, other_tab, 10, 1);

    // Full-viewport minimum is 40×5; reserving two chrome rows leaves 40×3.
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 40,
            row_count: 3
        })
    );
}

#[test]
fn tab_viewport_is_none_without_a_attach_viewer() {
    let tab = TabId::new();
    let session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(session.get_tab_viewport(tab), None);
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
fn tab_viewport_with_exactly_one_viewer_returns_its_own_reserved_size() {
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    attach_viewer(&mut session, tab, 100, 30);

    // With one viewer the result is that viewer's own size minus the two
    // chrome rows.
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 100,
            row_count: 28
        })
    );
}

#[test]
fn tab_viewport_saturates_rather_than_panics_below_the_chrome_rows() {
    // A viewport with fewer rows than the two reserved chrome rows saturates
    // the row count at `0`.
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    attach_viewer(&mut session, tab, 80, 1);

    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 0
        })
    );
}

#[test]
fn tab_viewport_leaves_a_starving_viewer_out() {
    let tab = TabId::new();
    let mut session = build_empty_session();

    attach_viewer(&mut session, tab, 80, 24);
    attach_viewer_with_pane_area(&mut session, tab, 80, 24, Some(PaneArea::Starving));

    // Only the viewer that reported a pane area counts: 80x24 minus the two
    // chrome rows.
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn tab_viewport_with_a_zero_reported_area_is_zero() {
    let tab = TabId::new();
    let mut session = build_empty_session();

    attach_viewer(&mut session, tab, 80, 24);
    attach_viewer_with_pane_area(
        &mut session,
        tab,
        80,
        24,
        Some(PaneArea::Reported(Size {
            column_count: 0,
            row_count: 0,
        })),
    );

    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 0,
            row_count: 0
        })
    );
}

#[test]
fn tab_viewport_is_none_when_every_viewer_is_starving() {
    let tab = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(&mut session, tab, 80, 24, Some(PaneArea::Starving));
    attach_viewer_with_pane_area(&mut session, tab, 80, 24, Some(PaneArea::Starving));

    assert_eq!(session.get_tab_viewport(tab), None);
}

#[test]
fn tab_viewport_takes_the_minimum_of_a_reported_and_an_unreported_attach_viewer() {
    let tab = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 60,
            row_count: 30,
        })),
    );
    attach_viewer(&mut session, tab, 80, 24);

    // 60x30 reported against 80x22 from the unreported viewer.
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 60,
            row_count: 22
        })
    );
}

#[test]
fn tab_viewport_takes_the_element_wise_minimum_of_two_reports() {
    let tab = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 100,
            row_count: 20,
        })),
    );
    attach_viewer_with_pane_area(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 60,
            row_count: 30,
        })),
    );

    // The narrower report gives the columns, the shorter one gives the rows.
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 60,
            row_count: 20
        })
    );
}

#[test]
fn tab_viewport_does_not_depend_on_where_the_starving_viewer_attached() {
    let tab = TabId::new();

    let mut starving_first = build_empty_session();
    attach_viewer_with_pane_area(&mut starving_first, tab, 80, 24, Some(PaneArea::Starving));
    attach_viewer(&mut starving_first, tab, 80, 24);

    let mut starving_second = build_empty_session();
    attach_viewer(&mut starving_second, tab, 80, 24);
    attach_viewer_with_pane_area(&mut starving_second, tab, 80, 24, Some(PaneArea::Starving));

    assert_eq!(
        starving_first.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
    assert_eq!(
        starving_second.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn detaching_a_starving_viewer_leaves_tab_viewport_unchanged() {
    let tab = TabId::new();
    let mut session = build_empty_session();
    attach_viewer(&mut session, tab, 80, 24);
    let starving =
        attach_viewer_with_pane_area(&mut session, tab, 80, 24, Some(PaneArea::Starving));
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );

    session.detach_client(starving);

    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 22
        })
    );
}

#[test]
fn detaching_the_smallest_viewer_lets_tab_viewport_grow() {
    let tab = TabId::new();
    let mut session = build_empty_session();
    let smallest = attach_viewer_with_pane_area(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size {
            column_count: 60,
            row_count: 30,
        })),
    );
    attach_viewer(&mut session, tab, 120, 40);
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 60,
            row_count: 30
        })
    );

    session.detach_client(smallest);

    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 120,
            row_count: 38
        })
    );
}

#[test]
fn attach_client_returns_the_client_it_displaced_on_reattach() {
    // A re-attach under the same id replaces the pane record in place and returns
    // the one it displaced.
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let client_id = ClientId::new();
    let initial_client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 0,
            row_count: 0,
        },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    assert_eq!(
        session
            .attach_client(initial_client)
            .map(|client| client.get_client_id()),
        None
    );

    let reattached_client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let displaced = session.attach_client(reattached_client);

    assert_eq!(
        displaced.map(|client| client.get_viewport_size()),
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
    assert_eq!(session.clients.client_count(), 1);
}

#[test]
fn attaching_a_client_before_any_tab_leaves_the_session_starting() {
    // `ClientAttached` moves only a `Detaching` session to `Running`. A session
    // that has not created its first tab is `Starting` and rejects the event.
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);

    let client = Client::from_attachment(
        ClientId::new(),
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 0,
            row_count: 0,
        },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detach_client_returns_the_exact_record_it_removed() {
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let client_id = ClientId::new();
    let client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 12,
            row_count: 3,
        },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);

    let removed = session.detach_client(client_id);

    assert_eq!(
        removed.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .map(Client::get_client_id),
        None
    );
    assert_eq!(session.clients.client_count(), 0);
}

#[test]
fn detach_client_on_an_unattached_id_returns_none() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(
        session
            .detach_client(ClientId::new())
            .map(|client| client.get_client_id()),
        None
    );
}

#[test]
fn request_session_stop_is_idempotent_once_already_stopping() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

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
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    session.complete_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_the_last_client_of_a_stopping_session_does_not_revert_it() {
    // A `Stopping` session that loses its last client stays `Stopping`; it does
    // not fall back to `Detaching`.
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let client_id = ClientId::new();
    let client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 0,
            row_count: 0,
        },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);
    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    let removed = session.detach_client(client_id);

    assert_eq!(
        removed.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn detaching_the_last_client_of_a_starting_session_leaves_it_starting() {
    // `LastClientDetached` moves only a `Running` session to `Detaching`. A
    // session that has not created its first tab is `Starting` and rejects it.
    let tab = TabId::new();
    let mut session = build_empty_session();
    let client_id = attach_viewer_with_pane_area(&mut session, tab, 80, 24, None);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);

    let removed = session.detach_client(client_id);

    assert_eq!(
        removed.map(|client| client.get_client_id()),
        Some(client_id)
    );
    assert_eq!(session.clients.client_count(), 0);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_again_with_no_clients_left_keeps_the_session_detaching() {
    let tab = TabId::new();
    let mut session = build_empty_session();
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");
    let client_id = attach_viewer_with_pane_area(&mut session, tab, 80, 24, None);

    session.detach_client(client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);

    // The registry is already empty, so the second detach fires
    // `LastClientDetached` again; `Detaching` rejects it and does not move.
    let removed = session.detach_client(ClientId::new());

    assert_eq!(removed.map(|client| client.get_client_id()), None);
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

/// Drive `session` to `Detaching`: create its first tab, attach one viewer of
/// `tab`, then detach it.
fn detached_session(tab: TabId) -> Session {
    let mut session = build_empty_session();
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");
    let client_id = attach_viewer_with_pane_area(&mut session, tab, 80, 24, None);
    session.detach_client(client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
    session
}

#[test]
fn request_session_stop_moves_a_detaching_session_to_stopping() {
    let mut session = detached_session(TabId::new());

    session.request_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn complete_session_stop_on_a_detaching_session_leaves_it_detaching() {
    // `StopCompleted` is legal only from `Stopping`; `Detaching` rejects it.
    let mut session = detached_session(TabId::new());

    session.complete_session_stop();

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn attaching_a_client_to_a_stopped_session_registers_it_without_reviving_it() {
    let tab = TabId::new();
    let mut session = build_empty_session();
    session.request_session_stop();
    session.complete_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);

    let client_id = attach_viewer_with_pane_area(&mut session, tab, 80, 24, None);

    assert_eq!(session.clients.client_count(), 1);
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
fn tab_viewport_counts_a_client_only_once_it_switches_onto_the_tab() {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let mut session = build_empty_session();
    let client_id = attach_viewer_with_pane_area(&mut session, other_tab, 100, 30, None);
    assert_eq!(session.get_tab_viewport(tab), None);

    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the viewer is attached")
        .update_active_tab(tab);

    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 100,
            row_count: 28
        })
    );
    assert_eq!(session.get_tab_viewport(other_tab), None);
}

#[test]
fn tab_viewport_clamps_a_report_larger_than_the_viewport() {
    let tab = TabId::new();
    let mut session = build_empty_session();

    attach_viewer_with_pane_area(
        &mut session,
        tab,
        80,
        24,
        Some(PaneArea::Reported(Size {
            column_count: u16::MAX,
            row_count: u16::MAX,
        })),
    );

    // A report is clamped per axis to the viewport, and it replaces the
    // two-chrome-row default outright: 80x24, not 80x22.
    assert_eq!(
        session.get_tab_viewport(tab),
        Some(Size {
            column_count: 80,
            row_count: 24
        })
    );
}

#[test]
fn an_empty_session_survives_a_serde_round_trip() {
    let session = build_empty_session();

    let written = serde_json::to_string(&session).expect("the session writes out");
    let read_back: Session = serde_json::from_str(&written).expect("the session reads back");

    assert_eq!(read_back.session_id, session.session_id);
    assert_eq!(read_back.session_name, "s");
    assert_eq!(read_back.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*read_back.get_lifecycle(), SessionLifecycle::Starting);
    assert!(!read_back.start_locked);
    assert_eq!(read_back.tabs.len(), 0);
    assert_eq!(read_back.panes.pane_record_count(), 0);
    assert_eq!(read_back.clients.client_count(), 0);
}

/// A whole session must survive being written out and read back: its identity
/// and lifecycle, its tabs with their nested layout trees, its pane registry,
/// and the view state each attached client keeps to itself.
#[test]
fn a_session_with_tabs_panes_and_clients_survives_a_serde_round_trip() {
    let tab_one = TabId::new();
    let tab_two = TabId::new();
    let pane_one = PaneId::new();
    let pane_two = PaneId::new();
    let pane_three = PaneId::new();
    let pane_four = PaneId::new();

    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "carried".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");

    // Tab one splits left and right, and its right half splits again: pane one
    // fills the left, panes two and three share the right.
    let tab_one_layout = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(pane_one),
            LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![LayoutNode::Pane(pane_two), LayoutNode::Pane(pane_three)],
            )),
        ],
    ));
    let mut first_tab = Tab::from_root_pane(tab_one, "one".to_owned(), 0, pane_one);
    first_tab.update_layout(tab_one_layout.clone());
    first_tab.record_focus_mru(pane_two);
    session.tabs.insert(tab_one, first_tab);
    session.tabs.insert(
        tab_two,
        Tab::from_root_pane(tab_two, "two".to_owned(), 1, pane_four),
    );

    for pane in [pane_one, pane_two, pane_three, pane_four] {
        session
            .panes
            .register_pane_record(PaneRecord::from_terminal_pane(pane, SystemTime::UNIX_EPOCH))
            .expect("each pane id is registered once");
    }

    let highlight = Selection {
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

    // The first client watches tab one zoomed on pane two, scrolled up in it,
    // locked, grabbing the mouse, with a highlight up.
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
        tab_one,
        ClientOrigin::Local,
        "C-brave-otter".to_owned(),
        3,
    );
    zoomed_client.update_focused_pane(tab_one, pane_two);
    zoomed_client.update_focused_pane(tab_two, pane_four);
    zoomed_client.zoom_pane(tab_one, pane_two);
    zoomed_client.set_scroll_offset(pane_two, 7);
    zoomed_client.update_lock_mode(LockMode::Locked);
    zoomed_client.toggle_mouse_selection();
    zoomed_client.set_selection(pane_two, highlight);
    session.attach_client(zoomed_client);

    // The second client watches tab two, tiled, scrolled up in a different
    // pane, so no field is the same for both.
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
        tab_two,
        ClientOrigin::Local,
        "C-calm-heron".to_owned(),
        5,
    );
    tiled_client.update_focused_pane(tab_one, pane_one);
    tiled_client.set_scroll_offset(pane_three, 3);
    session.attach_client(tiled_client);

    let serialized_session_json = serde_json::to_string(&session).expect("the session writes out");
    let decoded_session: Session =
        serde_json::from_str(&serialized_session_json).expect("the session reads back");

    assert_eq!(decoded_session.session_id, session.session_id);
    assert_eq!(decoded_session.session_name, "carried");
    assert_eq!(decoded_session.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*decoded_session.get_lifecycle(), SessionLifecycle::Running);

    assert_eq!(decoded_session.tabs.len(), 2);
    let recovered_one = decoded_session
        .tabs
        .get(&tab_one)
        .expect("tab one is carried");
    assert_eq!(recovered_one.get_tab_id(), tab_one);
    assert_eq!(recovered_one.get_tab_name(), "one");
    assert_eq!(recovered_one.get_tab_index(), 0);
    assert_eq!(*recovered_one.get_layout_tree(), tab_one_layout);
    assert_eq!(recovered_one.list_focus_mru(), [pane_two].as_slice());
    assert_eq!(*recovered_one.get_lifecycle(), TabLifecycle::Creating);
    let recovered_two = decoded_session
        .tabs
        .get(&tab_two)
        .expect("tab two is carried");
    assert_eq!(recovered_two.get_tab_index(), 1);
    assert_eq!(
        *recovered_two.get_layout_tree(),
        LayoutNode::Pane(pane_four)
    );

    assert_eq!(decoded_session.panes.pane_record_count(), 4);
    for pane in [pane_one, pane_two, pane_three, pane_four] {
        let pane_record = decoded_session
            .panes
            .get_pane_record_by_id(pane)
            .expect("the pane record is carried");
        assert_eq!(pane_record.get_pane_id(), pane);
        assert_eq!(*pane_record.get_lifecycle(), PaneLifecycle::Spawning);
    }

    assert_eq!(decoded_session.clients.client_count(), 2);
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
    assert_eq!(recovered_zoomed_client.get_color(), 3);
    assert_eq!(
        recovered_zoomed_client.get_viewport_size(),
        Size {
            column_count: 100,
            row_count: 30
        }
    );
    assert_eq!(recovered_zoomed_client.get_active_tab(), tab_one);
    assert_eq!(recovered_zoomed_client.get_lock_mode(), LockMode::Locked);
    assert!(recovered_zoomed_client.is_mouse_selection_enabled());
    assert_eq!(
        recovered_zoomed_client.get_focused_pane(tab_one),
        Some(pane_two)
    );
    assert_eq!(
        recovered_zoomed_client.get_focused_pane(tab_two),
        Some(pane_four)
    );
    assert_eq!(
        recovered_zoomed_client.get_zoomed_pane(tab_one),
        Some(pane_two)
    );
    assert_eq!(recovered_zoomed_client.get_zoomed_pane(tab_two), None);
    assert_eq!(recovered_zoomed_client.get_scroll_offset(pane_two), 7);
    assert_eq!(recovered_zoomed_client.get_scroll_offset(pane_three), 0);
    assert_eq!(
        recovered_zoomed_client.get_selection(pane_two),
        Some(highlight)
    );
    assert_eq!(recovered_zoomed_client.get_selection(pane_one), None);

    let recovered_tiled_client = decoded_session
        .clients
        .get_client_by_id(tiled_client_id)
        .expect("the second client is carried");
    assert_eq!(recovered_tiled_client.get_label(), "C-calm-heron");
    assert_eq!(recovered_tiled_client.get_color(), 5);
    assert_eq!(
        recovered_tiled_client.get_viewport_size(),
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    assert_eq!(recovered_tiled_client.get_active_tab(), tab_two);
    assert_eq!(recovered_tiled_client.get_lock_mode(), LockMode::Normal);
    assert!(!recovered_tiled_client.is_mouse_selection_enabled());
    assert_eq!(
        recovered_tiled_client.get_focused_pane(tab_one),
        Some(pane_one)
    );
    assert_eq!(recovered_tiled_client.get_focused_pane(tab_two), None);
    assert_eq!(recovered_tiled_client.get_zoomed_pane(tab_one), None);
    assert_eq!(recovered_tiled_client.get_scroll_offset(pane_three), 3);
    assert_eq!(recovered_tiled_client.get_scroll_offset(pane_two), 0);
}

#[test]
fn a_stored_session_carrying_a_config_snapshot_key_still_reads() {
    // `config_snapshot` is not a field of `Session`, so a stored session that
    // names it reads back with the key ignored and every other field taken.
    let session_id = SessionId::new();
    let stored = serde_json::json!({
        "id": session_id,
        "name": "carried",
        "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
        "tabs": {},
        "panes": { "records": {} },
        "clients": { "records": {} },
        "config_snapshot": null,
        "lifecycle": "Starting",
    });

    let read_back: Session = serde_json::from_value(stored).expect("the stored session reads back");

    assert_eq!(read_back.session_id, session_id);
    assert_eq!(read_back.session_name, "carried");
    assert_eq!(read_back.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*read_back.get_lifecycle(), SessionLifecycle::Starting);
    assert!(!read_back.start_locked);
    assert!(read_back.tabs.is_empty());
    assert!(!read_back.panes.has_pane_records());
    assert!(!read_back.clients.has_clients());
}

#[test]
fn attaching_a_client_to_a_stopping_session_registers_it_without_reviving_it() {
    // `ClientAttached` moves only a `Detaching` session to `Running`. A client
    // that attaches while the session is `Stopping` is still registered, and
    // the lifecycle stays `Stopping`.
    let tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    let client_id = ClientId::new();
    let client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let displaced = session.attach_client(client);

    assert_eq!(displaced.map(|client| client.get_client_id()), None);
    assert_eq!(session.clients.client_count(), 1);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .map(Client::get_client_id),
        Some(client_id)
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
}
