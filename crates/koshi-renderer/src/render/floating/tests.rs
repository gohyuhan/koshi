//! Tests for floating pane compositing. Each floating pane this viewer draws
//! is placed at the pane area's origin with no letterbox offset, and its
//! visible content is cut to the pane area. The panes draw back to front over
//! the tiled panes and the letterbox, each with its border, titlebar,
//! separator, cells, border style, grab handle hover tint, a hovered grab
//! handle beside a whole title, unavailable-image text and scroll position.
//! A glyph two columns wide that starts in the column
//! left of a floating pane turns blank, and the frame diff writes the pane's
//! left border beside it. `U+17D8` there takes 1 column and stays, and in a
//! title it takes 1 column. While pane placement shows, only the placement
//! source pane is placed and drawn, and only while the active tab holds no
//! slot of it. No floating pane is placed or drawn while every pane is
//! suppressed. The pane a move draft names is placed at the draft's top-left
//! cell, moved inside the pane area.

use super::*;

use ratatui::buffer::CellWidth;
use ratatui::style::{Color, Modifier};

use koshi_core::geometry::Point;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style as TerminalStyle;
use koshi_test_support::fixtures::build_cell_rect;

use crate::images::ImageRenderMode;
use crate::render::tests::{format_rendered_cells, get_pane_snapshot_mut};
use crate::render::{build_frame_geometry, render_frame};
use crate::snapshot::{
    ClientSnapshot, CursorSnapshot, FloatingPaneMoveDraft, GridView, ImagePlacementSnapshot,
    KeymapHints, PaneSlot, PaneSnapshot, PlacementHandleHover, ScrollbackMetadata, SessionSnapshot,
    TabMetadata, TabSnapshot,
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
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "s".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "t".to_string(),
                pane_slots: vec![PaneSlot {
                    pane_id: tiled_pane_id,
                    outer_rect: tiled_outer_rect,
                    content_rect: Some(tiled_outer_rect.compute_inner_with_border()),
                    is_visible: true,
                    is_suppressed: false,
                }],
                tab_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
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

/// Put a floating pane with outer rect `outer_rect`, in pane-area cells, on
/// top of the frame's floating panes, its content titled `pane_title` (none
/// when empty) with no grid. Returns its pane id.
fn push_floating_pane(
    render_snapshot: &mut RenderSnapshot,
    outer_rect: Rect,
    pane_title: &str,
) -> PaneId {
    let pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            pane_id, outer_rect, false,
        ));
    let mut pane_snapshot = build_pane_snapshot(pane_id);
    pane_snapshot.pane_title = (!pane_title.is_empty()).then(|| pane_title.to_string());
    render_snapshot.pane_snapshots.push(pane_snapshot);
    pane_id
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

/// [`list_placed_floating_panes`] for `render_snapshot` over the compiled-in
/// region solve of its client's viewport.
fn list_core_placed_floating_panes(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
) -> Vec<PlacedFloatingPane> {
    let viewport_size = render_snapshot.client_snapshot.viewport_size;
    list_placed_floating_panes(
        render_snapshot,
        &CommittedRegions::build_core(viewport_size, 0),
        viewer_chrome,
        RatatuiRect::new(0, 0, viewport_size.column_count, viewport_size.row_count),
    )
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

    let placed_floating_panes =
        list_core_placed_floating_panes(&render_snapshot, ViewerChrome::default());

    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            floating_pane_slot: render_snapshot.session_snapshot.floating_pane_slots[0],
            outer_area: RatatuiRect::new(150, 5, 40, 12),
            content_area: RatatuiRect::new(151, 8, 38, 8),
            visible_content_area: RatatuiRect::new(151, 8, 38, 8),
        }]
    );
}

