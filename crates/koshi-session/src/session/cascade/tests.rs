//! Tests for pane lifecycle cascades: removal, focus repair, and tab closure.
//!
//! These tests verify that pane removal (via exit or user action) correctly
//! cascades: focus is repaired on all clients, sibling panes inherit focus,
//! empty tabs close, and the session quits when no tabs remain. Also tests
//! the inverse: on child process exit, exit policies (e.g. close on exit,
//! respawn shell) determine whether a pane is removed or restarted.

use std::time::SystemTime;

use koshi_core::event::{Event, LayoutChanged, PaneClosing, PaneRemoved};
use koshi_core::geometry::{Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::{LayoutChild, LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::{PaneClosePolicy, PaneExitPolicy};
use koshi_pane::pane::state::PaneRecord;

use super::{on_child_exit, remove_pane_cascade};
use crate::client::{Client, ClientRegistry};
use crate::session::policy::EmptyTabPolicy;
use crate::session::state::{Session, Tab};

/// Standard terminal size (80×24) used across all test fixtures.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// Returns a rect covering the full viewport (80×24), used as the layout bounds when solving tab geometry.
fn rect() -> Rect {
    Rect::new(Point { x: 0, y: 0 }, VIEWPORT)
}

/// Creates a pane record with the specified lifecycle state and exit policy.
///
/// The record starts as a fresh `Spawning` state and is transitioned to the
/// requested lifecycle through legal event sequences (since lifecycle state
/// can only change via explicit `update_lifecycle` calls). Timestamps use
/// `UNIX_EPOCH` to keep tests deterministic. Close policy defaults to `Force`.
fn record(id: PaneId, lifecycle: PaneLifecycle, exit_policy: PaneExitPolicy) -> PaneRecord {
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record.close_policy = PaneClosePolicy::Force;
    record.exit_policy = exit_policy;
    walk_lifecycle(&mut record, lifecycle);
    record
}

/// Transitions a pane record from its current state to the target lifecycle state
/// by emitting the legal sequence of intermediate events.
fn walk_lifecycle(record: &mut PaneRecord, target: PaneLifecycle) {
    match target {
        PaneLifecycle::Spawning => {}
        PaneLifecycle::Running => {
            record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Exited { code, at } => {
            record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
            record
                .update_lifecycle(PaneLifecycleEvent::ProcessExited { code, at })
                .expect("walk_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Closing { since } => {
            record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
            record
                .update_lifecycle(PaneLifecycleEvent::CloseRequested { since })
                .expect("walk_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Removed => {
            record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
            record
                .update_lifecycle(PaneLifecycleEvent::CloseRequested {
                    since: SystemTime::UNIX_EPOCH,
                })
                .expect("walk_lifecycle drives only legal transitions");
            record
                .update_lifecycle(PaneLifecycleEvent::Cleaned)
                .expect("walk_lifecycle drives only legal transitions");
        }
    }
}

/// Creates a tab containing a single pane.
fn single_pane_tab(tab_id: TabId, pane: PaneId) -> Tab {
    Tab::new(tab_id, "code".to_owned(), 0, pane)
}

/// Creates a single-pane tab at the given display position (tab index).
fn tab_with_index(tab_id: TabId, pane: PaneId, index: usize) -> Tab {
    let mut tab = single_pane_tab(tab_id, pane);
    tab.update_index(index);
    tab
}

/// Creates a tab split horizontally (left/right) between two panes with equal widths.
fn two_pane_tab(tab_id: TabId, left: PaneId, right: PaneId) -> Tab {
    let mut tab = Tab::new(tab_id, "code".to_owned(), 0, left);
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutChild::new(LayoutNode::Pane(left)),
            LayoutChild::new(LayoutNode::Pane(right)),
        ],
    )));
    tab
}

/// Creates a client viewing the given tab with the given pane focused.
/// The client carries the provided session id so it can be attached to the session
/// without validation errors (see [`Session::validate`]).
fn focused_client(session_id: SessionId, tab_id: TabId, pane: PaneId) -> Client {
    let mut client = Client::new(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        tab_id,
    );
    client.update_focused_pane(tab_id, pane);
    client
}

/// Creates a session with the given tabs and pane records, but no attached clients.
/// Clients can be attached afterward with [`Session::attach_client`], using the
/// session's own id to keep the fixture valid.
fn session_with(tabs: Vec<Tab>, records: Vec<PaneRecord>) -> Session {
    let mut session = Session::new(SessionId::new(), "main".to_owned(), ClientRegistry::new());
    for tab in tabs {
        session.tabs.insert(tab.id(), tab);
    }
    for pane in records {
        session.panes.insert(pane).expect("unique pane id");
    }
    session
}

#[test]
fn fixtures_build_a_consistent_session() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    session.attach_client(focused_client(session.id, tab_id, a));

    assert_eq!(session.validate(), Ok(()));
}

