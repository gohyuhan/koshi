//! Tests for bounded placement-preview validation.

use crate::frame::{
    FrameAttrs, FrameCell, FrameImageAction, FrameImagePlacement, FrameImageRecordHeader, FrameRow,
    FrameRun, FrameStyle, FrameWindow,
};
use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneKind;

use super::*;

fn build_test_frame_cell() -> FrameCell {
    FrameCell {
        character: ' ',
        combining_characters: Vec::new(),
        cell_width: 1,
        style: FrameStyle {
            foreground_color: Default::default(),
            background_color: Default::default(),
            underline_color: None,
            text_attributes: FrameAttrs {
                is_bold: false,
                is_italic: false,
                is_reverse: false,
                is_faint: false,
                is_blinking: false,
                is_concealed: false,
                is_struck_through: false,
                is_overlined: false,
                underline_style: Default::default(),
            },
        },
    }
}

fn build_test_window(column_count: u16, row_count: u16) -> FrameWindow {
    FrameWindow {
        column_count,
        row_snapshots: vec![
            FrameRow {
                cell_runs: vec![FrameRun {
                    repeat_count: column_count,
                    cell: build_test_frame_cell(),
                }],
                row_end: Default::default(),
            };
            usize::from(row_count)
        ],
        view_row_offset: 0,
    }
}

fn build_test_image_placement(placement_id: u64, image_content_id: u64) -> FrameImagePlacement {
    FrameImagePlacement {
        cell_geometry: None,
        image_record: Some(FrameImageRecordHeader {
            protocol: Default::default(),
            pixel_width: 4_096,
            pixel_height: 4_096,
            image_action: FrameImageAction::Display,
            display: Default::default(),
            anchor_cell: (0, 0),
        }),
        placement_id,
        image_content_id,
        is_available: true,
        anchor_cell: (0, 0),
        column_count: 1,
        row_count: 1,
    }
}

fn build_test_snapshot() -> PanePlacementSnapshot {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    PanePlacementSnapshot {
        session_id: SessionId::new(),
        source_pane_id: pane_id,
        source_tab_id: tab_id,
        destination_tab_id: tab_id,
        session_placement_revision: 3,
        client_placement_revision: 4,
        source_tab_snapshot: PanePlacementTabSnapshot {
            tab_id,
            tab_name: "main".to_string(),
            layout_tree: LayoutNode::Pane(pane_id),
            pane_slots: vec![FrameSlot {
                pane_id,
                outer_rect: Rect::from_size_at_origin(viewport_size),
                content_rect: Some(Rect::from_size_at_origin(viewport_size)),
                pane_kind: PaneKind::Terminal,
                is_visible: true,
                is_suppressed: false,
                is_dead: false,
            }],
            effective_cell_size: viewport_size,
            stack_headers: Vec::new(),
            layout_mode: LayoutMode::Tiled,
            is_every_pane_suppressed: false,
            gap_cell_count: 0,
            pane_snapshots: vec![PanePlacementPaneSnapshot {
                pane_id,
                terminal_window: None,
                image_placement_snapshots: Vec::new(),
            }],
        },
        destination_tab_snapshot: None,
        client_snapshot: PanePlacementClientSnapshot {
            client_id: ClientId::new(),
            viewport_size,
            pane_area: None,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
        },
        pane_sizing: PanePlacementSizing {
            minimum_size: Size {
                column_count: 1,
                row_count: 1,
            },
            gap_cell_count: 0,
        },
    }
}

#[test]
fn valid_snapshot_passes_validation() {
    assert_eq!(build_test_snapshot().validate(), Ok(()));
}

#[test]
fn snapshot_rejects_a_missing_cross_tab_snapshot() {
    let mut snapshot = build_test_snapshot();
    snapshot.destination_tab_id = TabId::new();

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::TabIdentityMismatch)
    );
}

#[test]
fn snapshot_rejects_duplicate_pane_slots() {
    let mut snapshot = build_test_snapshot();
    snapshot
        .source_tab_snapshot
        .pane_slots
        .push(snapshot.source_tab_snapshot.pane_slots[0].clone());

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::PaneSnapshotMismatch)
    );
}

#[test]
fn snapshot_rejects_a_source_pane_outside_the_source_tab() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_pane_id = PaneId::new();

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::SourcePaneMissing)
    );
}

