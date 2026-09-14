//! Tests for focus repair: choosing which pane gets keyboard focus after a layout change.
//!
//! Verifies that `repair_focus` walks the recovery hierarchy in order: focus history (MRU),
//! spatial neighbor, absorbed pane, and finally the first visible pane in layout order.
//! Also validates the eligibility rule — a pane must sit in the visible layout order and
//! hold a registry pane record in any state but `Removed` — and the two no-pane verdicts.

use std::time::SystemTime;

use koshi_core::geometry::SplitDirection;
use koshi_core::ids::{PaneId, TabId};
use koshi_layout::focus::FocusCandidates;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use koshi_pane::pane::policy::PaneClosePolicy;
use koshi_pane::pane::state::PaneRecord;
use koshi_pane::registry::PaneRegistry;

use super::{repair_focus, FocusRepairResult};
use crate::session::policy::EmptyTabPolicy;
use crate::session::state::Tab;

/// A tab whose only leaf is `root`, with no focus history recorded yet.
fn tab_with_root(root: PaneId) -> Tab {
    Tab::from_root_pane(TabId::new(), "code".to_owned(), 0, root)
}

/// A terminal-pane pane record in `lifecycle`. Timestamps use `UNIX_EPOCH` so tests
/// stay deterministic. `lifecycle` is set only through events, so the fresh
/// `Spawning` pane record is walked to the requested state along a legal path.
fn build_pane_record(pane_id: PaneId, lifecycle: PaneLifecycle) -> PaneRecord {
    let mut pane_record = PaneRecord::from_terminal_pane(pane_id, SystemTime::UNIX_EPOCH);
    pane_record.close_policy = PaneClosePolicy::Force;
    walk_lifecycle(&mut pane_record, lifecycle);
    pane_record
}

/// Walk a fresh `Spawning` pane record to `target` through legal lifecycle events.
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

/// A registry holding exactly `pane_records`.
fn registry_with(pane_records: Vec<PaneRecord>) -> PaneRegistry {
    let mut registry = PaneRegistry::new();
    for pane_record in pane_records {
        registry
            .register_pane_record(pane_record)
            .expect("unique pane id");
    }
    registry
}

