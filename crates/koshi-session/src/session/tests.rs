//! Tests for session and tab state, lifecycle, and consistency validation.
//!
//! Covers session creation and lifecycle transitions, tab construction and
//! mutations, client attachment/detachment effects, and invariant validation
//! via `validate()` to catch structural inconsistencies in pane registries,
//! layout trees, client focus records, and lifecycle states.

use std::time::SystemTime;

use koshi_core::constant::MAX_TAB_FOCUS_MRU;
use koshi_core::event::{Event, PaneClosing, PaneRemoved, TabClosed};
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
fn new_tab(session: &mut Session, name: String, created_at: SystemTime) -> Vec<Event> {
    commit_new_tab(
        session,
        TabId::new(),
        PaneId::new(),
        name,
        None,
        NewPaneSpec::default(),
        created_at,
    )
    .1
}

/// A client viewing `active_tab`, with an 80x24 viewport, a fresh session id of
/// its own, and `UNIX_EPOCH` as its attach time.
fn client_viewing(active_tab: TabId) -> Client {
    Client::new(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        active_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    )
}

/// The id of the tab a `new_tab` call just created, read off its `TabCreated`.
fn created_tab_id(events: &[Event]) -> TabId {
    events
        .iter()
        .find_map(|event| match event {
            Event::TabCreated(created) => Some(created.tab_id),
            _ => None,
        })
        .expect("new_tab emits a TabCreated event")
}

#[test]
fn a_new_session_starts_empty() {
    let id = SessionId::new();
    let session = Session::new(
        id,
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(session.id, id);
    assert_eq!(session.name, "main");
    assert!(session.tabs.is_empty());
    assert!(session.panes.is_empty());
    assert!(session.clients.is_empty());
}

#[test]
fn a_new_tab_owns_its_layout_and_starts_unfocused() {
    let tab_id = TabId::new();
    let root = PaneId::new();
    let tab = Tab::new(tab_id, "code".to_owned(), 0, root);

    assert_eq!(tab.id(), tab_id);
    assert_eq!(tab.name(), "code");
    assert_eq!(tab.index(), 0);
    // A fresh tab shows exactly its root pane, mid-creation, no focus yet. It
    // carries no layout mode of its own: whether a pane is zoomed belongs to a
    // client's view, not to the tab.
    assert_eq!(*tab.layout(), LayoutNode::Pane(root));
    assert_eq!(*tab.lifecycle(), TabLifecycle::Creating);
    assert!(tab.focus_mru().is_empty());
}

#[test]
fn a_tab_index_can_be_reassigned() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());

    tab.update_index(3);

    assert_eq!(tab.index(), 3);
}

#[test]
fn record_focus_orders_newest_first() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());

    tab.record_focus_mru(a);
    tab.record_focus_mru(b);
    tab.record_focus_mru(c);

    assert_eq!(tab.focus_mru().to_vec(), vec![c, b, a]);
}

#[test]
fn re_focusing_moves_to_front_without_duplicating() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (a, b) = (PaneId::new(), PaneId::new());

    tab.record_focus_mru(a);
    tab.record_focus_mru(b);
    tab.record_focus_mru(a);

    // `a` returns to the front; it is not stored twice.
    assert_eq!(tab.focus_mru().to_vec(), vec![a, b]);
}

#[test]
fn focus_mru_is_capped_dropping_the_oldest() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let cap = MAX_TAB_FOCUS_MRU as usize;

    // Record one more distinct pane than the cap allows.
    let panes: Vec<PaneId> = (0..=cap).map(|_| PaneId::new()).collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }

    // Newest first, with the first-recorded pane evicted: every other pane keeps
    // its place in recording order.
    let surviving_newest_first: Vec<PaneId> = panes[1..].iter().rev().copied().collect();
    assert_eq!(tab.focus_mru().to_vec(), surviving_newest_first);
}

#[test]
fn focus_mru_at_exactly_the_cap_evicts_nothing() {
    // The boundary just below the eviction case above: recording exactly
    // `MAX_TAB_FOCUS_MRU` distinct panes must keep every one of them.
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let cap = MAX_TAB_FOCUS_MRU as usize;

    let panes: Vec<PaneId> = (0..cap).map(|_| PaneId::new()).collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }

    let newest_first: Vec<PaneId> = panes.iter().rev().copied().collect();
    assert_eq!(tab.focus_mru().to_vec(), newest_first);
}

