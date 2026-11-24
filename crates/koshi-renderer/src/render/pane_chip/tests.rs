//! Tests for the pane chips: where a pane's top border and a collapsed stack
//! header put the chips of the peers whose keys reach the pane, how the title
//! and the `[fullscreen]` tag give way, the `+N` mark on a short row, the
//! states of the peer cursor marker table that draw a chip and those that
//! draw none, the rows a frame redraws when a chip comes, goes or moves, and
//! that a chip changes no hit region.
//!
//! Most frames are the viewer Alice (index 0) over two tiled panes side by
//! side that fill the 60x26 pane area from buffer row 1: `left`, whose top
//! border is buffer row 1, and `right`, which Alice focuses. The peer Bob
//! (index 1, number 2) types in `left`.

use super::*;

use crate::snapshot::RenderSnapshot;

use koshi_core::geometry::{Direction, Point, Rect, Size, SplitDirection};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_terminal::grid::state::Grid;
use koshi_terminal::style::Style as TerminalStyle;
use koshi_test_support::fixtures::{
    build_cell_rect, build_floating_pane_id, build_test_uuid_with_suffix,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier};

use crate::hit_test::{resolve_hit_region, HitRegion};
use crate::images::ImageRenderMode;
use crate::notification::tests::build_test_notification_view;
use crate::notification::NotificationCenterEntries;
use crate::render::client_chip::{ClientChipGroup, ClientChipSpan, HiddenClientMark};
use crate::render::tests::{
    build_floating_pane_slot, build_one_pane_floating_pane_slot, build_peer_snapshot,
    build_render_snapshot, format_rendered_cells, get_pane_snapshot_mut, render_frame_to_buffer,
};
use crate::render::{
    build_frame_geometry, list_changed_screen_rows, render_frame, DrawnRows, PeerCursorMarker,
};
use crate::render::{find_client_chip_hover, ClientChipHover, ClientChipHoverTarget};
use crate::snapshot::{
    CommittedRegions, FloatingPane, GridView, KeymapHints, PanePlacementMessage,
    PanePlacementPresentation, PlacementHandleHover, ViewerChrome,
};
use crate::theme::Theme;

/// Alice's terminal: the tab strip on row 0, the hint row on row 27, and a
/// 60x26 pane area from row 1.
const VIEWPORT_SIZE: Size = Size {
    column_count: 60,
    row_count: 28,
};

/// Bob's index. His chip reads ` 2 `.
const BOB_COLOR_INDEX: u32 = 1;

/// Carol's index. Her chip reads ` 3 `.
const CAROL_COLOR_INDEX: u32 = 2;

/// The frame the module doc describes, with `left` `left_column_count`
/// columns wide and `right` taking the rest of the 60 columns. Neither pane
/// has a title or a grid. Returns the frame and the ids of left and right.
fn build_side_by_side_frame(left_column_count: u16) -> (RenderSnapshot, PaneId, PaneId) {
    let [left_pane_id, right_pane_id] = [PaneId::new(), PaneId::new()];
    let mut render_snapshot = build_render_snapshot(
        "work",
        &[("shell", true)],
        &[
            (
                left_pane_id,
                build_cell_rect(0, 0, left_column_count, 26),
                true,
            ),
            (
                right_pane_id,
                build_cell_rect(left_column_count, 0, 60 - left_column_count, 26),
                true,
            ),
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
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id))];
    (render_snapshot, left_pane_id, right_pane_id)
}

/// The frame the module doc describes, with left 30 columns wide.
fn build_two_pane_frame() -> (RenderSnapshot, PaneId, PaneId) {
    build_side_by_side_frame(30)
}

/// The compiled-in region solve of [`VIEWPORT_SIZE`].
fn build_core_regions() -> CommittedRegions {
    CommittedRegions::build_core(VIEWPORT_SIZE, 0)
}

