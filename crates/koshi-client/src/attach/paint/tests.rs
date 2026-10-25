//! Tests for [`build_render_snapshot`]: a frame that travels and is read back is the
//! frame that was sent — every style field, a wide glyph and its continuation
//! half, a combining mark, both cursor states, the scroll offset, the mouse
//! mode, a pane with no grid at all, the highlight rows, and the floating pane
//! slots in their order.

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::frame::{
    FrameGraphicsProtocol, FrameImageAction, FrameImageDisplay, FrameImagePlacement,
    FrameImageRecordHeader, FrameImageTransfer, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT,
    MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT, MAX_FRAME_IMAGE_TRANSFER_COUNT,
};
use koshi_ipc::image_chunk::ImageChunkFrame;
use koshi_ipc::transport::ReceivedFrame;
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{CursorSnapshot, GridView};
use koshi_runtime::runtime::frame::build_wire_frame;
use koshi_runtime::runtime::painted_update::{
    PaintedUpdateReceiver, PaintedUpdateSender, SessionFrame,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};
use koshi_terminal::grid::state::{Cell, Grid, RowEnd};
use koshi_terminal::state::CursorShape;
use koshi_terminal::style::{Color, Style, UnderlineStyle};
use koshi_test_support::fixtures::build_image_chunk_frame;

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

/// The transfer metadata for [`build_image_placement_snapshot`].
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

/// The RGBA bytes of [`build_image_transfer`]'s 2×1 image: one red pixel,
/// then one green pixel.
const TEST_IMAGE_RGBA_BYTES: [u8; 8] = [255, 0, 0, 255, 0, 255, 0, 255];

/// All RGBA bytes for [`build_image_transfer`] in one final chunk.
fn build_image_chunk(image_transfer_id: u64) -> ImageChunkFrame {
    build_image_chunk_frame(image_transfer_id, 0, true, &TEST_IMAGE_RGBA_BYTES)
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
    let image_placement_snapshot = build_image_placement_snapshot_from_record(
        &frame_image_placement,
        Some(&cached_image_record),
    )
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
        build_image_placement_snapshot_from_record(
            &frame_image_placement,
            Some(&cached_image_record)
        ),
        None
    );
}

/// Sets every style field away from its default.
fn build_test_style() -> Style {
    let mut terminal_style = Style::default();
    terminal_style.set_foreground_color(Color::Indexed(4));
    terminal_style.set_background_color(Color::Rgb(10, 20, 30));
    terminal_style.set_underline_color(Some(Color::Indexed(9)));
    terminal_style.set_bold(true);
    terminal_style.set_italic(true);
    terminal_style.set_reverse(true);
    terminal_style.set_faint(true);
    terminal_style.set_blinking(true);
    terminal_style.set_concealed(true);
    terminal_style.set_strikethrough(true);
    terminal_style.set_overlined(true);
    terminal_style.set_underline(UnderlineStyle::Curly);
    terminal_style
}

/// A 2×4 grid whose first row holds, left to right: a styled `e` carrying a
/// combining acute accent (U+0301), the wide glyph `漢` at width 2, the blank
/// continuation half it occupies at width 0, and one default blank. The second
/// row is all default blanks.
fn build_test_grid() -> Grid {
    let mut terminal_grid = Grid::build_blank(2, 4, Style::default());
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

/// The pane holding [`build_test_grid`]: scrolled 7 lines back, reporting any-motion mouse
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
        terminal_grid_view: Some(GridView::from_grid(&build_test_grid(), 7)),
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
        scrollback_metadata: ScrollbackMetadata {
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

/// A frame with one tab, two slots, and the two panes handed in.
fn build_two_pane_render_snapshot(pane_snapshots: Vec<PaneSnapshot>) -> RenderSnapshot {
    let content_pane_id = pane_snapshots[0].pane_id;
    let empty_pane_id = pane_snapshots[1].pane_id;
    let active_tab_id = TabId::new();
    let secondary_tab_id = TabId::new();
    RenderSnapshot {
        is_recovery_notice_visible: false,
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
                            size: Size {
                                column_count: 6,
                                row_count: 4,
                            },
                        },
                        content_rect: Some(Rect {
                            origin: Point { column: 1, row: 1 },
                            size: Size {
                                column_count: 4,
                                row_count: 2,
                            },
                        }),
                        is_visible: true,
                        is_suppressed: false,
                    },
                    PaneSlot {
                        pane_id: empty_pane_id,
                        outer_rect: Rect {
                            origin: Point { column: 6, row: 0 },
                            size: Size {
                                column_count: 6,
                                row_count: 4,
                            },
                        },
                        content_rect: None,
                        is_visible: false,
                        is_suppressed: true,
                    },
                ],
                tab_size: Size {
                    column_count: 12,
                    row_count: 4,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Fullscreen {
                    focused_pane_id: content_pane_id,
                },
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![
                TabMetadata {
                    tab_id: active_tab_id,
                    tab_name: String::from("tab"),
                    tab_index: 0,
                    is_active: true,
                },
                TabMetadata {
                    tab_id: secondary_tab_id,
                    tab_name: String::from("other"),
                    tab_index: 1,
                    is_active: false,
                },
            ],
            floating_pane_slots: Vec::new(),
            floating_panes: Vec::new(),
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
            active_tab_focused_pane_id: Some(content_pane_id),
            lock_mode: LockMode::Locked,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled: true,
        },
        placement_preview: None,
    }
}

