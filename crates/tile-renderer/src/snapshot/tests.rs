//! Tests for the render-snapshot DTOs: build a full snapshot from fixture
//! pieces, check exact field values, confirm it is `Send + Sync`, confirm
//! cloning shares the grid by reference (no cell copy), and confirm equality.

use super::*;

use tile_core::geometry::Point;
use tile_terminal::style::Style;

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
        },
        grid_view: Some(GridView {
            grid,
            view_offset: 0,
        }),
        reverse_video: false,
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
