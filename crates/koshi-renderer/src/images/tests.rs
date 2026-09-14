//! Tests for image destination geometry, source cropping, filtering and the
//! unsupported-image text.

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
use koshi_pane::pane::state::PaneKind;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, GridView, ImagePlacementSnapshot,
    KeymapHints, PaneSlot, PaneSnapshot, PluginUiSnapshot, RenderSnapshot, ScrollbackMeta,
    SelectionSpans, SessionSnapshot, TabMeta, TabSnapshot, ViewerChrome,
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
                .map(|channel_value| u8::try_from(channel_value % 256).expect("test byte fits"))
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
    are_all_panes_suppressed: bool,
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
        terminal_grid_view: has_terminal_grid.then(|| GridView {
            grid: Arc::new(Grid::blank(6, 38, Style::default())),
            view_row_offset: 0,
        }),
        image_placement_snapshots,
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
    };
    RenderSnapshot {
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
                        cell_size: viewport_size,
                    },
                    content_rect: Some(content_rect),
                    pane_kind: PaneKind::Terminal,
                    is_visible,
                    is_suppressed: are_all_panes_suppressed,
                    is_dead: false,
                }],
                effective_cell_size: viewport_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMeta {
                tab_id,
                tab_name: String::from("tab"),
                tab_index: 0,
                is_active: true,
            }],
        },
        pane_snapshots: vec![pane_snapshot],
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

