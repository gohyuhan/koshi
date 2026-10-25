//! Tests for image destination geometry, source cropping, filtering and the
//! unavailable-image text, and for the images and cells of floating panes: a
//! floating pane's image is anchored at its content area in pane-area cells
//! and cut to the pane area, an image that a floating pane drawn above its
//! pane meets is left out and shows no unavailable-image text in native mode,
//! and the cell snapshot writes each floating pane over the tiled cells, cut
//! to the pane area.

use super::*;

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::regions::SolvedRegions;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style;
use koshi_test_support::fixtures::build_cell_rect;

use crate::render::tests::{build_core_regions, format_rendered_cells, get_pane_snapshot_mut};
use crate::render::DrawnRows;
use crate::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, FloatingPaneSlot, GridView,
    ImagePlacementSnapshot, KeymapHints, PaneSlot, PaneSnapshot, RenderSnapshot,
    ScrollbackMetadata, SelectionSpans, SessionSnapshot, TabMetadata, TabSnapshot, ViewerChrome,
};
use crate::theme::Theme;

fn build_image_record(pixel_width: u32, pixel_height: u32, z_index: i32) -> Arc<ImageRecord> {
    let pixel_count = usize::try_from(pixel_width * pixel_height).expect("test image fits usize");
    Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width,
            pixel_height,
            rgba_bytes: (0..pixel_count * 4)
                .map(|rgba_byte_index| u8::try_from(rgba_byte_index % 256).expect("test byte fits"))
                .collect(),
        })
        .into(),
        animation: None,
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            z_index,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    })
}

fn build_render_snapshot(
    pane_id: PaneId,
    content_rect: Rect,
    image_placement_snapshots: Vec<ImagePlacementSnapshot>,
    has_terminal_grid: bool,
    is_visible: bool,
    is_every_pane_suppressed: bool,
) -> RenderSnapshot {
    let tab_id = TabId::new();
    let viewport_size = Size {
        column_count: 40,
        row_count: 8,
    };
    let pane_snapshot = PaneSnapshot {
        pane_id,
        pane_title: None,
        cursor_snapshot: CursorSnapshot {
            row_index: 0,
            column_index: 0,
            is_visible: false,
            is_blinking: false,
            shape: None,
        },
        terminal_grid_view: has_terminal_grid
            .then(|| GridView::from_grid(&Grid::build_blank(6, 38, Style::default()), 0)),
        image_placement_snapshots,
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
    };
    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("tab"),
                pane_slots: vec![PaneSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        size: viewport_size,
                    },
                    content_rect: Some(content_rect),
                    is_visible,
                    is_suppressed: is_every_pane_suppressed,
                }],
                tab_size: viewport_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: String::from("tab"),
                tab_index: 0,
                is_active: true,
            }],
            floating_pane_slots: Vec::new(),
            floating_panes: Vec::new(),
        },
        pane_snapshots: vec![pane_snapshot],
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            active_tab_focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            is_mouse_selection_enabled: false,
            last_applied_placement_command_id: None,
        },
        placement_preview: None,
    }
}

fn build_regions() -> CommittedRegions {
    CommittedRegions::from_solved_regions(
        Size {
            column_count: 40,
            row_count: 8,
        },
        SolvedRegions {
            region_rects: Vec::new(),
            pane_rect: Rect::from_size_at_origin(Size {
                column_count: 40,
                row_count: 8,
            }),
        },
        0,
    )
}

/// Apply `update_grid` to a grid holding the rows `pane_snapshot` shows, then
/// show the updated rows.
fn update_pane_grid(pane_snapshot: &mut PaneSnapshot, update_grid: impl FnOnce(&mut Grid)) {
    let grid_view = pane_snapshot
        .terminal_grid_view
        .as_mut()
        .expect("the pane shows a grid");
    let mut terminal_grid = Grid::build_blank(
        u16::try_from(grid_view.row_snapshots.len()).expect("the rows fit a grid"),
        grid_view.column_count,
        Style::default(),
    );
    for (row_index, row_snapshot) in (0..).zip(&grid_view.row_snapshots) {
        for (column_index, cell) in (0..).zip(row_snapshot.decode_row_cells()) {
            *terminal_grid
                .get_cell_mut(row_index, column_index)
                .expect("the cell is in the grid") = cell;
        }
    }
    update_grid(&mut terminal_grid);
    *grid_view = GridView::from_grid(&terminal_grid, grid_view.view_row_offset);
}

#[test]
fn image_cell_snapshot_keeps_exact_combining_characters() {
    let mut snapshot = build_render_snapshot(
        PaneId::new(),
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![],
        true,
        true,
        false,
    );
    update_pane_grid(&mut snapshot.pane_snapshots[0], |grid| {
        let mut first_cell = Cell::from_character('e', 1, Style::default());
        first_cell.push_combining('\u{301}');
        *grid.get_cell_mut(0, 0).unwrap() = first_cell;
        let mut second_cell = Cell::from_character('e', 1, Style::default());
        second_cell.push_combining('\u{300}');
        *grid.get_cell_mut(0, 1).unwrap() = second_cell;
    });
    let image_cell_snapshot = build_image_cell_snapshot(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
        build_screen_area(),
    )
    .unwrap();
    assert_eq!(
        image_cell_snapshot.find_cell(1, 1),
        Some(&ImageCellState {
            character: 'e',
            cell_width: 1,
            combining_characters: vec!['\u{301}'],
            style: Style::default(),
        })
    );
    assert_eq!(
        image_cell_snapshot.find_cell(2, 1),
        Some(&ImageCellState {
            character: 'e',
            cell_width: 1,
            combining_characters: vec!['\u{300}'],
            style: Style::default(),
        })
    );
}