/// The buffer area of [`VIEWPORT_SIZE`].
fn build_viewport_area() -> RatatuiRect {
    RatatuiRect::new(0, 0, VIEWPORT_SIZE.column_count, VIEWPORT_SIZE.row_count)
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

/// The pane chip groups of every layer of `render_snapshot` drawn with
/// `viewer_chrome` over the compiled-in regions.
fn list_core_pane_chip_groups(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
) -> Vec<PaneChipGroup> {
    let committed_regions = build_core_regions();
    let frame_theme = Theme::default();
    build_frame_geometry(
        render_snapshot,
        &committed_regions,
        viewer_chrome,
        build_viewport_area(),
        &frame_theme,
    )
    .pane_chip_groups
    .pane_chip_groups
}

/// The peer cursor markers on the active tab's tiled layout of
/// `render_snapshot` drawn with the default viewer chrome over the
/// compiled-in regions.
fn list_core_active_tab_markers(render_snapshot: &RenderSnapshot) -> Vec<PeerCursorMarker> {
    let committed_regions = build_core_regions();
    let frame_theme = Theme::default();
    build_frame_geometry(
        render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
        &frame_theme,
    )
    .get_peer_cursor_markers()
    .list_layer_markers(FrameLayer::ActiveTab)
    .to_vec()
}

/// The rows a frame redraws from `previous_render_snapshot`, with the markers
/// and the pane chip groups of its own frame, to `next_render_snapshot`, both
/// over the compiled-in regions with the default viewer chrome.
fn list_core_changed_screen_rows(
    previous_render_snapshot: &RenderSnapshot,
    next_render_snapshot: &RenderSnapshot,
) -> Option<Vec<u16>> {
    let committed_regions = build_core_regions();
    let frame_theme = Theme::default();
    let previous_frame_geometry = build_frame_geometry(
        previous_render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
        &frame_theme,
    );
    list_changed_screen_rows(
        previous_render_snapshot,
        previous_frame_geometry.get_peer_cursor_markers(),
        previous_frame_geometry.get_pane_chip_groups(),
        previous_frame_geometry.get_client_chip_tooltip(),
        &build_frame_geometry(
            next_render_snapshot,
            &committed_regions,
            ViewerChrome::default(),
            build_viewport_area(),
            &frame_theme,
        ),
    )
}

/// The text and fill colors of the client of index `color_index` in the
/// default theme.
fn get_client_colors(color_index: u32) -> (Color, Color) {
    let client_color = Theme::default().compute_client_color(color_index);
    (
        Color::from(client_color.text_color),
        Color::from(client_color.fill_color),
    )
}

/// The text and fill colors of each cell of `render_buffer` on the row
/// `row_index` in `column_range`.
fn list_cell_colors(
    render_buffer: &Buffer,
    row_index: u16,
    column_range: std::ops::Range<u16>,
) -> Vec<(Color, Color)> {
    column_range
        .map(|column_index| {
            let cell = &render_buffer[(column_index, row_index)];
            (cell.fg, cell.bg)
        })
        .collect()
}

/// The group of Bob alone on `frame_layer`, on the row `row_index` of
/// `pane_id`, his chip starting at `chip_start_column`.
fn build_bob_group(
    frame_layer: FrameLayer,
    pane_id: PaneId,
    row_index: u16,
    chip_start_column: u16,
) -> PaneChipGroup {
    PaneChipGroup {
        frame_layer,
        pane_id,
        is_on_stack_header: false,
        row_index,
        client_chip_group: ClientChipGroup {
            start_column: chip_start_column,
            client_chip_spans: vec![ClientChipSpan {
                color_index: BOB_COLOR_INDEX,
                is_viewer_chip: false,
                start_column: chip_start_column,
            }],
            hidden_client_mark: None,
        },
        client_colors: vec![Theme::default().compute_client_color(BOB_COLOR_INDEX)],
        client_ids: vec![ClientId::from_uuid(build_test_uuid_with_suffix(
            BOB_COLOR_INDEX as u8,
        ))],
    }
}

/// Give `pane_id` of `render_snapshot` a blank 28x24 grid with its cursor
/// shown at row 5, column 12: buffer cell (13, 7) for left.
fn show_blank_grid_with_cursor(render_snapshot: &mut RenderSnapshot, pane_id: PaneId) {
    let pane_snapshot = get_pane_snapshot_mut(render_snapshot, pane_id);
    pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
        &Grid::build_blank(24, 28, TerminalStyle::default()),
        0,
    ));
    pane_snapshot.cursor_snapshot.row_index = 5;
    pane_snapshot.cursor_snapshot.column_index = 12;
    pane_snapshot.cursor_snapshot.is_visible = true;
}

/// Bob's marker at buffer cell (13, 7) on the active tab's tiled layout.
fn build_bob_left_marker() -> PeerCursorMarker {
    PeerCursorMarker {
        frame_layer: FrameLayer::ActiveTab,
        position: Position { x: 13, y: 7 },
        color_index: BOB_COLOR_INDEX,
        client_color: Theme::default().compute_client_color(BOB_COLOR_INDEX),
    }
}

/// The frame of the module doc whose left column is a stack: `vim` open on
/// rows 0 to 24 of the layout, and `cargo watch` collapsed to a 30-column
/// header on row 25, the second of two members: buffer row 26. Bob types in
/// `cargo watch`. Returns the frame and the ids of vim and cargo watch.
fn build_stack_frame(header_column_count: u16) -> (RenderSnapshot, PaneId, PaneId) {
    let [vim_pane_id, watch_pane_id, right_pane_id] = [PaneId::new(), PaneId::new(), PaneId::new()];
    let mut render_snapshot = build_render_snapshot(
        "work",
        &[("shell", true)],
        &[
            (vim_pane_id, build_cell_rect(0, 0, 30, 25), true),
            (watch_pane_id, build_cell_rect(0, 25, 30, 1), false),
            (right_pane_id, build_cell_rect(30, 0, 30, 26), true),
        ],
        Some(right_pane_id),
        LockMode::Normal,
        VIEWPORT_SIZE,
    );
    let layout_area = &mut render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area;
    layout_area.solved_size = Size {
        column_count: 60,
        row_count: 26,
    };
    layout_area.stack_headers = vec![StackHeader {
        pane_id: watch_pane_id,
        header_rect: build_cell_rect(0, 25, header_column_count, 1),
        member_index: 1,
        member_count: 2,
    }];
    get_pane_snapshot_mut(&mut render_snapshot, watch_pane_id).pane_title =
        Some("cargo watch".to_string());
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(watch_pane_id))];
    (render_snapshot, vim_pane_id, watch_pane_id)
}

// --- where the chips sit on a tiled pane's top border -------------------------

#[test]
fn one_peer_on_a_tiled_pane_draws_its_number_one_border_cell_from_the_top_right_corner() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).pane_title =
        Some("vim src/main.rs".to_string());

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(FrameLayer::ActiveTab, left_pane_id, 1, 25)]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 0..30),
        "┌─── vim src/main.rs ──── 2 ─┐"
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 25..28),
        vec![get_client_colors(BOB_COLOR_INDEX); 3]
    );
    assert_eq!(render_buffer[(26, 1)].modifier, Modifier::empty());
    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 30..60),
        format!("┌{}┐", "─".repeat(28))
    );
}

