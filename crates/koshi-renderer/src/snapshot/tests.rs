//! Tests for the render-snapshot DTOs: build a full snapshot from fixture
//! pieces, check exact field values, confirm it is `Send + Sync`, confirm
//! cloning shares the grid by reference (no cell copy), confirm equality, and
//! check the mouse-frame projection, the borrowed layout views, and the
//! selection row lookup.

use super::*;

/// The mouse frame for `render_snapshot` under the compiled-in region solve for
/// its client's viewport, at input revision `0`.
fn build_mouse_frame(render_snapshot: RenderSnapshot) -> MouseFrame {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    MouseFrame::from_snapshot(&render_snapshot, committed_regions)
}

use koshi_core::geometry::Point;
use koshi_terminal::grid::state::Cell;
use koshi_terminal::style::Style;

/// A 24-row × 80-column blank grid, shared for cheap cloning.
fn build_render_snapshot_fixture_grid() -> Arc<Grid> {
    Arc::new(Grid::build_blank(24, 80, Style::default()))
}

/// A one-tab, one-terminal-pane, one-client snapshot built around `terminal_grid`.
fn build_render_snapshot_fixture(terminal_grid: Arc<Grid>) -> RenderSnapshot {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let pane_slot = PaneSlot {
        pane_id,
        outer_rect: Rect {
            origin: Point { column: 0, row: 0 },
            size: Size {
                column_count: 80,
                row_count: 24,
            },
        },
        content_rect: Some(Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 78,
                row_count: 22,
            },
        }),
        is_visible: true,
        is_suppressed: false,
    };

    let active_tab_snapshot = TabSnapshot {
        tab_id,
        tab_name: "shell".to_string(),
        pane_slots: vec![pane_slot],
        tab_size: Size {
            column_count: 80,
            row_count: 24,
        },
        stack_headers: Vec::new(),
        layout_mode: LayoutMode::Tiled,
        is_every_pane_suppressed: false,
        gap_cell_count: 0,
    };

    let session_snapshot = SessionSnapshot {
        session_id: SessionId::new(),
        session_revision: 0,
        session_name: "sess".to_string(),
        active_tab_snapshot,
        tabs_metadata: vec![TabMetadata {
            tab_id,
            tab_name: "shell".to_string(),
            tab_index: 0,
            is_active: true,
        }],
    };

    let pane_snapshot = PaneSnapshot {
        pane_id,
        pane_title: Some("bash".to_string()),
        cursor_snapshot: CursorSnapshot {
            row_index: 0,
            column_index: 5,
            is_visible: true,
            is_blinking: false,
            shape: None,
        },
        terminal_grid_view: Some(GridView {
            grid: terminal_grid,
            view_row_offset: 0,
        }),
        image_placement_snapshots: Vec::new(),
        is_reverse_video: false,
        mouse_tracking: MouseTracking::Off,
        is_alternate_scroll_enabled: false,
        is_on_alternate_screen: false,
        selection_spans: None,
        has_selection: false,
        view_top_row_index: 0,
        scrollback_metadata: ScrollbackMetadata {
            retained_line_count: 0,
        },
    };

    let client_snapshot = ClientSnapshot {
        client_id: ClientId::new(),
        client_revision: 0,
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        active_tab_id: tab_id,
        focused_pane_id: Some(pane_id),
        lock_mode: LockMode::Normal,
        is_mouse_selection_enabled: false,
    };

    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot,
        pane_snapshots: vec![pane_snapshot],
        client_snapshot,
    }
}

