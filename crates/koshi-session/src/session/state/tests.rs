//! Tests for [`Session`] helpers.

use super::*;
use std::time::SystemTime;

use koshi_core::command::{GridPos, Selection, SelectionKind};
use koshi_core::geometry::{PaneArea, SplitDirection};
use koshi_core::lock::LockMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::ClientOrigin;

/// Attach a client viewing `tab` with the given viewport, reporting no pane
/// area.
fn viewer(session: &mut Session, tab: TabId, cols: u16, rows: u16) {
    reporting_viewer(session, tab, cols, rows, None);
}

/// Attach a client viewing `tab` with the given viewport, reporting
/// `pane_area`, and return the id it attached under.
fn reporting_viewer(
    session: &mut Session,
    tab: TabId,
    cols: u16,
    rows: u16,
    pane_area: Option<PaneArea>,
) -> ClientId {
    let id = ClientId::new();
    let client = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols, rows },
        pane_area,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);
    id
}

/// A session with no tabs and no clients.
fn a_session() -> Session {
    Session::new(
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
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    // Two clients view `tab` with opposite aspect ratios.
    viewer(&mut session, tab, 80, 5);
    viewer(&mut session, tab, 40, 24);
    // A client on a different tab must not count.
    viewer(&mut session, other_tab, 10, 1);

    // Full-viewport minimum is 40×5; reserving two chrome rows leaves 40×3.
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 40, rows: 3 }));
}

#[test]
fn tab_viewport_is_none_without_a_viewer() {
    let tab = TabId::new();
    let session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(session.tab_viewport(tab), None);
}

#[test]
fn a_new_session_stores_the_supplied_creation_time() {
    let created_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234);
    let session = Session::new(
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
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    viewer(&mut session, tab, 100, 30);

    // With one viewer the result is that viewer's own size minus the two
    // chrome rows.
    assert_eq!(
        session.tab_viewport(tab),
        Some(Size {
            cols: 100,
            rows: 28
        })
    );
}

#[test]
fn tab_viewport_saturates_rather_than_panics_below_the_chrome_rows() {
    // A viewport with fewer rows than the two reserved chrome rows saturates
    // the row count at `0`.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    viewer(&mut session, tab, 80, 1);

    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 80, rows: 0 }));
}

#[test]
fn tab_viewport_leaves_a_starving_viewer_out() {
    let tab = TabId::new();
    let mut session = a_session();

    viewer(&mut session, tab, 80, 24);
    reporting_viewer(&mut session, tab, 80, 24, Some(PaneArea::Starving));

    // Only the viewer that reported a pane area counts: 80x24 minus the two
    // chrome rows.
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 80, rows: 22 }));
}

#[test]
fn tab_viewport_with_a_zero_reported_area_is_zero() {
    let tab = TabId::new();
    let mut session = a_session();

    viewer(&mut session, tab, 80, 24);
    reporting_viewer(
        &mut session,
        tab,
        80,
        24,
        Some(PaneArea::Reported(Size { cols: 0, rows: 0 })),
    );

    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 0, rows: 0 }));
}

#[test]
fn tab_viewport_is_none_when_every_viewer_is_starving() {
    let tab = TabId::new();
    let mut session = a_session();

    reporting_viewer(&mut session, tab, 80, 24, Some(PaneArea::Starving));
    reporting_viewer(&mut session, tab, 80, 24, Some(PaneArea::Starving));

    assert_eq!(session.tab_viewport(tab), None);
}

#[test]
fn tab_viewport_takes_the_minimum_of_a_reported_and_an_unreported_viewer() {
    let tab = TabId::new();
    let mut session = a_session();

    reporting_viewer(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size { cols: 60, rows: 30 })),
    );
    viewer(&mut session, tab, 80, 24);

    // 60x30 reported against 80x22 from the unreported viewer.
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 60, rows: 22 }));
}

