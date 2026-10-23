//! Tests for focus repair: choosing which pane gets keyboard focus after a layout change.
//!
//! Verifies that `repair_focus` walks the recovery hierarchy in order: the client's focus history,
//! spatial neighbor, absorbed pane, and finally the first visible pane in layout order.
//! Also validates the eligibility rule — a pane must sit in the visible layout order and
//! hold a registry pane record — and the two no-pane verdicts.
//! Then checks the `PaneFocused` event `build_pane_focused_event` builds for a move of a
//! client's input into, out of, and between floating panes, the events
//! `apply_tiled_pane_focus` and `build_pane_minimized_changed_events` return,
//! and what `update_client_tab_focus` changes and returns.

use super::*;

use std::time::SystemTime;

use koshi_core::geometry::{Size, SplitDirection};
use koshi_core::ids::{SessionId, TabId};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;

use crate::client::{ClientOrigin, FloatingPanePosition};
use crate::session::state::tests::{build_empty_session, build_tab};

/// A registry holding exactly `pane_records`.
fn build_registry_with(pane_records: Vec<PaneRecord>) -> PaneRegistry {
    let mut pane_registry = PaneRegistry::new();
    for pane_record in pane_records {
        pane_registry
            .register_pane_record(pane_record)
            .expect("unique pane id");
    }
    pane_registry
}

/// Construct a [`FocusCandidates`] struct from the given spatial neighbor, absorbed pane,
/// and visible layout order.
fn build_candidates(
    spatial_neighbor_pane_id: Option<PaneId>,
    absorbed_space_pane_id: Option<PaneId>,
    layout_order_pane_ids: Vec<PaneId>,
) -> FocusCandidates {
    FocusCandidates {
        spatial_neighbor_pane_id,
        absorbed_space_pane_id,
        layout_order_pane_ids,
    }
}