#[test]
fn render_snapshot_fixture_has_exact_field_values() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());

    // Session.
    assert_eq!(render_snapshot.session_snapshot.session_name, "sess");
    assert_eq!(render_snapshot.session_snapshot.tabs_metadata.len(), 1);
    assert_eq!(
        render_snapshot.session_snapshot.tabs_metadata[0].tab_name,
        "shell"
    );
    assert_eq!(
        render_snapshot.session_snapshot.tabs_metadata[0].tab_index,
        0
    );
    assert!(render_snapshot.session_snapshot.tabs_metadata[0].is_active);

    // Active tab + its one solved slot.
    let tab_snapshot = &render_snapshot.session_snapshot.active_tab_snapshot;
    assert_eq!(tab_snapshot.tab_name, "shell");
    assert_eq!(tab_snapshot.layout_mode, LayoutMode::Tiled);
    assert_eq!(
        tab_snapshot.tab_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert!(!tab_snapshot.is_every_pane_suppressed);
    assert!(tab_snapshot.stack_headers.is_empty());
    assert_eq!(tab_snapshot.pane_slots.len(), 1);

    let pane_slot = &tab_snapshot.pane_slots[0];
    assert_eq!(
        pane_slot.outer_rect,
        Rect {
            origin: Point { column: 0, row: 0 },
            size: Size {
                column_count: 80,
                row_count: 24,
            },
        }
    );
    assert_eq!(
        pane_slot.content_rect,
        Some(Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 78,
                row_count: 22,
            },
        })
    );
    assert!(pane_slot.is_visible);
    assert!(!pane_slot.is_suppressed);

    // Pane content, joined to the slot by id.
    assert_eq!(render_snapshot.pane_snapshots.len(), 1);
    let pane_snapshot = &render_snapshot.pane_snapshots[0];
    assert_eq!(pane_snapshot.pane_id, pane_slot.pane_id);
    assert_eq!(pane_snapshot.pane_title.as_deref(), Some("bash"));
    assert_eq!(
        pane_snapshot.cursor_snapshot,
        CursorSnapshot {
            row_index: 0,
            column_index: 5,
            is_visible: true,
            is_blinking: false,
            shape: None,
        }
    );
    assert!(!pane_snapshot.is_reverse_video);
    assert_eq!(
        pane_snapshot.scrollback_metadata,
        ScrollbackMetadata {
            retained_line_count: 0,
        }
    );

    let terminal_grid_view = pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("terminal pane has a grid");
    assert_eq!(terminal_grid_view.view_row_offset, 0);
    assert_eq!(terminal_grid_view.grid.get_grid_dimensions(), (24, 80));

    // Client projection.
    assert_eq!(
        render_snapshot.client_snapshot.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(render_snapshot.client_snapshot.lock_mode, LockMode::Normal);
    assert_eq!(
        render_snapshot.client_snapshot.active_tab_id,
        tab_snapshot.tab_id
    );
    // Focus is identified by matching this id against each PaneSlot's pane_id.
    assert_eq!(
        render_snapshot.client_snapshot.focused_pane_id,
        Some(pane_snapshot.pane_id)
    );
}

#[test]
fn a_mouse_frame_keeps_every_field_a_mouse_event_is_answered_from() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    // Each field takes a value of its own, so a copy that reads the wrong
    // source field lands on the wrong value here.
    render_snapshot.pane_snapshots[0].view_top_row_index = 42;
    render_snapshot.pane_snapshots[0].mouse_tracking = MouseTracking::ButtonMotion;
    render_snapshot.pane_snapshots[0].is_alternate_scroll_enabled = true;
    render_snapshot.pane_snapshots[0].is_on_alternate_screen = false;
    render_snapshot.pane_snapshots[0].has_selection = true;
    let pane_id = render_snapshot.pane_snapshots[0].pane_id;
    let client_id = render_snapshot.client_snapshot.client_id;
    let tab_id = render_snapshot.session_snapshot.active_tab_snapshot.tab_id;

    let mouse_frame = build_mouse_frame(render_snapshot);

    assert_eq!(
        mouse_frame.mouse_panes,
        vec![MousePane {
            pane_id,
            view_top_row_index: 42,
            mouse_tracking: MouseTracking::ButtonMotion,
            is_alternate_scroll_enabled: true,
            is_on_alternate_screen: false,
            has_selection: true,
        }]
    );
    // The session and client parts move across whole.
    assert_eq!(mouse_frame.client_snapshot.client_id, client_id);
    assert_eq!(
        mouse_frame.session_snapshot.active_tab_snapshot.tab_id,
        tab_id
    );
    assert_eq!(
        mouse_frame.committed_regions,
        CommittedRegions::build_core(
            Size {
                column_count: 80,
                row_count: 24,
            },
            0,
        )
    );
}