#[test]
fn a_snapshot_of_part_of_the_viewport_holds_the_cells_of_the_whole_viewport_there() {
    let mut snapshot = build_render_snapshot(
        PaneId::new(),
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![],
        true,
        true,
        false,
    );
    update_pane_grid(&mut snapshot.pane_snapshots[0], |grid| {
        for (row_index, column_index, character) in [(0, 0, 'a'), (1, 2, 'b'), (4, 7, 'c')] {
            *grid
                .get_cell_mut(row_index, column_index)
                .expect("the cell is in the grid") =
                Cell::from_character(character, 1, Style::default());
        }
    });
    let whole_viewport_snapshot = build_image_cell_snapshot(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
        build_screen_area(),
    )
    .expect("the snapshot fits");
    let part_area = RatatuiRect::new(2, 1, 4, 3);

    let part_snapshot = build_image_cell_snapshot(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
        part_area,
    )
    .expect("the snapshot fits");

    assert_eq!(part_snapshot.screen_area, part_area);
    for screen_row in 0..8 {
        for screen_column in 0..40 {
            let is_in_part = screen_column >= part_area.x
                && screen_column < part_area.right()
                && screen_row >= part_area.y
                && screen_row < part_area.bottom();
            assert_eq!(
                part_snapshot.find_cell(screen_column, screen_row),
                whole_viewport_snapshot
                    .find_cell(screen_column, screen_row)
                    .filter(|_| is_in_part),
                "({screen_column}, {screen_row})"
            );
        }
    }
    assert_eq!(
        part_snapshot.find_cell(3, 2).map(|cell| cell.character),
        Some('b')
    );
    assert_eq!(
        build_image_cell_snapshot(
            &snapshot,
            &build_regions(),
            ViewerChrome::default(),
            build_screen_area(),
            RatatuiRect::new(38, 6, 5, 5),
        )
        .expect("the snapshot fits")
        .screen_area,
        RatatuiRect::new(38, 6, 2, 2)
    );
}

#[test]
fn image_cell_snapshot_matches_screen_reverse_and_selection() {
    let pane_id = PaneId::new();
    let mut snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![],
        true,
        true,
        false,
    );
    snapshot.pane_snapshots[0].is_reverse_video = true;
    snapshot.pane_snapshots[0].selection_spans = Some(SelectionSpans {
        row_spans: vec![(0, 0, 0)],
    });
    update_pane_grid(&mut snapshot.pane_snapshots[0], |grid| {
        let mut reversed_style = Style::default();
        reversed_style.set_reverse(true);
        *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('a', 1, reversed_style);
        *grid.get_cell_mut(0, 1).unwrap() = Cell::from_character('b', 1, Style::default());
        let mut reversed_without_selection = Style::default();
        reversed_without_selection.set_reverse(true);
        *grid.get_cell_mut(0, 2).unwrap() =
            Cell::from_character('c', 1, reversed_without_selection);
    });

    let image_cell_snapshot = build_image_cell_snapshot(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
        build_screen_area(),
    )
    .unwrap();

    assert!(image_cell_snapshot
        .find_cell(1, 1)
        .unwrap()
        .style
        .get_attributes()
        .is_reverse());
    assert!(image_cell_snapshot
        .find_cell(2, 1)
        .unwrap()
        .style
        .get_attributes()
        .is_reverse());
    assert!(!image_cell_snapshot
        .find_cell(3, 1)
        .unwrap()
        .style
        .get_attributes()
        .is_reverse());
}

#[test]
fn image_order_is_one_global_sequence_across_panes() {
    let pane_id = PaneId::new();
    let mut snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![ImagePlacementSnapshot::from_image_record(
            1,
            build_image_record(1, 1, 0),
            (0, 0),
            1,
            1,
        )
        .unwrap()],
        true,
        true,
        false,
    );
    let mut second_pane_snapshot = snapshot.pane_snapshots[0].clone();
    second_pane_snapshot.pane_id = PaneId::new();
    let mut second_pane_slot = snapshot.session_snapshot.active_tab_snapshot.pane_slots[0].clone();
    second_pane_slot.pane_id = second_pane_snapshot.pane_id;
    second_pane_slot
        .content_rect
        .as_mut()
        .unwrap()
        .origin
        .column = 10;
    snapshot.pane_snapshots.push(second_pane_snapshot);
    snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .push(second_pane_slot);
    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
    );
    assert_eq!(
        image_paints
            .iter()
            .map(|image_paint| (image_paint.target_area, image_paint.draw_order))
            .collect::<Vec<_>>(),
        [
            (RatatuiRect::new(1, 1, 1, 1), 0),
            (RatatuiRect::new(10, 1, 1, 1), 1)
        ]
    );
}

#[test]
fn a_scrolled_crop_keeps_the_full_image_scale() {
    let pane_id = PaneId::new();
    let image_placement_snapshot =
        ImagePlacementSnapshot::from_image_record(7, build_image_record(8, 12, 0), (0, 0), 4, 4)
            .expect("valid placement")
            .with_cell_geometry(koshi_core::geometry::ImageCellGeometry {
                full_size: Size {
                    column_count: 4,
                    row_count: 6,
                },
                cell_offset: Point { column: 0, row: 2 },
            })
            .expect("visible crop");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![image_placement_snapshot],
        true,
        true,
        false,
    );
    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
    );
    assert_eq!(
        image_paints
            .iter()
            .map(|image_paint| (image_paint.target_area, image_paint.source_rect))
            .collect::<Vec<_>>(),
        [(
            RatatuiRect::new(1, 1, 4, 4),
            ImageSourceRect {
                pixel_x: 0,
                pixel_y: 4,
                pixel_width: 8,
                pixel_height: 8
            }
        )]
    );
}