fn regions() -> CommittedRegions {
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

#[test]
fn image_cell_snapshot_keeps_exact_combining_characters() {
    let mut snapshot = build_render_snapshot(
        PaneId::new(),
        Rect {
            origin: Point { column: 1, row: 1 },
            cell_size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![],
        true,
        true,
        false,
    );
    let grid = Arc::make_mut(
        &mut snapshot.pane_snapshots[0]
            .terminal_grid_view
            .as_mut()
            .unwrap()
            .grid,
    );
    let mut first_cell = Cell::from_character('e', 1, Style::default());
    first_cell.push_combining('\u{301}');
    *grid.get_cell_mut(0, 0).unwrap() = first_cell;
    let mut second_cell = Cell::from_character('e', 1, Style::default());
    second_cell.push_combining('\u{300}');
    *grid.get_cell_mut(0, 1).unwrap() = second_cell;
    let image_cells =
        build_image_cell_snapshot(&snapshot, &regions(), RatatuiRect::new(0, 0, 40, 8)).unwrap();
    assert_eq!(
        image_cells.find_cell(1, 1),
        Some(&ImageCellState {
            character: 'e',
            cell_width: 1,
            combining_characters: vec!['\u{301}'],
            style: Style::default(),
        })
    );
    assert_eq!(
        image_cells.find_cell(2, 1),
        Some(&ImageCellState {
            character: 'e',
            cell_width: 1,
            combining_characters: vec!['\u{300}'],
            style: Style::default(),
        })
    );
}

#[test]
fn image_cell_snapshot_matches_screen_reverse_and_selection() {
    let pane_id = PaneId::new();
    let mut snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            cell_size: Size {
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
    let grid = Arc::make_mut(
        &mut snapshot.pane_snapshots[0]
            .terminal_grid_view
            .as_mut()
            .unwrap()
            .grid,
    );
    let mut reversed = Style::default();
    reversed.set_reverse(true);
    *grid.get_cell_mut(0, 0).unwrap() = Cell::from_character('a', 1, reversed);
    *grid.get_cell_mut(0, 1).unwrap() = Cell::from_character('b', 1, Style::default());
    let mut reversed_without_selection = Style::default();
    reversed_without_selection.set_reverse(true);
    *grid.get_cell_mut(0, 2).unwrap() = Cell::from_character('c', 1, reversed_without_selection);

    let image_cells =
        build_image_cell_snapshot(&snapshot, &regions(), RatatuiRect::new(0, 0, 40, 8)).unwrap();

    assert!(image_cells
        .find_cell(1, 1)
        .unwrap()
        .style
        .get_attributes()
        .is_reverse());
    assert!(image_cells
        .find_cell(2, 1)
        .unwrap()
        .style
        .get_attributes()
        .is_reverse());
    assert!(!image_cells
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
            cell_size: Size {
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
    let image_paints = build_image_paints(&snapshot, &regions(), RatatuiRect::new(0, 0, 40, 8));
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
    let placement =
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
            cell_size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![placement],
        true,
        true,
        false,
    );
    let image_paints = build_image_paints(&snapshot, &regions(), RatatuiRect::new(0, 0, 40, 8));
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
    let placement =
        ImagePlacementSnapshot::from_image_record(7, build_image_record(6, 4, 0), (1, 2), 3, 2)
            .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            cell_size: Size {
                column_count: 8,
                row_count: 5,
            },
        },
        vec![placement],
        true,
        true,
        false,
    );
    let image_paints = build_image_paints(
        &snapshot,
        &regions(),
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
        cell_size: Size {
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
        &regions(),
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
    let placement =
        ImagePlacementSnapshot::from_image_record(1, Arc::new(image_record), (0, 0), 3, 2)
            .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 0, row: 0 },
            cell_size: Size {
                column_count: 3,
                row_count: 2,
            },
        },
        vec![placement],
        true,
        true,
        false,
    );

    let image_paints = build_image_paints(
        &snapshot,
        &regions(),
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
    let placement = ImagePlacementSnapshot::from_image_record(1, image_record, (0, 0), 1, 1)
        .expect("test image placement is valid");
    let snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 0, row: 0 },
            cell_size: Size {
                column_count: 1,
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
        &regions(),
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
    let available =
        ImagePlacementSnapshot::from_image_record(1, build_image_record(2, 3, 0), (4, 5), 6, 7)
            .expect("the available placement is valid");
    let unavailable = ImagePlacementSnapshot::unavailable(1, 9, (4, 5), 6, 7)
        .expect("the unavailable placement is valid");
    let expected_image_cell_geometry = koshi_core::geometry::ImageCellGeometry {
        full_size: Size {
            column_count: 6,
            row_count: 7,
        },
        cell_offset: Point { column: 0, row: 0 },
    };

    assert_eq!(available.get_cell_geometry(), expected_image_cell_geometry);
    assert_eq!(
        unavailable.get_cell_geometry(),
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
        cell_size: Size {
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
        &regions(),
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
        &regions(),
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
        &regions(),
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
                cell_size: Size {
                    column_count: 4,
                    row_count: 4,
                },
            },
            placements,
            true,
            true,
            false,
        ),
        &regions(),
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
            cell_size: Size {
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
        &regions(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );
    let mut buffer = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    });

    let image_target_areas: Vec<RatatuiRect> = image_paints
        .iter()
        .map(|image_paint| image_paint.target_area)
        .collect();
    draw_image_placeholders(&image_target_areas, &mut buffer);

    let rendered_placeholder_text: String = (0..26)
        .map(|column_index| buffer[(column_index, 0)].symbol())
        .collect();
    assert_eq!(rendered_placeholder_text, TERMINAL_IMAGE_UNAVAILABLE);
}

#[test]
fn native_mode_keeps_image_cells_and_placeholder_mode_writes_the_label() {
    let pane_id = PaneId::new();
    let mut snapshot = build_render_snapshot(
        pane_id,
        Rect {
            origin: Point { column: 1, row: 1 },
            cell_size: Size {
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
    let mut terminal_grid = Grid::blank(6, 38, Style::default());
    *terminal_grid
        .get_cell_mut(0, 0)
        .expect("image target cell exists") = Cell::from_character('X', 1, Style::default());
    snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(terminal_grid),
        view_row_offset: 0,
    });
    let viewport_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    };
    let keymap_hints = KeymapHints::default();
    let render_theme = Theme::default();

    let mut placeholder_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame_with_images(
        &snapshot,
        &regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Placeholder,
        viewport_area,
        &mut placeholder_buffer,
    );
    let placeholder_text: String = (1..5)
        .map(|column_index| placeholder_buffer[(column_index, 1)].symbol())
        .collect();
    assert_eq!(placeholder_text, "term");

    let mut native_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame_with_images(
        &snapshot,
        &regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        viewport_area,
        &mut native_buffer,
    );
    let native_text: String = (1..5)
        .map(|column_index| native_buffer[(column_index, 1)].symbol())
        .collect();
    assert_eq!(native_text, "X   ");
    assert!(native_buffer
        .content()
        .iter()
        .all(|buffer_cell| !buffer_cell.symbol().contains('\u{1b}')));

    let mut unavailable_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame_with_image_availability(
        &snapshot,
        &regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        Some(&[]),
        viewport_area,
        &mut unavailable_buffer,
    );
    let unavailable_text: String = (1..5)
        .map(|column_index| unavailable_buffer[(column_index, 1)].symbol())
        .collect();
    assert_eq!(unavailable_text, "term");

    let mut available_buffer = Buffer::empty(viewport_area);
    crate::render::render_frame_with_image_availability(
        &snapshot,
        &regions(),
        &render_theme,
        &keymap_hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        Some(&[(pane_id, 1)]),
        viewport_area,
        &mut available_buffer,
    );
    let available_text: String = (1..5)
        .map(|column_index| available_buffer[(column_index, 1)].symbol())
        .collect();
    assert_eq!(available_text, "X   ");
}
