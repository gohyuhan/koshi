//! Tests for pane lifecycle cascades: removal, focus repair, and tab closure.
//!
//! These tests verify that pane removal (via exit or user action) correctly
//! cascades: focus is repaired on all clients, sibling panes inherit focus,
//! emptying a tab closes it under `CloseTab`, and the session quits when no
//! tabs remain. Also tests the inverse — on child
//! process exit, the exit policy (`CloseOnExit`) decides
//! whether a pane is removed or restarted — and which of the terminal, the
//! client's own regions, or another viewer is named when no pane fits.

use std::time::SystemTime;

use koshi_core::event::{
    Event, LayoutChanged, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved, TabClosed,
    TerminalTooSmallCause,
};
use koshi_core::geometry::{PaneArea, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::{PaneSizing, MIN_PANE_SIZE};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::{PaneClosePolicy, PaneExitPolicy};
use koshi_pane::pane::state::PaneRecord;

use super::{on_child_exit, remove_pane_cascade, terminal_too_small_cause};
use crate::client::{Client, ClientOrigin, ClientRegistry};
use crate::session::policy::EmptyTabPolicy;
use crate::session::state::{Session, Tab};

/// Standard terminal size (80×24) used across all test fixtures.
const VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// Returns a rect covering the full viewport (80×24), used as the layout bounds when solving tab geometry.
fn rect() -> Rect {
    Rect::at_origin(VIEWPORT)
}

/// Creates a pane record with the specified lifecycle state and exit policy.
///
/// The record starts as a fresh `Spawning` state and is walked to the
/// requested lifecycle through legal `update_lifecycle` events, the only way
/// the state changes. Timestamps use `UNIX_EPOCH` to keep tests
/// deterministic. Close policy is set to `Force`.
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
        vec![LayoutNode::Pane(left), LayoutNode::Pane(right)],
    )));
    tab
}

/// Creates a client viewing the given tab with the given pane focused.
/// The client carries `session_id`, which [`Session::validate`] checks against
/// the session's own id.
fn focused_client(session_id: SessionId, tab_id: TabId, pane: PaneId) -> Client {
    let mut client = Client::new(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        VIEWPORT,
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane);
    client
}

/// Creates a session with the given tabs and pane records, but no attached
/// clients. [`Session::attach_client`] adds them afterward, each built with
/// the session's own id.
fn session_with(tabs: Vec<Tab>, records: Vec<PaneRecord>) -> Session {
    let mut session = Session::new(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
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

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // The survivor inherits focus, on the client and in the event stream.
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: b,
                prior_pane: Some(a),
            }),
        ]
    );
    // The removed pane is gone from the registry and the layout collapsed to B.
    assert_eq!(session.panes.get(a).map(PaneRecord::id), None);
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![b]);
}

#[test]
fn removing_a_pane_missing_from_the_layout_still_repairs_focus_and_zoom() {
    // Registry/layout desync: the registry holds A and B, the layout names
    // only B. Removing A must still move focus and zoom off it, or the
    // client keeps pointing at a pane with no registry record.
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(tab_id, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let mut client = focused_client(session.id, tab_id, a);
    client.zoom_pane(tab_id, a);
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    let client = session.clients.get(client_id).unwrap();
    assert_eq!(client.focused_pane(tab_id), Some(b));
    assert_eq!(client.zoomed_pane(tab_id), None);
    assert_eq!(session.panes.get(a).map(PaneRecord::id), None);
    // The layout never held A, so no layout change is announced.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: b,
                prior_pane: Some(a),
            }),
        ]
    );
    assert_eq!(session.validate(), Ok(()));
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

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    // No client was looking at A, so nothing beyond the removal is reported.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
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

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // The survivor's geometry changed when the leaf collapsed, so the cascade
    // announces it — a subscriber re-solves on LayoutChanged.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved { pane_id: a, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
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

    let _ = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        session.clients.get(first_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
    assert_eq!(
        session.clients.get(second_id).unwrap().focused_pane(tab_id),
        Some(b)
    );
}