#[test]
fn a_mouse_frame_keeps_one_entry_per_pane_in_frame_order() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let first_pane_snapshot = render_snapshot.pane_snapshots[0].clone();
    let mut second_pane_snapshot = first_pane_snapshot.clone();
    second_pane_snapshot.pane_id = PaneId::new();
    second_pane_snapshot.view_top_row_index = 7;
    let mut third_pane_snapshot = first_pane_snapshot.clone();
    third_pane_snapshot.pane_id = PaneId::new();
    third_pane_snapshot.view_top_row_index = 9;
    render_snapshot.pane_snapshots = vec![
        first_pane_snapshot.clone(),
        second_pane_snapshot.clone(),
        third_pane_snapshot.clone(),
    ];

    let mouse_frame = build_mouse_frame(render_snapshot);

    assert_eq!(
        mouse_frame
            .mouse_panes
            .iter()
            .map(|mouse_pane| (mouse_pane.pane_id, mouse_pane.view_top_row_index))
            .collect::<Vec<_>>(),
        vec![
            (first_pane_snapshot.pane_id, 0),
            (second_pane_snapshot.pane_id, 7),
            (third_pane_snapshot.pane_id, 9),
        ]
    );
}

#[test]
fn a_mouse_frame_solves_its_regions_from_the_client_viewport() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    // The client sees more than the tab was solved for, so a builder reading
    // the tab size instead of the client viewport lands elsewhere.
    render_snapshot.client_snapshot.viewport_size = Size {
        column_count: 100,
        row_count: 30,
    };
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );

    let mouse_frame = build_mouse_frame(render_snapshot);

    assert_eq!(
        mouse_frame.committed_regions,
        CommittedRegions::build_core(
            Size {
                column_count: 100,
                row_count: 30,
            },
            0
        )
    );
    assert_eq!(
        mouse_frame.committed_regions.viewport_size,
        Size {
            column_count: 100,
            row_count: 30,
        }
    );
}

#[test]
fn a_mouse_frame_from_a_render_snapshot_without_panes_carries_no_mouse_panes() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    render_snapshot.pane_snapshots.clear();

    let mouse_frame = build_mouse_frame(render_snapshot);

    assert_eq!(mouse_frame.mouse_panes, Vec::new());
    assert_eq!(
        mouse_frame.committed_regions,
        CommittedRegions::build_core(
            Size {
                column_count: 80,
                row_count: 24,
            },
            0,
        )
    );
}

#[test]
fn mouse_frame_from_snapshot_keeps_the_supplied_region_solve() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    // A viewport and revision that differ from the client's own, so a builder
    // that re-derived the solve instead of carrying it lands elsewhere.
    let committed_regions = CommittedRegions::build_core(
        Size {
            column_count: 40,
            row_count: 10,
        },
        12,
    );

    let mouse_frame = MouseFrame::from_snapshot(&render_snapshot, committed_regions.clone());

    assert_eq!(mouse_frame.committed_regions, committed_regions);
    assert_eq!(
        mouse_frame.committed_regions.viewport_size,
        Size {
            column_count: 40,
            row_count: 10,
        }
    );
    assert_eq!(mouse_frame.committed_regions.region_input_revision, 12);
}

