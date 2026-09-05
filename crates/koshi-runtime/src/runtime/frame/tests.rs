//! Tests for [`wire_frame`]: a cell travels with its character, its combining
//! marks, its width and every style field; equal neighbouring cells fold into
//! one run; a row travels the grid's full width and carries how its line ends;
//! rows travel top to bottom; a pane with no terminal content sends no window;
//! every color, underline style and cursor shape maps to its wire form; and the
//! session, tab, slot, client and per-pane scalars come across unchanged.

use std::sync::Arc;

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{FrameImageChunk, FrameRun, MAX_FRAME_IMAGE_CHUNK_BYTES};
use koshi_ipc::transport::MAX_FRAME_LEN;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;
use koshi_renderer::snapshot::{
    ClientSnapshot, CursorSnapshot, GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot,
    PluginUiSnapshot, RenderSnapshot, ScrollbackMeta, SelectionSpans, SessionSnapshot, TabMeta,
    TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};

use super::*;

/// The style the one written cell carries: every field set away from its
/// default.
fn style() -> Style {
    let mut style = Style::default();
    style.set_fg(Color::Indexed(4));
    style.set_bg(Color::Rgb(10, 20, 30));
    style.set_underline_color(Some(Color::Indexed(9)));
    style.set_bold(true);
    style.set_italic(true);
    style.set_reverse(true);
    style.set_faint(true);
    style.set_blink(true);
    style.set_conceal(true);
    style.set_strike(true);
    style.set_overline(true);
    style.set_underline(UnderlineStyle::Curly);
    style
}

/// A 1×3 grid: a styled `e` carrying a combining acute accent (U+0301), then
/// two blank cells in the default style.
fn grid() -> Grid {
    let mut grid = Grid::blank(1, 3, Style::default());
    let cell = grid.cell_mut(0, 0).expect("the grid has a cell at (0, 0)");
    *cell = Cell::new('e', 1, style());
    cell.push_combining('\u{301}');
    grid
}

/// The cell [`grid`] writes at (0, 0), as it travels.
fn wire_written_cell() -> FrameCell {
    FrameCell {
        ch: 'e',
        combining: vec!['\u{301}'],
        width: 1,
        style: FrameStyle {
            fg: FrameColor::Indexed(4),
            bg: FrameColor::Rgb(10, 20, 30),
            underline_color: Some(FrameColor::Indexed(9)),
            attrs: FrameAttrs {
                bold: true,
                italic: true,
                reverse: true,
                faint: true,
                blink: true,
                conceal: true,
                strike: true,
                overline: true,
                underline: FrameUnderline::Curly,
            },
        },
    }
}

/// The two blank cells [`grid`] leaves at (0, 1) and (0, 2), as they travel.
fn wire_blank_cell() -> FrameCell {
    FrameCell {
        ch: ' ',
        combining: Vec::new(),
        width: 1,
        style: FrameStyle {
            fg: FrameColor::Default,
            bg: FrameColor::Default,
            underline_color: None,
            attrs: FrameAttrs {
                bold: false,
                italic: false,
                reverse: false,
                faint: false,
                blink: false,
                conceal: false,
                strike: false,
                overline: false,
                underline: FrameUnderline::None,
            },
        },
    }
}

/// One 2×1 Kitty placement with pixels that make byte and field mistakes easy
/// to see in the wire assertions.
fn image_placement() -> ImagePlacementSnapshot {
    ImagePlacementSnapshot::new(
        41,
        Arc::new(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: DecodedImage {
                width: 2,
                height: 1,
                rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
            },
            action: ImageAction::TransmitAndDisplay,
            display: ImageDisplay {
                width: Some(ImageDimension::Cells(3)),
                height: Some(ImageDimension::Pixels(1)),
                preserve_aspect_ratio: false,
                sixel_background: Some(SixelBackground::Preserve),
                image_id: Some(7),
                image_number: Some(8),
                placement_id: Some(9),
                usage_hints: 0x12,
                unicode_placeholder: true,
                cell_columns: Some(2),
                cell_rows: Some(1),
                z_index: -2,
                source_offset_x: Some(1),
                source_offset_y: Some(0),
                cell_offset_x: Some(6),
                cell_offset_y: Some(7),
                move_cursor: false,
            },
            anchor: (0, 2),
        }),
        (0, 1),
        2,
        1,
    )
    .expect("test image placement is valid")
}

