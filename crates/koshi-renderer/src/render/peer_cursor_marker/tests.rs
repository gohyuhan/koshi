//! Tests for the peer cursor markers: the cell each one paints and its
//! colors, the thirteen states in which a viewer draws none, the layers that
//! cover one, and the rows a frame redraws when a marker comes, goes or moves.
//!
//! Most frames are the viewer Alice (index 0) over two tiled panes side by
//! side, each 30x26 with a 28x24 content area: left at buffer (1, 2) and
//! right, which Alice focuses, at (31, 2). The peer Bob (index 1) types in
//! left, whose cursor sits at row 5, column 12: buffer cell (13, 7).

use super::*;

use koshi_core::geometry::{Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::{Color as TerminalColor, Style as TerminalStyle};
use koshi_test_support::fixtures::{build_cell_rect, build_floating_pane_id};
use ratatui::style::Color;

use crate::images::ImageRenderMode;
use crate::notification::tests::build_test_notification_view;
use crate::notification::NotificationCenterEntries;
use crate::render::tests::{
    build_floating_pane_slot, build_one_pane_floating_pane_slot, build_render_snapshot,
    get_pane_snapshot_mut, render_frame_to_buffer,
};
use crate::render::{
    build_frame_geometry, get_cursor_position, list_changed_screen_rows, render_frame,
};
use crate::snapshot::{
    CommittedRegions, FloatingPane, FloatingPaneSlot, GridView, KeymapHints, PanePlacementMessage,
    PanePlacementPresentation, PeerSnapshot, SelectionSpans, ViewerChrome,
};

/// Alice's terminal: the tab strip on row 0, the hint row on row 27, and a
/// 60x26 pane area from row 1.
const VIEWPORT_SIZE: Size = Size {
    column_count: 60,
    row_count: 28,
};

/// Bob's index.
const BOB_COLOR_INDEX: u32 = 1;

/// Bob's cursor in the left pane: row 5, column 12.
const BOB_CURSOR_ROW_INDEX: u16 = 5;
const BOB_CURSOR_COLUMN_INDEX: u16 = 12;

/// The buffer cell of Bob's cursor: the left pane's content starts at (1, 2).
const BOB_MARKER_POSITION: Position = Position { x: 13, y: 7 };

/// A peer of index `color_index` whose keys reach `typed_pane_id`.
fn build_peer_snapshot(color_index: u32, typed_pane_id: Option<PaneId>) -> PeerSnapshot {
    PeerSnapshot {
        client_id: ClientId::new(),
        color_index,
        typed_pane_id,
    }
}

/// The two-pane frame the module doc describes: each pane shows a blank
/// 24x28 grid with its cursor at row 5, column 12. Returns the frame and the
/// ids of the left and the right pane.
fn build_two_pane_frame() -> (RenderSnapshot, PaneId, PaneId) {
    let [left_pane_id, right_pane_id] = [PaneId::new(), PaneId::new()];
    let mut render_snapshot = build_render_snapshot(
        "work",
        &[("shell", true)],
        &[
            (left_pane_id, build_cell_rect(0, 0, 30, 26), true),
            (right_pane_id, build_cell_rect(30, 0, 30, 26), true),
        ],
        Some(right_pane_id),
        LockMode::Normal,
        VIEWPORT_SIZE,
    );
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .solved_size = Size {
        column_count: 60,
        row_count: 26,
    };
    for pane_snapshot in &mut render_snapshot.pane_snapshots {
        pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
            &Grid::build_blank(24, 28, TerminalStyle::default()),
            0,
        ));
        pane_snapshot.cursor_snapshot.row_index = BOB_CURSOR_ROW_INDEX;
        pane_snapshot.cursor_snapshot.column_index = BOB_CURSOR_COLUMN_INDEX;
    }
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id))];
    (render_snapshot, left_pane_id, right_pane_id)
}

