//! Tests for the compiled-in region solve, which keeps one rectangle per
//! region down to a zero-size viewport, and for the tabline inputs a frame
//! borrows from its snapshot.

use super::*;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, Reconnecting, RenderSnapshot, SessionSnapshot, TabMetadata,
    TabSnapshot, ViewerChrome,
};

/// A frame of one session named `one`, holding one tab named `first` with no
/// panes in it.
fn build_region_test_render_snapshot() -> RenderSnapshot {
    let tab_id = TabId::new();

    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: "one".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "first".to_string(),
                pane_slots: Vec::new(),
                tab_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: "first".to_string(),
                tab_index: 0,
                is_active: true,
            }],
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id: tab_id,
            focused_pane_id: None,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
    }
}

#[test]
fn core_regions_commit_exact_chrome_rectangles_and_revision() {
    let committed_regions = CommittedRegions::build_core(
        Size {
            column_count: 80,
            row_count: 24,
        },
        7,
    );

    assert_eq!(
        committed_regions.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(committed_regions.region_input_revision, 7);
    assert_eq!(
        committed_regions.solved_regions.region_rects,
        vec![
            Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            Rect::from_origin_and_size(
                Point { column: 0, row: 23 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
        ]
    );
    assert_eq!(
        committed_regions.solved_regions.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: 80,
                row_count: 22,
            },
        )
    );
}

#[test]
fn solve_core_regions_keeps_a_rectangle_per_region_on_short_viewports() {
    // Two rows: the tab row takes the first, the hint row the second, and no row
    // is left for panes.
    let two_row_viewport_solve = solve_core_regions(Size {
        column_count: 80,
        row_count: 2,
    });
    assert_eq!(
        two_row_viewport_solve.region_rects,
        vec![
            Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            Rect::from_origin_and_size(
                Point { column: 0, row: 1 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
        ]
    );
    assert_eq!(
        two_row_viewport_solve.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: 80,
                row_count: 0,
            },
        )
    );

    // One row: the tab row takes it and the hint row keeps a zero-height
    // rectangle at the row after it.
    let one_row_viewport_solve = solve_core_regions(Size {
        column_count: 80,
        row_count: 1,
    });
    assert_eq!(
        one_row_viewport_solve.region_rects,
        vec![
            Rect::from_origin_and_size(
                Point { column: 0, row: 0 },
                Size {
                    column_count: 80,
                    row_count: 1,
                },
            ),
            Rect::from_origin_and_size(
                Point { column: 0, row: 1 },
                Size {
                    column_count: 80,
                    row_count: 0,
                },
            ),
        ]
    );
    assert_eq!(
        one_row_viewport_solve.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: 80,
                row_count: 0,
            },
        )
    );

    // A zero-size viewport: both rectangles are empty at the origin.
    let zero_size_viewport_solve = solve_core_regions(Size {
        column_count: 0,
        row_count: 0,
    });
    assert_eq!(
        zero_size_viewport_solve.region_rects,
        vec![Rect::build_empty_at_origin(), Rect::build_empty_at_origin()]
    );
    assert_eq!(
        zero_size_viewport_solve.pane_rect,
        Rect::build_empty_at_origin()
    );
}

#[test]
fn tabline_inputs_borrow_the_session_and_copy_the_client_and_viewer_state() {
    let mut render_snapshot = build_region_test_render_snapshot();
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    render_snapshot.client_snapshot.is_mouse_selection_enabled = true;
    let viewer_chrome = ViewerChrome {
        reconnecting: Some(Reconnecting {
            attempt: 3,
            retry_in_seconds: 8,
        }),
        active_input_mode: None,
        tabline_offset: Some(2),
        ..ViewerChrome::default()
    };

    let frame_layout = render_snapshot.build_frame_layout(viewer_chrome);
    let tabline_inputs = frame_layout.get_tabline_inputs();

    assert_eq!(
        tabline_inputs,
        TablineInputs {
            session_name: "one",
            tabs_metadata: &render_snapshot.session_snapshot.tabs_metadata,
            lock_mode: LockMode::Locked,
            is_mouse_selection_enabled: true,
            reconnecting: Some(Reconnecting {
                attempt: 3,
                retry_in_seconds: 8,
            }),
            tabline_offset: Some(2),
        }
    );
    assert!(
        std::ptr::eq(
            tabline_inputs.session_name,
            render_snapshot.session_snapshot.session_name.as_str(),
        ),
        "the session name is borrowed from the snapshot"
    );
    assert!(
        std::ptr::eq(
            tabline_inputs.tabs_metadata,
            render_snapshot.session_snapshot.tabs_metadata.as_slice(),
        ),
        "the tab metadata is borrowed from the snapshot"
    );
}