#[test]
fn image_paint_keeps_geometry_and_rgba_record() {
    let pane_id = PaneId::new();
    let image_placement_snapshot =
        ImagePlacementSnapshot::from_image_record(7, build_image_record(6, 4, 0), (1, 2), 3, 2)
            .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![image_placement_snapshot],
        true,
        true,
        false,
    );
    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(image_paints.len(), 1);
    assert_eq!(image_paints[0].pane_id, pane_id);
    assert_eq!(image_paints[0].placement_id, 7);
    assert_eq!(
        image_paints[0].target_area,
        RatatuiRect {
            x: 3,
            y: 2,
            width: 3,
            height: 2,
        }
    );
    assert_eq!(
        image_paints[0].source_rect,
        ImageSourceRect {
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: 6,
            pixel_height: 4,
        }
    );
    assert_eq!(image_paints[0].image_record.image.pixel_width, 6);
    assert_eq!(image_paints[0].image_record.image.pixel_height, 4);
    assert_eq!(image_paints[0].image_record.image.rgba_bytes[0], 0);
    assert_eq!(image_paints[0].image_record.image.rgba_bytes[95], 95);
}

#[test]
fn image_paint_crops_right_and_bottom_edges_to_the_pane() {
    let pane_id = PaneId::new();
    let pane_content_rect = Rect {
        origin: Point { column: 2, row: 2 },
        size: Size {
            column_count: 4,
            row_count: 4,
        },
    };
    let placements = vec![
        ImagePlacementSnapshot::from_image_record(1, build_image_record(8, 8, 0), (0, 3), 4, 4)
            .expect("test image placement is valid"),
        ImagePlacementSnapshot::from_image_record(2, build_image_record(8, 8, 0), (3, 0), 4, 4)
            .expect("test image placement is valid"),
    ];
    let snapshot = build_render_snapshot(pane_id, pane_content_rect, placements, true, true, false);
    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(image_paints.len(), 2);
    assert_eq!(image_paints[0].placement_id, 1);
    assert_eq!(
        image_paints[0].target_area,
        RatatuiRect {
            x: 5,
            y: 2,
            width: 1,
            height: 4
        }
    );
    assert_eq!(
        image_paints[0].source_rect,
        ImageSourceRect {
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: 2,
            pixel_height: 8
        }
    );
    assert_eq!(image_paints[1].placement_id, 2);
    assert_eq!(
        image_paints[1].target_area,
        RatatuiRect {
            x: 2,
            y: 5,
            width: 4,
            height: 1
        }
    );
    assert_eq!(
        image_paints[1].source_rect,
        ImageSourceRect {
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: 8,
            pixel_height: 2
        }
    );
}

#[test]
fn image_paint_applies_kitty_source_and_first_cell_offsets() {
    let pane_id = PaneId::new();
    let mut image_record = (*build_image_record(6, 4, 0)).clone();
    image_record.display = ImageDisplay {
        requested_width: Some(ImageDimension::Pixels(3)),
        requested_height: Some(ImageDimension::Pixels(2)),
        source_pixel_offset_x: Some(1),
        source_pixel_offset_y: Some(1),
        cell_pixel_offset_x: Some(4),
        cell_pixel_offset_y: Some(5),
        requested_column_count: Some(3),
        requested_row_count: Some(2),
        ..ImageDisplay::default()
    };
    let image_placement_snapshot =
        ImagePlacementSnapshot::from_image_record(1, Arc::new(image_record), (0, 0), 3, 2)
            .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 0, row: 0 },
            size: Size {
                column_count: 3,
                row_count: 2,
            },
        },
        vec![image_placement_snapshot],
        true,
        true,
        false,
    );

    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(image_paints.len(), 1);
    assert_eq!(
        image_paints[0].source_rect,
        ImageSourceRect {
            pixel_x: 1,
            pixel_y: 1,
            pixel_width: 3,
            pixel_height: 2,
        }
    );
    assert_eq!(image_paints[0].cell_pixel_offset_x, Some(4));
    assert_eq!(image_paints[0].cell_pixel_offset_y, Some(5));
}

#[test]
fn image_paint_ignores_kitty_offsets_on_other_protocols() {
    let pane_id = PaneId::new();
    let image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![0, 0, 0, 255],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay {
            cell_pixel_offset_x: Some(4),
            cell_pixel_offset_y: Some(5),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    let image_placement_snapshot =
        ImagePlacementSnapshot::from_image_record(1, image_record, (0, 0), 1, 1)
            .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 0, row: 0 },
            size: Size {
                column_count: 1,
                row_count: 1,
            },
        },
        vec![image_placement_snapshot],
        true,
        true,
        false,
    );

    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(image_paints.len(), 1);
    assert_eq!(image_paints[0].cell_pixel_offset_x, None);
    assert_eq!(image_paints[0].cell_pixel_offset_y, None);
}