/// An available placement preview of `render_snapshot`'s first pane from its
/// active tab into a new tab ([`crate::tests::build_test_placement_snapshot`]).
fn build_test_placement_preview(render_snapshot: &RenderSnapshot) -> PlacementPreview {
    PlacementPreview::Available(Box::new(crate::tests::build_test_placement_snapshot(
        render_snapshot.pane_snapshots[0].pane_id,
        render_snapshot.client_snapshot.active_tab_id,
        TabId::new(),
    )))
}

/// The painted frame a client reads when a connection that sent nothing sends
/// `render_snapshot`, whose JSON part is `painted_frame`.
pub(crate) fn carry_first_painted_frame(
    render_snapshot: &RenderSnapshot,
    painted_frame: &PaintedFrame,
) -> ReceivedPaintedFrame {
    carry_painted_update(
        &mut PaintedUpdateSender::new(),
        &mut PaintedUpdateReceiver::new(),
        render_snapshot,
        painted_frame,
    )
}

/// The painted frame `painted_update_receiver` applies when
/// `painted_update_sender` sends `render_snapshot`, whose JSON part is
/// `painted_frame`.
fn carry_painted_update(
    painted_update_sender: &mut PaintedUpdateSender,
    painted_update_receiver: &mut PaintedUpdateReceiver,
    render_snapshot: &RenderSnapshot,
    painted_frame: &PaintedFrame,
) -> ReceivedPaintedFrame {
    let part_payloads = painted_update_sender
        .build_update_frames(render_snapshot, painted_frame, false)
        .expect("the painted update fits the frame cap")
        .expect("a changed update is sent");
    let mut session_frame = None;
    for part_payload in part_payloads {
        session_frame = painted_update_receiver
            .accept_session_frame::<()>(ReceivedFrame::Binary(part_payload))
            .expect("a sent part applies");
    }
    match session_frame {
        Some(SessionFrame::Painted(received_painted_frame)) => *received_painted_frame,
        unexpected_frame => panic!("the last part gives the applied update: {unexpected_frame:?}"),
    }
}

/// The render snapshot a client builds from the first painted update that
/// carries `render_snapshot`.
fn build_received_render_snapshot(render_snapshot: &RenderSnapshot) -> RenderSnapshot {
    build_render_snapshot(&carry_first_painted_frame(
        render_snapshot,
        &build_wire_frame(render_snapshot),
    ))
}

#[test]
fn a_frame_that_travels_and_is_read_back_is_the_frame_that_was_sent() {
    let empty_pane_id = PaneId::new();
    let (lower_floating_pane_id, upper_floating_pane_id) = (PaneId::new(), PaneId::new());
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(empty_pane_id),
    ]);
    expected_render_snapshot
        .client_snapshot
        .active_tab_focused_pane_id = Some(empty_pane_id);
    expected_render_snapshot.is_recovery_notice_visible = true;
    expected_render_snapshot
        .client_snapshot
        .last_applied_lock_command_id = Some(CommandId::new());
    expected_render_snapshot
        .client_snapshot
        .last_applied_floating_pane_move_command_id = Some(CommandId::new());
    expected_render_snapshot
        .client_snapshot
        .last_applied_placement_command_id = Some(CommandId::new());
    expected_render_snapshot.placement_preview = Some(Arc::new(build_test_placement_preview(
        &expected_render_snapshot,
    )));
    expected_render_snapshot
        .session_snapshot
        .floating_pane_slots = vec![
        FloatingPaneSlot {
            pane_id: lower_floating_pane_id,
            outer_rect: Rect {
                origin: Point { column: 2, row: 1 },
                size: Size {
                    column_count: 10,
                    row_count: 5,
                },
            },
            content_rect: Rect {
                origin: Point { column: 3, row: 4 },
                size: Size {
                    column_count: 8,
                    row_count: 1,
                },
            },
            is_pinned: false,
        },
        FloatingPaneSlot {
            pane_id: upper_floating_pane_id,
            outer_rect: Rect {
                origin: Point { column: 8, row: 0 },
                size: Size {
                    column_count: 12,
                    row_count: 6,
                },
            },
            content_rect: Rect {
                origin: Point { column: 9, row: 3 },
                size: Size {
                    column_count: 10,
                    row_count: 2,
                },
            },
            is_pinned: true,
        },
    ];
    expected_render_snapshot.session_snapshot.floating_panes = vec![
        FloatingPane {
            pane_id: upper_floating_pane_id,
            pane_title: Some("logs".to_string()),
            is_minimized: false,
        },
        FloatingPane {
            pane_id: PaneId::new(),
            pane_title: Some("htop".to_string()),
            is_minimized: true,
        },
        FloatingPane {
            pane_id: lower_floating_pane_id,
            pane_title: None,
            is_minimized: false,
        },
    ];
    assert_eq!(
        build_received_render_snapshot(&expected_render_snapshot),
        expected_render_snapshot
    );
}

#[test]
fn an_unavailable_placement_preview_travels_and_is_read_back_as_itself() {
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    expected_render_snapshot.placement_preview = Some(Arc::new(PlacementPreview::Unavailable {
        source_pane_id: expected_render_snapshot.pane_snapshots[0].pane_id,
        destination_tab_id: TabId::new(),
    }));

    assert_eq!(
        build_received_render_snapshot(&expected_render_snapshot),
        expected_render_snapshot
    );
}