/// One pane holding [`grid`], scrolled 7 lines back, reporting any-motion mouse
/// tracking and a truncated scrollback of 500 retained lines.
fn pane(id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        id,
        title: Some(String::from("~/work")),
        cursor: CursorSnapshot {
            row: 0,
            col: 2,
            visible: true,
            blink: true,
            shape: Some(CursorShape::Bar),
        },
        grid_view: Some(GridView {
            grid: Arc::new(grid()),
            view_offset: 7,
        }),
        image_placements: vec![image_placement()],
        reverse_video: true,
        mouse_tracking: MouseTracking::AnyMotion,
        alt_scroll: true,
        on_alt_screen: false,
        view_top_row: 493,
        selection: Some(SelectionSpans {
            rows: vec![(0, 1, 2)],
        }),
        has_selection: true,
        scrollback: ScrollbackMeta {
            truncated: true,
            retained_lines: 500,
        },
    }
}

/// A pane with no terminal content: no window, a hidden cursor, nothing
/// highlighted.
fn empty_pane(id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        id,
        title: None,
        cursor: CursorSnapshot {
            row: 0,
            col: 0,
            visible: false,
            blink: false,
            shape: None,
        },
        grid_view: None,
        image_placements: Vec::new(),
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
    }
}

/// A frame with one tab, two slots, and the two panes above.
fn snapshot(content: PaneId, empty: PaneId) -> RenderSnapshot {
    let tab = TabId::new();
    let other_tab = TabId::new();
    let client = ClientId::new();
    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::from("session"),
            active_tab: TabSnapshot {
                id: tab,
                name: String::from("tab"),
                layout_solved: vec![
                    PaneSlot {
                        pane_id: content,
                        rect: Rect {
                            origin: Point { x: 0, y: 0 },
                            size: Size { cols: 5, rows: 3 },
                        },
                        inner_rect: Some(Rect {
                            origin: Point { x: 1, y: 1 },
                            size: Size { cols: 3, rows: 1 },
                        }),
                        kind: PaneKind::Terminal,
                        visible: true,
                        suppressed: false,
                        dead: false,
                    },
                    PaneSlot {
                        pane_id: empty,
                        rect: Rect {
                            origin: Point { x: 5, y: 0 },
                            size: Size { cols: 5, rows: 3 },
                        },
                        inner_rect: None,
                        kind: PaneKind::Terminal,
                        visible: false,
                        suppressed: true,
                        dead: true,
                    },
                ],
                effective_size: Size { cols: 10, rows: 3 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Fullscreen { focused: content },
                all_suppressed: false,
                gap: 0,
            },
            tabs_metadata: vec![
                TabMeta {
                    id: tab,
                    name: String::from("tab"),
                    index: 0,
                    active: true,
                },
                TabMeta {
                    id: other_tab,
                    name: String::from("other"),
                    index: 1,
                    active: false,
                },
            ],
        },
        panes: vec![pane(content), empty_pane(empty)],
        client: ClientSnapshot {
            id: client,
            viewport: Size { cols: 20, rows: 6 },
            active_tab: tab,
            focused_pane: Some(content),
            lock_mode: LockMode::Locked,
            mouse_select: true,
        },
        plugin_ui: PluginUiSnapshot::default(),
    }
}