#[test]
fn the_most_recent_history_pane_is_focused_first() {
    let (older_pane_id, newer_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history = [newer_pane_id, older_pane_id];
    let registry = build_registry_with(vec![
        PaneRecord::from_terminal_pane(older_pane_id),
        PaneRecord::from_terminal_pane(newer_pane_id),
    ]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, vec![newer_pane_id, older_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(newer_pane_id)
    );
}

#[test]
fn history_outranks_the_spatial_neighbor_and_absorbed_pane() {
    let (history_pane_id, spatial_neighbor_pane_id, absorbed_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let focus_history = [history_pane_id];
    let registry = build_registry_with(vec![
        PaneRecord::from_terminal_pane(history_pane_id),
        PaneRecord::from_terminal_pane(spatial_neighbor_pane_id),
        PaneRecord::from_terminal_pane(absorbed_pane_id),
    ]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(
            Some(spatial_neighbor_pane_id),
            Some(absorbed_pane_id),
            vec![history_pane_id, spatial_neighbor_pane_id, absorbed_pane_id],
        ),
    );

    // All three are eligible; the recovery order picks history first.
    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(history_pane_id)
    );
}

#[test]
fn the_spatial_neighbor_wins_when_history_has_no_eligible_pane() {
    let (spatial_neighbor_pane_id, absorbed_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![
        PaneRecord::from_terminal_pane(spatial_neighbor_pane_id),
        PaneRecord::from_terminal_pane(absorbed_pane_id),
    ]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(
            Some(spatial_neighbor_pane_id),
            Some(absorbed_pane_id),
            vec![spatial_neighbor_pane_id, absorbed_pane_id],
        ),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(spatial_neighbor_pane_id)
    );
}

#[test]
fn the_absorbed_pane_wins_with_no_history_and_no_spatial_neighbor() {
    let absorbed_pane_id = PaneId::new();
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![PaneRecord::from_terminal_pane(absorbed_pane_id)]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, Some(absorbed_pane_id), vec![absorbed_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(absorbed_pane_id)
    );
}

#[test]
fn the_first_visible_pane_is_the_last_resort() {
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![
        PaneRecord::from_terminal_pane(first_pane_id),
        PaneRecord::from_terminal_pane(second_pane_id),
    ]);

    // No history, no spatial neighbor, no absorbed pane: fall to layout order.
    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, vec![first_pane_id, second_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(first_pane_id)
    );
}

#[test]
fn the_last_resort_walks_past_ineligible_panes_to_the_first_live_one() {
    // The visible layout order leads with a pane that has no registry record;
    // the last-resort step must skip it and focus the first registered pane, not
    // fall through to a no-pane verdict while an eligible pane is still present.
    let (ghost_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![PaneRecord::from_terminal_pane(live_pane_id)]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, vec![ghost_pane_id, live_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(live_pane_id)
    );
}

#[test]
fn a_suppressed_pane_is_never_focused() {
    // `suppressed` is alive and sits in history, but it is absent from the
    // visible layout order, so it is not a focus target.
    let (suppressed_pane_id, visible_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history = [suppressed_pane_id, visible_pane_id];
    let registry = build_registry_with(vec![
        PaneRecord::from_terminal_pane(suppressed_pane_id),
        PaneRecord::from_terminal_pane(visible_pane_id),
    ]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, vec![visible_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(visible_pane_id)
    );
}

#[test]
fn a_history_pane_absent_from_the_registry_is_skipped() {
    let (ghost_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history = [ghost_pane_id, live_pane_id];
    let registry = build_registry_with(vec![PaneRecord::from_terminal_pane(live_pane_id)]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, vec![ghost_pane_id, live_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(live_pane_id)
    );
}

#[test]
fn a_spatial_neighbor_outside_the_visible_layout_order_is_skipped() {
    // The ranked candidates are gated on the visible layout order too, not
    // only the focus history: a live pane the layout order omits is skipped.
    let (hidden_pane_id, visible_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![
        PaneRecord::from_terminal_pane(hidden_pane_id),
        PaneRecord::from_terminal_pane(visible_pane_id),
    ]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(
            Some(hidden_pane_id),
            Some(hidden_pane_id),
            vec![visible_pane_id],
        ),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(visible_pane_id)
    );
}

#[test]
fn visible_panes_all_missing_from_the_registry_report_terminal_too_small() {
    // The tab's layout still names a pane and the layout order lists it, but
    // no pane record backs it, so nothing is eligible and the tab is not empty.
    let ghost_pane_id = PaneId::new();
    let focus_history: [PaneId; 0] = [];
    let registry = PaneRegistry::new();

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, vec![ghost_pane_id]),
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn every_candidate_missing_from_the_registry_reports_terminal_too_small() {
    // Both panes are in the visible layout order, the focus history and the
    // ranked candidates, but neither holds a pane record, so nothing is eligible
    // while the tab's layout still holds a leaf.
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history = [second_pane_id];
    let registry = PaneRegistry::new();

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(
            Some(first_pane_id),
            Some(second_pane_id),
            vec![first_pane_id, second_pane_id],
        ),
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn all_panes_suppressed_reports_terminal_too_small() {
    // The tab still has a leaf, but nothing is visible: the window is too small.
    let only_pane_id = PaneId::new();
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![PaneRecord::from_terminal_pane(only_pane_id)]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(None, None, Vec::new()),
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn an_ineligible_spatial_neighbor_falls_through_to_the_absorbed_pane() {
    // The spatial-neighbor candidate has no registry record, so it must be
    // skipped — the recovery order still has an eligible pane at the next
    // step (`absorbed_space`), and that one must win, not a no-pane verdict.
    let (spatial_neighbor_pane_id, absorbed_pane_id) = (PaneId::new(), PaneId::new());
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![PaneRecord::from_terminal_pane(absorbed_pane_id)]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(
            Some(spatial_neighbor_pane_id),
            Some(absorbed_pane_id),
            vec![spatial_neighbor_pane_id, absorbed_pane_id],
        ),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(absorbed_pane_id)
    );
}

#[test]
fn ineligible_spatial_and_absorbed_candidates_fall_through_to_layout_order() {
    // Both ranked candidates have no registry record; the last-resort layout-order
    // scan finds the one live pane.
    let (spatial_neighbor_pane_id, absorbed_pane_id, live_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let focus_history: [PaneId; 0] = [];
    let registry = build_registry_with(vec![PaneRecord::from_terminal_pane(live_pane_id)]);

    let focus_repair_result = repair_focus(
        &focus_history,
        &registry,
        build_candidates(
            Some(spatial_neighbor_pane_id),
            Some(absorbed_pane_id),
            vec![spatial_neighbor_pane_id, absorbed_pane_id, live_pane_id],
        ),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(live_pane_id)
    );
}

/// A client viewing `active_tab_id` on an `80x24` viewport, focusing nothing,
/// with a minimized view at the default placement of each of
/// `floating_pane_ids`.
fn build_client_on(active_tab_id: TabId, floating_pane_ids: &[PaneId]) -> Client {
    let mut client = Client::from_attachment(
        ClientId::new(),
        SessionId::new(),
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
    );
    for &floating_pane_id in floating_pane_ids {
        client.add_floating_pane_view(floating_pane_id, FloatingPanePosition::Default);
    }
    client
}

#[test]
fn a_move_into_a_floating_pane_reports_no_tab_and_the_pane_it_left() {
    let tab_id = TabId::new();
    let (vim_pane_id, htop_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_client_on(tab_id, &[htop_pane_id]);
    client.update_focused_pane(tab_id, vim_pane_id);
    let previous_focused_pane_id = client.get_active_focused_pane_id();
    assert!(client.focus_floating_pane(htop_pane_id));

    assert_eq!(
        build_pane_focused_event(&client, previous_focused_pane_id),
        Some(Event::PaneFocused(PaneFocused {
            client_id: client.get_client_id(),
            tab_id: None,
            pane_id: htop_pane_id,
            previous_pane_id: Some(vim_pane_id),
        }))
    );
}

#[test]
fn a_move_out_of_the_floating_panes_reports_the_active_tab_and_its_focused_pane() {
    let tab_id = TabId::new();
    let (vim_pane_id, htop_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_client_on(tab_id, &[htop_pane_id]);
    client.update_focused_pane(tab_id, vim_pane_id);
    assert!(client.focus_floating_pane(htop_pane_id));
    let previous_focused_pane_id = client.get_active_focused_pane_id();
    assert!(client.minimize_floating_pane(htop_pane_id));

    assert_eq!(
        build_pane_focused_event(&client, previous_focused_pane_id),
        Some(Event::PaneFocused(PaneFocused {
            client_id: client.get_client_id(),
            tab_id: Some(tab_id),
            pane_id: vim_pane_id,
            previous_pane_id: Some(htop_pane_id),
        }))
    );
}

#[test]
fn a_move_between_floating_panes_reports_no_tab_and_the_floating_pane_it_left() {
    let tab_id = TabId::new();
    let (vim_pane_id, htop_pane_id, btop_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut client = build_client_on(tab_id, &[htop_pane_id, btop_pane_id]);
    client.update_focused_pane(tab_id, vim_pane_id);
    assert!(client.focus_floating_pane(htop_pane_id));
    let previous_focused_pane_id = client.get_active_focused_pane_id();
    assert!(client.focus_floating_pane(btop_pane_id));

    assert_eq!(
        build_pane_focused_event(&client, previous_focused_pane_id),
        Some(Event::PaneFocused(PaneFocused {
            client_id: client.get_client_id(),
            tab_id: None,
            pane_id: btop_pane_id,
            previous_pane_id: Some(htop_pane_id),
        }))
    );
}

#[test]
fn an_unchanged_input_and_no_pane_to_type_into_report_nothing() {
    let tab_id = TabId::new();
    let (vim_pane_id, htop_pane_id) = (PaneId::new(), PaneId::new());
    let mut client = build_client_on(tab_id, &[htop_pane_id]);

    assert_eq!(build_pane_focused_event(&client, None), None);
    assert!(client.focus_floating_pane(htop_pane_id));
    let previous_focused_pane_id = client.get_active_focused_pane_id();
    assert!(client.minimize_floating_pane(htop_pane_id));
    assert_eq!(
        build_pane_focused_event(&client, previous_focused_pane_id),
        None
    );

    client.update_focused_pane(tab_id, vim_pane_id);
    assert_eq!(build_pane_focused_event(&client, Some(vim_pane_id)), None);
}

#[test]
fn a_tiled_focus_in_the_active_tab_reports_each_minimized_floating_pane_then_the_focus() {
    let tab_id = TabId::new();
    let (vim_pane_id, shell_pane_id, htop_pane_id, logs_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new(), PaneId::new());
    let mut client = build_client_on(tab_id, &[htop_pane_id, logs_pane_id]);
    client.update_focused_pane(tab_id, vim_pane_id);
    assert!(client.focus_floating_pane(htop_pane_id));
    assert!(client.focus_floating_pane(logs_pane_id));
    let client_id = client.get_client_id();

    assert_eq!(
        apply_tiled_pane_focus(&mut client, tab_id, shell_pane_id),
        vec![
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id,
                pane_id: htop_pane_id,
                is_minimized: true,
            }),
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id,
                pane_id: logs_pane_id,
                is_minimized: true,
            }),
            Event::PaneFocused(PaneFocused {
                client_id,
                tab_id: Some(tab_id),
                pane_id: shell_pane_id,
                previous_pane_id: Some(logs_pane_id),
            }),
        ]
    );
    assert_eq!(client.get_active_focused_pane_id(), Some(shell_pane_id));
    assert_eq!(client.list_shown_floating_pane_ids(), Vec::<PaneId>::new());
}

#[test]
fn a_tiled_focus_on_the_pane_already_focused_reports_nothing() {
    let tab_id = TabId::new();
    let vim_pane_id = PaneId::new();
    let mut client = build_client_on(tab_id, &[]);
    client.update_focused_pane(tab_id, vim_pane_id);

    assert_eq!(
        apply_tiled_pane_focus(&mut client, tab_id, vim_pane_id),
        Vec::new()
    );
}

#[test]
fn a_tiled_focus_in_another_tab_reports_that_tab_and_minimizes_nothing() {
    let (active_tab_id, other_tab_id) = (TabId::new(), TabId::new());
    let (old_pane_id, new_pane_id, htop_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut client = build_client_on(active_tab_id, &[htop_pane_id]);
    client.update_focused_pane(other_tab_id, old_pane_id);
    assert!(client.focus_floating_pane(htop_pane_id));
    let client_id = client.get_client_id();

    assert_eq!(
        apply_tiled_pane_focus(&mut client, other_tab_id, new_pane_id),
        vec![Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(other_tab_id),
            pane_id: new_pane_id,
            previous_pane_id: Some(old_pane_id),
        })]
    );
    assert_eq!(client.get_active_focused_pane_id(), Some(htop_pane_id));
}

#[test]
fn minimized_changed_events_name_each_pane_in_order_with_the_given_flag() {
    let client_id = ClientId::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());

    assert_eq!(
        build_pane_minimized_changed_events(client_id, &[second_pane_id, first_pane_id], false)
            .collect::<Vec<Event>>(),
        vec![
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id,
                pane_id: second_pane_id,
                is_minimized: false,
            }),
            Event::PaneMinimizedChanged(PaneMinimizedChanged {
                client_id,
                pane_id: first_pane_id,
                is_minimized: false,
            }),
        ]
    );
    assert_eq!(
        build_pane_minimized_changed_events(client_id, &[], true).count(),
        0
    );
}

/// A session holding one tab `vim | shell` and one attached client viewing
/// it that shows and focuses the floating pane `htop` and focuses `vim` in
/// the tab. Returns the session, the tab id, the client id and
/// `[vim, shell, htop]`.
fn build_session_with_floating_focus() -> (Session, TabId, ClientId, [PaneId; 3]) {
    let (vim_pane_id, shell_pane_id, htop_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tab = build_tab(
        TabId::new(),
        "code",
        0,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(vim_pane_id),
                LayoutNode::Pane(shell_pane_id),
            ],
        )),
    );
    let tab_id = tab.get_tab_id();
    let mut session = build_empty_session();
    session.tabs.insert(tab_id, tab);
    let mut client = build_client_on(tab_id, &[htop_pane_id]);
    client.update_focused_pane(tab_id, vim_pane_id);
    assert!(client.focus_floating_pane(htop_pane_id));
    let client_id = client.get_client_id();
    session.attach_client(client);
    (
        session,
        tab_id,
        client_id,
        [vim_pane_id, shell_pane_id, htop_pane_id],
    )
}

#[test]
fn a_tab_focus_moves_the_tabs_focus_and_keeps_the_input_on_the_floating_pane() {
    let (mut session, tab_id, client_id, [vim_pane_id, shell_pane_id, htop_pane_id]) =
        build_session_with_floating_focus();

    let pane_focused_event =
        update_client_tab_focus(&mut session, client_id, tab_id, shell_pane_id);

    assert_eq!(
        pane_focused_event,
        Some(Event::PaneFocused(PaneFocused {
            client_id,
            tab_id: Some(tab_id),
            pane_id: shell_pane_id,
            previous_pane_id: Some(vim_pane_id),
        }))
    );
    let client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the client is attached");
    assert_eq!(
        client.list_focus_history(tab_id),
        &[shell_pane_id, vim_pane_id]
    );
    assert_eq!(client.get_active_focused_pane_id(), Some(htop_pane_id));
    assert_eq!(session.tabs[&tab_id].list_focus_mru(), &[shell_pane_id]);
}

#[test]
fn a_tab_focus_for_a_client_that_is_not_attached_changes_nothing() {
    let (mut session, tab_id, _, [_, shell_pane_id, _]) = build_session_with_floating_focus();

    let pane_focused_event =
        update_client_tab_focus(&mut session, ClientId::new(), tab_id, shell_pane_id);

    assert_eq!(pane_focused_event, None);
    assert_eq!(session.tabs[&tab_id].list_focus_mru(), &[] as &[PaneId]);
}