#[test]
fn tab_viewport_takes_the_element_wise_minimum_of_two_reports() {
    let tab = TabId::new();
    let mut session = a_session();

    reporting_viewer(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size {
            cols: 100,
            rows: 20,
        })),
    );
    reporting_viewer(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size { cols: 60, rows: 30 })),
    );

    // The narrower report gives the columns, the shorter one gives the rows.
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 60, rows: 20 }));
}

#[test]
fn tab_viewport_does_not_depend_on_where_the_starving_viewer_attached() {
    let tab = TabId::new();

    let mut starving_first = a_session();
    reporting_viewer(&mut starving_first, tab, 80, 24, Some(PaneArea::Starving));
    viewer(&mut starving_first, tab, 80, 24);

    let mut starving_second = a_session();
    viewer(&mut starving_second, tab, 80, 24);
    reporting_viewer(&mut starving_second, tab, 80, 24, Some(PaneArea::Starving));

    assert_eq!(
        starving_first.tab_viewport(tab),
        Some(Size { cols: 80, rows: 22 })
    );
    assert_eq!(
        starving_second.tab_viewport(tab),
        Some(Size { cols: 80, rows: 22 })
    );
}

#[test]
fn detaching_a_starving_viewer_leaves_tab_viewport_unchanged() {
    let tab = TabId::new();
    let mut session = a_session();
    viewer(&mut session, tab, 80, 24);
    let starving = reporting_viewer(&mut session, tab, 80, 24, Some(PaneArea::Starving));
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 80, rows: 22 }));

    session.detach_client(starving);

    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 80, rows: 22 }));
}

#[test]
fn detaching_the_smallest_viewer_lets_tab_viewport_grow() {
    let tab = TabId::new();
    let mut session = a_session();
    let smallest = reporting_viewer(
        &mut session,
        tab,
        120,
        40,
        Some(PaneArea::Reported(Size { cols: 60, rows: 30 })),
    );
    viewer(&mut session, tab, 120, 40);
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 60, rows: 30 }));

    session.detach_client(smallest);

    assert_eq!(
        session.tab_viewport(tab),
        Some(Size {
            cols: 120,
            rows: 38
        })
    );
}

#[test]
fn attach_client_returns_the_client_it_displaced_on_reattach() {
    // A re-attach under the same id replaces the record in place and returns
    // the one it displaced.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let id = ClientId::new();
    let first = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 0, rows: 0 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    assert_eq!(session.attach_client(first).map(|c| c.id()), None);

    let second = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 40, rows: 10 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let displaced = session.attach_client(second);

    assert_eq!(
        displaced.map(|c| c.viewport()),
        Some(Size { cols: 0, rows: 0 })
    );
    assert_eq!(
        session.clients.get(id).map(Client::viewport),
        Some(Size { cols: 40, rows: 10 })
    );
    assert_eq!(session.clients.len(), 1);
}

#[test]
fn attaching_a_client_before_any_tab_leaves_the_session_starting() {
    // `ClientAttached` moves only a `Detaching` session to `Running`. A session
    // that has not created its first tab is `Starting` and rejects the event.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);

    let client = Client::new(
        ClientId::new(),
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 0, rows: 0 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);

    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detach_client_returns_the_exact_record_it_removed() {
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let id = ClientId::new();
    let client = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 12, rows: 3 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);

    let removed = session.detach_client(id);

    assert_eq!(removed.map(|c| c.id()), Some(id));
    assert_eq!(session.clients.get(id).map(Client::id), None);
    assert_eq!(session.clients.len(), 0);
}

#[test]
fn detach_client_on_an_unattached_id_returns_none() {
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(session.detach_client(ClientId::new()).map(|c| c.id()), None);
}

