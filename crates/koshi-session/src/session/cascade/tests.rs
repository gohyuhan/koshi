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
    Event, LayoutChanged, PaneClosing, PaneFocused, PaneProcessExited, PaneRemoved, QuitCause,
    TabClosed, TerminalTooSmallCause,
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
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// Returns a rect covering the full viewport (80×24), used as the layout bounds when solving tab geometry.
fn rect() -> Rect {
    Rect::from_size_at_origin(TEST_VIEWPORT_SIZE)
}

/// Creates a pane pane record with the specified lifecycle state and exit policy.
///
/// The pane record starts as a fresh `Spawning` state and is walked to the
/// requested lifecycle through legal `update_lifecycle` events, the only way
/// the state changes. Timestamps use `UNIX_EPOCH` to keep tests
/// deterministic. Close policy is set to `Force`.
fn build_pane_record(
    pane_id: PaneId,
    lifecycle: PaneLifecycle,
    exit_policy: PaneExitPolicy,
) -> PaneRecord {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, SystemTime::UNIX_EPOCH);
    pane_record.close_policy = PaneClosePolicy::Force;
    pane_record.exit_policy = exit_policy;
    walk_lifecycle(&mut pane_record, lifecycle);
    pane_record
}

/// Transitions a pane pane record from its current state to the target lifecycle state
/// by emitting the legal sequence of intermediate events.
fn walk_lifecycle(pane_record: &mut PaneRecord, target_lifecycle: PaneLifecycle) {
    match target_lifecycle {
        PaneLifecycle::Spawning => {}
        PaneLifecycle::Running => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Exited {
            exit_code,
            exited_at,
        } => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                    exit_code,
                    exited_at,
                })
                .expect("walk_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Closing { close_requested_at } => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::CloseRequested { close_requested_at })
                .expect("walk_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Removed => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::CloseRequested {
                    close_requested_at: SystemTime::UNIX_EPOCH,
                })
                .expect("walk_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::Cleaned)
                .expect("walk_lifecycle drives only legal transitions");
        }
    }
}

/// Creates a tab containing a single pane.
fn single_pane_tab(tab_id: TabId, pane: PaneId) -> Tab {
    Tab::from_root_pane(tab_id, "code".to_owned(), 0, pane)
}

/// Creates a single-pane tab at the given display position (`tab_index`).
fn tab_at_index(tab_id: TabId, pane_id: PaneId, tab_index: usize) -> Tab {
    let mut tab = single_pane_tab(tab_id, pane_id);
    tab.update_tab_index(tab_index);
    tab
}

/// Creates a tab split horizontally (left/right) between two panes with equal widths.
fn two_pane_tab(tab_id: TabId, left: PaneId, right: PaneId) -> Tab {
    let mut tab = Tab::from_root_pane(tab_id, "code".to_owned(), 0, left);
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
    let mut client = Client::from_attachment(
        ClientId::new(),
        session_id,
        SystemTime::UNIX_EPOCH,
        TEST_VIEWPORT_SIZE,
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
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "main".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    for tab in tabs {
        session.tabs.insert(tab.get_tab_id(), tab);
    }
    for pane in records {
        session
            .panes
            .register_pane_record(pane)
            .expect("unique pane id");
    }
    session
}

#[test]
fn fixtures_build_a_consistent_session() {
    let tab_id = TabId::new();
    let (left_pane_id, right_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, left_pane_id, right_pane_id)],
        vec![
            build_pane_record(
                left_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                right_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    session.attach_client(focused_client(session.session_id, tab_id, left_pane_id));

    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn removing_a_focused_pane_focuses_a_survivor() {
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, removed_pane_id, surviving_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let client = focused_client(session.session_id, tab_id, removed_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    // The survivor inherits focus, on the client and in the event stream.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(surviving_pane_id)
    );
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: surviving_pane_id,
                previous_pane_id: Some(removed_pane_id),
            }),
        ]
    );
    // The removed pane is gone from the registry and the layout collapsed to B.
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(removed_pane_id)
            .map(PaneRecord::get_pane_id),
        None
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![surviving_pane_id]
    );
}

