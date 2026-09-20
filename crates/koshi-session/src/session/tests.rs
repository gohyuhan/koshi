//! Tests for session and tab state, lifecycle, and consistency validation.
//!
//! Covers session creation and lifecycle transitions, tab construction and
//! mutations, client attachment/detachment effects, and invariant validation
//! via `validate_session_consistency()` to catch structural inconsistencies in pane registries,
//! layout trees, client focus records, and lifecycle states.

use std::time::SystemTime;

use koshi_core::constant::MAX_TAB_FOCUS_MRU_ENTRY_COUNT;
use koshi_core::event::{Event, PaneClosing, PaneRemoved, QuitCause, TabClosed};
use koshi_core::geometry::{Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::state::PaneRecord;

use super::lifecycle::{SessionLifecycle, TabLifecycle};
use super::pane_ops::NewPaneSpec;
use super::state::{Session, Tab};
use super::tab_ops::{close_tab, commit_new_tab};
use crate::client::{Client, ClientOrigin, ClientRegistry};
use crate::error::SessionConsistencyError;

/// Create a tab through [`commit_new_tab`] with freshly minted ids, no focus
/// client, and an empty spec — the session-level fixture for these tests.
fn commit_test_tab(session: &mut Session, tab_name: String, created_at: SystemTime) -> Vec<Event> {
    commit_new_tab(
        session,
        TabId::new(),
        PaneId::new(),
        tab_name,
        None,
        NewPaneSpec::default(),
        created_at,
    )
    .1
}

/// A client viewing `active_tab`, with an 80x24 viewport, a fresh session id of
/// its own, and `UNIX_EPOCH` as its attach time.
fn client_viewing(active_tab: TabId) -> Client {
    Client::from_attachment(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        active_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

#[test]
fn tab_cell_size_uses_the_oldest_measured_viewer_and_changes_on_detach() {
    use koshi_core::geometry::PixelCellSize;
    let tab = TabId::new();
    let other_tab = TabId::new();
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "images".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let mut clients = [
        client_viewing(tab),
        client_viewing(tab),
        client_viewing(other_tab),
    ];
    clients.sort_by_key(Client::get_client_id);
    let first_client_id = clients[0].get_client_id();
    let second_client_id = clients[1].get_client_id();
    clients[0].update_active_tab(tab);
    clients[1].update_active_tab(tab);
    clients[2].update_active_tab(other_tab);
    clients[1].update_cell_size(PixelCellSize::from_pixel_dimensions(12, 24).expect("nonzero"));
    clients[2].update_cell_size(PixelCellSize::from_pixel_dimensions(8, 16).expect("nonzero"));
    for client in clients {
        session.clients.attach_client(client);
    }
    assert_eq!(
        session.get_tab_cell_size(tab),
        PixelCellSize::from_pixel_dimensions(12, 24)
    );
    session
        .clients
        .get_client_mut_by_id(first_client_id)
        .expect("client")
        .update_cell_size(PixelCellSize::from_pixel_dimensions(10, 20).expect("nonzero"));
    assert_eq!(
        session.get_tab_cell_size(tab),
        PixelCellSize::from_pixel_dimensions(10, 20)
    );
    session.clients.detach_client(first_client_id);
    assert_eq!(
        session.get_tab_cell_size(tab),
        PixelCellSize::from_pixel_dimensions(12, 24)
    );
    session.clients.detach_client(second_client_id);
    assert_eq!(session.get_tab_cell_size(tab), None);
    assert_eq!(
        session.get_tab_cell_size(other_tab),
        PixelCellSize::from_pixel_dimensions(8, 16)
    );
}

/// The id of the tab a `new_tab` call just created, read off its `TabCreated`.
fn get_created_tab_id(emitted_events: &[Event]) -> TabId {
    emitted_events
        .iter()
        .find_map(|event| match event {
            Event::TabCreated(created) => Some(created.tab_id),
            _ => None,
        })
        .expect("new_tab emits a TabCreated event")
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
    assert!(!session.panes.has_pane_records());
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
    // A fresh tab shows exactly its root pane, mid-creation, no focus yet. It
    // carries no layout mode of its own: whether a pane is zoomed belongs to a
    // client's view, not to the tab.
    assert_eq!(*tab.get_layout_tree(), LayoutNode::Pane(root_pane_id));
    assert_eq!(*tab.get_lifecycle(), TabLifecycle::Creating);
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
    let focus_history_capacity = MAX_TAB_FOCUS_MRU_ENTRY_COUNT as usize;

    // Record one more distinct pane than the cap allows.
    let panes: Vec<PaneId> = (0..=focus_history_capacity)
        .map(|_| PaneId::new())
        .collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }

    // Newest first, with the first-recorded pane evicted: every other pane keeps
    // its place in recording order.
    let surviving_newest_first: Vec<PaneId> = panes[1..].iter().rev().copied().collect();
    assert_eq!(tab.list_focus_mru().to_vec(), surviving_newest_first);
}

#[test]
fn focus_mru_at_exactly_the_cap_evicts_nothing() {
    // The boundary just below the eviction case above: recording exactly
    // `MAX_TAB_FOCUS_MRU_ENTRY_COUNT` distinct panes must keep every one of them.
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let focus_history_capacity = MAX_TAB_FOCUS_MRU_ENTRY_COUNT as usize;

    let panes: Vec<PaneId> = (0..focus_history_capacity).map(|_| PaneId::new()).collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }

    let newest_first: Vec<PaneId> = panes.iter().rev().copied().collect();
    assert_eq!(tab.list_focus_mru().to_vec(), newest_first);
}

#[test]
fn re_recording_an_existing_pane_at_the_cap_moves_it_front_without_evicting() {
    // Re-recording an entry a full history already holds evicts nothing: the
    // duplicate is dropped before the length is checked.
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let focus_history_capacity = MAX_TAB_FOCUS_MRU_ENTRY_COUNT as usize;
    let panes: Vec<PaneId> = (0..focus_history_capacity).map(|_| PaneId::new()).collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }
    // The re-recorded pane comes from the middle of the history. The back entry
    // is the one the cap evicts on its own.
    let middle_pane_id = panes[focus_history_capacity / 2];

    tab.record_focus_mru(middle_pane_id);

    // `middle_pane_id` moves to the front and every other pane keeps its order behind it.
    let mut expected_focus_history: Vec<PaneId> = panes.iter().rev().copied().collect();
    expected_focus_history.retain(|&pane_id| pane_id != middle_pane_id);
    expected_focus_history.insert(0, middle_pane_id);
    assert_eq!(tab.list_focus_mru().to_vec(), expected_focus_history);
}

