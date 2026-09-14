//! Tests for [`build_render_snapshot`]: a frame that travels and is read back is the
//! frame that was sent — every style field, a wide glyph and its continuation
//! half, a combining mark, both cursor states, the scroll offset, the mouse
//! mode, a pane with no grid at all, and the highlight rows.

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::frame::{
    FrameGraphicsProtocol, FrameImageAction, FrameImageChunk, FrameImageDisplay,
    FrameImagePlacement, FrameImageRecordHeader, FrameImageTransfer,
    MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT, MAX_FRAME_IMAGE_TRANSFER_COUNT,
};
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::PaneKind;
use koshi_runtime::runtime::frame::wire_frame;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};

use super::*;

fn build_image_placement_snapshot() -> ImagePlacementSnapshot {
    ImagePlacementSnapshot::with_content_id(
        41,
        1,
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
                requested_width: Some(ImageDimension::Cells(3)),
                requested_height: Some(ImageDimension::Pixels(1)),
                is_aspect_ratio_preserved: false,
                sixel_background: Some(SixelBackground::Preserve),
                image_id: Some(7),
                image_number: Some(8),
                placement_id: Some(9),
                usage_hints: 0x12,
                is_unicode_placeholder: true,
                z_index: -2,
                relative_image_id: None,
                relative_placement_id: None,
                relative_column_offset: 0,
                relative_row_offset: 0,
                requested_column_count: Some(2),
                requested_row_count: Some(1),
                source_pixel_offset_x: Some(1),
                source_pixel_offset_y: Some(0),
                cell_pixel_offset_x: Some(6),
                cell_pixel_offset_y: Some(7),
                should_move_cursor: false,
                response_suppression_level: 0,
            },
            anchor: (0, 2),
        }),
        (0, 1),
        2,
        1,
    )
    .expect("test image placement is valid")
}

/// The transfer metadata for [`image_placement`].
fn build_image_transfer(image_content_id: u64) -> FrameImageTransfer {
    FrameImageTransfer {
        image_content_id,
        image_record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            pixel_width: 2,
            pixel_height: 1,
            image_action: FrameImageAction::TransmitAndDisplay,
            display: FrameImageDisplay {
                response_suppression_level: 0,
                requested_width: Some(FrameImageDimension::Cells(3)),
                requested_height: Some(FrameImageDimension::Pixels(1)),
                is_aspect_ratio_preserved: false,
                sixel_background: Some(FrameSixelBackground::Preserve),
                image_id: Some(7),
                image_number: Some(8),
                placement_id: Some(9),
                usage_hints: 0x12,
                is_unicode_placeholder: true,
                z_index: -2,
                relative_image_id: None,
                relative_placement_id: None,
                relative_column_offset: 0,
                relative_row_offset: 0,
                requested_column_count: Some(2),
                requested_row_count: Some(1),
                source_pixel_offset_x: Some(1),
                source_pixel_offset_y: Some(0),
                cell_pixel_offset_x: Some(6),
                cell_pixel_offset_y: Some(7),
                should_move_cursor: false,
            },
            anchor_cell: (0, 2),
        },
        image_byte_count: 8,
    }
}

/// All RGBA bytes for [`image_transfer`] in one final chunk.
fn build_image_chunk(image_transfer_id: u64) -> FrameImageChunk {
    FrameImageChunk {
        image_transfer_id,
        byte_offset: 0,
        is_last: true,
        chunk_bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
    }
}

#[test]
fn shared_pixels_keep_each_placements_own_display_metadata() {
    let cached_image_record = build_image_placement_snapshot()
        .clone_image_record()
        .expect("pixels");
    let mut image_record_header = build_image_transfer(1).image_record;
    image_record_header.image_action = FrameImageAction::Display;
    image_record_header.display.requested_column_count = Some(4);
    image_record_header.display.requested_row_count = Some(6);
    image_record_header.display.z_index = 3;
    image_record_header.display.placement_id = Some(10);
    image_record_header.anchor_cell = (5, 6);
    let image_cell_geometry = koshi_core::geometry::ImageCellGeometry {
        full_size: Size {
            column_count: 4,
            row_count: 6,
        },
        cell_offset: Point { column: 0, row: 2 },
    };
    let frame_image_placement = FrameImagePlacement {
        placement_id: 42,
        image_content_id: 1,
        is_available: true,
        anchor_cell: (0, 6),
        column_count: 4,
        row_count: 4,
        cell_geometry: Some(image_cell_geometry),
        image_record: Some(image_record_header.clone()),
    };
    let image_placement_snapshot =
        image_placement_with_record(&frame_image_placement, Some(&cached_image_record))
            .expect("placement");
    let display_image_record = image_placement_snapshot
        .clone_image_record()
        .expect("pixels");
    assert!(Arc::ptr_eq(
        &display_image_record.image,
        &cached_image_record.image
    ));
    assert_eq!(
        image_placement_snapshot.get_cell_geometry(),
        image_cell_geometry
    );
    assert_eq!(image_placement_snapshot.get_anchor_cell(), (0, 6));
    assert_eq!(image_placement_snapshot.get_cell_dimensions(), (4, 4));
    assert_eq!(display_image_record.action, ImageAction::Display);
    assert_eq!(
        display_image_record.display,
        build_image_display(&image_record_header.display)
    );
    assert_eq!(display_image_record.anchor, (5, 6));
    assert_eq!(cached_image_record.display.placement_id, Some(9));
    assert_eq!(cached_image_record.anchor, (0, 2));
}