#[test]
fn removing_a_pane_missing_from_the_layout_still_repairs_focus_and_zoom() {
    // Registry/layout desync: the registry holds A and B, the layout names
    // only B. Removing A must still move focus and zoom off it, or the
    // client keeps pointing at a pane with no registry pane record.
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![single_pane_tab(tab_id, surviving_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, tab_id, removed_pane_id);
    client.zoom_pane(tab_id, removed_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    let client = session.clients.get_client_by_id(client_id).unwrap();
    assert_eq!(client.get_focused_pane(tab_id), Some(surviving_pane_id));
    assert_eq!(client.get_zoomed_pane(tab_id), None);
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(removed_pane_id)
            .map(PaneRecord::get_pane_id),
        None
    );
    // The layout never held A, so no layout change is announced.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id,
                pane_id: surviving_pane_id,
                previous_pane_id: Some(removed_pane_id),
            }),
        ]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn removing_a_nonfocused_pane_leaves_focus_untouched() {
    let tab_id = TabId::new();
    let (removed_pane_id, focused_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, removed_pane_id, focused_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                focused_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let client = focused_client(session.session_id, tab_id, focused_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(focused_pane_id)
    );
    // No client was looking at the removed pane, so nothing beyond the removal is reported.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
}

#[test]
fn collapsing_a_multi_pane_tab_emits_layout_changed() {
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, removed_pane_id, surviving_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    // The survivor's geometry changed when the leaf collapsed, so the cascade
    // announces it — a subscriber re-solves on LayoutChanged.
    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
}

#[test]
fn focus_repair_runs_for_every_client_on_the_removed_pane() {
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, removed_pane_id, surviving_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let first_client = focused_client(session.session_id, tab_id, removed_pane_id);
    let second_client = focused_client(session.session_id, tab_id, removed_pane_id);
    let (first_client_id, second_client_id) =
        (first_client.get_client_id(), second_client.get_client_id());
    session.attach_client(first_client);
    session.attach_client(second_client);

    let _ = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    assert_eq!(
        session
            .clients
            .get_client_by_id(first_client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(surviving_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(second_client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        Some(surviving_pane_id)
    );
}

/// Focus repair reads each client's remembered focus in the removed pane's
/// tab, not the tab it is currently viewing, so a client parked on another tab
/// gets the same inherited pane and the same `PaneFocused` event.
#[test]
fn focus_repair_reaches_a_client_viewing_another_tab() {
    let (removed_tab_id, viewing_tab_id) = (TabId::new(), TabId::new());
    let (removed_pane_id, surviving_pane_id, viewing_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            two_pane_tab(removed_tab_id, removed_pane_id, surviving_pane_id),
            single_pane_tab(viewing_tab_id, viewing_pane_id),
        ],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                viewing_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, viewing_tab_id, viewing_pane_id);
    let client_id = client.get_client_id();
    client.update_focused_pane(removed_tab_id, removed_pane_id);
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        removed_tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id: removed_tab_id,
            }),
            Event::LayoutChanged(LayoutChanged {
                tab_id: removed_tab_id,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: removed_tab_id,
                pane_id: surviving_pane_id,
                previous_pane_id: Some(removed_pane_id),
            }),
        ]
    );
    let client = session.clients.get_client_by_id(client_id).expect("client");
    // The client stays on the tab it was viewing; only its remembered focus in
    // the edited tab moved.
    assert_eq!(client.get_active_tab(), viewing_tab_id);
    assert_eq!(
        client.get_focused_pane(removed_tab_id),
        Some(surviving_pane_id)
    );
    assert_eq!(
        client.get_focused_pane(viewing_tab_id),
        Some(viewing_pane_id)
    );
}

