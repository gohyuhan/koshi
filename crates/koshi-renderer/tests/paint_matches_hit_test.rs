//! The crate's two entry points read one frame the same way: the region
//! [`resolve_hit_region`] reports for a cell is the region [`render_frame`] painted
//! there.
//!
//! Each test paints one frame into a buffer and classifies the same frame cell
//! by cell, then checks the two against each other: a chrome row is on the row
//! its committed region owns, a pane's content cells sit inside the rect
//! [`find_pane_content_rect`] gives for that pane, a border cell carries a box
//! glyph, a stack header carries the header background, and every unclassified
//! cell carries the letterbox backdrop.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::SolvedRegions;
use koshi_layout::solver::StackHeader;
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, FloatingPaneMoveDraft, FloatingPaneSlot,
    GridView, KeymapHints, MouseFrame, PaneSlot, PaneSnapshot, RenderSnapshot, ScrollbackMetadata,
    SessionSnapshot, TabMetadata, TabSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    find_pane_content_rect, get_cursor_position, render_frame, resolve_hit_region, DrawnRows,
    HitRegion, ImageRenderMode,
};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style as CellStyle;
use koshi_test_support::fixtures::build_cell_rect;

/// The client viewport every frame here is painted and hit-tested in.
const TEST_VIEWPORT_SIZE: Size = Size {
    column_count: 60,
    row_count: 14,
};

/// The size the tiled layout is solved for. Smaller than [`TEST_VIEWPORT_SIZE`] on both
/// axes, so the frame carries a letterbox margin on all four sides.
const EFFECTIVE_LAYOUT_SIZE: Size = Size {
    column_count: 50,
    row_count: 10,
};

/// The names of the three tabs every frame here carries, the first active.
const TAB_NAMES: [&str; 3] = ["one", "two", "three"];

/// The glyphs a tiled pane's border ring is drawn with.
const BORDER_GLYPHS: [&str; 6] = ["┌", "┐", "└", "┘", "─", "│"];

/// A visible pane slot whose content area is `outer_rect` inset by its one-cell
/// border.
fn build_pane_slot(pane_id: PaneId, outer_rect: Rect) -> PaneSlot {
    PaneSlot {
        pane_id,
        outer_rect,
        content_rect: Some(outer_rect.compute_inner_with_border()),
        is_visible: true,
        is_suppressed: false,
    }
}

/// A pane whose every cell holds `fill_character`, sized `column_count x row_count`.
fn build_filled_grid(column_count: u16, row_count: u16, fill_character: char) -> GridView {
    let mut terminal_grid = Grid::build_blank(row_count, column_count, CellStyle::default());
    for row_index in 0..row_count {
        for column_index in 0..column_count {
            *terminal_grid
                .get_cell_mut(row_index, column_index)
                .expect("the cell is in the grid") =
                Cell::from_character(fill_character, 1, CellStyle::default());
        }
    }
    GridView::from_grid(&terminal_grid, 0)
}

/// One pane's content with the cursor at `(cursor_row_index, cursor_column_index)` of its content
/// area.
fn build_pane_snapshot(
    pane_id: PaneId,
    grid_view: Option<GridView>,
    cursor_row_index: u16,
    cursor_column_index: u16,
) -> PaneSnapshot {
    PaneSnapshot {
        pane_id,
        pane_title: None,
        cursor_snapshot: CursorSnapshot {
            row_index: cursor_row_index,
            column_index: cursor_column_index,
            is_visible: true,
            is_blinking: false,
            shape: None,
        },
        terminal_grid_view: grid_view,
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

/// A frame with `pane_slots` laid out for `effective_layout_size`, `stack_headers` on top
/// of them, and three tabs.
fn build_render_snapshot(
    effective_layout_size: Size,
    pane_slots: Vec<PaneSlot>,
    pane_snapshots: Vec<PaneSnapshot>,
    stack_headers: Vec<StackHeader>,
    focused_pane_id: Option<PaneId>,
) -> RenderSnapshot {
    let tab_id = TabId::new();
    let tabs_metadata = TAB_NAMES
        .iter()
        .enumerate()
        .map(|(tab_index, tab_name)| TabMetadata {
            tab_id: TabId::new(),
            tab_name: (*tab_name).to_string(),
            tab_index,
            is_active: tab_index == 0,
        })
        .collect();
    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: "work".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "one".to_string(),
                pane_slots,
                tab_size: effective_layout_size,
                stack_headers,
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata,
            floating_pane_slots: Vec::new(),
            floating_pane_ids: Vec::new(),
        },
        pane_snapshots,
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id,
            active_tab_focused_pane_id: focused_pane_id,
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            is_mouse_selection_enabled: false,
            last_applied_placement_command_id: None,
        },
        placement_preview: None,
    }
}