#[test]
fn placement_metadata_cannot_change_cached_pixel_dimensions() {
    let cached_image_record = build_image_placement_snapshot()
        .clone_image_record()
        .expect("pixels");
    let mut image_record_header = build_image_transfer(1).image_record;
    image_record_header.pixel_width = 3;
    let frame_image_placement = FrameImagePlacement {
        placement_id: 42,
        image_content_id: 1,
        is_available: true,
        anchor_cell: (0, 0),
        column_count: 2,
        row_count: 1,
        cell_geometry: None,
        image_record: Some(image_record_header),
    };
    assert_eq!(
        image_placement_with_record(&frame_image_placement, Some(&cached_image_record)),
        None
    );
}

/// Every style field set away from its default, so a field lost on the way
/// there or back shows up.
fn build_test_style() -> Style {
    let mut terminal_style = Style::default();
    terminal_style.set_foreground_color(Color::Indexed(4));
    terminal_style.set_background_color(Color::Rgb(10, 20, 30));
    terminal_style.set_underline_color(Some(Color::Indexed(9)));
    terminal_style.set_bold(true);
    terminal_style.set_italic(true);
    terminal_style.set_reverse(true);
    terminal_style.set_faint(true);
    terminal_style.set_blink(true);
    terminal_style.set_conceal(true);
    terminal_style.set_strike(true);
    terminal_style.set_overline(true);
    terminal_style.set_underline(UnderlineStyle::Curly);
    terminal_style
}

/// A 2×4 grid whose first row holds, left to right: a styled `e` carrying a
/// combining acute accent (U+0301), the wide glyph `漢` at width 2, the blank
/// continuation half it occupies at width 0, and one default blank. The second
/// row is all default blanks.
fn build_test_grid() -> Grid {
    let mut terminal_grid = Grid::blank(2, 4, Style::default());
    let accented_cell = terminal_grid
        .get_cell_mut(0, 0)
        .expect("the grid has a cell at (0, 0)");
    *accented_cell = Cell::from_character('e', 1, build_test_style());
    accented_cell.push_combining('\u{301}');
    *terminal_grid
        .get_cell_mut(0, 1)
        .expect("the grid has a cell at (0, 1)") =
        Cell::from_character('漢', 2, build_test_style());
    *terminal_grid
        .get_cell_mut(0, 2)
        .expect("the grid has a cell at (0, 2)") = Cell::from_character(' ', 0, build_test_style());
    terminal_grid
}