#[test]
fn the_image_cache_builds_a_placement_preview_once_while_updates_hold_the_same_preview() {
    let mut render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let sent_placement_preview = build_test_placement_preview(&render_snapshot);
    render_snapshot.placement_preview = Some(Arc::new(sent_placement_preview.clone()));
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    let mut image_cache = ImageCache::new();
    let mut adopt_update = |render_snapshot: &RenderSnapshot| {
        image_cache
            .adopt_painted_frame(Box::new(carry_painted_update(
                &mut painted_update_sender,
                &mut painted_update_receiver,
                render_snapshot,
                &build_wire_frame(render_snapshot),
            )))
            .expect("the update reads")
            .expect("the update holds no image")
            .placement_preview
            .expect("the update holds the preview")
    };

    let first_placement_preview = adopt_update(&render_snapshot);
    render_snapshot.client_snapshot.lock_mode = LockMode::Normal;
    let unchanged_placement_preview = adopt_update(&render_snapshot);
    render_snapshot.placement_preview = Some(Arc::new(sent_placement_preview.clone()));
    let replaced_placement_preview = adopt_update(&render_snapshot);

    assert_eq!(*first_placement_preview, sent_placement_preview);
    assert!(
        Arc::ptr_eq(&first_placement_preview, &unchanged_placement_preview),
        "an update that keeps the preview shares the preview the cache built"
    );
    assert!(
        !Arc::ptr_eq(&unchanged_placement_preview, &replaced_placement_preview),
        "an update that replaces the preview gets a preview built anew"
    );
    assert_eq!(*replaced_placement_preview, sent_placement_preview);
}

#[test]
fn a_painted_update_with_an_invalid_placement_preview_is_refused_and_the_cache_keeps_its_frame() {
    let base_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &base_render_snapshot,
            &build_wire_frame(&base_render_snapshot),
        )))
        .expect("the base frame reads");
    let held_painted_frame = image_cache.painted_frame.clone();
    let mut invalid_render_snapshot = base_render_snapshot.clone();
    let mut placement_snapshot = crate::tests::build_test_placement_snapshot(
        base_render_snapshot.pane_snapshots[0].pane_id,
        base_render_snapshot.client_snapshot.active_tab_id,
        TabId::new(),
    );
    // A preview into another tab that holds no destination tab.
    placement_snapshot.destination_tab_snapshot = None;
    invalid_render_snapshot.placement_preview = Some(Arc::new(PlacementPreview::Available(
        Box::new(placement_snapshot),
    )));

    assert_eq!(
        image_cache.adopt_painted_frame(Box::new(carry_first_painted_frame(
            &invalid_render_snapshot,
            &build_wire_frame(&invalid_render_snapshot),
        ))),
        Err(ImageAssemblyError::InvalidPlacementPreview)
    );
    assert_eq!(image_cache.painted_frame, held_painted_frame);
    assert!(image_cache.placement_preview.is_none());
}

#[test]
fn a_frame_waits_for_the_complete_image_record() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut image_cache = ImageCache::new();

    let initial_snapshot = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the placement frame reads");
    assert_eq!(initial_snapshot, None);

    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    let rebuilt_snapshot = image_cache
        .accept_image_chunk(&build_image_chunk(1))
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
fn a_frame_whose_placements_are_unavailable_paints_without_waiting() {
    let render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&render_snapshot);
    let image_placement = &mut painted_frame.pane_snapshots[0].image_placement_snapshots[0];
    image_placement.image_record = None;
    image_placement.is_available = false;
    let mut image_cache = ImageCache::new();

    let painted_snapshot = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &render_snapshot,
            &painted_frame,
        )))
        .expect("the placement frame reads")
        .expect("a frame with no available placement paints at once");

    assert_eq!(
        painted_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_image_record(),
        None
    );
}

#[test]
fn a_complete_cached_image_is_reused_by_the_next_frame() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the first placement frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the image produces a redraw");

    let reused_snapshot = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the repeated placement frame reads");

    assert_eq!(reused_snapshot, Some(expected_render_snapshot));
    assert_eq!(image_cache.image_record_by_content_id.len(), 1);
    assert!(image_cache.missing_painted_image_content_ids.is_empty());
    assert_eq!(
        image_cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image_transfer| pending_image_transfer.received_byte_count),
        None
    );
}

#[test]
fn a_cached_record_cannot_hide_an_invalid_new_placement() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    let valid_received_frame = carry_first_painted_frame(&expected_render_snapshot, &painted_frame);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(valid_received_frame.clone()))
        .expect("the valid frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the image produces a redraw");
    painted_frame.pane_snapshots[0].image_placement_snapshots[0].anchor_cell = (u16::MAX, 0);
    painted_frame.pane_snapshots[0].image_placement_snapshots[0].row_count = 2;
    // The wire refuses to read this placement in a JSON part. The cache gets
    // it beside the pane views of the valid frame.
    let invalid_received_frame = ReceivedPaintedFrame {
        painted_frame: Arc::new(painted_frame),
        ..valid_received_frame
    };

    let image_assembly_error = image_cache
        .adopt_painted_frame(Box::new(invalid_received_frame))
        .expect_err("the cached record cannot make bad geometry valid");

    assert_eq!(image_assembly_error, ImageAssemblyError::InvalidPlacement);
    assert_eq!(image_cache.image_record_by_content_id.len(), 1);
    assert_eq!(image_cache.retained_image_byte_count, 8);
}