#[test]
fn committed_regions_carries_the_exact_solve_it_was_built_from() {
    let solved_regions = solve_core_regions(Size {
        column_count: 80,
        row_count: 24,
    });
    let committed_regions = CommittedRegions::from_solved_regions(
        Size {
            column_count: 80,
            row_count: 24,
        },
        solved_regions.clone(),
        5,
    );

    assert_eq!(
        committed_regions.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(committed_regions.solved_regions, solved_regions);
    assert_eq!(committed_regions.region_input_revision, 5);
    assert_eq!(
        committed_regions,
        CommittedRegions::build_core(
            Size {
                column_count: 80,
                row_count: 24,
            },
            5,
        )
    );
}

#[test]
fn viewer_chrome_defaults_to_no_pointer_no_tab_offset_and_no_reconnect() {
    assert_eq!(
        ViewerChrome::default(),
        ViewerChrome {
            hovered_pane_id: None,
            placement_handle_pane_id: None,
            active_input_mode: None,
            tabline_offset: None,
            reconnecting: None,
            is_pane_placement_visible: false,
            placement_source_pane_id: None,
        }
    );
}

#[test]
fn render_snapshot_frame_layout_borrows_the_frame_without_committed_regions() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let viewer_chrome = ViewerChrome {
        hovered_pane_id: Some(render_snapshot.pane_snapshots[0].pane_id),
        placement_handle_pane_id: None,
        active_input_mode: None,
        tabline_offset: Some(3),
        reconnecting: Some(Reconnecting {
            attempt: 4,
            retry_in_seconds: 8,
        }),
        ..ViewerChrome::default()
    };

    let frame_layout = render_snapshot.build_frame_layout(viewer_chrome);

    assert_eq!(
        *frame_layout.session_snapshot,
        render_snapshot.session_snapshot
    );
    assert_eq!(
        *frame_layout.client_snapshot,
        render_snapshot.client_snapshot
    );
    assert_eq!(frame_layout.viewer_chrome, viewer_chrome);
    assert_eq!(frame_layout.committed_regions, None);
}

#[test]
fn an_active_viewer_mode_replaces_the_frames_base_mode_in_tabline_inputs() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    let viewer_chrome = ViewerChrome {
        active_input_mode: Some(LockMode::PanePlacement),
        ..ViewerChrome::default()
    };

    assert_eq!(
        render_snapshot
            .build_frame_layout(viewer_chrome)
            .get_tabline_inputs()
            .lock_mode,
        LockMode::PanePlacement
    );
}

#[test]
fn an_owned_frame_layout_borrows_its_session_and_client() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let owned_frame_layout = OwnedFrameLayout {
        session_snapshot: render_snapshot.session_snapshot.clone(),
        client_snapshot: render_snapshot.client_snapshot.clone(),
    };

    let frame_layout = owned_frame_layout.build_frame_layout(ViewerChrome::default());

    assert_eq!(
        *frame_layout.session_snapshot,
        render_snapshot.session_snapshot
    );
    assert_eq!(
        *frame_layout.client_snapshot,
        render_snapshot.client_snapshot
    );
    assert_eq!(frame_layout.viewer_chrome, ViewerChrome::default());
    assert_eq!(frame_layout.committed_regions, None);
}

#[test]
fn a_mouse_frame_layout_carries_the_regions_that_were_painted() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let committed_regions = CommittedRegions::build_core(
        Size {
            column_count: 80,
            row_count: 24,
        },
        3,
    );
    let mouse_frame = MouseFrame::from_snapshot(&render_snapshot, committed_regions.clone());

    let frame_layout = mouse_frame.build_frame_layout(ViewerChrome::default());

    assert_eq!(frame_layout.committed_regions, Some(&committed_regions));
}

#[test]
fn tabline_inputs_borrow_session_client_and_viewer_fields() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    render_snapshot.client_snapshot.is_mouse_selection_enabled = true;
    let reconnecting = Reconnecting {
        attempt: 4,
        retry_in_seconds: 8,
    };
    let viewer_chrome = ViewerChrome {
        hovered_pane_id: None,
        placement_handle_pane_id: None,
        active_input_mode: None,
        tabline_offset: Some(2),
        reconnecting: Some(reconnecting),
        ..ViewerChrome::default()
    };

    let frame_layout = render_snapshot.build_frame_layout(viewer_chrome);
    let tabline_inputs = frame_layout.get_tabline_inputs();

    assert_eq!(tabline_inputs.session_name, "sess");
    assert_eq!(
        tabline_inputs.tabs_metadata,
        render_snapshot.session_snapshot.tabs_metadata.as_slice()
    );
    assert_eq!(tabline_inputs.lock_mode, LockMode::Locked);
    assert!(tabline_inputs.is_mouse_selection_enabled);
    assert_eq!(tabline_inputs.reconnecting, Some(reconnecting));
    assert_eq!(tabline_inputs.tabline_offset, Some(2));
}