#[test]
fn recording_the_same_pane_twice_keeps_one_entry() {
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let pane = PaneId::new();

    tab.record_focus_mru(pane);
    tab.record_focus_mru(pane);

    assert_eq!(tab.list_focus_mru().to_vec(), vec![pane]);
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
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.start_locked = true;

    assert!(session.take_start_lock(), "the first read takes the lock");
    assert!(!session.take_start_lock(), "the second read finds none");
    assert!(!session.start_locked);
}

#[test]
fn a_session_seeded_without_the_lock_marker_has_no_starting_lock() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert!(!session.take_start_lock());
}

#[test]
fn the_starting_lock_survives_a_serde_round_trip() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.start_locked = true;

    let json = serde_json::to_string(&session).expect("serialize");
    let restored_session: Session = serde_json::from_str(&json).expect("deserialize");

    assert!(
        restored_session.start_locked,
        "a session server that restarts before any client attaches still locks the first one"
    );
}

#[test]
fn the_starting_lock_is_stored_as_a_plain_json_bool() {
    // Pins the stored shape: the member is named `start_locked` and holds a
    // JSON boolean.
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.start_locked = true;

    let serialized_session = serde_json::to_value(&session).expect("serialize");

    assert_eq!(
        serialized_session["start_locked"],
        serde_json::Value::Bool(true)
    );
}