#[test]
fn a_floating_pane_past_the_pane_area_keeps_its_content_area_and_cuts_its_visible_one() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(30, 4, 20, 10), "htop");

    let placed_floating_panes =
        list_core_placed_floating_panes(&render_snapshot, ViewerChrome::default());

    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            floating_pane_slot: render_snapshot.session_snapshot.floating_pane_slots[0],
            outer_area: RatatuiRect::new(30, 5, 20, 10),
            content_area: RatatuiRect::new(31, 8, 18, 6),
            visible_content_area: RatatuiRect::new(31, 8, 9, 3),
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
            pane_id: moved_pane_id,
            top_left_cell: Point { column: 15, row: 4 },
        }),
        ..ViewerChrome::default()
    };

    let placed_floating_panes = list_core_placed_floating_panes(&render_snapshot, viewer_chrome);

    assert_eq!(
        placed_floating_panes,
        [
            PlacedFloatingPane {
                floating_pane_slot: FloatingPaneSlot::from_outer_rect(
                    moved_pane_id,
                    build_cell_rect(15, 4, 20, 6),
                    false,
                ),
                outer_area: RatatuiRect::new(15, 5, 20, 6),
                content_area: RatatuiRect::new(16, 8, 18, 2),
                visible_content_area: RatatuiRect::new(16, 8, 18, 2),
            },
            PlacedFloatingPane {
                floating_pane_slot: render_snapshot.session_snapshot.floating_pane_slots[1],
                outer_area: RatatuiRect::new(4, 4, 20, 6),
                content_area: RatatuiRect::new(5, 7, 18, 2),
                visible_content_area: RatatuiRect::new(5, 7, 18, 2),
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
            pane_id: pinned_pane_id,
            top_left_cell: Point { column: 15, row: 4 },
        }),
        ..ViewerChrome::default()
    };

    let placed_floating_panes = list_core_placed_floating_panes(&render_snapshot, viewer_chrome);

    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            floating_pane_slot: FloatingPaneSlot::from_outer_rect(
                pinned_pane_id,
                build_cell_rect(2, 1, 20, 6),
                true,
            ),
            outer_area: RatatuiRect::new(2, 2, 20, 6),
            content_area: RatatuiRect::new(3, 5, 18, 2),
            visible_content_area: RatatuiRect::new(3, 5, 18, 2),
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
            pane_id: moved_pane_id,
            top_left_cell: Point { column: 35, row: 9 },
        }),
        ..ViewerChrome::default()
    };

    let placed_floating_panes = list_core_placed_floating_panes(&render_snapshot, viewer_chrome);

    // The 20x6 pane in the 40x10 pane area, which starts at row 1, keeps its
    // top-left cell at (20, 4) at most.
    assert_eq!(
        placed_floating_panes,
        [PlacedFloatingPane {
            floating_pane_slot: FloatingPaneSlot::from_outer_rect(
                moved_pane_id,
                build_cell_rect(20, 4, 20, 6),
                false,
            ),
            outer_area: RatatuiRect::new(20, 5, 20, 6),
            content_area: RatatuiRect::new(21, 8, 18, 2),
            visible_content_area: RatatuiRect::new(21, 8, 18, 2),
        }]
    );
}

#[test]
fn no_floating_pane_is_placed_while_pane_placement_shows() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let placed_floating_panes = list_core_placed_floating_panes(
        &render_snapshot,
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

    let list_placed_pane_ids = |render_snapshot: &RenderSnapshot| -> Vec<PaneId> {
        list_core_placed_floating_panes(render_snapshot, placement_viewer_chrome)
            .iter()
            .map(|placed_floating_pane| placed_floating_pane.floating_pane_slot.pane_id)
            .collect()
    };
    assert_eq!(list_placed_pane_ids(&render_snapshot), vec![source_pane_id]);

    // The preview draws the source pane in the active tab's layout.
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
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
        .is_every_pane_suppressed = true;
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let placed_floating_panes =
        list_core_placed_floating_panes(&render_snapshot, ViewerChrome::default());

    assert_eq!(placed_floating_panes, []);
}

#[test]
fn a_floating_pane_draws_its_box_titlebar_separator_and_cells_over_the_tiled_pane() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    get_pane_snapshot_mut(&mut render_snapshot, floating_pane_id).terminal_grid_view =
        Some(build_text_grid_view(2, 24, "top"));

    let render_buffer = render_default_core_frame(&render_snapshot);

    let expected_rows = [
        format!("│{}│", " ".repeat(38)),
        format!("│     ┌{}┐       │", "─".repeat(24)),
        "│     │ ⣿⣿  htop    [ ]  –  ✕  │       │".to_string(),
        format!("│     ├{}┤       │", "─".repeat(24)),
        format!("│     │top{}│       │", " ".repeat(21)),
        format!("│     │{}│       │", " ".repeat(24)),
        format!("│     └{}┘       │", "─".repeat(24)),
        format!("│{}│", " ".repeat(38)),
    ];
    for (row_index, expected_row) in (2..).zip(expected_rows) {
        assert_eq!(
            format_rendered_cells(&render_buffer, row_index, 0..40),
            expected_row,
            "row {row_index}"
        );
    }
}

#[test]
fn a_hovered_grab_handle_leaves_the_floating_pane_title_whole() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            placement_handle_hover: Some(PlacementHandleHover::FloatingHandle {
                pane_id: floating_pane_id,
                is_pinned: false,
            }),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );

    assert_eq!(
        format_rendered_cells(&render_buffer, 3, 6..32),
        format!("┌{}┐", "─".repeat(24))
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..32),
        "│ ⣿⣿  htop    [ ]  –  ✕  │"
    );
}