#[test]
fn re_recording_an_existing_pane_at_the_cap_moves_it_front_without_evicting() {
    // Re-recording an entry a full history already holds evicts nothing: the
    // duplicate is dropped before the length is checked.
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let cap = MAX_TAB_FOCUS_MRU as usize;
    let panes: Vec<PaneId> = (0..cap).map(|_| PaneId::new()).collect();
    for &pane in &panes {
        tab.record_focus_mru(pane);
    }
    // The re-recorded pane comes from the middle of the history. The back entry
    // is the one the cap evicts on its own.
    let middle = panes[cap / 2];

    tab.record_focus_mru(middle);

    // `middle` moves to the front and every other pane keeps its order behind it.
    let mut expected: Vec<PaneId> = panes.iter().rev().copied().collect();
    expected.retain(|&pane| pane != middle);
    expected.insert(0, middle);
    assert_eq!(tab.focus_mru().to_vec(), expected);
}

#[test]
fn recording_the_same_pane_twice_keeps_one_entry() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let pane = PaneId::new();

    tab.record_focus_mru(pane);
    tab.record_focus_mru(pane);

    assert_eq!(tab.focus_mru().to_vec(), vec![pane]);
}

#[test]
fn remove_focus_mru_drops_only_the_named_pane() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    tab.record_focus_mru(a);
    tab.record_focus_mru(b);
    tab.record_focus_mru(c); // newest first: [c, b, a]

    tab.remove_focus_mru(b);

    assert_eq!(tab.focus_mru().to_vec(), vec![c, a]);
}

#[test]
fn remove_focus_mru_for_a_pane_never_recorded_is_a_noop() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let recorded = PaneId::new();
    tab.record_focus_mru(recorded);

    tab.remove_focus_mru(PaneId::new());

    assert_eq!(tab.focus_mru().to_vec(), vec![recorded]);
}

#[test]
fn remove_focus_mru_on_an_empty_history_is_a_noop() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());

    tab.remove_focus_mru(PaneId::new());

    assert!(tab.focus_mru().is_empty());
}

#[test]
fn the_starting_lock_is_taken_once() {
    let mut session = Session::new(
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
    let mut session = Session::new(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert!(!session.take_start_lock());
}

#[test]
fn the_starting_lock_survives_a_serde_round_trip() {
    let mut session = Session::new(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.start_locked = true;

    let json = serde_json::to_string(&session).expect("serialize");
    let restored: Session = serde_json::from_str(&json).expect("deserialize");

    assert!(
        restored.start_locked,
        "a session server that restarts before any client attaches still locks the first one"
    );
}

#[test]
fn the_starting_lock_is_stored_as_a_plain_json_bool() {
    // Pins the stored shape: the member is named `start_locked` and holds a
    // JSON boolean.
    let mut session = Session::new(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session.start_locked = true;

    let value = serde_json::to_value(&session).expect("serialize");

    assert_eq!(value["start_locked"], serde_json::Value::Bool(true));
}

#[test]
fn a_stored_session_without_the_lock_key_reads_back_unlocked() {
    // A stored session with no `start_locked` member reads the field back as
    // `false` through `#[serde(default)]`.
    let session = Session::new(
        SessionId::new(),
        "work".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let mut value = serde_json::to_value(&session).expect("serialize");
    value
        .as_object_mut()
        .expect("a session serializes to a JSON object")
        .remove("start_locked")
        .expect("the key is present before it is dropped");

    let mut restored: Session =
        serde_json::from_value(value).expect("a session without the key deserializes");

    assert!(!restored.start_locked);
    assert!(!restored.take_start_lock());
}

#[test]
fn a_tab_survives_a_serde_round_trip() {
    let root = PaneId::new();
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 2, root);
    tab.record_focus_mru(root);

    let json = serde_json::to_string(&tab).expect("serialize");
    let restored: Tab = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(tab, restored);
}

#[test]
fn a_tabs_name_is_stored_as_a_plain_json_string() {
    // Pins the stored shape: the member is named `name` and holds a JSON
    // string, not a nested object.
    let tab = Tab::new(TabId::new(), "code".to_owned(), 2, PaneId::new());

    let value = serde_json::to_value(&tab).expect("serialize");

    assert_eq!(value["name"], serde_json::Value::String("code".to_owned()));
}

#[test]
fn a_fresh_session_is_starting() {
    let session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn the_first_tab_moves_the_session_to_running() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );

    let _ = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);

    // A second tab does not re-fire the start transition.
    let _ = new_tab(&mut session, "logs".to_owned(), SystemTime::UNIX_EPOCH);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_the_last_client_parks_the_session_without_destroying_state() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let pane = created_pane_id(&events);

    let client = client_viewing(tab);
    let client_id = client.id();
    session.attach_client(client);
    // Attaching to a running session leaves it running.
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);

    session.detach_client(client_id);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);
    // Parking is not destruction: the tab and its pane stay alive.
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        vec![tab]
    );
    assert_eq!(session.panes.get(pane).expect("the pane stays").id(), pane);
}