/// Two panes tiling [`EFFECTIVE_LAYOUT_SIZE`] side by side, a one-row stack header covering
/// the right pane's first content row, and the left pane filled with `a`.
///
/// Returns the frame, the left pane, the right pane, and the header's pane.
fn build_tiled_frame() -> (RenderSnapshot, PaneId, PaneId, PaneId) {
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let collapsed_pane_id = PaneId::new();
    let left_outer_rect = build_cell_rect(0, 0, 25, 10);
    let right_outer_rect = build_cell_rect(25, 0, 25, 10);
    let stack_header = StackHeader {
        pane_id: collapsed_pane_id,
        header_rect: build_cell_rect(26, 1, 23, 1),
        member_index: 0,
        member_count: 2,
    };
    let render_snapshot = build_render_snapshot(
        EFFECTIVE_LAYOUT_SIZE,
        vec![
            build_pane_slot(left_pane_id, left_outer_rect),
            build_pane_slot(right_pane_id, right_outer_rect),
        ],
        vec![
            build_pane_snapshot(left_pane_id, Some(build_filled_grid(23, 8, 'a')), 3, 7),
            build_pane_snapshot(right_pane_id, None, 0, 0),
        ],
        vec![stack_header],
        Some(left_pane_id),
    );
    (
        render_snapshot,
        left_pane_id,
        right_pane_id,
        collapsed_pane_id,
    )
}

/// The whole viewport as a ratatui area at the origin.
fn build_viewport_area() -> RatatuiRect {
    RatatuiRect::new(
        0,
        0,
        TEST_VIEWPORT_SIZE.column_count,
        TEST_VIEWPORT_SIZE.row_count,
    )
}

/// Paint `render_snapshot` with `committed_regions` and `viewer_chrome` into a
/// fresh viewport-sized buffer.
fn paint_render_snapshot(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
) -> Buffer {
    let viewport_area = build_viewport_area();
    let mut screen_buffer = Buffer::empty(viewport_area);
    render_frame(
        render_snapshot,
        committed_regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        viewer_chrome,
        ImageRenderMode::Placeholder,
        &[],
        None,
        None,
        viewport_area,
        DrawnRows::Every,
        &mut screen_buffer,
    );
    screen_buffer
}

/// The text painted on row `row_index` across the half-open column span
/// `[starting_column, ending_column)`.
fn get_row_text(
    screen_buffer: &Buffer,
    row_index: u16,
    starting_column: u16,
    ending_column: u16,
) -> String {
    (starting_column..ending_column)
        .map(|column_index| screen_buffer[(column_index, row_index)].symbol())
        .collect()
}

