//! Tests for the render-snapshot DTOs: build a full snapshot from fixture
//! pieces, check exact field values, confirm it is `Send + Sync`, confirm
//! cloning shares the grid by reference (no cell copy), confirm equality, and
//! check the mouse-frame projection, the borrowed layout views, and the
//! selection row lookup.

use super::*;

use koshi_core::geometry::Point;
use koshi_terminal::grid::state::Cell;
use koshi_terminal::style::Style;

/// A 24-row × 80-column blank grid, shared for cheap cloning.
fn fixture_grid() -> Arc<Grid> {
    Arc::new(Grid::blank(24, 80, Style::default()))
}

/// A one-tab, one-terminal-pane, one-client snapshot built around `grid`.
fn fixture(grid: Arc<Grid>) -> RenderSnapshot {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();

    let slot = PaneSlot {
        pane_id,
        outer_rect: Rect {
            origin: Point { column: 0, row: 0 },
            cell_size: Size {
                column_count: 80,
                row_count: 24,
            },
        },
        content_rect: Some(Rect {
            origin: Point { column: 1, row: 1 },
            cell_size: Size {
                column_count: 78,
                row_count: 22,
            },
        }),
        pane_kind: PaneKind::Terminal,
        is_visible: true,
        is_suppressed: false,
        is_dead: false,
    };

    let active_tab = TabSnapshot {
        tab_id,
        tab_name: "shell".to_string(),
        pane_slots: vec![slot],
        effective_cell_size: Size {
            column_count: 80,
            row_count: 24,
        },
        stack_headers: Vec::new(),
        layout_mode: LayoutMode::Tiled,
        are_all_panes_suppressed: false,
        gap_cell_count: 0,
    };

    let session_snapshot = SessionSnapshot {
        session_id: SessionId::new(),
        session_name: "sess".to_string(),
        active_tab_snapshot: active_tab,
        tabs_metadata: vec![TabMeta {
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
            grid,
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
        scrollback_meta: ScrollbackMeta {
            is_truncated: false,
            retained_line_count: 0,
        },
    };

    let client = ClientSnapshot {
        client_id: ClientId::new(),
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
        session_snapshot,
        pane_snapshots: vec![pane_snapshot],
        client_snapshot: client,
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

#[test]
fn builds_from_fixture_with_exact_values() {
    let snap = fixture(fixture_grid());

    // Session.
    assert_eq!(snap.session_snapshot.session_name, "sess");
    assert_eq!(snap.session_snapshot.tabs_metadata.len(), 1);
    assert_eq!(snap.session_snapshot.tabs_metadata[0].tab_name, "shell");
    assert_eq!(snap.session_snapshot.tabs_metadata[0].tab_index, 0);
    assert!(snap.session_snapshot.tabs_metadata[0].is_active);

    // Active tab + its one solved slot.
    let tab_snapshot = &snap.session_snapshot.active_tab_snapshot;
    assert_eq!(tab_snapshot.tab_name, "shell");
    assert_eq!(tab_snapshot.layout_mode, LayoutMode::Tiled);
    assert_eq!(
        tab_snapshot.effective_cell_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert!(!tab_snapshot.are_all_panes_suppressed);
    assert!(tab_snapshot.stack_headers.is_empty());
    assert_eq!(tab_snapshot.pane_slots.len(), 1);

    let pane_slot = &tab_snapshot.pane_slots[0];
    assert_eq!(pane_slot.pane_kind, PaneKind::Terminal);
    assert_eq!(
        pane_slot.outer_rect,
        Rect {
            origin: Point { column: 0, row: 0 },
            cell_size: Size {
                column_count: 80,
                row_count: 24,
            },
        }
    );
    assert_eq!(
        pane_slot.content_rect,
        Some(Rect {
            origin: Point { column: 1, row: 1 },
            cell_size: Size {
                column_count: 78,
                row_count: 22,
            },
        })
    );
    assert!(pane_slot.is_visible);
    assert!(!pane_slot.is_suppressed);
    assert!(!pane_slot.is_dead);

    // Pane content, joined to the slot by id.
    assert_eq!(snap.pane_snapshots.len(), 1);
    let pane_snapshot = &snap.pane_snapshots[0];
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
        pane_snapshot.scrollback_meta,
        ScrollbackMeta {
            is_truncated: false,
            retained_line_count: 0,
        }
    );

    let grid_view = pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("terminal pane has a grid");
    assert_eq!(grid_view.view_row_offset, 0);
    assert_eq!(grid_view.grid.get_grid_dimensions(), (24, 80));

    // Client projection.
    assert_eq!(
        snap.client_snapshot.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(snap.client_snapshot.lock_mode, LockMode::Normal);
    assert_eq!(snap.client_snapshot.active_tab_id, tab_snapshot.tab_id);
    // Focus is identified by matching this id against each PaneSlot's pane_id.
    assert_eq!(
        snap.client_snapshot.focused_pane_id,
        Some(pane_snapshot.pane_id)
    );

    // Stock, plugin-free UI.
    assert_eq!(snap.plugin_ui_snapshot, PluginUiSnapshot::default());
    assert!(snap.plugin_ui_snapshot.statusline_segments.is_empty());
    assert!(snap.plugin_ui_snapshot.tabline_segments.is_empty());
    assert!(snap.plugin_ui_snapshot.notifications.is_empty());
    assert!(snap.plugin_ui_snapshot.overlays.is_empty());
}

#[test]
fn a_mouse_frame_keeps_every_field_a_mouse_event_is_answered_from() {
    let mut snap = fixture(fixture_grid());
    // Each field takes a value of its own, so a copy that reads the wrong
    // source field lands on the wrong value here.
    snap.pane_snapshots[0].view_top_row_index = 42;
    snap.pane_snapshots[0].mouse_tracking = MouseTracking::ButtonMotion;
    snap.pane_snapshots[0].is_alternate_scroll_enabled = true;
    snap.pane_snapshots[0].is_on_alternate_screen = false;
    snap.pane_snapshots[0].has_selection = true;
    let pane_id = snap.pane_snapshots[0].pane_id;
    let client_id = snap.client_snapshot.client_id;
    let tab_id = snap.session_snapshot.active_tab_snapshot.tab_id;

    let frame = MouseFrame::from(snap);

    assert_eq!(
        frame.mouse_panes,
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
    assert_eq!(frame.client_snapshot.client_id, client_id);
    assert_eq!(frame.session_snapshot.active_tab_snapshot.tab_id, tab_id);
    assert_eq!(
        frame.committed_regions,
        CommittedRegions::core(
            Size {
                column_count: 80,
                row_count: 24,
            },
            0,
        )
    );
}

#[test]
fn a_borrowed_mouse_frame_matches_the_owned_constructor() {
    let snapshot = fixture(fixture_grid());
    let committed = CommittedRegions::core(
        Size {
            column_count: 40,
            row_count: 10,
        },
        12,
    );

    let borrowed = MouseFrame::from_snapshot(&snapshot, committed.clone());
    let owned = MouseFrame::from_snapshot_with_regions(snapshot, committed);

    assert_eq!(borrowed, owned);
}

#[test]
fn a_mouse_frame_keeps_one_entry_per_pane_in_frame_order() {
    let mut snap = fixture(fixture_grid());
    let first_pane_snapshot = snap.pane_snapshots[0].clone();
    let mut second_pane_snapshot = first_pane_snapshot.clone();
    second_pane_snapshot.pane_id = PaneId::new();
    second_pane_snapshot.view_top_row_index = 7;
    let mut third_pane_snapshot = first_pane_snapshot.clone();
    third_pane_snapshot.pane_id = PaneId::new();
    third_pane_snapshot.view_top_row_index = 9;
    snap.pane_snapshots = vec![
        first_pane_snapshot.clone(),
        second_pane_snapshot.clone(),
        third_pane_snapshot.clone(),
    ];

    let frame = MouseFrame::from(snap);

    assert_eq!(
        frame
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
    let mut snap = fixture(fixture_grid());
    // The client sees more than the tab was solved for, so a builder reading
    // the tab's effective size instead of the client viewport lands elsewhere.
    snap.client_snapshot.viewport_size = Size {
        column_count: 100,
        row_count: 30,
    };
    assert_eq!(
        snap.session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );

    let frame = MouseFrame::from(snap);

    assert_eq!(
        frame.committed_regions,
        CommittedRegions::core(
            Size {
                column_count: 100,
                row_count: 30,
            },
            0
        )
    );
    assert_eq!(
        frame.committed_regions.viewport_size,
        Size {
            column_count: 100,
            row_count: 30,
        }
    );
}

#[test]
fn a_mouse_frame_from_a_paneless_snapshot_carries_no_pane_entries() {
    let mut snap = fixture(fixture_grid());
    snap.pane_snapshots.clear();

    let frame = MouseFrame::from(snap);

    assert_eq!(frame.mouse_panes, Vec::new());
    assert_eq!(
        frame.committed_regions,
        CommittedRegions::core(
            Size {
                column_count: 80,
                row_count: 24,
            },
            0,
        )
    );
}

#[test]
fn from_snapshot_with_regions_keeps_the_solve_it_is_given() {
    let snap = fixture(fixture_grid());
    // A viewport and revision that differ from the client's own, so a builder
    // that re-derived the solve instead of carrying it lands elsewhere.
    let committed = CommittedRegions::core(
        Size {
            column_count: 40,
            row_count: 10,
        },
        12,
    );

    let frame = MouseFrame::from_snapshot_with_regions(snap, committed.clone());

    assert_eq!(frame.committed_regions, committed);
    assert_eq!(
        frame.committed_regions.viewport_size,
        Size {
            column_count: 40,
            row_count: 10,
        }
    );
    assert_eq!(frame.committed_regions.region_input_revision, 12);
}

#[test]
fn committed_regions_carries_the_exact_solve_it_was_built_from() {
    let solved_regions = solve_core_regions(Size {
        column_count: 80,
        row_count: 24,
    });
    let committed = CommittedRegions::from_solved_regions(
        Size {
            column_count: 80,
            row_count: 24,
        },
        solved_regions.clone(),
        5,
    );

    assert_eq!(
        committed.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(committed.solved_regions, solved_regions);
    assert_eq!(committed.region_input_revision, 5);
    assert_eq!(
        committed,
        CommittedRegions::core(
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
            tabline_offset: None,
            reconnecting: None,
        }
    );
}

#[test]
fn a_snapshot_layout_borrows_the_frame_and_holds_no_committed_regions() {
    let snap = fixture(fixture_grid());
    let viewer = ViewerChrome {
        hovered_pane_id: Some(snap.pane_snapshots[0].pane_id),
        tabline_offset: Some(3),
        reconnecting: Some(Reconnecting {
            attempt: 4,
            retry_in_seconds: 8,
        }),
    };

    let layout = snap.build_frame_layout(viewer);

    assert_eq!(*layout.session_snapshot, snap.session_snapshot);
    assert_eq!(*layout.client_snapshot, snap.client_snapshot);
    assert_eq!(layout.viewer_chrome, viewer);
    assert_eq!(layout.committed_regions, None);
}

#[test]
fn an_owned_frame_layout_borrows_its_session_and_client() {
    let snap = fixture(fixture_grid());
    let owned = OwnedFrameLayout {
        session_snapshot: snap.session_snapshot.clone(),
        client_snapshot: snap.client_snapshot.clone(),
    };

    let layout = owned.build_frame_layout(ViewerChrome::default());

    assert_eq!(*layout.session_snapshot, snap.session_snapshot);
    assert_eq!(*layout.client_snapshot, snap.client_snapshot);
    assert_eq!(layout.viewer_chrome, ViewerChrome::default());
    assert_eq!(layout.committed_regions, None);
}

#[test]
fn a_mouse_frame_layout_carries_the_regions_that_were_painted() {
    let snap = fixture(fixture_grid());
    let committed = CommittedRegions::core(
        Size {
            column_count: 80,
            row_count: 24,
        },
        3,
    );
    let frame = MouseFrame::from_snapshot_with_regions(snap, committed.clone());

    let layout = frame.build_frame_layout(ViewerChrome::default());

    assert_eq!(layout.committed_regions, Some(&committed));
}

#[test]
fn the_tabline_view_takes_its_fields_from_the_session_client_and_viewer() {
    let mut snap = fixture(fixture_grid());
    snap.client_snapshot.lock_mode = LockMode::Locked;
    snap.client_snapshot.is_mouse_selection_enabled = true;
    let reconnecting = Reconnecting {
        attempt: 4,
        retry_in_seconds: 8,
    };
    let viewer = ViewerChrome {
        hovered_pane_id: None,
        tabline_offset: Some(2),
        reconnecting: Some(reconnecting),
    };

    let layout = snap.build_frame_layout(viewer);
    let tabline = layout.get_tabline_inputs();

    assert_eq!(tabline.session_name, "sess");
    assert_eq!(
        tabline.tabs_metadata,
        snap.session_snapshot.tabs_metadata.as_slice()
    );
    assert_eq!(tabline.lock_mode, LockMode::Locked);
    assert!(tabline.is_mouse_selection_enabled);
    assert_eq!(tabline.reconnecting, Some(reconnecting));
    assert_eq!(tabline.tabline_offset, Some(2));
}

#[test]
fn row_span_returns_the_inclusive_columns_of_a_highlighted_row() {
    let spans = SelectionSpans {
        row_spans: vec![(4, 12, 79), (5, 0, 79), (6, 0, 33)],
    };

    assert_eq!(spans.find_row_span(4), Some((12, 79)));
    assert_eq!(spans.find_row_span(5), Some((0, 79)));
    assert_eq!(spans.find_row_span(6), Some((0, 33)));
}

#[test]
fn row_span_is_none_for_a_row_the_highlight_does_not_touch() {
    let spans = SelectionSpans {
        row_spans: vec![(4, 12, 79), (6, 0, 33)],
    };

    assert_eq!(spans.find_row_span(3), None);
    assert_eq!(spans.find_row_span(5), None);
    assert_eq!(spans.find_row_span(7), None);
    assert_eq!(spans.find_row_span(u16::MAX), None);
}

#[test]
fn row_span_of_an_empty_highlight_is_none() {
    let spans = SelectionSpans {
        row_spans: Vec::new(),
    };

    assert_eq!(spans.find_row_span(0), None);
}

#[test]
fn row_span_answers_a_single_cell_highlight_with_that_one_column() {
    let spans = SelectionSpans {
        row_spans: vec![(0, 7, 7)],
    };

    assert_eq!(spans.find_row_span(0), Some((7, 7)));
}

#[test]
fn row_span_takes_the_first_entry_when_a_row_is_listed_twice() {
    let spans = SelectionSpans {
        row_spans: vec![(2, 0, 5), (2, 10, 20)],
    };

    assert_eq!(spans.find_row_span(2), Some((0, 5)));
}

#[test]
fn snapshot_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RenderSnapshot>();
}

#[test]
fn cloning_shares_the_grid_by_reference() {
    let grid = fixture_grid();
    assert_eq!(Arc::strong_count(&grid), 1);

    // The snapshot holds one shared reference to the grid.
    let snap = fixture(grid.clone());
    assert_eq!(Arc::strong_count(&grid), 2);

    // Cloning the snapshot bumps the refcount rather than copying the cells.
    let clone = snap.clone();
    assert_eq!(Arc::strong_count(&grid), 3);

    let original = snap.pane_snapshots[0].terminal_grid_view.as_ref().unwrap();
    let cloned = clone.pane_snapshots[0].terminal_grid_view.as_ref().unwrap();
    assert!(Arc::ptr_eq(&original.grid, &cloned.grid));
}

#[test]
fn clone_equals_original() {
    let snap = fixture(fixture_grid());
    assert_eq!(snap, snap.clone());
}

#[test]
fn snapshots_differing_by_one_grid_cell_are_not_equal() {
    // Derived `PartialEq` recurses into the grid; a single-cell difference
    // deep inside it must still make the two snapshots unequal, not just a
    // difference at the top-level fields.
    let grid_a = Grid::blank(24, 80, Style::default());
    let snap_a = fixture(Arc::new(grid_a));

    let mut grid_b = Grid::blank(24, 80, Style::default());
    *grid_b.get_cell_mut(0, 0).unwrap() = Cell::from_character('x', 1, Style::default());
    let snap_b = fixture(Arc::new(grid_b));

    assert_ne!(snap_a, snap_b);
}

#[test]
fn snapshot_with_no_panes_or_tabs_is_valid_and_equals_its_clone() {
    // An empty snapshot (no panes, no layout slots, no tabs, no focus) must
    // still construct and compare without panicking — the degenerate state
    // right after a session's last pane closes.
    let mut snap = fixture(fixture_grid());
    snap.pane_snapshots.clear();
    snap.session_snapshot.active_tab_snapshot.pane_slots.clear();
    snap.session_snapshot.tabs_metadata.clear();
    snap.client_snapshot.focused_pane_id = None;

    assert!(snap.pane_snapshots.is_empty());
    assert!(snap
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .is_empty());
    assert_eq!(snap, snap.clone());
}