/// Focus repair reads each client's remembered focus in the removed pane's
/// tab, not the tab it is currently viewing, so a client parked on another tab
/// gets the same inherited pane and the same `PaneFocused` event.
#[test]
fn focus_repair_reaches_a_client_viewing_another_tab() {
    let (removed_from, viewing) = (TabId::new(), TabId::new());
    let (a, b, elsewhere) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            two_pane_tab(removed_from, a, b),
            single_pane_tab(viewing, elsewhere),
        ],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(
                elsewhere,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.id, viewing, elsewhere);
    let client_id = client.id();
    client.update_focused_pane(removed_from, a);
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        removed_from,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: a }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: a,
                tab_id: removed_from,
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: removed_from,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: removed_from,
                pane_id: b,
                prior_pane: Some(a),
            }),
        ]
    );
    let client = session.clients.get(client_id).expect("client");
    // The client stays on the tab it was viewing; only its remembered focus in
    // the edited tab moved.
    assert_eq!(client.active_tab(), viewing);
    assert_eq!(client.focused_pane(removed_from), Some(b));
    assert_eq!(client.focused_pane(viewing), Some(elsewhere));
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
    let tiny = Rect::at_origin(Size { cols: 1, rows: 1 });
    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        tiny,
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // The overlay is reported with the viewport and the two-row fallback area,
    // and the client's stale focus on the gone pane is cleared.
    let entered = events
        .iter()
        .find_map(|event| match event {
            Event::TerminalTooSmallEntered(entered) => Some(entered),
            _ => None,
        })
        .expect("the too-small event was emitted");
    assert_eq!(entered.client_id, client_id);
    assert_eq!(entered.size, VIEWPORT);
    assert_eq!(entered.pane_area, None);
    assert_eq!(entered.cause, TerminalTooSmallCause::Terminal);
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        None
    );
    // The survivor stays — the tab is not empty, only unfocusable at this size.
    assert_eq!(session.panes.get(b).map(PaneRecord::id), Some(b));
    assert_eq!(session.tabs[&tab_id].layout().leaf_panes(), vec![b]);
}

#[test]
fn a_too_small_event_carries_a_starving_area_and_region_cause() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let mut client = focused_client(session.id, tab_id, a);
    client.update_pane_area(Some(PaneArea::Starving));
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        Rect::at_origin(Size { cols: 1, rows: 1 }),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );
    let entered = events
        .iter()
        .find_map(|event| match event {
            Event::TerminalTooSmallEntered(entered) => Some(entered),
            _ => None,
        })
        .expect("the too-small event was emitted");

    assert_eq!(entered.client_id, client_id);
    assert_eq!(entered.size, VIEWPORT);
    assert_eq!(entered.pane_area, Some(PaneArea::Starving));
    assert_eq!(entered.cause, TerminalTooSmallCause::Regions);
}

#[test]
fn a_starving_report_names_the_clients_own_regions() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let mut client = focused_client(session.id, tab_id, pane_id);
    client.update_pane_area(Some(PaneArea::Starving));
    let client_id = client.id();
    session.attach_client(client);

    assert_eq!(
        terminal_too_small_cause(&session, tab_id, client_id, rect()),
        TerminalTooSmallCause::Regions
    );
}

#[test]
fn a_reported_area_smaller_than_the_default_names_the_clients_regions() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let mut client = focused_client(session.id, tab_id, pane_id);
    client.update_pane_area(Some(PaneArea::Reported(Size { cols: 40, rows: 22 })));
    let client_id = client.id();
    session.attach_client(client);

    assert_eq!(
        terminal_too_small_cause(&session, tab_id, client_id, rect()),
        TerminalTooSmallCause::Regions
    );
}

#[test]
fn a_smaller_viewer_names_the_other_client() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let target = focused_client(session.id, tab_id, pane_id);
    let target_id = target.id();
    let mut smaller = focused_client(session.id, tab_id, pane_id);
    smaller.update_pane_area(Some(PaneArea::Reported(Size { cols: 40, rows: 24 })));
    let smaller_id = smaller.id();
    session.attach_client(target);
    session.attach_client(smaller);

    assert_eq!(
        terminal_too_small_cause(
            &session,
            tab_id,
            target_id,
            Rect::at_origin(Size { cols: 40, rows: 22 }),
        ),
        TerminalTooSmallCause::OtherClient(smaller_id)
    );
}

#[test]
fn a_shorter_solve_rect_is_a_terminal_shortage() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let target = focused_client(session.id, tab_id, pane_id);
    let target_id = target.id();
    let mut smaller = focused_client(session.id, tab_id, pane_id);
    smaller.update_pane_area(Some(PaneArea::Reported(Size { cols: 40, rows: 24 })));
    session.attach_client(target);
    session.attach_client(smaller);

    assert_eq!(
        terminal_too_small_cause(
            &session,
            tab_id,
            target_id,
            Rect::at_origin(Size { cols: 1, rows: 1 }),
        ),
        TerminalTooSmallCause::Terminal
    );
}