#[test]
fn every_cell_classifies_as_what_was_painted() {
    let (render_snapshot, left_pane_id, right_pane_id, collapsed_pane_id) = build_tiled_frame();
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let screen_buffer = paint_render_snapshot(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );
    let theme = Theme::default();
    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions, ViewerChrome::default());
    let frame_layout = mouse_frame.build_frame_layout();

    let statusline_row_index = TEST_VIEWPORT_SIZE.row_count - 1;
    let mut content_cell_count = 0_u32;
    let mut border_cell_count = 0_u32;
    let mut header_cell_count = 0_u32;
    let mut letterbox_cell_count = 0_u32;

    for row_index in 0..TEST_VIEWPORT_SIZE.row_count {
        for column_index in 0..TEST_VIEWPORT_SIZE.column_count {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            let painted_cell = &screen_buffer[(column_index, row_index)];
            match resolve_hit_region(frame_layout, screen_point) {
                HitRegion::Tabline
                | HitRegion::Tab { .. }
                | HitRegion::TablineScrollLeft
                | HitRegion::TablineScrollRight => {
                    assert_eq!(
                        row_index, 0,
                        "tabline classified off the tabline row at ({column_index}, {row_index})"
                    );
                }
                HitRegion::Statusline => {
                    assert_eq!(
                        row_index, statusline_row_index,
                        "statusline classified off its row at ({column_index}, {row_index})"
                    );
                    assert_eq!(
                        painted_cell.bg, theme.bar_background_color,
                        "statusline cell ({column_index}, {row_index}) is not the bar background"
                    );
                }
                HitRegion::PaneContent { pane_id } => {
                    let content_rect = find_pane_content_rect(frame_layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        content_rect.is_point_inside(screen_point),
                        "content hit at ({column_index}, {row_index}) is outside {content_rect:?}"
                    );
                    if pane_id == left_pane_id {
                        assert_eq!(
                            painted_cell.symbol(),
                            "a",
                            "left pane content cell ({column_index}, {row_index}) was not painted"
                        );
                    } else {
                        assert_eq!(
                            pane_id, right_pane_id,
                            "unexpected pane hit at ({column_index}, {row_index})"
                        );
                    }
                    content_cell_count += 1;
                }
                HitRegion::PaneBorder { pane_id, .. } => {
                    let content_rect = find_pane_content_rect(frame_layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        !content_rect.is_point_inside(screen_point),
                        "border hit at ({column_index}, {row_index}) is inside {content_rect:?}"
                    );
                    assert!(
                        BORDER_GLYPHS.contains(&painted_cell.symbol()),
                        "border hit at ({column_index}, {row_index}) painted {:?}",
                        painted_cell.symbol()
                    );
                    border_cell_count += 1;
                }
                HitRegion::PlacementHandle { pane_id } => {
                    let content_rect = find_pane_content_rect(frame_layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        !content_rect.is_point_inside(screen_point),
                        "placement handle hit at ({column_index}, {row_index}) is inside {content_rect:?}"
                    );
                    assert_eq!(
                        painted_cell.symbol(),
                        "⠿",
                        "placement handle cell ({column_index}, {row_index}) was not painted"
                    );
                    border_cell_count += 1;
                }
                HitRegion::PaneTitlebar { .. }
                | HitRegion::PanePinToggle { .. }
                | HitRegion::PaneMinimizeButton { .. }
                | HitRegion::PaneCloseButton { .. } => {
                    panic!(
                        "floating pane region at ({column_index}, {row_index}) in a frame with no floating pane"
                    );
                }
                HitRegion::StackHeader { pane_id } => {
                    assert_eq!(pane_id, collapsed_pane_id);
                    assert_eq!(
                        painted_cell.bg, theme.stack_header_background_color,
                        "stack header cell ({column_index}, {row_index}) is not the header background"
                    );
                    header_cell_count += 1;
                }
                HitRegion::None => {
                    assert_eq!(
                        painted_cell.bg, theme.letterbox_color,
                        "unclassified cell ({column_index}, {row_index}) is not letterbox margin"
                    );
                    letterbox_cell_count += 1;
                }
            }
        }
    }

    // Both panes' content minus the row the stack header takes.
    assert_eq!(content_cell_count, 2 * 23 * 8 - 23);
    assert_eq!(border_cell_count, 2 * (25 * 10 - 23 * 8));
    assert_eq!(header_cell_count, 23);
    // The 60x14 viewport less the two chrome rows and the centered 50x10 layout.
    assert_eq!(
        letterbox_cell_count,
        u32::from(
            TEST_VIEWPORT_SIZE.column_count * 12
                - EFFECTIVE_LAYOUT_SIZE.column_count * EFFECTIVE_LAYOUT_SIZE.row_count
        )
    );
}

