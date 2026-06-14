use std::time::SystemTime;

use tile_core::geometry::SplitDirection;
use tile_core::ids::{PaneId, TabId};
use tile_layout::focus::FocusCandidates;
use tile_layout::tree::{LayoutNode, SplitNode};
use tile_pane::pane::lifecycle::{PaneLifecycle, PaneLifecycleEvent};
use tile_pane::pane::policy::PaneClosePolicy;
use tile_pane::pane::state::PaneRecord;
use tile_pane::registry::PaneRegistry;

use super::{repair_focus, FocusRepairResult};
use crate::session::policy::EmptyTabPolicy;
use crate::session::state::Tab;

/// A tab whose only leaf is `root`, with no focus history recorded yet.
fn tab_with_root(root: PaneId) -> Tab {
    Tab::new(TabId::new(), "code".to_owned(), 0, root)
}

/// A terminal-pane record in `lifecycle`. Timestamps use `UNIX_EPOCH` so tests
/// stay deterministic. `lifecycle` is set only through events, so the fresh
/// `Spawning` record is walked to the requested state along a legal path.
fn record(id: PaneId, lifecycle: PaneLifecycle) -> PaneRecord {
    let mut record = PaneRecord::new(id, SystemTime::UNIX_EPOCH);
    record.close_policy = PaneClosePolicy::Force;
    walk_lifecycle(&mut record, lifecycle);
    record
}

/// Walk a fresh `Spawning` record to `target` through legal lifecycle events.
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

/// A registry holding exactly `records`.
fn registry_with(records: Vec<PaneRecord>) -> PaneRegistry {
    let mut registry = PaneRegistry::new();
    for pane in records {
        registry.insert(pane).expect("unique pane id");
    }
    registry
}

/// Focus candidates with the given spatial neighbor, absorbed pane, and
/// visible layout order.
fn candidates(
    spatial_neighbor: Option<PaneId>,
    absorbed_space: Option<PaneId>,
    layout_order: Vec<PaneId>,
) -> FocusCandidates {
    FocusCandidates {
        spatial_neighbor,
        absorbed_space,
        layout_order,
    }
}

#[test]
fn the_most_recent_history_pane_is_focused_first() {
    let (older, newer) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(newer);
    tab.record_focus_mru(older);
    tab.record_focus_mru(newer); // newest first: [newer, older]
    let registry = registry_with(vec![
        record(older, PaneLifecycle::Running),
        record(newer, PaneLifecycle::Running),
    ]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![newer, older]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(newer));
}

#[test]
fn history_outranks_the_spatial_neighbor_and_absorbed_pane() {
    let (history, spatial, absorbed) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(history);
    tab.record_focus_mru(history);
    let registry = registry_with(vec![
        record(history, PaneLifecycle::Running),
        record(spatial, PaneLifecycle::Running),
        record(absorbed, PaneLifecycle::Running),
    ]);

    let result = repair_focus(
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
    assert_eq!(result, FocusRepairResult::Focused(history));
}

#[test]
fn the_spatial_neighbor_wins_when_history_has_no_eligible_pane() {
    let (spatial, absorbed) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(spatial); // no focus history recorded
    let registry = registry_with(vec![
        record(spatial, PaneLifecycle::Running),
        record(absorbed, PaneLifecycle::Running),
    ]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(Some(spatial), Some(absorbed), vec![spatial, absorbed]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(spatial));
}

#[test]
fn the_absorbed_pane_wins_with_no_history_and_no_spatial_neighbor() {
    let absorbed = PaneId::new();
    let tab = tab_with_root(absorbed);
    let registry = registry_with(vec![record(absorbed, PaneLifecycle::Running)]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, Some(absorbed), vec![absorbed]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(absorbed));
}

#[test]
fn the_first_visible_pane_is_the_last_resort() {
    let (first, second) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(first);
    let registry = registry_with(vec![
        record(first, PaneLifecycle::Running),
        record(second, PaneLifecycle::Running),
    ]);

    // No history, no spatial neighbor, no absorbed pane: fall to layout order.
    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![first, second]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(first));
}

#[test]
fn the_last_resort_walks_past_ineligible_panes_to_the_first_live_one() {
    // The visible layout order leads with a Removed pane; the last-resort step
    // must skip it and focus the first live pane, not fall through to a no-pane
    // verdict while an eligible pane is still present.
    let (removed, live) = (PaneId::new(), PaneId::new());
    let tab = tab_with_root(live); // no focus history recorded
    let registry = registry_with(vec![
        record(removed, PaneLifecycle::Removed),
        record(live, PaneLifecycle::Running),
    ]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![removed, live]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(live));
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
        record(suppressed, PaneLifecycle::Running),
        record(visible, PaneLifecycle::Running),
    ]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![visible]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(visible));
}

