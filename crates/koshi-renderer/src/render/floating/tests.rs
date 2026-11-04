//! Tests for floating pane compositing. Each floating pane this viewer draws
//! is placed at the pane area's origin with no letterbox offset, its layout
//! area starts on the row below its strip, and its visible area is its box cut
//! to the pane area. The panes draw back to front over the tiled panes and the
//! letterbox. Each one draws its titlebar strip with no title, then its tree
//! as the tiled layer draws a tab: one pane, `[vim | htop]`, a stack, and a
//! zoomed pane whose label carries `[fullscreen]`, each with its pane cells,
//! border style, labels, scroll position and unavailable-image text cut to
//! its own pane. A hovered grab handle styles and tints every pane of the
//! tree and draws no placement handle. A floating pane whose every pane is
//! suppressed draws its strip and `Terminal too small — enlarge window` below
//! it, and a one-row floating pane draws its strip alone. A narrow strip drops
//! the pin toggle, then the minimize button, then the close button. A glyph
//! two columns wide that starts in the column left of a floating pane turns
//! blank, and the frame diff writes the pane's left border beside it.
//! `U+17D8` there takes 1 column and stays, and in a label it takes 1 column.
//! While pane placement shows, only the placement source pane is placed and
//! drawn, and only while the active tab holds no slot of it. No floating pane
//! is placed or drawn while every tiled pane is suppressed. The pane a move
//! draft names is placed at the draft's top-left cell, moved inside the pane
//! area.

use koshi_test_support::fixtures::build_floating_pane_id;

use super::*;

use ratatui::buffer::CellWidth;
use ratatui::style::{Color, Modifier};

use koshi_core::geometry::{Point, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::{solve_layout_with_sizing, PaneSizing};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style as TerminalStyle;
use koshi_test_support::fixtures::build_cell_rect;

use crate::images::ImageRenderMode;
use crate::notification::tests::build_test_notification_view;
use crate::notification::NotificationCenterEntries;
use crate::render::tests::{
    build_floating_pane_slot, build_one_pane_floating_pane_slot, format_rendered_cells,
    get_pane_snapshot_mut,
};
use crate::render::{build_frame_geometry, render_frame};
use crate::snapshot::{
    ClientSnapshot, CursorSnapshot, FloatingPaneMoveDraft, FloatingPaneSlot, GridView,
    ImagePlacementSnapshot, KeymapHints, LayoutAreaSnapshot, PaneSlot, PaneSnapshot,
    PlacementHandleHover, ScrollbackMetadata, SessionSnapshot, TabMetadata, TabSnapshot,
};

/// The content of `pane_id`: no title, no grid, a visible cursor at `(0, 0)`.
fn build_pane_snapshot(pane_id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        pane_id,
        pane_title: None,
        cursor_snapshot: CursorSnapshot {
            row_index: 0,
            column_index: 0,
            is_visible: true,
            is_blinking: false,
            shape: None,
        },
        terminal_grid_view: None,
        image_placement_snapshots: Vec::new(),
        is_reverse_video: false,
        mouse_tracking: MouseTracking::Off,
        is_alternate_scroll_enabled: false,
        is_on_alternate_screen: false,
        view_top_row_index: 0,
        selection_spans: None,
        has_selection: false,
        scrollback_metadata: ScrollbackMetadata {
            retained_line_count: 0,
        },
    }
}

/// A frame for a client with a `viewport_size` viewport, whose tab is one
/// focused tiled pane filling a `tab_size` layout. The tiled pane has no grid,
/// and no floating pane shows.
fn build_tiled_render_snapshot(viewport_size: Size, tab_size: Size) -> RenderSnapshot {
    let tab_id = TabId::new();
    let tiled_pane_id = PaneId::new();
    let tiled_outer_rect = Rect::from_size_at_origin(tab_size);
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "s".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "t".to_string(),
                layout_area: LayoutAreaSnapshot {
                    solved_size: tab_size,
                    pane_slots: vec![PaneSlot {
                        pane_id: tiled_pane_id,
                        outer_rect: tiled_outer_rect,
                        content_rect: Some(tiled_outer_rect.compute_inner_with_border()),
                        is_visible: true,
                        is_suppressed: false,
                    }],
                    stack_headers: Vec::new(),
                    layout_mode: LayoutMode::Tiled,
                    is_every_pane_suppressed: false,
                    gap_cell_count: 0,
                },
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: "t".to_string(),
                tab_index: 0,
                is_active: true,
            }],
            floating_pane_slots: Vec::new(),
            floating_panes: Vec::new(),
        },
        pane_snapshots: vec![build_pane_snapshot(tiled_pane_id)],
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size,
            active_tab_id: tab_id,
            focused_pane_id: Some(tiled_pane_id),
            active_tab_focused_pane_id: Some(tiled_pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            is_mouse_selection_enabled: false,
            last_applied_placement_command_id: None,
        },
        placement_preview: None,
    }
}

/// A `40x12` client whose tab is one focused tiled pane filling the `40x10`
/// pane area, which starts at row `1`.
fn build_forty_by_twelve_render_snapshot() -> RenderSnapshot {
    build_tiled_render_snapshot(
        Size {
            column_count: 40,
            row_count: 12,
        },
        Size {
            column_count: 40,
            row_count: 10,
        },
    )
}