#[test]
fn a_client_id_that_is_not_attached_is_a_terminal_shortage() {
    // Nothing is known about a client that already detached, so the shortage
    // falls to the terminal rather than naming a region or another viewer.
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    assert_eq!(
        terminal_too_small_cause(&session, tab_id, ClientId::new(), rect()),
        TerminalTooSmallCause::Terminal
    );
}

#[test]
fn a_tab_no_client_is_viewing_is_a_terminal_shortage() {
    // The client is attached but active on another tab, so the queried tab has
    // no viewer contributing a size and no other viewer can be blamed.
    let (viewed, other) = (TabId::new(), TabId::new());
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![tab_with_index(viewed, a, 0), tab_with_index(other, b, 1)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, viewed, a);
    let client_id = client.id();
    session.attach_client(client);

    assert_eq!(session.tab_viewport(other), None);
    assert_eq!(
        terminal_too_small_cause(&session, other, client_id, rect()),
        TerminalTooSmallCause::Terminal
    );
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

    let _ = remove_pane_cascade(
        &mut session,
        tab_id,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // Only the survivor is left, and it kept its place in the history.
    assert_eq!(session.tabs[&tab_id].focus_mru(), [b]);
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

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        only,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        Vec::new()
    );
    // The tab is gone, so this is a tab-close, not a within-tab layout change:
    // no LayoutChanged is emitted for a tab that no longer exists.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: only }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: only,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit,
        ]
    );
}

/// Emptying the only tab while a client focuses its last pane leaves nothing
/// dangling: the pane record, the tab and the client's focus entry all go, and
/// the session passes its own consistency check.
#[test]
fn removing_the_last_pane_a_client_focuses_leaves_a_consistent_session() {
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
    let client = focused_client(session.id, tab_id, only);
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        only,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: only }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: only,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit,
        ]
    );
    assert_eq!(session.panes.get(only).map(PaneRecord::id), None);
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        Vec::new()
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("the client stays attached")
            .focused_pane(tab_id),
        None
    );
    assert_eq!(session.validate(), Ok(()));
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
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        vec![tab_two]
    );
    // The emptied tab closes, but a tab survives, so no `Quit` follows.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pane_one }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pane_one,
                tab_id: tab_one,
            }),
            Event::TabClosed(TabClosed { tab_id: tab_one }),
        ]
    );
}

#[test]
fn on_child_exit_for_an_unknown_pane_only_emits_the_exit_fact() {
    // The exit fact is reported unconditionally, but there is no pane record
    // to read a policy off of, so nothing else in the session may change.
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
    let unknown = PaneId::new();

    let events = on_child_exit(
        &mut session,
        tab_id,
        unknown,
        Some(1),
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events,
        vec![Event::PaneProcessExited(PaneProcessExited {
            pane_id: unknown,
            exit_code: Some(1),
        })]
    );
    // The real pane in the tab is completely untouched.
    assert_eq!(session.panes.get(only).map(PaneRecord::id), Some(only));
    assert_eq!(session.tabs[&tab_id].layout(), &LayoutNode::Pane(only));
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
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(events, Vec::new());
    assert_eq!(session.panes.get(only).map(PaneRecord::id), Some(only));
    assert_eq!(session.tabs[&tab_id].layout(), &LayoutNode::Pane(only));
}