#[test]
fn two_peers_on_one_pane_draw_in_color_index_order_one_border_cell_apart() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers = vec![
        build_peer_snapshot(3, Some(left_pane_id)),
        build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id)),
    ];

    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());

    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 0..30),
        format!("┌{} 2 ─ 4 ─┐", "─".repeat(20))
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 21..24),
        vec![get_client_colors(BOB_COLOR_INDEX); 3]
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 25..28),
        vec![get_client_colors(3); 3]
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 24..25),
        list_cell_colors(&render_buffer, 1, 20..21)
    );
}

#[test]
fn a_pane_whose_half_row_has_no_room_for_two_chips_counts_them_into_the_mark() {
    let (mut render_snapshot, left_pane_id, _) = build_side_by_side_frame(10);
    render_snapshot.session_snapshot.peers = vec![
        build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id)),
        build_peer_snapshot(CAROL_COLOR_INDEX, Some(left_pane_id)),
    ];

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![PaneChipGroup {
            frame_layer: FrameLayer::ActiveTab,
            pane_id: left_pane_id,
            is_on_stack_header: false,
            row_index: 1,
            client_chip_group: ClientChipGroup {
                start_column: 6,
                client_chip_spans: Vec::new(),
                hidden_client_mark: Some(HiddenClientMark {
                    start_column: 6,
                    client_count: 2,
                }),
            },
            client_colors: vec![
                Theme::default().compute_client_color(BOB_COLOR_INDEX),
                Theme::default().compute_client_color(CAROL_COLOR_INDEX),
            ],
            client_ids: vec![
                ClientId::from_uuid(build_test_uuid_with_suffix(BOB_COLOR_INDEX as u8)),
                ClientId::from_uuid(build_test_uuid_with_suffix(CAROL_COLOR_INDEX as u8)),
            ],
        }]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 0..10),
        "┌─────+2─┐"
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 6..8),
        list_cell_colors(&render_buffer, 1, 4..6)
    );
}

#[test]
fn a_five_column_pane_draws_the_mark_and_a_four_column_pane_draws_no_chip() {
    let (five_column_render_snapshot, _, _) = build_side_by_side_frame(5);
    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&five_column_render_snapshot, ViewerChrome::default()),
            1,
            0..5
        ),
        "┌+1─┐"
    );

    let (four_column_render_snapshot, _, _) = build_side_by_side_frame(4);
    let mut idle_render_snapshot = four_column_render_snapshot.clone();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    assert_eq!(
        list_core_pane_chip_groups(&four_column_render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
    assert_eq!(
        render_core_frame(&four_column_render_snapshot, ViewerChrome::default()),
        render_core_frame(&idle_render_snapshot, ViewerChrome::default())
    );
}

#[test]
fn the_title_ends_one_cell_before_the_chips() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id).pane_title =
        Some("cargo build --release --all-targets".to_string());
    let mut idle_render_snapshot = render_snapshot.clone();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;

    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            1,
            0..30
        ),
        "┌─── cargo build --relea─ 2 ─┐"
    );
    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&idle_render_snapshot, ViewerChrome::default()),
            1,
            0..30
        ),
        "┌─── cargo build --release -─┐"
    );
}

#[test]
fn a_two_digit_client_number_takes_four_cells() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers = vec![build_peer_snapshot(11, Some(left_pane_id))];

    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            1,
            0..30
        ),
        format!("┌{} 12 ─┐", "─".repeat(23))
    );
}

#[test]
fn the_largest_client_index_takes_a_twelve_cell_chip_and_a_narrow_pane_counts_it_into_the_mark() {
    let (mut wide_render_snapshot, wide_left_pane_id, _) = build_two_pane_frame();
    wide_render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(u32::MAX, Some(wide_left_pane_id))];
    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&wide_render_snapshot, ViewerChrome::default()),
            1,
            0..30
        ),
        format!("┌{} 4294967296 ─┐", "─".repeat(15))
    );

    let (mut narrow_render_snapshot, narrow_left_pane_id, _) = build_side_by_side_frame(20);
    narrow_render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(u32::MAX, Some(narrow_left_pane_id))];
    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&narrow_render_snapshot, ViewerChrome::default()),
            1,
            0..20
        ),
        format!("┌{}+1─┐", "─".repeat(15))
    );
}

#[test]
fn a_zoomed_pane_truncates_its_title_first_and_keeps_its_fullscreen_tag_until_it_does_not_fit() {
    for (column_count, expected_top_border) in [
        (30, "┌─── vim s [fullscreen] ─ 2 ─┐"),
        (22, "┌─── vim src/mai─ 2 ─┐"),
    ] {
        let viewport_size = Size {
            column_count,
            row_count: 28,
        };
        let vim_pane_id = PaneId::new();
        let mut render_snapshot = build_render_snapshot(
            "work",
            &[("shell", true)],
            &[(vim_pane_id, build_cell_rect(0, 0, column_count, 26), true)],
            Some(vim_pane_id),
            LockMode::Normal,
            viewport_size,
        );
        let layout_area = &mut render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .layout_area;
        layout_area.solved_size = Size {
            column_count,
            row_count: 26,
        };
        layout_area.layout_mode = LayoutMode::Fullscreen {
            focused_pane_id: vim_pane_id,
        };
        get_pane_snapshot_mut(&mut render_snapshot, vim_pane_id).pane_title =
            Some("vim src/main.rs".to_string());
        render_snapshot.session_snapshot.peers =
            vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(vim_pane_id))];

        let render_buffer = render_frame_to_buffer(
            &render_snapshot,
            &CommittedRegions::build_core(viewport_size, 0),
            &Theme::default(),
            &KeymapHints::default(),
            ViewerChrome::default(),
        );

        assert_eq!(
            format_rendered_cells(&render_buffer, 1, 0..column_count),
            expected_top_border,
            "a {column_count}-column pane"
        );
    }
}