#[test]
fn re_attaching_resumes_a_detached_session() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let first = client_viewing(tab);
    let first_id = first.id();
    session.attach_client(first);
    session.detach_client(first_id);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);

    session.attach_client(client_viewing(tab));
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_one_of_several_clients_keeps_the_session_running() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let a = client_viewing(tab);
    let a_id = a.id();
    let b = client_viewing(tab);
    let b_id = b.id();
    session.attach_client(a);
    session.attach_client(b);

    session.detach_client(a_id);
    // One client remains, so the session is still running.
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);

    session.detach_client(b_id);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);
}

#[test]
fn requesting_then_completing_a_stop_walks_to_stopped() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let _ = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    session.complete_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopped);
}

#[test]
fn closing_the_last_tab_requests_a_stop() {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
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
            Event::Quit,
        ]
    );
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

/// A fresh, empty session with a random id.
fn empty_session() -> Session {
    Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// A `Running` pane record registered in `session`, returned by id. Live and a
/// valid layout leaf, so on its own it trips no pane-level consistency check.
fn register_live_pane(session: &mut Session) -> PaneId {
    let id = PaneId::new();
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("Spawning -> Running is a legal transition");
    session
        .panes
        .insert(record)
        .expect("a fresh pane id is unique");
    id
}

/// A `Removed` pane record registered in `session`, returned by id.
fn register_removed_pane(session: &mut Session) -> PaneId {
    let id = PaneId::new();
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            since: SystemTime::UNIX_EPOCH,
        })
        .expect("Spawning -> Closing is a legal transition");
    record
        .update_lifecycle(PaneLifecycleEvent::Cleaned)
        .expect("Closing -> Removed is a legal transition");
    session
        .panes
        .insert(record)
        .expect("a fresh pane id is unique");
    id
}

/// A `Closing` pane record registered in `session`, returned by id. It still
/// holds a record and is still a legal layout leaf.
fn register_closing_pane(session: &mut Session) -> PaneId {
    let id = PaneId::new();
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record
        .update_lifecycle(PaneLifecycleEvent::CloseRequested {
            since: SystemTime::UNIX_EPOCH,
        })
        .expect("Spawning -> Closing is a legal transition");
    session
        .panes
        .insert(record)
        .expect("a fresh pane id is unique");
    id
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
    let client = Client::new(
        ClientId::new(),
        session.id,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        active_tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let client_id = client.id();
    session.attach_client(client);
    client_id
}

/// A clone of `tab` whose private `lifecycle` field is set to `lifecycle`,
/// built by rewriting that member of the tab's JSON and reading it back.
/// `lifecycle` is a [`TabLifecycle`] variant name, such as `"Closed"`. Panics
/// when the name is not one of them.
fn force_tab_lifecycle(tab: &Tab, lifecycle: &str) -> Tab {
    let mut value = serde_json::to_value(tab).expect("a tab serializes");
    value["lifecycle"] = serde_json::Value::String(lifecycle.to_owned());
    serde_json::from_value(value).expect("a tab with a forced lifecycle deserializes")
}

#[test]
fn a_freshly_built_session_is_consistent() {
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let pane = created_pane_id(&events);

    let client_id = attach_viewing(&mut session, tab);
    session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab, pane);

    session
        .validate()
        .expect("a session built through the normal operations is consistent");
}

