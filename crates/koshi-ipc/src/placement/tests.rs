//! Tests for placement-preview validation.

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{PaneId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::LayoutNode;

use crate::frame::FrameSlot;

use super::*;

/// The `80x24` tab every test tab is solved at.
const TEST_TAB_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// A tiled tab `tab_id` holding only `pane_id`, solved at [`TEST_TAB_SIZE`],
/// with `pane_id` focused.
fn build_test_tab_snapshot(tab_id: TabId, pane_id: PaneId) -> PanePlacementTabSnapshot {
    PanePlacementTabSnapshot {
        layout_tree: LayoutNode::Pane(pane_id),
        focus_history: vec![pane_id],
        tab_snapshot: FrameTab {
            tab_id,
            tab_name: "main".to_string(),
            pane_slots: vec![FrameSlot {
                pane_id,
                outer_rect: Rect::from_size_at_origin(TEST_TAB_SIZE),
                content_rect: Some(Rect::from_size_at_origin(TEST_TAB_SIZE)),
                is_visible: true,
                is_suppressed: false,
            }],
            tab_size: TEST_TAB_SIZE,
            stack_headers: Vec::new(),
            layout_mode: LayoutMode::Tiled,
            is_every_pane_suppressed: false,
            gap_cell_count: 0,
        },
    }
}

/// A preview of the only pane of one tab within that tab.
fn build_test_snapshot() -> PanePlacementSnapshot {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    PanePlacementSnapshot {
        source_pane_id: pane_id,
        source: PanePlacementSource::Tiled(build_test_tab_snapshot(tab_id, pane_id)),
        destination_tab_id: tab_id,
        destination_tab_snapshot: None,
        pane_sizing: PanePlacementSizing {
            minimum_size: Size {
                column_count: 1,
                row_count: 1,
            },
            gap_cell_count: 0,
        },
        insertion_tab_sizes: PlacementTabSizes {
            destination_tab_size: Some(TEST_TAB_SIZE),
            source_tab_size: None,
        },
        swap_tab_sizes: Vec::new(),
    }
}

/// The source tab snapshot of `placement_snapshot`, for in-place edits.
/// Panics when the source pane floats.
fn get_source_tab_snapshot_mut(
    placement_snapshot: &mut PanePlacementSnapshot,
) -> &mut PanePlacementTabSnapshot {
    let PanePlacementSource::Tiled(source_tab_snapshot) = &mut placement_snapshot.source else {
        panic!("the test snapshot's source pane is tiled");
    };
    source_tab_snapshot
}

/// [`build_test_snapshot`] previewing a move into another tab `[D]` of the
/// same size, with the swap tab sizes of `D`.
pub(crate) fn build_cross_tab_test_snapshot() -> PanePlacementSnapshot {
    let mut placement_snapshot = build_test_snapshot();
    let destination_tab_id = TabId::new();
    let destination_pane_id = PaneId::new();
    placement_snapshot.destination_tab_id = destination_tab_id;
    placement_snapshot.destination_tab_snapshot = Some(build_test_tab_snapshot(
        destination_tab_id,
        destination_pane_id,
    ));
    placement_snapshot.swap_tab_sizes = vec![SwapTabSizes {
        target_pane_id: destination_pane_id,
        tab_sizes: placement_snapshot.insertion_tab_sizes,
    }];
    placement_snapshot
}

/// [`build_cross_tab_test_snapshot`] with its pane floating: the preview
/// carries no source tab and no swap tab sizes.
fn build_floating_source_test_snapshot() -> PanePlacementSnapshot {
    let mut placement_snapshot = build_cross_tab_test_snapshot();
    placement_snapshot.source = PanePlacementSource::Floating;
    placement_snapshot.swap_tab_sizes.clear();
    placement_snapshot
}

#[test]
fn valid_snapshot_passes_validation() {
    assert_eq!(build_test_snapshot().validate(), Ok(()));
}

#[test]
fn snapshot_rejects_a_missing_cross_tab_snapshot() {
    let mut placement_snapshot = build_test_snapshot();
    placement_snapshot.destination_tab_id = TabId::new();

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::TabIdentityMismatch)
    );
}