// --- where the chips sit on a stack header and in a floating pane -------------

#[test]
fn state_6_a_peer_in_a_member_collapsed_in_this_viewers_stack_puts_the_chip_left_of_its_indicator()
{
    let (render_snapshot, _, watch_pane_id) = build_stack_frame(30);

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![PaneChipGroup {
            is_on_stack_header: true,
            ..build_bob_group(FrameLayer::ActiveTab, watch_pane_id, 26, 21)
        }]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        format_rendered_cells(&render_buffer, 26, 0..30),
        format!("▸ cargo watch{} 2  [2/2]", " ".repeat(8))
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 26, 21..24),
        vec![get_client_colors(BOB_COLOR_INDEX); 3]
    );
    assert_eq!(
        list_cell_colors(&render_buffer, 26, 24..25),
        list_cell_colors(&render_buffer, 26, 15..16)
    );
}

#[test]
fn a_stack_header_title_longer_than_its_room_ends_one_cell_before_the_chip() {
    let (mut render_snapshot, _, watch_pane_id) = build_stack_frame(30);
    get_pane_snapshot_mut(&mut render_snapshot, watch_pane_id).pane_title =
        Some("cargo watch --exec test".to_string());

    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            26,
            0..30
        ),
        "▸ cargo watch --exec  2  [2/2]"
    );
}

#[test]
fn a_pane_with_a_visible_box_and_a_stack_header_draws_its_chip_on_the_header_only() {
    let (mut render_snapshot, _, watch_pane_id) = build_stack_frame(30);
    show_watch_pane_box(
        &mut render_snapshot,
        watch_pane_id,
        build_cell_rect(0, 20, 30, 5),
    );
    get_pane_snapshot_mut(&mut render_snapshot, watch_pane_id).pane_title =
        Some("cargo watch --exec test".to_string());

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![PaneChipGroup {
            is_on_stack_header: true,
            ..build_bob_group(FrameLayer::ActiveTab, watch_pane_id, 26, 21)
        }]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        format_rendered_cells(&render_buffer, 21, 0..30),
        "┌─── cargo watch --exec test─┐"
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 26, 0..30),
        "▸ cargo watch --exec  2  [2/2]"
    );
}

#[test]
fn a_pane_box_under_its_own_stack_header_leaves_the_position_count_whole() {
    let (mut render_snapshot, _, watch_pane_id) = build_stack_frame(30);
    show_watch_pane_box(
        &mut render_snapshot,
        watch_pane_id,
        build_cell_rect(0, 25, 30, 1),
    );

    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            26,
            0..30
        ),
        format!("▸ cargo watch{} 2  [2/2]", " ".repeat(8))
    );
}

/// Make the slot of `watch_pane_id` in `render_snapshot` a visible box at
/// `outer_rect`, as a frame of a placement animation draws a pane that
/// collapses into its stack.
fn show_watch_pane_box(
    render_snapshot: &mut RenderSnapshot,
    watch_pane_id: PaneId,
    outer_rect: Rect,
) {
    let watch_pane_slot = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .layout_area
        .pane_slots
        .iter_mut()
        .find(|pane_slot| pane_slot.pane_id == watch_pane_id)
        .expect("the stack frame holds a slot for cargo watch");
    watch_pane_slot.outer_rect = outer_rect;
    watch_pane_slot.is_visible = true;
}

#[test]
fn a_stack_header_narrower_than_its_position_indicator_draws_no_chip() {
    let (render_snapshot, _, _) = build_stack_frame(4);

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn a_peer_in_a_floating_pane_puts_the_chip_on_that_panes_own_top_border_and_none_on_the_strip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let [vim_pane_id, htop_pane_id] = [PaneId::new(), PaneId::new()];
    let floating_pane_slot = build_floating_pane_slot(
        vim_pane_id,
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(vim_pane_id),
                LayoutNode::Pane(htop_pane_id),
            ],
        )),
        LayoutMode::Tiled,
        build_cell_rect(10, 5, 40, 12),
        false,
    );
    // The floating pane's strip lands on buffer row 6 and its tree from
    // buffer (10, 7).
    let htop_box = floating_pane_slot
        .layout_area
        .find_visible_pane_slot(htop_pane_id)
        .expect("htop has room in the floating pane")
        .outer_rect;
    let htop_top_border_row = 7 + htop_box.origin.row;
    let chip_start_column = 10 + htop_box.origin.column + htop_box.size.column_count - 5;
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(floating_pane_slot);
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(htop_pane_id))];

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(
            FrameLayer::FloatingPane {
                floating_pane_index: 0
            },
            htop_pane_id,
            htop_top_border_row,
            chip_start_column
        )]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        format_rendered_cells(
            &render_buffer,
            htop_top_border_row,
            chip_start_column..chip_start_column + 3
        ),
        " 2 "
    );
    assert_eq!(
        list_cell_colors(
            &render_buffer,
            htop_top_border_row,
            chip_start_column..chip_start_column + 3
        ),
        vec![get_client_colors(BOB_COLOR_INDEX); 3]
    );
    let bob_fill_color = get_client_colors(BOB_COLOR_INDEX).1;
    assert_eq!(
        (10..50)
            .filter(|column_index| render_buffer[(*column_index, 6)].bg == bob_fill_color)
            .count(),
        0
    );
}

