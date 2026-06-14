use std::time::SystemTime;

use tile_core::constant::MAX_TAB_FOCUS_MRU;
use tile_core::event::Event;
use tile_core::geometry::Size;
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_layout::mode::LayoutMode;
use tile_layout::tree::LayoutNode;
use tile_pane::registry::PaneRegistry;

use super::lifecycle::{SessionLifecycle, TabLifecycle};
use super::state::{Session, Tab};
use super::tab_ops::{close_tab, new_tab};
use crate::client::{Client, ClientRegistry};

/// A client viewing `active_tab`, with a fixed viewport and `UNIX_EPOCH` attach
/// time so tests stay deterministic.
fn client_viewing(active_tab: TabId) -> Client {
    Client::new(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size { cols: 80, rows: 24 },
        active_tab,
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
    let session = Session::new(id, "main".to_owned(), ClientRegistry::new());

    assert_eq!(session.id, id);
    assert_eq!(session.name, "main");
    assert!(session.tabs.is_empty());
    assert!(session.plugin_runtime_ref.is_none());
    // The registries are part of the public shape, reachable as fields.
    let _: &PaneRegistry = &session.panes;
    let _: &ClientRegistry = &session.clients;
}

#[test]
fn a_new_tab_owns_its_layout_and_starts_unfocused() {
    let tab_id = TabId::new();
    let root = PaneId::new();
    let tab = Tab::new(tab_id, "code".to_owned(), 0, root);

    assert_eq!(tab.id, tab_id);
    assert_eq!(tab.name, "code");
    assert_eq!(tab.index, 0);
    // A fresh tab shows exactly its root pane, tiled, mid-creation, no focus yet.
    assert_eq!(tab.layout, LayoutNode::Pane(root));
    assert_eq!(tab.layout_mode, LayoutMode::Tiled);
    assert_eq!(*tab.lifecycle(), TabLifecycle::Creating);
    assert!(tab.focus_mru().is_empty());
}

#[test]
fn renaming_a_tab_changes_only_its_name() {
    let root = PaneId::new();
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, root);

    tab.name = "logs".to_owned();

    assert_eq!(tab.name, "logs");
    // Position and layout are untouched by a rename.
    assert_eq!(tab.index, 0);
    assert_eq!(tab.layout, LayoutNode::Pane(root));
}

#[test]
fn a_tab_index_can_be_reassigned() {
    let mut tab = Tab::new(TabId::new(), "code".to_owned(), 0, PaneId::new());

    tab.index = 3;

    assert_eq!(tab.index, 3);
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

    let mru = tab.focus_mru();
    assert_eq!(mru.len(), cap);
    // Newest sits at the front; the first-recorded pane is evicted.
    assert_eq!(mru[0], *panes.last().unwrap());
    assert!(!mru.contains(&panes[0]));
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
fn a_fresh_session_is_starting() {
    let session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());

    assert_eq!(*session.lifecycle(), SessionLifecycle::Starting);
}

#[test]
fn the_first_tab_moves_the_session_to_running() {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());

    let _ = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);

    // A second tab does not re-fire the start transition.
    let _ = new_tab(&mut session, "logs".to_owned(), SystemTime::UNIX_EPOCH);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
}

#[test]
fn detaching_the_last_client_parks_the_session_without_destroying_state() {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let client = client_viewing(tab);
    let client_id = client.id();
    session.attach_client(client);
    // Attaching to a running session leaves it running.
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);

    session.detach_client(client_id);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Detaching);
    // Parking is not destruction: the tabs and panes stay alive.
    assert!(!session.tabs.is_empty());
    assert!(!session.panes.is_empty());
}

#[test]
fn re_attaching_resumes_a_detached_session() {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
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
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
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
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
    let _ = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);

    session.request_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    session.complete_stop();
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopped);
}

#[test]
fn closing_the_last_tab_requests_a_stop() {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    let teardown = close_tab(&mut session, tab);

    assert!(teardown.iter().any(|event| matches!(event, Event::Quit)));
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}

#[test]
fn a_new_tab_is_refused_after_the_session_has_wound_down() {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
    let events = new_tab(&mut session, "code".to_owned(), SystemTime::UNIX_EPOCH);
    let tab = created_tab_id(&events);

    // Closing the last tab winds the session down to `Stopping`.
    let _ = close_tab(&mut session, tab);
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);

    // A late tab request must not revive a shutting-down session: the rejected
    // `FirstTabCreated` aborts the operation before any state is touched, so no
    // tab or pane is inserted and no events are emitted.
    let late = new_tab(&mut session, "late".to_owned(), SystemTime::UNIX_EPOCH);
    assert!(late.is_empty());
    assert!(session.tabs.is_empty());
    assert!(session.panes.is_empty());
    assert_eq!(*session.lifecycle(), SessionLifecycle::Stopping);
}