/// The pane holding [`grid`]: scrolled 7 lines back, reporting any-motion mouse
/// tracking, showing a shaped blinking cursor, and highlighting the first row's
/// columns 1 to 2.
fn build_content_pane_snapshot(pane_id: PaneId) -> PaneSnapshot {
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
        image_placement_snapshots: Vec::new(),
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

/// The content pane with one image placement included.
fn build_content_pane_snapshot_with_image(pane_id: PaneId) -> PaneSnapshot {
    let mut pane_snapshot = build_content_pane_snapshot(pane_id);
    pane_snapshot
        .image_placement_snapshots
        .push(build_image_placement_snapshot());
    pane_snapshot
}

/// A pane with no terminal content: no grid, a hidden and unshaped cursor,
/// nothing highlighted.
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

/// A frame with one tab, two slots, and the two panes handed in.
fn build_render_snapshot(pane_snapshots: Vec<PaneSnapshot>) -> RenderSnapshot {
    let content_pane_id = pane_snapshots[0].pane_id;
    let empty_pane_id = pane_snapshots[1].pane_id;
    let active_tab_id = TabId::new();
    let secondary_tab_id = TabId::new();
    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id: active_tab_id,
                tab_name: String::from("tab"),
                pane_slots: vec![
                    PaneSlot {
                        pane_id: content_pane_id,
                        outer_rect: Rect {
                            origin: Point { column: 0, row: 0 },
                            cell_size: Size {
                                column_count: 6,
                                row_count: 4,
                            },
                        },
                        content_rect: Some(Rect {
                            origin: Point { column: 1, row: 1 },
                            cell_size: Size {
                                column_count: 4,
                                row_count: 2,
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
                            origin: Point { column: 6, row: 0 },
                            cell_size: Size {
                                column_count: 6,
                                row_count: 4,
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
                    column_count: 12,
                    row_count: 4,
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
                    tab_id: active_tab_id,
                    tab_name: String::from("tab"),
                    tab_index: 0,
                    is_active: true,
                },
                TabMeta {
                    tab_id: secondary_tab_id,
                    tab_name: String::from("other"),
                    tab_index: 1,
                    is_active: false,
                },
            ],
        },
        pane_snapshots,
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: Size {
                column_count: 20,
                row_count: 6,
            },
            active_tab_id,
            focused_pane_id: Some(content_pane_id),
            lock_mode: LockMode::Locked,
            is_mouse_selection_enabled: true,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

#[test]
fn a_frame_that_travels_and_is_read_back_is_the_frame_that_was_sent() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    assert_eq!(
        super::build_render_snapshot(&wire_frame(&expected_render_snapshot)),
        expected_render_snapshot
    );
}

#[test]
fn a_frame_waits_for_the_complete_image_record() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let frame_bytes = wire_frame(&expected_render_snapshot);
    let mut cache = ImageCache::new();

    let initial_snapshot = cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the placement frame reads");
    assert_eq!(initial_snapshot, None);

    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    let rebuilt_snapshot = cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the last missing record produces a redraw");

    assert_eq!(rebuilt_snapshot, expected_render_snapshot);
    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .expect("the complete image has its record")
            .image
            .rgba_bytes,
        vec![255, 0, 0, 255, 0, 255, 0, 255]
    );
    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .expect("the complete image has its record")
            .display
            .source_pixel_offset_y,
        Some(0)
    );
}

#[test]
fn a_complete_cached_image_is_reused_by_the_next_frame() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let frame_bytes = wire_frame(&expected_render_snapshot);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes.clone()))
        .expect("the first placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the image produces a redraw");

    let reused_snapshot = cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the repeated placement frame reads");

    assert_eq!(reused_snapshot, Some(expected_render_snapshot));
    assert_eq!(cache.image_record_by_content_id.len(), 1);
    assert!(cache.missing_image_content_ids.is_empty());
    assert_eq!(
        cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image| pending_image.received_byte_count),
        None
    );
}

#[test]
fn a_cached_record_cannot_hide_an_invalid_new_placement() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes.clone()))
        .expect("the valid frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the image produces a redraw");
    frame_bytes.pane_snapshots[0].image_placement_snapshots[0].anchor_cell = (u16::MAX, 0);
    frame_bytes.pane_snapshots[0].image_placement_snapshots[0].row_count = 2;

    let image_assembly_error = cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect_err("the cached record cannot make bad geometry valid");

    assert_eq!(image_assembly_error, ImageAssemblyError::InvalidPlacement);
    assert_eq!(cache.image_record_by_content_id.len(), 1);
    assert_eq!(cache.retained_image_byte_count, 8);
}

#[test]
fn a_frame_redraws_only_after_every_missing_image_record_arrives() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    frame_bytes.pane_snapshots[0]
        .image_placement_snapshots
        .push(FrameImagePlacement {
            cell_geometry: None,
            image_record: None,
            placement_id: 42,
            image_content_id: 2,
            is_available: true,
            anchor_cell: (0, 0),
            column_count: 1,
            row_count: 1,
        });
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the placement frame reads");

    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    let after_first_image = cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the first complete record reads");
    cache
        .start_image_transfer(build_image_transfer(2))
        .expect("the second transfer starts");
    let after_second_image = cache
        .accept_image_chunk(build_image_chunk(2))
        .expect("the second complete record reads")
        .expect("the last record produces one redraw");

    assert_eq!(after_first_image, None);
    assert_eq!(
        after_second_image.pane_snapshots[0]
            .image_placement_snapshots
            .len(),
        2
    );
    assert_eq!(
        after_second_image.pane_snapshots[0].image_placement_snapshots[0].get_image_content_id(),
        1
    );
    assert_eq!(
        after_second_image.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .map(|image_record| image_record.image.rgba_bytes.as_slice()),
        Some([255, 0, 0, 255, 0, 255, 0, 255].as_slice())
    );
    assert_eq!(
        after_second_image.pane_snapshots[0].image_placement_snapshots[1].get_image_content_id(),
        2
    );
    assert_eq!(
        after_second_image.pane_snapshots[0].image_placement_snapshots[1]
            .get_image_record()
            .map(|image_record| image_record.image.rgba_bytes.as_slice()),
        Some([255, 0, 0, 255, 0, 255, 0, 255].as_slice())
    );
}