#[test]
fn available_and_unavailable_placements_start_with_full_geometry() {
    let available_image_placement =
        ImagePlacementSnapshot::from_image_record(1, build_image_record(2, 3, 0), (4, 5), 6, 7)
            .expect("the available placement is valid");
    let unavailable_image_placement = ImagePlacementSnapshot::build_unavailable(1, 9, (4, 5), 6, 7)
        .expect("the unavailable placement is valid");
    let expected_image_cell_geometry = koshi_core::geometry::ImageCellGeometry {
        full_size: Size {
            column_count: 6,
            row_count: 7,
        },
        cell_offset: Point { column: 0, row: 0 },
    };

    assert_eq!(
        available_image_placement.get_cell_geometry(),
        expected_image_cell_geometry
    );
    assert_eq!(
        unavailable_image_placement.get_cell_geometry(),
        expected_image_cell_geometry
    );
}

#[test]
fn image_placement_constructor_rejects_invalid_basic_state() {
    let valid_image_record = build_image_record(1, 1, 0);
    assert_eq!(
        ImagePlacementSnapshot::from_image_record(0, valid_image_record.clone(), (0, 0), 1, 1),
        None
    );
    assert_eq!(
        ImagePlacementSnapshot::from_image_record(1, valid_image_record.clone(), (0, 0), 0, 1),
        None
    );
    assert_eq!(
        ImagePlacementSnapshot::from_image_record(1, valid_image_record.clone(), (0, 0), 1, 0),
        None
    );
    assert_eq!(
        ImagePlacementSnapshot::from_image_record(1, valid_image_record, (u16::MAX, 0), 2, 2),
        None
    );

    let invalid_image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: Vec::new(),
        })
        .into(),
        animation: None,
        action: ImageAction::Transmit,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    assert_eq!(
        ImagePlacementSnapshot::from_image_record(1, invalid_image_record, (0, 0), 1, 1),
        None
    );

    let invalid_source_image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![0, 0, 0, 255],
        })
        .into(),
        animation: None,
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            source_pixel_offset_x: Some(1),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    assert_eq!(
        ImagePlacementSnapshot::from_image_record(1, invalid_source_image_record, (0, 0), 1, 1,),
        None
    );
}

#[test]
fn image_placeholder_clips_all_four_buffer_edges() {
    let image_paint = ImagePaint::from_image_placement(
        PaneId::new(),
        1,
        1,
        build_image_record(4, 4, 0),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
        ImageSourceRect {
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: 4,
            pixel_height: 4,
        },
        0,
    );
    let mut render_buffer = Buffer::empty(RatatuiRect {
        x: 1,
        y: 1,
        width: 2,
        height: 2,
    });

    draw_image_placeholders(&[image_paint.target_area], &mut render_buffer);

    assert_eq!(render_buffer[(1, 1)].symbol(), "t");
    assert_eq!(render_buffer[(2, 1)].symbol(), "e");
    assert_eq!(render_buffer[(1, 2)].symbol(), "r");
    assert_eq!(render_buffer[(2, 2)].symbol(), "m");
}

#[test]
fn image_paints_skip_hidden_suppressed_and_gridless_panes() {
    let pane_id = PaneId::new();
    let placement =
        ImagePlacementSnapshot::from_image_record(1, build_image_record(2, 2, 0), (0, 0), 1, 1)
            .expect("test image placement is valid");
    let pane_content_rect = Rect {
        origin: Point { column: 0, row: 0 },
        size: Size {
            column_count: 4,
            row_count: 4,
        },
    };
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    };
    assert!(build_image_paints(
        &build_render_snapshot(
            pane_id,
            pane_content_rect,
            vec![placement.clone()],
            false,
            true,
            false,
        ),
        &build_regions(),
        ViewerChrome::default(),
        viewport_area
    )
    .is_empty());
    assert!(build_image_paints(
        &build_render_snapshot(
            pane_id,
            pane_content_rect,
            vec![placement.clone()],
            true,
            false,
            false,
        ),
        &build_regions(),
        ViewerChrome::default(),
        viewport_area
    )
    .is_empty());
    assert!(build_image_paints(
        &build_render_snapshot(
            pane_id,
            pane_content_rect,
            vec![placement],
            true,
            true,
            true,
        ),
        &build_regions(),
        ViewerChrome::default(),
        viewport_area
    )
    .is_empty());
}

#[test]
fn image_paints_sort_overlaps_by_z_index() {
    let pane_id = PaneId::new();
    let placements = vec![
        ImagePlacementSnapshot::from_image_record(10, build_image_record(2, 2, 4), (0, 0), 2, 1)
            .expect("test image placement is valid"),
        ImagePlacementSnapshot::from_image_record(9, build_image_record(2, 2, -1), (0, 0), 2, 1)
            .expect("test image placement is valid"),
    ];
    let image_paints = build_image_paints(
        &build_render_snapshot(
            pane_id,
            Rect {
                origin: Point { column: 0, row: 0 },
                size: Size {
                    column_count: 4,
                    row_count: 4,
                },
            },
            placements,
            true,
            true,
            false,
        ),
        &build_regions(),
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(
        image_paints
            .iter()
            .map(|image_paint| image_paint.placement_id)
            .collect::<Vec<_>>(),
        [9, 10]
    );
    assert_eq!(
        image_paints
            .iter()
            .map(|image_paint| image_paint.z_index)
            .collect::<Vec<_>>(),
        [-1, 4]
    );
}

#[test]
fn unsupported_image_text_fills_the_visible_coverage() {
    let pane_id = PaneId::new();
    let placement =
        ImagePlacementSnapshot::from_image_record(1, build_image_record(26, 1, 0), (0, 0), 26, 1)
            .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 0, row: 0 },
            size: Size {
                column_count: 26,
                row_count: 1,
            },
        },
        vec![placement],
        true,
        true,
        false,
    );
    let image_paints = build_image_paints(
        &snapshot,
        &build_regions(),
        ViewerChrome::default(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );
    let mut screen_buffer = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    });

    let image_target_areas: Vec<RatatuiRect> = image_paints
        .iter()
        .map(|image_paint| image_paint.target_area)
        .collect();
    draw_image_placeholders(&image_target_areas, &mut screen_buffer);

    let rendered_placeholder_text = format_rendered_cells(&screen_buffer, 0, 0..26);
    assert_eq!(rendered_placeholder_text, TERMINAL_IMAGE_UNAVAILABLE);
}