/// Put `floating_pane_slot` on top of the frame's floating panes, with one
/// pane snapshot for each `(pane_id, pane_title)` of `titled_pane_ids`, titled
/// `pane_title` (none when empty) with no grid.
fn push_floating_pane_slot(
    render_snapshot: &mut RenderSnapshot,
    floating_pane_slot: FloatingPaneSlot,
    titled_pane_ids: &[(PaneId, &str)],
) {
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(floating_pane_slot);
    for &(pane_id, pane_title) in titled_pane_ids {
        let mut pane_snapshot = build_pane_snapshot(pane_id);
        pane_snapshot.pane_title = (!pane_title.is_empty()).then(|| pane_title.to_string());
        render_snapshot.pane_snapshots.push(pane_snapshot);
    }
}

/// Put a floating pane with outer rect `outer_rect`, in pane-area cells, that
/// holds one pane, on top of the frame's floating panes, its content titled
/// `pane_title` (none when empty) with no grid. Returns its pane id.
fn push_floating_pane(
    render_snapshot: &mut RenderSnapshot,
    outer_rect: Rect,
    pane_title: &str,
) -> PaneId {
    let pane_id = PaneId::new();
    push_floating_pane_slot(
        render_snapshot,
        build_one_pane_floating_pane_slot(pane_id, outer_rect, false),
        &[(pane_id, pane_title)],
    );
    pane_id
}

/// Put a `36x7` floating pane at pane-area `(2, 1)` on top of the frame's
/// floating panes, frame cells `(2, 2)` to `(37, 8)`, whose tree is `vim`
/// beside `htop` and whose input pane is `vim`. While `is_vim_zoomed` is
/// `true`, the viewer zooms `vim`. Returns `[vim, htop]`.
fn push_vim_beside_htop_floating_pane(
    render_snapshot: &mut RenderSnapshot,
    is_vim_zoomed: bool,
) -> [PaneId; 2] {
    let [vim_pane_id, htop_pane_id] = [PaneId::new(), PaneId::new()];
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(vim_pane_id),
            LayoutNode::Pane(htop_pane_id),
        ],
    ));
    let layout_mode = if is_vim_zoomed {
        LayoutMode::Fullscreen {
            focused_pane_id: vim_pane_id,
        }
    } else {
        LayoutMode::Tiled
    };
    push_floating_pane_slot(
        render_snapshot,
        build_floating_pane_slot(
            vim_pane_id,
            &layout_tree,
            layout_mode,
            build_cell_rect(2, 1, 36, 7),
            false,
        ),
        &[(vim_pane_id, "vim"), (htop_pane_id, "htop")],
    );
    [vim_pane_id, htop_pane_id]
}

/// A blank `row_count x column_count` grid whose first row starts with
/// `first_row_text`, shown at the live tail.
fn build_text_grid_view(row_count: u16, column_count: u16, first_row_text: &str) -> GridView {
    let mut grid = Grid::build_blank(row_count, column_count, TerminalStyle::default());
    for (column_index, character) in (0..).zip(first_row_text.chars()) {
        *grid
            .get_cell_mut(0, column_index)
            .expect("the text fits the grid") =
            Cell::from_character(character, 1, TerminalStyle::default());
    }
    GridView::from_grid(&grid, 0)
}

/// Paint `render_snapshot` over the compiled-in region solve of its client's
/// viewport into a fresh buffer of that size, in the default theme, with
/// `viewer_chrome`, and images in `image_render_mode` with no native image
/// ready list.
fn render_core_frame(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
    image_render_mode: ImageRenderMode,
) -> Buffer {
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    let viewport_area = RatatuiRect::new(0, 0, viewport_size.column_count, viewport_size.row_count);
    let mut render_buffer = Buffer::empty(viewport_area);
    render_frame(
        &build_frame_geometry(
            render_snapshot,
            &CommittedRegions::build_core(viewport_size, 0),
            viewer_chrome,
            viewport_area,
        ),
        &Theme::default(),
        &KeymapHints::default(),
        None,
        image_render_mode,
        &[],
        None,
        None,
        build_test_notification_view(&NotificationCenterEntries::default()),
        DrawnRows::Every,
        &mut render_buffer,
    );
    render_buffer
}

/// [`render_core_frame`] with the default viewer chrome and placeholder
/// images.
fn render_default_core_frame(render_snapshot: &RenderSnapshot) -> Buffer {
    render_core_frame(
        render_snapshot,
        ViewerChrome::default(),
        ImageRenderMode::Placeholder,
    )
}

/// Assert that rows `2` to `2 + expected_rows.len() - 1` of `render_buffer`,
/// read over columns `0` to `39`, are `expected_rows`.
fn assert_rows_from_row_two(render_buffer: &Buffer, expected_rows: &[String]) {
    for (row_index, expected_row) in (2..).zip(expected_rows) {
        assert_eq!(
            &format_rendered_cells(render_buffer, row_index, 0..40),
            expected_row,
            "row {row_index}"
        );
    }
}

/// The compiled-in region solve of `render_snapshot`'s client viewport.
fn build_core_committed_regions(render_snapshot: &RenderSnapshot) -> CommittedRegions {
    CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0)
}

/// [`list_placed_floating_panes`] for `render_snapshot` over
/// `committed_regions` and its client's viewport.
fn list_core_placed_floating_panes<'a>(
    render_snapshot: &'a RenderSnapshot,
    committed_regions: &'a CommittedRegions,
    viewer_chrome: ViewerChrome,
) -> Vec<PlacedFloatingPane<'a>> {
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    list_placed_floating_panes(
        render_snapshot,
        committed_regions,
        viewer_chrome,
        RatatuiRect::new(0, 0, viewport_size.column_count, viewport_size.row_count),
    )
}

/// The drawn slot of the floating pane at `slot_index` of `render_snapshot`,
/// drawn at `outer_rect`.
fn get_drawn_floating_pane_slot(
    render_snapshot: &RenderSnapshot,
    slot_index: usize,
    outer_rect: Rect,
) -> DrawnFloatingPaneSlot<'_> {
    DrawnFloatingPaneSlot {
        floating_pane_slot: &render_snapshot.session_snapshot.floating_pane_slots[slot_index],
        outer_rect,
    }
}

