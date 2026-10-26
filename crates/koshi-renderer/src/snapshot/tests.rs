//! Tests for the render-snapshot DTOs: build a full snapshot from fixture
//! pieces, check exact field values, confirm it is `Send + Sync`, confirm
//! cloning shares every row by reference (no cell copy), confirm equality, and
//! check the mouse-frame projection, the borrowed layout views, the selection
//! row lookup, the pane slots of a layout solve, the tab a placement
//! preview shows for a tiled and a floating source, and the comparison of two
//! frames' floating pane visibility.

use super::*;

/// The mouse frame for `render_snapshot` under the compiled-in region solve for
/// its client's viewport, at input revision `0`.
fn build_mouse_frame(render_snapshot: RenderSnapshot) -> MouseFrame {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    MouseFrame::from_snapshot(&render_snapshot, committed_regions, ViewerChrome::default())
}

use koshi_core::geometry::Point;
use koshi_terminal::grid::state::Cell;
use koshi_terminal::style::Style;
use koshi_test_support::fixtures::build_cell_rect;

/// A 24-row × 80-column blank grid.
fn build_render_snapshot_fixture_grid() -> Grid {
    Grid::build_blank(24, 80, Style::default())
}

/// A one-tab, one-terminal-pane, one-client snapshot built around `terminal_grid`.
fn build_render_snapshot_fixture(terminal_grid: Grid) -> RenderSnapshot {
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
        session_name: "sess".to_string(),
        active_tab_snapshot,
        tabs_metadata: vec![TabMetadata {
            tab_id,
            tab_name: "shell".to_string(),
            tab_index: 0,
            is_active: true,
        }],
        floating_pane_slots: Vec::new(),
        floating_panes: Vec::new(),
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
        terminal_grid_view: Some(GridView::from_grid(&terminal_grid, 0)),
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
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        active_tab_id: tab_id,
        focused_pane_id: Some(pane_id),
        active_tab_focused_pane_id: Some(pane_id),
        lock_mode: LockMode::Normal,
        last_applied_lock_command_id: None,
        last_applied_floating_pane_move_command_id: None,
        is_mouse_selection_enabled: false,
        last_applied_placement_command_id: None,
    };

    RenderSnapshot {
        session_snapshot,
        pane_snapshots: vec![pane_snapshot],
        client_snapshot,
        placement_preview: None,
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
    assert_eq!(terminal_grid_view.get_view_dimensions(), (24, 80));

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
    // Each field gets a value that no other field holds.
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
    // The client viewport is larger than the size the tab was solved for.
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
fn mouse_frame_from_snapshot_keeps_the_supplied_region_solve_and_viewer_chrome() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    // A viewport and revision that differ from the client's own.
    let committed_regions = CommittedRegions::build_core(
        Size {
            column_count: 40,
            row_count: 10,
        },
        12,
    );

    let viewer_chrome = ViewerChrome {
        tabline_offset: Some(2),
        floating_pane_move_draft: Some(FloatingPaneMoveDraft {
            pane_id: PaneId::new(),
            top_left_cell: Point { column: 7, row: 1 },
        }),
        ..ViewerChrome::default()
    };

    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions.clone(), viewer_chrome);

    assert_eq!(mouse_frame.committed_regions, committed_regions);
    assert_eq!(mouse_frame.viewer_chrome, viewer_chrome);
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
fn a_tiled_hover_shows_only_while_linked_in_a_mode_whose_handle_press_starts_a_drag() {
    let pane_id = PaneId::new();
    for (active_input_mode, is_dock_undock_command_pending, is_shown) in [
        (LockMode::Normal, false, true),
        (LockMode::Locked, false, true),
        (LockMode::PaneMoveChoice, false, true),
        (LockMode::DockUndock, false, true),
        (LockMode::DockUndock, true, false),
        (LockMode::PanePlacement, false, false),
        (LockMode::FloatingPaneMove, false, false),
    ] {
        for tiled_hover in [
            PlacementHandleHover::TiledTopBorder { pane_id },
            PlacementHandleHover::TiledHandle { pane_id },
        ] {
            assert_eq!(
                find_shown_placement_handle_hover(
                    Some(tiled_hover),
                    active_input_mode,
                    LinkStatus::Linked,
                    false,
                    is_dock_undock_command_pending,
                    false,
                ),
                is_shown.then_some(tiled_hover),
                "{tiled_hover:?} in {active_input_mode:?} with a pending dock/undock \
                 {is_dock_undock_command_pending}"
            );
        }
    }
}