#[test]
fn a_frame_redraws_only_after_every_missing_image_record_arrives() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    painted_frame.pane_snapshots[0]
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
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the placement frame reads");

    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    let snapshot_after_first_image = image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the first complete record reads");
    image_cache
        .start_image_transfer(build_image_transfer(2))
        .expect("the second transfer starts");
    let snapshot_after_second_image = image_cache
        .accept_image_chunk(&build_image_chunk(2))
        .expect("the second complete record reads")
        .expect("the last record produces one redraw");

    assert_eq!(snapshot_after_first_image, None);
    assert_eq!(
        snapshot_after_second_image.pane_snapshots[0]
            .image_placement_snapshots
            .len(),
        2
    );
    assert_eq!(
        snapshot_after_second_image.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_content_id(),
        1
    );
    assert_eq!(
        snapshot_after_second_image.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .map(|image_record| image_record.image.rgba_bytes.as_slice()),
        Some([255, 0, 0, 255, 0, 255, 0, 255].as_slice())
    );
    assert_eq!(
        snapshot_after_second_image.pane_snapshots[0].image_placement_snapshots[1]
            .get_image_content_id(),
        2
    );
    assert_eq!(
        snapshot_after_second_image.pane_snapshots[0].image_placement_snapshots[1]
            .get_image_record()
            .map(|image_record| image_record.image.rgba_bytes.as_slice()),
        Some([255, 0, 0, 255, 0, 255, 0, 255].as_slice())
    );
}

#[test]
fn a_frame_without_the_placement_keeps_the_record_until_its_release() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the placement frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the image reads")
        .expect("the image produces a redraw");
    painted_frame.pane_snapshots[0]
        .image_placement_snapshots
        .clear();

    let snapshot_without_image = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the frame without the placement reads")
        .expect("the frame without an image is complete");

    assert_eq!(
        snapshot_without_image.pane_snapshots[0].image_placement_snapshots,
        Vec::new()
    );
    assert_eq!(
        image_cache
            .image_record_by_content_id
            .keys()
            .collect::<Vec<_>>(),
        vec![&1]
    );
    assert_eq!(image_cache.retained_image_byte_count, 8);

    image_cache
        .release_image_contents(&[1])
        .expect("the record no frame shows is released");

    assert_eq!(image_cache.image_record_by_content_id.len(), 0);
    assert_eq!(image_cache.retained_image_byte_count, 0);
    assert_eq!(image_cache.missing_painted_image_content_ids.len(), 0);
}

#[test]
fn a_release_of_a_record_the_cache_does_not_hold_is_refused() {
    let mut image_cache = ImageCache::new();

    assert_eq!(
        image_cache.release_image_contents(&[4]),
        Err(ImageAssemblyError::UnknownRelease {
            image_content_id: 4
        })
    );
}

#[test]
fn a_release_of_a_record_the_newest_frame_shows_is_refused_and_keeps_it() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &build_wire_frame(&expected_render_snapshot),
        )))
        .expect("the placement frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the image reads")
        .expect("the image produces a redraw");

    assert_eq!(
        image_cache.release_image_contents(&[1]),
        Err(ImageAssemblyError::ReleasedImageInUse {
            image_content_id: 1
        })
    );
    assert_eq!(image_cache.retained_image_byte_count, 8);
}

#[test]
fn a_returning_image_waits_for_its_new_connection_identity() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let initial_painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut painted_frame_without_image = initial_painted_frame.clone();
    painted_frame_without_image.pane_snapshots[0]
        .image_placement_snapshots
        .clear();
    let mut returning_painted_frame = initial_painted_frame.clone();
    returning_painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id = 2;
    let mut image_cache = ImageCache::new();

    assert_eq!(
        image_cache
            .adopt_painted_frame(Box::new(carry_first_painted_frame(
                &expected_render_snapshot,
                &initial_painted_frame,
            )))
            .expect("the first frame reads"),
        None
    );
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the first transfer reads")
        .expect("the first image completes the frame");
    let snapshot_without_image = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame_without_image,
        )))
        .expect("the frame without the image reads")
        .expect("the frame without the image is complete");
    assert_eq!(
        snapshot_without_image.pane_snapshots[0].image_placement_snapshots,
        []
    );
    image_cache
        .release_image_contents(&[1])
        .expect("the record no frame shows is released");
    assert_eq!(image_cache.image_record_by_content_id.len(), 0);

    assert_eq!(
        image_cache
            .adopt_painted_frame(Box::new(carry_first_painted_frame(
                &expected_render_snapshot,
                &returning_painted_frame,
            )))
            .expect("the returning frame reads"),
        None
    );
    assert_eq!(image_cache.image_record_by_content_id.len(), 0);
    assert_eq!(
        image_cache.missing_painted_image_content_ids,
        HashSet::from([2])
    );
    let mut image_transfer = build_image_transfer(2);
    image_transfer.image_content_id = 2;
    image_cache
        .start_image_transfer(image_transfer)
        .expect("the returning transfer starts");
    let rebuilt_snapshot = image_cache
        .accept_image_chunk(&build_image_chunk(2))
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
fn a_replaced_image_paints_after_its_frame_the_old_release_and_the_new_record() {
    // The session replaces record 1 with record 2 in this order: the frame
    // that names record 2, the release of record 1, and record 2.
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let initial_painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut replacing_painted_frame = initial_painted_frame.clone();
    replacing_painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id = 2;
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &initial_painted_frame,
        )))
        .expect("the first frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the first transfer reads")
        .expect("the first image completes the frame");

    assert_eq!(
        image_cache
            .adopt_painted_frame(Box::new(carry_first_painted_frame(
                &expected_render_snapshot,
                &replacing_painted_frame,
            )))
            .expect("the replacing frame reads"),
        None
    );
    image_cache
        .release_image_contents(&[1])
        .expect("the record the replacing frame stops showing is released");
    image_cache
        .start_image_transfer(build_image_transfer(2))
        .expect("the replacing transfer starts");
    let replaced_snapshot = image_cache
        .accept_image_chunk(&build_image_chunk(2))
        .expect("the replacing transfer reads")
        .expect("the replacing image completes the frame");

    assert_eq!(
        image_cache
            .image_record_by_content_id
            .keys()
            .collect::<Vec<_>>(),
        vec![&2]
    );
    assert_eq!(image_cache.retained_image_byte_count, 8);
    assert_eq!(
        replaced_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_image_content_id(),
        2
    );
}

