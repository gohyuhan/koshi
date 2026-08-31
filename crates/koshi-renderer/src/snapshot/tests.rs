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
        rect: Rect {
            origin: Point { x: 0, y: 0 },
            size: Size { cols: 80, rows: 24 },
        },
        inner_rect: Some(Rect {
            origin: Point { x: 1, y: 1 },
            size: Size { cols: 78, rows: 22 },
        }),
        kind: PaneKind::Terminal,
        visible: true,
        suppressed: false,
        dead: false,
    };

    let active_tab = TabSnapshot {
        id: tab_id,
        name: "shell".to_string(),
        layout_solved: vec![slot],
        effective_size: Size { cols: 80, rows: 24 },
        stack_headers: Vec::new(),
        layout_mode: LayoutMode::Tiled,
        all_suppressed: false,
        gap: 0,
    };

    let session = SessionSnapshot {
        id: SessionId::new(),
        name: "sess".to_string(),
        active_tab,
        tabs_metadata: vec![TabMeta {
            id: tab_id,
            name: "shell".to_string(),
            index: 0,
            active: true,
        }],
    };

    let pane = PaneSnapshot {
        id: pane_id,
        title: Some("bash".to_string()),
        cursor: CursorSnapshot {
            row: 0,
            col: 5,
            visible: true,
            blink: false,
            shape: None,
        },
        grid_view: Some(GridView {
            grid,
            view_offset: 0,
        }),
        reverse_video: false,
        mouse_tracking: MouseTracking::Off,
        alt_scroll: false,
        on_alt_screen: false,
        selection: None,
        has_selection: false,
        view_top_row: 0,
        scrollback: ScrollbackMeta {
            truncated: false,
            retained_lines: 0,
        },
    };

    let client = ClientSnapshot {
        id: ClientId::new(),
        viewport: Size { cols: 80, rows: 24 },
        active_tab: tab_id,
        focused_pane: Some(pane_id),
        lock_mode: LockMode::Normal,
        mouse_select: false,
    };

    RenderSnapshot {
        session,
        panes: vec![pane],
        client,
        plugin_ui: PluginUiSnapshot::default(),
    }
}

#[test]
fn builds_from_fixture_with_exact_values() {
    let snap = fixture(fixture_grid());

    // Session.
    assert_eq!(snap.session.name, "sess");
    assert_eq!(snap.session.tabs_metadata.len(), 1);
    assert_eq!(snap.session.tabs_metadata[0].name, "shell");
    assert_eq!(snap.session.tabs_metadata[0].index, 0);
    assert!(snap.session.tabs_metadata[0].active);

    // Active tab + its one solved slot.
    let tab = &snap.session.active_tab;
    assert_eq!(tab.name, "shell");
    assert_eq!(tab.layout_mode, LayoutMode::Tiled);
    assert_eq!(tab.effective_size, Size { cols: 80, rows: 24 });
    assert!(!tab.all_suppressed);
    assert!(tab.stack_headers.is_empty());
    assert_eq!(tab.layout_solved.len(), 1);

    let slot = &tab.layout_solved[0];
    assert_eq!(slot.kind, PaneKind::Terminal);
    assert_eq!(
        slot.rect,
        Rect {
            origin: Point { x: 0, y: 0 },
            size: Size { cols: 80, rows: 24 },
        }
    );
    assert_eq!(
        slot.inner_rect,
        Some(Rect {
            origin: Point { x: 1, y: 1 },
            size: Size { cols: 78, rows: 22 },
        })
    );
    assert!(slot.visible);
    assert!(!slot.suppressed);
    assert!(!slot.dead);

    // Pane content, joined to the slot by id.
    assert_eq!(snap.panes.len(), 1);
    let pane = &snap.panes[0];
    assert_eq!(pane.id, slot.pane_id);
    assert_eq!(pane.title.as_deref(), Some("bash"));
    assert_eq!(
        pane.cursor,
        CursorSnapshot {
            row: 0,
            col: 5,
            visible: true,
            blink: false,
            shape: None,
        }
    );
    assert!(!pane.reverse_video);
    assert_eq!(
        pane.scrollback,
        ScrollbackMeta {
            truncated: false,
            retained_lines: 0,
        }
    );

    let grid_view = pane.grid_view.as_ref().expect("terminal pane has a grid");
    assert_eq!(grid_view.view_offset, 0);
    assert_eq!(grid_view.grid.dimensions(), (24, 80));

    // Client projection.
    assert_eq!(snap.client.viewport, Size { cols: 80, rows: 24 });
    assert_eq!(snap.client.lock_mode, LockMode::Normal);
    assert_eq!(snap.client.active_tab, tab.id);
    // Focus is identified by matching this id against each PaneSlot's pane_id.
    assert_eq!(snap.client.focused_pane, Some(pane.id));

    // Stock, plugin-free UI.
    assert_eq!(snap.plugin_ui, PluginUiSnapshot::default());
    assert!(snap.plugin_ui.statusline_segments.is_empty());
    assert!(snap.plugin_ui.tabline_segments.is_empty());
    assert!(snap.plugin_ui.notifications.is_empty());
    assert!(snap.plugin_ui.overlays.is_empty());
}