#[test]
fn a_stored_session_without_the_lock_key_reads_back_unlocked() {
    // A stored session with no `start_locked` member reads the field back as
    // `false` through `#[serde(default)]`.
    let session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let mut serialized_session = serde_json::to_value(&session).expect("serialize");
    serialized_session
        .as_object_mut()
        .expect("a session serializes to a JSON object")
        .remove("start_locked")
        .expect("the key is present before it is dropped");

    let mut restored_session: Session =
        serde_json::from_value(serialized_session).expect("a session without the key deserializes");

    assert!(!restored_session.start_locked);
    assert!(!restored_session.take_start_lock());
}

#[test]
fn a_tab_survives_a_serde_round_trip() {
    let root = PaneId::new();
    let mut tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 2, root);
    tab.record_focus_mru(root);

    let json = serde_json::to_string(&tab).expect("serialize");
    let restored_tab: Tab = serde_json::from_str(&json).expect("deserialize");

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
    let session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn the_first_tab_moves_the_session_to_running() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    let _ = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    // A second tab does not re-fire the start transition.
    let _ = commit_test_tab(&mut session, "logs".to_owned(), SystemTime::UNIX_EPOCH);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_the_last_client_parks_the_session_without_destroying_state() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let client = client_viewing(tab);
    let client_id = client.get_client_id();
    session.attach_client(client);
    // Attaching to a running session leaves it running.
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    session.detach_client(client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
    // Parking is not destruction: the tab and its pane stay alive.
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        vec![tab]
    );
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane)
            .expect("the pane stays")
            .get_pane_id(),
        pane
    );
}

#[test]
fn re_attaching_resumes_a_detached_session() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);

    let detached_client = client_viewing(tab);
    let detached_client_id = detached_client.get_client_id();
    session.attach_client(detached_client);
    session.detach_client(detached_client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);

    session.attach_client(client_viewing(tab));
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_one_of_several_clients_keeps_the_session_running() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);

    let first_client = client_viewing(tab);
    let first_client_id = first_client.get_client_id();
    let second_client = client_viewing(tab);
    let second_client_id = second_client.get_client_id();
    session.attach_client(first_client);
    session.attach_client(second_client);

    session.detach_client(first_client_id);
    // One client remains, so the session is still running.
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);

    session.detach_client(second_client_id);
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn requesting_then_completing_a_stop_walks_to_stopped() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let _ = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    session.request_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);

    session.complete_session_stop();
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopped);
}

#[test]
fn closing_the_last_tab_requests_a_stop() {
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let teardown = close_tab(&mut session, tab);

    // The tab's only pane is torn down, then the tab, then the session quits.
    assert_eq!(
        teardown,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pane }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pane,
                tab_id: tab,
            }),
            Event::TabClosed(TabClosed { tab_id: tab }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id: tab,
                pane_exit: None,
            }),
        ]
    );
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Stopping);
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

/// A `Running` pane record registered in `session`, returned by id. Live and a
/// valid layout leaf, so on its own it trips no pane-level consistency check.
fn register_live_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, SystemTime::UNIX_EPOCH);
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
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, SystemTime::UNIX_EPOCH);
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
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, SystemTime::UNIX_EPOCH);
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

/// The id of the pane a `new_tab` call just created, read off its `PaneCreated`.
fn created_pane_id(events: &[Event]) -> PaneId {
    events
        .iter()
        .find_map(|event| match event {
            Event::PaneCreated(created) => Some(created.pane_id),
            _ => None,
        })
        .expect("new_tab emits a PaneCreated event")
}