#[test]
fn a_floating_pane_drawn_over_a_chip_covers_it() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(build_one_pane_floating_pane_slot(
            float_pane_id,
            build_cell_rect(20, 0, 30, 12),
            false,
        ));
    let mut idle_render_snapshot = render_snapshot.clone();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;

    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            1,
            0..60
        ),
        format_rendered_cells(
            &render_core_frame(&idle_render_snapshot, ViewerChrome::default()),
            1,
            0..60
        )
    );
}

#[test]
fn a_top_border_past_the_right_edge_of_the_viewport_keeps_its_chips_whole_inside_it() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(build_one_pane_floating_pane_slot(
            float_pane_id,
            build_cell_rect(40, 5, 30, 12),
            false,
        ));
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(
            FrameLayer::FloatingPane {
                floating_pane_index: 0
            },
            float_pane_id,
            7,
            57
        )]
    );
    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, ViewerChrome::default()),
            7,
            57..60
        ),
        " 2 "
    );
}

#[test]
fn a_top_border_below_the_viewport_draws_no_chip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let [upper_pane_id, lower_pane_id] = [PaneId::new(), PaneId::new()];
    let floating_pane_slot = build_floating_pane_slot(
        upper_pane_id,
        &LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Vertical,
            vec![
                LayoutNode::Pane(upper_pane_id),
                LayoutNode::Pane(lower_pane_id),
            ],
        )),
        LayoutMode::Tiled,
        build_cell_rect(10, 15, 30, 30),
        false,
    );
    // The tree starts at buffer row 17, so the lower box's top border lies
    // past the last buffer row, 27.
    let lower_box = floating_pane_slot
        .layout_area
        .find_visible_pane_slot(lower_pane_id)
        .expect("the lower pane has room in the floating pane")
        .outer_rect;
    assert!(17 + lower_box.origin.row >= VIEWPORT_SIZE.row_count);
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(floating_pane_slot);
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(lower_pane_id))];

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

// --- the states of the marker table -------------------------------------------

#[test]
fn state_1_a_peer_in_a_pane_this_frame_does_not_hold_draws_no_chip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(PaneId::new()))];

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_2_a_peer_in_a_pane_hidden_behind_the_pane_this_viewer_zooms_draws_no_chip() {
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
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_3_a_peer_in_a_pane_with_no_room_in_this_viewers_solve_draws_no_chip() {
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
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_4_and_5_a_peer_in_a_floating_pane_this_viewer_does_not_draw_draws_no_chip() {
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
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_7_a_peer_whose_keys_reach_no_pane_draws_no_chip_and_changes_no_pane_area_cell() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    let mut lone_render_snapshot = render_snapshot.clone();
    lone_render_snapshot.session_snapshot.peers.clear();

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    let lone_buffer = render_core_frame(&lone_render_snapshot, ViewerChrome::default());
    for row_index in 1..27 {
        assert_eq!(
            format_rendered_cells(&render_buffer, row_index, 0..60),
            format_rendered_cells(&lone_buffer, row_index, 0..60),
            "row {row_index}"
        );
        assert_eq!(
            list_cell_colors(&render_buffer, row_index, 0..60),
            list_cell_colors(&lone_buffer, row_index, 0..60),
            "row {row_index}"
        );
    }
}

#[test]
fn a_peer_whose_keys_reach_no_pane_draws_its_chip_on_its_recent_input_pane_and_no_marker() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    show_blank_grid_with_cursor(&mut render_snapshot, left_pane_id);
    render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    render_snapshot.session_snapshot.peers[0].recent_input_pane_ids = vec![left_pane_id];

    assert_eq!(list_core_active_tab_markers(&render_snapshot), Vec::new());
    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(FrameLayer::ActiveTab, left_pane_id, 1, 25)]
    );
}

#[test]
fn a_peer_typing_in_one_pane_with_recent_input_in_another_draws_its_chip_on_both() {
    let (mut render_snapshot, left_pane_id, right_pane_id) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers[0].recent_input_pane_ids = vec![right_pane_id];
    let mut expected_pane_chip_groups = vec![
        (
            left_pane_id,
            build_bob_group(FrameLayer::ActiveTab, left_pane_id, 1, 25),
        ),
        (
            right_pane_id,
            build_bob_group(FrameLayer::ActiveTab, right_pane_id, 1, 55),
        ),
    ];
    expected_pane_chip_groups.sort_by_key(|(pane_id, _)| *pane_id);

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        expected_pane_chip_groups
            .into_iter()
            .map(|(_, pane_chip_group)| pane_chip_group)
            .collect::<Vec<PaneChipGroup>>()
    );
}

#[test]
fn state_8_a_peer_in_a_pane_with_a_hidden_cursor_draws_the_chip_and_no_marker() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    show_blank_grid_with_cursor(&mut render_snapshot, left_pane_id);
    assert_eq!(
        list_core_active_tab_markers(&render_snapshot),
        vec![build_bob_left_marker()]
    );

    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id)
        .cursor_snapshot
        .is_visible = false;

    assert_eq!(list_core_active_tab_markers(&render_snapshot), Vec::new());
    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(FrameLayer::ActiveTab, left_pane_id, 1, 25)]
    );
}

