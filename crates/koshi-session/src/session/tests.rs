//! Tests for session and tab state, lifecycle, and consistency validation.
//!
//! Covers session creation and lifecycle transitions, tab construction and
//! mutations, client attachment and detachment effects, the floating set and
//! its removal from every client's view, and the invariants
//! `validate_session_consistency()` checks across pane registries, layout
//! trees, floating members, client focus and floating view records, and
//! lifecycle states.

use std::collections::HashMap;
use std::num::NonZeroU16;
use std::time::SystemTime;

use koshi_core::constant::{MAX_FLOATING_PANES_PER_SESSION, MAX_TAB_FOCUS_MRU_ENTRY_COUNT};
use koshi_core::event::{Event, PaneClosing, PaneRemoved, QuitCause, TabClosed};
use koshi_core::geometry::{
    AxisPercent, FloatingPaneDimension, FloatingPaneSize, PixelCellSize, Point, Size,
    SplitDirection,
};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::state::PaneRecord;

use super::lifecycle::SessionLifecycle;
use super::pane_ops::NewPaneSpec;
use super::state::{FloatingMember, FloatingSet, Session, Tab};
use super::tab_ops::{close_tab, commit_new_tab};
use crate::client::{Client, ClientOrigin, ClientRegistry, FloatingPaneView};
use crate::error::{FloatingSetError, SessionConsistencyError};

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