#[test]
fn a_layout_leaf_with_no_record_is_reported() {
    let mut session = empty_session();
    let ghost = PaneId::new();
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, ghost);
    let tab_id = tab.id();
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::PaneNotInRegistry {
            tab: tab_id,
            pane: ghost,
        }])
    );
}

#[test]
fn a_session_with_no_tabs_panes_or_clients_is_consistent() {
    assert_eq!(empty_session().validate(), Ok(()));
}

#[test]
fn a_closing_pane_still_in_the_layout_is_consistent() {
    // Only a `Removed` pane is an illegal leaf. A pane in `Closing` keeps both
    // its leaf and its registry record, so neither side reports it.
    let mut session = empty_session();
    let closing = register_closing_pane(&mut session);
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, closing);
    session.tabs.insert(tab.id(), tab);

    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn a_removed_pane_left_in_the_layout_is_reported() {
    let mut session = empty_session();
    let pane = register_removed_pane(&mut session);
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, pane);
    let tab_id = tab.id();
    session.tabs.insert(tab_id, tab);

    // A removed pane kept as a leaf breaks two invariants at once: it is an
    // illegal leaf *and* a record that should have been dropped.
    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::RemovedPaneInLayout { tab: tab_id, pane },
            SessionConsistencyError::LingeringRemovedRecord { pane },
        ])
    );
}

#[test]
fn a_live_record_in_no_layout_is_reported() {
    let mut session = empty_session();
    let orphan = register_live_pane(&mut session);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::OrphanedPaneRecord {
            pane: orphan,
            lifecycle: PaneLifecycle::Running,
        }])
    );
}

#[test]
fn a_removed_record_with_no_layout_is_reported() {
    let mut session = empty_session();
    let pane = register_removed_pane(&mut session);

    // It is not a leaf, so the layout-side check does not also fire.
    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::LingeringRemovedRecord {
            pane
        }])
    );
}

#[test]
fn a_pane_placed_in_two_tabs_is_reported() {
    let mut session = empty_session();
    let shared = register_live_pane(&mut session);
    let tab_a = Tab::new(TabId::new(), "a".to_owned(), 0, shared);
    let tab_b = Tab::new(TabId::new(), "b".to_owned(), 1, shared);
    let (tab_a_id, tab_b_id) = (tab_a.id(), tab_b.id());
    session.tabs.insert(tab_a_id, tab_a);
    session.tabs.insert(tab_b_id, tab_b);

    // The tabs are listed in the order `Session::tabs` walks them: ascending id.
    let mut holding_tabs = vec![tab_a_id, tab_b_id];
    holding_tabs.sort();
    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::PaneInMultipleLayouts {
            pane: shared,
            tabs: holding_tabs,
        }])
    );
}

#[test]
fn a_tab_stored_under_the_wrong_key_is_reported() {
    let mut session = empty_session();
    let pane = register_live_pane(&mut session);
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, pane);
    let tab_id = tab.id();
    let wrong_key = TabId::new();
    session.tabs.insert(wrong_key, tab);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::TabKeyMismatch {
            key: wrong_key,
            tab_id,
        }])
    );
}

#[test]
fn two_tabs_sharing_a_bar_index_are_reported() {
    let mut session = empty_session();
    let pane_a = register_live_pane(&mut session);
    let pane_b = register_live_pane(&mut session);
    let tab_a = Tab::new(TabId::new(), "a".to_owned(), 0, pane_a);
    let tab_b = Tab::new(TabId::new(), "b".to_owned(), 0, pane_b);
    session.tabs.insert(tab_a.id(), tab_a);
    session.tabs.insert(tab_b.id(), tab_b);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::DuplicateTabIndex {
            index: 0
        }])
    );
}