#[test]
fn removing_a_focused_pane_with_no_room_to_refocus_clears_focus() {
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, removed_pane_id, surviving_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let client = focused_client(session.session_id, tab_id, removed_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    // A rect narrower than `MIN_PANE_SIZE` suppresses the survivor, so focus
    // recovery finds no focusable pane though the tab still holds one.
    let tiny = Rect::from_size_at_origin(Size {
        column_count: 1,
        row_count: 1,
    });
    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        tiny,
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
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
    assert_eq!(entered.viewport_size, TEST_VIEWPORT_SIZE);
    assert_eq!(entered.pane_area, None);
    assert_eq!(entered.cause, TerminalTooSmallCause::Terminal);
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        None
    );
    // The survivor stays — the tab is not empty, only unfocusable at this size.
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(surviving_pane_id)
            .map(PaneRecord::get_pane_id),
        Some(surviving_pane_id)
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![surviving_pane_id]
    );
}

#[test]
fn a_too_small_event_carries_a_starving_area_and_region_cause() {
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, removed_pane_id, surviving_pane_id)],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, tab_id, removed_pane_id);
    client.update_pane_area(Some(PaneArea::Starving));
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        Rect::from_size_at_origin(Size {
            column_count: 1,
            row_count: 1,
        }),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );
    let entered = events
        .iter()
        .find_map(|event| match event {
            Event::TerminalTooSmallEntered(entered) => Some(entered),
            _ => None,
        })
        .expect("the too-small event was emitted");

    assert_eq!(entered.client_id, client_id);
    assert_eq!(entered.viewport_size, TEST_VIEWPORT_SIZE);
    assert_eq!(entered.pane_area, Some(PaneArea::Starving));
    assert_eq!(entered.cause, TerminalTooSmallCause::Regions);
}