/// Construct a [`FocusCandidates`] struct from the given spatial neighbor, absorbed pane,
/// and visible layout order.
fn candidates(
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
    let (older, newer) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(newer);
    tab.record_focus_mru(older);
    tab.record_focus_mru(newer); // newest first: [newer, older]
    let registry = registry_with(vec![
        build_pane_record(older, PaneLifecycle::Running),
        build_pane_record(newer, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![newer, older]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(newer));
}

#[test]
fn history_outranks_the_spatial_neighbor_and_absorbed_pane() {
    let (history, spatial, absorbed) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(history);
    tab.record_focus_mru(history);
    let registry = registry_with(vec![
        build_pane_record(history, PaneLifecycle::Running),
        build_pane_record(spatial, PaneLifecycle::Running),
        build_pane_record(absorbed, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(
            Some(spatial),
            Some(absorbed),
            vec![history, spatial, absorbed],
        ),
        EmptyTabPolicy::CloseTab,
    );

    // All three are eligible; the recovery order picks history first.
    assert_eq!(focus_repair_result, FocusRepairResult::Focused(history));
}

#[test]
fn the_spatial_neighbor_wins_when_history_has_no_eligible_pane() {
    let (spatial, absorbed) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(spatial); // no focus history recorded
    let registry = registry_with(vec![
        build_pane_record(spatial, PaneLifecycle::Running),
        build_pane_record(absorbed, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(Some(spatial), Some(absorbed), vec![spatial, absorbed]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(spatial));
}

#[test]
fn the_absorbed_pane_wins_with_no_history_and_no_spatial_neighbor() {
    let absorbed = PaneId::new();
    let tab = tab_with_root(absorbed);
    let registry = registry_with(vec![build_pane_record(absorbed, PaneLifecycle::Running)]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, Some(absorbed), vec![absorbed]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(absorbed));
}

#[test]
fn the_first_visible_pane_is_the_last_resort() {
    let (first, second) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(first);
    let registry = registry_with(vec![
        build_pane_record(first, PaneLifecycle::Running),
        build_pane_record(second, PaneLifecycle::Running),
    ]);

    // No history, no spatial neighbor, no absorbed pane: fall to layout order.
    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![first, second]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(first));
}

#[test]
fn the_last_resort_walks_past_ineligible_panes_to_the_first_live_one() {
    // The visible layout order leads with a Removed pane; the last-resort step
    // must skip it and focus the first live pane, not fall through to a no-pane
    // verdict while an eligible pane is still present.
    let (removed, live) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(live); // no focus history recorded
    let registry = registry_with(vec![
        build_pane_record(removed, PaneLifecycle::Removed),
        build_pane_record(live, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![removed, live]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(live));
}

#[test]
fn a_suppressed_pane_is_never_focused() {
    // `suppressed` is alive and sits in history, but it is absent from the
    // visible layout order, so it is not a focus target.
    let (suppressed, visible) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(visible);
    tab.record_focus_mru(visible);
    tab.record_focus_mru(suppressed); // newest, but suppressed
    let registry = registry_with(vec![
        build_pane_record(suppressed, PaneLifecycle::Running),
        build_pane_record(visible, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![visible]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(visible));
}

#[test]
fn a_dead_exited_pane_is_eligible_for_focus() {
    // A dead pane is a visible, focusable placeholder, so focus may land on it.
    let dead = PaneId::new();
    let mut tab = tab_with_root(dead);
    tab.record_focus_mru(dead);
    let registry = registry_with(vec![build_pane_record(
        dead,
        PaneLifecycle::Exited {
            exit_code: None,
            exited_at: SystemTime::UNIX_EPOCH,
        },
    )]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![dead]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(dead));
}

#[test]
fn a_closing_pane_is_eligible_for_focus() {
    // Only `Removed` is skipped; a pane mid-teardown stays focusable until gone.
    let closing = PaneId::new();
    let mut tab = tab_with_root(closing);
    tab.record_focus_mru(closing);
    let registry = registry_with(vec![build_pane_record(
        closing,
        PaneLifecycle::Closing {
            close_requested_at: SystemTime::UNIX_EPOCH,
        },
    )]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![closing]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(closing));
}

#[test]
fn a_removed_pane_in_history_is_skipped() {
    let (removed, live) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(live);
    tab.record_focus_mru(live);
    tab.record_focus_mru(removed); // newest, but Removed
    let registry = registry_with(vec![
        build_pane_record(removed, PaneLifecycle::Removed),
        build_pane_record(live, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![removed, live]),
        EmptyTabPolicy::CloseTab,
    );

    // The Removed pane is skipped even though it is newest and visible.
    assert_eq!(focus_repair_result, FocusRepairResult::Focused(live));
}

#[test]
fn a_history_pane_absent_from_the_registry_is_skipped() {
    let (ghost, live) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(live);
    tab.record_focus_mru(live);
    tab.record_focus_mru(ghost); // newest, but not in the registry
    let registry = registry_with(vec![build_pane_record(live, PaneLifecycle::Running)]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![ghost, live]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(live));
}

#[test]
fn a_spawning_pane_is_eligible_for_focus() {
    // A pane whose process has not started yet is still a visible placeholder.
    let spawning = PaneId::new();
    let mut tab = tab_with_root(spawning);
    tab.record_focus_mru(spawning);
    let registry = registry_with(vec![build_pane_record(spawning, PaneLifecycle::Spawning)]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![spawning]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(spawning));
}

#[test]
fn a_spatial_neighbor_outside_the_visible_layout_order_is_skipped() {
    // The ranked candidates are gated on the visible layout order too, not
    // only the focus history: a live pane the layout order omits is skipped.
    let (hidden, visible) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(visible); // no focus history recorded
    let registry = registry_with(vec![
        build_pane_record(hidden, PaneLifecycle::Running),
        build_pane_record(visible, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(Some(hidden), Some(hidden), vec![visible]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(visible));
}

#[test]
fn visible_panes_all_missing_from_the_registry_report_terminal_too_small() {
    // The tab's layout still names a pane and the layout order lists it, but
    // no pane record backs it, so nothing is eligible and the tab is not empty.
    let ghost = PaneId::new();
    let tab = tab_with_root(ghost);
    let registry = PaneRegistry::new();

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![ghost]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn every_visible_pane_removed_reports_terminal_too_small() {
    // Both panes are in the visible layout order and both hold a pane record, but
    // both records are `Removed`, so nothing is eligible while the tab's layout
    // still holds a leaf.
    let (first, second) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(first);
    tab.record_focus_mru(second);
    let registry = registry_with(vec![
        build_pane_record(first, PaneLifecycle::Removed),
        build_pane_record(second, PaneLifecycle::Removed),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(Some(first), Some(second), vec![first, second]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn all_panes_suppressed_reports_terminal_too_small() {
    // The tab still has a leaf, but nothing is visible: the window is too small.
    let only = PaneId::new();
    let tab = tab_with_root(only);
    let registry = registry_with(vec![build_pane_record(only, PaneLifecycle::Running)]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, Vec::new()),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn an_ineligible_spatial_neighbor_falls_through_to_the_absorbed_pane() {
    // The spatial-neighbor candidate is present but `Removed`, so it must be
    // skipped — the recovery order still has an eligible pane at the next
    // step (`absorbed_space`), and that one must win, not a no-pane verdict.
    let (spatial, absorbed) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(spatial); // no focus history recorded
    let registry = registry_with(vec![
        build_pane_record(spatial, PaneLifecycle::Removed),
        build_pane_record(absorbed, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(Some(spatial), Some(absorbed), vec![spatial, absorbed]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(absorbed));
}

#[test]
fn ineligible_spatial_and_absorbed_candidates_fall_through_to_layout_order() {
    // Both ranked candidates are ineligible; the last-resort layout-order
    // scan must still find the one live pane rather than reporting
    // `TerminalTooSmall` while a focusable pane is actually present.
    let (spatial, absorbed, live) = (PaneId::new(), PaneId::new(), PaneId::new());
    let tab = tab_with_root(live);
    let registry = registry_with(vec![
        build_pane_record(spatial, PaneLifecycle::Removed),
        build_pane_record(absorbed, PaneLifecycle::Removed),
        build_pane_record(live, PaneLifecycle::Running),
    ]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(Some(spatial), Some(absorbed), vec![spatial, absorbed, live]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(focus_repair_result, FocusRepairResult::Focused(live));
}

/// A tab whose layout holds no leaf at all.
fn empty_tab() -> Tab {
    let mut tab = tab_with_root(PaneId::new());
    tab.update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    )));
    tab
}

#[test]
fn an_empty_tab_carries_the_empty_tab_policy_back_unchanged() {
    // A tab with no leaves at all falls to its empty-tab policy, passed
    // straight through to the caller.
    let tab = empty_tab();
    let registry = PaneRegistry::new();

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, Vec::new()),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::EmptyTab(EmptyTabPolicy::CloseTab)
    );
}

#[test]
fn an_empty_tab_ignores_focus_history_left_behind() {
    // History naming panes that no longer exist must not resurrect a verdict:
    // none is in the layout order, so the empty-tab policy still wins.
    let stale = PaneId::new();
    let mut tab = empty_tab();
    tab.record_focus_mru(stale);
    let registry = registry_with(vec![build_pane_record(stale, PaneLifecycle::Running)]);

    let focus_repair_result = repair_focus(
        &tab,
        &registry,
        candidates(Some(stale), Some(stale), Vec::new()),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(
        focus_repair_result,
        FocusRepairResult::EmptyTab(EmptyTabPolicy::CloseTab)
    );
}