#[test]
fn snapshot_rejects_a_layout_tree_that_does_not_match_the_slots() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.layout_tree = LayoutNode::Pane(PaneId::new());

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::LayoutMismatch)
    );
}

#[test]
fn snapshot_rejects_missing_pane_content() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.pane_snapshots.clear();

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::PaneSnapshotMismatch)
    );
}

#[test]
fn snapshot_rejects_duplicate_pane_ids_across_tabs() {
    let mut snapshot = build_test_snapshot();
    let destination_tab_id = TabId::new();
    let mut destination_tab_snapshot = snapshot.source_tab_snapshot.clone();
    destination_tab_snapshot.tab_id = destination_tab_id;
    snapshot.destination_tab_id = destination_tab_id;
    snapshot.destination_tab_snapshot = Some(destination_tab_snapshot);

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::DuplicatePaneId)
    );
}

#[test]
fn snapshot_accepts_a_window_that_does_not_match_the_preview_slot() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window = Some(build_test_window(1, 1));

    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn snapshot_rejects_a_window_row_that_does_not_match_the_window_width() {
    let mut snapshot = build_test_snapshot();
    let mut terminal_window = build_test_window(1, 1);
    terminal_window.row_snapshots[0].cell_runs[0].repeat_count = 2;
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window = Some(terminal_window);

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::TerminalWindowMismatch)
    );
}

#[test]
fn snapshot_rejects_cells_over_the_preview_limit() {
    let mut snapshot = build_test_snapshot();
    let oversized_size = Size {
        column_count: u16::MAX,
        row_count: 5,
    };
    let oversized_rect = Rect::from_size_at_origin(oversized_size);
    snapshot.source_tab_snapshot.effective_cell_size = oversized_size;
    snapshot.source_tab_snapshot.pane_slots[0].outer_rect = oversized_rect;
    snapshot.source_tab_snapshot.pane_slots[0].content_rect = Some(oversized_rect);
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window =
        Some(build_test_window(u16::MAX, 5));

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::CellCountExceeded)
    );
}

#[test]
fn snapshot_rejects_duplicate_image_placement_ids() {
    let mut snapshot = build_test_snapshot();
    let image_placement = build_test_image_placement(1, 1);
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window =
        Some(build_test_window(80, 24));
    snapshot.source_tab_snapshot.pane_snapshots[0].image_placement_snapshots =
        vec![image_placement.clone(), image_placement];

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::DuplicateImagePlacementId)
    );
}

#[test]
fn snapshot_rejects_image_bytes_over_the_preview_limit() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window =
        Some(build_test_window(80, 24));
    snapshot.source_tab_snapshot.pane_snapshots[0].image_placement_snapshots = vec![
        build_test_image_placement(1, 1),
        build_test_image_placement(2, 2),
    ];

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::ImageByteCountExceeded)
    );
}

#[test]
fn snapshot_counts_shared_image_bytes_once() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window =
        Some(build_test_window(80, 24));
    snapshot.source_tab_snapshot.pane_snapshots[0].image_placement_snapshots = vec![
        build_test_image_placement(1, 1),
        build_test_image_placement(2, 1),
    ];

    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn snapshot_accepts_an_image_outside_the_preview_slot_inside_the_terminal_window() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window =
        Some(build_test_window(100, 30));
    let mut image_placement = build_test_image_placement(1, 1);
    image_placement.anchor_cell = (25, 90);
    snapshot.source_tab_snapshot.pane_snapshots[0].image_placement_snapshots =
        vec![image_placement];

    assert_eq!(snapshot.validate(), Ok(()));
}

#[test]
fn snapshot_rejects_an_image_outside_the_terminal_window() {
    let mut snapshot = build_test_snapshot();
    snapshot.source_tab_snapshot.pane_snapshots[0].terminal_window =
        Some(build_test_window(100, 30));
    let mut image_placement = build_test_image_placement(1, 1);
    image_placement.anchor_cell = (30, 90);
    snapshot.source_tab_snapshot.pane_snapshots[0].image_placement_snapshots =
        vec![image_placement];

    assert_eq!(
        snapshot.validate(),
        Err(PanePlacementSnapshotValidationError::ImagePlacementMismatch)
    );
}