#[test]
fn state_9_a_view_scrolled_past_the_peers_cursor_row_keeps_the_chip() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    show_blank_grid_with_cursor(&mut render_snapshot, left_pane_id);
    get_pane_snapshot_mut(&mut render_snapshot, left_pane_id)
        .terminal_grid_view
        .as_mut()
        .expect("the left pane shows a grid")
        .view_row_offset = 20;

    assert_eq!(list_core_active_tab_markers(&render_snapshot), Vec::new());
    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(FrameLayer::ActiveTab, left_pane_id, 1, 25)]
    );
}

#[test]
fn state_10_a_peer_in_this_viewers_own_input_pane_draws_the_chip_and_no_marker() {
    let (mut render_snapshot, _, right_pane_id) = build_two_pane_frame();
    show_blank_grid_with_cursor(&mut render_snapshot, right_pane_id);
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(right_pane_id))];

    assert_eq!(list_core_active_tab_markers(&render_snapshot), Vec::new());
    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        vec![build_bob_group(FrameLayer::ActiveTab, right_pane_id, 1, 55)]
    );
    let render_buffer = render_core_frame(&render_snapshot, ViewerChrome::default());
    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 30..60),
        format!("┌{} 2 ─┐", "─".repeat(24))
    );
    assert_eq!(render_buffer[(54, 1)].modifier, Modifier::BOLD);
    for column_index in 55..58 {
        assert_eq!(
            render_buffer[(column_index, 1)].modifier,
            Modifier::empty(),
            "the chip cell at column {column_index} on the bold border of the focused pane"
        );
    }
}

#[test]
fn state_11_a_frame_with_no_peer_entry_draws_no_chip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers.clear();

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

#[test]
fn state_12_a_peer_in_a_floating_pane_while_this_viewer_places_a_pane_draws_no_chip() {
    let (mut render_snapshot, _, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(build_one_pane_floating_pane_slot(
            float_pane_id,
            build_cell_rect(10, 5, 30, 12),
            false,
        ));
    render_snapshot.session_snapshot.peers =
        vec![build_peer_snapshot(BOB_COLOR_INDEX, Some(float_pane_id))];
    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()).len(),
        1
    );

    let placing_viewer_chrome = ViewerChrome {
        is_pane_placement_visible: true,
        ..ViewerChrome::default()
    };

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, placing_viewer_chrome),
        Vec::new()
    );
}

#[test]
fn state_13_the_terminal_too_small_overlay_draws_no_chip() {
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

    assert_eq!(
        list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default()),
        Vec::new()
    );
}

// --- paint order, the placement preview and the hit test ----------------------

#[test]
fn a_chip_paints_over_the_handle_of_a_narrow_pane_and_the_press_still_hits_the_handle() {
    let (render_snapshot, left_pane_id, _) = build_side_by_side_frame(7);
    let mut idle_render_snapshot = render_snapshot.clone();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    let hovering_viewer_chrome = ViewerChrome {
        placement_handle_hover: Some(PlacementHandleHover::TiledTopBorder {
            pane_id: left_pane_id,
        }),
        ..ViewerChrome::default()
    };

    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&render_snapshot, hovering_viewer_chrome),
            1,
            0..7
        ),
        "┌⠿⠿+1─┐"
    );
    assert_eq!(
        format_rendered_cells(
            &render_core_frame(&idle_render_snapshot, hovering_viewer_chrome),
            1,
            0..7
        ),
        "┌⠿⠿⠿──┐"
    );
    let handle_cell = Point { column: 3, row: 1 };
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(hovering_viewer_chrome),
            handle_cell
        ),
        HitRegion::PlacementHandle {
            pane_id: left_pane_id
        }
    );
}

#[test]
fn every_chip_cell_hits_what_it_hits_with_no_peer() {
    let (render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let mut lone_render_snapshot = render_snapshot.clone();
    lone_render_snapshot.session_snapshot.peers.clear();

    for column_index in 0..60 {
        let screen_point = Point {
            column: column_index,
            row: 1,
        };
        assert_eq!(
            resolve_hit_region(
                render_snapshot.build_frame_layout(ViewerChrome::default()),
                screen_point
            ),
            resolve_hit_region(
                lone_render_snapshot.build_frame_layout(ViewerChrome::default()),
                screen_point
            ),
            "column {column_index}"
        );
    }
    assert_eq!(
        resolve_hit_region(
            render_snapshot.build_frame_layout(ViewerChrome::default()),
            Point { column: 26, row: 1 }
        ),
        HitRegion::PaneBorder {
            pane_id: left_pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }
    );
}

#[test]
fn a_placement_preview_keeps_the_number_of_the_moving_panes_chip_in_its_outline_color() {
    let (mut render_snapshot, left_pane_id, right_pane_id) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers = vec![
        build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id)),
        build_peer_snapshot(CAROL_COLOR_INDEX, Some(right_pane_id)),
    ];
    let committed_regions = build_core_regions();
    let frame_theme = Theme::default();
    let placement_presentation = PanePlacementPresentation {
        source_pane_id: left_pane_id,
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
            &frame_theme,
        ),
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

    assert_eq!(format_rendered_cells(&render_buffer, 1, 25..28), " 2 ");
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 25..28),
        vec![(frame_theme.focused_border_color, Color::Rgb(0, 42, 51)); 3]
    );
    assert_eq!(format_rendered_cells(&render_buffer, 1, 55..58), " 3 ");
    assert_eq!(
        list_cell_colors(&render_buffer, 1, 55..58),
        vec![get_client_colors(CAROL_COLOR_INDEX); 3]
    );
}