/// The compiled-in region solve of [`VIEWPORT_SIZE`].
fn build_core_regions() -> CommittedRegions {
    CommittedRegions::build_core(VIEWPORT_SIZE, 0)
}

/// The buffer area of [`VIEWPORT_SIZE`].
fn build_viewport_area() -> RatatuiRect {
    RatatuiRect::new(0, 0, VIEWPORT_SIZE.column_count, VIEWPORT_SIZE.row_count)
}

/// The markers of every layer of `render_snapshot` drawn with `viewer_chrome`
/// over the compiled-in regions ([`list_frame_peer_cursor_markers`]).
fn list_core_peer_cursor_markers(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
) -> Vec<PeerCursorMarker> {
    let committed_regions = build_core_regions();
    list_frame_peer_cursor_markers(
        render_snapshot,
        &build_frame_geometry(
            render_snapshot,
            &committed_regions,
            viewer_chrome,
            build_viewport_area(),
        ),
    )
}

/// `render_snapshot` painted over the compiled-in regions in the default
/// theme, with `viewer_chrome`.
fn render_core_frame(render_snapshot: &RenderSnapshot, viewer_chrome: ViewerChrome) -> Buffer {
    render_frame_to_buffer(
        render_snapshot,
        &build_core_regions(),
        &Theme::default(),
        &KeymapHints::default(),
        viewer_chrome,
    )
}

/// The rows a frame redraws from `previous_render_snapshot` to
/// `next_render_snapshot` over the compiled-in regions.
fn list_core_changed_screen_rows(
    previous_render_snapshot: &RenderSnapshot,
    next_render_snapshot: &RenderSnapshot,
) -> Option<Vec<u16>> {
    let committed_regions = build_core_regions();
    list_changed_screen_rows(
        previous_render_snapshot,
        &build_frame_geometry(
            next_render_snapshot,
            &committed_regions,
            ViewerChrome::default(),
            build_viewport_area(),
        ),
    )
}

/// Bob's marker at `position`.
fn build_bob_marker(position: Position) -> PeerCursorMarker {
    PeerCursorMarker {
        position,
        color_index: BOB_COLOR_INDEX,
    }
}

/// The text and fill colors of the cell at `position` of `render_buffer`.
fn get_cell_colors(render_buffer: &Buffer, position: Position) -> (Color, Color) {
    let marker_cell = &render_buffer[(position.x, position.y)];
    (marker_cell.fg, marker_cell.bg)
}

/// The text and fill colors of the client of index `color_index` in the
/// default theme.
fn get_client_colors(color_index: u32) -> (Color, Color) {
    let client_color = Theme::default().compute_client_color(color_index);
    (client_color.text_color, client_color.fill_color)
}

/// Add a one-pane floating pane at `outer_rect` whose pane `float_pane_id`
/// shows a blank grid of the slot's content size, with its cursor at row
/// `2`, column `3`.
fn push_one_pane_floating_pane(
    render_snapshot: &mut RenderSnapshot,
    float_pane_id: PaneId,
    outer_rect: Rect,
) {
    let floating_pane_slot = build_one_pane_floating_pane_slot(float_pane_id, outer_rect, false);
    push_floating_pane_slot(render_snapshot, floating_pane_slot, &[float_pane_id]);
}

/// Add `floating_pane_slot` as a shown floating pane, with a snapshot of a
/// blank grid for each of `pane_ids`, each with its cursor at row `2`,
/// column `3`.
fn push_floating_pane_slot(
    render_snapshot: &mut RenderSnapshot,
    floating_pane_slot: FloatingPaneSlot,
    pane_ids: &[PaneId],
) {
    render_snapshot
        .session_snapshot
        .floating_panes
        .push(FloatingPane::Shown {
            floating_pane_id: floating_pane_slot.floating_pane_id,
            pane_id: floating_pane_slot.pane_id,
        });
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(floating_pane_slot);
    for pane_id in pane_ids {
        let mut pane_snapshot = render_snapshot.pane_snapshots[0].clone();
        pane_snapshot.pane_id = *pane_id;
        pane_snapshot.cursor_snapshot.row_index = 2;
        pane_snapshot.cursor_snapshot.column_index = 3;
        render_snapshot.pane_snapshots.push(pane_snapshot);
    }
}