/// Attach a client *of this session*, viewing `active_tab`, and return its id.
fn attach_viewing(session: &mut Session, active_tab: TabId) -> ClientId {
    let client = Client::from_attachment(
        ClientId::new(),
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        active_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let client_id = client.get_client_id();
    session.attach_client(client);
    client_id
}

/// A clone of `source_tab` whose private `lifecycle` field is set to
/// `target_tab_lifecycle`,
/// built by rewriting that member of the tab's JSON and reading it back.
/// `target_tab_lifecycle` is a [`TabLifecycle`] variant name, such as
/// `"Closed"`. Panics when the name is not one of them.
fn force_tab_lifecycle(source_tab: &Tab, target_tab_lifecycle: &str) -> Tab {
    let mut tab_value = serde_json::to_value(source_tab).expect("a tab serializes");
    tab_value["lifecycle"] = serde_json::Value::String(target_tab_lifecycle.to_owned());
    serde_json::from_value(tab_value).expect("a tab with a forced lifecycle deserializes")
}

#[test]
fn a_freshly_built_session_is_consistent() {
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let client_id = attach_viewing(&mut session, tab);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab, pane);

    session
        .validate_session_consistency()
        .expect("a session built through the normal operations is consistent");
}

#[test]
fn a_layout_leaf_with_no_record_is_reported() {
    let mut session = build_empty_session();
    let ghost = PaneId::new();
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, ghost);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::PaneNotInRegistry {
            tab_id,
            pane_id: ghost,
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
    // its leaf and its registry record, so neither side reports it.
    let mut session = build_empty_session();
    let closing = register_closing_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, closing);
    session.tabs.insert(tab.get_tab_id(), tab);

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_removed_pane_left_in_the_layout_is_reported() {
    let mut session = build_empty_session();
    let pane = register_removed_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, pane);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);

    // A removed pane kept as a leaf breaks two invariants at once: it is an
    // illegal leaf *and* a record that should have been dropped.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::RemovedPaneInLayout {
                tab_id,
                pane_id: pane,
            },
            SessionConsistencyError::LingeringRemovedRecord { pane_id: pane },
        ])
    );
}

#[test]
fn a_live_record_in_no_layout_is_reported() {
    let mut session = build_empty_session();
    let orphan = register_live_pane(&mut session);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::OrphanedPaneRecord {
            pane_id: orphan,
            pane_lifecycle: PaneLifecycle::Running,
        }])
    );
}

#[test]
fn a_removed_record_with_no_layout_is_reported() {
    let mut session = build_empty_session();
    let pane = register_removed_pane(&mut session);

    // It is not a leaf, so the layout-side check does not also fire.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::LingeringRemovedRecord {
            pane_id: pane,
        }])
    );
}

#[test]
fn a_pane_placed_in_two_tabs_is_reported() {
    let mut session = build_empty_session();
    let shared = register_live_pane(&mut session);
    let tab_a = Tab::from_root_pane(TabId::new(), "a".to_owned(), 0, shared);
    let tab_b = Tab::from_root_pane(TabId::new(), "b".to_owned(), 1, shared);
    let (tab_a_id, tab_b_id) = (tab_a.get_tab_id(), tab_b.get_tab_id());
    session.tabs.insert(tab_a_id, tab_a);
    session.tabs.insert(tab_b_id, tab_b);

    // The tabs are listed in the order `Session::tabs` walks them: ascending id.
    let mut holding_tabs = vec![tab_a_id, tab_b_id];
    holding_tabs.sort();
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::PaneInMultipleLayouts {
            pane_id: shared,
            tab_ids: holding_tabs,
        }])
    );
}

#[test]
fn a_tab_stored_under_the_wrong_key_is_reported() {
    let mut session = build_empty_session();
    let pane = register_live_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, pane);
    let tab_id = tab.get_tab_id();
    let wrong_key = TabId::new();
    session.tabs.insert(wrong_key, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::TabKeyMismatch {
            stored_tab_id: wrong_key,
            reported_tab_id: tab_id,
        }])
    );
}