#[test]
fn a_mouse_frame_keeps_every_field_a_mouse_event_is_answered_from() {
    let mut snap = fixture(fixture_grid());
    // Each field takes a value of its own, so a copy that reads the wrong
    // source field lands on the wrong value here.
    snap.panes[0].view_top_row = 42;
    snap.panes[0].mouse_tracking = MouseTracking::ButtonMotion;
    snap.panes[0].alt_scroll = true;
    snap.panes[0].on_alt_screen = false;
    snap.panes[0].has_selection = true;
    let pane_id = snap.panes[0].id;
    let client_id = snap.client.id;
    let tab_id = snap.session.active_tab.id;

    let frame = MouseFrame::from(snap);

    assert_eq!(
        frame.panes,
        vec![MousePane {
            id: pane_id,
            view_top_row: 42,
            mouse_tracking: MouseTracking::ButtonMotion,
            alt_scroll: true,
            on_alt_screen: false,
            has_selection: true,
        }]
    );
    // The session and client parts move across whole.
    assert_eq!(frame.client.id, client_id);
    assert_eq!(frame.session.active_tab.id, tab_id);
    assert_eq!(
        frame.committed_regions,
        CommittedRegions::core(Size { cols: 80, rows: 24 }, 0)
    );
}

#[test]
fn a_borrowed_mouse_frame_matches_the_owned_constructor() {
    let snapshot = fixture(fixture_grid());
    let committed = CommittedRegions::core(Size { cols: 40, rows: 10 }, 12);

    let borrowed = MouseFrame::from_snapshot(&snapshot, committed.clone());
    let owned = MouseFrame::with_regions(snapshot, committed);

    assert_eq!(borrowed, owned);
}

#[test]
fn a_mouse_frame_keeps_one_entry_per_pane_in_frame_order() {
    let mut snap = fixture(fixture_grid());
    let first = snap.panes[0].clone();
    let mut second = first.clone();
    second.id = PaneId::new();
    second.view_top_row = 7;
    let mut third = first.clone();
    third.id = PaneId::new();
    third.view_top_row = 9;
    snap.panes = vec![first.clone(), second.clone(), third.clone()];

    let frame = MouseFrame::from(snap);

    assert_eq!(
        frame
            .panes
            .iter()
            .map(|pane| (pane.id, pane.view_top_row))
            .collect::<Vec<_>>(),
        vec![(first.id, 0), (second.id, 7), (third.id, 9)]
    );
}