/// A tab id the session does not hold, with a pane id it does: the pane's
/// registry record is dropped and the cascade stops there, so the tab that
/// really holds the pane keeps a leaf with no record behind it.
#[test]
fn removing_a_pane_under_an_unknown_tab_changes_nothing_and_emits_nothing() {
    let tab_id = TabId::new();
    let (kept, target) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, kept, target)],
        vec![
            record(kept, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(target, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );

    let events = remove_pane_cascade(
        &mut session,
        TabId::new(),
        target,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(events, Vec::new());
    assert_eq!(session.panes.get(target).map(PaneRecord::id), Some(target));
    assert_eq!(
        session.tabs[&tab_id].layout().leaf_panes(),
        vec![kept, target]
    );
    assert_eq!(session.validate(), Ok(()));
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
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(session.panes.get(pane).map(PaneRecord::id), None);
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        Vec::new()
    );
    // The exit fact leads, then the shared removal cascade in full.
    assert_eq!(
        events,
        vec![
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: pane,
                exit_code: Some(0),
            }),
            Event::PaneClosing(PaneClosing { pane_id: pane }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pane,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit,
        ]
    );
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

    let _ = remove_pane_cascade(
        &mut session,
        middle,
        b,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

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

    let _ = remove_pane_cascade(
        &mut session,
        first,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

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

    let _ = remove_pane_cascade(
        &mut session,
        other,
        a,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

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

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        pane,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // No surviving tab to move the client to, so no `TabFocused` is emitted.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: pane }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pane,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit,
        ]
    );
    // The focus entry for the closed tab is pruned even as the session quits.
    assert_eq!(
        session.clients.get(client_id).unwrap().focused_pane(tab_id),
        None
    );
}

/// Removing a pane a client cannot even see — it is zoomed on another one —
/// leaves that client's zoom alone. The pane it is looking at did not go
/// anywhere, so its view has no reason to change.
#[test]
fn removing_a_hidden_pane_leaves_a_zoomed_client_zoomed() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let mut client = focused_client(session.id, tab_id, a);
    let client_id = client.id();
    client.zoom_pane(tab_id, a);
    session.attach_client(client);

    // The focus was on the survivor, so no repair events follow.
    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        b,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: b }),
            Event::PaneRemoved(PaneRemoved { pane_id: b, tab_id }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
    assert_eq!(*session.tabs[&tab_id].layout(), LayoutNode::Pane(a));
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .layout_mode(tab_id),
        LayoutMode::Fullscreen { focused: a },
        "the pane this client is zoomed on still exists, so its zoom stands"
    );
}

/// Removing the very pane a client is zoomed on leaves that zoom with nothing
/// to show, so the client drops back to its tiled view — it does not silently
/// zoom whichever pane inherits the focus.
#[test]
fn removing_the_zoomed_pane_drops_that_clients_zoom() {
    let tab_id = TabId::new();
    let (a, b) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, a, b)],
        vec![
            record(a, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(b, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let mut client = focused_client(session.id, tab_id, b);
    let client_id = client.id();
    client.zoom_pane(tab_id, b);
    session.attach_client(client);

    let _ = remove_pane_cascade(
        &mut session,
        tab_id,
        b,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    let client = session.clients.get(client_id).expect("client");
    assert_eq!(
        client.layout_mode(tab_id),
        LayoutMode::Tiled,
        "the zoomed pane is gone, so the zoom is gone"
    );
    assert_eq!(
        client.focused_pane(tab_id),
        Some(a),
        "focus repair moves to the survivor"
    );
}

/// A pane with a registry record but no leaf in the tab's tree: the record is
/// dropped and the cascade stops there. Nothing else in the tab may move — the
/// tab's own pane, its tree, its client's focus and the tab itself all stand,
/// and the empty-tab policy never fires.
#[test]
fn a_registry_pane_missing_from_the_layout_is_dropped_without_touching_the_tab() {
    let tab_id = TabId::new();
    let (kept, ghost) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(tab_id, kept)],
        vec![
            record(kept, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            record(ghost, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.id, tab_id, kept);
    let client_id = client.id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        ghost,
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    // Exactly the two removal facts: no `LayoutChanged`, no `TabClosed`, no
    // `Quit`.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing { pane_id: ghost }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: ghost,
                tab_id,
            }),
        ]
    );
    assert_eq!(session.panes.get(ghost).map(PaneRecord::id), None);
    assert_eq!(session.panes.get(kept).map(PaneRecord::id), Some(kept));
    assert_eq!(session.tabs[&tab_id].layout(), &LayoutNode::Pane(kept));
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        vec![tab_id]
    );
    assert_eq!(
        session
            .clients
            .get(client_id)
            .expect("client")
            .focused_pane(tab_id),
        Some(kept)
    );
}

/// A second exit report for a pane already recorded as `Exited`. The removal
/// cascade reads the policy, not the lifecycle, so the pane is removed and its
/// last tab closes exactly as on the first report.
#[test]
fn a_repeated_exit_still_removes_the_pane() {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![record(
            pane,
            PaneLifecycle::Exited {
                code: Some(1),
                at: SystemTime::UNIX_EPOCH,
            },
            PaneExitPolicy::CloseOnExit,
        )],
    );

    let events = on_child_exit(
        &mut session,
        tab_id,
        pane,
        Some(2),
        rect(),
        PaneSizing {
            min: MIN_PANE_SIZE,
            gap: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events.first(),
        Some(&Event::PaneProcessExited(PaneProcessExited {
            pane_id: pane,
            exit_code: Some(2),
        }))
    );
    assert_eq!(session.panes.get(pane), None);
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        Vec::new()
    );
}