#[test]
fn selection_row_span_returns_inclusive_columns_of_a_highlighted_row() {
    let selection_spans = SelectionSpans {
        row_spans: vec![(4, 12, 79), (5, 0, 79), (6, 0, 33)],
    };

    assert_eq!(selection_spans.find_row_span(4), Some((12, 79)));
    assert_eq!(selection_spans.find_row_span(5), Some((0, 79)));
    assert_eq!(selection_spans.find_row_span(6), Some((0, 33)));
}

#[test]
fn selection_row_span_is_none_when_the_highlight_does_not_touch_the_row() {
    let selection_spans = SelectionSpans {
        row_spans: vec![(4, 12, 79), (6, 0, 33)],
    };

    assert_eq!(selection_spans.find_row_span(3), None);
    assert_eq!(selection_spans.find_row_span(5), None);
    assert_eq!(selection_spans.find_row_span(7), None);
    assert_eq!(selection_spans.find_row_span(u16::MAX), None);
}

#[test]
fn empty_selection_spans_have_no_row_span() {
    let selection_spans = SelectionSpans {
        row_spans: Vec::new(),
    };

    assert_eq!(selection_spans.find_row_span(0), None);
}

#[test]
fn selection_row_span_returns_the_single_highlighted_column() {
    let selection_spans = SelectionSpans {
        row_spans: vec![(0, 7, 7)],
    };

    assert_eq!(selection_spans.find_row_span(0), Some((7, 7)));
}

#[test]
fn selection_row_span_uses_the_first_duplicate_row_entry() {
    let selection_spans = SelectionSpans {
        row_spans: vec![(2, 0, 5), (2, 10, 20)],
    };

    assert_eq!(selection_spans.find_row_span(2), Some((0, 5)));
}

#[test]
fn render_snapshot_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RenderSnapshot>();
}

#[test]
fn cloning_render_snapshot_shares_the_grid_by_reference() {
    let terminal_grid = build_render_snapshot_fixture_grid();
    assert_eq!(Arc::strong_count(&terminal_grid), 1);

    // The snapshot holds one shared reference to the grid.
    let render_snapshot = build_render_snapshot_fixture(terminal_grid.clone());
    assert_eq!(Arc::strong_count(&terminal_grid), 2);

    // Cloning the snapshot bumps the refcount rather than copying the cells.
    let cloned_render_snapshot = render_snapshot.clone();
    assert_eq!(Arc::strong_count(&terminal_grid), 3);

    let original_grid_view = render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .unwrap();
    let cloned_grid_view = cloned_render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .unwrap();
    assert!(Arc::ptr_eq(
        &original_grid_view.grid,
        &cloned_grid_view.grid
    ));
}

#[test]
fn cloning_render_snapshot_preserves_equality() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    assert_eq!(render_snapshot, render_snapshot.clone());
}

#[test]
fn render_snapshots_with_different_grid_cells_are_not_equal() {
    // Derived `PartialEq` recurses into the grid; a single-cell difference
    // deep inside it must still make the two snapshots unequal, not just a
    // difference at the top-level fields.
    let first_terminal_grid = Grid::build_blank(24, 80, Style::default());
    let first_render_snapshot = build_render_snapshot_fixture(Arc::new(first_terminal_grid));

    let mut second_terminal_grid = Grid::build_blank(24, 80, Style::default());
    *second_terminal_grid.get_cell_mut(0, 0).unwrap() =
        Cell::from_character('x', 1, Style::default());
    let second_render_snapshot = build_render_snapshot_fixture(Arc::new(second_terminal_grid));

    assert_ne!(first_render_snapshot, second_render_snapshot);
}

#[test]
fn empty_render_snapshot_is_valid_and_equals_its_clone() {
    // An empty snapshot (no panes, no layout slots, no tabs, no focus) must
    // still construct and compare without panicking — the degenerate state
    // right after a session's last pane closes.
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    render_snapshot.pane_snapshots.clear();
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .clear();
    render_snapshot.session_snapshot.tabs_metadata.clear();
    render_snapshot.client_snapshot.focused_pane_id = None;

    assert!(render_snapshot.pane_snapshots.is_empty());
    assert!(render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .is_empty());
    assert_eq!(render_snapshot, render_snapshot.clone());
}