#[test]
fn a_client_belonging_to_another_session_is_reported() {
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let foreign = SessionId::new();
    let client = Client::new(
        ClientId::new(),
        foreign,
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        None,
        tab,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    let client_id = client.id();
    session.attach_client(client);

    // `found` names the offending client's session, not this session's id.
    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::ClientSessionMismatch {
            client: client_id,
            found: foreign,
        }])
    );
}

#[test]
fn a_client_active_tab_that_does_not_exist_is_reported() {
    let mut session = empty_session();
    let _ = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    let phantom = TabId::new();
    let client_id = attach_viewing(&mut session, phantom);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::ActiveTabMissing {
            client: client_id,
            tab: phantom,
        }])
    );
}

#[test]
fn every_client_with_a_missing_active_tab_is_reported() {
    // The client walk covers each attached client, not only the first one that
    // trips a check. Clients are reported in ascending id order, the order
    // `ClientRegistry` iterates.
    let mut session = empty_session();
    let _ = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    let phantom = TabId::new();
    let mut viewers = [
        attach_viewing(&mut session, phantom),
        attach_viewing(&mut session, phantom),
    ];
    viewers.sort();

    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::ActiveTabMissing {
                client: viewers[0],
                tab: phantom,
            },
            SessionConsistencyError::ActiveTabMissing {
                client: viewers[1],
                tab: phantom,
            },
        ])
    );
}

#[test]
fn a_client_viewing_a_gone_tab_is_not_reported_once_the_session_has_no_tabs() {
    // Closing the last tab quits the session with no successor tab to point
    // viewers at, so every client's `active_tab` names the closed tab until the
    // transport disconnects it. That state is not a violation.
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let client_id = attach_viewing(&mut session, tab);

    let _ = close_tab(&mut session, tab);

    assert!(session.tabs.is_empty());
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("the client stays attached")
            .active_tab(),
        tab
    );
    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn a_client_focus_on_an_unknown_pane_is_reported() {
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, tab);
    session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab, ghost);

    // The tab is real and the pane is not, so the registry check and the layout
    // check both fire, in that order, and nothing else does.
    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client: client_id,
                tab,
                pane: ghost,
            },
            SessionConsistencyError::FocusTargetMissing {
                client: client_id,
                tab,
                pane: ghost,
            },
        ])
    );
}

#[test]
fn a_client_focus_in_a_missing_tab_is_reported() {
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let pane = created_pane_id(&events);

    let phantom_tab = TabId::new();
    let client_id = attach_viewing(&mut session, tab);
    // Focus remembered under a tab that is not in the session, on a real pane.
    session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached")
        .update_focused_pane(phantom_tab, pane);

    // The pane is real, so the registry-side focus check does not fire.
    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::FocusTabMissing {
            client: client_id,
            tab: phantom_tab,
        }])
    );
}

#[test]
fn a_focus_in_a_missing_tab_on_a_ghost_pane_reports_the_missing_record_and_the_missing_tab() {
    // Focus naming a pane with no record, remembered under a tab the session no
    // longer holds, trips the registry check and the tab check. The layout check
    // needs a tab to look inside, so it does not also fire.
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let real_tab = created_tab_id(&events);

    let phantom_tab = TabId::new();
    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, real_tab);
    session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached")
        .update_focused_pane(phantom_tab, ghost);

    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client: client_id,
                tab: phantom_tab,
                pane: ghost,
            },
            SessionConsistencyError::FocusTabMissing {
                client: client_id,
                tab: phantom_tab,
            },
        ])
    );
}

#[test]
fn a_client_focus_on_a_pane_outside_its_tab_is_reported() {
    let mut session = empty_session();
    let events_a = new_tab(&mut session, "a".to_owned(), SystemTime::UNIX_EPOCH);
    let pane_a = created_pane_id(&events_a);
    let events_b = new_tab(&mut session, "b".to_owned(), SystemTime::UNIX_EPOCH);
    let tab_b = created_tab_id(&events_b);

    let client_id = attach_viewing(&mut session, tab_b);
    // Focus recorded for tab_b but pointing at tab_a's pane: a real pane that is
    // not a leaf of the tab it is focused in.
    session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab_b, pane_a);

    // The pane exists in the registry, so this is a target mismatch, not a
    // missing record.
    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::FocusTargetMissing {
            client: client_id,
            tab: tab_b,
            pane: pane_a,
        }])
    );
}