#[test]
fn a_cell_travels_with_its_character_marks_width_and_every_style_field() {
    let content = PaneId::new();
    let frame = wire_frame(&snapshot(content, PaneId::new()));

    let window = frame.panes[0]
        .window
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(window.rows[0].cells()[0], wire_written_cell());
}

#[test]
fn an_oversized_image_frame_splits_into_bounded_wire_events() {
    let content = PaneId::new();
    let mut source = snapshot(content, PaneId::new());
    let record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: DecodedImage {
            width: 4_096,
            height: 1_024,
            rgba: vec![0x7f; 16 * 1024 * 1024],
        },
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    source.panes[0].image_placements[0] =
        ImagePlacementSnapshot::new(41, Arc::clone(&record), (0, 0), 2, 1)
            .expect("test image placement is valid");
    let frame = wire_frame(&source);

    let painted = SessionEvent::Painted {
        frame: Box::new(frame.clone()),
    };
    assert!(
        serde_json::to_vec(&painted)
            .expect("the placement frame encodes")
            .len()
            <= MAX_FRAME_LEN as usize
    );

    let transfer = wire_image_transfer(1, &record);
    assert_eq!(transfer.id, 1);
    assert_eq!(transfer.byte_len, 16 * 1024 * 1024);
    assert!(
        serde_json::to_vec(&SessionEvent::ImageContentStart { image: transfer })
            .expect("the image start encodes")
            .len()
            <= MAX_FRAME_LEN as usize
    );

    let chunks: Vec<(u64, bool, usize)> = wire_image_chunk_sources(&record)
        .map(|(offset, last, bytes)| (offset, last, bytes.len()))
        .collect();
    assert_eq!(chunks.len(), 16);
    assert_eq!(chunks[0], (0, false, MAX_FRAME_IMAGE_CHUNK_BYTES));
    assert_eq!(
        chunks[15],
        (15 * 1024 * 1024, true, MAX_FRAME_IMAGE_CHUNK_BYTES)
    );
    for (offset, last, length) in chunks {
        let event = SessionEvent::ImageContentChunk {
            chunk: FrameImageChunk {
                transfer_id: 1,
                offset,
                last,
                bytes: vec![0; length],
            },
        };
        assert!(
            serde_json::to_vec(&event).expect("the chunk encodes").len() <= MAX_FRAME_LEN as usize
        );
    }
}

#[test]
fn equal_neighbouring_cells_fold_into_one_run() {
    let content = PaneId::new();
    let frame = wire_frame(&snapshot(content, PaneId::new()));

    // "e" then two default blanks: one run of 1, then one run of 2.
    let window = frame.panes[0]
        .window
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(window.cols, 3);
    assert_eq!(window.rows.len(), 1);
    assert_eq!(
        window.rows[0].runs,
        vec![
            FrameRun {
                count: 1,
                cell: wire_written_cell(),
            },
            FrameRun {
                count: 2,
                cell: wire_blank_cell(),
            },
        ]
    );
}

#[test]
fn a_wire_row_is_as_wide_as_the_grid() {
    let grid = grid();
    let (_, cols) = grid.dimensions();

    let cells = wire_row(&grid, 0, cols).cells();

    assert_eq!(cells.len(), cols as usize);
    assert_eq!(
        cells,
        vec![wire_written_cell(), wire_blank_cell(), wire_blank_cell()]
    );
}

#[test]
fn a_wire_row_carries_how_its_line_ends() {
    let mut grid = Grid::blank(3, 1, Style::default());
    grid.set_row_end(0, RowEnd::Hard);
    grid.set_row_end(1, RowEnd::Soft);
    grid.set_row_end(2, RowEnd::SoftWide);

    assert_eq!(wire_row(&grid, 0, 1).end, FrameRowEnd::Hard);
    assert_eq!(wire_row(&grid, 1, 1).end, FrameRowEnd::Soft);
    assert_eq!(wire_row(&grid, 2, 1).end, FrameRowEnd::SoftWide);
}