// --- where a marker sits, and its colors ----------------------------------

#[test]
fn a_peer_typing_in_another_pane_marks_its_cursor_cell_in_the_peers_colors() {
    let (render_snapshot, _, _) = build_two_pane_frame();

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(BOB_MARKER_POSITION)]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        get_cell_colors(&render_buffer, BOB_MARKER_POSITION),
        get_client_colors(BOB_COLOR_INDEX)
    );
    assert_eq!(render_buffer[(13, 7)].symbol(), " ");
}

#[test]
fn at_scroll_offset_zero_the_marker_is_the_cell_the_hardware_cursor_takes_in_that_pane() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let marker_positions: Vec<Position> =
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default())
            .iter()
            .map(|peer_cursor_marker| peer_cursor_marker.position)
            .collect();
    render_snapshot.client_snapshot.focused_pane_id = Some(left_pane_id);
    render_snapshot.session_snapshot.peers.clear();
    let committed_regions = build_core_regions();

    let hardware_cursor_position = get_cursor_position(&build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
    ));

    assert_eq!(hardware_cursor_position, Some(BOB_MARKER_POSITION));
    assert_eq!(marker_positions, vec![BOB_MARKER_POSITION]);
}

#[test]
fn a_view_scrolled_back_moves_the_marker_down_by_the_offset_and_past_the_last_row_draws_none() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let set_view_row_offset = |render_snapshot: &mut RenderSnapshot, view_row_offset: usize| {
        get_pane_snapshot_mut(render_snapshot, left_pane_id)
            .terminal_grid_view
            .as_mut()
            .expect("the left pane shows a grid")
            .view_row_offset = view_row_offset;
    };

    set_view_row_offset(&mut render_snapshot, 3);
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(Position { x: 13, y: 10 })]
    );
    set_view_row_offset(&mut render_snapshot, 18);
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(Position { x: 13, y: 25 })]
    );
    set_view_row_offset(&mut render_snapshot, 19);
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn a_view_scroll_offset_of_usize_max_draws_no_marker() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id)
        .terminal_grid_view
        .as_mut()
        .expect("the left pane shows a grid")
        .view_row_offset = usize::MAX;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn a_peer_on_the_alternate_screen_marks_the_unshifted_cell() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).is_on_alternate_screen = true;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(BOB_MARKER_POSITION)]
    );
}

#[test]
fn two_peers_on_one_cell_draw_one_marker_in_the_colors_of_the_lower_index() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers = vec![
        build_peer_snapshot(4, Some(left_pane_id)),
        build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id)),
    ];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(BOB_MARKER_POSITION)]
    );
    assert_eq!(
        get_cell_colors(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            BOB_MARKER_POSITION
        ),
        get_client_colors(BOB_COLOR_INDEX)
    );
}

#[test]
fn a_marker_over_a_cell_the_program_painted_red_keeps_its_letter_in_the_peers_colors() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let mut left_grid = Grid::build_blank(24, 28, TerminalStyle::default());
    let mut red_style = TerminalStyle::default();
    red_style.set_foreground_color(TerminalColor::Rgb(0xff, 0x00, 0x00));
    red_style.set_background_color(TerminalColor::Rgb(0xff, 0x00, 0x00));
    *left_grid
        .get_cell_mut(BOB_CURSOR_ROW_INDEX, BOB_CURSOR_COLUMN_INDEX)
        .expect("the cursor cell is inside the grid") = Cell::from_character('r', 1, red_style);
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).terminal_grid_view =
        Some(GridView::from_grid(&left_grid, 0));

    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());

    assert_eq!(render_buffer[(13, 7)].symbol(), "r");
    assert_eq!(
        get_cell_colors(&render_buffer, BOB_MARKER_POSITION),
        get_client_colors(BOB_COLOR_INDEX)
    );
}