#[test]
fn removing_a_focused_pane_focuses_a_survivor() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(&mut session, tab_id, a, rect(), EmptyTabPolicy::CloseTab);

    // The survivor inherits focus, on the client and in the event stream.
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PaneFocused(p) if p.pane_id == b && p.tab_id == tab_id)));
    // The removed pane is gone from the registry and the layout collapsed to B.
    assert!(session.panes.get(a).is_none());
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![b]);
    // Removal facts are reported.
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PaneClosing(p) if p.pane_id == a)));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PaneRemoved(p) if p.pane_id == a && p.tab_id == tab_id)));
}

#[test]
fn removing_a_nonfocused_pane_leaves_focus_untouched() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, b); // focused on the survivor
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(&mut session, tab_id, a, rect(), EmptyTabPolicy::CloseTab);

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert!(!events.iter().any(|e| matches!(e, Event::PaneFocused(_))));
}

#[test]
fn collapsing_a_multi_pane_tab_emits_layout_changed() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );

    let events = remove_pane_cascade(&mut session, tab_id, a, rect(), EmptyTabPolicy::CloseTab);

    // The survivor's geometry changed when the leaf collapsed, so the cascade
    // announces it — a subscriber re-solves on LayoutChanged.
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::LayoutChanged(changed) if changed.tab_id == tab_id)));
}

#[test]
fn focus_repair_runs_for_every_client_on_the_removed_pane() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let first = focused_client(session.id, tab_id, a);
    let second = focused_client(session.id, tab_id, a);
    let (first_id, second_id) = (first.id(), second.id());
    session.attach_client(first);
    session.attach_client(second);

    let _ = remove_pane_cascade(&mut session, tab_id, a, rect(), EmptyTabPolicy::CloseTab);

    assert_eq!(
        session.clients.get(first_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(
        session.clients.get(second_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
}

#[test]
fn removing_a_focused_pane_with_no_room_to_refocus_clears_focus() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, a);
    let client_id = client.id();
    session.attach_client(client);

    // A rect narrower than `MIN_PANE_SIZE` suppresses the survivor, so focus
    // recovery finds no focusable pane though the tab still holds one.
    let tiny = Rect::new(Point { x: 0, y: 0 }, Size { cols: 1, rows: 1 });
    let events = remove_pane_cascade(&mut session, tab_id, a, tiny, EmptyTabPolicy::CloseTab);

    // The overlay is reported and the client's stale focus on the gone pane is
    // cleared rather than left dangling.
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TerminalTooSmallEntered(t) if t.client_id == client_id)));
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        None
    );
    // The survivor stays — the tab is not empty, just unfocusable for now.
    assert!(session.panes.get(b).is_some());
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![b]);
}

#[test]
fn the_removed_pane_leaves_the_tab_focus_history() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut tab = two_pane_tab(tab_id, a, b);
    tab.record_focus_mru(b);
    tab.record_focus_mru(a); // history: [a, b]
    let mut session = session_with(
        vec![tab],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );

    let _ = remove_pane_cascade(&mut session, tab_id, a, rect(), EmptyTabPolicy::CloseTab);

    let history = session.tabs[&tab_id].focus_mru();
    assert!(!history.contains(&a));
    assert!(history.contains(&b));
}

#[test]
fn removing_the_last_pane_closes_the_tab_and_quits() {
    let tab_id = TabId::new();
    let only = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![record(
            only,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    let events = remove_pane_cascade(&mut session, tab_id, only, rect(), EmptyTabPolicy::CloseTab);

    assert!(session.tabs.is_empty());
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TabClosed(t) if t.tab_id == tab_id)));
    assert!(events.iter().any(|e| matches!(e, Event::Quit)));
    // The tab is gone, so this is a tab-close, not a within-tab layout change:
    // no LayoutChanged is emitted for a tab that no longer exists.
    assert!(!events.iter().any(|e| matches!(e, Event::LayoutChanged(_))));
}

#[test]
fn closing_the_last_pane_of_one_tab_among_several_does_not_quit() {
    let (tab_one, tab_two) = (TabId::new(), TabId::new());
    let (pane_one, pane_two) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            single_pane_tab(tab_one, pane_one),
            single_pane_tab(tab_two, pane_two),
        ],
        vec![
            record(
                pane_one,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            record(
                pane_two,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );

    let events = remove_pane_cascade(
        &mut session,
        tab_one,
        pane_one,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    assert!(!session.tabs.contains_key(&tab_one));
    assert!(session.tabs.contains_key(&tab_two));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::TabClosed(t) if t.tab_id == tab_one)));
    assert!(!events.iter().any(|e| matches!(e, Event::Quit)));
}