#[test]
fn a_cell_travels_with_its_display_width() {
    let mut grid = Grid::blank(1, 2, Style::default());
    let cell = grid.cell_mut(0, 0).expect("the grid has a cell at (0, 0)");
    *cell = Cell::new('世', 2, Style::default());

    let cells = wire_row(&grid, 0, 2).cells();

    assert_eq!(cells[0].ch, '世');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[1], wire_blank_cell());
}

#[test]
fn a_window_carries_every_grid_row_top_to_bottom() {
    let mut grid = Grid::blank(2, 1, Style::default());
    *grid.cell_mut(0, 0).expect("the grid has a cell at (0, 0)") =
        Cell::new('a', 1, Style::default());
    *grid.cell_mut(1, 0).expect("the grid has a cell at (1, 0)") =
        Cell::new('b', 1, Style::default());
    let view = GridView {
        grid: Arc::new(grid),
        view_offset: 4,
    };

    let window = wire_window(&view);

    assert_eq!(window.cols, 1);
    assert_eq!(window.view_offset, 4);
    assert_eq!(window.rows.len(), 2);
    assert_eq!(window.rows[0].cells()[0].ch, 'a');
    assert_eq!(window.rows[1].cells()[0].ch, 'b');
}

#[test]
fn a_grid_with_no_rows_travels_as_a_window_with_no_rows() {
    let view = GridView {
        grid: Arc::new(Grid::blank(0, 0, Style::default())),
        view_offset: 0,
    };

    let window = wire_window(&view);

    assert_eq!(window.cols, 0);
    assert_eq!(window.rows, Vec::new());
    assert_eq!(window.view_offset, 0);
}

#[test]
fn every_color_travels_as_its_wire_form() {
    assert_eq!(wire_color(Color::Default), FrameColor::Default);
    assert_eq!(wire_color(Color::Indexed(0)), FrameColor::Indexed(0));
    assert_eq!(wire_color(Color::Indexed(255)), FrameColor::Indexed(255));
    assert_eq!(
        wire_color(Color::Rgb(0, 128, 255)),
        FrameColor::Rgb(0, 128, 255)
    );
}

#[test]
fn every_underline_style_travels_as_its_wire_form() {
    assert_eq!(wire_underline(UnderlineStyle::None), FrameUnderline::None);
    assert_eq!(
        wire_underline(UnderlineStyle::Single),
        FrameUnderline::Single
    );
    assert_eq!(
        wire_underline(UnderlineStyle::Double),
        FrameUnderline::Double
    );
    assert_eq!(wire_underline(UnderlineStyle::Curly), FrameUnderline::Curly);
    assert_eq!(
        wire_underline(UnderlineStyle::Dotted),
        FrameUnderline::Dotted
    );
    assert_eq!(
        wire_underline(UnderlineStyle::Dashed),
        FrameUnderline::Dashed
    );
}

#[test]
fn every_cursor_shape_travels_as_its_wire_form() {
    assert_eq!(
        wire_cursor_shape(CursorShape::Block),
        FrameCursorShape::Block
    );
    assert_eq!(
        wire_cursor_shape(CursorShape::Underline),
        FrameCursorShape::Underline
    );
    assert_eq!(wire_cursor_shape(CursorShape::Bar), FrameCursorShape::Bar);
}

#[test]
fn a_pane_with_no_grid_travels_with_no_window() {
    let empty = PaneId::new();
    let frame = wire_frame(&snapshot(PaneId::new(), empty));

    assert_eq!(frame.panes[1].id, empty);
    assert_eq!(frame.panes[1].window, None);
}

