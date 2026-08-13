//! Tests for [`Session`] helpers.

use super::*;
use std::time::SystemTime;

use koshi_core::command::{GridPos, Selection, SelectionKind};
use koshi_core::geometry::SplitDirection;
use koshi_core::lock::LockMode;
use koshi_layout::tree::{LayoutChild, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::{AuthorityTier, ClientOrigin};

/// Attach a client viewing `tab` with the given viewport.
fn viewer(session: &mut Session, tab: TabId, cols: u16, rows: u16) {
    let client = Client::new(
        ClientId::new(),
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols, rows },
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);
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

    // A single viewer's own size, minus the two chrome rows, wins outright —
    // there is no second viewport to take a minimum against.
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
    // A viewport with fewer rows than the two reserved chrome rows must not
    // underflow the `u16` row count; `1 - 2` would panic in debug builds
    // under plain subtraction, so the contract is `0`, not a crash.
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
fn attach_client_returns_the_client_it_displaced_on_reattach() {
    // A re-attach under the same id replaces in place; the caller needs the
    // displaced record back (e.g. to tear down its old view state).
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
}

#[test]
fn attaching_a_client_before_any_tab_leaves_the_session_starting() {
    // `ClientAttached` only revives a `Detaching` session; a session that
    // has not created its first tab yet is `Starting`, and attaching there
    // is not one of the legal moves out of `Starting` — the session stays
    // `Starting` until its first tab arrives.
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
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    session.attach_client(client);

    let removed = session.detach_client(id);

    assert_eq!(removed.map(|c| c.id()), Some(id));
    assert!(session.clients.get(id).is_none());
}

#[test]
fn detach_client_on_an_unattached_id_returns_none() {
    let mut session = Session::new(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert!(session.detach_client(ClientId::new()).is_none());
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

    // A second request from `Stopping` is rejected by the state machine and
    // silently ignored here; the session must not move or panic.
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
    // A session already winding down (`Stopping`) that loses its last client
    // must stay `Stopping`, never fall back to `Detaching` — that would be a
    // step backward in the shutdown sequence.
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
            LayoutChild::new(LayoutNode::Pane(pane_one)),
            LayoutChild::new(LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Vertical,
                vec![
                    LayoutChild::new(LayoutNode::Pane(pane_two)),
                    LayoutChild::new(LayoutNode::Pane(pane_three)),
                ],
            ))),
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
    // The plugin runtime handle names a live process, so it is left out.
    assert!(read_back.plugin_runtime_ref.is_none());

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
    assert_eq!(recovered_first.tier(), AuthorityTier::Admin);
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
