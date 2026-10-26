//! The crate's two entry points read one frame the same way: the region
//! [`resolve_hit_region`] reports for a cell is the region [`render_frame`] painted
//! there.
//!
//! Each test paints one frame into a buffer and classifies the same frame cell
//! by cell, then checks the two against each other: a chrome row is on the row
//! its committed region owns, a pane's content cells sit inside the rect
//! [`find_pane_content_rect`] gives for that pane, a border cell carries a box
//! glyph, a shown tiled handle cell carries `⠿` and the title cells after it
//! hit the top border, a stack header carries the header background, every
//! notification center and toast control carries its label, and every
//! unclassified cell carries the letterbox backdrop.

use std::collections::VecDeque;
use std::time::SystemTime;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;

use koshi_config::types::ToastPosition;
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_core::notification::{
    NotificationEntry, NotificationEpoch, NotificationId, NotificationQueueOwner,
    NotificationSeverity, NotificationSource,
};
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::SolvedRegions;
use koshi_layout::solver::StackHeader;
use koshi_renderer::notification::{
    NotificationCenterEntries, NotificationCenterEntry, NotificationToastContent,
    NotificationToastView, NotificationView, ToastTextRowCache,
};
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, DrawnNotifications, FloatingPane,
    FloatingPaneMoveDraft, FloatingPaneSlot, FloatingPaneTray, GridView, KeymapHints, MouseFrame,
    NotificationButton, NotificationCenter, NotificationToast, PaneSlot, PaneSnapshot,
    PlacementHandleHover, RenderSnapshot, ScrollbackMetadata, SessionSnapshot, TabMetadata,
    TabSnapshot, ViewerChrome,
};
use koshi_renderer::theme::Theme;
use koshi_renderer::{
    build_frame_geometry, find_pane_content_rect, get_cursor_position, render_frame,
    resolve_hit_region, DrawnRows, HitRegion, ImageRenderMode,
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
            floating_panes: Vec::new(),
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

/// A `viewport_size` viewport as a ratatui area at the origin.
fn build_viewport_area(viewport_size: Size) -> RatatuiRect {
    RatatuiRect::new(0, 0, viewport_size.column_count, viewport_size.row_count)
}

/// Paint `render_snapshot` with `committed_regions` and `viewer_chrome` into a
/// fresh viewport-sized buffer, with no notification.
fn paint_render_snapshot(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
) -> Buffer {
    paint_render_snapshot_with_notifications(
        render_snapshot,
        committed_regions,
        viewer_chrome,
        NotificationView {
            center_entries: &NotificationCenterEntries::default(),
            dropped_entry_count: 0,
            shown_toast: None,
        },
    )
}

/// Paint `render_snapshot` with `committed_regions`, `viewer_chrome` and
/// `notification_view` into a fresh buffer the size of `render_snapshot`'s
/// viewport.
fn paint_render_snapshot_with_notifications(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
    notification_view: NotificationView<'_>,
) -> Buffer {
    let viewport_area = build_viewport_area(render_snapshot.client_snapshot.viewport_size);
    let mut screen_buffer = Buffer::empty(viewport_area);
    render_frame(
        &build_frame_geometry(
            render_snapshot,
            committed_regions,
            viewer_chrome,
            viewport_area,
        ),
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ImageRenderMode::Placeholder,
        &[],
        None,
        None,
        notification_view,
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
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        ViewerChrome::default(),
        DrawnNotifications::default(),
    );
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
                | HitRegion::PaneCloseButton { .. }
                | HitRegion::FloatingPaneTrayButton
                | HitRegion::FloatingPaneTrayTile { .. }
                | HitRegion::FloatingPaneTrayToggleMinimizeAll
                | HitRegion::FloatingPaneTrayClose
                | HitRegion::FloatingPaneTray => {
                    panic!(
                        "floating pane region at ({column_index}, {row_index}) in a frame with no floating pane"
                    );
                }
                HitRegion::NotificationButton
                | HitRegion::NotificationCenterClose
                | HitRegion::NotificationCenterClearAll
                | HitRegion::NotificationCenterBody
                | HitRegion::NotificationCenter
                | HitRegion::NotificationToastClose => {
                    panic!(
                        "notification region at ({column_index}, {row_index}) in a frame with no notification"
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
fn a_shown_tiled_handle_paints_and_hits_the_three_cells_before_the_title() {
    let (mut render_snapshot, left_pane_id, ..) = build_tiled_frame();
    render_snapshot.pane_snapshots[0].pane_title = Some("user@host".to_string());
    let viewer_chrome = ViewerChrome {
        placement_handle_hover: Some(PlacementHandleHover::TiledTopBorder {
            pane_id: left_pane_id,
        }),
        ..ViewerChrome::default()
    };
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let screen_buffer = paint_render_snapshot(&render_snapshot, &committed_regions, viewer_chrome);
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        viewer_chrome,
        DrawnNotifications::default(),
    );
    let frame_layout = mouse_frame.build_frame_layout();
    // The left pane's 25x10 box starts at screen (5, 2).
    assert_eq!(
        find_pane_content_rect(frame_layout, left_pane_id),
        Some(build_cell_rect(6, 3, 23, 8))
    );

    let top_row_hit_regions: Vec<HitRegion> = (5..30)
        .map(|column_index| {
            resolve_hit_region(
                frame_layout,
                Point {
                    column: column_index,
                    row: 2,
                },
            )
        })
        .collect();

    assert_eq!(
        get_row_text(&screen_buffer, 2, 5, 30),
        format!("┌⠿⠿⠿ user@host {}┐", "─".repeat(9))
    );
    let mut expected_hit_regions = vec![HitRegion::PaneBorder {
        pane_id: left_pane_id,
        side: Direction::Left,
        corner_horizontal_side: Some(Direction::Up),
    }];
    expected_hit_regions.extend(
        [HitRegion::PlacementHandle {
            pane_id: left_pane_id,
        }; 3],
    );
    expected_hit_regions.extend(
        [HitRegion::PaneBorder {
            pane_id: left_pane_id,
            side: Direction::Up,
            corner_horizontal_side: None,
        }; 20],
    );
    expected_hit_regions.push(HitRegion::PaneBorder {
        pane_id: left_pane_id,
        side: Direction::Right,
        corner_horizontal_side: Some(Direction::Up),
    });
    assert_eq!(top_row_hit_regions, expected_hit_regions);
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
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        ViewerChrome::default(),
        DrawnNotifications::default(),
    );
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
fn every_open_tray_cell_classifies_as_what_was_painted() {
    let (mut render_snapshot, ..) = build_tiled_frame();
    let htop_pane_id = PaneId::new();
    // Pane-area (1, 0) is screen (1, 1): the float lies under the tray's left
    // part.
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            htop_pane_id,
            build_cell_rect(1, 0, 24, 8),
            false,
        ));
    let mut htop_pane_snapshot =
        build_pane_snapshot(htop_pane_id, Some(build_filled_grid(22, 4, 'f')), 0, 0);
    htop_pane_snapshot.pane_title = Some("htop".to_string());
    render_snapshot.pane_snapshots.push(htop_pane_snapshot);
    render_snapshot.session_snapshot.floating_panes = vec![
        FloatingPane::Shown {
            pane_id: htop_pane_id,
        },
        FloatingPane::Minimized {
            pane_id: PaneId::new(),
            pane_title: Some("logs".to_string()),
        },
    ];
    let viewer_chrome = ViewerChrome {
        active_input_mode: Some(LockMode::FloatingPaneTray),
        floating_pane_tray: Some(FloatingPaneTray {
            selected_tile_index: 0,
            hovered_tile_index: None,
        }),
        ..ViewerChrome::default()
    };
    // A 15-row viewport leaves a 13-row pane area: the height of a tray of
    // three rows of slots.
    let tray_viewport_size = Size {
        column_count: TEST_VIEWPORT_SIZE.column_count,
        row_count: 15,
    };
    render_snapshot.client_snapshot.viewport_size = tray_viewport_size;
    let committed_regions = CommittedRegions::build_core(tray_viewport_size, 0);
    let screen_buffer = paint_render_snapshot(&render_snapshot, &committed_regions, viewer_chrome);
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        viewer_chrome,
        DrawnNotifications::default(),
    );
    let frame_layout = mouse_frame.build_frame_layout();

    // ` TRAY ` takes columns 54 to 59 and the button 51 to 53. Four slots per
    // row fit the 54 columns left of the tray's right border: the tray is
    // 47x13 at (7, 1), slot 0 at (9, 4) and slot 1 at (20, 4).
    assert_eq!(get_row_text(&screen_buffer, 0, 51, 54), " ⧉ ");
    for column_index in 51..54 {
        assert_eq!(
            resolve_hit_region(
                frame_layout,
                Point {
                    column: column_index,
                    row: 0
                }
            ),
            HitRegion::FloatingPaneTrayButton
        );
    }
    // The tray paints, row by row from (7, 1): the top border, the titlebar
    // with the selected tile's title, the label at columns 36 to 47 and the
    // close button at column 50, the separator, three rows of four slots,
    // and the bottom border.
    let empty_slot_rows = ["┌────────┐", "│        │", "└────────┘"];
    let expected_tray_rows = vec![
        format!("┌{}┐", "─".repeat(45)),
        format!("│ htop{}minimize all  ✕  │", " ".repeat(23)),
        format!("├{}┤", "─".repeat(45)),
        format!("│ ┌ 1 ─────┐ ┌ 2 ─────┐ {0} {0} │", empty_slot_rows[0]),
        format!("│ │ htop   │ │ logs   │ {0} {0} │", empty_slot_rows[1]),
        format!("│ └────────┘ └────────┘ {0} {0} │", empty_slot_rows[2]),
        format!("│ {0} {0} {0} {0} │", empty_slot_rows[0]),
        format!("│ {0} {0} {0} {0} │", empty_slot_rows[1]),
        format!("│ {0} {0} {0} {0} │", empty_slot_rows[2]),
        format!("│ {0} {0} {0} {0} │", empty_slot_rows[0]),
        format!("│ {0} {0} {0} {0} │", empty_slot_rows[1]),
        format!("│ {0} {0} {0} {0} │", empty_slot_rows[2]),
        format!("└{}┘", "─".repeat(45)),
    ];
    let painted_tray_rows: Vec<String> = (1..14)
        .map(|row_index| get_row_text(&screen_buffer, row_index, 7, 54))
        .collect();
    assert_eq!(painted_tray_rows, expected_tray_rows);

    // Every cell of the tray hits the region painted there: tile 0's box at
    // (9, 4) 10x3, tile 1's box at (20, 4) 10x3, the label's 12 cells and the
    // close button on row 2, and the tray for every other cell, the empty
    // slots included.
    let tile_rects = [build_cell_rect(9, 4, 10, 3), build_cell_rect(20, 4, 10, 3)];
    let label_rect = build_cell_rect(36, 2, 12, 1);
    let close_button_point = Point { column: 50, row: 2 };
    for row_index in 1..14 {
        for column_index in 7..54 {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            let expected_hit_region = if tile_rects[0].is_point_inside(screen_point) {
                HitRegion::FloatingPaneTrayTile { tile_index: 0 }
            } else if tile_rects[1].is_point_inside(screen_point) {
                HitRegion::FloatingPaneTrayTile { tile_index: 1 }
            } else if label_rect.is_point_inside(screen_point) {
                HitRegion::FloatingPaneTrayToggleMinimizeAll
            } else if screen_point == close_button_point {
                HitRegion::FloatingPaneTrayClose
            } else {
                HitRegion::FloatingPaneTray
            };
            assert_eq!(
                resolve_hit_region(frame_layout, screen_point),
                expected_hit_region,
                "({column_index}, {row_index})"
            );
        }
    }
}