/// A local client of `session_id` viewing `active_tab_id`, with an 80x24
/// viewport and `UNIX_EPOCH` as its attach time.
fn build_test_client(session_id: SessionId, active_tab_id: TabId) -> Client {
    Client::from_attachment(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        active_tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// Attach a client *of this session*, viewing `active_tab_id`, and return its
/// id.
fn attach_viewer(session: &mut Session, active_tab_id: TabId) -> ClientId {
    let client = build_test_client(session.session_id, active_tab_id);
    let client_id = client.get_client_id();
    session.attach_client(client);
    client_id
}

/// A fresh, empty session with a random id.
fn build_empty_session() -> Session {
    Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
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
        build_test_client(session.session_id, tab_id),
        build_test_client(session.session_id, tab_id),
        build_test_client(session.session_id, other_tab_id),
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
fn the_first_tab_moves_the_session_to_running() {
    let mut session = build_empty_session();

    let _ = commit_test_tab(&mut session, "code".to_owned());
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    // A second tab does not re-fire the start transition.
    let _ = commit_test_tab(&mut session, "logs".to_owned());
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_the_last_client_moves_the_session_to_detaching_and_keeps_its_tabs() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let client_id = attach_viewer(&mut session, tab_id);
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

    let detached_client_id = attach_viewer(&mut session, tab_id);
    session.detach_client(detached_client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);

    attach_viewer(&mut session, tab_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_one_of_several_clients_keeps_the_session_running() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);

    let first_client_id = attach_viewer(&mut session, tab_id);
    let second_client_id = attach_viewer(&mut session, tab_id);

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

#[test]
fn closing_the_last_tab_requests_a_stop() {
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let teardown_events = close_tab(&mut session, tab_id);

    // The tab's only pane is torn down, then the tab, then the session quits.
    assert_eq!(
        teardown_events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id }),
            Event::PaneRemoved(PaneRemoved { pane_id, tab_id }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
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

    let client_id = attach_viewer(&mut session, tab_id);
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
    let client = build_test_client(foreign_session_id, tab_id);
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
    let client_id = attach_viewer(&mut session, phantom_tab_id);

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
        attach_viewer(&mut session, phantom_tab_id),
        attach_viewer(&mut session, phantom_tab_id),
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
    let client_id = attach_viewer(&mut session, tab_id);

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
    let client_id = attach_viewer(&mut session, tab_id);
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
    let client_id = attach_viewer(&mut session, tab_id);
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
    let client_id = attach_viewer(&mut session, real_tab_id);
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

    let client_id = attach_viewer(&mut session, second_tab_id);
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

    let client_id = attach_viewer(&mut session, second_tab_id);
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

    let client_id = attach_viewer(&mut session, tab_id);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab_id, pane_id);
    client.zoom_pane(tab_id, pane_id);

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_focus_on_a_ghost_pane_in_a_real_tab_reports_both_missing_record_and_missing_target() {
    // Focus pointing at a pane with no record, inside a tab that *does* exist,
    // trips two independent checks at once: the registry has no such pane
    // (`FocusPaneNotInRegistry`), and the tab's layout does not hold it either
    // (`FocusTargetMissing`). Both name the same client, tab, and pane.
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);

    let ghost_pane_id = PaneId::new();
    let client_id = attach_viewer(&mut session, tab_id);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab_id, ghost_pane_id);

    // Exactly those two: the real tab, its real pane, and the session-matched
    // client add nothing else.
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
fn a_zoom_on_a_pane_with_no_record_is_reported() {
    // A zoom naming a pane the registry has never heard of is not a live leaf:
    // it is reported even though the tab it is keyed under is real. The
    // client's focus sits on the real pane: no focus check fires alongside.
    let mut session = build_empty_session();
    let emitted_events = commit_test_tab(&mut session, "code".to_owned());
    let tab_id = get_created_tab_id(&emitted_events);
    let pane_id = get_created_pane_id(&emitted_events);

    let ghost_pane_id = PaneId::new();
    let client_id = attach_viewer(&mut session, tab_id);
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
    let client_id = attach_viewer(&mut session, tab_id);
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

/// A floating member for `pane_id` asking for 40x10 cells and solved to 40x10.
fn build_floating_member(pane_id: PaneId) -> FloatingMember {
    FloatingMember {
        pane_id,
        desired_size: FloatingPaneSize {
            width: FloatingPaneDimension::Cells(NonZeroU16::new(40).expect("40 is nonzero")),
            height: FloatingPaneDimension::Cells(NonZeroU16::new(10).expect("10 is nonzero")),
        },
        solved_size: Size {
            column_count: 40,
            row_count: 10,
        },
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
fn add_member_refuses_a_pane_the_set_already_holds() {
    let mut floating_set = FloatingSet::default();
    let pane_id = PaneId::new();
    floating_set
        .add_member(build_floating_member(pane_id))
        .expect("the floating set is empty");
    let mut repeated_member = build_floating_member(pane_id);
    repeated_member.solved_size = Size {
        column_count: 20,
        row_count: 5,
    };

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
        floating_set
            .add_member(build_floating_member(pane_id))
            .expect("the floating set has room");
    }

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
            solved_size: Size {
                column_count: 48,
                row_count: 24,
            },
        })
        .expect("the floating set has room");

    let floating_set_json = serde_json::to_value(&floating_set).expect("the set encodes");

    assert_eq!(
        floating_set_json,
        serde_json::json!({"members": [
            {
                "pane_id": cell_sized_pane_id,
                "desired_size": {"width": {"Cells": 40}, "height": {"Cells": 10}},
                "solved_size": {"column_count": 40, "row_count": 10}
            },
            {
                "pane_id": percent_sized_pane_id,
                "desired_size": {"width": {"Percent": 60}, "height": {"Percent": 100}},
                "solved_size": {"column_count": 48, "row_count": 24}
            }
        ]})
    );
    assert_eq!(
        serde_json::from_value::<FloatingSet>(floating_set_json).expect("the set decodes"),
        floating_set
    );
}

#[test]
fn remove_floating_member_keeps_the_order_of_the_rest_and_clears_every_client_view() {
    let mut session = build_empty_session();
    let removed_pane_id = register_floating_pane(&mut session);
    let middle_pane_id = register_floating_pane(&mut session);
    let last_pane_id = register_floating_pane(&mut session);
    let tab_id = TabId::new();
    let focusing_client_id = attach_viewer(&mut session, tab_id);
    let pinning_client_id = attach_viewer(&mut session, tab_id);
    let focusing_client = get_attached_client_mut(&mut session, focusing_client_id);
    assert!(focusing_client.focus_floating_pane(middle_pane_id));
    assert!(focusing_client.focus_floating_pane(removed_pane_id));
    let pinning_client = get_attached_client_mut(&mut session, pinning_client_id);
    assert!(pinning_client.focus_floating_pane(removed_pane_id));
    pinning_client.set_floating_pane_pinned(removed_pane_id, true);
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
                placement: None,
                is_pinned: false,
                is_minimized: true,
            },
        )])
    );
}

#[test]
fn remove_floating_member_of_a_pane_that_is_not_floating_changes_nothing() {
    let mut session = build_empty_session();
    let floating_pane_id = register_floating_pane(&mut session);
    let client_id = attach_viewer(&mut session, TabId::new());
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
    let client_id = attach_viewer(&mut session, TabId::new());
    let client = get_attached_client_mut(&mut session, client_id);
    assert!(client.focus_floating_pane(floating_pane_id));
    assert!(client.set_floating_pane_placement(floating_pane_id, Point { column: 2, row: 1 }));

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
    let client_id = attach_viewer(&mut session, TabId::new());
    let mut stray_pane_ids: [PaneId; 8] = std::array::from_fn(|_| PaneId::new());
    let client = get_attached_client_mut(&mut session, client_id);
    for stray_pane_id in stray_pane_ids {
        client.set_floating_pane_pinned(stray_pane_id, true);
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
    let client_id = attach_viewer(&mut session, TabId::new());
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
    let client_id = attach_viewer(&mut session, TabId::new());
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
    let client_id = attach_viewer(&mut session, TabId::new());
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
    let client_id = attach_viewer(&mut session, TabId::new());
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
    let client_id = attach_viewer(&mut session, TabId::new());
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
    let client_id = attach_viewer(&mut session, tab_id);
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