#[test]
fn native_mode_keeps_image_cells_and_placeholder_mode_writes_the_label() {
    let pane_id = PaneId::new();
    let mut snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            size: Size {
                column_count: 4,
                row_count: 1,
            },
        },
        vec![ImagePlacementSnapshot::from_image_record(
            1,
            build_image_record(4, 1, 0),
            (0, 0),
            4,
            1,
        )
        .expect("test image placement is valid")],
        true,
        true,
        false,
    );
    let mut terminal_grid = Grid::build_blank(6, 38, Style::default());
    *terminal_grid
        .get_cell_mut(0, 0)
        .expect("image target cell exists") = Cell::from_character('X', 1, Style::default());
    snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView::from_grid(&terminal_grid, 0));
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    };
    let keymap_hints = KeymapHints::default();
    let render_theme = Theme::default();

    let mut placeholder_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame(
        &snapshot,
        &build_regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Placeholder,
        &[],
        None,
        None,
        viewport_area,
        DrawnRows::Every,
        &mut placeholder_buffer,
    );
    let placeholder_text = format_rendered_cells(&placeholder_buffer, 1, 1..5);
    assert_eq!(placeholder_text, "term");

    let placement_presentation = crate::snapshot::PanePlacementPresentation {
        source_pane_id: pane_id,
        target_pane_ids: Vec::new(),
        source_message: crate::snapshot::PanePlacementMessage {
            full_text: "Moving pane: choose a destination".to_string(),
            compact_text: "Moving pane",
            detail_text: None,
        },
        target_message: None,
    };
    let mut placement_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame(
        &snapshot,
        &build_regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Placeholder,
        &[],
        Some(&placement_presentation),
        None,
        viewport_area,
        DrawnRows::Every,
        &mut placement_buffer,
    );
    assert_eq!(placement_buffer[(1, 1)].symbol(), "X");
    assert_eq!(
        placement_buffer[(1, 1)].fg,
        render_theme.unfocused_border_color
    );

    let mut native_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame(
        &snapshot,
        &build_regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        &[],
        None,
        None,
        viewport_area,
        DrawnRows::Every,
        &mut native_buffer,
    );
    let native_text = format_rendered_cells(&native_buffer, 1, 1..5);
    assert_eq!(native_text, "X   ");
    assert!(native_buffer
        .content()
        .iter()
        .all(|buffer_cell| !buffer_cell.symbol().contains('\u{1b}')));

    let mut unavailable_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame(
        &snapshot,
        &build_regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        &[(pane_id, 1)],
        None,
        None,
        viewport_area,
        DrawnRows::Every,
        &mut unavailable_buffer,
    );
    let unavailable_text = format_rendered_cells(&unavailable_buffer, 1, 1..5);
    assert_eq!(unavailable_text, "term");

    let mut other_unavailable_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame(
        &snapshot,
        &build_regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        &[(pane_id, 2)],
        None,
        None,
        viewport_area,
        DrawnRows::Every,
        &mut other_unavailable_buffer,
    );
    let other_unavailable_text = format_rendered_cells(&other_unavailable_buffer, 1, 1..5);
    assert_eq!(other_unavailable_text, "X   ");
}

/// Put a floating pane with outer rect `outer_rect`, in pane-area cells, on
/// top of `render_snapshot`'s floating panes, showing a blank `6x38` grid and
/// `image_placement_snapshots`. Returns its pane id.
fn push_floating_pane(
    render_snapshot: &mut RenderSnapshot,
    outer_rect: Rect,
    image_placement_snapshots: Vec<ImagePlacementSnapshot>,
) -> PaneId {
    let pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            pane_id, outer_rect, false,
        ));
    let mut pane_snapshot = render_snapshot.pane_snapshots[0].clone();
    pane_snapshot.pane_id = pane_id;
    pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
        &Grid::build_blank(6, 38, Style::default()),
        0,
    ));
    pane_snapshot.image_placement_snapshots = image_placement_snapshots;
    render_snapshot.pane_snapshots.push(pane_snapshot);
    pane_id
}

/// A visible tiled pane whose content rect is `(1, 1)` `38x6`, showing a blank
/// grid and `image_placement_snapshots`, with no floating pane.
fn build_tiled_image_render_snapshot(
    image_placement_snapshots: Vec<ImagePlacementSnapshot>,
) -> RenderSnapshot {
    build_render_snapshot(
        PaneId::new(),
        build_cell_rect(1, 1, 38, 6),
        image_placement_snapshots,
        true,
        true,
        false,
    )
}