#[test]
fn each_malformed_chunk_gives_its_typed_error_closes_its_transfer_and_allows_an_exact_restart() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let oversized_chunk_bytes = vec![0; MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT + 1];
    let malformed_chunk_cases = [
        (
            "empty and not last",
            build_image_chunk_frame(1, 0, false, &[]),
            ImageAssemblyError::FinalMarkerMismatch,
        ),
        (
            "past the image's end",
            build_image_chunk_frame(1, 0, false, &[0; 12]),
            ImageAssemblyError::ChunkExceedsImage,
        ),
        (
            "one byte over 1 MiB",
            build_image_chunk_frame(1, 0, false, &oversized_chunk_bytes),
            ImageAssemblyError::ChunkTooLarge,
        ),
        (
            "wrong offset",
            build_image_chunk_frame(1, 1, true, &TEST_IMAGE_RGBA_BYTES),
            ImageAssemblyError::WrongOffset {
                image_transfer_id: 1,
                expected_byte_offset: 0,
                actual_byte_offset: 1,
            },
        ),
        (
            "wrong last flag",
            build_image_chunk_frame(1, 0, false, &TEST_IMAGE_RGBA_BYTES),
            ImageAssemblyError::FinalMarkerMismatch,
        ),
        (
            "unknown transfer",
            build_image_chunk_frame(2, 0, true, &TEST_IMAGE_RGBA_BYTES),
            ImageAssemblyError::UnknownTransfer {
                image_content_id: 2,
            },
        ),
    ];

    for (case_name, malformed_chunk_frame, expected_assembly_error) in malformed_chunk_cases {
        let mut image_cache = ImageCache::new();
        image_cache
            .adopt_painted_frame(Box::new(carry_first_painted_frame(
                &expected_render_snapshot,
                &build_wire_frame(&expected_render_snapshot),
            )))
            .expect("the placement frame reads");
        image_cache
            .start_image_transfer(build_image_transfer(1))
            .expect("the first transfer starts");

        assert_eq!(
            image_cache.accept_image_chunk(&malformed_chunk_frame),
            Err(expected_assembly_error),
            "{case_name}"
        );
        assert_eq!(
            image_cache
                .pending_image_transfer
                .as_ref()
                .map(|pending_image_transfer| pending_image_transfer.received_byte_count),
            None,
            "{case_name}"
        );

        image_cache
            .start_image_transfer(build_image_transfer(1))
            .expect("the same transfer can restart");
        let completed_snapshot = image_cache
            .accept_image_chunk(&build_image_chunk(1))
            .expect("the restarted transfer reads")
            .expect("the restarted transfer produces a redraw");
        assert_eq!(completed_snapshot, expected_render_snapshot, "{case_name}");
    }
}

#[test]
fn a_chunk_of_exactly_the_chunk_limit_is_taken() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &build_wire_frame(&expected_render_snapshot),
        )))
        .expect("the placement frame reads");
    let mut image_transfer = build_image_transfer(1);
    image_transfer.image_record.pixel_width = (MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT / 4 + 2) as u32;
    image_transfer.image_byte_count = MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT as u64 + 8;
    image_cache
        .start_image_transfer(image_transfer)
        .expect("the transfer starts");

    assert_eq!(
        image_cache.accept_image_chunk(&build_image_chunk_frame(
            1,
            0,
            false,
            &vec![0; MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT]
        )),
        Ok(None)
    );
    assert_eq!(
        image_cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image_transfer| pending_image_transfer.received_byte_count),
        Some(MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT as u64)
    );
}

#[test]
fn one_pane_cannot_repeat_a_terminal_image_placement_identity() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut repeated_image_placement =
        painted_frame.pane_snapshots[0].image_placement_snapshots[0].clone();
    repeated_image_placement.image_content_id = 2;
    painted_frame.pane_snapshots[0]
        .image_placement_snapshots
        .push(repeated_image_placement);
    let mut image_cache = ImageCache::new();

    let image_assembly_error = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect_err("the repeated placement identity is refused");

    assert_eq!(image_assembly_error, ImageAssemblyError::DuplicatePlacement);
    assert_eq!(image_cache.image_record_by_content_id.len(), 0);
    assert_eq!(image_cache.retained_image_byte_count, 0);
    assert_eq!(image_cache.painted_frame, None);
    assert_eq!(image_cache.missing_painted_image_content_ids.len(), 0);
    assert_eq!(
        image_cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image_transfer| pending_image_transfer.received_byte_count),
        None
    );
}

#[test]
fn an_image_cache_reset_discards_complete_and_incomplete_connection_state() {
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let sent_placement_preview = build_test_placement_preview(&expected_render_snapshot);
    expected_render_snapshot.placement_preview = Some(Arc::new(sent_placement_preview.clone()));
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &build_wire_frame(&expected_render_snapshot),
        )))
        .expect("the placement frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");
    assert_eq!(
        image_cache
            .placement_preview
            .as_ref()
            .map(|held_placement_preview| held_placement_preview.placement_preview.as_ref()),
        Some(&sent_placement_preview)
    );

    image_cache.clear_image_cache();

    assert_eq!(image_cache.image_record_by_content_id.len(), 0);
    assert_eq!(image_cache.retained_image_byte_count, 0);
    assert_eq!(image_cache.painted_frame, None);
    assert!(image_cache.placement_preview.is_none());
    assert_eq!(image_cache.missing_painted_image_content_ids.len(), 0);
    assert_eq!(
        image_cache
            .pending_image_transfer
            .as_ref()
            .map(|pending_image_transfer| pending_image_transfer.received_byte_count),
        None
    );
    assert_eq!(
        image_cache.start_image_transfer(build_image_transfer(1)),
        Ok(())
    );
}