#[test]
fn a_marker_inside_the_viewers_highlight_shows_without_reverse_video() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).selection_spans =
        Some(SelectionSpans {
            row_spans: vec![(BOB_CURSOR_ROW_INDEX, 10, 14)],
        });

    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());

    assert_eq!(
        get_cell_colors(&render_buffer, BOB_MARKER_POSITION),
        get_client_colors(BOB_COLOR_INDEX)
    );
    assert!(!render_buffer[(13, 7)].modifier.contains(Modifier::REVERSED));
    assert!(render_buffer[(12, 7)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn a_cursor_on_the_second_cell_of_a_wide_character_marks_its_first_cell() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let mut left_grid = Grid::build_blank(24, 28, TerminalStyle::default());
    *left_grid
        .get_cell_mut(BOB_CURSOR_ROW_INDEX, BOB_CURSOR_COLUMN_INDEX - 1)
        .expect("the wide character is inside the grid") =
        Cell::from_character('世', 2, TerminalStyle::default());
    *left_grid
        .get_cell_mut(BOB_CURSOR_ROW_INDEX, BOB_CURSOR_COLUMN_INDEX)
        .expect("its second cell is inside the grid") =
        Cell::from_character(' ', 0, TerminalStyle::default());
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).terminal_grid_view =
        Some(GridView::from_grid(&left_grid, 0));

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(Position { x: 12, y: 7 })]
    );
}

#[test]
fn a_cursor_past_a_shrunk_content_area_marks_the_last_row() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id)
        .cursor_snapshot
        .row_index = 30;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(Position { x: 13, y: 25 })]
    );
}

#[test]
fn a_marker_paints_only_on_a_row_the_frame_draws() {
    let (render_snapshot, _, _) = build_two_pane_frame();
    let committed_regions = build_core_regions();
    let frame_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
    );
    let render_rows = |listed_rows: &[u16]| {
        let mut render_buffer = Buffer::empty(build_viewport_area());
        render_frame(
            &frame_geometry,
            &Theme::default(),
            &KeymapHints::default(),
            None,
            ImageRenderMode::Placeholder,
            &[],
            None,
            None,
            build_test_notification_view(&NotificationCenterEntries::default()),
            DrawnRows::Listed(listed_rows),
            &mut render_buffer,
        );
        render_buffer
    };

    assert_eq!(
        get_cell_colors(&render_rows(&[3]), BOB_MARKER_POSITION),
        (Color::Reset, Color::Reset)
    );
    assert_eq!(
        get_cell_colors(&render_rows(&[7]), BOB_MARKER_POSITION),
        get_client_colors(BOB_COLOR_INDEX)
    );
}

#[test]
fn a_peer_typing_in_a_shown_floating_pane_marks_the_cell_where_this_viewer_places_that_pane() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    push_one_pane_floating_pane(
        &mut render_snapshot,
        float_pane_id,
        build_cell_rect(10, 5, 30, 12),
    );
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];

    let float_marker_position = Position { x: 14, y: 10 };
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(float_marker_position)]
    );
    assert_eq!(
        get_cell_colors(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            float_marker_position
        ),
        get_client_colors(BOB_COLOR_INDEX)
    );
}