#[test]
fn two_tabs_sharing_a_bar_index_are_reported() {
    let mut session = build_empty_session();
    let pane_a = register_live_pane(&mut session);
    let pane_b = register_live_pane(&mut session);
    let tab_a = Tab::from_root_pane(TabId::new(), "a".to_owned(), 0, pane_a);
    let tab_b = Tab::from_root_pane(TabId::new(), "b".to_owned(), 0, pane_b);
    session.tabs.insert(tab_a.get_tab_id(), tab_a);
    session.tabs.insert(tab_b.get_tab_id(), tab_b);

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
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);

    let foreign = SessionId::new();
    let client = Client::from_attachment(
        ClientId::new(),
        foreign,
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
    let client_id = client.get_client_id();
    session.attach_client(client);

    // `found` names the offending client's session, not this session's id.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ClientSessionMismatch {
            client_id,
            found_session_id: foreign,
        }])
    );
}

#[test]
fn a_client_active_tab_that_does_not_exist_is_reported() {
    let mut session = build_empty_session();
    let _ = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    let phantom = TabId::new();
    let client_id = attach_viewing(&mut session, phantom);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ActiveTabMissing {
            client_id,
            tab_id: phantom,
        }])
    );
}

#[test]
fn every_client_with_a_missing_active_tab_is_reported() {
    // The client walk covers each attached client, not only the first one that
    // trips a check. Clients are reported in ascending id order, the order
    // `ClientRegistry` iterates.
    let mut session = build_empty_session();
    let _ = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    let phantom = TabId::new();
    let mut viewers = [
        attach_viewing(&mut session, phantom),
        attach_viewing(&mut session, phantom),
    ];
    viewers.sort();

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::ActiveTabMissing {
                client_id: viewers[0],
                tab_id: phantom,
            },
            SessionConsistencyError::ActiveTabMissing {
                client_id: viewers[1],
                tab_id: phantom,
            },
        ])
    );
}

#[test]
fn a_client_viewing_a_gone_tab_is_not_reported_once_the_session_has_no_tabs() {
    // Closing the last tab quits the session with no successor tab to point
    // viewers at, so every client's `active_tab` names the closed tab until the
    // transport disconnects it. That state is not a violation.
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let client_id = attach_viewing(&mut session, tab);

    let _ = close_tab(&mut session, tab);

    assert!(session.tabs.is_empty());
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_active_tab(),
        tab
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_client_focus_on_an_unknown_pane_is_reported() {
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);

    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, tab);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab, ghost);

    // The tab is real and the pane is not, so the registry check and the layout
    // check both fire, in that order, and nothing else does.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client_id,
                tab_id: tab,
                pane_id: ghost,
            },
            SessionConsistencyError::FocusTargetMissing {
                client_id,
                tab_id: tab,
                pane_id: ghost,
            },
        ])
    );
}

#[test]
fn a_client_focus_in_a_missing_tab_is_reported() {
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let phantom_tab = TabId::new();
    let client_id = attach_viewing(&mut session, tab);
    // Focus remembered under a tab that is not in the session, on a real pane.
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(phantom_tab, pane);

    // The pane is real, so the registry-side focus check does not fire.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FocusTabMissing {
            client_id,
            tab_id: phantom_tab,
        }])
    );
}

#[test]
fn a_focus_in_a_missing_tab_on_a_ghost_pane_reports_the_missing_record_and_the_missing_tab() {
    // Focus naming a pane with no record, remembered under a tab the session no
    // longer holds, trips the registry check and the tab check. The layout check
    // needs a tab to look inside, so it does not also fire.
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let real_tab = get_created_tab_id(&events);

    let phantom_tab = TabId::new();
    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, real_tab);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(phantom_tab, ghost);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client_id,
                tab_id: phantom_tab,
                pane_id: ghost,
            },
            SessionConsistencyError::FocusTabMissing {
                client_id,
                tab_id: phantom_tab,
            },
        ])
    );
}