#[test]
fn image_transfer_metadata_cannot_reserve_more_than_the_frame_limit() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the first placement frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the first transfer starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the first image reads")
        .expect("the first image produces a redraw");
    painted_frame.pane_snapshots[0]
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
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
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

    let image_assembly_error = image_cache
        .start_image_transfer(oversized_image_transfer)
        .expect_err("the retained and incoming records are over the limit");
    assert_eq!(
        image_assembly_error,
        ImageAssemblyError::TransferBytesExceedFrame
    );
}

#[test]
fn image_placements_from_several_panes_do_not_share_one_pane_limit() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    painted_frame.pane_snapshots[0].image_placement_snapshots = (0..MAX_FRAME_IMAGE_TRANSFER_COUNT)
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
    painted_frame.pane_snapshots[1]
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
    let mut image_cache = ImageCache::new();

    let rebuilt_snapshot = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
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
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    painted_frame.pane_snapshots[0]
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
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the placement frame reads");
    image_cache.image_transfer_count = MAX_FRAME_IMAGE_TRANSFER_COUNT - 1;
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("transfer 4096 starts");
    image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("transfer 4096 completes");

    let image_assembly_error = image_cache
        .start_image_transfer(build_image_transfer(2))
        .expect_err("transfer 4097 is rejected");

    assert_eq!(
        image_assembly_error,
        ImageAssemblyError::TransferCountExceedsFrame
    );
}

#[test]
fn an_unavailable_placement_does_not_hold_back_an_available_image() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    painted_frame.pane_snapshots[0]
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
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame,
        )))
        .expect("the mixed placement frame reads");
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the transfer starts");

    let rebuilt_snapshot = image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the available image completes")
        .expect("the available image produces a redraw");
    let image_transfer = build_image_transfer(1);
    let expected_image_record =
        build_image_record(&image_transfer.image_record, TEST_IMAGE_RGBA_BYTES.to_vec());

    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_image_record(),
        Some(&expected_image_record)
    );
    assert_eq!(
        rebuilt_snapshot.pane_snapshots[0].image_placement_snapshots[1].get_image_record(),
        None
    );
    assert!(image_cache.missing_painted_image_content_ids.is_empty());
}

#[test]
fn a_record_no_frame_names_yet_is_kept_and_the_next_frame_naming_it_paints_at_once() {
    // The transfer of record 9 fills no frame. The record is kept, and the
    // next frame that shows record 9 paints at once.
    let image_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let empty_render_snapshot = build_two_pane_render_snapshot(vec![
        PaneSnapshot::from_empty_pane(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut image_cache = ImageCache::new();
    image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &empty_render_snapshot,
            &build_wire_frame(&empty_render_snapshot),
        )))
        .expect("the base frame reads");

    image_cache
        .start_image_transfer(build_image_transfer(9))
        .expect("the transfer of record 9 starts");
    assert_eq!(
        image_cache
            .accept_image_chunk(&build_image_chunk(9))
            .expect("the transfer of record 9 reads"),
        None
    );
    assert_eq!(
        image_cache
            .image_record_by_content_id
            .keys()
            .collect::<Vec<_>>(),
        vec![&9]
    );
    assert_eq!(image_cache.retained_image_byte_count, 8);

    let mut painted_frame = build_wire_frame(&image_render_snapshot);
    painted_frame.pane_snapshots[0].image_placement_snapshots[0].image_content_id = 9;
    let render_snapshot = image_cache
        .adopt_painted_frame(Box::new(carry_first_painted_frame(
            &image_render_snapshot,
            &painted_frame,
        )))
        .expect("the frame naming record 9 reads")
        .expect("the frame naming record 9 paints at once");

    assert_eq!(
        render_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_image_content_id(),
        9
    );
    assert_eq!(
        render_snapshot.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .map(|image_record| image_record.image.rgba_bytes.as_slice()),
        Some(TEST_IMAGE_RGBA_BYTES.as_slice())
    );
}

