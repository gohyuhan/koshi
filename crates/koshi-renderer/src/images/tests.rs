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
use koshi_layout::regions::RegionSolve;
use koshi_pane::pane::state::PaneKind;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::style::Style;

use crate::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, GridView, ImagePlacementSnapshot,
    KeymapHints, PaneSlot, PaneSnapshot, PluginUiSnapshot, RenderSnapshot, ScrollbackMeta,
    SessionSnapshot, TabMeta, TabSnapshot, ViewerChrome,
};
use crate::theme::Theme;

fn record(width: u32, height: u32, z_index: i32) -> Arc<ImageRecord> {
    let pixel_count = usize::try_from(width * height).expect("test image fits usize");
    Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width,
            height,
            rgba: (0..pixel_count * 4)
                .map(|value| u8::try_from(value % 256).expect("test byte fits"))
                .collect(),
        },
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            z_index,
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    })
}

fn snapshot(
    pane_id: PaneId,
    inner: Rect,
    placements: Vec<ImagePlacementSnapshot>,
    grid: bool,
    visible: bool,
    all_suppressed: bool,
) -> RenderSnapshot {
    let tab_id = TabId::new();
    let viewport = Size { cols: 40, rows: 8 };
    let pane = PaneSnapshot {
        id: pane_id,
        title: None,
        cursor: CursorSnapshot {
            row: 0,
            col: 0,
            visible: false,
            blink: false,
            shape: None,
        },
        grid_view: grid.then(|| GridView {
            grid: Arc::new(Grid::blank(6, 38, Style::default())),
            view_offset: 0,
        }),
        image_placements: placements,
        reverse_video: false,
        mouse_tracking: MouseTracking::Off,
        alt_scroll: false,
        on_alt_screen: false,
        view_top_row: 0,
        selection: None,
        has_selection: false,
        scrollback: ScrollbackMeta {
            truncated: false,
            retained_lines: 0,
        },
    };
    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::from("session"),
            active_tab: TabSnapshot {
                id: tab_id,
                name: String::from("tab"),
                layout_solved: vec![PaneSlot {
                    pane_id,
                    rect: Rect {
                        origin: Point { x: 0, y: 0 },
                        size: viewport,
                    },
                    inner_rect: Some(inner),
                    kind: PaneKind::Terminal,
                    visible,
                    suppressed: all_suppressed,
                    dead: false,
                }],
                effective_size: viewport,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed,
                gap: 0,
            },
            tabs_metadata: vec![TabMeta {
                id: tab_id,
                name: String::from("tab"),
                index: 0,
                active: true,
            }],
        },
        panes: vec![pane],
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport,
            active_tab: tab_id,
            focused_pane: Some(pane_id),
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

fn regions() -> CommittedRegions {
    CommittedRegions::new(
        Size { cols: 40, rows: 8 },
        RegionSolve {
            regions: Vec::new(),
            pane_rect: Rect::at_origin(Size { cols: 40, rows: 8 }),
        },
        0,
    )
}