#[test]
fn a_frame_without_a_cached_placement_releases_its_rgba_bytes() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes.clone()))
        .expect("the placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the image reads")
        .expect("the image produces a redraw");
    frame_bytes.pane_snapshots[0]
        .image_placement_snapshots
        .clear();

    let snapshot_without_image = cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the frame without the placement reads")
        .expect("the frame without an image is complete");

    assert_eq!(
        snapshot_without_image.pane_snapshots[0].image_placement_snapshots,
        Vec::new()
    );
    assert_eq!(cache.image_record_by_content_id.len(), 0);
    assert_eq!(cache.retained_image_byte_count, 0);
    assert_eq!(cache.missing_image_content_ids.len(), 0);
}

#[test]
fn a_returning_image_waits_for_its_new_connection_identity() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let initial_frame_bytes = wire_frame(&expected_render_snapshot);
    let mut frame_without_image = initial_frame_bytes.clone();
    frame_without_image.pane_snapshots[0]
        .image_placement_snapshots
        .clear();
    let mut returning_frame_bytes = initial_frame_bytes.clone();
    returning_frame_bytes.pane_snapshots[0].image_placement_snapshots[0].image_content_id = 2;
    let mut cache = ImageCache::new();

    assert_eq!(
        cache
            .adopt_painted_frame(Box::new(initial_frame_bytes))
            .expect("the first frame reads"),
        None
    );
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the first transfer reads")
        .expect("the first image completes the frame");
    let snapshot_without_image = cache
        .adopt_painted_frame(Box::new(frame_without_image))
        .expect("the frame without the image reads")
        .expect("the frame without the image is complete");
    assert_eq!(
        snapshot_without_image.pane_snapshots[0].image_placement_snapshots,
        []
    );
    assert_eq!(cache.image_record_by_content_id.len(), 0);

    assert_eq!(
        cache
            .adopt_painted_frame(Box::new(returning_frame_bytes))
            .expect("the returning frame reads"),
        None
    );
    assert_eq!(cache.image_record_by_content_id.len(), 0);
    assert_eq!(cache.missing_image_content_ids, HashSet::from([2]));
    let mut image_transfer = build_image_transfer(2);
    image_transfer.image_content_id = 2;
    cache
        .start_image_transfer(image_transfer)
        .expect("the returning transfer starts");
    let mut image_chunk = build_image_chunk(2);
    image_chunk.image_transfer_id = 2;
    let rebuilt_snapshot = cache
        .accept_image_chunk(image_chunk)
        .expect("the returning transfer reads")
        .expect("the returning image completes the frame");

    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_image_content_id(),
        2
    );
    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .expect("the returning image has pixels")
            .image
            .rgba_bytes,
        [255, 0, 0, 255, 0, 255, 0, 255]
    );
}

#[test]
fn a_rejected_chunk_closes_its_transfer_and_allows_an_exact_restart() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(wire_frame(&expected_render_snapshot)))
        .expect("the placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    let image_assembly_error = cache
        .accept_image_chunk(FrameImageChunk {
            image_transfer_id: 1,
            byte_offset: 1,
            is_last: true,
            chunk_bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
        })
        .expect_err("the wrong offset is refused");

    assert_eq!(
        image_assembly_error,
        ImageAssemblyError::WrongOffset {
            image_transfer_id: 1,
            expected_byte_offset: 0,
            actual_byte_offset: 1,
        }
    );
    assert_eq!(
        cache
            .pending_image_transfer
            .as_ref()
            .map(|pending| pending.received_byte_count),
        None
    );

    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the same transfer can restart");
    let completed_snapshot = cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the restarted transfer reads")
        .expect("the restarted transfer produces a redraw");
    assert_eq!(completed_snapshot, expected_render_snapshot);
}