#[test]
fn a_handle_hover_that_names_a_pane_of_the_other_kind_or_pin_state_changes_no_cell() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .pane_id;
    let floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    render_snapshot.client_snapshot.focused_pane_id = None;
    let unhovered_render_buffer = render_default_core_frame(&render_snapshot);

    for placement_handle_hover in [
        PlacementHandleHover::FloatingHandle {
            pane_id: tiled_pane_id,
            is_pinned: false,
        },
        PlacementHandleHover::FloatingHandle {
            pane_id: floating_pane_id,
            is_pinned: true,
        },
        PlacementHandleHover::TiledTopBorder {
            pane_id: floating_pane_id,
        },
        PlacementHandleHover::TiledHandle {
            pane_id: floating_pane_id,
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
fn a_floating_pane_title_takes_one_column_for_the_khmer_sign_beyyal() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    // A `22`-column box at frame `(2, 2)` has a 2-column title area: frame
    // columns `8` and `9` of row `3`.
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(2, 1, 22, 5),
        "\u{17D8}x",
    );
    // A `30`-column box at frame `(2, 7)` has a 10-column title area from
    // frame column `8` of row `8`.
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(2, 6, 30, 4),
        "\u{17D8}xy",
    );

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(render_buffer[(8, 3)].symbol(), "\u{17D8}");
    assert_eq!(render_buffer[(8, 3)].cell_width(), 1);
    assert_eq!(render_buffer[(9, 3)].symbol(), "x");
    let wide_title_cells: Vec<&str> = (8..12)
        .map(|column_index| render_buffer[(column_index, 8)].symbol())
        .collect();
    assert_eq!(wide_title_cells, ["\u{17D8}", "x", "y", " "]);
}

#[test]
fn floating_panes_draw_back_to_front_in_slot_order() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "lower");
    push_floating_pane(&mut render_snapshot, build_cell_rect(12, 3, 20, 6), "upper");

    // `(21, 4)` is on the upper pane's top border and the right end of the
    // lower pane's separator.
    let render_buffer = render_default_core_frame(&render_snapshot);
    assert_eq!(render_buffer[(21, 4)].symbol(), "─");

    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .reverse();
    let render_buffer = render_default_core_frame(&render_snapshot);
    assert_eq!(render_buffer[(21, 4)].symbol(), "┤");
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
    // pane area's origin `(0, 1)`.
    assert_eq!(render_buffer[(10, 2)].symbol(), "┌");
    assert_eq!(render_buffer[(0, 1)].symbol(), "┌");
    assert_eq!(render_buffer[(0, 1)].bg, Color::Reset);
    assert_eq!(render_buffer[(3, 3)].bg, Color::Reset);
    assert_eq!(render_buffer[(9, 6)].bg, Theme::default().letterbox_color);
}

#[test]
fn a_floating_pane_draws_over_a_fullscreen_tiled_pane() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let tiled_pane_id = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .pane_id;
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: tiled_pane_id,
    };
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..32),
        "│ ⣿⣿  htop    [ ]  –  ✕  │"
    );
}

#[test]
fn no_floating_pane_draws_under_the_too_small_overlay() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
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
    let floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");

    // `(6, 3)` is the top-left corner and `(8, 4)` the grab handle.
    let render_buffer = render_default_core_frame(&render_snapshot);
    for chrome_cell in [(6, 3), (8, 4)] {
        assert_eq!(render_buffer[chrome_cell].fg, theme.unfocused_border_color);
        assert_eq!(render_buffer[chrome_cell].modifier, Modifier::empty());
    }

    let hovered_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            hovered_pane_id: Some(floating_pane_id),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );
    for chrome_cell in [(6, 3), (8, 4)] {
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
                pane_id: floating_pane_id,
                is_pinned: false,
            }),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );
    for chrome_cell in [(6, 3), (8, 4)] {
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

    render_snapshot.client_snapshot.focused_pane_id = Some(floating_pane_id);
    let focused_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome {
            hovered_pane_id: Some(floating_pane_id),
            ..ViewerChrome::default()
        },
        ImageRenderMode::Placeholder,
    );
    for chrome_cell in [(6, 3), (8, 4)] {
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
    let floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 20, 6), "htop");
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_pane_id);
    floating_pane_snapshot.terminal_grid_view = Some(GridView {
        view_row_offset: 5,
        ..build_text_grid_view(2, 18, "")
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
fn a_long_title_is_cut_to_the_title_cells() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(6, 2, 26, 6),
        "abcdefghij",
    );

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..32),
        "│ ⣿⣿  abcdef  [ ]  –  ✕  │"
    );
}