#[test]
fn a_placed_floating_pane_moves_by_the_pane_area_origin_and_takes_no_letterbox_offset() {
    let mut render_snapshot = build_tiled_render_snapshot(
        Size {
            column_count: 200,
            row_count: 50,
        },
        Size {
            column_count: 80,
            row_count: 22,
        },
    );
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(150, 4, 40, 12),
        "htop",
    );

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes = list_core_placed_floating_panes(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );

    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            drawn_floating_pane_slot: get_drawn_floating_pane_slot(
                &render_snapshot,
                0,
                build_cell_rect(150, 4, 40, 12)
            ),
            outer_area: RatatuiRect::new(150, 5, 40, 12),
            layout_origin: Point {
                column: 150,
                row: 6,
            },
            visible_area: RatatuiRect::new(150, 5, 40, 12),
        }]
    );
}

#[test]
fn a_floating_pane_past_the_pane_area_keeps_its_layout_origin_and_cuts_its_visible_area() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(30, 4, 20, 10), "htop");

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes = list_core_placed_floating_panes(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );

    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            drawn_floating_pane_slot: get_drawn_floating_pane_slot(
                &render_snapshot,
                0,
                build_cell_rect(30, 4, 20, 10)
            ),
            outer_area: RatatuiRect::new(30, 5, 20, 10),
            layout_origin: Point { column: 30, row: 6 },
            visible_area: RatatuiRect::new(30, 5, 10, 6),
        }]
    );
}

#[test]
fn a_move_draft_places_its_floating_pane_at_the_draft_cell() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let moved_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "htop");
    push_floating_pane(&mut render_snapshot, build_cell_rect(4, 3, 20, 6), "top");
    let viewer_chrome = ViewerChrome {
        floating_pane_move_draft: Some(FloatingPaneMoveDraft {
            floating_pane_id: build_floating_pane_id(moved_pane_id),

            top_left_cell: Point { column: 15, row: 4 },
        }),
        ..ViewerChrome::default()
    };

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes =
        list_core_placed_floating_panes(&render_snapshot, &committed_regions, viewer_chrome);

    assert_eq!(
        placed_floating_panes,
        [
            PlacedFloatingPane {
                drawn_floating_pane_slot: get_drawn_floating_pane_slot(
                    &render_snapshot,
                    0,
                    build_cell_rect(15, 4, 20, 6)
                ),
                outer_area: RatatuiRect::new(15, 5, 20, 6),
                layout_origin: Point { column: 15, row: 6 },
                visible_area: RatatuiRect::new(15, 5, 20, 6),
            },
            PlacedFloatingPane {
                drawn_floating_pane_slot: get_drawn_floating_pane_slot(
                    &render_snapshot,
                    1,
                    build_cell_rect(4, 3, 20, 6)
                ),
                outer_area: RatatuiRect::new(4, 4, 20, 6),
                layout_origin: Point { column: 4, row: 5 },
                visible_area: RatatuiRect::new(4, 4, 20, 6),
            },
        ]
    );
}

#[test]
fn a_move_draft_leaves_a_pinned_floating_pane_at_its_frame_cell() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let pinned_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "htop");
    render_snapshot.session_snapshot.floating_pane_slots[0].is_pinned = true;
    let viewer_chrome = ViewerChrome {
        floating_pane_move_draft: Some(FloatingPaneMoveDraft {
            floating_pane_id: build_floating_pane_id(pinned_pane_id),

            top_left_cell: Point { column: 15, row: 4 },
        }),
        ..ViewerChrome::default()
    };

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes =
        list_core_placed_floating_panes(&render_snapshot, &committed_regions, viewer_chrome);

    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            drawn_floating_pane_slot: get_drawn_floating_pane_slot(
                &render_snapshot,
                0,
                build_cell_rect(2, 1, 20, 6)
            ),
            outer_area: RatatuiRect::new(2, 2, 20, 6),
            layout_origin: Point { column: 2, row: 3 },
            visible_area: RatatuiRect::new(2, 2, 20, 6),
        }]
    );
}

#[test]
fn a_move_draft_past_the_pane_area_places_its_floating_pane_moved_inside_the_pane_area() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let moved_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "htop");
    let viewer_chrome = ViewerChrome {
        floating_pane_move_draft: Some(FloatingPaneMoveDraft {
            floating_pane_id: build_floating_pane_id(moved_pane_id),

            top_left_cell: Point { column: 35, row: 9 },
        }),
        ..ViewerChrome::default()
    };

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes =
        list_core_placed_floating_panes(&render_snapshot, &committed_regions, viewer_chrome);

    // The 20x6 pane in the 40x10 pane area, which starts at row 1, keeps its
    // top-left cell at (20, 4) at most.
    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            drawn_floating_pane_slot: get_drawn_floating_pane_slot(
                &render_snapshot,
                0,
                build_cell_rect(20, 4, 20, 6)
            ),
            outer_area: RatatuiRect::new(20, 5, 20, 6),
            layout_origin: Point { column: 20, row: 6 },
            visible_area: RatatuiRect::new(20, 5, 20, 6),
        }]
    );
}

#[test]
fn no_floating_pane_is_placed_while_pane_placement_shows() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes = list_core_placed_floating_panes(
        &render_snapshot,
        &committed_regions,
        ViewerChrome {
            is_pane_placement_visible: true,
            ..ViewerChrome::default()
        },
    );

    assert_eq!(placed_floating_panes, []);
}