#[test]
fn a_mouse_frame_solves_its_regions_from_the_client_viewport() {
    let mut snap = fixture(fixture_grid());
    // The client sees more than the tab was solved for, so a builder reading
    // the tab's effective size instead of the client viewport lands elsewhere.
    snap.client.viewport = Size {
        cols: 100,
        rows: 30,
    };
    assert_eq!(
        snap.session.active_tab.effective_size,
        Size { cols: 80, rows: 24 }
    );

    let frame = MouseFrame::from(snap);

    assert_eq!(
        frame.committed_regions,
        CommittedRegions::core(
            Size {
                cols: 100,
                rows: 30
            },
            0
        )
    );
    assert_eq!(
        frame.committed_regions.viewport,
        Size {
            cols: 100,
            rows: 30
        }
    );
}

#[test]
fn a_mouse_frame_from_a_paneless_snapshot_carries_no_pane_entries() {
    let mut snap = fixture(fixture_grid());
    snap.panes.clear();

    let frame = MouseFrame::from(snap);

    assert_eq!(frame.panes, Vec::new());
    assert_eq!(
        frame.committed_regions,
        CommittedRegions::core(Size { cols: 80, rows: 24 }, 0)
    );
}

#[test]
fn with_regions_keeps_the_solve_it_is_given() {
    let snap = fixture(fixture_grid());
    // A viewport and revision that differ from the client's own, so a builder
    // that re-derived the solve instead of carrying it lands elsewhere.
    let committed = CommittedRegions::core(Size { cols: 40, rows: 10 }, 12);

    let frame = MouseFrame::with_regions(snap, committed.clone());

    assert_eq!(frame.committed_regions, committed);
    assert_eq!(
        frame.committed_regions.viewport,
        Size { cols: 40, rows: 10 }
    );
    assert_eq!(frame.committed_regions.input_revision, 12);
}

#[test]
fn committed_regions_carries_the_exact_solve_it_was_built_from() {
    let solve = core_region_solve(Size { cols: 80, rows: 24 });
    let committed = CommittedRegions::new(Size { cols: 80, rows: 24 }, solve.clone(), 5);

    assert_eq!(committed.viewport, Size { cols: 80, rows: 24 });
    assert_eq!(committed.solve, solve);
    assert_eq!(committed.input_revision, 5);
    assert_eq!(
        committed,
        CommittedRegions::core(Size { cols: 80, rows: 24 }, 5)
    );
}

#[test]
fn viewer_chrome_defaults_to_no_pointer_no_tab_offset_and_no_reconnect() {
    assert_eq!(
        ViewerChrome::default(),
        ViewerChrome {
            hovered_pane: None,
            tabline_offset: None,
            reconnecting: None,
        }
    );
}

#[test]
fn a_snapshot_layout_borrows_the_frame_and_holds_no_committed_regions() {
    let snap = fixture(fixture_grid());
    let viewer = ViewerChrome {
        hovered_pane: Some(snap.panes[0].id),
        tabline_offset: Some(3),
        reconnecting: Some(Reconnecting {
            attempt: 4,
            retry_in_seconds: 8,
        }),
    };

    let layout = snap.layout(viewer);

    assert_eq!(*layout.session, snap.session);
    assert_eq!(*layout.client, snap.client);
    assert_eq!(layout.viewer, viewer);
    assert_eq!(layout.committed_regions, None);
}

#[test]
fn an_owned_frame_layout_borrows_its_session_and_client() {
    let snap = fixture(fixture_grid());
    let owned = OwnedFrameLayout {
        session: snap.session.clone(),
        client: snap.client.clone(),
    };

    let layout = owned.layout(ViewerChrome::default());

    assert_eq!(*layout.session, snap.session);
    assert_eq!(*layout.client, snap.client);
    assert_eq!(layout.viewer, ViewerChrome::default());
    assert_eq!(layout.committed_regions, None);
}