/// A zoom answers to the same rule as a focus: the pane it names must be a live
/// leaf of the tab it is zoomed in.
#[test]
fn a_client_zoom_on_a_pane_outside_its_tab_is_reported() {
    let mut session = empty_session();
    let events_a = new_tab(&mut session, "a".to_owned(), SystemTime::UNIX_EPOCH);
    let pane_a = created_pane_id(&events_a);
    let events_b = new_tab(&mut session, "b".to_owned(), SystemTime::UNIX_EPOCH);
    let tab_b = created_tab_id(&events_b);
    let pane_b = created_pane_id(&events_b);

    let client_id = attach_viewing(&mut session, tab_b);
    let client = session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached");
    // A legitimate focus in tab_b, but a zoom pointing at tab_a's pane.
    client.update_focused_pane(tab_b, pane_b);
    client.zoom_pane(tab_b, pane_a);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client: client_id,
            tab: tab_b,
            pane: pane_a,
        }])
    );
}

/// Zoomed on the pane this client has focused, in the tab it is viewing, is a
/// consistent state and reports nothing.
#[test]
fn a_zoom_on_the_focused_pane_is_consistent() {
    let mut session = empty_session();
    let events = new_tab(&mut session, "a".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let pane = created_pane_id(&events);

    let client_id = attach_viewing(&mut session, tab);
    let client = session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab, pane);
    client.zoom_pane(tab, pane);

    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn a_closed_tab_left_in_the_map_is_reported() {
    let mut session = empty_session();
    let pane = register_live_pane(&mut session);
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, pane);
    let tab_id = tab.id();
    let closed = force_tab_lifecycle(&tab, "Closed");
    session.tabs.insert(tab_id, closed);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::LingeringClosedTab {
            tab: tab_id
        }])
    );
}

/// An `Exited` pane record registered in `session`, returned by id. It holds
/// exit code `Some(0)` at `UNIX_EPOCH`. It is not `Removed`, so the orphan
/// check is the one that fires when it is a leaf nowhere.
fn register_exited_pane(session: &mut Session) -> PaneId {
    let id = PaneId::new();
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
        .expect("Spawning -> Running is a legal transition");
    record
        .update_lifecycle(PaneLifecycleEvent::ProcessExited {
            code: Some(0),
            at: SystemTime::UNIX_EPOCH,
        })
        .expect("Running -> Exited is a legal transition");
    session
        .panes
        .insert(record)
        .expect("a fresh pane id is unique");
    id
}

#[test]
fn a_focus_on_a_ghost_pane_in_a_real_tab_reports_both_missing_record_and_missing_target() {
    // Focus pointing at a pane with no record, inside a tab that *does* exist,
    // trips two independent checks at once: the registry has no such pane
    // (`FocusPaneNotInRegistry`), and the tab's layout does not hold it either
    // (`FocusTargetMissing`). Both name the same client, tab, and pane.
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, tab);
    session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached")
        .update_focused_pane(tab, ghost);

    // Exactly those two — the real tab, its real pane, and the session-matched
    // client add nothing else.
    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::FocusPaneNotInRegistry {
                client: client_id,
                tab,
                pane: ghost,
            },
            SessionConsistencyError::FocusTargetMissing {
                client: client_id,
                tab,
                pane: ghost,
            },
        ])
    );
}

#[test]
fn a_zoom_on_a_pane_with_no_record_is_reported() {
    // A zoom naming a pane the registry has never heard of is not a live leaf,
    // so it is reported even though the tab it is keyed under is real. The
    // client's focus sits on the real pane, so no focus check fires alongside.
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let pane = created_pane_id(&events);

    let ghost = PaneId::new();
    let client_id = attach_viewing(&mut session, tab);
    let client = session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab, pane);
    client.zoom_pane(tab, ghost);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client: client_id,
            tab,
            pane: ghost,
        }])
    );
}