#[test]
fn a_frame_whose_last_record_arrives_is_returned_with_its_placement_preview() {
    let mut image_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot_with_image(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    image_render_snapshot.placement_preview = Some(Arc::new(build_test_placement_preview(
        &image_render_snapshot,
    )));
    let mut image_cache = ImageCache::new();
    assert_eq!(
        image_cache
            .adopt_painted_frame(Box::new(carry_first_painted_frame(
                &image_render_snapshot,
                &build_wire_frame(&image_render_snapshot),
            )))
            .expect("the frame reads"),
        None
    );
    image_cache
        .start_image_transfer(build_image_transfer(1))
        .expect("the frame's transfer starts");

    let completed_render_snapshot = image_cache
        .accept_image_chunk(&build_image_chunk(1))
        .expect("the frame's record reads");

    assert_eq!(completed_render_snapshot, Some(image_render_snapshot));
}

#[test]
fn the_tabs_gap_arrives_with_the_frame() {
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut painted_frame = build_wire_frame(&expected_render_snapshot);
    painted_frame
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count = 2;

    assert_eq!(
        build_render_snapshot(&carry_first_painted_frame(
            &expected_render_snapshot,
            &painted_frame
        ))
        .session_snapshot
        .active_tab_snapshot
        .gap_cell_count,
        2
    );
}

#[test]
fn a_soft_wrapped_row_arrives_still_soft_wrapped() {
    // Row 0 ends `RowEnd::Soft`, row 1 ends `RowEnd::SoftWide`, and row 2 ends
    // `RowEnd::Hard`. The frame carries each row's end.
    let mut terminal_grid = Grid::build_blank(3, 1, Style::default());
    for (row_index, character) in (0..).zip(['a', 'b', 'c']) {
        *terminal_grid
            .get_cell_mut(row_index, 0)
            .expect("the cell is in the grid") =
            Cell::from_character(character, 1, Style::default());
    }
    terminal_grid.set_row_end(0, RowEnd::Soft);
    terminal_grid.set_row_end(1, RowEnd::SoftWide);

    let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
    content_pane_snapshot.terminal_grid_view = Some(GridView::from_grid(&terminal_grid, 0));
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        content_pane_snapshot,
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

    let received_grid_view = received_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(
        received_grid_view.row_snapshots[0]
            .get_row_metadata()
            .row_end,
        RowEnd::Soft
    );
    assert_eq!(
        received_grid_view.row_snapshots[1]
            .get_row_metadata()
            .row_end,
        RowEnd::SoftWide
    );
    assert_eq!(
        received_grid_view.row_snapshots[2]
            .get_row_metadata()
            .row_end,
        RowEnd::Hard
    );
    assert_eq!(received_snapshot, expected_render_snapshot);
}

#[test]
fn a_highlight_scrolled_entirely_off_screen_arrives_as_no_highlight() {
    // A highlight above every visible row arrives with `selection_spans` set to
    // `None` and `has_selection` set to `true`.
    let mut off_screen_pane_snapshot = build_content_pane_snapshot(PaneId::new());
    off_screen_pane_snapshot.selection_spans = None;
    off_screen_pane_snapshot.has_selection = true;
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        off_screen_pane_snapshot,
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

    assert_eq!(received_snapshot.pane_snapshots[0].selection_spans, None);
    assert!(received_snapshot.pane_snapshots[0].has_selection);
    assert_eq!(received_snapshot, expected_render_snapshot);
}

/// Applying a frame sets this viewer's lock mode and mouse-select state from the
/// frame. A [`MouseFrame`] built from the same frame carries the frame's active
/// tab and focused pane.
#[test]
fn adopting_a_frame_takes_the_viewer_state_the_session_decided() {
    let mut client = crate::Client::from_client_id_and_viewport_size(
        ClientId::new(),
        Size {
            column_count: 20,
            row_count: 6,
        },
        koshi_observability::cleanup::TerminalCleanupGuard::new(),
    );
    // A fresh viewer is unlocked with mouse-select off. The frame carries both
    // the other way.
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());

    let render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let focused_pane_id = render_snapshot.client_snapshot.focused_pane_id;

    client.apply_render_snapshot(&render_snapshot);

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(client.is_mouse_selection_enabled());

    let committed_regions = koshi_renderer::snapshot::CommittedRegions::build_core(
        render_snapshot.client_snapshot.viewport_size,
        0,
    );
    let mouse_frame = koshi_renderer::snapshot::MouseFrame::from_snapshot(
        &render_snapshot,
        committed_regions,
        koshi_renderer::snapshot::ViewerChrome::default(),
    );
    assert_eq!(mouse_frame.client_snapshot.active_tab_id, active_tab_id);
    assert_eq!(mouse_frame.client_snapshot.focused_pane_id, focused_pane_id);
}

/// Each underline style reads back as itself. The cell also carries
/// `Color::Default` as its foreground, the one color whose wire spelling names
/// no value.
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
        let mut terminal_grid = Grid::build_blank(1, 1, Style::default());
        *terminal_grid
            .get_cell_mut(0, 0)
            .expect("the cell is in the grid") = Cell::from_character('x', 1, cell_style);
        let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
        content_pane_snapshot.terminal_grid_view = Some(GridView::from_grid(&terminal_grid, 0));
        let expected_render_snapshot = build_two_pane_render_snapshot(vec![
            content_pane_snapshot,
            PaneSnapshot::from_empty_pane(PaneId::new()),
        ]);

        let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

        let received_grid_view = received_snapshot.pane_snapshots[0]
            .terminal_grid_view
            .as_ref()
            .expect("the pane carries a grid");
        let received_cell = received_grid_view
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
        let expected_render_snapshot = build_two_pane_render_snapshot(vec![
            content_pane_snapshot,
            PaneSnapshot::from_empty_pane(PaneId::new()),
        ]);

        let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

        assert_eq!(
            received_snapshot.pane_snapshots[0].cursor_snapshot.shape,
            cursor_shape
        );
        assert_eq!(received_snapshot, expected_render_snapshot);
    }
}

/// A blank 80-column row reads back as 80 cells.
#[test]
fn a_blank_eighty_column_row_reads_back_as_eighty_cells() {
    let mut content_pane_snapshot = build_content_pane_snapshot(PaneId::new());
    content_pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
        &Grid::build_blank(1, 80, Style::default()),
        0,
    ));
    let expected_render_snapshot = build_two_pane_render_snapshot(vec![
        content_pane_snapshot,
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);
    let received_grid_view = received_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(received_grid_view.get_view_dimensions(), (1, 80));
    assert_eq!(
        received_grid_view.row_snapshots[0].list_row_cells().len(),
        80
    );
    assert_eq!(received_snapshot, expected_render_snapshot);
}