#[test]
fn a_client_focus_on_a_pane_outside_its_tab_is_reported() {
    let mut session = build_empty_session();
    let events_a = commit_test_tab(&mut session, "a".to_owned(), SystemTime::UNIX_EPOCH);
    let pane_a = created_pane_id(&events_a);
    let events_b = commit_test_tab(&mut session, "b".to_owned(), SystemTime::UNIX_EPOCH);
    let tab_b = get_created_tab_id(&events_b);

    let client_id = attach_viewing(&mut session, tab_b);
    // Focus recorded for tab_b but pointing at tab_a's pane_id: a real pane that is
    // not a leaf of the tab it is focused in.
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab_b, pane_a);

    // The pane exists in the registry, so this is a target mismatch, not a
    // missing record.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::FocusTargetMissing {
            client_id,
            tab_id: tab_b,
            pane_id: pane_a,
        }])
    );
}

/// A zoom answers to the same rule as a focus: the pane it names must be a live
/// leaf of the tab it is zoomed in.
#[test]
fn a_client_zoom_on_a_pane_outside_its_tab_is_reported() {
    let mut session = build_empty_session();
    let events_a = commit_test_tab(&mut session, "a".to_owned(), SystemTime::UNIX_EPOCH);
    let pane_a = created_pane_id(&events_a);
    let events_b = commit_test_tab(&mut session, "b".to_owned(), SystemTime::UNIX_EPOCH);
    let tab_b = get_created_tab_id(&events_b);
    let pane_b = created_pane_id(&events_b);

    let client_id = attach_viewing(&mut session, tab_b);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    // A legitimate focus in tab_b, but a zoom pointing at tab_a's pane.
    client.update_focused_pane(tab_b, pane_b);
    client.zoom_pane(tab_b, pane_a);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client_id,
            tab_id: tab_b,
            pane_id: pane_a,
        }])
    );
}

/// Zoomed on the pane this client has focused, in the tab it is viewing, is a
/// consistent state and reports nothing.
#[test]
fn a_zoom_on_the_focused_pane_is_consistent() {
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "a".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let client_id = attach_viewing(&mut session, tab);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab, pane);
    client.zoom_pane(tab, pane);

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_closed_tab_left_in_the_map_is_reported() {
    let mut session = build_empty_session();
    let pane = register_live_pane(&mut session);
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, pane);
    let tab_id = tab.get_tab_id();
    let closed = force_tab_lifecycle(&tab, "Closed");
    session.tabs.insert(tab_id, closed);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::LingeringClosedTab { tab_id }])
    );
}

/// An `Exited` pane record registered in `session`, returned by id. It holds
/// exit code `Some(0)` at `UNIX_EPOCH`. It is not `Removed`, so the orphan
/// check is the one that fires when it is a leaf nowhere.
fn register_exited_pane(session: &mut Session) -> PaneId {
    let pane_id = PaneId::new();
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, SystemTime::UNIX_EPOCH);
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
fn a_focus_on_a_ghost_pane_in_a_real_tab_reports_both_missing_record_and_missing_target() {
    // Focus pointing at a pane with no record, inside a tab that *does* exist,
    // trips two independent checks at once: the registry has no such pane
    // (`FocusPaneNotInRegistry`), and the tab's layout does not hold it either
    // (`FocusTargetMissing`). Both name the same client, tab, and pane.
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);

    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, tab);
    session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab, ghost);

    // Exactly those two — the real tab, its real pane, and the session-matched
    // client add nothing else.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client_id,
                tab_id: tab,
                pane_id: ghost,
            },
            SessionConsistencyError::FocusTargetMissing {
                client_id,
                tab_id: tab,
                pane_id: ghost,
            },
        ])
    );
}

#[test]
fn a_zoom_on_a_pane_with_no_record_is_reported() {
    // A zoom naming a pane the registry has never heard of is not a live leaf,
    // so it is reported even though the tab it is keyed under is real. The
    // client's focus sits on the real pane, so no focus check fires alongside.
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, tab);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab, pane);
    client.zoom_pane(tab, ghost);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client_id,
            tab_id: tab,
            pane_id: ghost,
        }])
    );
}