#[test]
fn a_zoom_in_a_tab_that_is_gone_is_reported() {
    // A zoom entry left under a tab that has since closed points at a real pane
    // but through a tab that is no longer in the session, so the pane is not a
    // live leaf of it — reported as `ZoomTargetMissing` naming the gone tab.
    let mut session = empty_session();
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);
    let pane = created_pane_id(&events);

    let phantom_tab = TabId::new();
    let client_id = attach_viewing(&mut session, tab);
    let client = session
        .clients
        .get_mut(client_id)
        .expect("the client was just attached");
    client.update_focused_pane(tab, pane);
    client.zoom_pane(phantom_tab, pane);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::ZoomTargetMissing {
            client: client_id,
            tab: phantom_tab,
            pane,
        }])
    );
}

#[test]
fn a_pane_appearing_twice_in_one_tabs_tree_is_reported() {
    // The multi-layout check also catches a pane that is a leaf twice inside a
    // *single* tab's tree, not only one split across two tabs. Both entries name
    // the same tab id.
    let mut session = empty_session();
    let doubled = register_live_pane(&mut session);
    let tab_id = TabId::new();
    let mut tab = Tab::new(tab_id, "code".to_owned(), 0, doubled);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![LayoutNode::Pane(doubled), LayoutNode::Pane(doubled)],
    )));
    session.tabs.insert(tab_id, tab);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::PaneInMultipleLayouts {
            pane: doubled,
            tabs: vec![tab_id, tab_id],
        }])
    );
}

#[test]
fn an_exited_orphan_record_is_reported() {
    // The orphan check covers `Exited` records, not just live ones: a dead
    // placeholder pane that is a leaf nowhere is reported, and the reported
    // lifecycle is the exact `Exited` state it holds.
    let mut session = empty_session();
    let orphan = register_exited_pane(&mut session);

    assert_eq!(
        session.validate(),
        Err(vec![SessionConsistencyError::OrphanedPaneRecord {
            pane: orphan,
            lifecycle: PaneLifecycle::Exited {
                code: Some(0),
                at: SystemTime::UNIX_EPOCH,
            },
        }])
    );
}

#[test]
fn every_violation_is_collected_in_one_pass() {
    let mut session = empty_session();
    // A layout leaf with no record.
    let ghost = PaneId::new();
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, ghost);
    let tab_id = tab.id();
    session.tabs.insert(tab_id, tab);
    // A live record that is a leaf nowhere.
    let orphan = register_live_pane(&mut session);

    // Both faults surface from a single call rather than only the first.
    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::PaneNotInRegistry {
                tab: tab_id,
                pane: ghost,
            },
            SessionConsistencyError::OrphanedPaneRecord {
                pane: orphan,
                lifecycle: PaneLifecycle::Running,
            },
        ])
    );
}

#[test]
fn validate_reports_two_orphan_records_in_id_order() {
    // The registry walks in id order, so two faults of one kind always come
    // out the same way round.
    let mut session = empty_session();
    let a = register_live_pane(&mut session);
    let b = register_live_pane(&mut session);
    let (first, second) = if a < b { (a, b) } else { (b, a) };

    assert_eq!(
        session.validate(),
        Err(vec![
            SessionConsistencyError::OrphanedPaneRecord {
                pane: first,
                lifecycle: PaneLifecycle::Running,
            },
            SessionConsistencyError::OrphanedPaneRecord {
                pane: second,
                lifecycle: PaneLifecycle::Running,
            },
        ])
    );
}

#[test]
fn a_restored_focus_history_longer_than_the_cap_still_evicts() {
    // The length is compared in `usize`: a history of 65 536 entries is over
    // the cap, not `0` entries with the low sixteen bits read alone.
    let tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());
    let mut value = serde_json::to_value(&tab).expect("a tab serializes");
    let oversized: Vec<PaneId> = (0..65_536).map(|_| PaneId::new()).collect();
    value["focus_mru"] = serde_json::to_value(&oversized).expect("the history serializes");
    let mut restored: Tab = serde_json::from_value(value).expect("the tab deserializes");

    restored.record_focus_mru(PaneId::new());

    assert_eq!(restored.focus_mru().len(), usize::from(MAX_TAB_FOCUS_MRU));
}