#[test]
fn a_mouse_frame_layout_carries_the_regions_that_were_painted() {
    let snap = fixture(fixture_grid());
    let committed = CommittedRegions::core(Size { cols: 80, rows: 24 }, 3);
    let frame = MouseFrame::with_regions(snap, committed.clone());

    let layout = frame.layout(ViewerChrome::default());

    assert_eq!(layout.committed_regions, Some(&committed));
}

#[test]
fn the_tabline_view_takes_its_fields_from_the_session_client_and_viewer() {
    let mut snap = fixture(fixture_grid());
    snap.client.lock_mode = LockMode::Locked;
    snap.client.mouse_select = true;
    let reconnecting = Reconnecting {
        attempt: 4,
        retry_in_seconds: 8,
    };
    let viewer = ViewerChrome {
        hovered_pane: None,
        tabline_offset: Some(2),
        reconnecting: Some(reconnecting),
    };

    let layout = snap.layout(viewer);
    let tabline = layout.tabline();

    assert_eq!(tabline.session_name, "sess");
    assert_eq!(tabline.tabs, snap.session.tabs_metadata.as_slice());
    assert_eq!(tabline.lock_mode, LockMode::Locked);
    assert!(tabline.mouse_select);
    assert_eq!(tabline.reconnecting, Some(reconnecting));
    assert_eq!(tabline.tabline_offset, Some(2));
}

#[test]
fn row_span_returns_the_inclusive_columns_of_a_highlighted_row() {
    let spans = SelectionSpans {
        rows: vec![(4, 12, 79), (5, 0, 79), (6, 0, 33)],
    };

    assert_eq!(spans.row_span(4), Some((12, 79)));
    assert_eq!(spans.row_span(5), Some((0, 79)));
    assert_eq!(spans.row_span(6), Some((0, 33)));
}

#[test]
fn row_span_is_none_for_a_row_the_highlight_does_not_touch() {
    let spans = SelectionSpans {
        rows: vec![(4, 12, 79), (6, 0, 33)],
    };

    assert_eq!(spans.row_span(3), None);
    assert_eq!(spans.row_span(5), None);
    assert_eq!(spans.row_span(7), None);
    assert_eq!(spans.row_span(u16::MAX), None);
}

#[test]
fn row_span_of_an_empty_highlight_is_none() {
    let spans = SelectionSpans { rows: Vec::new() };

    assert_eq!(spans.row_span(0), None);
}

#[test]
fn row_span_answers_a_single_cell_highlight_with_that_one_column() {
    let spans = SelectionSpans {
        rows: vec![(0, 7, 7)],
    };

    assert_eq!(spans.row_span(0), Some((7, 7)));
}

#[test]
fn row_span_takes_the_first_entry_when_a_row_is_listed_twice() {
    let spans = SelectionSpans {
        rows: vec![(2, 0, 5), (2, 10, 20)],
    };

    assert_eq!(spans.row_span(2), Some((0, 5)));
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

    let original = snap.panes[0].grid_view.as_ref().unwrap();
    let cloned = clone.panes[0].grid_view.as_ref().unwrap();
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
    *grid_b.cell_mut(0, 0).unwrap() = Cell::new('x', 1, Style::default());
    let snap_b = fixture(Arc::new(grid_b));

    assert_ne!(snap_a, snap_b);
}

#[test]
fn snapshot_with_no_panes_or_tabs_is_valid_and_equals_its_clone() {
    // An empty snapshot (no panes, no layout slots, no tabs, no focus) must
    // still construct and compare without panicking — the degenerate state
    // right after a session's last pane closes.
    let mut snap = fixture(fixture_grid());
    snap.panes.clear();
    snap.session.active_tab.layout_solved.clear();
    snap.session.tabs_metadata.clear();
    snap.client.focused_pane = None;

    assert!(snap.panes.is_empty());
    assert!(snap.session.active_tab.layout_solved.is_empty());
    assert_eq!(snap, snap.clone());
}