#[test]
fn snapshot_rejects_a_destination_snapshot_that_names_another_tab() {
    let mut placement_snapshot = build_cross_tab_test_snapshot();
    placement_snapshot.destination_tab_id = TabId::new();

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::TabIdentityMismatch)
    );
}

#[test]
fn snapshot_rejects_duplicate_pane_slots() {
    let mut placement_snapshot = build_test_snapshot();
    let source_tab_snapshot = get_source_tab_snapshot_mut(&mut placement_snapshot);
    let duplicate_pane_slot = source_tab_snapshot.tab_snapshot.pane_slots[0].clone();
    source_tab_snapshot
        .tab_snapshot
        .pane_slots
        .push(duplicate_pane_slot);

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::DuplicatePaneId)
    );
}

#[test]
fn snapshot_rejects_a_source_pane_outside_the_source_tab() {
    let mut placement_snapshot = build_test_snapshot();
    placement_snapshot.source_pane_id = PaneId::new();

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::SourcePaneMissing)
    );
}

#[test]
fn snapshot_rejects_a_layout_tree_that_does_not_match_the_slots() {
    let mut placement_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut placement_snapshot).layout_tree =
        LayoutNode::Pane(PaneId::new());

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::LayoutMismatch)
    );
}

#[test]
fn snapshot_rejects_a_zoomed_tab_or_a_gap_other_than_the_sizing_gap() {
    let mut zoomed_snapshot = build_test_snapshot();
    let source_pane_id = zoomed_snapshot.source_pane_id;
    get_source_tab_snapshot_mut(&mut zoomed_snapshot)
        .tab_snapshot
        .layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: source_pane_id,
    };
    let mut gap_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut gap_snapshot)
        .tab_snapshot
        .gap_cell_count = 1;

    assert_eq!(
        [zoomed_snapshot.validate(), gap_snapshot.validate()],
        [Err(PanePlacementSnapshotValidationError::LayoutMismatch); 2]
    );
}

#[test]
fn snapshot_rejects_a_focus_history_that_names_a_pane_outside_the_tree() {
    let mut placement_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut placement_snapshot)
        .focus_history
        .push(PaneId::new());

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::LayoutMismatch)
    );
}

#[test]
fn snapshot_rejects_a_focus_history_that_repeats_a_pane() {
    let mut placement_snapshot = build_test_snapshot();
    let source_pane_id = placement_snapshot.source_pane_id;
    get_source_tab_snapshot_mut(&mut placement_snapshot)
        .focus_history
        .push(source_pane_id);

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::LayoutMismatch)
    );
}

#[test]
fn snapshot_rejects_a_slot_outside_the_tab_or_with_flags_that_disagree() {
    let mut outside_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut outside_snapshot)
        .tab_snapshot
        .pane_slots[0]
        .outer_rect = Rect::from_size_at_origin(Size {
        column_count: 81,
        row_count: 24,
    });
    let mut hidden_content_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut hidden_content_snapshot)
        .tab_snapshot
        .pane_slots[0]
        .is_visible = false;
    let mut suppressed_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut suppressed_snapshot)
        .tab_snapshot
        .pane_slots[0]
        .is_suppressed = true;

    assert_eq!(
        [
            outside_snapshot.validate(),
            hidden_content_snapshot.validate(),
            suppressed_snapshot.validate(),
        ],
        [Err(PanePlacementSnapshotValidationError::PaneSlotMismatch); 3]
    );
}

#[test]
fn snapshot_rejects_a_stack_header_for_a_pane_outside_the_tree() {
    let mut placement_snapshot = build_test_snapshot();
    get_source_tab_snapshot_mut(&mut placement_snapshot)
        .tab_snapshot
        .stack_headers
        .push(StackHeader {
            pane_id: PaneId::new(),
            header_rect: Rect::from_size_at_origin(Size {
                column_count: 80,
                row_count: 1,
            }),
            member_index: 0,
            member_count: 2,
        });

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::LayoutMismatch)
    );
}

#[test]
fn a_cross_tab_snapshot_with_swap_tab_sizes_for_each_destination_pane_passes_validation() {
    assert_eq!(build_cross_tab_test_snapshot().validate(), Ok(()));
}