#[test]
fn only_the_placement_source_floating_pane_is_placed_while_pane_placement_shows() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    let source_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "logs");
    let placement_viewer_chrome = ViewerChrome {
        is_pane_placement_visible: true,
        placement_source_pane_id: Some(source_pane_id),
        ..ViewerChrome::default()
    };

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let list_placed_pane_ids = |render_snapshot: &RenderSnapshot| -> Vec<PaneId> {
        list_core_placed_floating_panes(
            render_snapshot,
            &committed_regions,
            placement_viewer_chrome,
        )
        .iter()
        .map(|placed_floating_pane| {
            placed_floating_pane
                .drawn_floating_pane_slot
                .floating_pane_slot
                .pane_id
        })
        .collect()
    };
    assert_eq!(list_placed_pane_ids(&render_snapshot), vec![source_pane_id]);

    // The preview draws the source pane in the active tab's layout.
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots
        .push(PaneSlot {
            pane_id: source_pane_id,
            outer_rect: build_cell_rect(20, 0, 20, 10),
            content_rect: Some(build_cell_rect(21, 1, 18, 8)),
            is_visible: true,
            is_suppressed: false,
        });
    assert_eq!(list_placed_pane_ids(&render_snapshot), Vec::<PaneId>::new());
}

#[test]
fn no_floating_pane_is_placed_while_every_pane_is_suppressed() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .is_every_pane_suppressed = true;
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let committed_regions = build_core_committed_regions(&render_snapshot);
    let placed_floating_panes = list_core_placed_floating_panes(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );

    assert_eq!(placed_floating_panes, []);
}

#[test]
fn a_one_pane_floating_pane_draws_its_strip_over_a_bordered_pane_box() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_slot_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    get_pane_snapshot_mut(&mut render_snapshot, floating_slot_pane_id).terminal_grid_view =
        Some(build_text_grid_view(3, 24, "top"));

    let render_buffer = render_default_core_frame(&render_snapshot);

    // The `26x6` box at frame `(6, 3)` holds the strip on row 3 and one pane
    // box `26x5` with content `24x3` below it.
    assert_rows_from_row_two(
        &render_buffer,
        &[
            format!("│{}│", " ".repeat(38)),
            format!("│      ⣿⣿{}[ ]  –  ✕ {}│", " ".repeat(13), " ".repeat(7)),
            format!("│     ┌─── htop {}┐       │", "─".repeat(15)),
            format!("│     │top{}│       │", " ".repeat(21)),
            format!("│     │{}│       │", " ".repeat(24)),
            format!("│     │{}│       │", " ".repeat(24)),
            format!("│     └{}┘       │", "─".repeat(24)),
            format!("│{}│", " ".repeat(38)),
        ],
    );
}

#[test]
fn a_split_floating_pane_draws_each_pane_box_under_one_strip() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let [vim_pane_id, htop_pane_id] =
        push_vim_beside_htop_floating_pane(&mut render_snapshot, false);
    get_pane_snapshot_mut(&mut render_snapshot, vim_pane_id).terminal_grid_view =
        Some(build_text_grid_view(4, 16, "vim"));
    get_pane_snapshot_mut(&mut render_snapshot, htop_pane_id).terminal_grid_view =
        Some(build_text_grid_view(4, 16, "htop"));

    let render_buffer = render_default_core_frame(&render_snapshot);

    // `vim` takes box `(2, 3)` `18x6` and `htop` box `(20, 3)` `18x6`.
    assert_rows_from_row_two(
        &render_buffer,
        &[
            format!("│  ⣿⣿{}[ ]  –  ✕  │", " ".repeat(23)),
            "│ ┌─── vim ────────┐┌─── htop ───────┐ │".to_string(),
            format!("│ │vim{}││htop{}│ │", " ".repeat(13), " ".repeat(12)),
            format!("│ │{0}││{0}│ │", " ".repeat(16)),
            format!("│ │{0}││{0}│ │", " ".repeat(16)),
            format!("│ │{0}││{0}│ │", " ".repeat(16)),
            format!("│ └{0}┘└{0}┘ │", "─".repeat(16)),
        ],
    );
}

#[test]
fn a_stack_inside_a_floating_pane_draws_its_header_strip_and_its_open_member() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let [logs_pane_id, vim_pane_id] = [PaneId::new(), PaneId::new()];
    push_floating_pane_slot(
        &mut render_snapshot,
        build_floating_pane_slot(
            vim_pane_id,
            &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
                vec![logs_pane_id, vim_pane_id],
                1,
            )),
            LayoutMode::Tiled,
            build_cell_rect(2, 1, 36, 7),
            false,
        ),
        &[(logs_pane_id, "logs"), (vim_pane_id, "vim")],
    );
    get_pane_snapshot_mut(&mut render_snapshot, vim_pane_id).terminal_grid_view =
        Some(build_text_grid_view(3, 34, "vim"));

    let render_buffer = render_default_core_frame(&render_snapshot);

    // `logs` collapses to the header row 3, and `vim` opens in box `(2, 4)`
    // `36x5`.
    assert_rows_from_row_two(
        &render_buffer,
        &[
            format!("│  ⣿⣿{}[ ]  –  ✕  │", " ".repeat(23)),
            format!("│ ▸ logs{}[1/2] │", " ".repeat(25)),
            format!("│ ┌─── vim {}┐ │", "─".repeat(26)),
            format!("│ │vim{}│ │", " ".repeat(31)),
            format!("│ │{}│ │", " ".repeat(34)),
            format!("│ │{}│ │", " ".repeat(34)),
            format!("│ └{}┘ │", "─".repeat(34)),
        ],
    );
    assert_eq!(
        render_buffer[(3, 3)].bg,
        Theme::default().stack_header_background_color
    );
}