#[test]
fn a_dead_exited_pane_is_eligible_for_focus() {
    // A dead pane is a visible, focusable placeholder, so focus may land on it.
    let dead = PaneId::new();
    let mut tab = tab_with_root(dead);
    tab.record_focus_mru(dead);
    let registry = registry_with(vec![record(
        dead,
        PaneLifecycle::Exited {
            code: None,
            at: SystemTime::UNIX_EPOCH,
        },
    )]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![dead]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(dead));
}

#[test]
fn a_closing_pane_is_eligible_for_focus() {
    // Only `Removed` is skipped; a pane mid-teardown stays focusable until gone.
    let closing = PaneId::new();
    let mut tab = tab_with_root(closing);
    tab.record_focus_mru(closing);
    let registry = registry_with(vec![record(
        closing,
        PaneLifecycle::Closing {
            since: SystemTime::UNIX_EPOCH,
        },
    )]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![closing]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(closing));
}

#[test]
fn a_removed_pane_in_history_is_skipped() {
    let (removed, live) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(live);
    tab.record_focus_mru(live);
    tab.record_focus_mru(removed); // newest, but Removed
    let registry = registry_with(vec![
        record(removed, PaneLifecycle::Removed),
        record(live, PaneLifecycle::Running),
    ]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![removed, live]),
        EmptyTabPolicy::CloseTab,
    );

    // The Removed pane is skipped even though it is newest and visible.
    assert_eq!(result, FocusRepairResult::Focused(live));
}

#[test]
fn a_history_pane_absent_from_the_registry_is_skipped() {
    let (ghost, live) = (PaneId::new(), PaneId::new());
    let mut tab = tab_with_root(live);
    tab.record_focus_mru(live);
    tab.record_focus_mru(ghost); // newest, but not in the registry
    let registry = registry_with(vec![record(live, PaneLifecycle::Running)]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, vec![ghost, live]),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::Focused(live));
}

#[test]
fn all_panes_suppressed_reports_terminal_too_small() {
    // The tab still has a leaf, but nothing is visible: the window is too small.
    let only = PaneId::new();
    let tab = tab_with_root(only);
    let registry = registry_with(vec![record(only, PaneLifecycle::Running)]);

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, Vec::new()),
        EmptyTabPolicy::CloseTab,
    );

    assert_eq!(result, FocusRepairResult::TerminalTooSmall);
}

#[test]
fn an_empty_tab_reports_the_empty_tab_policy() {
    // A tab with no leaves at all falls to its empty-tab policy, carried out.
    let mut tab = tab_with_root(PaneId::new());
    tab.layout = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        Vec::new(),
    ));
    let registry = PaneRegistry::new();

    let result = repair_focus(
        &tab,
        &registry,
        candidates(None, None, Vec::new()),
        EmptyTabPolicy::RespawnShell,
    );

    assert_eq!(
        result,
        FocusRepairResult::EmptyTab(EmptyTabPolicy::RespawnShell)
    );
}