/// The image placement `placement_id` of a complete `pixel_width x pixel_height`
/// image covering `column_count x row_count` cells from `anchor_cell`.
fn build_image_placement(
    placement_id: u64,
    pixel_width: u32,
    pixel_height: u32,
    anchor_cell: (u16, u16),
    column_count: u16,
    row_count: u16,
) -> ImagePlacementSnapshot {
    ImagePlacementSnapshot::from_image_record(
        placement_id,
        build_image_record(pixel_width, pixel_height, 0),
        anchor_cell,
        column_count,
        row_count,
    )
    .expect("valid placement")
}

/// The whole `40x8` frame of [`build_render_snapshot`].
fn build_screen_area() -> RatatuiRect {
    RatatuiRect::new(0, 0, 40, 8)
}

/// Each image paint as `(pane, target cells, source pixels)`, in draw order.
fn list_paint_targets(image_paints: &[ImagePaint]) -> Vec<(PaneId, RatatuiRect, ImageSourceRect)> {
    image_paints
        .iter()
        .map(|image_paint| {
            (
                image_paint.pane_id,
                image_paint.target_area,
                image_paint.source_rect,
            )
        })
        .collect()
}

/// The source rect of a whole `pixel_width x pixel_height` image.
fn build_whole_source_rect(pixel_width: u32, pixel_height: u32) -> ImageSourceRect {
    ImageSourceRect {
        pixel_x: 0,
        pixel_y: 0,
        pixel_width,
        pixel_height,
    }
}

#[test]
fn a_floating_panes_image_paint_targets_its_content_cells_from_the_pane_area_origin() {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    let floating_pane_id = push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(8, 0, 20, 7),
        vec![build_image_placement(1, 8, 4, (1, 2), 4, 2)],
    );
    let committed_regions = CommittedRegions::from_solved_regions(
        Size {
            column_count: 40,
            row_count: 8,
        },
        SolvedRegions {
            region_rects: Vec::new(),
            pane_rect: build_cell_rect(0, 1, 40, 7),
        },
        0,
    );

    let image_paints = build_image_paints(
        &render_snapshot,
        &committed_regions,
        ViewerChrome::default(),
        build_screen_area(),
    );

    // Content `(9, 3)` in the pane area starting at `(0, 1)` is screen
    // `(9, 4)`; anchor row `1`, column `2` from there is `(11, 5)`.
    assert_eq!(
        list_paint_targets(&image_paints),
        [(
            floating_pane_id,
            RatatuiRect::new(11, 5, 4, 2),
            build_whole_source_rect(8, 4)
        )]
    );
}

#[test]
fn an_image_a_floating_pane_meets_is_left_out_and_an_image_it_misses_is_painted() {
    let mut render_snapshot = build_tiled_image_render_snapshot(vec![
        build_image_placement(1, 8, 4, (0, 0), 4, 2),
        build_image_placement(2, 8, 4, (0, 20), 4, 2),
    ]);
    let tiled_pane_id = render_snapshot.pane_snapshots[0].pane_id;
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(4, 0, 6, 5),
        Vec::new(),
    );

    let image_paints = build_image_paints(
        &render_snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
    );

    // The floating pane covers columns `4` to `9`: it meets the image at
    // `(1, 1)` `4x2` and misses the one at `(21, 1)`.
    assert_eq!(
        list_paint_targets(&image_paints),
        [(
            tiled_pane_id,
            RatatuiRect::new(21, 1, 4, 2),
            build_whole_source_rect(8, 4)
        )]
    );
}

#[test]
fn a_later_floating_pane_hides_an_earlier_ones_image_and_an_earlier_one_hides_none() {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(0, 0, 20, 8),
        vec![build_image_placement(1, 8, 4, (0, 0), 4, 2)],
    );
    let upper_floating_pane_id = push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(3, 2, 20, 6),
        vec![build_image_placement(1, 8, 2, (0, 0), 4, 1)],
    );

    let image_paints = build_image_paints(
        &render_snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
    );

    // The lower pane's image at `(1, 3)` `4x2` meets the upper pane's outer
    // area `(3, 2)` `20x6`. The upper pane's image at `(4, 5)` lies over the
    // lower pane, which is drawn before it.
    assert_eq!(
        list_paint_targets(&image_paints),
        [(
            upper_floating_pane_id,
            RatatuiRect::new(4, 5, 4, 1),
            build_whole_source_rect(8, 2)
        )]
    );
}

#[test]
fn while_pane_placement_shows_no_floating_image_is_painted_and_no_tiled_image_is_hidden() {
    let mut render_snapshot = build_tiled_image_render_snapshot(vec![
        build_image_placement(1, 8, 4, (0, 0), 4, 2),
        build_image_placement(2, 8, 4, (0, 20), 4, 2),
    ]);
    let tiled_pane_id = render_snapshot.pane_snapshots[0].pane_id;
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(4, 0, 6, 5),
        vec![build_image_placement(1, 4, 2, (0, 0), 2, 1)],
    );

    let image_paints = build_image_paints(
        &render_snapshot,
        &build_regions(),
        ViewerChrome {
            is_pane_placement_visible: true,
            ..ViewerChrome::default()
        },
        build_screen_area(),
    );

    assert_eq!(
        list_paint_targets(&image_paints),
        [
            (
                tiled_pane_id,
                RatatuiRect::new(1, 1, 4, 2),
                build_whole_source_rect(8, 4)
            ),
            (
                tiled_pane_id,
                RatatuiRect::new(21, 1, 4, 2),
                build_whole_source_rect(8, 4)
            ),
        ]
    );
}