#[test]
fn a_zoomed_pane_inside_a_floating_pane_fills_its_tree_and_tags_its_label() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let [vim_pane_id, _] = push_vim_beside_htop_floating_pane(&mut render_snapshot, true);
    get_pane_snapshot_mut(&mut render_snapshot, vim_pane_id).terminal_grid_view =
        Some(build_text_grid_view(4, 34, "vim"));

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_rows_from_row_two(
        &render_buffer,
        &[
            format!("│  ⣿⣿{}[ ]  –  ✕  │", " ".repeat(23)),
            format!("│ ┌─── vim [fullscreen] {}┐ │", "─".repeat(13)),
            format!("│ │vim{}│ │", " ".repeat(31)),
            format!("│ │{}│ │", " ".repeat(34)),
            format!("│ │{}│ │", " ".repeat(34)),
            format!("│ │{}│ │", " ".repeat(34)),
            format!("│ └{}┘ │", "─".repeat(34)),
        ],
    );
}

#[test]
fn a_hovered_grab_handle_styles_every_pane_of_the_floating_pane_and_draws_no_placement_handle() {
    let theme = Theme::default();
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let [vim_pane_id, _] = push_vim_beside_htop_floating_pane(&mut render_snapshot, false);
    let unhovered_render_buffer = render_default_core_frame(&render_snapshot);

    let hovered_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            placement_handle_hover: Some(PlacementHandleHover::FloatingHandle {
                floating_pane_id: build_floating_pane_id(vim_pane_id),

                is_pinned: false,
            }),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );

    for row_index in 2..9 {
        assert_eq!(
            format_rendered_cells(&hovered_render_buffer, row_index, 0..40),
            format_rendered_cells(&unhovered_render_buffer, row_index, 0..40),
            "row {row_index}"
        );
    }
    // `(3, 2)` is the grab handle, `(2, 3)` the top-left corner of `vim` and
    // `(20, 3)` that of `htop`.
    for chrome_cell in [(3, 2), (2, 3), (20, 3)] {
        assert_eq!(hovered_render_buffer[chrome_cell].fg, theme.accent_color);
        assert_eq!(hovered_render_buffer[chrome_cell].modifier, Modifier::BOLD);
    }
    for content_cell in [(3, 4), (21, 4)] {
        assert_eq!(
            hovered_render_buffer[content_cell].bg,
            Color::Rgb(0x3a, 0x3a, 0x3a)
        );
        assert_eq!(unhovered_render_buffer[content_cell].bg, Color::Reset);
    }
}

#[test]
fn a_floating_pane_whose_every_pane_is_suppressed_draws_its_strip_and_the_too_small_text() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let pane_id = PaneId::new();
    let mut floating_pane_slot =
        build_one_pane_floating_pane_slot(pane_id, build_cell_rect(2, 1, 36, 7), false);
    let layout_size = floating_pane_slot.layout_area.solved_size;
    floating_pane_slot.layout_area = LayoutAreaSnapshot::from_layout_solve(
        &solve_layout_with_sizing(
            &LayoutNode::Pane(pane_id),
            Rect::from_size_at_origin(layout_size),
            PaneSizing {
                minimum_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                ..PaneSizing::default()
            },
        ),
        layout_size,
        LayoutMode::Tiled,
        0,
    );
    push_floating_pane_slot(
        &mut render_snapshot,
        floating_pane_slot,
        &[(pane_id, "htop")],
    );

    let render_buffer = render_default_core_frame(&render_snapshot);

    // The text sits on row 6, the middle of the six rows below the strip.
    assert_rows_from_row_two(
        &render_buffer,
        &[
            format!("│  ⣿⣿{}[ ]  –  ✕  │", " ".repeat(23)),
            format!("│ {} │", " ".repeat(36)),
            format!("│ {} │", " ".repeat(36)),
            format!("│ {} │", " ".repeat(36)),
            "│ Terminal too small — enlarge window  │".to_string(),
            format!("│ {} │", " ".repeat(36)),
            format!("│ {} │", " ".repeat(36)),
        ],
    );
}

#[test]
fn a_one_row_floating_pane_draws_only_its_strip() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let render_buffer_without_floating_pane = render_default_core_frame(&render_snapshot);
    push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 1), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 2, 0..40),
        format!("│  ⣿⣿{}[ ]  –  ✕ {}│", " ".repeat(7), " ".repeat(17))
    );
    for row_index in 3..12 {
        assert_eq!(
            format_rendered_cells(&render_buffer, row_index, 0..40),
            format_rendered_cells(&render_buffer_without_floating_pane, row_index, 0..40),
            "row {row_index}"
        );
    }
}

#[test]
fn a_handle_hover_that_names_a_pane_of_the_other_kind_or_pin_state_changes_no_cell() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots[0]
        .pane_id;
    let floating_slot_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    render_snapshot.client_snapshot.focused_pane_id = None;
    let unhovered_render_buffer = render_default_core_frame(&render_snapshot);

    for placement_handle_hover in [
        PlacementHandleHover::FloatingHandle {
            floating_pane_id: build_floating_pane_id(tiled_pane_id),

            is_pinned: false,
        },
        PlacementHandleHover::FloatingHandle {
            floating_pane_id: build_floating_pane_id(floating_slot_pane_id),

            is_pinned: true,
        },
        PlacementHandleHover::TiledTopBorder {
            pane_id: floating_slot_pane_id,
        },
        PlacementHandleHover::TiledHandle {
            pane_id: floating_slot_pane_id,
        },
    ] {
        let render_buffer = render_core_frame(
            &render_snapshot,
            ViewerChrome {
                placement_handle_hover: Some(placement_handle_hover),
                ..ViewerChrome::default()
            },
            ImageRenderMode::Placeholder,
        );
        assert_eq!(
            render_buffer, unhovered_render_buffer,
            "{placement_handle_hover:?}"
        );
    }
}

