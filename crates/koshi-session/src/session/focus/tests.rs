//! Tests for focus repair: choosing which pane gets keyboard focus after a layout change.
//!
//! Verifies that `repair_focus` walks the recovery hierarchy in order: focus history (MRU),
//! spatial neighbor, absorbed pane, and finally the first visible pane in layout order.
//! Also validates the eligibility rule — a pane must sit in the visible layout order and
//! hold a registry pane record in any state but `Removed` — and the two no-pane verdicts.

use std::time::SystemTime;

use koshi_core::ids::{PaneId, TabId};
use koshi_layout::focus::FocusCandidates;
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::PaneClosePolicy;
use koshi_pane::pane::state::PaneRecord;
use koshi_pane::registry::PaneRegistry;

use super::{repair_focus, FocusRepairResult};
use crate::session::state::Tab;

/// A tab whose only leaf is `root_pane_id`, with no focus history recorded yet.
fn build_tab_with_root(root_pane_id: PaneId) -> Tab {
    Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, root_pane_id)
}

/// A terminal pane record in `pane_lifecycle`. Timestamps use `UNIX_EPOCH` so tests
/// stay deterministic. `pane_lifecycle` is set only through events, so the fresh
/// `Spawning` pane record is walked to the requested state along a legal path.
fn build_pane_record(pane_id: PaneId, pane_lifecycle: PaneLifecycle) -> PaneRecord {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id);
    pane_record.close_policy = PaneClosePolicy::Force;
    walk_pane_lifecycle(&mut pane_record, pane_lifecycle);
    pane_record
}

