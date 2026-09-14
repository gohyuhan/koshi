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
use koshi_ipc::frame::{FrameImageChunk, FrameRun, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT};
use koshi_ipc::transport::MAX_FRAME_BYTE_COUNT;
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
fn build_test_style() -> Style {
    let mut test_style = Style::default();
    test_style.set_foreground_color(Color::Indexed(4));
    test_style.set_background_color(Color::Rgb(10, 20, 30));
    test_style.set_underline_color(Some(Color::Indexed(9)));
    test_style.set_bold(true);
    test_style.set_italic(true);
    test_style.set_reverse(true);
    test_style.set_faint(true);
    test_style.set_blink(true);
    test_style.set_conceal(true);
    test_style.set_strike(true);
    test_style.set_overline(true);
    test_style.set_underline(UnderlineStyle::Curly);
    test_style
}

/// A 1×3 grid: a styled `e` carrying a combining acute accent (U+0301), then
/// two blank cells in the default style.
fn build_test_grid() -> Grid {
    let mut test_grid = Grid::blank(1, 3, Style::default());
    let written_cell = test_grid
        .get_cell_mut(0, 0)
        .expect("the grid has a cell at (0, 0)");
    *written_cell = Cell::from_character('e', 1, build_test_style());
    written_cell.push_combining('\u{301}');
    test_grid
}

/// The cell [`grid`] writes at (0, 0), as it travels.
fn build_written_frame_cell() -> FrameCell {
    FrameCell {
        character: 'e',
        combining_characters: vec!['\u{301}'],
        cell_width: 1,
        style: FrameStyle {
            foreground_color: FrameColor::Indexed(4),
            background_color: FrameColor::Rgb(10, 20, 30),
            underline_color: Some(FrameColor::Indexed(9)),
            text_attributes: FrameAttrs {
                is_bold: true,
                is_italic: true,
                is_reverse: true,
                is_faint: true,
                is_blinking: true,
                is_concealed: true,
                is_struck_through: true,
                is_overlined: true,
                underline_style: FrameUnderline::Curly,
            },
        },
    }
}

/// The two blank cells [`grid`] leaves at (0, 1) and (0, 2), as they travel.
fn build_blank_frame_cell() -> FrameCell {
    FrameCell {
        character: ' ',
        combining_characters: Vec::new(),
        cell_width: 1,
        style: FrameStyle {
            foreground_color: FrameColor::Default,
            background_color: FrameColor::Default,
            underline_color: None,
            text_attributes: FrameAttrs {
                is_bold: false,
                is_italic: false,
                is_reverse: false,
                is_faint: false,
                is_blinking: false,
                is_concealed: false,
                is_struck_through: false,
                is_overlined: false,
                underline_style: FrameUnderline::None,
            },
        },
    }
}