#[test]
fn a_wide_glyph_cut_by_a_floating_pane_is_blanked_and_one_that_ends_before_the_box_stays() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots[0]
        .pane_id;
    let mut glyph_style = TerminalStyle::default();
    glyph_style.set_bold(true);
    let mut grid = Grid::build_blank(8, 38, TerminalStyle::default());
    // Grid cell `(4, 4)` is frame cell `(5, 6)`: `世` covers columns `5` and `6`.
    *grid.get_cell_mut(4, 4).expect("the glyph fits the grid") =
        Cell::from_character('世', 2, glyph_style);
    *grid.get_cell_mut(4, 5).expect("the glyph fits the grid") =
        Cell::from_character(' ', 0, glyph_style);
    // Grid cell `(5, 3)` is frame cell `(4, 7)`: `世ﾞ` measures 3 columns in a
    // 2-column cell, so it draws `世` in columns `4` and `5`, left of the box.
    let mut marked_glyph_cell = Cell::from_character('世', 2, glyph_style);
    marked_glyph_cell.push_combining('\u{FF9E}');
    *grid.get_cell_mut(5, 3).expect("the glyph fits the grid") = marked_glyph_cell;
    *grid.get_cell_mut(5, 4).expect("the glyph fits the grid") =
        Cell::from_character(' ', 0, glyph_style);
    get_pane_snapshot_mut(&mut render_snapshot, tiled_pane_id).terminal_grid_view =
        Some(GridView::from_grid(&grid, 0));
    let previous_buffer = render_default_core_frame(&render_snapshot);
    // The floating pane's left border is column `6`.
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(previous_buffer[(5, 6)].symbol(), "世");
    assert_eq!(previous_buffer[(4, 7)].symbol(), "世");
    assert_eq!(render_buffer[(5, 6)].symbol(), " ");
    assert_eq!(render_buffer[(5, 6)].modifier, Modifier::BOLD);
    assert_eq!(render_buffer[(4, 7)].symbol(), "世");
    for row_index in [6, 7] {
        assert_eq!(render_buffer[(6, row_index)].symbol(), "│");
    }
    let written_cells: Vec<(u16, u16, &str)> = previous_buffer
        .diff(&render_buffer)
        .into_iter()
        .filter(|(column_index, row_index, _)| (6..=7).contains(row_index) && *column_index <= 6)
        .map(|(column_index, row_index, written_cell)| {
            (column_index, row_index, written_cell.symbol())
        })
        .collect();
    assert_eq!(written_cells, [(5, 6, " "), (6, 6, "│"), (6, 7, "│")]);
}

#[test]
fn the_khmer_sign_beyyal_beside_a_floating_pane_stays_and_a_wide_glyph_after_it_is_blanked() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots[0]
        .pane_id;
    let mut grid = Grid::build_blank(8, 38, TerminalStyle::default());
    // Grid cell `(4, 4)` is frame cell `(5, 6)`: `U+17D8` covers column `5`.
    *grid.get_cell_mut(4, 4).expect("the glyph fits the grid") =
        Cell::from_character('\u{17D8}', 1, TerminalStyle::default());
    // Grid cell `(5, 2)` is frame cell `(3, 7)`: `U+17D8` covers column `3`,
    // and `世` at frame cell `(5, 7)` covers columns `5` and `6`.
    *grid.get_cell_mut(5, 2).expect("the glyph fits the grid") =
        Cell::from_character('\u{17D8}', 1, TerminalStyle::default());
    *grid.get_cell_mut(5, 4).expect("the glyph fits the grid") =
        Cell::from_character('世', 2, TerminalStyle::default());
    *grid.get_cell_mut(5, 5).expect("the glyph fits the grid") =
        Cell::from_character(' ', 0, TerminalStyle::default());
    get_pane_snapshot_mut(&mut render_snapshot, tiled_pane_id).terminal_grid_view =
        Some(GridView::from_grid(&grid, 0));
    let previous_buffer = render_default_core_frame(&render_snapshot);
    // The floating pane's left border is column `6`.
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(render_buffer[(5, 6)].symbol(), "\u{17D8}");
    assert_eq!(render_buffer[(3, 7)].symbol(), "\u{17D8}");
    assert_eq!(previous_buffer[(5, 7)].symbol(), "世");
    assert_eq!(render_buffer[(5, 7)].symbol(), " ");
    let written_cells: Vec<(u16, u16, &str)> = previous_buffer
        .diff(&render_buffer)
        .into_iter()
        .filter(|(column_index, row_index, _)| (6..=7).contains(row_index) && *column_index <= 6)
        .map(|(column_index, row_index, written_cell)| {
            (column_index, row_index, written_cell.symbol())
        })
        .collect();
    assert_eq!(written_cells, [(6, 6, "│"), (5, 7, " "), (6, 7, "│")]);
}