#[test]
fn a_floating_hover_shows_only_in_a_mode_whose_grab_handle_press_starts_a_drag() {
    let pane_id = PaneId::new();
    for (
        active_input_mode,
        is_pinned,
        is_dock_undock_command_pending,
        is_floating_pane_move_waiting,
        is_shown,
    ) in [
        (LockMode::Normal, false, false, false, true),
        (LockMode::Normal, true, false, false, false),
        (LockMode::Normal, false, false, true, false),
        (LockMode::Locked, false, false, false, true),
        (LockMode::Locked, true, false, false, false),
        (LockMode::Locked, false, false, true, false),
        (LockMode::PaneMoveChoice, false, false, false, true),
        (LockMode::PaneMoveChoice, true, false, false, false),
        (LockMode::PaneMoveChoice, false, false, true, false),
        (LockMode::FloatingPaneMove, false, false, false, true),
        (LockMode::FloatingPaneMove, true, false, false, false),
        (LockMode::FloatingPaneMove, false, false, true, false),
        (LockMode::DockUndock, false, false, false, true),
        (LockMode::DockUndock, true, false, false, true),
        (LockMode::DockUndock, false, true, false, false),
        (LockMode::DockUndock, true, true, false, false),
        (LockMode::DockUndock, false, false, true, false),
        (LockMode::PanePlacement, false, false, false, false),
    ] {
        let floating_hover = PlacementHandleHover::FloatingHandle { pane_id, is_pinned };
        assert_eq!(
            find_shown_placement_handle_hover(
                Some(floating_hover),
                active_input_mode,
                LinkStatus::Linked,
                false,
                is_dock_undock_command_pending,
                is_floating_pane_move_waiting,
            ),
            is_shown.then_some(floating_hover),
            "{floating_hover:?} in {active_input_mode:?} with a pending dock/undock \
             {is_dock_undock_command_pending} and a waiting floating pane move \
             {is_floating_pane_move_waiting}"
        );
    }
}

#[test]
fn no_hover_shows_while_pane_placement_shows_or_the_link_is_down() {
    let pane_id = PaneId::new();
    let hidden_link_statuses = [
        LinkStatus::AwaitingRestartedSession,
        LinkStatus::Redialing(Reconnecting {
            attempt: 1,
            retry_in_seconds: None,
        }),
    ];
    for pointer_hover in [
        PlacementHandleHover::TiledTopBorder { pane_id },
        PlacementHandleHover::TiledHandle { pane_id },
        PlacementHandleHover::FloatingHandle {
            pane_id,
            is_pinned: false,
        },
    ] {
        for active_input_mode in LockMode::ALL {
            assert_eq!(
                find_shown_placement_handle_hover(
                    Some(pointer_hover),
                    active_input_mode,
                    LinkStatus::Linked,
                    true,
                    false,
                    false,
                ),
                None,
                "{pointer_hover:?} in {active_input_mode:?} while pane placement shows"
            );
            for link_status in hidden_link_statuses {
                assert_eq!(
                    find_shown_placement_handle_hover(
                        Some(pointer_hover),
                        active_input_mode,
                        link_status,
                        false,
                        false,
                        false,
                    ),
                    None,
                    "{pointer_hover:?} in {active_input_mode:?} while {link_status:?}"
                );
            }
        }
    }
    assert_eq!(
        find_shown_placement_handle_hover(
            None,
            LockMode::Normal,
            LinkStatus::Linked,
            false,
            false,
            false
        ),
        None
    );
}

#[test]
fn tabline_inputs_carry_no_reconnect_tag_while_linked_or_awaiting_a_restarted_session() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    for link_status in [LinkStatus::Linked, LinkStatus::AwaitingRestartedSession] {
        let viewer_chrome = ViewerChrome {
            link_status,
            ..ViewerChrome::default()
        };
        assert_eq!(
            render_snapshot
                .build_frame_layout(viewer_chrome)
                .get_tabline_inputs()
                .reconnecting,
            None,
            "{link_status:?}"
        );
    }
}