#[test]
fn a_floating_pane_cursor_below_the_pane_area_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    push_floating_pane_slot(
        &mut render_snapshot,
        build_one_pane_floating_pane_slot(float_pane_id, build_cell_rect(40, 20, 18, 12), true),
        &[float_pane_id],
    );
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_marker(Position { x: 44, y: 25 })]
    );

    get_pane_snapshot_mut(&mut render_snapshot, float_pane_id)
        .cursor_snapshot
        .row_index = 4;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn a_floating_pane_drawn_over_a_marker_covers_it() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    push_one_pane_floating_pane(
        &mut render_snapshot,
        float_pane_id,
        build_cell_rect(5, 3, 20, 10),
    );
    get_pane_snapshot_mut(&mut render_snapshot, float_pane_id).terminal_grid_view = None;

    assert_eq!(
        get_cell_colors(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            BOB_MARKER_POSITION
        ),
        (Color::Reset, Color::Reset)
    );
}

#[test]
fn a_marker_on_the_pane_a_placement_preview_moves_takes_the_tint_and_another_keeps_its_colors() {
    let (render_snapshot, left_pane_id, right_pane_id) = build_two_pane_frame();
    let committed_regions = build_core_regions();
    let render_with_moving_pane = |source_pane_id: PaneId| {
        let placement_presentation = PanePlacementPresentation {
            source_pane_id,
            target_pane_ids: Vec::new(),
            source_message: PanePlacementMessage {
                full_text: "Moving pane".to_string(),
                compact_text: "Moving",
                detail_text: None,
            },
            target_message: None,
        };
        let mut render_buffer = Buffer::empty(build_viewport_area());
        render_frame(
            &build_frame_geometry(
                &render_snapshot,
                &committed_regions,
                ViewerChrome::default(),
                build_viewport_area(),
            ),
            &Theme::default(),
            &KeymapHints::default(),
            None,
            ImageRenderMode::Placeholder,
            &[],
            Some(&placement_presentation),
            None,
            build_test_notification_view(&NotificationCenterEntries::default()),
            DrawnRows::Every,
            &mut render_buffer,
        );
        render_buffer
    };

    let moving_left_buffer = render_with_moving_pane(left_pane_id);
    assert_eq!(moving_left_buffer[(13, 7)].bg, Color::Rgb(0, 42, 51));
    let moving_right_buffer = render_with_moving_pane(right_pane_id);
    assert_eq!(
        get_cell_colors(&moving_right_buffer, BOB_MARKER_POSITION),
        get_client_colors(BOB_COLOR_INDEX)
    );
}

// --- the thirteen states that draw no marker --------------------------------