#[test]
fn request_stop_is_idempotent_once_already_stopping() {
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    // `StopRequested` from `Stopping` is rejected; the session stays
    // `Stopping`.
    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn complete_stop_before_a_stop_was_requested_is_a_noop() {
    // `StopCompleted` is only legal from `Stopping`; calling it on a fresh
    // (`Starting`) session is an illegal transition the wrapper swallows.
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    session.complete_stop();

    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_the_last_client_of_a_stopping_session_does_not_revert_it() {
    // A `Stopping` session that loses its last client stays `Stopping`; it does
    // not fall back to `Detaching`.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let id = ClientId::new();
    let client = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 0, rows: 0 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);
    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    let removed = session.detach_client(id);

    assert_eq!(removed.map(|c| c.id()), Some(id));
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn detaching_the_last_client_of_a_starting_session_leaves_it_starting() {
    // `LastClientDetached` moves only a `Running` session to `Detaching`. A
    // session that has not created its first tab is `Starting` and rejects it.
    let tab = TabId::new();
    let mut session = a_session();
    let id = reporting_viewer(&mut session, tab, 80, 24, None);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);

    let removed = session.detach_client(id);

    assert_eq!(removed.map(|c| c.id()), Some(id));
    assert_eq!(session.clients.len(), 0);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn detaching_again_with_no_clients_left_keeps_the_session_detaching() {
    let tab = TabId::new();
    let mut session = a_session();
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");
    let id = reporting_viewer(&mut session, tab, 80, 24, None);

    session.detach_client(id);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);

    // The registry is already empty, so the second detach fires
    // `LastClientDetached` again; `Detaching` rejects it and does not move.
    let removed = session.detach_client(ClientId::new());

    assert_eq!(removed.map(|c| c.id()), None);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn completing_a_stop_twice_leaves_the_session_stopped() {
    let mut session = a_session();
    session.request_stop();
    session.complete_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopped);

    // `Stopped` is terminal and rejects every event, including a repeat.
    session.complete_stop();

    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopped);
}

/// Drive `session` to `Detaching`: create its first tab, attach one viewer of
/// `tab`, then detach it.
fn detached_session(tab: TabId) -> Session {
    let mut session = a_session();
    session
        .update_lifecycle(SessionLifecycleEvent::FirstTabCreated)
        .expect("a starting session accepts its first tab");
    let id = reporting_viewer(&mut session, tab, 80, 24, None);
    session.detach_client(id);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);
    session
}

#[test]
fn request_stop_moves_a_detaching_session_to_stopping() {
    let mut session = detached_session(TabId::new());

    session.request_stop();

    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn complete_stop_on_a_detaching_session_leaves_it_detaching() {
    // `StopCompleted` is legal only from `Stopping`; `Detaching` rejects it.
    let mut session = detached_session(TabId::new());

    session.complete_stop();

    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn attaching_a_client_to_a_stopped_session_registers_it_without_reviving_it() {
    let tab = TabId::new();
    let mut session = a_session();
    session.request_stop();
    session.complete_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopped);

    let id = reporting_viewer(&mut session, tab, 80, 24, None);

    assert_eq!(session.clients.len(), 1);
    assert_eq!(session.clients.get(id).map(Client::id), Some(id));
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopped);
}

#[test]
fn tab_viewport_counts_a_client_only_once_it_switches_onto_the_tab() {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let mut session = a_session();
    let id = reporting_viewer(&mut session, other_tab, 100, 30, None);
    assert_eq!(session.tab_viewport(tab), None);

    session
        .clients
        .get_mut(id)
        .expect("the viewer is attached")
        .update_active_tab(tab);

    assert_eq!(
        session.tab_viewport(tab),
        Some(Size {
            cols: 100,
            rows: 28
        })
    );
    assert_eq!(session.tab_viewport(other_tab), None);
}

#[test]
fn tab_viewport_clamps_a_report_larger_than_the_viewport() {
    let tab = TabId::new();
    let mut session = a_session();

    reporting_viewer(
        &mut session,
        tab,
        80,
        24,
        Some(PaneArea::Reported(Size {
            cols: u16::MAX,
            rows: u16::MAX,
        })),
    );

    // A report is clamped per axis to the viewport, and it replaces the
    // two-chrome-row default outright: 80x24, not 80x22.
    assert_eq!(session.tab_viewport(tab), Some(Size { cols: 80, rows: 24 }));
}