#[test]
fn every_floating_pane_cell_classifies_as_what_was_painted() {
    let (mut render_snapshot, left_pane_id, ..) = build_tiled_frame();
    let floating_pane_id = PaneId::new();
    // Pane-area (1, 0) is screen (1, 1): the float covers letterbox margin
    // cells and the left tiled pane.
    let floating_outer_rect = build_cell_rect(1, 0, 24, 8);
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            floating_pane_id,
            floating_outer_rect,
            false,
        ));
    let mut floating_pane_snapshot =
        build_pane_snapshot(floating_pane_id, Some(build_filled_grid(22, 4, 'f')), 0, 0);
    floating_pane_snapshot.pane_title = Some("htop".to_string());
    render_snapshot.pane_snapshots.push(floating_pane_snapshot);
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let screen_buffer = paint_render_snapshot(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );
    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions, ViewerChrome::default());
    let frame_layout = mouse_frame.build_frame_layout();
    let floating_content_rect = find_pane_content_rect(frame_layout, floating_pane_id)
        .unwrap_or_else(|| panic!("the floating pane has no content rect"));
    assert_eq!(floating_content_rect, build_cell_rect(2, 4, 22, 4));

    let mut region_cell_counts = [0_u32; 7];
    for row_index in 1..9 {
        for column_index in 1..25 {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            let painted_symbol = screen_buffer[(column_index, row_index)].symbol();
            let (region_index, pane_id, painted_symbols): (usize, PaneId, &[&str]) =
                match resolve_hit_region(frame_layout, screen_point) {
                    HitRegion::PaneContent { pane_id } => {
                        assert!(
                            floating_content_rect.is_point_inside(screen_point),
                            "content hit at ({column_index}, {row_index}) is outside {floating_content_rect:?}"
                        );
                        (0, pane_id, &["f"])
                    }
                    HitRegion::PaneBorder { pane_id, .. } => {
                        (1, pane_id, &["┌", "┐", "└", "┘", "─", "│", "├", "┤"])
                    }
                    HitRegion::PlacementHandle { pane_id } => (2, pane_id, &["⣿"]),
                    HitRegion::PaneTitlebar { pane_id } => {
                        (3, pane_id, &[" ", "h", "t", "o", "p", "─"])
                    }
                    HitRegion::PanePinToggle { pane_id } => (4, pane_id, &["[", " ", "]"]),
                    HitRegion::PaneMinimizeButton { pane_id } => (5, pane_id, &["–"]),
                    HitRegion::PaneCloseButton { pane_id } => (6, pane_id, &["✕"]),
                    unexpected_region => {
                        panic!(
                            "({column_index}, {row_index}) inside the float hit {unexpected_region:?}"
                        )
                    }
                };
            assert_eq!(
                pane_id, floating_pane_id,
                "({column_index}, {row_index}) inside the float hit another pane"
            );
            assert!(
                painted_symbols.contains(&painted_symbol),
                "({column_index}, {row_index}) painted {painted_symbol:?} for region {region_index}"
            );
            region_cell_counts[region_index] += 1;
        }
    }

    // Content 22x4; ring 2x24 + 2x6; handle 2; titlebar 14 + separator 22;
    // pin 4; minimize 1; close 1. 24x8 = 192 cells.
    assert_eq!(region_cell_counts, [88, 60, 2, 36, 4, 1, 1]);
    // The cell right of the float is the left tiled pane again.
    assert_eq!(
        resolve_hit_region(frame_layout, Point { column: 25, row: 5 }),
        HitRegion::PaneContent {
            pane_id: left_pane_id
        }
    );
}

#[test]
fn a_move_draft_past_the_pane_area_paints_and_hits_inside_the_pane_area() {
    let (mut render_snapshot, ..) = build_tiled_frame();
    let floating_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            floating_pane_id,
            build_cell_rect(1, 0, 24, 8),
            false,
        ));
    render_snapshot.pane_snapshots.push(build_pane_snapshot(
        floating_pane_id,
        Some(build_filled_grid(22, 4, 'f')),
        0,
        0,
    ));
    let viewer_chrome = ViewerChrome {
        floating_pane_move_draft: Some(FloatingPaneMoveDraft {
            pane_id: floating_pane_id,
            top_left_cell: Point { column: 50, row: 9 },
        }),
        ..ViewerChrome::default()
    };
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let screen_buffer = paint_render_snapshot(&render_snapshot, &committed_regions, viewer_chrome);
    let mouse_frame = MouseFrame::from_snapshot(&render_snapshot, committed_regions, viewer_chrome);
    let frame_layout = mouse_frame.build_frame_layout();

    // The 24x8 pane in the 60x12 pane area, which starts at screen row 1,
    // draws with its top-left cell at pane-area (36, 4).
    let floating_content_rect = find_pane_content_rect(frame_layout, floating_pane_id)
        .unwrap_or_else(|| panic!("the floating pane has no content rect"));
    assert_eq!(floating_content_rect, build_cell_rect(37, 8, 22, 4));
    for row_index in 8..12 {
        for column_index in 37..59 {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            assert_eq!(
                resolve_hit_region(frame_layout, screen_point),
                HitRegion::PaneContent {
                    pane_id: floating_pane_id
                },
                "({column_index}, {row_index}) does not hit the moved pane"
            );
            assert_eq!(
                screen_buffer[(column_index, row_index)].symbol(),
                "f",
                "({column_index}, {row_index}) was not painted with the moved pane"
            );
        }
    }
}