#[test]
fn a_starving_report_names_the_clients_own_regions() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![build_pane_record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let mut client = focused_client(session.session_id, tab_id, pane_id);
    client.update_pane_area(Some(PaneArea::Starving));
    let client_id = client.get_client_id();
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
        vec![build_pane_record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let mut client = focused_client(session.session_id, tab_id, pane_id);
    client.update_pane_area(Some(PaneArea::Reported(Size {
        column_count: 40,
        row_count: 22,
    })));
    let client_id = client.get_client_id();
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
        vec![build_pane_record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let target_client = focused_client(session.session_id, tab_id, pane_id);
    let target_client_id = target_client.get_client_id();
    let mut smaller_client = focused_client(session.session_id, tab_id, pane_id);
    smaller_client.update_pane_area(Some(PaneArea::Reported(Size {
        column_count: 40,
        row_count: 24,
    })));
    let smaller_client_id = smaller_client.get_client_id();
    session.attach_client(target_client);
    session.attach_client(smaller_client);

    assert_eq!(
        terminal_too_small_cause(
            &session,
            tab_id,
            target_client_id,
            Rect::from_size_at_origin(Size {
                column_count: 40,
                row_count: 22
            }),
        ),
        TerminalTooSmallCause::OtherClient(smaller_client_id)
    );
}

#[test]
fn a_shorter_solve_rect_is_a_terminal_shortage() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane_id)],
        vec![build_pane_record(
            pane_id,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let target_client = focused_client(session.session_id, tab_id, pane_id);
    let target_client_id = target_client.get_client_id();
    let mut smaller_client = focused_client(session.session_id, tab_id, pane_id);
    smaller_client.update_pane_area(Some(PaneArea::Reported(Size {
        column_count: 40,
        row_count: 24,
    })));
    session.attach_client(target_client);
    session.attach_client(smaller_client);

    assert_eq!(
        terminal_too_small_cause(
            &session,
            tab_id,
            target_client_id,
            Rect::from_size_at_origin(Size {
                column_count: 1,
                row_count: 1
            }),
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
        vec![build_pane_record(
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
    let (viewed_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let (viewed_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            tab_at_index(viewed_tab_id, viewed_pane_id, 0),
            tab_at_index(other_tab_id, other_pane_id, 1),
        ],
        vec![
            build_pane_record(
                viewed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                other_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let client = focused_client(session.session_id, viewed_tab_id, viewed_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    assert_eq!(session.get_tab_viewport(other_tab_id), None);
    assert_eq!(
        terminal_too_small_cause(&session, other_tab_id, client_id, rect()),
        TerminalTooSmallCause::Terminal
    );
}

#[test]
fn the_removed_pane_leaves_the_tab_focus_history() {
    let tab_id = TabId::new();
    let (removed_pane_id, surviving_pane_id) = (PaneId::new(), PaneId::new());
    let mut tab = two_pane_tab(tab_id, removed_pane_id, surviving_pane_id);
    tab.record_focus_mru(surviving_pane_id);
    tab.record_focus_mru(removed_pane_id); // history: [removed, surviving]
    let mut session = session_with(
        vec![tab],
        vec![
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );

    let _ = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    // Only the survivor is left, and it kept its place in the history.
    assert_eq!(session.tabs[&tab_id].list_focus_mru(), [surviving_pane_id]);
}

#[test]
fn removing_the_last_pane_closes_the_tab_and_quits() {
    let tab_id = TabId::new();
    let only = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![build_pane_record(
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
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
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
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
}

/// Emptying the only tab while a client focuses its last pane leaves nothing
/// dangling: the pane pane record, the tab and the client's focus entry all go, and
/// the session passes its own consistency check.
#[test]
fn removing_the_last_pane_a_client_focuses_leaves_a_consistent_session() {
    let tab_id = TabId::new();
    let only = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![build_pane_record(
            only,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let client = focused_client(session.session_id, tab_id, only);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        only,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
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
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(only)
            .map(PaneRecord::get_pane_id),
        None
    );
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        Vec::new()
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client stays attached")
            .get_focused_pane(tab_id),
        None
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
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
            build_pane_record(
                pane_one,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
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
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
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
    // The exit fact is reported unconditionally, but there is no pane pane record
    // to read a policy off of, so nothing else in the session may change.
    let tab_id = TabId::new();
    let only = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![build_pane_record(
            only,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let unknown = PaneId::new();

    let events = on_child_exit(
        &mut session,
        tab_id,
        PaneProcessExited {
            pane_id: unknown,
            exit_code: Some(1),
            signal: None,
        },
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events,
        vec![Event::PaneProcessExited(PaneProcessExited {
            pane_id: unknown,
            exit_code: Some(1),
            signal: None,
        })]
    );
    // The real pane in the tab is completely untouched.
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(only)
            .map(PaneRecord::get_pane_id),
        Some(only)
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(only)
    );
}

#[test]
fn removing_an_unknown_pane_emits_nothing() {
    let tab_id = TabId::new();
    let only = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, only)],
        vec![build_pane_record(
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
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    assert_eq!(events, Vec::new());
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(only)
            .map(PaneRecord::get_pane_id),
        Some(only)
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(only)
    );
}

/// A tab id the session does not hold, with a pane id it does: the pane's
/// registry pane record is dropped and the cascade stops there, so the tab that
/// really holds the pane keeps a leaf with no pane record behind it.
#[test]
fn removing_a_pane_under_an_unknown_tab_changes_nothing_and_emits_nothing() {
    let tab_id = TabId::new();
    let (kept, target) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, kept, target)],
        vec![
            build_pane_record(kept, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            build_pane_record(target, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );

    let events = remove_pane_cascade(
        &mut session,
        TabId::new(),
        target,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    assert_eq!(events, Vec::new());
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(target)
            .map(PaneRecord::get_pane_id),
        Some(target)
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree().list_leaf_pane_ids(),
        vec![kept, target]
    );
    assert_eq!(session.validate_session_consistency(), Ok(()));
}

#[test]
fn a_close_on_exit_pane_runs_the_removal_cascade() {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![build_pane_record(
            pane,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );

    let events = on_child_exit(
        &mut session,
        tab_id,
        PaneProcessExited {
            pane_id: pane,
            exit_code: Some(0),
            signal: None,
        },
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(pane)
            .map(PaneRecord::get_pane_id),
        None
    );
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
                signal: None,
            }),
            Event::PaneClosing(PaneClosing { pane_id: pane }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: pane,
                tab_id,
            }),
            Event::TabClosed(TabClosed { tab_id }),
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: Some(PaneProcessExited {
                    pane_id: pane,
                    exit_code: Some(0),
                    signal: None,
                }),
            }),
        ]
    );
}

#[test]
fn closing_a_clients_active_tab_moves_it_to_the_previous_tab() {
    let (left_tab_id, middle_tab_id, right_tab_id) = (TabId::new(), TabId::new(), TabId::new());
    let (left_pane_id, middle_pane_id, right_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            tab_at_index(left_tab_id, left_pane_id, 0),
            tab_at_index(middle_tab_id, middle_pane_id, 1),
            tab_at_index(right_tab_id, right_pane_id, 2),
        ],
        vec![
            build_pane_record(
                left_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                middle_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                right_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, middle_tab_id, middle_pane_id);
    let client_id = client.get_client_id();
    client.update_focused_pane(left_tab_id, left_pane_id);
    session.attach_client(client);

    let _ = remove_pane_cascade(
        &mut session,
        middle_tab_id,
        middle_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    let client = session.clients.get_client_by_id(client_id).unwrap();
    // The previous tab (largest index below the closed one) inherits the client.
    assert_eq!(client.get_active_tab(), left_tab_id);
    // Its focus entry for the gone tab is pruned.
    assert_eq!(client.get_focused_pane(middle_tab_id), None);
    // Focus it still holds on the surviving left tab is untouched.
    assert_eq!(client.get_focused_pane(left_tab_id), Some(left_pane_id));
}

#[test]
fn closing_the_first_tab_moves_the_client_to_the_next_tab() {
    let (first_tab_id, second_tab_id) = (TabId::new(), TabId::new());
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            tab_at_index(first_tab_id, first_pane_id, 0),
            tab_at_index(second_tab_id, second_pane_id, 1),
        ],
        vec![
            build_pane_record(
                first_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                second_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let client = focused_client(session.session_id, first_tab_id, first_pane_id);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let _ = remove_pane_cascade(
        &mut session,
        first_tab_id,
        first_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    // No previous tab, so the next one inherits the client.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_active_tab(),
        second_tab_id
    );
}

#[test]
fn closing_a_tab_a_client_is_not_viewing_leaves_its_active_tab() {
    let (other_tab_id, viewing_tab_id) = (TabId::new(), TabId::new());
    let (other_pane_id, viewing_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![
            tab_at_index(other_tab_id, other_pane_id, 0),
            tab_at_index(viewing_tab_id, viewing_pane_id, 1),
        ],
        vec![
            build_pane_record(
                other_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                viewing_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, viewing_tab_id, viewing_pane_id);
    let client_id = client.get_client_id();
    client.update_focused_pane(other_tab_id, other_pane_id);
    session.attach_client(client);

    let _ = remove_pane_cascade(
        &mut session,
        other_tab_id,
        other_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    let client = session.clients.get_client_by_id(client_id).unwrap();
    // The client was not viewing the closed tab, so its active tab is unchanged.
    assert_eq!(client.get_active_tab(), viewing_tab_id);
    // The stale focus entry for the closed tab is still pruned.
    assert_eq!(client.get_focused_pane(other_tab_id), None);
}

#[test]
fn closing_the_last_tab_prunes_client_focus_and_quits() {
    let tab_id = TabId::new();
    let pane = PaneId::new();
    let mut session = session_with(
        vec![single_pane_tab(tab_id, pane)],
        vec![build_pane_record(
            pane,
            PaneLifecycle::Running,
            PaneExitPolicy::CloseOnExit,
        )],
    );
    let client = focused_client(session.session_id, tab_id, pane);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        pane,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
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
            Event::Quit(QuitCause::LastTabClosed {
                tab_id,
                pane_exit: None,
            }),
        ]
    );
    // The focus entry for the closed tab is pruned even as the session quits.
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_focused_pane(tab_id),
        None
    );
}

/// Removing a pane a client cannot even see — it is zoomed on another one —
/// leaves that client's zoom alone. The pane it is looking at did not go
/// anywhere, so its view has no reason to change.
#[test]
fn removing_a_hidden_pane_leaves_a_zoomed_client_zoomed() {
    let tab_id = TabId::new();
    let (focused_pane_id, removed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, focused_pane_id, removed_pane_id)],
        vec![
            build_pane_record(
                focused_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                removed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, tab_id, focused_pane_id);
    let client_id = client.get_client_id();
    client.zoom_pane(tab_id, focused_pane_id);
    session.attach_client(client);

    // The focus was on the survivor, so no repair events follow.
    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        removed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    assert_eq!(
        events,
        vec![
            Event::PaneClosing(PaneClosing {
                pane_id: removed_pane_id,
            }),
            Event::PaneRemoved(PaneRemoved {
                pane_id: removed_pane_id,
                tab_id,
            }),
            Event::LayoutChanged(LayoutChanged { tab_id }),
        ]
    );
    assert_eq!(
        *session.tabs[&tab_id].get_layout_tree(),
        LayoutNode::Pane(focused_pane_id)
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_layout_mode(tab_id),
        LayoutMode::Fullscreen { focused_pane_id },
        "the pane this client is zoomed on still exists, so its zoom stands"
    );
}

/// Removing the very pane a client is zoomed on leaves that zoom with nothing
/// to show, so the client drops back to its tiled view — it does not silently
/// zoom whichever pane inherits the focus.
#[test]
fn removing_the_zoomed_pane_drops_that_clients_zoom() {
    let tab_id = TabId::new();
    let (surviving_pane_id, zoomed_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = session_with(
        vec![two_pane_tab(tab_id, surviving_pane_id, zoomed_pane_id)],
        vec![
            build_pane_record(
                surviving_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
            build_pane_record(
                zoomed_pane_id,
                PaneLifecycle::Running,
                PaneExitPolicy::CloseOnExit,
            ),
        ],
    );
    let mut client = focused_client(session.session_id, tab_id, zoomed_pane_id);
    let client_id = client.get_client_id();
    client.zoom_pane(tab_id, zoomed_pane_id);
    session.attach_client(client);

    let _ = remove_pane_cascade(
        &mut session,
        tab_id,
        zoomed_pane_id,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
    );

    let client = session.clients.get_client_by_id(client_id).expect("client");
    assert_eq!(
        client.get_layout_mode(tab_id),
        LayoutMode::Tiled,
        "the zoomed pane is gone, so the zoom is gone"
    );
    assert_eq!(
        client.get_focused_pane(tab_id),
        Some(surviving_pane_id),
        "focus repair moves to the survivor"
    );
}

/// A pane with a registry pane record but no leaf in the tab's tree: the pane record is
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
            build_pane_record(kept, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
            build_pane_record(ghost, PaneLifecycle::Running, PaneExitPolicy::CloseOnExit),
        ],
    );
    let client = focused_client(session.session_id, tab_id, kept);
    let client_id = client.get_client_id();
    session.attach_client(client);

    let events = remove_pane_cascade(
        &mut session,
        tab_id,
        ghost,
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
        None,
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
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(ghost)
            .map(PaneRecord::get_pane_id),
        None
    );
    assert_eq!(
        session
            .panes
            .get_pane_record_by_id(kept)
            .map(PaneRecord::get_pane_id),
        Some(kept)
    );
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Pane(kept)
    );
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        vec![tab_id]
    );
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client")
            .get_focused_pane(tab_id),
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
        vec![build_pane_record(
            pane,
            PaneLifecycle::Exited {
                exit_code: Some(1),
                exited_at: SystemTime::UNIX_EPOCH,
            },
            PaneExitPolicy::CloseOnExit,
        )],
    );

    let events = on_child_exit(
        &mut session,
        tab_id,
        PaneProcessExited {
            pane_id: pane,
            exit_code: Some(2),
            signal: None,
        },
        rect(),
        PaneSizing {
            minimum_size: MIN_PANE_SIZE,
            gap_cell_count: 0,
        },
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        events.first(),
        Some(&Event::PaneProcessExited(PaneProcessExited {
            pane_id: pane,
            exit_code: Some(2),
            signal: None,
        }))
    );
    assert_eq!(session.panes.get_pane_record_by_id(pane), None);
    assert_eq!(
        session.tabs.keys().copied().collect::<Vec<TabId>>(),
        Vec::new()
    );
}