#[test]
fn image_paint_keeps_geometry_and_rgba_record() {
    let pane_id = PaneId::new();
    let placement = ImagePlacementSnapshot::new(7, record(6, 4, 0), (1, 2), 3, 2)
        .expect("test image placement is valid");
    let snapshot = snapshot(
        pane_id,
        Rect {
            origin: Point { x: 1, y: 1 },
            size: Size { cols: 8, rows: 5 },
        },
        vec![placement],
        true,
        true,
        false,
    );
    let paints = image_paints(
        &snapshot,
        &regions(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(paints.len(), 1);
    assert_eq!(paints[0].pane_id, pane_id);
    assert_eq!(paints[0].placement_id, 7);
    assert_eq!(
        paints[0].target,
        RatatuiRect {
            x: 3,
            y: 2,
            width: 3,
            height: 2,
        }
    );
    assert_eq!(
        paints[0].source,
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 6,
            height: 4,
        }
    );
    assert_eq!(paints[0].record.image.width, 6);
    assert_eq!(paints[0].record.image.height, 4);
    assert_eq!(paints[0].record.image.rgba[0], 0);
    assert_eq!(paints[0].record.image.rgba[95], 95);
}

#[test]
fn image_paint_crops_right_and_bottom_edges_to_the_pane() {
    let pane_id = PaneId::new();
    let inner = Rect {
        origin: Point { x: 2, y: 2 },
        size: Size { cols: 4, rows: 4 },
    };
    let placements = vec![
        ImagePlacementSnapshot::new(1, record(8, 8, 0), (0, 3), 4, 4)
            .expect("test image placement is valid"),
        ImagePlacementSnapshot::new(2, record(8, 8, 0), (3, 0), 4, 4)
            .expect("test image placement is valid"),
    ];
    let snapshot = snapshot(pane_id, inner, placements, true, true, false);
    let paints = image_paints(
        &snapshot,
        &regions(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(paints.len(), 2);
    assert_eq!(paints[0].placement_id, 1);
    assert_eq!(
        paints[0].target,
        RatatuiRect {
            x: 5,
            y: 2,
            width: 1,
            height: 4
        }
    );
    assert_eq!(
        paints[0].source,
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 2,
            height: 8
        }
    );
    assert_eq!(paints[1].placement_id, 2);
    assert_eq!(
        paints[1].target,
        RatatuiRect {
            x: 2,
            y: 5,
            width: 4,
            height: 1
        }
    );
    assert_eq!(
        paints[1].source,
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 8,
            height: 2
        }
    );
}

#[test]
fn image_paint_applies_kitty_source_and_first_cell_offsets() {
    let pane_id = PaneId::new();
    let mut record = (*record(6, 4, 0)).clone();
    record.display = ImageDisplay {
        width: Some(ImageDimension::Pixels(3)),
        height: Some(ImageDimension::Pixels(2)),
        source_offset_x: Some(1),
        source_offset_y: Some(1),
        cell_offset_x: Some(4),
        cell_offset_y: Some(5),
        cell_columns: Some(3),
        cell_rows: Some(2),
        ..ImageDisplay::default()
    };
    let placement = ImagePlacementSnapshot::new(1, Arc::new(record), (0, 0), 3, 2)
        .expect("test image placement is valid");
    let snapshot = snapshot(
        pane_id,
        Rect {
            origin: Point { x: 0, y: 0 },
            size: Size { cols: 3, rows: 2 },
        },
        vec![placement],
        true,
        true,
        false,
    );

    let paints = image_paints(
        &snapshot,
        &regions(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(paints.len(), 1);
    assert_eq!(
        paints[0].source,
        ImageSourceRect {
            x: 1,
            y: 1,
            width: 3,
            height: 2,
        }
    );
    assert_eq!(paints[0].cell_offset_x, Some(4));
    assert_eq!(paints[0].cell_offset_y, Some(5));
}

#[test]
fn image_paint_ignores_kitty_offsets_on_other_protocols() {
    let pane_id = PaneId::new();
    let record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        },
        action: ImageAction::Display,
        display: ImageDisplay {
            cell_offset_x: Some(4),
            cell_offset_y: Some(5),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    let placement = ImagePlacementSnapshot::new(1, record, (0, 0), 1, 1)
        .expect("test image placement is valid");
    let snapshot = snapshot(
        pane_id,
        Rect {
            origin: Point { x: 0, y: 0 },
            size: Size { cols: 1, rows: 1 },
        },
        vec![placement],
        true,
        true,
        false,
    );

    let paints = image_paints(
        &snapshot,
        &regions(),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        },
    );

    assert_eq!(paints.len(), 1);
    assert_eq!(paints[0].cell_offset_x, None);
    assert_eq!(paints[0].cell_offset_y, None);
}

#[test]
fn image_placement_constructor_rejects_invalid_basic_state() {
    let valid = record(1, 1, 0);
    assert_eq!(
        ImagePlacementSnapshot::new(0, valid.clone(), (0, 0), 1, 1),
        None
    );
    assert_eq!(
        ImagePlacementSnapshot::new(1, valid.clone(), (0, 0), 0, 1),
        None
    );
    assert_eq!(
        ImagePlacementSnapshot::new(1, valid.clone(), (0, 0), 1, 0),
        None
    );
    assert_eq!(
        ImagePlacementSnapshot::new(1, valid, (u16::MAX, 0), 2, 2),
        None
    );

    let invalid_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: Vec::new(),
        },
        action: ImageAction::Transmit,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    assert_eq!(
        ImagePlacementSnapshot::new(1, invalid_record, (0, 0), 1, 1),
        None
    );

    let invalid_source = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        },
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay {
            source_offset_x: Some(1),
            ..ImageDisplay::default()
        },
        anchor: (0, 0),
    });
    assert_eq!(
        ImagePlacementSnapshot::new(1, invalid_source, (0, 0), 1, 1),
        None
    );
}

#[test]
fn image_placeholder_clips_all_four_buffer_edges() {
    let paint = ImagePaint::new(
        PaneId::new(),
        1,
        record(4, 4, 0),
        RatatuiRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
        ImageSourceRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
        0,
    );
    let mut buffer = Buffer::empty(RatatuiRect {
        x: 1,
        y: 1,
        width: 2,
        height: 2,
    });

    draw_image_placeholders(&[paint.target], &mut buffer);

    assert_eq!(buffer[(1, 1)].symbol(), "t");
    assert_eq!(buffer[(2, 1)].symbol(), "e");
    assert_eq!(buffer[(1, 2)].symbol(), "r");
    assert_eq!(buffer[(2, 2)].symbol(), "m");
}