#[test]
fn a_floating_panes_image_is_cut_where_its_content_passes_the_pane_area() {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    let floating_pane_id = push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(30, 2, 20, 6),
        vec![build_image_placement(1, 24, 4, (0, 0), 12, 2)],
    );

    let image_paints = build_image_paints(
        &render_snapshot,
        &build_core_regions(40, 8),
        ViewerChrome::default(),
        build_screen_area(),
    );

    // The pane area is `(0, 1)` `40x6`, so it ends after column `39` and row
    // `6`. The content starts at `(31, 6)`: nine of the twelve image columns
    // and one of its two rows show, from eighteen of its twenty-four pixel
    // columns and two of its four pixel rows.
    assert_eq!(
        list_paint_targets(&image_paints),
        [(
            floating_pane_id,
            RatatuiRect::new(31, 6, 9, 1),
            build_whole_source_rect(18, 2)
        )]
    );
}

#[test]
fn the_image_cell_snapshot_writes_no_floating_pane_cell_past_the_pane_area() {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    let floating_pane_id = push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(4, 2, 6, 6),
        Vec::new(),
    );
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_pane_id);
    update_pane_grid(floating_pane_snapshot, |floating_grid| {
        *floating_grid
            .get_cell_mut(0, 0)
            .expect("the cell is in the grid") = Cell::from_character('y', 1, Style::default());
        *floating_grid
            .get_cell_mut(1, 0)
            .expect("the cell is in the grid") = Cell::from_character('z', 1, Style::default());
    });

    let image_cell_snapshot = build_image_cell_snapshot(
        &render_snapshot,
        &build_core_regions(40, 8),
        ViewerChrome::default(),
        build_screen_area(),
        build_screen_area(),
    )
    .expect("the snapshot fits");

    // The content starts at `(5, 6)`. Its second row is screen row `7`, the
    // statusline row below the pane area.
    let read_character = |screen_column: u16, screen_row: u16| {
        image_cell_snapshot
            .find_cell(screen_column, screen_row)
            .expect("the cell is in the frame")
            .character
    };
    assert_eq!(read_character(5, 6), 'y');
    assert_eq!(read_character(5, 7), ' ');
}

#[test]
fn a_snapshot_of_part_of_the_viewport_places_each_floating_pane_as_the_whole_viewport_does() {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    let floating_pane_id = push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(4, 2, 6, 6),
        Vec::new(),
    );
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_pane_id);
    update_pane_grid(floating_pane_snapshot, |floating_grid| {
        *floating_grid
            .get_cell_mut(0, 0)
            .expect("the cell is in the grid") = Cell::from_character('y', 1, Style::default());
    });
    let whole_viewport_snapshot = build_image_cell_snapshot(
        &render_snapshot,
        &build_core_regions(40, 8),
        ViewerChrome::default(),
        build_screen_area(),
        build_screen_area(),
    )
    .expect("the snapshot fits");
    let part_area = RatatuiRect::new(3, 4, 5, 3);

    let part_snapshot = build_image_cell_snapshot(
        &render_snapshot,
        &build_core_regions(40, 8),
        ViewerChrome::default(),
        build_screen_area(),
        part_area,
    )
    .expect("the snapshot fits");

    for screen_row in part_area.y..part_area.bottom() {
        for screen_column in part_area.x..part_area.right() {
            assert_eq!(
                part_snapshot.find_cell(screen_column, screen_row),
                whole_viewport_snapshot.find_cell(screen_column, screen_row),
                "({screen_column}, {screen_row})"
            );
        }
    }
    assert_eq!(
        part_snapshot.find_cell(5, 6).map(|cell| cell.character),
        Some('y')
    );
}

#[test]
fn the_image_cell_snapshot_writes_each_floating_pane_over_the_tiled_cells() {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    update_pane_grid(&mut render_snapshot.pane_snapshots[0], |tiled_grid| {
        for row_index in 0..6 {
            for column_index in 0..38 {
                *tiled_grid
                    .get_cell_mut(row_index, column_index)
                    .expect("the cell is in the grid") =
                    Cell::from_character('x', 1, Style::default());
            }
        }
    });
    let floating_pane_id = push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(4, 0, 6, 5),
        Vec::new(),
    );
    let floating_pane_snapshot = get_pane_snapshot_mut(&mut render_snapshot, floating_pane_id);
    update_pane_grid(floating_pane_snapshot, |floating_grid| {
        *floating_grid
            .get_cell_mut(0, 0)
            .expect("the cell is in the grid") = Cell::from_character('y', 1, Style::default());
    });

    let read_character = |viewer_chrome: ViewerChrome, screen_column: u16, screen_row: u16| {
        build_image_cell_snapshot(
            &render_snapshot,
            &build_regions(),
            viewer_chrome,
            build_screen_area(),
            build_screen_area(),
        )
        .expect("the snapshot fits")
        .find_cell(screen_column, screen_row)
        .expect("the cell is in the frame")
        .character
    };

    // `(4, 1)` is on the floating pane's left border, `(5, 3)` its first
    // content cell, and `(10, 1)` a tiled cell beside it.
    assert_eq!(read_character(ViewerChrome::default(), 4, 1), ' ');
    assert_eq!(read_character(ViewerChrome::default(), 5, 3), 'y');
    assert_eq!(read_character(ViewerChrome::default(), 10, 1), 'x');
    let placement_viewer_chrome = ViewerChrome {
        is_pane_placement_visible: true,
        ..ViewerChrome::default()
    };
    assert_eq!(read_character(placement_viewer_chrome, 4, 1), 'x');
    assert_eq!(read_character(placement_viewer_chrome, 5, 3), 'x');
}