#[test]
fn removing_an_unknown_pane_emits_nothing() {
    let tab_id = TabId::new();
    let only = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![record(
            only,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        PaneId::new(),
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    assert!(events.is_empty());
    assert!(session.panes.get(only).is_some());
    assert!(session.tabs.contains_key(&tab_id));
}

#[test]
fn a_respawn_shell_pane_returns_to_spawning() {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![record(
            pane,
            PaneLifecycle::Running,
            PaneExitPolicy::RespawnShell,
        )],
    );

    let events = on_child_exit(
        &mut session,
        tab_id,
        pane,
        Some(1),
        SystemTime::UNIX_EPOCH,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    let kept = session.panes.get(pane).expect("the pane is kept");
    assert_eq!(*kept.lifecycle(), PaneLifecycle::Spawning);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PaneProcessExited(p) if p.pane_id == pane)));
    assert!(!events.iter().any(|e| matches!(e, Event::PaneRemoved(_))));
}

#[test]
fn a_close_on_exit_pane_runs_the_removal_cascade() {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![record(
            pane,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    let events = on_child_exit(
        &mut session,
        tab_id,
        pane,
        Some(0),
        SystemTime::UNIX_EPOCH,
        rect(),
        EmptyTabPolicy::CloseTab,
    );

    assert!(session.panes.get(pane).is_none());
    assert!(session.tabs.is_empty());
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::PaneProcessExited(p) if p.pane_id == pane)));
    assert!(events.iter().any(|e| matches!(e, Event::PaneRemoved(_))));
    assert!(events.iter().any(|e| matches!(e, Event::Quit)));
}

#[test]
fn closing_a_clients_active_tab_moves_it_to_the_previous_tab() {
    let (left, middle, right) = (TabId::new(), TabId::new(), TabId::new());
    let (a, b, c) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            tab_with_index(left, a, 0),
            tab_with_index(middle, b, 1),
            tab_with_index(right, c, 2),
        ],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(c, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let mut client = focused_client(session.id, middle, b); // viewing the middle tab
    let client_id = client.id();
    client.update_focused_pane(left, a); // also has a focus recorded on the left tab
    session.attach_client(client);

    let _ = remove_pane_cascade(&mut session, middle, b, rect(), EmptyTabPolicy::CloseTab);

    let client = session.clients.get(client_id).unwrap();
    // The previous tab (largest index below the closed one) inherits the client.
    assert_eq!(client.active_tab(), left);
    // Its focus entry for the gone tab is pruned.
    assert_eq!(client.focused_pane(middle), None);
    // Focus it still holds on the surviving left tab is untouched.
    assert_eq!(client.focused_pane(left), Some(a));
}

#[test]
fn closing_the_first_tab_moves_the_client_to_the_next_tab() {
    let (first, second) = (TabId::new(), TabId::new());
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![tab_with_index(first, a, 0), tab_with_index(second, b, 1)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, first, a);
    let client_id = client.id();
    session.attach_client(client);

    let _ = remove_pane_cascade(&mut session, first, a, rect(), EmptyTabPolicy::CloseTab);

    // No previous tab, so the next one inherits the client.
    assert_eq!(session.clients.get(client_id).unwrap().active_tab(), second);
}

#[test]
fn closing_a_tab_a_client_is_not_viewing_leaves_its_active_tab() {
    let (other, viewing) = (TabId::new(), TabId::new());
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![tab_with_index(other, a, 0), tab_with_index(viewing, b, 1)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let mut client = focused_client(session.id, viewing, b); // active on `viewing`
    let client_id = client.id();
    client.update_focused_pane(other, a); // but holds a stale focus on `other`
    session.attach_client(client);

    let _ = remove_pane_cascade(&mut session, other, a, rect(), EmptyTabPolicy::CloseTab);

    let client = session.clients.get(client_id).unwrap();
    // The client was not viewing the closed tab, so its active tab is unchanged.
    assert_eq!(client.active_tab(), viewing);
    // The stale focus entry for the closed tab is still pruned.
    assert_eq!(client.focused_pane(other), None);
}

#[test]
fn closing_the_last_tab_prunes_client_focus_and_quits() {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![record(
            pane,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let client = focused_client(session.id, tab_id, pane);
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(&mut session, tab_id, pane, rect(), EmptyTabPolicy::CloseTab);

    assert!(events.iter().any(|e| matches!(e, Event::Quit)));
    // The focus entry for the closed tab is pruned even as the session quits.
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        None
    );
}

#[test]
fn removing_a_pane_from_a_fullscreen_tab_returns_it_to_tiled() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    session.attach_client(focused_client(session.id, tab_id, a));
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout_mode(LayoutMode::Fullscreen { focused: a });

    // Removing the hidden pane drops the fullscreen along with the leaf;
    // the focus was on the survivor, so no repair events follow.
    let events = remove_pane_cascade(&mut session, tab_id, b, rect(), EmptyTabPolicy::CloseTab);

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: b }),
            Event::PaneRemoved(PaneRemoved { pane_id: b, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
    assert_eq!(session.tabs[&tab_id].layout_mode(), LayoutMode::Tiled);
    assert_eq!(*session.tabs[&tab_id].layout(), LayoutNode::Pane(a));
}