#[test]
fn one_pane_cannot_repeat_a_terminal_image_placement_identity() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    let mut repeated_image_placement =
        frame_bytes.pane_snapshots[0].image_placement_snapshots[0].clone();
    repeated_image_placement.image_content_id = 2;
    frame_bytes.pane_snapshots[0]
        .image_placement_snapshots
        .push(repeated_image_placement);
    let mut cache = ImageCache::new();

    let image_assembly_error = cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect_err("the repeated placement identity is refused");

    assert_eq!(image_assembly_error, ImageAssemblyError::DuplicatePlacement);
    assert_eq!(cache.image_record_by_content_id.len(), 0);
    assert_eq!(cache.retained_image_byte_count, 0);
    assert_eq!(cache.painted_frame, None);
    assert_eq!(cache.missing_image_content_ids.len(), 0);
    assert_eq!(
        cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image| pending_image.received_byte_count),
        None
    );
}

#[test]
fn an_image_cache_reset_discards_complete_and_incomplete_connection_state() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(wire_frame(&expected_render_snapshot)))
        .expect("the placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");

    cache.clear_image_cache();

    assert_eq!(cache.image_record_by_content_id.len(), 0);
    assert_eq!(cache.retained_image_byte_count, 0);
    assert_eq!(cache.painted_frame, None);
    assert_eq!(cache.missing_image_content_ids.len(), 0);
    assert_eq!(
        cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image| pending_image.received_byte_count),
        None
    );
    assert_eq!(
        cache.start_image_transfer(build_image_transfer(1)),
        Err(ImageAssemblyError::MissingBaseFrame)
    );
}

#[test]
fn an_image_chunk_with_a_wrong_offset_is_refused_exactly() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let frame_bytes = wire_frame(&expected_render_snapshot);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    let image_assembly_error = cache
        .accept_image_chunk(FrameImageChunk {
            image_transfer_id: 1,
            byte_offset: 1,
            is_last: true,
            chunk_bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
        })
        .expect_err("the first chunk must start at offset zero");

    assert_eq!(
        image_assembly_error,
        ImageAssemblyError::WrongOffset {
            image_transfer_id: 1,
            expected_byte_offset: 0,
            actual_byte_offset: 1,
        }
    );
}

#[test]
fn image_transfer_metadata_cannot_reserve_more_than_the_frame_limit() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes.clone()))
        .expect("the first placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the first image reads")
        .expect("the first image produces a redraw");
    frame_bytes.pane_snapshots[0]
        .image_placement_snapshots
        .push(FrameImagePlacement {
            cell_geometry: None,
            image_record: None,
            placement_id: 42,
            image_content_id: 2,
            is_available: true,
            anchor_cell: (0, 0),
            column_count: 1,
            row_count: 1,
        });
    cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the two-placement frame reads");
    let oversized_image_transfer = FrameImageTransfer {
        image_content_id: 2,
        image_record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            pixel_width: 4_096,
            pixel_height: 4_096,
            image_action: FrameImageAction::Display,
            display: FrameImageDisplay::default(),
            anchor_cell: (0, 0),
        },
        image_byte_count: MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT,
    };

    let image_assembly_error = cache
        .start_image_transfer(oversized_image_transfer)
        .expect_err("the retained and incoming records are over the limit");
    assert_eq!(
        image_assembly_error,
        ImageAssemblyError::TransferBytesExceedFrame
    );
}

#[test]
fn image_placements_from_several_panes_do_not_share_one_pane_limit() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    frame_bytes.pane_snapshots[0].image_placement_snapshots = (0..MAX_FRAME_IMAGE_TRANSFER_COUNT)
        .map(|placement_index| FrameImagePlacement {
            cell_geometry: None,
            image_record: None,
            placement_id: u64::try_from(placement_index + 1).expect("the placement identity fits"),
            image_content_id: u64::try_from(placement_index + 1)
                .expect("the content identity fits"),
            is_available: false,
            anchor_cell: (0, 0),
            column_count: 1,
            row_count: 1,
        })
        .collect();
    frame_bytes.pane_snapshots[1]
        .image_placement_snapshots
        .push(FrameImagePlacement {
            cell_geometry: None,
            image_record: None,
            placement_id: 1,
            image_content_id: u64::try_from(MAX_FRAME_IMAGE_TRANSFER_COUNT + 1)
                .expect("the content identity fits"),
            is_available: false,
            anchor_cell: (0, 0),
            column_count: 1,
            row_count: 1,
        });
    let mut cache = ImageCache::new();

    let rebuilt_snapshot = cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("placements in distinct panes are accepted")
        .expect("unavailable placements need no image transfer");

    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0]
            .image_placement_snapshots
            .len(),
        4_096
    );
    assert_eq!(
        rebuilt_snapshot.pane_snapshots[1]
            .image_placement_snapshots
            .len(),
        1
    );
}