#[test]
fn state_1_a_peer_on_another_tab_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(PaneId::new()))];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_2_a_peer_in_a_pane_hidden_behind_the_pane_this_viewer_zooms_draws_no_marker() {
    let (mut render_snapshot, left_pane_id, right_pane_id) = build_two_pane_frame();
    let layout_area = &mut render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area;
    layout_area.layout_mode = LayoutMode::Fullscreen {
        focused_pane_id: right_pane_id,
    };
    let left_pane_slot = &mut layout_area.pane_slots[0];
    assert_eq!(left_pane_slot.pane_id, left_pane_id);
    left_pane_slot.is_visible = false;
    left_pane_slot.content_rect = None;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_2_inside_a_floating_pane_a_peer_in_htop_while_this_viewer_zooms_vim_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let [vim_pane_id, htop_pane_id] = [PaneId::new(), PaneId::new()];
    push_floating_pane_slot(
        &mut render_snapshot,
        build_floating_pane_slot(
            vim_pane_id,
            &LayoutNode::Split(SplitNode::with_equal_weights(
                SplitDirection::Horizontal,
                vec![
                    LayoutNode::Pane(vim_pane_id),
                    LayoutNode::Pane(htop_pane_id),
                ],
            )),
            LayoutMode::Fullscreen {
                focused_pane_id: vim_pane_id,
            },
            build_cell_rect(10, 5, 40, 12),
            false,
        ),
        &[vim_pane_id, htop_pane_id],
    );
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(htop_pane_id))];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_3_a_peer_in_a_pane_with_no_room_in_this_viewers_solve_draws_no_marker() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let left_pane_slot = &mut render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots[0];
    assert_eq!(left_pane_slot.pane_id, left_pane_id);
    left_pane_slot.is_visible = false;
    left_pane_slot.is_suppressed = true;
    left_pane_slot.content_rect = None;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_4_a_peer_in_a_floating_pane_this_viewer_minimized_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_panes
        .push(FloatingPane::Minimized {
            floating_pane_id: build_floating_pane_id(float_pane_id),
            pane_id: float_pane_id,
            pane_title: None,
            pane_count: std::num::NonZeroUsize::MIN,
        });
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_5_a_peer_in_a_floating_pane_while_this_viewer_has_no_floating_viewport_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    push_one_pane_floating_pane(
        &mut render_snapshot,
        float_pane_id,
        build_cell_rect(10, 5, 30, 12),
    );
    render_snapshot.session_snapshot.floating_pane_slots.clear();
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_6_a_peer_in_a_collapsed_member_of_this_viewers_stack_draws_no_marker() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let left_pane_slot = &mut render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots[0];
    assert_eq!(left_pane_slot.pane_id, left_pane_id);
    left_pane_slot.is_visible = false;
    left_pane_slot.content_rect = None;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_7_a_peer_whose_keys_reach_no_pane_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers = vec![build_peer_snapshot(BOB_COLOR_INDEX, None)];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_8_a_hidden_cursor_and_a_pane_with_no_grid_draw_no_marker() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id)
        .cursor_snapshot
        .is_visible = false;
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );

    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).terminal_grid_view = None;
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_9_a_view_scrolled_past_the_cursor_row_draws_no_marker_and_live_brings_back_only_the_marker(
) {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id)
        .terminal_grid_view
        .as_mut()
        .expect("the left pane shows a grid")
        .view_row_offset = 20;
    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );

    let (live_render_snapshot, _, _) = build_two_pane_frame();
    let mut no_peer_render_snapshot = live_render_snapshot.clone();
    no_peer_render_snapshot.session_snapshot.peers.clear();
    let live_buffer = render_core_frame(&live_render_snapshot, ViewerChrome::default());
    let no_peer_buffer = render_core_frame(&no_peer_render_snapshot, ViewerChrome::default());
    let mut differing_pane_area_cells = Vec::new();
    for row_index in 1..27 {
        for column_index in 0..60 {
            if live_buffer[(column_index, row_index)] != no_peer_buffer[(column_index, row_index)] {
                differing_pane_area_cells.push((column_index, row_index));
            }
        }
    }
    assert_eq!(differing_pane_area_cells, vec![(13, 7)]);
}

#[test]
fn state_10_a_peer_in_this_viewers_own_input_pane_draws_no_marker() {
    let (mut render_snapshot, _, right_pane_id) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(right_pane_id))];

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_11_a_frame_with_no_peer_entry_draws_no_marker_and_no_chip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers.clear();
    let mut other_index_render_snapshot = render_snapshot.clone();
    other_index_render_snapshot.client_snapshot.color_index = 7;

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    let other_index_buffer =
        render_core_frame(&other_index_render_snapshot, ViewerChrome::default());
    let list_tab_strip_cells = |screen_buffer: &Buffer| -> Vec<ratatui::buffer::Cell> {
        (0..60)
            .map(|column_index| screen_buffer[(column_index, 0)].clone())
            .collect()
    };
    assert_eq!(
        list_tab_strip_cells(&render_buffer),
        list_tab_strip_cells(&other_index_buffer)
    );
}

#[test]
fn state_12_a_peer_in_a_floating_pane_while_this_viewer_places_a_pane_draws_no_marker() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    push_one_pane_floating_pane(
        &mut render_snapshot,
        float_pane_id,
        build_cell_rect(10, 5, 30, 12),
    );
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];
    let placing_viewer_chrome = ViewerChrome {
        is_pane_placement_visible: true,
        ..ViewerChrome::default()
    };

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, placing_viewer_chrome),
        Vec::new()
    );
}