/// Walk a fresh `Spawning` pane record to `target_pane_lifecycle` through legal lifecycle events.
fn walk_pane_lifecycle(pane_record: &mut PaneRecord, target_pane_lifecycle: PaneLifecycle) {
    match target_pane_lifecycle {
        PaneLifecycle::Spawning => {}
        PaneLifecycle::Running => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_pane_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Exited {
            exit_code,
            exited_at,
        } => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_pane_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessExited {
                    exit_code,
                    exited_at,
                })
                .expect("walk_pane_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Closing { close_requested_at } => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_pane_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::CloseRequested { close_requested_at })
                .expect("walk_pane_lifecycle drives only legal transitions");
        }
        PaneLifecycle::Removed => {
            pane_record
                .update_lifecycle(PaneLifecycleEvent::ProcessStarted)
                .expect("walk_pane_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::CloseRequested {
                    close_requested_at: SystemTime::UNIX_EPOCH,
                })
                .expect("walk_pane_lifecycle drives only legal transitions");
            pane_record
                .update_lifecycle(PaneLifecycleEvent::Cleaned)
                .expect("walk_pane_lifecycle drives only legal transitions");
        }
    }
}

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
    let mut tab = build_tab_with_root(newer_pane_id);
    tab.record_focus_mru(older_pane_id);
    tab.record_focus_mru(newer_pane_id); // newest first: [newer, older]
    let registry = build_registry_with(vec![
        build_pane_record(older_pane_id, PaneLifecycle::Running),
        build_pane_record(newer_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
    let mut tab = build_tab_with_root(history_pane_id);
    tab.record_focus_mru(history_pane_id);
    let registry = build_registry_with(vec![
        build_pane_record(history_pane_id, PaneLifecycle::Running),
        build_pane_record(spatial_neighbor_pane_id, PaneLifecycle::Running),
        build_pane_record(absorbed_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
    let tab = build_tab_with_root(spatial_neighbor_pane_id); // no focus history recorded
    let registry = build_registry_with(vec![
        build_pane_record(spatial_neighbor_pane_id, PaneLifecycle::Running),
        build_pane_record(absorbed_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
    let tab = build_tab_with_root(absorbed_pane_id);
    let registry = build_registry_with(vec![build_pane_record(
        absorbed_pane_id,
        PaneLifecycle::Running,
    )]);

    let focus_repair_result = repair_focus(
        &tab,
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
    let tab = build_tab_with_root(first_pane_id);
    let registry = build_registry_with(vec![
        build_pane_record(first_pane_id, PaneLifecycle::Running),
        build_pane_record(second_pane_id, PaneLifecycle::Running),
    ]);

    // No history, no spatial neighbor, no absorbed pane: fall to layout order.
    let focus_repair_result = repair_focus(
        &tab,
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
    // The visible layout order leads with a Removed pane; the last-resort step
    // must skip it and focus the first live pane, not fall through to a no-pane
    // verdict while an eligible pane is still present.
    let (removed_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let tab = build_tab_with_root(live_pane_id); // no focus history recorded
    let registry = build_registry_with(vec![
        build_pane_record(removed_pane_id, PaneLifecycle::Removed),
        build_pane_record(live_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![removed_pane_id, live_pane_id]),
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
    let mut tab = build_tab_with_root(visible_pane_id);
    tab.record_focus_mru(visible_pane_id);
    tab.record_focus_mru(suppressed_pane_id); // newest, but suppressed
    let registry = build_registry_with(vec![
        build_pane_record(suppressed_pane_id, PaneLifecycle::Running),
        build_pane_record(visible_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![visible_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(visible_pane_id)
    );
}

#[test]
fn a_dead_exited_pane_is_eligible_for_focus() {
    // A dead pane is a visible, focusable placeholder, so focus may land on it.
    let dead_pane_id = PaneId::new();
    let mut tab = build_tab_with_root(dead_pane_id);
    tab.record_focus_mru(dead_pane_id);
    let registry = build_registry_with(vec![build_pane_record(
        dead_pane_id,
        PaneLifecycle::Exited {
            exit_code: None,
            exited_at: SystemTime::UNIX_EPOCH,
        },
    )]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![dead_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(dead_pane_id)
    );
}

#[test]
fn a_closing_pane_is_eligible_for_focus() {
    // Only `Removed` is skipped; a pane mid-teardown stays focusable until gone.
    let closing_pane_id = PaneId::new();
    let mut tab = build_tab_with_root(closing_pane_id);
    tab.record_focus_mru(closing_pane_id);
    let registry = build_registry_with(vec![build_pane_record(
        closing_pane_id,
        PaneLifecycle::Closing {
            close_requested_at: SystemTime::UNIX_EPOCH,
        },
    )]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![closing_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(closing_pane_id)
    );
}

#[test]
fn a_removed_pane_in_history_is_skipped() {
    let (removed_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let mut tab = build_tab_with_root(live_pane_id);
    tab.record_focus_mru(live_pane_id);
    tab.record_focus_mru(removed_pane_id); // newest, but Removed
    let registry = build_registry_with(vec![
        build_pane_record(removed_pane_id, PaneLifecycle::Removed),
        build_pane_record(live_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![removed_pane_id, live_pane_id]),
    );

    // The Removed pane is skipped even though it is newest and visible.
    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(live_pane_id)
    );
}

#[test]
fn a_history_pane_absent_from_the_registry_is_skipped() {
    let (ghost_pane_id, live_pane_id) = (PaneId::new(), PaneId::new());
    let mut tab = build_tab_with_root(live_pane_id);
    tab.record_focus_mru(live_pane_id);
    tab.record_focus_mru(ghost_pane_id); // newest, but not in the registry
    let registry = build_registry_with(vec![build_pane_record(
        live_pane_id,
        PaneLifecycle::Running,
    )]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![ghost_pane_id, live_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(live_pane_id)
    );
}

#[test]
fn a_spawning_pane_is_eligible_for_focus() {
    // A pane whose process has not started yet is still a visible placeholder.
    let spawning_pane_id = PaneId::new();
    let mut tab = build_tab_with_root(spawning_pane_id);
    tab.record_focus_mru(spawning_pane_id);
    let registry = build_registry_with(vec![build_pane_record(
        spawning_pane_id,
        PaneLifecycle::Spawning,
    )]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![spawning_pane_id]),
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::Focused(spawning_pane_id)
    );
}

#[test]
fn a_spatial_neighbor_outside_the_visible_layout_order_is_skipped() {
    // The ranked candidates are gated on the visible layout order too, not
    // only the focus history: a live pane the layout order omits is skipped.
    let (hidden_pane_id, visible_pane_id) = (PaneId::new(), PaneId::new());
    let tab = build_tab_with_root(visible_pane_id); // no focus history recorded
    let registry = build_registry_with(vec![
        build_pane_record(hidden_pane_id, PaneLifecycle::Running),
        build_pane_record(visible_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
    let tab = build_tab_with_root(ghost_pane_id);
    let registry = PaneRegistry::new();

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        build_candidates(None, None, vec![ghost_pane_id]),
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn every_visible_pane_removed_reports_terminal_too_small() {
    // Both panes are in the visible layout order and both hold a pane record, but
    // both records are `Removed`, so nothing is eligible while the tab's layout
    // still holds a leaf.
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let mut tab = build_tab_with_root(first_pane_id);
    tab.record_focus_mru(second_pane_id);
    let registry = build_registry_with(vec![
        build_pane_record(first_pane_id, PaneLifecycle::Removed),
        build_pane_record(second_pane_id, PaneLifecycle::Removed),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
    let tab = build_tab_with_root(only_pane_id);
    let registry = build_registry_with(vec![build_pane_record(
        only_pane_id,
        PaneLifecycle::Running,
    )]);

    let focus_repair_result =
        repair_focus(&tab, &registry, build_candidates(None, None, Vec::new()));

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn an_ineligible_spatial_neighbor_falls_through_to_the_absorbed_pane() {
    // The spatial-neighbor candidate is present but `Removed`, so it must be
    // skipped — the recovery order still has an eligible pane at the next
    // step (`absorbed_space`), and that one must win, not a no-pane verdict.
    let (spatial_neighbor_pane_id, absorbed_pane_id) = (PaneId::new(), PaneId::new());
    let tab = build_tab_with_root(spatial_neighbor_pane_id); // no focus history recorded
    let registry = build_registry_with(vec![
        build_pane_record(spatial_neighbor_pane_id, PaneLifecycle::Removed),
        build_pane_record(absorbed_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
    // Both ranked candidates are ineligible; the last-resort layout-order
    // scan must still find the one live pane rather than reporting
    // `TerminalTooSmall` while a focusable pane is actually present.
    let (spatial_neighbor_pane_id, absorbed_pane_id, live_pane_id) =
        (PaneId::new(), PaneId::new(), PaneId::new());
    let tab = build_tab_with_root(live_pane_id);
    let registry = build_registry_with(vec![
        build_pane_record(spatial_neighbor_pane_id, PaneLifecycle::Removed),
        build_pane_record(absorbed_pane_id, PaneLifecycle::Removed),
        build_pane_record(live_pane_id, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
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