#[test]
fn every_notification_center_toast_and_button_cell_classifies_as_what_was_painted() {
    let (render_snapshot, ..) = build_tiled_frame();
    let notification_entries = VecDeque::from([NotificationEntry {
        notification_id: NotificationId {
            owner: NotificationQueueOwner::Session(SessionId::new()),
            epoch: NotificationEpoch::new(),
            sequence: 0,
        },
        source: NotificationSource::Core,
        severity: NotificationSeverity::Error,
        text: "pane exited".to_string(),
        created_at: SystemTime::UNIX_EPOCH,
        repeat_count: 1,
    }]);
    let viewer_chrome = ViewerChrome {
        active_input_mode: Some(LockMode::NotificationCenter),
        notification_center: Some(NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: None,
            hidden_per_mille: 0,
        }),
        notification_button: Some(NotificationButton {
            unread_entry_count: 3,
        }),
        notification_toast: Some(NotificationToast {
            toast_position: ToastPosition::BottomLeft,
            text_row_count: 1,
            hidden_per_mille: 0,
        }),
        ..ViewerChrome::default()
    };
    let committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);
    let mut center_entries = NotificationCenterEntries::default();
    for notification_entry in &notification_entries {
        center_entries.add_newest_center_entry(NotificationCenterEntry::from_notification_entry(
            notification_entry.clone(),
        ));
    }
    let screen_buffer = paint_render_snapshot_with_notifications(
        &render_snapshot,
        &committed_regions,
        viewer_chrome,
        NotificationView {
            center_entries: &center_entries,
            dropped_entry_count: 0,
            shown_toast: Some(NotificationToastView {
                toast_content: NotificationToastContent::Entry(&notification_entries[0]),
                toast_text_row_cache: &ToastTextRowCache::default(),
            }),
        },
    );
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        viewer_chrome,
        DrawnNotifications::default(),
    );
    let frame_layout = mouse_frame.build_frame_layout();

    // The mode tag ` NOTIFICATIONS ` takes columns 45 to 59 and the button
    // ` ●3 ` the 4 columns before it.
    assert_eq!(get_row_text(&screen_buffer, 0, 45, 60), " NOTIFICATIONS ");
    assert_eq!(get_row_text(&screen_buffer, 0, 41, 45), " ●3 ");
    for column_index in 41..45 {
        assert_eq!(
            resolve_hit_region(
                frame_layout,
                Point {
                    column: column_index,
                    row: 0
                }
            ),
            HitRegion::NotificationButton
        );
    }

    // The center is 24x12 at (36, 1): its close button at (56, 2), its body
    // at (37, 4) 22x7 with the entry's card on rows 4 to 7, and `Clear all`
    // at columns 43 to 51 of row 11.
    let expected_center_rows = vec![
        format!("┌{}┐", "─".repeat(22)),
        "│ Notifications     ✕  │".to_string(),
        format!("├{}┤", "─".repeat(22)),
        format!("│┌{} ✕ ─┐│", "─".repeat(16)),
        format!("││ [Koshi] Error{}││", " ".repeat(6)),
        format!("││ pane exited{}││", " ".repeat(8)),
        format!("│└{}┘│", "─".repeat(20)),
        format!("│{}│", " ".repeat(22)),
        format!("│{}│", " ".repeat(22)),
        format!("│{}│", " ".repeat(22)),
        "│      Clear all       │".to_string(),
        format!("└{}┘", "─".repeat(22)),
    ];
    let painted_center_rows: Vec<String> = (1..13)
        .map(|row_index| get_row_text(&screen_buffer, row_index, 36, 60))
        .collect();
    assert_eq!(painted_center_rows, expected_center_rows);
    let center_close_point = Point { column: 56, row: 2 };
    let center_body_rect = build_cell_rect(37, 4, 22, 7);
    let center_clear_all_rect = build_cell_rect(43, 11, 9, 1);
    for row_index in 1..13 {
        for column_index in 36..60 {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            let expected_hit_region = if screen_point == center_close_point {
                HitRegion::NotificationCenterClose
            } else if center_clear_all_rect.is_point_inside(screen_point) {
                HitRegion::NotificationCenterClearAll
            } else if center_body_rect.is_point_inside(screen_point) {
                HitRegion::NotificationCenterBody
            } else {
                HitRegion::NotificationCenter
            };
            assert_eq!(
                resolve_hit_region(frame_layout, screen_point),
                expected_hit_region,
                "({column_index}, {row_index})"
            );
        }
    }

    // The toast is 24x4 at (0, 9): its close control at columns 19 to 21 of
    // row 9. Every other toast cell hits what lies under the toast.
    let expected_toast_rows = vec![
        format!("┌{} ✕ ─┐", "─".repeat(18)),
        format!("│ [Koshi] Error{}│", " ".repeat(8)),
        format!("│ pane exited{}│", " ".repeat(10)),
        format!("└{}┘", "─".repeat(22)),
    ];
    let painted_toast_rows: Vec<String> = (9..13)
        .map(|row_index| get_row_text(&screen_buffer, row_index, 0, 24))
        .collect();
    assert_eq!(painted_toast_rows, expected_toast_rows);
    let toast_close_rect = build_cell_rect(19, 9, 3, 1);
    for row_index in 9..13 {
        for column_index in 0..24 {
            let screen_point = Point {
                column: column_index,
                row: row_index,
            };
            let hit_region = resolve_hit_region(frame_layout, screen_point);
            if toast_close_rect.is_point_inside(screen_point) {
                assert_eq!(
                    hit_region,
                    HitRegion::NotificationToastClose,
                    "({column_index}, {row_index})"
                );
            } else {
                assert_ne!(
                    hit_region,
                    HitRegion::NotificationToastClose,
                    "({column_index}, {row_index})"
                );
            }
        }
    }
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
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        viewer_chrome,
        DrawnNotifications::default(),
    );
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
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        ViewerChrome::default(),
        DrawnNotifications::default(),
    );
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
    let cursor_position = get_cursor_position(&build_frame_geometry(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_viewport_area(TEST_VIEWPORT_SIZE),
    ))
    .expect("the focused pane has a cursor");
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        ViewerChrome::default(),
        DrawnNotifications::default(),
    );
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
    let mouse_frame = MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        ViewerChrome::default(),
        DrawnNotifications::default(),
    );
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