#[test]
fn a_floating_pane_label_takes_one_column_for_the_khmer_sign_beyyal() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    // A `9`-column box at frame `(2, 7)` has a 3-column label area: frame
    // columns `6` to `8` of row `8`.
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(2, 6, 9, 4),
        "\u{17D8}x",
    );
    // A `30`-column box at frame `(2, 2)` has a 24-column label area from
    // frame column `6` of row `3`.
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(2, 1, 30, 4),
        "\u{17D8}xy",
    );

    let render_buffer = render_default_core_frame(&render_snapshot);

    let narrow_label_cells: Vec<&str> = (6..11)
        .map(|column_index| render_buffer[(column_index, 8)].symbol())
        .collect();
    assert_eq!(narrow_label_cells, [" ", "\u{17D8}", "x", "─", "┐"]);
    assert_eq!(render_buffer[(7, 8)].cell_width(), 1);
    let wide_label_cells: Vec<&str> = (6..12)
        .map(|column_index| render_buffer[(column_index, 3)].symbol())
        .collect();
    assert_eq!(wide_label_cells, [" ", "\u{17D8}", "x", "y", " ", "─"]);
}

#[test]
fn floating_panes_draw_back_to_front_in_slot_order() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "lower");
    push_floating_pane(&mut render_snapshot, build_cell_rect(12, 3, 20, 6), "upper");

    // `(21, 7)` is the bottom-right corner of the lower pane box and a content
    // cell of the upper pane box.
    let render_buffer = render_default_core_frame(&render_snapshot);
    assert_eq!(render_buffer[(21, 7)].symbol(), " ");

    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .reverse();
    let render_buffer = render_default_core_frame(&render_snapshot);
    assert_eq!(render_buffer[(21, 7)].symbol(), "┘");
}

#[test]
fn a_floating_pane_draws_over_the_letterbox_margin_at_its_own_cells() {
    let mut render_snapshot = build_tiled_render_snapshot(
        Size {
            column_count: 60,
            row_count: 12,
        },
        Size {
            column_count: 40,
            row_count: 8,
        },
    );
    push_floating_pane(&mut render_snapshot, build_cell_rect(0, 0, 8, 5), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    // The tiled layout starts at `(10, 2)`; the floating pane starts at the
    // pane area's origin `(0, 1)`, and its pane box one row below.
    assert_eq!(render_buffer[(10, 2)].symbol(), "┌");
    assert_eq!(render_buffer[(1, 1)].symbol(), "⣿");
    assert_eq!(render_buffer[(0, 1)].bg, Color::Reset);
    assert_eq!(render_buffer[(0, 2)].symbol(), "┌");
    assert_eq!(render_buffer[(3, 3)].bg, Color::Reset);
    assert_eq!(render_buffer[(9, 6)].bg, Theme::default().letterbox_color);
}

#[test]
fn a_floating_pane_draws_over_a_fullscreen_tiled_pane() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots[0]
        .pane_id;
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: tiled_pane_id,
    };
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 3, 6..32),
        format!(" ⣿⣿{}[ ]  –  ✕ ", " ".repeat(13))
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..32),
        format!("┌─── htop {}┐", "─".repeat(15))
    );
}

#[test]
fn no_floating_pane_draws_under_the_too_small_overlay() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .is_every_pane_suppressed = true;
    let render_buffer_without_floating_pane = render_default_core_frame(&render_snapshot);
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    assert_eq!(
        render_default_core_frame(&render_snapshot),
        render_buffer_without_floating_pane
    );
}

#[test]
fn no_floating_pane_draws_while_pane_placement_shows() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let placement_viewer_chrome = ViewerChrome {
        is_pane_placement_visible: true,
        ..ViewerChrome::default()
    };
    let render_buffer_without_floating_pane = render_core_frame(
        &render_snapshot,
        placement_viewer_chrome,
        ImageRenderMode::Placeholder,
    );
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    assert_eq!(
        render_core_frame(
            &render_snapshot,
            placement_viewer_chrome,
            ImageRenderMode::Placeholder
        ),
        render_buffer_without_floating_pane
    );
}

#[test]
fn a_floating_pane_takes_the_border_style_of_its_focus_and_the_pointer() {
    let theme = Theme::default();
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_slot_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    // `(7, 3)` is the grab handle and `(6, 4)` the top-left corner of the
    // pane box.
    let render_buffer = render_default_core_frame(&render_snapshot);
    for chrome_cell in [(7, 3), (6, 4)] {
        assert_eq!(render_buffer[chrome_cell].fg, theme.unfocused_border_color);
        assert_eq!(render_buffer[chrome_cell].modifier, Modifier::empty());
    }

    let hovered_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            hovered_pane_id: Some(floating_slot_pane_id),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );
    for chrome_cell in [(7, 3), (6, 4)] {
        assert_eq!(
            hovered_render_buffer[chrome_cell].fg,
            theme.hover_border_color
        );
        assert_eq!(hovered_render_buffer[chrome_cell].modifier, Modifier::BOLD);
    }

    let handle_hovered_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            placement_handle_hover: Some(PlacementHandleHover::FloatingHandle {
                floating_pane_id: build_floating_pane_id(floating_slot_pane_id),

                is_pinned: false,
            }),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );
    for chrome_cell in [(7, 3), (6, 4)] {
        assert_eq!(
            handle_hovered_render_buffer[chrome_cell].fg,
            theme.accent_color
        );
        assert_eq!(
            handle_hovered_render_buffer[chrome_cell].modifier,
            Modifier::BOLD
        );
    }
    assert_eq!(
        handle_hovered_render_buffer[(7, 6)].bg,
        Color::Rgb(0x3a, 0x3a, 0x3a)
    );
    assert_eq!(render_buffer[(7, 6)].bg, Color::Reset);

    render_snapshot.client_snapshot.focused_pane_id = Some(floating_slot_pane_id);
    let focused_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            hovered_pane_id: Some(floating_slot_pane_id),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );
    for chrome_cell in [(7, 3), (6, 4)] {
        assert_eq!(
            focused_render_buffer[chrome_cell].fg,
            theme.focused_border_color
        );
        assert_eq!(focused_render_buffer[chrome_cell].modifier, Modifier::BOLD);
    }
}