#[test]
fn a_zoom_in_a_tab_that_is_gone_is_reported() {
    // A zoom entry left under a tab that has since closed points at a real pane
    // but through a tab that is no longer in the session, so the pane is not a
    // live leaf of it — reported as `ZoomTargetMissing` naming the gone tab.
    let mut session = build_empty_session();
    let events = commit_test_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = get_created_tab_id(&events);
    let pane = created_pane_id(&events);

    let phantom_tab = TabId::new();
    let client_id = attach_viewing(&mut session, tab);
    let client = session
        .clients
        .get_client_mut_by_id(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab, pane);
    client.zoom_pane(phantom_tab, pane);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client_id,
            tab_id: phantom_tab,
            pane_id: pane,
        }])
    );
}

#[test]
fn a_pane_appearing_twice_in_one_tabs_tree_is_reported() {
    // The multi-layout check also catches a pane that is a leaf twice inside a
    // *single* tab's tree, not only one split across two tabs. Both entries name
    // the same tab id.
    let mut session = build_empty_session();
    let doubled = register_live_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::from_root_pane(tab_id, "code".to_owned(), 0, doubled);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(doubled), LayoutNode::Pane(doubled)],
    )));
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::PaneInMultipleLayouts {
            pane_id: doubled,
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
    let orphan = register_exited_pane(&mut session);

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![SessionConsistencyError::OrphanedPaneRecord {
            pane_id: orphan,
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
    let ghost = PaneId::new();
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, ghost);
    let tab_id = tab.get_tab_id();
    session.tabs.insert(tab_id, tab);
    // A live record that is a leaf nowhere.
    let orphan = register_live_pane(&mut session);

    // Both faults surface from a single call rather than only the first.
    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::PaneNotInRegistry {
                tab_id,
                pane_id: ghost,
            },
            SessionConsistencyError::OrphanedPaneRecord {
                pane_id: orphan,
                pane_lifecycle: PaneLifecycle::Running,
            },
        ])
    );
}

#[test]
fn validate_reports_two_orphan_records_in_id_order() {
    // The registry walks in id order, so two faults of one kind always come
    // out the same way round.
    let mut session = build_empty_session();
    let first_registered_pane_id = register_live_pane(&mut session);
    let second_registered_pane_id = register_live_pane(&mut session);
    let (first_pane_id, second_pane_id) = if first_registered_pane_id < second_registered_pane_id {
        (first_registered_pane_id, second_registered_pane_id)
    } else {
        (second_registered_pane_id, first_registered_pane_id)
    };

    assert_eq!(
        session.validate_session_consistency(),
        Err(vec![
            SessionConsistencyError::OrphanedPaneRecord {
                pane_id: first_pane_id,
                pane_lifecycle: PaneLifecycle::Running,
            },
            SessionConsistencyError::OrphanedPaneRecord {
                pane_id: second_pane_id,
                pane_lifecycle: PaneLifecycle::Running,
            },
        ])
    );
}

#[test]
fn a_restored_focus_history_longer_than_the_cap_still_evicts() {
    // The length is compared in `usize`: a history of 65 536 entries is over
    // the cap, not `0` entries with the low sixteen bits read alone.
    let tab = Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let mut serialized_tab = serde_json::to_value(&tab).expect("a tab serializes");
    let oversized: Vec<PaneId> = (0..65_536).map(|_| PaneId::new()).collect();
    serialized_tab["focus_mru"] = serde_json::to_value(&oversized).expect("the history serializes");
    let mut restored_tab: Tab =
        serde_json::from_value(serialized_tab).expect("the tab deserializes");

    restored_tab.record_focus_mru(PaneId::new());

    assert_eq!(
        restored_tab.list_focus_mru().len(),
        usize::from(MAX_TAB_FOCUS_MRU_ENTRY_COUNT)
    );
}