#[test]
fn a_pane_with_no_grid_travels_with_its_remaining_fields_at_rest() {
    let empty = PaneId::new();
    let frame = wire_frame(&snapshot(PaneId::new(), empty));

    let pane = &frame.panes[1];
    assert_eq!(pane.title, None);
    assert_eq!(
        pane.cursor,
        FrameCursor {
            row: 0,
            col: 0,
            visible: false,
            blink: false,
            shape: None,
        }
    );
    assert_eq!(pane.mouse_tracking, MouseTracking::Off);
    assert!(!pane.reverse_video);
    assert!(!pane.alt_scroll);
    assert!(!pane.on_alt_screen);
    assert_eq!(pane.view_top_row, 0);
    assert_eq!(pane.selection, None);
    assert!(!pane.has_selection);
    assert_eq!(
        pane.scrollback,
        FrameScrollback {
            truncated: false,
            retained_lines: 0,
        }
    );
}

#[test]
fn the_view_offset_mouse_mode_and_scrollback_scalars_come_through_unchanged() {
    let content = PaneId::new();
    let frame = wire_frame(&snapshot(content, PaneId::new()));

    let pane = &frame.panes[0];
    assert_eq!(
        pane.window
            .as_ref()
            .expect("the pane carries a grid")
            .view_offset,
        7
    );
    assert_eq!(pane.mouse_tracking, MouseTracking::AnyMotion);
    assert!(pane.scrollback.truncated);
    assert_eq!(pane.scrollback.retained_lines, 500);
    assert_eq!(pane.view_top_row, 493);
    assert_eq!(pane.title, Some(String::from("~/work")));
    assert!(pane.reverse_video);
    assert!(pane.alt_scroll);
    assert!(!pane.on_alt_screen);
    assert!(pane.has_selection);
    assert_eq!(
        pane.selection,
        Some(FrameSelection {
            rows: vec![(0, 1, 2)],
        })
    );
    assert_eq!(
        pane.cursor,
        FrameCursor {
            row: 0,
            col: 2,
            visible: true,
            blink: true,
            shape: Some(FrameCursorShape::Bar),
        }
    );
}