#[test]
fn a_tiled_image_a_floating_pane_meets_shows_no_unavailable_text_in_native_mode() {
    let mut render_snapshot =
        build_tiled_image_render_snapshot(vec![ImagePlacementSnapshot::build_unavailable(
            1,
            1,
            (0, 0),
            4,
            2,
        )
        .expect("valid placement")]);
    let list_tiled_placeholder_rects =
        |render_snapshot: &RenderSnapshot, image_mode: ImageRenderMode| {
            let placed_floating_panes = list_placed_floating_panes(
                render_snapshot,
                &build_regions(),
                ViewerChrome::default(),
                build_screen_area(),
            );
            compute_image_placeholder_rects(
                render_snapshot,
                &build_regions(),
                &placed_floating_panes,
                build_screen_area(),
                image_mode,
                &[],
                None,
            )
        };
    assert_eq!(
        list_tiled_placeholder_rects(&render_snapshot, ImageRenderMode::Native),
        [RatatuiRect::new(1, 1, 4, 2)]
    );

    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(4, 0, 6, 5),
        Vec::new(),
    );

    assert_eq!(
        list_tiled_placeholder_rects(&render_snapshot, ImageRenderMode::Native),
        []
    );
    assert_eq!(
        list_tiled_placeholder_rects(&render_snapshot, ImageRenderMode::Placeholder),
        [RatatuiRect::new(1, 1, 4, 2)]
    );
}

#[test]
fn a_floating_panes_unavailable_image_shows_text_unless_a_later_floating_pane_covers_it_in_native_mode(
) {
    let mut render_snapshot = build_tiled_image_render_snapshot(Vec::new());
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(0, 0, 20, 8),
        vec![
            ImagePlacementSnapshot::build_unavailable(1, 1, (0, 0), 4, 2).expect("valid placement"),
        ],
    );
    push_floating_pane(
        &mut render_snapshot,
        build_cell_rect(3, 2, 20, 6),
        Vec::new(),
    );
    let placed_floating_panes = list_placed_floating_panes(
        &render_snapshot,
        &build_regions(),
        ViewerChrome::default(),
        build_screen_area(),
    );
    let list_lower_placeholder_rects =
        |covering_floating_panes: &[PlacedFloatingPane], image_mode: ImageRenderMode| {
            compute_floating_pane_image_placeholder_rects(
                &render_snapshot,
                &placed_floating_panes[0],
                covering_floating_panes,
                build_screen_area(),
                image_mode,
                &[],
            )
        };

    assert_eq!(
        list_lower_placeholder_rects(&placed_floating_panes[1..], ImageRenderMode::Native),
        []
    );
    assert_eq!(
        list_lower_placeholder_rects(&placed_floating_panes[1..], ImageRenderMode::Placeholder),
        [RatatuiRect::new(1, 3, 4, 2)]
    );
    assert_eq!(
        list_lower_placeholder_rects(&[], ImageRenderMode::Native),
        [RatatuiRect::new(1, 3, 4, 2)]
    );
}

#[test]
fn an_image_the_open_tray_meets_is_left_out_and_paints_again_once_the_tray_closes() {
    use crate::floating_pane_tray::tests::{
        build_open_tray_viewer_chrome, build_tray_test_render_snapshot,
    };

    let viewport_size = Size {
        column_count: 80,
        row_count: 24,
    };
    let mut render_snapshot = build_tray_test_render_snapshot(viewport_size, &["htop"]);
    let tiled_pane_snapshot = &mut render_snapshot.pane_snapshots[0];
    let tiled_pane_id = tiled_pane_snapshot.pane_id;
    tiled_pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
        &Grid::build_blank(20, 78, Style::default()),
        0,
    ));
    // The tiled pane's content starts at screen (1, 2), and the tray takes
    // (17, 1) 57x8: the image at row 0, column 20 lies inside it, and the one
    // at row 12, column 2 outside it.
    tiled_pane_snapshot.image_placement_snapshots = vec![
        build_image_placement(1, 8, 4, (0, 20), 4, 2),
        build_image_placement(2, 8, 4, (12, 2), 4, 2),
    ];
    let committed_regions = CommittedRegions::build_core(viewport_size, 0);
    let screen_area = RatatuiRect::new(0, 0, 80, 24);
    let list_sorted_paint_targets = |viewer_chrome: ViewerChrome| {
        let mut paint_targets = list_paint_targets(&build_image_paints(
            &render_snapshot,
            &committed_regions,
            viewer_chrome,
            screen_area,
        ));
        paint_targets.sort_by_key(|(_, target_area, _)| target_area.x);
        paint_targets
    };

    assert_eq!(
        list_sorted_paint_targets(build_open_tray_viewer_chrome(0)),
        [(
            tiled_pane_id,
            RatatuiRect::new(3, 14, 4, 2),
            build_whole_source_rect(8, 4)
        )]
    );
    assert_eq!(
        list_sorted_paint_targets(ViewerChrome::default()),
        [
            (
                tiled_pane_id,
                RatatuiRect::new(3, 14, 4, 2),
                build_whole_source_rect(8, 4)
            ),
            (
                tiled_pane_id,
                RatatuiRect::new(21, 2, 4, 2),
                build_whole_source_rect(8, 4)
            ),
        ]
    );
}