#[test]
fn viewer_chrome_defaults_to_no_pointer_no_tab_offset_and_no_reconnect() {
    assert_eq!(
        ViewerChrome::default(),
        ViewerChrome {
            hovered_pane_id: None,
            placement_handle_hover: None,
            active_input_mode: None,
            tabline_offset: None,
            link_status: LinkStatus::Linked,
            is_pane_placement_visible: false,
            placement_source_pane_id: None,
            floating_pane_move_draft: None,
            floating_pane_tray: None,
            notification_center: None,
            notification_button: None,
            notification_toast: None,
        }
    );
}

#[test]
fn render_snapshot_frame_layout_borrows_the_frame_without_committed_regions() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let viewer_chrome = ViewerChrome {
        hovered_pane_id: Some(render_snapshot.pane_snapshots[0].pane_id),
        placement_handle_hover: None,
        active_input_mode: None,
        tabline_offset: Some(3),
        link_status: LinkStatus::Redialing(Reconnecting {
            attempt: 4,
            retry_in_seconds: Some(8),
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
fn a_mouse_frame_layout_carries_the_regions_and_viewer_chrome_that_were_painted() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let committed_regions = CommittedRegions::build_core(
        Size {
            column_count: 80,
            row_count: 24,
        },
        3,
    );
    let viewer_chrome = ViewerChrome {
        tabline_offset: Some(1),
        ..ViewerChrome::default()
    };
    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions.clone(), viewer_chrome);

    let frame_layout = mouse_frame.build_frame_layout();

    assert_eq!(frame_layout.committed_regions, Some(&committed_regions));
    assert_eq!(frame_layout.viewer_chrome, viewer_chrome);
}

#[test]
fn tabline_inputs_borrow_session_client_and_viewer_fields() {
    let mut render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    render_snapshot.client_snapshot.lock_mode = LockMode::Locked;
    render_snapshot.client_snapshot.is_mouse_selection_enabled = true;
    let reconnecting = Reconnecting {
        attempt: 4,
        retry_in_seconds: Some(8),
    };
    let viewer_chrome = ViewerChrome {
        hovered_pane_id: None,
        placement_handle_hover: None,
        active_input_mode: None,
        tabline_offset: Some(2),
        link_status: LinkStatus::Redialing(reconnecting),
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
fn cloning_render_snapshot_shares_every_row_by_reference() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());

    // Cloning the snapshot adds one reference to each row and copies no cell.
    let cloned_render_snapshot = render_snapshot.clone();

    let original_grid_view = render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .unwrap();
    let cloned_grid_view = cloned_render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .unwrap();
    assert_eq!(original_grid_view.row_snapshots.len(), 24);
    assert!(original_grid_view
        .row_snapshots
        .iter()
        .zip(&cloned_grid_view.row_snapshots)
        .all(|(original_row_snapshot, cloned_row_snapshot)| Arc::ptr_eq(
            original_row_snapshot,
            cloned_row_snapshot
        )));
    assert!(original_grid_view
        .row_snapshots
        .iter()
        .all(|row_snapshot| Arc::strong_count(row_snapshot) == 2));
}

#[test]
fn cloning_render_snapshot_preserves_equality() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    assert_eq!(render_snapshot, render_snapshot.clone());
}