// --- the rows a frame redraws -----------------------------------------------------

#[test]
fn a_chip_that_comes_with_no_marker_redraws_only_its_top_border_row() {
    let (mut idle_render_snapshot, _, right_pane_id) = build_two_pane_frame();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    let mut typing_render_snapshot = idle_render_snapshot.clone();
    typing_render_snapshot.session_snapshot.peers[0].typed_pane_id = Some(right_pane_id);

    assert_eq!(
        list_core_changed_screen_rows(&idle_render_snapshot, &typing_render_snapshot),
        Some(vec![1])
    );
    assert_eq!(
        list_core_changed_screen_rows(&typing_render_snapshot, &idle_render_snapshot),
        Some(vec![1])
    );
}

#[test]
fn a_recent_input_pane_that_comes_and_goes_redraws_only_its_top_border_row() {
    let (mut idle_render_snapshot, _, right_pane_id) = build_two_pane_frame();
    idle_render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    let mut marked_render_snapshot = idle_render_snapshot.clone();
    marked_render_snapshot.session_snapshot.peers[0].recent_input_pane_ids = vec![right_pane_id];

    assert_eq!(
        list_core_changed_screen_rows(&idle_render_snapshot, &marked_render_snapshot),
        Some(vec![1])
    );
    assert_eq!(
        list_core_changed_screen_rows(&marked_render_snapshot, &idle_render_snapshot),
        Some(vec![1])
    );
}

#[test]
fn a_peer_whose_keys_move_from_a_top_border_to_a_stack_header_redraws_both_rows() {
    let (header_render_snapshot, vim_pane_id, _) = build_stack_frame(30);
    let mut top_border_render_snapshot = header_render_snapshot.clone();
    top_border_render_snapshot.session_snapshot.peers[0].typed_pane_id = Some(vim_pane_id);

    assert_eq!(
        list_core_changed_screen_rows(&top_border_render_snapshot, &header_render_snapshot),
        Some(vec![1, 26])
    );
}

#[test]
fn a_frame_lists_the_groups_of_each_layer_apart() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let float_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(build_one_pane_floating_pane_slot(
            float_pane_id,
            build_cell_rect(10, 5, 30, 12),
            false,
        ));
    render_snapshot.session_snapshot.peers = vec![
        build_peer_snapshot(BOB_COLOR_INDEX, Some(left_pane_id)),
        build_peer_snapshot(BOB_COLOR_INDEX + 5, Some(float_pane_id)),
    ];
    let committed_regions = build_core_regions();
    let frame_theme = Theme::default();
    let frame_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
        &frame_theme,
    );
    let pane_chip_groups = frame_geometry.get_pane_chip_groups();

    assert_eq!(
        pane_chip_groups.list_layer_groups(FrameLayer::ActiveTab),
        [build_bob_group(FrameLayer::ActiveTab, left_pane_id, 1, 25)]
    );
    assert_eq!(
        pane_chip_groups
            .list_layer_groups(FrameLayer::FloatingPane {
                floating_pane_index: 0
            })
            .iter()
            .map(|pane_chip_group| (pane_chip_group.pane_id, pane_chip_group.row_index))
            .collect::<Vec<_>>(),
        vec![(float_pane_id, 7)]
    );
    assert_eq!(
        pane_chip_groups.list_layer_groups(FrameLayer::FloatingPane {
            floating_pane_index: 1
        }),
        [] as [PaneChipGroup; 0]
    );
}

#[test]
fn a_pane_group_draws_at_most_three_chips_then_the_mark() {
    for peer_count in 1..=5u32 {
        let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
        render_snapshot.session_snapshot.peers = (1..=peer_count)
            .map(|color_index| build_peer_snapshot(color_index, Some(left_pane_id)))
            .collect();

        let pane_chip_groups =
            list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default());
        let pane_chip_group = pane_chip_groups
            .iter()
            .find(|pane_chip_group| {
                pane_chip_group.pane_id == left_pane_id && !pane_chip_group.is_on_stack_header
            })
            .expect("the left pane draws its chip group");

        assert_eq!(
            pane_chip_group.client_chip_group.client_chip_spans.len(),
            peer_count.min(3) as usize,
            "peer count {peer_count}"
        );
        assert_eq!(
            pane_chip_group
                .client_chip_group
                .hidden_client_mark
                .map(|hidden_client_mark| hidden_client_mark.client_count),
            (peer_count > 3).then_some(peer_count.saturating_sub(3) as usize),
            "peer count {peer_count}"
        );
        assert_eq!(
            pane_chip_group.client_ids.len(),
            peer_count as usize,
            "peer count {peer_count}"
        );
    }
}

#[test]
fn five_peers_on_a_pane_border_draw_three_chips_and_a_mark_for_two() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers = (1..=5)
        .map(|color_index| build_peer_snapshot(color_index, Some(left_pane_id)))
        .collect();

    let pane_chip_groups = list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default());
    let pane_chip_group = pane_chip_groups
        .iter()
        .find(|pane_chip_group| pane_chip_group.pane_id == left_pane_id)
        .expect("the left pane draws its chip group");

    assert_eq!(
        pane_chip_group.client_chip_group,
        ClientChipGroup {
            start_column: 14,
            client_chip_spans: vec![
                ClientChipSpan {
                    color_index: 1,
                    is_viewer_chip: false,
                    start_column: 14,
                },
                ClientChipSpan {
                    color_index: 2,
                    is_viewer_chip: false,
                    start_column: 18,
                },
                ClientChipSpan {
                    color_index: 3,
                    is_viewer_chip: false,
                    start_column: 22,
                },
            ],
            hidden_client_mark: Some(HiddenClientMark {
                start_column: 26,
                client_count: 2,
            }),
        }
    );
}

