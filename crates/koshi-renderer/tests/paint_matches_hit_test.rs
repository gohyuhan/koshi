//! The crate's two entry points read one frame the same way: the region
//! [`hit_test`] reports for a cell is the region [`render_frame`] painted
//! there.
//!
//! Each test paints one frame into a buffer and classifies the same frame cell
//! by cell, then checks the two against each other: a chrome row is on the row
//! its committed region owns, a pane's content cells sit inside the rect
//! [`pane_content_rect`] gives for that pane, a border cell carries a box
//! glyph, a stack header carries the header background, and every unclassified
//! cell carries the letterbox backdrop.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::SolvedRegions;
use koshi_layout::solver::StackHeader;
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, GridView, KeymapHints, MouseFrame, PaneKind,
    PaneSlot, PaneSnapshot, PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SessionSnapshot,
    TabMeta, TabSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{get_cursor_position, hit_test, pane_content_rect, render_frame, HitRegion};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style as CellStyle;

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

/// The glyphs `Block` draws a full border ring with.
const BORDER_GLYPHS: [&str; 6] = ["┌", "┐", "└", "┘", "─", "│"];

fn build_cell_rect(column: u16, row_index: u16, column_count: u16, row_count: u16) -> Rect {
    Rect {
        origin: Point {
            column,
            row: row_index,
        },
        cell_size: Size {
            column_count,
            row_count,
        },
    }
}

/// A visible pane slot whose content area is `outer_rect` inset by its one-cell
/// border.
fn build_pane_slot(pane_id: PaneId, outer_rect: Rect) -> PaneSlot {
    PaneSlot {
        pane_id,
        outer_rect,
        content_rect: Some(outer_rect.compute_inner_with_border()),
        pane_kind: PaneKind::Terminal,
        is_visible: true,
        is_suppressed: false,
        is_dead: false,
    }
}

/// A pane whose every cell holds `fill_character`, sized `column_count x row_count`.
fn build_filled_grid(column_count: u16, row_count: u16, fill_character: char) -> GridView {
    let cells = vec![
        vec![
            Cell::from_character(fill_character, 1, CellStyle::default());
            column_count as usize
        ];
        row_count as usize
    ];
    GridView {
        grid: Arc::new(Grid::from_rows(cells, column_count, CellStyle::default())),
        view_row_offset: 0,
    }
}

/// One pane's content with the cursor at `(cursor_row_index, cursor_column_index)` of its content area.
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
        scrollback_meta: ScrollbackMeta {
            is_truncated: false,
            retained_line_count: 0,
        },
    }
}

/// A frame with `pane_slots` laid out for `effective_size`, `headers` on top of them, and
/// three tabs.
fn build_render_snapshot(
    effective_size: Size,
    pane_slots: Vec<PaneSlot>,
    pane_snapshots: Vec<PaneSnapshot>,
    headers: Vec<StackHeader>,
    focused_pane_id: Option<PaneId>,
) -> RenderSnapshot {
    let tab_id = TabId::new();
    let tabs_metadata = TAB_NAMES
        .iter()
        .enumerate()
        .map(|(tab_index, tab_name)| TabMeta {
            tab_id: TabId::new(),
            tab_name: (*tab_name).to_string(),
            tab_index,
            is_active: tab_index == 0,
        })
        .collect();
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: "work".to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "one".to_string(),
                pane_slots,
                effective_cell_size: effective_size,
                stack_headers: headers,
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata,
        },
        pane_snapshots,
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id,
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