#[test]
fn render_snapshots_with_different_grid_cells_are_not_equal() {
    // Two snapshots that differ in one grid cell and in no top-level field
    // compare unequal: the derived `PartialEq` compares the grid too.
    let first_terminal_grid = Grid::build_blank(24, 80, Style::default());
    let first_render_snapshot = build_render_snapshot_fixture(first_terminal_grid);

    let mut second_terminal_grid = Grid::build_blank(24, 80, Style::default());
    *second_terminal_grid.get_cell_mut(0, 0).unwrap() =
        Cell::from_character('x', 1, Style::default());
    let second_render_snapshot = build_render_snapshot_fixture(second_terminal_grid);

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

/// A shown pane gets its outer rect inset by one cell as its content, a
/// suppressed pane and a collapsed stack member get no content, and only the
/// suppressed pane is marked suppressed, each slot in solve order.
#[test]
fn list_pane_slots_gives_a_shown_a_suppressed_and_a_collapsed_stack_pane_their_slots() {
    let shown_pane_id = PaneId::new();
    let suppressed_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let layout_solve = LayoutSolve {
        pane_rects: vec![
            (shown_pane_id, build_cell_rect(0, 1, 10, 5)),
            (suppressed_pane_id, build_cell_rect(10, 0, 0, 0)),
            (collapsed_pane_id, build_cell_rect(0, 0, 10, 1)),
        ],
        suppressed_pane_ids: vec![suppressed_pane_id],
        is_every_pane_suppressed: false,
        stack_headers: vec![StackHeader {
            pane_id: collapsed_pane_id,
            header_rect: build_cell_rect(0, 0, 10, 1),
            member_index: 1,
            member_count: 2,
        }],
    };

    assert_eq!(
        list_pane_slots(&layout_solve),
        vec![
            PaneSlot {
                pane_id: shown_pane_id,
                outer_rect: build_cell_rect(0, 1, 10, 5),
                content_rect: Some(build_cell_rect(1, 2, 8, 3)),
                is_visible: true,
                is_suppressed: false,
            },
            PaneSlot {
                pane_id: suppressed_pane_id,
                outer_rect: build_cell_rect(10, 0, 0, 0),
                content_rect: None,
                is_visible: false,
                is_suppressed: true,
            },
            PaneSlot {
                pane_id: collapsed_pane_id,
                outer_rect: build_cell_rect(0, 0, 10, 1),
                content_rect: None,
                is_visible: false,
                is_suppressed: false,
            },
        ]
    );
}

/// A visible slot is found by its pane id. A hidden slot and a pane id with no
/// slot give `None`.
#[test]
fn find_visible_pane_slot_finds_only_a_visible_slot_of_the_pane() {
    let shown_pane_id = PaneId::new();
    let hidden_pane_id = PaneId::new();
    let shown_pane_slot = PaneSlot {
        pane_id: shown_pane_id,
        outer_rect: build_cell_rect(0, 0, 10, 5),
        content_rect: Some(build_cell_rect(1, 1, 8, 3)),
        is_visible: true,
        is_suppressed: false,
    };
    let tab_snapshot = TabSnapshot {
        pane_slots: vec![
            PaneSlot {
                pane_id: hidden_pane_id,
                outer_rect: build_cell_rect(10, 0, 10, 5),
                content_rect: None,
                is_visible: false,
                is_suppressed: false,
            },
            shown_pane_slot.clone(),
        ],
        ..build_render_snapshot_fixture(build_render_snapshot_fixture_grid())
            .session_snapshot
            .active_tab_snapshot
    };

    assert_eq!(
        tab_snapshot.find_visible_pane_slot(shown_pane_id),
        Some(&shown_pane_slot)
    );
    assert_eq!(tab_snapshot.find_visible_pane_slot(hidden_pane_id), None);
    assert_eq!(tab_snapshot.find_visible_pane_slot(PaneId::new()), None);
}

/// A placement tab snapshot of `tab_snapshot`, its layout one leaf of the
/// tab's first pane.
fn build_placement_tab_snapshot(tab_snapshot: TabSnapshot) -> PlacementTabSnapshot {
    PlacementTabSnapshot {
        layout_tree: LayoutNode::Pane(tab_snapshot.pane_slots[0].pane_id),
        focus_history: Vec::new(),
        tab_snapshot,
    }
}

/// A tiled source shows its own tab when that tab is the destination, and the
/// destination tab otherwise. A floating source has no tab and shows the
/// destination tab.
#[test]
fn a_placement_snapshot_shows_the_source_tab_only_when_it_is_the_destination() {
    let render_snapshot = build_render_snapshot_fixture(build_render_snapshot_fixture_grid());
    let source_tab_snapshot = render_snapshot.session_snapshot.active_tab_snapshot.clone();
    let source_tab_id = source_tab_snapshot.tab_id;
    let destination_tab_snapshot = TabSnapshot {
        tab_id: TabId::new(),
        ..source_tab_snapshot.clone()
    };
    let destination_tab_id = destination_tab_snapshot.tab_id;
    let build_placement_snapshot =
        |source_pane_id: PaneId,
         source: PlacementSource,
         destination_tab_id: TabId,
         destination_tab_snapshot: Option<PlacementTabSnapshot>| {
            PlacementSnapshot {
                source_pane_id,
                source,
                destination_tab_id,
                destination_tab_snapshot,
                pane_sizing: PaneSizing::default(),
                insertion_tab_sizes: koshi_layout::placement::PlacementTabSizes {
                    destination_tab_size: None,
                    source_tab_size: None,
                },
                swap_tab_sizes: Vec::new(),
            }
        };
    let tiled_pane_id = source_tab_snapshot.pane_slots[0].pane_id;
    let floating_pane_id = PaneId::new();

    let same_tab_placement_snapshot = build_placement_snapshot(
        tiled_pane_id,
        PlacementSource::Tiled(build_placement_tab_snapshot(source_tab_snapshot.clone())),
        source_tab_id,
        None,
    );
    let other_tab_placement_snapshot = build_placement_snapshot(
        tiled_pane_id,
        PlacementSource::Tiled(build_placement_tab_snapshot(source_tab_snapshot.clone())),
        destination_tab_id,
        Some(build_placement_tab_snapshot(
            destination_tab_snapshot.clone(),
        )),
    );
    let floating_placement_snapshot = build_placement_snapshot(
        floating_pane_id,
        PlacementSource::Floating,
        destination_tab_id,
        Some(build_placement_tab_snapshot(
            destination_tab_snapshot.clone(),
        )),
    );

    assert_eq!(
        same_tab_placement_snapshot
            .find_source_tab_snapshot()
            .map(|source_tab_snapshot| source_tab_snapshot.tab_snapshot.tab_id),
        Some(source_tab_id)
    );
    assert_eq!(
        same_tab_placement_snapshot.find_displayed_tab_snapshot(),
        Some(&build_placement_tab_snapshot(source_tab_snapshot.clone()))
    );
    assert_eq!(
        other_tab_placement_snapshot.find_source_tab_snapshot(),
        Some(&build_placement_tab_snapshot(source_tab_snapshot))
    );
    assert_eq!(
        other_tab_placement_snapshot.find_displayed_tab_snapshot(),
        Some(&build_placement_tab_snapshot(
            destination_tab_snapshot.clone()
        ))
    );
    assert_eq!(floating_placement_snapshot.find_source_tab_snapshot(), None);
    assert_eq!(
        floating_placement_snapshot.find_displayed_tab_snapshot(),
        Some(&build_placement_tab_snapshot(destination_tab_snapshot))
    );
}

#[test]
fn same_floating_pane_visibility_needs_the_same_panes_in_order_each_shown_or_minimized_in_both() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let build_session_snapshot = |floating_panes: Vec<FloatingPane>| SessionSnapshot {
        floating_panes,
        ..build_render_snapshot_fixture(build_render_snapshot_fixture_grid()).session_snapshot
    };
    let build_minimized_floating_pane =
        |pane_id: PaneId, pane_title: &str| FloatingPane::Minimized {
            pane_id,
            pane_title: Some(pane_title.to_string()),
        };
    let session_snapshot = build_session_snapshot(vec![
        FloatingPane::Shown {
            pane_id: first_pane_id,
        },
        build_minimized_floating_pane(second_pane_id, "htop"),
    ]);

    assert!(
        session_snapshot.has_same_floating_pane_visibility(&build_session_snapshot(vec![
            FloatingPane::Shown {
                pane_id: first_pane_id,
            },
            build_minimized_floating_pane(second_pane_id, "logs"),
        ]))
    );
    for (changed_case, other_floating_panes) in [
        (
            "the second pane restored",
            vec![
                FloatingPane::Shown {
                    pane_id: first_pane_id,
                },
                FloatingPane::Shown {
                    pane_id: second_pane_id,
                },
            ],
        ),
        (
            "the first pane minimized",
            vec![
                build_minimized_floating_pane(first_pane_id, "vim"),
                build_minimized_floating_pane(second_pane_id, "htop"),
            ],
        ),
        (
            "the second pane closed",
            vec![FloatingPane::Shown {
                pane_id: first_pane_id,
            }],
        ),
        (
            "the panes listed in the other order",
            vec![
                build_minimized_floating_pane(second_pane_id, "htop"),
                FloatingPane::Shown {
                    pane_id: first_pane_id,
                },
            ],
        ),
        (
            "a third pane added",
            vec![
                FloatingPane::Shown {
                    pane_id: first_pane_id,
                },
                build_minimized_floating_pane(second_pane_id, "htop"),
                FloatingPane::Shown {
                    pane_id: PaneId::new(),
                },
            ],
        ),
    ] {
        assert!(
            !session_snapshot
                .has_same_floating_pane_visibility(&build_session_snapshot(other_floating_panes)),
            "{changed_case}"
        );
    }
}