#[test]
fn one_painted_frame_accepts_at_most_4096_image_transfers() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    frame_bytes.pane_snapshots[0]
        .image_placement_snapshots
        .push(FrameImagePlacement {
            cell_geometry: None,
            image_record: None,
            placement_id: 42,
            image_content_id: 2,
            is_available: true,
            anchor_cell: (0, 0),
            column_count: 1,
            row_count: 1,
        });
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the placement frame reads");
    cache.image_transfer_count = MAX_FRAME_IMAGE_TRANSFER_COUNT - 1;
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("transfer 4096 starts");
    cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("transfer 4096 completes");

    let image_assembly_error = cache
        .start_image_transfer(build_image_transfer(2))
        .expect_err("transfer 4097 is rejected");

    assert_eq!(
        image_assembly_error,
        ImageAssemblyError::TransferCountExceedsFrame
    );
}

#[test]
fn an_unavailable_placement_does_not_hold_back_an_available_image() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    frame_bytes.pane_snapshots[0]
        .image_placement_snapshots
        .push(FrameImagePlacement {
            cell_geometry: None,
            image_record: None,
            placement_id: 42,
            image_content_id: 2,
            is_available: false,
            anchor_cell: (0, 0),
            column_count: 1,
            row_count: 1,
        });
    let mut cache = ImageCache::new();
    cache
        .adopt_painted_frame(Box::new(frame_bytes))
        .expect("the mixed placement frame reads");
    cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");

    let rebuilt_snapshot = cache
        .accept_image_chunk(build_image_chunk(1))
        .expect("the available image completes")
        .expect("the available image produces a redraw");
    let image_transfer = build_image_transfer(1);
    let expected_image_record = build_image_record(
        &image_transfer.image_record,
        build_image_chunk(1).chunk_bytes,
    );

    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_image_record(),
        Some(&expected_image_record)
    );
    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[1].get_image_record(),
        None
    );
    assert_eq!(
        cache.start_image_transfer(build_image_transfer(2)),
        Err(ImageAssemblyError::UnknownTransfer {
            image_content_id: 2,
        })
    );
}

#[test]
fn the_tabs_gap_arrives_with_the_frame() {
    let expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut frame_bytes = wire_frame(&expected_render_snapshot);
    frame_bytes
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count = 2;

    assert_eq!(
        super::build_render_snapshot(&frame_bytes)
            .session_snapshot
            .active_tab_snapshot
            .gap_cell_count,
        2
    );
}

#[test]
fn a_soft_wrapped_row_arrives_still_soft_wrapped() {
    // A shell printing a line longer than the pane is wide leaves the first row
    // soft-wrapped and the last row hard-ended. Without the row-end state on the
    // wire every row reads as hard, and copying the text out breaks the line at
    // the wrap.
    let mut terminal_grid = Grid::from_rows(
        vec![
            vec![Cell::from_character('a', 1, Style::default())],
            vec![Cell::from_character('b', 1, Style::default())],
            vec![Cell::from_character('c', 1, Style::default())],
        ],
        1,
        Style::default(),
    );
    terminal_grid.set_row_end(0, RowEnd::Soft);
    terminal_grid.set_row_end(1, RowEnd::SoftWide);

    let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
    content_pane_snapshot.terminal_grid_view = Some(GridView {
        grid: Arc::new(terminal_grid),
        view_row_offset: 0,
    });
    let expected_render_snapshot = build_render_snapshot(vec![
        content_pane_snapshot,
        build_empty_pane_snapshot(PaneId::new()),
    ]);

    let received_snapshot = super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

    let received_grid_view = received_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(received_grid_view.grid.get_row_end(0), RowEnd::Soft);
    assert_eq!(received_grid_view.grid.get_row_end(1), RowEnd::SoftWide);
    assert_eq!(received_grid_view.grid.get_row_end(2), RowEnd::Hard);
    assert_eq!(received_snapshot, expected_render_snapshot);
}