#[test]
fn a_narrow_floating_pane_drops_the_pin_toggle_then_minimize_then_close() {
    for (outer_column_count, expected_titlebar) in [
        (6, "│ ⣿⣿ │"),
        (10, "│ ⣿⣿  ✕  │"),
        (13, "│ ⣿⣿  –  ✕  │"),
        (18, "│ ⣿⣿  [ ]  –  ✕  │"),
        (21, "│ ⣿⣿  h  [ ]  –  ✕  │"),
    ] {
        let mut render_snapshot = build_forty_by_twelve_render_snapshot();
        push_floating_pane(
            &mut render_snapshot,
            build_cell_rect(0, 0, outer_column_count, 6),
            "htop",
        );

        let render_buffer = render_default_core_frame(&render_snapshot);

        assert_eq!(
            format_rendered_cells(&render_buffer, 2, 0..outer_column_count),
            expected_titlebar,
            "{outer_column_count} columns"
        );
    }
}

#[test]
fn an_untitled_floating_pane_leaves_its_title_cells_blank() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 4, 6..32),
        "│ ⣿⣿          [ ]  –  ✕  │"
    );
}

#[test]
fn a_pinned_floating_pane_shows_the_pinned_toggle() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    render_snapshot.session_snapshot.floating_pane_slots[0].is_pinned = true;

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(render_buffer[(20, 4)].symbol(), "[");
    assert_eq!(render_buffer[(21, 4)].symbol(), "⚪");
    assert_eq!(render_buffer[(23, 4)].symbol(), "]");
    assert_eq!(render_buffer[(25, 4)].symbol(), "–");
}

#[test]
fn a_floating_pane_shorter_than_four_rows_draws_a_plain_box() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 8, 3), "htop");

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(format_rendered_cells(&render_buffer, 2, 2..10), "┌──────┐");
    assert_eq!(format_rendered_cells(&render_buffer, 3, 2..10), "│      │");
    assert_eq!(format_rendered_cells(&render_buffer, 4, 2..10), "└──────┘");
}

#[test]
fn a_floating_panes_image_shows_the_unavailable_text_in_its_content() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(6, 2, 26, 6), "htop");
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_pane_id);
    floating_pane_snapshot.terminal_grid_view = Some(build_text_grid_view(2, 24, ""));
    floating_pane_snapshot.image_placement_snapshots = vec![
        ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 12, 1).expect("valid placement"),
    ];

    let render_buffer = render_default_core_frame(&render_snapshot);

    assert_eq!(
        format_rendered_cells(&render_buffer, 6, 6..32),
        "│terminal ima            │"
    );
}

#[test]
fn a_higher_floating_pane_covers_a_lower_ones_unavailable_text_in_placeholder_mode() {
    let mut render_snapshot = build_forty_by_twelve_render_snapshot();
    let lower_floating_pane_id =
        push_floating_pane(&mut render_snapshot, build_cell_rect(2, 1, 26, 6), "htop");
    let lower_floating_pane_snapshot =
        get_pane_snapshot_mut(&mut render_snapshot, lower_floating_pane_id);
    lower_floating_pane_snapshot.terminal_grid_view = Some(build_text_grid_view(2, 24, ""));
    lower_floating_pane_snapshot.image_placement_snapshots = vec![
        ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 24, 1).expect("valid placement"),
    ];
    push_floating_pane(&mut render_snapshot, build_cell_rect(14, 3, 12, 5), "");

    let render_buffer = render_default_core_frame(&render_snapshot);

    // The lower pane's image text covers `(3, 5)` to `(26, 5)`; the higher
    // pane's titlebar covers columns `14` to `25` of that row.
    assert_eq!(
        format_rendered_cells(&render_buffer, 5, 2..28),
        "│terminal im│ ⣿⣿    ✕  │b│"
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

    // The image covers `(1, 2)` to `(30, 2)`; the floating pane covers columns
    // `10` to `19` of that row with its titlebar.
    let placeholder_render_buffer = render_default_core_frame(&render_snapshot);
    assert_eq!(
        format_rendered_cells(&placeholder_render_buffer, 2, 1..31),
        "terminal │ ⣿⣿  ✕  │ailable    "
    );

    let native_render_buffer = render_core_frame(
        &render_snapshot,
        ViewerChrome::default(),
        ImageRenderMode::Native,
    );
    assert_eq!(
        format_rendered_cells(&native_render_buffer, 2, 1..31),
        "         │ ⣿⣿  ✕  │           "
    );
}