/// A frame whose panes all closed carries no panes, and reads back carrying
/// none.
#[test]
fn a_frame_carrying_no_panes_reads_back_with_no_panes() {
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    expected_render_snapshot.pane_snapshots = Vec::new();

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

    assert_eq!(received_snapshot.pane_snapshots, Vec::<PaneSnapshot>::new());
    assert_eq!(received_snapshot, expected_render_snapshot);
}

#[test]
fn every_name_the_answering_session_chose_reads_back_filtered() {
    // Each name reads back with its control characters removed: `BEL`, `ESC`,
    // the 8-bit `CSI` (U+009B), and the right-to-left override (U+202E).
    let focused_pane_id = PaneId::new();
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(focused_pane_id),
        PaneSnapshot::from_empty_pane(PaneId::new()),
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
    let mut placement_snapshot = crate::tests::build_test_placement_snapshot(
        focused_pane_id,
        expected_render_snapshot.client_snapshot.active_tab_id,
        TabId::new(),
    );
    crate::tests::get_source_tab_snapshot_mut(&mut placement_snapshot)
        .tab_snapshot
        .tab_name = String::from("src\u{1b}[2J");
    placement_snapshot
        .destination_tab_snapshot
        .as_mut()
        .expect("the test preview has a destination tab")
        .tab_snapshot
        .tab_name = String::from("dst\u{7}");
    expected_render_snapshot.placement_preview = Some(Arc::new(PlacementPreview::Available(
        Box::new(placement_snapshot),
    )));

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

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
    let received_placement_snapshot = received_snapshot
        .placement_preview
        .as_deref()
        .and_then(PlacementPreview::find_placement_snapshot)
        .expect("the preview is available");
    assert_eq!(
        received_placement_snapshot
            .find_source_tab_snapshot()
            .map(|source_tab_snapshot| source_tab_snapshot.tab_snapshot.tab_name.as_str()),
        Some("src[2J")
    );
    assert_eq!(
        received_placement_snapshot
            .destination_tab_snapshot
            .as_ref()
            .map(|destination_tab_snapshot| destination_tab_snapshot
                .tab_snapshot
                .tab_name
                .as_str()),
        Some("dst")
    );
}

#[test]
fn a_name_past_the_reported_text_cap_reads_back_cut_to_it() {
    let reported_text_byte_limit = koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT;
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(PaneId::new()),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    expected_render_snapshot.session_snapshot.session_name =
        "a".repeat(reported_text_byte_limit + 1);

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

    assert_eq!(
        received_snapshot.session_snapshot.session_name,
        "a".repeat(reported_text_byte_limit)
    );
}

#[test]
fn a_pane_cell_holding_a_control_character_reads_back_holding_it() {
    // A grid cell keeps its control character. The frame does not filter cell
    // text.
    let content_pane_id = PaneId::new();
    let mut expected_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(content_pane_id),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    let mut terminal_grid = build_test_grid();
    *terminal_grid
        .get_cell_mut(1, 0)
        .expect("the grid has a cell at (1, 0)") =
        Cell::from_character('\u{1b}', 1, Style::default());
    expected_render_snapshot.pane_snapshots[0].terminal_grid_view =
        Some(GridView::from_grid(&terminal_grid, 0));

    let received_snapshot = build_received_render_snapshot(&expected_render_snapshot);

    let received_grid_view = received_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(
        received_grid_view
            .get_cell(1, 0)
            .expect("the cell survives")
            .get_character(),
        '\u{1b}'
    );
}

#[test]
fn a_typed_character_lists_only_its_row_after_the_painted_updates_travel() {
    let content_pane_id = PaneId::new();
    let mut terminal_grid = build_test_grid();
    let mut first_render_snapshot = build_two_pane_render_snapshot(vec![
        build_content_pane_snapshot(content_pane_id),
        PaneSnapshot::from_empty_pane(PaneId::new()),
    ]);
    first_render_snapshot.pane_snapshots[0].terminal_grid_view =
        Some(GridView::from_grid(&terminal_grid, 7));
    *terminal_grid
        .get_cell_mut(1, 3)
        .expect("the grid has a cell at (1, 3)") = Cell::from_character('x', 1, build_test_style());
    let mut second_render_snapshot = first_render_snapshot.clone();
    second_render_snapshot.pane_snapshots[0].terminal_grid_view =
        Some(GridView::from_grid(&terminal_grid, 7));
    let mut painted_update_sender = PaintedUpdateSender::new();
    let mut painted_update_receiver = PaintedUpdateReceiver::new();
    let mut carry_render_snapshot = |render_snapshot: &RenderSnapshot| {
        build_render_snapshot(&carry_painted_update(
            &mut painted_update_sender,
            &mut painted_update_receiver,
            render_snapshot,
            &build_wire_frame(render_snapshot),
        ))
    };
    let first_received_render_snapshot = carry_render_snapshot(&first_render_snapshot);
    let second_received_render_snapshot = carry_render_snapshot(&second_render_snapshot);
    let viewport_size = second_received_render_snapshot
        .client_snapshot
        .viewport_size;

    // The `12x4` tab centers in the `20x4` pane area at `(4, 1)`, so grid row
    // 1 lands on buffer row 3.
    assert_eq!(
        koshi_renderer::list_changed_screen_rows(
            &first_received_render_snapshot,
            &second_received_render_snapshot,
            &koshi_renderer::snapshot::CommittedRegions::build_core(viewport_size, 0),
            koshi_renderer::snapshot::ViewerChrome::default(),
            ratatui::layout::Rect::new(0, 0, viewport_size.column_count, viewport_size.row_count),
        ),
        Some(vec![3])
    );
}