#[test]
fn a_highlight_scrolled_entirely_off_screen_arrives_as_no_highlight() {
    // The session resolves the highlight to the rows this frame shows before
    // it builds the pane, so a highlight above every visible row leaves
    // `selection` empty while `has_selection` still reports it exists.
    let mut off_screen_pane_snapshot = build_content_pane_snapshot(PaneId::new());
    off_screen_pane_snapshot.selection_spans = None;
    off_screen_pane_snapshot.has_selection = true;
    let expected_render_snapshot = build_render_snapshot(vec![
        off_screen_pane_snapshot,
        build_empty_pane_snapshot(PaneId::new()),
    ]);

    let received_snapshot = super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

    assert_eq!(received_snapshot.pane_snapshots[0].selection_spans, None);
    assert!(received_snapshot.pane_snapshots[0].has_selection);
    assert_eq!(received_snapshot, expected_render_snapshot);
}

/// The session decides this viewer's lock mode and whether mouse-select is on,
/// so a painted frame is where both are read from, and the same frame cut down
/// to [`MouseFrame`] is what the next mouse event is placed against.
#[test]
fn adopting_a_frame_takes_the_viewer_state_the_session_decided() {
    let (_events_sender, events_receiver) = std::sync::mpsc::sync_channel(8);
    let mut client = crate::Client::from_client_id_and_viewport(
        ClientId::new(),
        Size {
            column_count: 20,
            row_count: 6,
        },
        events_receiver,
        koshi_observability::cleanup::TerminalCleanupGuard::new(),
    );
    // A fresh viewer is unlocked with mouse-select off. The frame carries both
    // the other way.
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());

    let render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let focused_pane_id = render_snapshot.client_snapshot.focused_pane_id;

    super::super::apply_frame_to_client(&mut client, &render_snapshot);

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(client.is_mouse_selection_enabled());

    let mouse_frame = koshi_renderer::snapshot::MouseFrame::from(render_snapshot);
    assert_eq!(mouse_frame.client_snapshot.active_tab_id, active_tab_id);
    assert_eq!(mouse_frame.client_snapshot.focused_pane_id, focused_pane_id);
}

/// Each underline style has its own wire spelling, so a variant read back as
/// another one shows up here. The cell also carries a default foreground, which
/// is the one color whose wire spelling names no value.
#[test]
fn every_underline_style_reads_back_as_itself() {
    for underline_style in [
        UnderlineStyle::None,
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ] {
        let mut cell_style = Style::default();
        cell_style.set_foreground_color(Color::Default);
        cell_style.set_underline(underline_style);
        let terminal_grid = Grid::from_rows(
            vec![vec![Cell::from_character('x', 1, cell_style)]],
            1,
            Style::default(),
        );
        let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
        content_pane_snapshot.terminal_grid_view = Some(GridView {
            grid: Arc::new(terminal_grid),
            view_row_offset: 0,
        });
        let expected_render_snapshot = build_render_snapshot(vec![
            content_pane_snapshot,
            build_empty_pane_snapshot(PaneId::new()),
        ]);

        let received_snapshot =
            super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

        let received_grid_view = received_snapshot.pane_snapshots[0]
            .terminal_grid_view
            .as_ref()
            .expect("the pane carries a grid");
        let received_cell = received_grid_view
            .grid
            .get_cell(0, 0)
            .expect("the grid has a cell at (0, 0)");
        assert_eq!(
            received_cell
                .get_style()
                .get_attributes()
                .get_underline_style(),
            underline_style
        );
        assert_eq!(
            received_cell.get_style().get_foreground_color(),
            Color::Default
        );
        assert_eq!(received_snapshot, expected_render_snapshot);
    }
}

/// Each cursor shape has its own wire spelling, and a pane that named none
/// reads back naming none.
#[test]
fn every_cursor_shape_reads_back_as_itself() {
    for cursor_shape in [
        Some(CursorShape::Block),
        Some(CursorShape::Underline),
        Some(CursorShape::Bar),
        None,
    ] {
        let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
        content_pane_snapshot.cursor_snapshot.shape = cursor_shape;
        let expected_render_snapshot = build_render_snapshot(vec![
            content_pane_snapshot,
            build_empty_pane_snapshot(PaneId::new()),
        ]);

        let received_snapshot =
            super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

        assert_eq!(
            received_snapshot.pane_snapshots[0].cursor_snapshot.shape,
            cursor_shape
        );
        assert_eq!(received_snapshot, expected_render_snapshot);
    }
}