#[test]
fn snapshot_rejects_swap_tab_sizes_that_do_not_name_each_destination_pane_once() {
    let mut missing_swap_snapshot = build_cross_tab_test_snapshot();
    missing_swap_snapshot.swap_tab_sizes.clear();
    let mut foreign_swap_snapshot = build_cross_tab_test_snapshot();
    foreign_swap_snapshot.swap_tab_sizes[0].target_pane_id = PaneId::new();
    let mut repeated_swap_snapshot = build_cross_tab_test_snapshot();
    let repeated_swap_tab_sizes = repeated_swap_snapshot.swap_tab_sizes[0];
    repeated_swap_snapshot
        .swap_tab_sizes
        .push(repeated_swap_tab_sizes);

    assert_eq!(
        [
            missing_swap_snapshot.validate(),
            foreign_swap_snapshot.validate(),
            repeated_swap_snapshot.validate(),
        ],
        [Err(PanePlacementSnapshotValidationError::TabSizesMismatch); 3]
    );
}

#[test]
fn snapshot_rejects_swap_tab_sizes_within_one_tab_or_for_a_floating_source() {
    let mut same_tab_snapshot = build_test_snapshot();
    same_tab_snapshot.swap_tab_sizes = vec![SwapTabSizes {
        target_pane_id: same_tab_snapshot.source_pane_id,
        tab_sizes: same_tab_snapshot.insertion_tab_sizes,
    }];
    let mut floating_source_snapshot = build_floating_source_test_snapshot();
    let destination_pane_id = floating_source_snapshot
        .destination_tab_snapshot
        .as_ref()
        .expect("the floating source previews a destination tab")
        .tab_snapshot
        .pane_slots[0]
        .pane_id;
    floating_source_snapshot.swap_tab_sizes = vec![SwapTabSizes {
        target_pane_id: destination_pane_id,
        tab_sizes: floating_source_snapshot.insertion_tab_sizes,
    }];

    assert_eq!(
        [
            same_tab_snapshot.validate(),
            floating_source_snapshot.validate()
        ],
        [Err(PanePlacementSnapshotValidationError::TabSizesMismatch); 2]
    );
}

#[test]
fn snapshot_rejects_duplicate_pane_ids_across_tabs() {
    let mut placement_snapshot = build_test_snapshot();
    let destination_tab_id = TabId::new();
    placement_snapshot.destination_tab_id = destination_tab_id;
    placement_snapshot.destination_tab_snapshot = Some(build_test_tab_snapshot(
        destination_tab_id,
        placement_snapshot.source_pane_id,
    ));

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::DuplicatePaneId)
    );
}

#[test]
fn a_floating_source_with_a_destination_tab_passes_validation() {
    assert_eq!(build_floating_source_test_snapshot().validate(), Ok(()));
}

#[test]
fn a_floating_source_that_the_destination_tab_holds_is_refused() {
    let mut placement_snapshot = build_floating_source_test_snapshot();
    placement_snapshot.source_pane_id = placement_snapshot
        .destination_tab_snapshot
        .as_ref()
        .expect("the destination tab is carried")
        .tab_snapshot
        .pane_slots[0]
        .pane_id;

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::DuplicatePaneId)
    );
}

#[test]
fn a_floating_source_with_no_destination_tab_is_refused() {
    let mut placement_snapshot = build_floating_source_test_snapshot();
    placement_snapshot.destination_tab_snapshot = None;

    assert_eq!(
        placement_snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::TabIdentityMismatch)
    );
}

#[test]
fn a_preview_survives_a_serde_round_trip() {
    for placement_preview in [
        PanePlacementPreview::Available(Box::new(build_cross_tab_test_snapshot())),
        PanePlacementPreview::Available(Box::new(build_floating_source_test_snapshot())),
        PanePlacementPreview::Unavailable {
            source_pane_id: PaneId::new(),
            destination_tab_id: TabId::new(),
        },
    ] {
        let preview_json_bytes =
            serde_json::to_vec(&placement_preview).expect("a preview serializes");

        assert_eq!(
            serde_json::from_slice::<PanePlacementPreview>(&preview_json_bytes)
                .expect("a preview decodes"),
            placement_preview
        );
    }
}