#[test]
fn state_13_the_terminal_too_small_overlay_draws_no_marker_and_no_chip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let layout_area = &mut render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area;
    layout_area.is_every_pane_suppressed = true;
    for pane_slot in &mut layout_area.pane_slots {
        pane_slot.is_visible = false;
        pane_slot.is_suppressed = true;
        pane_slot.content_rect = None;
    }
    let mut no_peer_render_snapshot = render_snapshot.clone();
    no_peer_render_snapshot.session_snapshot.peers.clear();

    assert_eq!(
        list_core_peer_cursor_markers(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
    assert_eq!(
        render_core_frame(&render_snapshot, ViewerChrome::default()),
        render_core_frame(&no_peer_render_snapshot, ViewerChrome::default())
    );
}

// --- the rows a frame redraws ------------------------------------------------

#[test]
fn a_peer_whose_keys_leave_a_pane_redraws_the_row_of_the_marker_it_removes() {
    let (previous_render_snapshot, _, _) = build_two_pane_frame();
    let mut next_render_snapshot = previous_render_snapshot.clone();
    next_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;

    assert_eq!(
        list_core_changed_screen_rows(&previous_render_snapshot, &next_render_snapshot),
        Some(vec![7])
    );
    assert_eq!(
        list_core_changed_screen_rows(&next_render_snapshot, &previous_render_snapshot),
        Some(vec![7])
    );
}

#[test]
fn a_peer_whose_keys_move_to_another_pane_redraws_the_rows_of_the_marker_it_removes_and_adds() {
    let (mut previous_render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    push_one_pane_floating_pane(
        &mut previous_render_snapshot,
        float_pane_id,
        build_cell_rect(30, 12, 28, 12),
    );
    let mut next_render_snapshot = previous_render_snapshot.clone();
    next_render_snapshot.session_snapshot.peers[0].typed_pane_id = Some(float_pane_id);

    assert_eq!(
        list_core_changed_screen_rows(&previous_render_snapshot, &next_render_snapshot),
        Some(vec![7, 17])
    );
}

#[test]
fn a_peer_cursor_that_moves_with_no_row_change_redraws_the_old_and_the_new_marker_rows() {
    let (previous_render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let mut next_render_snapshot = previous_render_snapshot.clone();
    get_pane_snapshot_mut(&mut next_render_snapshot, left_pane_id)
        .cursor_snapshot
        .row_index = BOB_CURSOR_ROW_INDEX + 1;

    assert_eq!(
        list_core_changed_screen_rows(&previous_render_snapshot, &next_render_snapshot),
        Some(vec![7, 8])
    );
}

#[test]
fn a_peer_that_attaches_redraws_the_tab_strip_and_a_peer_that_leaves_typing_redraws_its_marker_row()
{
    let (typing_render_snapshot, _, _) = build_two_pane_frame();
    let mut alone_render_snapshot = typing_render_snapshot.clone();
    alone_render_snapshot.session_snapshot.peers.clear();
    let mut idle_render_snapshot = typing_render_snapshot.clone();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;

    assert_eq!(
        list_core_changed_screen_rows(&alone_render_snapshot, &idle_render_snapshot),
        Some(vec![0])
    );
    assert_eq!(
        list_core_changed_screen_rows(&typing_render_snapshot, &alone_render_snapshot),
        Some(vec![0, 7])
    );
    assert_eq!(
        list_core_changed_screen_rows(&alone_render_snapshot, &typing_render_snapshot),
        Some(vec![0, 7])
    );
}

#[test]
fn a_frame_whose_peers_and_cursors_do_not_change_lists_no_row() {
    let (render_snapshot, _, _) = build_two_pane_frame();

    assert_eq!(
        list_core_changed_screen_rows(&render_snapshot, &render_snapshot.clone()),
        Some(Vec::new())
    );
}