/// A run stands for every cell it covers, so a blank 80-column row travels as
/// one run of 80 and rebuilds into 80 cells.
#[test]
fn a_blank_eighty_column_row_travels_as_one_run_and_rebuilds_eighty_cells() {
    let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
    content_pane_snapshot.terminal_grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(1, 80, Style::default())),
        view_row_offset: 0,
    });
    let expected_render_snapshot = build_render_snapshot(vec![
        content_pane_snapshot,
        build_empty_pane_snapshot(PaneId::new()),
    ]);

    let frame_bytes = wire_frame(&expected_render_snapshot);
    let terminal_window = frame_bytes.pane_snapshots[0]
        .terminal_window
        .as_ref()
        .expect("the pane carries a window");
    assert_eq!(terminal_window.column_count, 80);
    assert_eq!(terminal_window.row_snapshots.len(), 1);
    assert_eq!(terminal_window.row_snapshots[0].cell_runs.len(), 1);
    assert_eq!(
        terminal_window.row_snapshots[0].cell_runs[0].repeat_count,
        80
    );

    let received_snapshot = super::build_render_snapshot(&frame_bytes);
    let received_grid_view = received_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(received_grid_view.grid.get_grid_dimensions(), (1, 80));
    assert_eq!(received_grid_view.grid.list_rows()[0].len(), 80);
    assert_eq!(received_snapshot, expected_render_snapshot);
}

/// A frame whose panes all closed carries no panes, and reads back carrying
/// none.
#[test]
fn a_frame_carrying_no_panes_reads_back_with_no_panes() {
    let mut expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    expected_render_snapshot.pane_snapshots = Vec::new();

    let received_snapshot = super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

    assert_eq!(received_snapshot.pane_snapshots, Vec::<PaneSnapshot>::new());
    assert_eq!(received_snapshot, expected_render_snapshot);
}

#[test]
fn every_name_the_answering_session_chose_reads_back_filtered() {
    // This process paints all four into its own terminal, and puts the session
    // name and the focused pane's title inside an `OSC 0` window title.
    let focused_pane_id = PaneId::new();
    let mut expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(focused_pane_id),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    expected_render_snapshot.session_snapshot.session_name =
        String::from("dev\u{7}\u{1b}]0;owned\u{7}");
    expected_render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .tab_name = String::from("build\u{1b}[2J");
    expected_render_snapshot.session_snapshot.tabs_metadata[0].tab_name =
        String::from("build\u{9b}2J");
    expected_render_snapshot.session_snapshot.tabs_metadata[1].tab_name =
        String::from("\u{202e}gpj.exe");
    expected_render_snapshot.pane_snapshots[0].pane_title =
        Some(String::from("~/work\u{7}\u{1b}]0;owned\u{7}"));

    let received_snapshot = super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

    assert_eq!(
        received_snapshot.session_snapshot.session_name,
        "dev]0;owned"
    );
    assert_eq!(
        received_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_name,
        "build[2J"
    );
    assert_eq!(
        received_snapshot.session_snapshot.tabs_metadata[0].tab_name,
        "build2J"
    );
    assert_eq!(
        received_snapshot.session_snapshot.tabs_metadata[1].tab_name,
        "gpj.exe"
    );
    assert_eq!(
        received_snapshot.pane_snapshots[0].pane_title.as_deref(),
        Some("~/work]0;owned"),
        "the pane title reaches the window title"
    );
}

#[test]
fn a_name_past_the_reported_text_cap_reads_back_cut_to_it() {
    let reported_text_byte_limit = koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;
    let mut expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    expected_render_snapshot.session_snapshot.session_name =
        "a".repeat(reported_text_byte_limit + 1);

    let received_snapshot = super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

    assert_eq!(
        received_snapshot.session_snapshot.session_name,
        "a".repeat(reported_text_byte_limit)
    );
}

#[test]
fn a_pane_cell_holding_a_control_character_reads_back_holding_it() {
    // A cell is the pane's own screen. The grid stores what the pane drew, and
    // the renderer places each cell rather than writing it through.
    let content_pane_id = PaneId::new();
    let mut expected_render_snapshot = build_render_snapshot(vec![
        build_content_pane_snapshot(content_pane_id),
        build_empty_pane_snapshot(PaneId::new()),
    ]);
    let mut terminal_grid = build_test_grid();
    *terminal_grid
        .get_cell_mut(1, 0)
        .expect("the grid has a cell at (1, 0)") =
        Cell::from_character('\u{1b}', 1, Style::default());
    expected_render_snapshot.pane_snapshots[0].terminal_grid_view = Some(GridView {
        grid: Arc::new(terminal_grid),
        view_row_offset: 0,
    });

    let received_snapshot = super::build_render_snapshot(&wire_frame(&expected_render_snapshot));

    let received_grid_view = received_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(
        received_grid_view
            .grid
            .get_cell(1, 0)
            .expect("the cell survives")
            .get_character(),
        '\u{1b}'
    );
}