#[test]
fn a_stack_header_chip_is_hoverable_and_names_its_peer() {
    let (mut render_snapshot, _, watch_pane_id) = build_stack_frame(30);
    render_snapshot.session_snapshot.peers = (1..=2)
        .map(|color_index| build_peer_snapshot(color_index, Some(watch_pane_id)))
        .collect();
    let committed_regions = build_core_regions();
    let viewer_chrome = ViewerChrome::default();
    let frame_layout =
        render_snapshot.build_painted_frame_layout(viewer_chrome, &committed_regions);
    let frame_theme = Theme::default();
    let frame_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        build_viewport_area(),
        &frame_theme,
    );
    let pane_chip_group = frame_geometry
        .pane_chip_groups
        .find_pane_chip_group(watch_pane_id, true)
        .expect("the stack header draws its chip group");
    let client_chip_span = pane_chip_group.client_chip_group.client_chip_spans[0];
    let pointer_position = Point {
        column: client_chip_span.start_column + 1,
        row: pane_chip_group.row_index,
    };
    let hit_region = resolve_hit_region(frame_layout, pointer_position);

    assert_eq!(
        hit_region,
        HitRegion::StackHeader {
            pane_id: watch_pane_id
        }
    );
    assert_eq!(
        find_client_chip_hover(
            frame_layout,
            &frame_geometry.client_chip_groups,
            hit_region,
            pointer_position,
        ),
        Some(ClientChipHover::Pane {
            pane_id: watch_pane_id,
            is_on_stack_header: true,
            target: ClientChipHoverTarget::PeerClient {
                client_id: pane_chip_group.client_ids[0],
            },
            row_index: pane_chip_group.row_index,
            start_column: client_chip_span.start_column,
        })
    );
}

#[test]
fn a_recent_input_chip_that_expires_under_the_pointer_opens_no_tooltip() {
    let (mut render_snapshot, left_pane_id, _) = build_two_pane_frame();
    render_snapshot.session_snapshot.peers[0].typed_pane_id = None;
    render_snapshot.session_snapshot.peers[0].recent_input_pane_ids = vec![left_pane_id];
    let pane_chip_groups = list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default());
    let pane_chip_group = pane_chip_groups
        .iter()
        .find(|pane_chip_group| pane_chip_group.pane_id == left_pane_id)
        .expect("the left pane draws the recent input chip");
    let client_chip_span = pane_chip_group.client_chip_group.client_chip_spans[0];
    let pointer_position = Point {
        column: client_chip_span.start_column + 1,
        row: pane_chip_group.row_index,
    };
    let committed_regions = build_core_regions();
    let frame_theme = Theme::default();
    let frame_layout =
        render_snapshot.build_painted_frame_layout(ViewerChrome::default(), &committed_regions);
    let hit_region = resolve_hit_region(frame_layout, pointer_position);
    let frame_geometry = build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
        &frame_theme,
    );
    assert!(find_client_chip_hover(
        frame_layout,
        &frame_geometry.client_chip_groups,
        hit_region,
        pointer_position,
    )
    .is_some());
    let mut expired_render_snapshot = render_snapshot.clone();
    expired_render_snapshot.session_snapshot.peers[0]
        .recent_input_pane_ids
        .clear();
    let expired_frame_layout = expired_render_snapshot
        .build_painted_frame_layout(ViewerChrome::default(), &committed_regions);
    let expired_hit_region = resolve_hit_region(expired_frame_layout, pointer_position);
    let expired_frame_geometry = build_frame_geometry(
        &expired_render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
        &frame_theme,
    );

    assert_eq!(
        find_client_chip_hover(
            expired_frame_layout,
            &expired_frame_geometry.client_chip_groups,
            expired_hit_region,
            pointer_position
        ),
        None
    );
}

#[test]
fn a_floating_pane_over_a_chip_makes_that_chip_not_hoverable() {
    let (render_snapshot, left_pane_id, _) = build_two_pane_frame();
    let pane_chip_groups = list_core_pane_chip_groups(&render_snapshot, ViewerChrome::default());
    let pane_chip_group = pane_chip_groups
        .iter()
        .find(|pane_chip_group| pane_chip_group.pane_id == left_pane_id)
        .expect("the left pane draws its chip group");
    let client_chip_span = pane_chip_group.client_chip_group.client_chip_spans[0];
    let pointer_position = Point {
        column: client_chip_span.start_column + 1,
        row: pane_chip_group.row_index,
    };
    let mut covered_render_snapshot = render_snapshot.clone();
    covered_render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(build_one_pane_floating_pane_slot(
            PaneId::new(),
            build_cell_rect(20, 0, 30, 12),
            false,
        ));
    let committed_regions = build_core_regions();
    let frame_layout = covered_render_snapshot
        .build_painted_frame_layout(ViewerChrome::default(), &committed_regions);
    let hit_region = resolve_hit_region(frame_layout, pointer_position);
    let theme = Theme::default();
    let frame_geometry = build_frame_geometry(
        &covered_render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
        &theme,
    );

    assert_ne!(
        hit_region,
        HitRegion::PaneBorder {
            pane_id: left_pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }
    );
    assert_eq!(
        find_client_chip_hover(
            frame_layout,
            &frame_geometry.client_chip_groups,
            hit_region,
            pointer_position
        ),
        None
    );
}