#[test]
fn a_scrolled_back_floating_pane_shows_its_scroll_position_in_its_bottom_border() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_slot_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "htop");
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_slot_pane_id);
    floating_pane_snapshot.terminal_grid_view = Some(GridView {
        view_row_offset: 5,
        ..build_text_grid_view(3, 18, "")
    });
    floating_pane_snapshot
        .scrollback_metadata
        .retained_line_count = 200;

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 7, 2..22),
        format!("└{} 5/200 ┘", "─".repeat(11))
    );
}

#[test]
fn a_long_label_is_cut_to_the_label_cells() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(6, 2, 12, 6),
        "abcdefghij",
    );

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..18),
        "┌─── abcde─┐"
    );
}

#[test]
fn a_narrow_floating_pane_drops_the_pin_toggle_then_minimize_then_close() {
    for (outer_column_count, expected_strip) in [
        (3, " ⣿⣿"),
        (6, " ⣿⣿   "),
        (7, " ⣿⣿  ✕ "),
        (10, " ⣿⣿  –  ✕ "),
        (14, " ⣿⣿      –  ✕ "),
        (15, " ⣿⣿  [ ]  –  ✕ "),
    ] {
        let mut render_snapshot = build_forty_by_twelve_render_snapshot();
        push_floating_pane(
            &mut render_snapshot,
            build_cell_rect(0, 0, outer_column_count, 6),
            "htop",
        );

        let render_buffer = render_default_core_frame(&render_snapshot);

        assert_eq!(
            format_rendered_cells(&render_buffer, 1, 0..outer_column_count),
            expected_strip,
            "{outer_column_count} columns"
        );
    }
}

#[test]
fn an_untitled_floating_pane_draws_a_plain_top_border() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..32),
        format!("┌{}┐", "─".repeat(24))
    );
}

#[test]
fn a_pinned_floating_pane_shows_the_pinned_toggle() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    render_snapshot.session_snapshot.floating_pane_slots[0].is_pinned = true;

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(render_buffer[(22, 3)].symbol(), "[");
    assert_eq!(render_buffer[(23, 3)].symbol(), "⚪");
    assert_eq!(render_buffer[(25, 3)].symbol(), "]");
    assert_eq!(render_buffer[(27, 3)].symbol(), "–");
}

#[test]
fn a_floating_panes_image_shows_the_unavailable_text_in_its_content() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_slot_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_slot_pane_id);
    floating_pane_snapshot.terminal_grid_view = Some(build_text_grid_view(3, 24, ""));
    floating_pane_snapshot.image_placement_snapshots = vec![
        ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 12, 1).expect("valid placement"),
    ];

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 5, 6..32),
        "│terminal ima            │"
    );
}

#[test]
fn an_image_in_a_split_floating_pane_is_cut_to_its_own_pane() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let [vim_pane_id, _] = push_vim_beside_htop_floating_pane(&mut render_snapshot, false);
    let vim_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, vim_pane_id);
    vim_pane_snapshot.terminal_grid_view = Some(build_text_grid_view(4, 16, ""));
    vim_pane_snapshot.image_placement_snapshots =
        vec![
            ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 30, 1)
                .expect("valid placement"),
        ];

    let render_buffer = render_default_core_frame(&render_snapshot);

    // `vim` shows content columns `3` to `18`; `htop`'s box starts at `20`.
    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 2..38),
        format!("│terminal image u││{}│", " ".repeat(16))
    );
}

#[test]
fn a_higher_floating_pane_covers_a_lower_ones_unavailable_text_in_placeholder_mode() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let lower_floating_slot_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 26, 6), "htop");
    let lower_floating_pane_snapshot =
        get_pane_snapshot_mut(&mut render_snapshot, lower_floating_slot_pane_id);
    lower_floating_pane_snapshot.terminal_grid_view = Some(build_text_grid_view(3, 24, ""));
    lower_floating_pane_snapshot.image_placement_snapshots = vec![
        ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 24, 1).expect("valid placement"),
    ];
    push_floating_pane(&mut render_snapshot, build_cell_rect(14, 3, 12, 5), "");

    let render_buffer = render_default_core_frame(&render_snapshot);

    // The lower pane's image text covers `(3, 4)` to `(26, 4)`; the higher
    // pane's strip covers columns `14` to `25` of that row.
    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 2..28),
        "│terminal im ⣿⣿    –  ✕ b│"
    );
}

#[test]
fn a_floating_pane_covers_a_tiled_images_unavailable_text_in_placeholder_mode_and_hides_it_in_native_mode(
) {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_snapshot = &mut render_snapshot.pane_snapshots[0];
    tiled_pane_snapshot.terminal_grid_view = Some(build_text_grid_view(8, 38, ""));
    tiled_pane_snapshot.image_placement_snapshots = vec![
        ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 30, 1).expect("valid placement"),
    ];
    push_floating_pane(&mut render_snapshot, build_cell_rect(10, 0, 10, 6), "htop");

    // The image covers `(1, 2)` to `(30, 2)`; the floating pane's box covers
    // columns `10` to `19` of that row with its top border.
    let placeholder_render_buffer = render_default_core_frame(&render_snapshot);
    assert_eq!(
        format_rendered_cells(&placeholder_render_buffer, 2, 1..31),
        "terminal ┌─── hto─┐ailable    "
    );

    let native_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome::default(),
        ImageRenderMode::Native,
    );
    assert_eq!(
        format_rendered_cells(&native_render_buffer, 2, 1..31),
        "         ┌─── hto─┐           "
    );
}