#[test]
fn a_tab_ribbon_spells_the_tab_it_hits() {
    let (render_snapshot, ..) = build_tiled_frame();
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let screen_buffer = paint_render_snapshot(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );
    let tabs_metadata = render_snapshot.session_snapshot.tabs_metadata.clone();
    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions, ViewerChrome::default());
    let frame_layout = mouse_frame.build_frame_layout();

    for tab_metadata in &tabs_metadata {
        let tab_column_indexes: Vec<u16> = (0..TEST_VIEWPORT_SIZE.column_count)
            .filter(|&column_index| {
                resolve_hit_region(
                    frame_layout,
                    Point {
                        column: column_index,
                        row: 0,
                    },
                ) == HitRegion::Tab {
                    tab_id: tab_metadata.tab_id,
                }
            })
            .collect();
        let first_column_index = *tab_column_indexes.first().expect("every tab fits this row");
        let last_column_index = *tab_column_indexes.last().expect("every tab fits this row");
        assert_eq!(
            tab_column_indexes.len() as u16,
            last_column_index - first_column_index + 1,
            "tab {} hits a broken column run",
            tab_metadata.tab_name
        );
        assert_eq!(
            get_row_text(&screen_buffer, 0, first_column_index, last_column_index + 1),
            format!(
                " #{}  {} ",
                tab_metadata.tab_index + 1,
                tab_metadata.tab_name
            ),
            "tab {} hits columns that spell something else",
            tab_metadata.tab_name
        );
    }
}

#[test]
fn the_cursor_lands_in_the_focused_pane_content() {
    let (render_snapshot, left_pane_id, ..) = build_tiled_frame();
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let cursor_position = get_cursor_position(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(),
    )
    .expect("the focused pane has a cursor");
    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions, ViewerChrome::default());
    let frame_layout = mouse_frame.build_frame_layout();

    let content_rect =
        find_pane_content_rect(frame_layout, left_pane_id).expect("the focused pane is drawn");
    let cursor_point = Point {
        column: cursor_position.x,
        row: cursor_position.y,
    };
    assert!(
        content_rect.is_point_inside(cursor_point),
        "cursor {cursor_point:?} is outside {content_rect:?}"
    );
    assert_eq!(cursor_point.column, content_rect.origin.column + 7);
    assert_eq!(cursor_point.row, content_rect.origin.row + 3);
    assert_eq!(
        resolve_hit_region(frame_layout, cursor_point),
        HitRegion::PaneContent {
            pane_id: left_pane_id
        },
        "the cursor cell does not hit the pane it belongs to"
    );
}

#[test]
fn a_zero_height_chrome_region_paints_nothing() {
    let only_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(
        TEST_VIEWPORT_SIZE,
        vec![build_pane_slot(
            only_pane_id,
            build_cell_rect(
                0,
                0,
                TEST_VIEWPORT_SIZE.column_count,
                TEST_VIEWPORT_SIZE.row_count,
            ),
        )],
        vec![build_pane_snapshot(only_pane_id, None, 0, 0)],
        Vec::new(),
        None,
    );
    let empty_height_rect = Rect::from_origin_and_size(
        Point { column: 0, row: 0 },
        Size {
            column_count: TEST_VIEWPORT_SIZE.column_count,
            row_count: 0,
        },
    );
    let committed_regions = CommittedRegions::from_solved_regions(
        TEST_VIEWPORT_SIZE,
        SolvedRegions {
            region_rects: vec![empty_height_rect, empty_height_rect],
            pane_rect: Rect::from_size_at_origin(TEST_VIEWPORT_SIZE),
        },
        0,
    );
    let screen_buffer = paint_render_snapshot(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
    );
    let mouse_frame =
        MouseFrame::from_snapshot(&render_snapshot, committed_regions, ViewerChrome::default());
    let frame_layout = mouse_frame.build_frame_layout();

    let top_row_text = get_row_text(&screen_buffer, 0, 0, TEST_VIEWPORT_SIZE.column_count);
    assert!(
        !top_row_text.contains("work") && !top_row_text.contains("BASE"),
        "a zero-height tabline region painted the tab bar: {top_row_text:?}"
    );
    assert_eq!(
        top_row_text,
        format!(
            "┌{}┐",
            "─".repeat(usize::from(TEST_VIEWPORT_SIZE.column_count) - 2)
        )
    );
    assert_eq!(
        resolve_hit_region(frame_layout, Point { column: 0, row: 0 }),
        HitRegion::PaneBorder {
            pane_id: only_pane_id,
            side: Direction::Left,
            corner_horizontal_side: Some(Direction::Up),
        }
    );
}