/// Two panes tiling [`EFFECTIVE_LAYOUT_SIZE`] side by side, a one-row stack header covering
/// the right pane's first content row, and the left pane filled with `a`.
///
/// Returns the frame, the left pane, the right pane, and the header's pane.
fn build_tiled_frame() -> (RenderSnapshot, PaneId, PaneId, PaneId) {
    let left = PaneId::new();
    let right = PaneId::new();
    let collapsed = PaneId::new();
    let left_outer = build_cell_rect(0, 0, 25, 10);
    let right_outer = build_cell_rect(25, 0, 25, 10);
    let header = StackHeader {
        pane_id: collapsed,
        header_rect: build_cell_rect(26, 1, 23, 1),
        member_index: 0,
        member_count: 2,
    };
    let frame = build_render_snapshot(
        EFFECTIVE_LAYOUT_SIZE,
        vec![
            build_pane_slot(left, left_outer),
            build_pane_slot(right, right_outer),
        ],
        vec![
            build_pane_snapshot(left, Some(build_filled_grid(23, 8, 'a')), 3, 7),
            build_pane_snapshot(right, None, 0, 0),
        ],
        vec![header],
        Some(left),
    );
    (frame, left, right, collapsed)
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

/// Paint `frame` with `regions` into a fresh viewport-sized buffer.
fn paint_render_snapshot(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
) -> Buffer {
    let viewport_area = build_viewport_area();
    let mut buffer = Buffer::empty(viewport_area);
    render_frame(
        render_snapshot,
        committed_regions,
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ViewerChrome::default(),
        viewport_area,
        &mut buffer,
    );
    buffer
}

/// The text painted across the half-open column span `[from, to)` of `row`.
fn get_row_text(
    buffer: &Buffer,
    row_index: u16,
    starting_column: u16,
    ending_column: u16,
) -> String {
    (starting_column..ending_column)
        .map(|column| buffer[(column, row_index)].symbol())
        .collect()
}

#[test]
fn every_cell_classifies_as_what_was_painted() {
    let (frame, left, right, collapsed) = build_tiled_frame();
    let regions = CommittedRegions::core(TEST_VIEWPORT_SIZE, 0);
    let buffer = paint_render_snapshot(&frame, &regions);
    let theme = Theme::default();
    let mouse = MouseFrame::from_snapshot_with_regions(frame, regions);
    let layout = mouse.build_frame_layout(ViewerChrome::default());

    let statusline_row = TEST_VIEWPORT_SIZE.row_count - 1;
    let mut seen_content = 0_u32;
    let mut seen_border = 0_u32;
    let mut seen_header = 0_u32;
    let mut seen_letterbox = 0_u32;

    for row_index in 0..TEST_VIEWPORT_SIZE.row_count {
        for column in 0..TEST_VIEWPORT_SIZE.column_count {
            let point = Point {
                column,
                row: row_index,
            };
            let painted_cell = &buffer[(column, row_index)];
            match hit_test(layout, point) {
                HitRegion::Tabline
                | HitRegion::Tab { .. }
                | HitRegion::TablineScrollLeft { .. }
                | HitRegion::TablineScrollRight { .. } => {
                    assert_eq!(
                        row_index, 0,
                        "tabline classified off the tabline row at ({column}, {row_index})"
                    );
                }
                HitRegion::Statusline => {
                    assert_eq!(
                        row_index, statusline_row,
                        "statusline classified off its row at ({column}, {row_index})"
                    );
                    assert_eq!(
                        painted_cell.bg, theme.bar_background_color,
                        "statusline cell ({column}, {row_index}) is not the bar background"
                    );
                }
                HitRegion::PaneContent { pane_id } => {
                    let content_rect = pane_content_rect(layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        content_rect.is_point_inside(point),
                        "content hit at ({column}, {row_index}) is outside {content_rect:?}"
                    );
                    if pane_id == left {
                        assert_eq!(
                            painted_cell.symbol(),
                            "a",
                            "left pane content cell ({column}, {row_index}) was not painted"
                        );
                    } else {
                        assert_eq!(
                            pane_id, right,
                            "unexpected pane hit at ({column}, {row_index})"
                        );
                    }
                    seen_content += 1;
                }
                HitRegion::PaneBorder { pane_id, .. } => {
                    let content_rect = pane_content_rect(layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        !content_rect.is_point_inside(point),
                        "border hit at ({column}, {row_index}) is inside {content_rect:?}"
                    );
                    assert!(
                        BORDER_GLYPHS.contains(&painted_cell.symbol()),
                        "border hit at ({column}, {row_index}) painted {:?}",
                        painted_cell.symbol()
                    );
                    seen_border += 1;
                }
                HitRegion::PlacementHandle { pane_id } => {
                    let content_rect = pane_content_rect(layout, pane_id)
                        .unwrap_or_else(|| panic!("pane {pane_id:?} has no content rect"));
                    assert!(
                        !content_rect.is_point_inside(point),
                        "placement handle hit at ({column}, {row_index}) is inside {content_rect:?}"
                    );
                    assert_eq!(
                        painted_cell.symbol(),
                        "⠿",
                        "placement handle cell ({column}, {row_index}) was not painted"
                    );
                    seen_border += 1;
                }
                HitRegion::StackHeader { pane_id } => {
                    assert_eq!(pane_id, collapsed);
                    assert_eq!(
                        painted_cell.bg, theme.stack_header_background_color,
                        "stack header cell ({column}, {row_index}) is not the header background"
                    );
                    seen_header += 1;
                }
                HitRegion::None => {
                    assert_eq!(
                        painted_cell.bg, theme.letterbox_color,
                        "unclassified cell ({column}, {row_index}) is not letterbox margin"
                    );
                    seen_letterbox += 1;
                }
            }
        }
    }

    // Both panes' content minus the row the stack header takes.
    assert_eq!(seen_content, 2 * 23 * 8 - 23);
    assert_eq!(seen_border, 2 * (25 * 10 - 23 * 8));
    assert_eq!(seen_header, 23);
    // The 60x14 viewport less the two chrome rows and the centered 50x10 layout.
    assert_eq!(
        seen_letterbox,
        u32::from(
            TEST_VIEWPORT_SIZE.column_count * 12
                - EFFECTIVE_LAYOUT_SIZE.column_count * EFFECTIVE_LAYOUT_SIZE.row_count
        )
    );
}

#[test]
fn a_tab_ribbon_spells_the_tab_it_hits() {
    let (frame, ..) = build_tiled_frame();
    let regions = CommittedRegions::core(TEST_VIEWPORT_SIZE, 0);
    let buffer = paint_render_snapshot(&frame, &regions);
    let tabs_metadata = frame.session_snapshot.tabs_metadata.clone();
    let mouse = MouseFrame::from_snapshot_with_regions(frame, regions);
    let layout = mouse.build_frame_layout(ViewerChrome::default());

    for tab_metadata in &tabs_metadata {
        let tab_columns: Vec<u16> = (0..TEST_VIEWPORT_SIZE.column_count)
            .filter(|&column| {
                hit_test(layout, Point { column, row: 0 })
                    == HitRegion::Tab {
                        tab_id: tab_metadata.tab_id,
                    }
            })
            .collect();
        let first_column = *tab_columns.first().expect("every tab fits this row");
        let last_column = *tab_columns.last().expect("every tab fits this row");
        assert_eq!(
            tab_columns.len() as u16,
            last_column - first_column + 1,
            "tab {} hits a broken column run",
            tab_metadata.tab_name
        );
        assert_eq!(
            get_row_text(&buffer, 0, first_column, last_column + 1),
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
    let (frame, left, ..) = build_tiled_frame();
    let regions = CommittedRegions::core(TEST_VIEWPORT_SIZE, 0);
    let position = get_cursor_position(&frame, &regions, build_viewport_area())
        .expect("the focused pane has a cursor");
    let mouse = MouseFrame::from_snapshot_with_regions(frame, regions);
    let layout = mouse.build_frame_layout(ViewerChrome::default());

    let content_rect = pane_content_rect(layout, left).expect("the focused pane is drawn");
    let point = Point {
        column: position.x,
        row: position.y,
    };
    assert!(
        content_rect.is_point_inside(point),
        "cursor {point:?} is outside {content_rect:?}"
    );
    assert_eq!(point.column, content_rect.origin.column + 7);
    assert_eq!(point.row, content_rect.origin.row + 3);
    assert_eq!(
        hit_test(layout, point),
        HitRegion::PaneContent { pane_id: left },
        "the cursor cell does not hit the pane it belongs to"
    );
}

#[test]
fn a_zero_height_chrome_region_paints_nothing() {
    let only = PaneId::new();
    let frame = build_render_snapshot(
        TEST_VIEWPORT_SIZE,
        vec![build_pane_slot(
            only,
            build_cell_rect(
                0,
                0,
                TEST_VIEWPORT_SIZE.column_count,
                TEST_VIEWPORT_SIZE.row_count,
            ),
        )],
        vec![build_pane_snapshot(only, None, 0, 0)],
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
    let regions = CommittedRegions::from_solved_regions(
        TEST_VIEWPORT_SIZE,
        SolvedRegions {
            region_rects: vec![empty_height_rect, empty_height_rect],
            pane_rect: Rect::from_size_at_origin(TEST_VIEWPORT_SIZE),
        },
        0,
    );
    let buffer = paint_render_snapshot(&frame, &regions);
    let mouse = MouseFrame::from_snapshot_with_regions(frame, regions);
    let layout = mouse.build_frame_layout(ViewerChrome::default());

    let top = get_row_text(&buffer, 0, 0, TEST_VIEWPORT_SIZE.column_count);
    assert!(
        !top.contains("work") && !top.contains("BASE"),
        "a zero-height tabline region painted the tab bar: {top:?}"
    );
    assert_eq!(
        top,
        format!(
            "┌{}┐",
            "─".repeat(usize::from(TEST_VIEWPORT_SIZE.column_count) - 2)
        )
    );
    assert_eq!(
        hit_test(layout, Point { column: 0, row: 0 }),
        HitRegion::PaneBorder {
            pane_id: only,
            side: koshi_core::geometry::Direction::Left,
        }
    );
}