#[test]
fn image_paints_skip_hidden_suppressed_and_gridless_panes() {
    let pane_id = PaneId::new();
    let placement = ImagePlacementSnapshot::new(1, record(2, 2, 0), (0, 0), 1, 1)
        .expect("test image placement is valid");
    let inner = Rect {
        origin: Point { x: 0, y: 0 },
        size: Size { cols: 4, rows: 4 },
    };
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    };
    assert!(image_paints(
        &snapshot(pane_id, inner, vec![placement.clone()], false, true, false),
        &regions(),
        area
    )
    .is_empty());
    assert!(image_paints(
        &snapshot(pane_id, inner, vec![placement.clone()], true, false, false),
        &regions(),
        area
    )
    .is_empty());
    assert!(image_paints(
        &snapshot(pane_id, inner, vec![placement], true, true, true),
        &regions(),
        area
    )
    .is_empty());
}

#[test]
fn image_paints_sort_overlaps_by_z_index() {
    let pane_id = PaneId::new();
    let placements = vec![
        ImagePlacementSnapshot::new(10, record(2, 2, 4), (0, 0), 2, 1)
            .expect("test image placement is valid"),
        ImagePlacementSnapshot::new(9, record(2, 2, -1), (0, 0), 2, 1)
            .expect("test image placement is valid"),
    ];
    let paints = image_paints(
        &snapshot(
            pane_id,
            Rect {
                origin: Point { x: 0, y: 0 },
                size: Size { cols: 4, rows: 4 },
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
        paints
            .iter()
            .map(|paint| paint.placement_id)
            .collect::<Vec<_>>(),
        [9, 10]
    );
    assert_eq!(
        paints.iter().map(|paint| paint.z_index).collect::<Vec<_>>(),
        [-1, 4]
    );
}

#[test]
fn unsupported_image_text_fills_the_visible_coverage() {
    let pane_id = PaneId::new();
    let placement = ImagePlacementSnapshot::new(1, record(26, 1, 0), (0, 0), 26, 1)
        .expect("test image placement is valid");
    let snapshot = snapshot(
        pane_id,
        Rect {
            origin: Point { x: 0, y: 0 },
            size: Size { cols: 26, rows: 1 },
        },
        vec![placement],
        true,
        true,
        false,
    );
    let paints = image_paints(
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

    let rects: Vec<RatatuiRect> = paints.iter().map(|paint| paint.target).collect();
    draw_image_placeholders(&rects, &mut buffer);

    let text: String = (0..26).map(|col| buffer[(col, 0)].symbol()).collect();
    assert_eq!(text, TERMINAL_IMAGE_UNAVAILABLE);
}

#[test]
fn native_mode_keeps_image_cells_and_placeholder_mode_writes_the_label() {
    let pane_id = PaneId::new();
    let mut snapshot = snapshot(
        pane_id,
        Rect {
            origin: Point { x: 1, y: 1 },
            size: Size { cols: 4, rows: 1 },
        },
        vec![
            ImagePlacementSnapshot::new(1, record(4, 1, 0), (0, 0), 4, 1)
                .expect("test image placement is valid"),
        ],
        true,
        true,
        false,
    );
    let mut grid = Grid::blank(6, 38, Style::default());
    *grid.cell_mut(0, 0).expect("image target cell exists") = Cell::new('X', 1, Style::default());
    snapshot.panes[0].grid_view = Some(GridView {
        grid: Arc::new(grid),
        view_offset: 0,
    });
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 40,
        height: 8,
    };
    let hints = KeymapHints::default();
    let theme = Theme::default();

    let mut placeholder = Buffer::empty(area);
    crate::render::render_frame_with_images(
        &snapshot,
        &regions(),
        &theme,
        &hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Placeholder,
        area,
        &mut placeholder,
    );
    let placeholder_text: String = (1..5)
        .map(|column| placeholder[(column, 1)].symbol())
        .collect();
    assert_eq!(placeholder_text, "term");

    let mut native = Buffer::empty(area);
    crate::render::render_frame_with_images(
        &snapshot,
        &regions(),
        &theme,
        &hints,
        None,
        ViewerChrome::default(),
        ImageRenderMode::Native,
        area,
        &mut native,
    );
    let native_text: String = (1..5).map(|column| native[(column, 1)].symbol()).collect();
    assert_eq!(native_text, "X   ");
    assert!(native
        .content()
        .iter()
        .all(|cell| !cell.symbol().contains('\u{1b}')));
}