#[test]
fn an_empty_session_survives_a_serde_round_trip() {
    let session = a_session();

    let written = serde_json::to_string(&session).expect("the session writes out");
    let read_back: Session = serde_json::from_str(&written).expect("the session reads back");

    assert_eq!(read_back.id, session.id);
    assert_eq!(read_back.name, "s");
    assert_eq!(read_back.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*read_back.lifecycle(), SessionLifecycle::Starting);
    assert!(!read_back.start_locked);
    assert_eq!(read_back.tabs.len(), 0);
    assert_eq!(read_back.panes.len(), 0);
    assert_eq!(read_back.clients.len(), 0);
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

    let mut session = Session::new(
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
    let mut first_tab = Tab::new(tab_one, "one".to_owned(), 0, pane_one);
    first_tab.update_layout(tab_one_layout.clone());
    first_tab.record_focus_mru(pane_two);
    session.tabs.insert(tab_one, first_tab);
    session
        .tabs
        .insert(tab_two, Tab::new(tab_two, "two".to_owned(), 1, pane_four));

    for pane in [pane_one, pane_two, pane_three, pane_four] {
        session
            .panes
            .insert(PaneRecord::new(pane, SystemTime::UNIX_EPOCH))
            .expect("each pane id is registered once");
    }

    let highlight = Selection {
        kind: SelectionKind::Block,
        anchor: GridPos { row: 12, col: 4 },
        cursor: GridPos { row: 40, col: 9 },
    };

    // The first client watches tab one zoomed on pane two, scrolled up in it,
    // locked, grabbing the mouse, with a highlight up.
    let first_id = ClientId::new();
    let mut first = Client::new(
        first_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size {
            cols: 100,
            rows: 30,
        },
        None,
        tab_one,
        ClientOrigin::Local,
        "C-brave-otter".to_owned(),
        3,
    );
    first.update_focused_pane(tab_one, pane_two);
    first.update_focused_pane(tab_two, pane_four);
    first.zoom_pane(tab_one, pane_two);
    first.set_scroll_offset(pane_two, 7);
    first.update_lock_mode(LockMode::Locked);
    first.toggle_mouse_select();
    first.set_selection(pane_two, highlight);
    session.attach_client(first);

    // The second client watches tab two, tiled, scrolled up in a different
    // pane, so no field is the same for both.
    let second_id = ClientId::new();
    let mut second = Client::new(
        second_id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        tab_two,
        ClientOrigin::Local,
        "C-calm-heron".to_owned(),
        5,
    );
    second.update_focused_pane(tab_one, pane_one);
    second.set_scroll_offset(pane_three, 3);
    session.attach_client(second);

    let written = serde_json::to_string(&session).expect("the session writes out");
    let read_back: Session = serde_json::from_str(&written).expect("the session reads back");

    assert_eq!(read_back.id, session.id);
    assert_eq!(read_back.name, "carried");
    assert_eq!(read_back.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*read_back.lifecycle(), SessionLifecycle::Running);

    assert_eq!(read_back.tabs.len(), 2);
    let recovered_one = read_back.tabs.get(&tab_one).expect("tab one is carried");
    assert_eq!(recovered_one.id(), tab_one);
    assert_eq!(recovered_one.name(), "one");
    assert_eq!(recovered_one.index(), 0);
    assert_eq!(*recovered_one.layout(), tab_one_layout);
    assert_eq!(recovered_one.focus_mru(), [pane_two].as_slice());
    assert_eq!(*recovered_one.lifecycle(), TabLifecycle::Creating);
    let recovered_two = read_back.tabs.get(&tab_two).expect("tab two is carried");
    assert_eq!(recovered_two.index(), 1);
    assert_eq!(*recovered_two.layout(), LayoutNode::Pane(pane_four));

    assert_eq!(read_back.panes.len(), 4);
    for pane in [pane_one, pane_two, pane_three, pane_four] {
        let record = read_back
            .panes
            .get(pane)
            .expect("the pane record is carried");
        assert_eq!(record.id(), pane);
        assert_eq!(*record.lifecycle(), PaneLifecycle::Spawning);
    }

    assert_eq!(read_back.clients.len(), 2);
    let recovered_first = read_back
        .clients
        .get(first_id)
        .expect("the first client is carried");
    assert_eq!(recovered_first.session_id(), session.id);
    assert_eq!(recovered_first.attached_at(), SystemTime::UNIX_EPOCH);
    assert_eq!(recovered_first.origin(), ClientOrigin::Local);
    assert_eq!(recovered_first.label(), "C-brave-otter");
    assert_eq!(recovered_first.colour(), 3);
    assert_eq!(
        recovered_first.viewport(),
        Size {
            cols: 100,
            rows: 30
        }
    );
    assert_eq!(recovered_first.active_tab(), tab_one);
    assert_eq!(recovered_first.lock_mode(), LockMode::Locked);
    assert!(recovered_first.mouse_select());
    assert_eq!(recovered_first.focused_pane(tab_one), Some(pane_two));
    assert_eq!(recovered_first.focused_pane(tab_two), Some(pane_four));
    assert_eq!(recovered_first.zoomed_pane(tab_one), Some(pane_two));
    assert_eq!(recovered_first.zoomed_pane(tab_two), None);
    assert_eq!(recovered_first.scroll_offset(pane_two), 7);
    assert_eq!(recovered_first.scroll_offset(pane_three), 0);
    assert_eq!(recovered_first.selection(pane_two), Some(highlight));
    assert_eq!(recovered_first.selection(pane_one), None);

    let recovered_second = read_back
        .clients
        .get(second_id)
        .expect("the second client is carried");
    assert_eq!(recovered_second.label(), "C-calm-heron");
    assert_eq!(recovered_second.colour(), 5);
    assert_eq!(recovered_second.viewport(), Size { cols: 80, rows: 24 });
    assert_eq!(recovered_second.active_tab(), tab_two);
    assert_eq!(recovered_second.lock_mode(), LockMode::Normal);
    assert!(!recovered_second.mouse_select());
    assert_eq!(recovered_second.focused_pane(tab_one), Some(pane_one));
    assert_eq!(recovered_second.focused_pane(tab_two), None);
    assert_eq!(recovered_second.zoomed_pane(tab_one), None);
    assert_eq!(recovered_second.scroll_offset(pane_three), 3);
    assert_eq!(recovered_second.scroll_offset(pane_two), 0);
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

    assert_eq!(read_back.id, session_id);
    assert_eq!(read_back.name, "carried");
    assert_eq!(read_back.created_at, SystemTime::UNIX_EPOCH);
    assert_eq!(*read_back.lifecycle(), SessionLifecycle::Starting);
    assert!(!read_back.start_locked);
    assert!(read_back.tabs.is_empty());
    assert!(read_back.panes.is_empty());
    assert!(read_back.clients.is_empty());
}

#[test]
fn attaching_a_client_to_a_stopping_session_registers_it_without_reviving_it() {
    // `ClientAttached` moves only a `Detaching` session to `Running`. A client
    // that attaches while the session is `Stopping` is still registered, and
    // the lifecycle stays `Stopping`.
    let tab = TabId::new();
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    let id = ClientId::new();
    let client = Client::new(
        id,
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let displaced = session.attach_client(client);

    assert_eq!(displaced.map(|c| c.id()), None);
    assert_eq!(session.clients.len(), 1);
    assert_eq!(session.clients.get(id).map(Client::id), Some(id));
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}