/// One 2×1 Kitty placement with pixels that make byte and field mistakes easy
/// to see in the wire assertions.
fn build_image_placement_snapshot() -> ImagePlacementSnapshot {
    ImagePlacementSnapshot::from_image_record(
        41,
        Arc::new(ImageRecord {
            protocol: GraphicsProtocol::Kitty,
            image: (DecodedImage {
                pixel_width: 2,
                pixel_height: 1,
                rgba_bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
            })
            .into(),
            animation: None,
            action: ImageAction::TransmitAndDisplay,
            display: ImageDisplay {
                response_suppression_level: 0,
                requested_width: Some(ImageDimension::Cells(3)),
                requested_height: Some(ImageDimension::Pixels(1)),
                is_aspect_ratio_preserved: false,
                sixel_background: Some(SixelBackground::Preserve),
                image_id: Some(7),
                image_number: Some(8),
                placement_id: Some(9),
                usage_hints: 0x12,
                is_unicode_placeholder: true,
                requested_column_count: Some(2),
                requested_row_count: Some(1),
                z_index: -2,
                source_pixel_offset_x: Some(1),
                source_pixel_offset_y: Some(0),
                cell_pixel_offset_x: Some(6),
                cell_pixel_offset_y: Some(7),
                relative_image_id: None,
                relative_placement_id: None,
                relative_column_offset: 0,
                relative_row_offset: 0,
                should_move_cursor: false,
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
fn build_pane_snapshot(pane_id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        pane_id,
        pane_title: Some(String::from("~/work")),
        cursor_snapshot: CursorSnapshot {
            row_index: 0,
            column_index: 2,
            is_visible: true,
            is_blinking: true,
            shape: Some(CursorShape::Bar),
        },
        terminal_grid_view: Some(GridView {
            grid: Arc::new(build_test_grid()),
            view_row_offset: 7,
        }),
        image_placement_snapshots: vec![build_image_placement_snapshot()],
        is_reverse_video: true,
        mouse_tracking: MouseTracking::AnyMotion,
        is_alternate_scroll_enabled: true,
        is_on_alternate_screen: false,
        view_top_row_index: 493,
        selection_spans: Some(SelectionSpans {
            row_spans: vec![(0, 1, 2)],
        }),
        has_selection: true,
        scrollback_meta: ScrollbackMeta {
            is_truncated: true,
            retained_line_count: 500,
        },
    }
}

/// A pane with no terminal content: no window, a hidden cursor, nothing
/// highlighted.
fn build_empty_pane_snapshot(pane_id: PaneId) -> PaneSnapshot {
    PaneSnapshot {
        pane_id,
        pane_title: None,
        cursor_snapshot: CursorSnapshot {
            row_index: 0,
            column_index: 0,
            is_visible: false,
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
        scrollback_meta: ScrollbackMeta {
            is_truncated: false,
            retained_line_count: 0,
        },
    }
}

/// A frame with one tab, two slots, and the two panes above.
fn build_render_snapshot(content_pane_id: PaneId, empty_pane_id: PaneId) -> RenderSnapshot {
    let tab_id = TabId::new();
    let other_tab_id = TabId::new();
    let client_id = ClientId::new();
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("tab"),
                pane_slots: vec![
                    PaneSlot {
                        pane_id: content_pane_id,
                        outer_rect: Rect {
                            origin: Point { column: 0, row: 0 },
                            cell_size: Size {
                                column_count: 5,
                                row_count: 3,
                            },
                        },
                        content_rect: Some(Rect {
                            origin: Point { column: 1, row: 1 },
                            cell_size: Size {
                                column_count: 3,
                                row_count: 1,
                            },
                        }),
                        pane_kind: PaneKind::Terminal,
                        is_visible: true,
                        is_suppressed: false,
                        is_dead: false,
                    },
                    PaneSlot {
                        pane_id: empty_pane_id,
                        outer_rect: Rect {
                            origin: Point { column: 5, row: 0 },
                            cell_size: Size {
                                column_count: 5,
                                row_count: 3,
                            },
                        },
                        content_rect: None,
                        pane_kind: PaneKind::Terminal,
                        is_visible: false,
                        is_suppressed: true,
                        is_dead: true,
                    },
                ],
                effective_cell_size: Size {
                    column_count: 10,
                    row_count: 3,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Fullscreen {
                    focused_pane_id: content_pane_id,
                },
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![
                TabMeta {
                    tab_id,
                    tab_name: String::from("tab"),
                    tab_index: 0,
                    is_active: true,
                },
                TabMeta {
                    tab_id: other_tab_id,
                    tab_name: String::from("other"),
                    tab_index: 1,
                    is_active: false,
                },
            ],
        },
        pane_snapshots: vec![
            build_pane_snapshot(content_pane_id),
            build_empty_pane_snapshot(empty_pane_id),
        ],
        client_snapshot: ClientSnapshot {
            client_id,
            viewport_size: Size {
                column_count: 20,
                row_count: 6,
            },
            active_tab_id: tab_id,
            focused_pane_id: Some(content_pane_id),
            lock_mode: LockMode::Locked,
            is_mouse_selection_enabled: true,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

#[test]
fn a_cell_travels_with_its_character_marks_width_and_every_style_field() {
    let source_pane_id = PaneId::new();
    let painted_frame = wire_frame(&build_render_snapshot(source_pane_id, PaneId::new()));

    let terminal_window = painted_frame.pane_snapshots[0]
        .terminal_window
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(
        terminal_window.row_snapshots[0].expand_cells()[0],
        build_written_frame_cell()
    );
}

#[test]
fn an_oversized_image_frame_splits_into_bounded_wire_events() {
    let source_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(source_pane_id, PaneId::new());
    let image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: (DecodedImage {
            pixel_width: 4_096,
            pixel_height: 1_024,
            rgba_bytes: vec![0x7f; 16 * 1024 * 1024],
        })
        .into(),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    render_snapshot.pane_snapshots[0].image_placement_snapshots[0] =
        ImagePlacementSnapshot::from_image_record(41, Arc::clone(&image_record), (0, 0), 2, 1)
            .expect("test image placement is valid");
    let painted_frame = wire_frame(&render_snapshot);

    let painted_event = SessionEvent::Painted {
        frame: Box::new(painted_frame.clone()),
    };
    assert!(
        serde_json::to_vec(&painted_event)
            .expect("the placement frame encodes")
            .len()
            <= MAX_FRAME_BYTE_COUNT as usize
    );

    let image_transfer = wire_image_transfer(1, &image_record);
    assert_eq!(image_transfer.image_content_id, 1);
    assert_eq!(image_transfer.image_byte_count, 16 * 1024 * 1024);
    assert!(
        serde_json::to_vec(&SessionEvent::ImageContentStart { image_transfer })
            .expect("the image start encodes")
            .len()
            <= MAX_FRAME_BYTE_COUNT as usize
    );

    let image_chunks: Vec<(u64, bool, usize)> = wire_image_chunk_sources(&image_record)
        .map(|(byte_offset, is_last, chunk_bytes)| (byte_offset, is_last, chunk_bytes.len()))
        .collect();
    assert_eq!(image_chunks.len(), 16);
    assert_eq!(
        image_chunks[0],
        (0, false, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
    );
    assert_eq!(
        image_chunks[15],
        (15 * 1024 * 1024, true, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
    );
    for (byte_offset, is_last, chunk_byte_count) in image_chunks {
        let image_chunk_event = SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: 1,
                byte_offset,
                is_last,
                chunk_bytes: vec![0; chunk_byte_count],
            },
        };
        assert!(
            serde_json::to_vec(&image_chunk_event)
                .expect("the chunk encodes")
                .len()
                <= MAX_FRAME_BYTE_COUNT as usize
        );
    }
}

#[test]
fn equal_neighbouring_cells_fold_into_one_run() {
    let source_pane_id = PaneId::new();
    let painted_frame = wire_frame(&build_render_snapshot(source_pane_id, PaneId::new()));

    // "e" then two default blanks: one run of 1, then one run of 2.
    let terminal_window = painted_frame.pane_snapshots[0]
        .terminal_window
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(terminal_window.column_count, 3);
    assert_eq!(terminal_window.row_snapshots.len(), 1);
    assert_eq!(
        terminal_window.row_snapshots[0].cell_runs,
        vec![
            FrameRun {
                repeat_count: 1,
                cell: build_written_frame_cell(),
            },
            FrameRun {
                repeat_count: 2,
                cell: build_blank_frame_cell(),
            },
        ]
    );
}

#[test]
fn a_wire_row_is_as_wide_as_the_grid() {
    let test_grid = build_test_grid();
    let (_, column_count) = test_grid.get_grid_dimensions();

    let expanded_cells = wire_row(&test_grid, 0, column_count).expand_cells();

    assert_eq!(expanded_cells.len(), column_count as usize);
    assert_eq!(
        expanded_cells,
        vec![
            build_written_frame_cell(),
            build_blank_frame_cell(),
            build_blank_frame_cell()
        ]
    );
}

#[test]
fn a_wire_row_carries_how_its_line_ends() {
    let mut grid = Grid::blank(3, 1, Style::default());
    grid.set_row_end(0, RowEnd::Hard);
    grid.set_row_end(1, RowEnd::Soft);
    grid.set_row_end(2, RowEnd::SoftWide);

    assert_eq!(wire_row(&grid, 0, 1).row_end, FrameRowEnd::Hard);
    assert_eq!(wire_row(&grid, 1, 1).row_end, FrameRowEnd::Soft);
    assert_eq!(wire_row(&grid, 2, 1).row_end, FrameRowEnd::SoftWide);
}

#[test]
fn a_cell_travels_with_its_display_width() {
    let mut grid = Grid::blank(1, 2, Style::default());
    let wide_cell = grid
        .get_cell_mut(0, 0)
        .expect("the grid has a cell at (0, 0)");
    *wide_cell = Cell::from_character('世', 2, Style::default());

    let expanded_cells = wire_row(&grid, 0, 2).expand_cells();

    assert_eq!(expanded_cells[0].character, '世');
    assert_eq!(expanded_cells[0].cell_width, 2);
    assert_eq!(expanded_cells[1], build_blank_frame_cell());
}

#[test]
fn a_window_carries_every_grid_row_top_to_bottom() {
    let mut grid = Grid::blank(2, 1, Style::default());
    *grid
        .get_cell_mut(0, 0)
        .expect("the grid has a cell at (0, 0)") = Cell::from_character('a', 1, Style::default());
    *grid
        .get_cell_mut(1, 0)
        .expect("the grid has a cell at (1, 0)") = Cell::from_character('b', 1, Style::default());
    let grid_view = GridView {
        grid: Arc::new(grid),
        view_row_offset: 4,
    };

    let terminal_window = wire_window(&grid_view);

    assert_eq!(terminal_window.column_count, 1);
    assert_eq!(terminal_window.view_row_offset, 4);
    assert_eq!(terminal_window.row_snapshots.len(), 2);
    assert_eq!(
        terminal_window.row_snapshots[0].expand_cells()[0].character,
        'a'
    );
    assert_eq!(
        terminal_window.row_snapshots[1].expand_cells()[0].character,
        'b'
    );
}

#[test]
fn a_grid_with_no_rows_travels_as_a_window_with_no_rows() {
    let view = GridView {
        grid: Arc::new(Grid::blank(0, 0, Style::default())),
        view_row_offset: 0,
    };

    let terminal_window = wire_window(&view);

    assert_eq!(terminal_window.column_count, 0);
    assert_eq!(terminal_window.row_snapshots, Vec::new());
    assert_eq!(terminal_window.view_row_offset, 0);
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
    let empty_pane_id = PaneId::new();
    let painted_frame = wire_frame(&build_render_snapshot(PaneId::new(), empty_pane_id));

    assert_eq!(painted_frame.pane_snapshots[1].pane_id, empty_pane_id);
    assert_eq!(painted_frame.pane_snapshots[1].terminal_window, None);
}

#[test]
fn a_pane_with_no_grid_travels_with_its_remaining_fields_at_rest() {
    let empty_pane_id = PaneId::new();
    let painted_frame = wire_frame(&build_render_snapshot(PaneId::new(), empty_pane_id));

    let pane_snapshot = &painted_frame.pane_snapshots[1];
    assert_eq!(pane_snapshot.pane_title, None);
    assert_eq!(
        pane_snapshot.cursor_snapshot,
        FrameCursor {
            row_index: 0,
            column_index: 0,
            is_visible: false,
            is_blinking: false,
            shape: None,
        }
    );
    assert_eq!(pane_snapshot.mouse_tracking, MouseTracking::Off);
    assert!(!pane_snapshot.is_reverse_video);
    assert!(!pane_snapshot.is_alt_scroll_enabled);
    assert!(!pane_snapshot.is_on_alt_screen);
    assert_eq!(pane_snapshot.view_top_row_index, 0);
    assert_eq!(pane_snapshot.selection_spans, None);
    assert!(!pane_snapshot.has_selection);
    assert_eq!(
        pane_snapshot.scrollback_meta,
        FrameScrollback {
            is_truncated: false,
            retained_line_count: 0,
        }
    );
}

#[test]
fn the_view_offset_mouse_mode_and_scrollback_scalars_come_through_unchanged() {
    let source_pane_id = PaneId::new();
    let painted_frame = wire_frame(&build_render_snapshot(source_pane_id, PaneId::new()));

    let pane_snapshot = &painted_frame.pane_snapshots[0];
    assert_eq!(
        pane_snapshot
            .terminal_window
            .as_ref()
            .expect("the pane carries a grid")
            .view_row_offset,
        7
    );
    assert_eq!(pane_snapshot.mouse_tracking, MouseTracking::AnyMotion);
    assert!(pane_snapshot.scrollback_meta.is_truncated);
    assert_eq!(pane_snapshot.scrollback_meta.retained_line_count, 500);
    assert_eq!(pane_snapshot.view_top_row_index, 493);
    assert_eq!(pane_snapshot.pane_title, Some(String::from("~/work")));
    assert!(pane_snapshot.is_reverse_video);
    assert!(pane_snapshot.is_alt_scroll_enabled);
    assert!(!pane_snapshot.is_on_alt_screen);
    assert!(pane_snapshot.has_selection);
    assert_eq!(
        pane_snapshot.selection_spans,
        Some(FrameSelection {
            row_spans: vec![(0, 1, 2)],
        })
    );
    assert_eq!(
        pane_snapshot.cursor_snapshot,
        FrameCursor {
            row_index: 0,
            column_index: 2,
            is_visible: true,
            is_blinking: true,
            shape: Some(FrameCursorShape::Bar),
        }
    );
}

#[test]
fn an_image_placement_and_its_record_travel_in_separate_values() {
    let source_pane_id = PaneId::new();
    let source_render_snapshot = build_render_snapshot(source_pane_id, PaneId::new());
    let image_record = source_render_snapshot.pane_snapshots[0].image_placement_snapshots[0]
        .clone_image_record()
        .expect("the source placement has image content");
    let painted_frame = wire_frame(&source_render_snapshot);
    let image_placement = &painted_frame.pane_snapshots[0].image_placement_snapshots[0];
    let image_transfer = wire_image_transfer(image_placement.image_content_id, &image_record);

    assert_eq!(image_placement.placement_id, 41);
    assert_eq!(image_placement.image_content_id, 1);
    assert_eq!(image_placement.anchor_cell, (0, 1));
    assert_eq!(image_placement.column_count, 2);
    assert_eq!(image_placement.row_count, 1);
    assert_eq!(image_transfer.image_content_id, 1);
    assert_eq!(
        image_transfer.image_record.protocol,
        FrameGraphicsProtocol::Kitty
    );
    assert_eq!(
        image_transfer.image_record.image_action,
        FrameImageAction::TransmitAndDisplay
    );
    assert_eq!(image_transfer.image_record.pixel_width, 2);
    assert_eq!(image_transfer.image_record.pixel_height, 1);
    assert_eq!(image_transfer.image_byte_count, 8);
    assert_eq!(
        image_record.image.rgba_bytes,
        vec![255, 0, 0, 255, 0, 255, 0, 255]
    );
    assert_eq!(image_transfer.image_record.display.image_id, Some(7));
    assert_eq!(image_transfer.image_record.display.image_number, Some(8));
    assert_eq!(image_transfer.image_record.display.placement_id, Some(9));
    assert_eq!(
        image_transfer.image_record.display.requested_width,
        Some(FrameImageDimension::Cells(3))
    );
    assert_eq!(
        image_transfer.image_record.display.requested_height,
        Some(FrameImageDimension::Pixels(1))
    );
    assert!(
        !image_transfer
            .image_record
            .display
            .is_aspect_ratio_preserved
    );
    assert_eq!(
        image_transfer.image_record.display.sixel_background,
        Some(FrameSixelBackground::Preserve)
    );
    assert_eq!(image_transfer.image_record.display.usage_hints, 0x12);
    assert!(image_transfer.image_record.display.is_unicode_placeholder);
    assert_eq!(
        image_transfer.image_record.display.requested_column_count,
        Some(2)
    );
    assert_eq!(
        image_transfer.image_record.display.requested_row_count,
        Some(1)
    );
    assert_eq!(image_transfer.image_record.display.z_index, -2);
    assert_eq!(
        image_transfer.image_record.display.source_pixel_offset_x,
        Some(1)
    );
    assert_eq!(
        image_transfer.image_record.display.source_pixel_offset_y,
        Some(0)
    );
    assert_eq!(
        image_transfer.image_record.display.cell_pixel_offset_x,
        Some(6)
    );
    assert_eq!(
        image_transfer.image_record.display.cell_pixel_offset_y,
        Some(7)
    );
    assert!(!image_transfer.image_record.display.should_move_cursor);
    assert_eq!(image_transfer.image_record.anchor_cell, (0, 2));
}

#[test]
fn the_session_tab_slot_and_client_fields_copy_straight_across() {
    let focused_pane_id = PaneId::new();
    let empty_pane_id = PaneId::new();
    let render_snapshot = build_render_snapshot(focused_pane_id, empty_pane_id);

    let painted_frame = wire_frame(&render_snapshot);

    assert_eq!(
        painted_frame.session_snapshot.session_id,
        render_snapshot.session_snapshot.session_id
    );
    assert_eq!(
        painted_frame.session_snapshot.session_name,
        String::from("session")
    );
    assert_eq!(
        painted_frame.session_snapshot.active_tab_snapshot.tab_id,
        render_snapshot.session_snapshot.active_tab_snapshot.tab_id
    );
    assert_eq!(
        painted_frame.session_snapshot.active_tab_snapshot.tab_name,
        String::from("tab")
    );
    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 10,
            row_count: 3
        }
    );
    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .stack_headers,
        Vec::new()
    );
    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .layout_mode,
        LayoutMode::Fullscreen { focused_pane_id }
    );
    assert!(
        !painted_frame
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    );
    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .pane_slots,
        vec![
            FrameSlot {
                pane_id: focused_pane_id,
                outer_rect: Rect {
                    origin: Point { column: 0, row: 0 },
                    cell_size: Size {
                        column_count: 5,
                        row_count: 3
                    },
                },
                content_rect: Some(Rect {
                    origin: Point { column: 1, row: 1 },
                    cell_size: Size {
                        column_count: 3,
                        row_count: 1
                    },
                }),
                pane_kind: PaneKind::Terminal,
                is_visible: true,
                is_suppressed: false,
                is_dead: false,
            },
            FrameSlot {
                pane_id: empty_pane_id,
                outer_rect: Rect {
                    origin: Point { column: 5, row: 0 },
                    cell_size: Size {
                        column_count: 5,
                        row_count: 3
                    },
                },
                content_rect: None,
                pane_kind: PaneKind::Terminal,
                is_visible: false,
                is_suppressed: true,
                is_dead: true,
            },
        ]
    );
    assert_eq!(
        painted_frame.session_snapshot.tab_snapshots,
        vec![
            FrameTabMeta {
                tab_id: render_snapshot.session_snapshot.active_tab_snapshot.tab_id,
                tab_name: String::from("tab"),
                tab_index: 0,
                is_active: true,
            },
            FrameTabMeta {
                tab_id: render_snapshot.session_snapshot.tabs_metadata[1].tab_id,
                tab_name: String::from("other"),
                tab_index: 1,
                is_active: false,
            },
        ]
    );
    assert_eq!(
        painted_frame.client_snapshot,
        FrameClient {
            client_id: render_snapshot.client_snapshot.client_id,
            viewport_size: Size {
                column_count: 20,
                row_count: 6
            },
            active_tab_id: render_snapshot.session_snapshot.active_tab_snapshot.tab_id,
            focused_pane_id: Some(focused_pane_id),
            lock_mode: LockMode::Locked,
            is_mouse_selection_enabled: true,
        }
    );
}

#[test]
fn the_tab_gap_travels_with_the_frame() {
    let focused_pane_id = PaneId::new();
    let empty_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(focused_pane_id, empty_pane_id);
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count = 2;

    let painted_frame = wire_frame(&render_snapshot);

    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .gap_cell_count,
        2
    );
}

#[test]
fn the_tab_stack_headers_and_all_suppressed_flag_travel_with_the_frame() {
    let focused_pane_id = PaneId::new();
    let empty_pane_id = PaneId::new();
    let stack_header = StackHeader {
        pane_id: empty_pane_id,
        header_rect: Rect {
            origin: Point { column: 5, row: 0 },
            cell_size: Size {
                column_count: 5,
                row_count: 1,
            },
        },
        member_index: 1,
        member_count: 2,
    };
    let mut render_snapshot = build_render_snapshot(focused_pane_id, empty_pane_id);
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers = vec![stack_header];
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .are_all_panes_suppressed = true;

    let painted_frame = wire_frame(&render_snapshot);

    assert_eq!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .stack_headers,
        vec![stack_header]
    );
    assert!(
        painted_frame
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    );
}