#[test]
fn an_image_placement_and_its_record_travel_in_separate_values() {
    let content = PaneId::new();
    let source = snapshot(content, PaneId::new());
    let record = source.panes[0].image_placements[0]
        .record()
        .expect("the source placement has image content");
    let frame = wire_frame(&source);
    let placement = &frame.panes[0].image_placements[0];
    let transfer = wire_image_transfer(placement.content_id, record);

    assert_eq!(placement.id, 41);
    assert_eq!(placement.content_id, 1);
    assert_eq!(placement.anchor, (0, 1));
    assert_eq!(placement.columns, 2);
    assert_eq!(placement.rows, 1);
    assert_eq!(transfer.id, 1);
    assert_eq!(transfer.record.protocol, FrameGraphicsProtocol::Kitty);
    assert_eq!(transfer.record.action, FrameImageAction::TransmitAndDisplay);
    assert_eq!(transfer.record.width, 2);
    assert_eq!(transfer.record.height, 1);
    assert_eq!(transfer.byte_len, 8);
    assert_eq!(record.image.rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    assert_eq!(transfer.record.display.image_id, Some(7));
    assert_eq!(transfer.record.display.image_number, Some(8));
    assert_eq!(transfer.record.display.placement_id, Some(9));
    assert_eq!(
        transfer.record.display.width,
        Some(FrameImageDimension::Cells(3))
    );
    assert_eq!(
        transfer.record.display.height,
        Some(FrameImageDimension::Pixels(1))
    );
    assert!(!transfer.record.display.preserve_aspect_ratio);
    assert_eq!(
        transfer.record.display.sixel_background,
        Some(FrameSixelBackground::Preserve)
    );
    assert_eq!(transfer.record.display.usage_hints, 0x12);
    assert!(transfer.record.display.unicode_placeholder);
    assert_eq!(transfer.record.display.cell_columns, Some(2));
    assert_eq!(transfer.record.display.cell_rows, Some(1));
    assert_eq!(transfer.record.display.z_index, -2);
    assert_eq!(transfer.record.display.source_offset_x, Some(1));
    assert_eq!(transfer.record.display.source_offset_y, Some(0));
    assert_eq!(transfer.record.display.cell_offset_x, Some(6));
    assert_eq!(transfer.record.display.cell_offset_y, Some(7));
    assert!(!transfer.record.display.move_cursor);
    assert_eq!(transfer.record.anchor, (0, 2));
}

#[test]
fn the_session_tab_slot_and_client_fields_copy_straight_across() {
    let content = PaneId::new();
    let empty = PaneId::new();
    let snapshot = snapshot(content, empty);

    let frame = wire_frame(&snapshot);

    assert_eq!(frame.session.id, snapshot.session.id);
    assert_eq!(frame.session.name, String::from("session"));
    assert_eq!(frame.session.active_tab.id, snapshot.session.active_tab.id);
    assert_eq!(frame.session.active_tab.name, String::from("tab"));
    assert_eq!(
        frame.session.active_tab.effective_size,
        Size { cols: 10, rows: 3 }
    );
    assert_eq!(frame.session.active_tab.stack_headers, Vec::new());
    assert_eq!(
        frame.session.active_tab.layout_mode,
        LayoutMode::Fullscreen { focused: content }
    );
    assert!(!frame.session.active_tab.all_suppressed);
    assert_eq!(
        frame.session.active_tab.slots,
        vec![
            FrameSlot {
                pane_id: content,
                rect: Rect {
                    origin: Point { x: 0, y: 0 },
                    size: Size { cols: 5, rows: 3 },
                },
                inner_rect: Some(Rect {
                    origin: Point { x: 1, y: 1 },
                    size: Size { cols: 3, rows: 1 },
                }),
                kind: PaneKind::Terminal,
                visible: true,
                suppressed: false,
                dead: false,
            },
            FrameSlot {
                pane_id: empty,
                rect: Rect {
                    origin: Point { x: 5, y: 0 },
                    size: Size { cols: 5, rows: 3 },
                },
                inner_rect: None,
                kind: PaneKind::Terminal,
                visible: false,
                suppressed: true,
                dead: true,
            },
        ]
    );
    assert_eq!(
        frame.session.tabs,
        vec![
            FrameTabMeta {
                id: snapshot.session.active_tab.id,
                name: String::from("tab"),
                index: 0,
                active: true,
            },
            FrameTabMeta {
                id: snapshot.session.tabs_metadata[1].id,
                name: String::from("other"),
                index: 1,
                active: false,
            },
        ]
    );
    assert_eq!(
        frame.client,
        FrameClient {
            id: snapshot.client.id,
            viewport: Size { cols: 20, rows: 6 },
            active_tab: snapshot.session.active_tab.id,
            focused_pane: Some(content),
            lock_mode: LockMode::Locked,
            mouse_select: true,
        }
    );
}

#[test]
fn the_tab_gap_travels_with_the_frame() {
    let content = PaneId::new();
    let empty = PaneId::new();
    let mut snapshot = snapshot(content, empty);
    snapshot.session.active_tab.gap = 2;

    let frame = wire_frame(&snapshot);

    assert_eq!(frame.session.active_tab.gap, 2);
}

#[test]
fn the_tab_stack_headers_and_all_suppressed_flag_travel_with_the_frame() {
    let content = PaneId::new();
    let empty = PaneId::new();
    let header = StackHeader {
        pane: empty,
        rect: Rect {
            origin: Point { x: 5, y: 0 },
            size: Size { cols: 5, rows: 1 },
        },
        position: 1,
        total: 2,
    };
    let mut snapshot = snapshot(content, empty);
    snapshot.session.active_tab.stack_headers = vec![header];
    snapshot.session.active_tab.all_suppressed = true;

    let frame = wire_frame(&snapshot);

    assert_eq!(frame.session.active_tab.stack_headers, vec![header]);
    assert!(frame.session.active_tab.all_suppressed);
}
