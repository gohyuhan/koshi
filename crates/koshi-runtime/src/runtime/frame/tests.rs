//! Tests for [`build_wire_frame`] and the placement preview builder: the
//! session, tab, slot, floating pane slot, client and per-pane scalars come
//! across unchanged; an image placement and its record travel apart; a large
//! image's frames stay under the frame cap; and a placement preview travels
//! field for field.

use std::sync::Arc;

use koshi_core::geometry::SplitDirection;
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT;
use koshi_ipc::image_chunk::build_image_chunk_frame_head;
use koshi_ipc::transport::MAX_FRAME_BYTE_COUNT;
use koshi_layout::mode::LayoutMode;
use koshi_layout::placement::PlacementTabSizes;
use koshi_layout::solver::{solve_layout_with_sizing, PaneSizing, StackHeader};
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_renderer::snapshot::{
    list_pane_slots, ClientSnapshot, CursorSnapshot, FloatingPane, FloatingPaneSlot, GridView,
    ImagePlacementSnapshot, PaneSlot, PaneSnapshot, PlacementSnapshot, RenderSnapshot,
    ScrollbackMetadata, SelectionSpans, SessionSnapshot, TabMetadata, TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};
use koshi_terminal::grid::state::{Cell, Grid};
use koshi_terminal::state::CursorShape;
use koshi_terminal::style::{Color, Style, UnderlineStyle};

use super::*;
use crate::runtime::painted_update::PaintedUpdateSender;

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
    test_style.set_blinking(true);
    test_style.set_concealed(true);
    test_style.set_strikethrough(true);
    test_style.set_overlined(true);
    test_style.set_underline(UnderlineStyle::Curly);
    test_style
}

/// A 1×3 grid: a styled `e` carrying a combining acute accent (U+0301), then
/// two blank cells in the default style.
fn build_test_grid() -> Grid {
    let mut test_grid = Grid::build_blank(1, 3, Style::default());
    let written_cell = test_grid
        .get_cell_mut(0, 0)
        .expect("the grid has a cell at (0, 0)");
    *written_cell = Cell::from_character('e', 1, build_test_style());
    written_cell.push_combining('\u{301}');
    test_grid
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

/// One pane holding [`build_test_grid`], scrolled 7 lines back, reporting any-motion mouse
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
        terminal_grid_view: Some(GridView::from_grid(&build_test_grid(), 7)),
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
        scrollback_metadata: ScrollbackMetadata {
            retained_line_count: 500,
        },
    }
}

/// A frame with one tab, two slots, and the two panes above.
pub(crate) fn build_render_snapshot(
    content_pane_id: PaneId,
    empty_pane_id: PaneId,
) -> RenderSnapshot {
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
                            size: Size {
                                column_count: 5,
                                row_count: 3,
                            },
                        },
                        content_rect: Some(Rect {
                            origin: Point { column: 1, row: 1 },
                            size: Size {
                                column_count: 3,
                                row_count: 1,
                            },
                        }),
                        is_visible: true,
                        is_suppressed: false,
                    },
                    PaneSlot {
                        pane_id: empty_pane_id,
                        outer_rect: Rect {
                            origin: Point { column: 5, row: 0 },
                            size: Size {
                                column_count: 5,
                                row_count: 3,
                            },
                        },
                        content_rect: None,
                        is_visible: false,
                        is_suppressed: true,
                    },
                ],
                tab_size: Size {
                    column_count: 10,
                    row_count: 3,
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
                    tab_id,
                    tab_name: String::from("tab"),
                    tab_index: 0,
                    is_active: true,
                },
                TabMetadata {
                    tab_id: other_tab_id,
                    tab_name: String::from("other"),
                    tab_index: 1,
                    is_active: false,
                },
            ],
            floating_pane_slots: Vec::new(),
            floating_panes: Vec::new(),
        },
        pane_snapshots: vec![
            build_pane_snapshot(content_pane_id),
            PaneSnapshot::from_empty_pane(empty_pane_id),
        ],
        client_snapshot: ClientSnapshot {
            client_id,
            viewport_size: Size {
                column_count: 20,
                row_count: 6,
            },
            active_tab_id: tab_id,
            focused_pane_id: Some(content_pane_id),
            active_tab_focused_pane_id: Some(content_pane_id),
            lock_mode: LockMode::Locked,
            last_applied_lock_command_id: Some(CommandId::new()),
            last_applied_floating_pane_move_command_id: Some(CommandId::new()),
            is_mouse_selection_enabled: true,
            last_applied_placement_command_id: None,
        },
        placement_preview: None,
    }
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
    let painted_frame = build_wire_frame(&render_snapshot);

    let part_payloads = PaintedUpdateSender::new()
        .build_update_frames(&render_snapshot, &painted_frame, false)
        .expect("the painted update fits the frame cap")
        .expect("a first update is sent");
    assert_eq!(part_payloads.len(), 1);
    assert!(part_payloads[0].len() <= MAX_FRAME_BYTE_COUNT as usize);

    let image_transfer = build_wire_image_transfer(1, &image_record);
    assert_eq!(image_transfer.image_content_id, 1);
    assert_eq!(image_transfer.image_byte_count, 16 * 1024 * 1024);
    assert!(
        serde_json::to_vec(&SessionEvent::ImageContentStart { image_transfer })
            .expect("the image start encodes")
            .len()
            <= MAX_FRAME_BYTE_COUNT as usize
    );

    let image_chunks: Vec<(u64, bool, usize)> = build_wire_image_chunk_sources(&image_record)
        .map(|(byte_offset, is_last, chunk_bytes)| (byte_offset, is_last, chunk_bytes.len()))
        .collect();
    let expected_image_chunks: Vec<(u64, bool, usize)> = (0..16u64)
        .map(|chunk_index| {
            (
                chunk_index * 1024 * 1024,
                chunk_index == 15,
                MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT,
            )
        })
        .collect();
    assert_eq!(image_chunks, expected_image_chunks);
    for (byte_offset, is_last_chunk, chunk_bytes) in build_wire_image_chunk_sources(&image_record) {
        let chunk_frame_head = build_image_chunk_frame_head(1, byte_offset, is_last_chunk);
        assert!(chunk_frame_head.len() + chunk_bytes.len() <= MAX_FRAME_BYTE_COUNT as usize);
    }
}

#[test]
fn a_placement_preview_travels_field_for_field() {
    let (source_pane_id, other_pane_id) = (PaneId::new(), PaneId::new());
    let tab_id = TabId::new();
    let layout_tree = LayoutNode::Split(SplitNode::with_equal_weights(
        SplitDirection::Horizontal,
        vec![
            LayoutNode::Pane(source_pane_id),
            LayoutNode::Pane(other_pane_id),
        ],
    ));
    let tab_size = Size {
        column_count: 40,
        row_count: 10,
    };
    let pane_sizing = PaneSizing::default();
    let layout_solve = solve_layout_with_sizing(
        &layout_tree,
        Rect::from_size_at_origin(tab_size),
        pane_sizing,
    );
    let tab_snapshot = TabSnapshot {
        tab_id,
        tab_name: String::from("work"),
        pane_slots: list_pane_slots(&layout_solve),
        tab_size,
        stack_headers: layout_solve.stack_headers,
        layout_mode: LayoutMode::Tiled,
        is_every_pane_suppressed: layout_solve.is_every_pane_suppressed,
        gap_cell_count: pane_sizing.gap_cell_count,
    };
    let insertion_tab_sizes = PlacementTabSizes {
        destination_tab_size: Some(tab_size),
        source_tab_size: None,
    };
    let placement_preview = PlacementPreview::Available(Box::new(PlacementSnapshot {
        source_pane_id,
        source: PlacementSource::Tiled(PlacementTabSnapshot {
            layout_tree: layout_tree.clone(),
            focus_history: vec![source_pane_id],
            tab_snapshot: tab_snapshot.clone(),
        }),
        destination_tab_id: tab_id,
        destination_tab_snapshot: None,
        pane_sizing,
        insertion_tab_sizes,
        swap_tab_sizes: Vec::new(),
    }));

    let wire_placement_preview = build_wire_placement_preview(&placement_preview);

    let expected_wire_placement_snapshot = WirePlacementSnapshot {
        source_pane_id,
        source: WirePlacementSource::Tiled(WirePlacementTabSnapshot {
            layout_tree,
            focus_history: vec![source_pane_id],
            tab_snapshot: build_wire_tab(&tab_snapshot),
        }),
        destination_tab_id: tab_id,
        destination_tab_snapshot: None,
        pane_sizing: WirePlacementSizing::from(pane_sizing),
        insertion_tab_sizes,
        swap_tab_sizes: Vec::new(),
    };
    assert_eq!(expected_wire_placement_snapshot.validate(), Ok(()));
    assert_eq!(
        wire_placement_preview,
        WirePlacementPreview::Available(Box::new(expected_wire_placement_snapshot))
    );
    assert_eq!(
        build_wire_placement_preview(&PlacementPreview::Unavailable {
            source_pane_id,
            destination_tab_id: tab_id,
        }),
        WirePlacementPreview::Unavailable {
            source_pane_id,
            destination_tab_id: tab_id,
        }
    );
}

#[test]
fn a_pane_with_no_grid_travels_with_its_remaining_fields_at_rest() {
    let empty_pane_id = PaneId::new();
    let painted_frame = build_wire_frame(&build_render_snapshot(PaneId::new(), empty_pane_id));

    let pane_snapshot = &painted_frame.pane_snapshots[1];
    assert_eq!(pane_snapshot.pane_id, empty_pane_id);
    assert_eq!(pane_snapshot.pane_title, None);
    assert_eq!(pane_snapshot.image_placement_snapshots, Vec::new());
    assert_eq!(pane_snapshot.mouse_tracking, MouseTracking::Off);
    assert!(!pane_snapshot.is_reverse_video);
    assert!(!pane_snapshot.is_alternate_scroll_enabled);
    assert!(!pane_snapshot.is_on_alt_screen);
    assert_eq!(pane_snapshot.selection_spans, None);
    assert!(!pane_snapshot.has_selection);
}

#[test]
fn the_title_mode_and_selection_scalars_come_through_unchanged() {
    let source_pane_id = PaneId::new();
    let painted_frame = build_wire_frame(&build_render_snapshot(source_pane_id, PaneId::new()));

    let pane_snapshot = &painted_frame.pane_snapshots[0];
    assert_eq!(pane_snapshot.pane_id, source_pane_id);
    assert_eq!(pane_snapshot.mouse_tracking, MouseTracking::AnyMotion);
    assert_eq!(pane_snapshot.pane_title, Some(String::from("~/work")));
    assert!(pane_snapshot.is_reverse_video);
    assert!(pane_snapshot.is_alternate_scroll_enabled);
    assert!(!pane_snapshot.is_on_alt_screen);
    assert!(pane_snapshot.has_selection);
    assert_eq!(
        pane_snapshot.selection_spans,
        Some(FrameSelection {
            row_spans: vec![(0, 1, 2)],
        })
    );
}

#[test]
fn an_image_placement_and_its_record_travel_in_separate_values() {
    let source_pane_id = PaneId::new();
    let source_render_snapshot = build_render_snapshot(source_pane_id, PaneId::new());
    let image_record = source_render_snapshot.pane_snapshots[0].image_placement_snapshots[0]
        .clone_image_record()
        .expect("the source placement has image content");
    let painted_frame = build_wire_frame(&source_render_snapshot);
    let image_placement = &painted_frame.pane_snapshots[0].image_placement_snapshots[0];
    let image_transfer = build_wire_image_transfer(image_placement.image_content_id, &image_record);

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
fn floating_pane_slots_copy_straight_across_back_to_front() {
    let mut render_snapshot = build_render_snapshot(PaneId::new(), PaneId::new());
    let (lower_floating_pane_id, upper_floating_pane_id) = (PaneId::new(), PaneId::new());
    let lower_outer_rect = Rect {
        origin: Point { column: 2, row: 1 },
        size: Size {
            column_count: 8,
            row_count: 5,
        },
    };
    let upper_outer_rect = Rect {
        origin: Point { column: 6, row: 0 },
        size: Size {
            column_count: 6,
            row_count: 6,
        },
    };
    render_snapshot.session_snapshot.floating_pane_slots = vec![
        FloatingPaneSlot {
            pane_id: lower_floating_pane_id,
            outer_rect: lower_outer_rect,
            is_pinned: false,
        },
        FloatingPaneSlot {
            pane_id: upper_floating_pane_id,
            outer_rect: upper_outer_rect,
            is_pinned: true,
        },
    ];
    let minimized_floating_pane_id = PaneId::new();
    let untitled_minimized_floating_pane_id = PaneId::new();
    render_snapshot.session_snapshot.floating_panes = vec![
        FloatingPane::Shown {
            pane_id: upper_floating_pane_id,
        },
        FloatingPane::Minimized {
            pane_id: minimized_floating_pane_id,
            pane_title: Some("htop".to_string()),
        },
        FloatingPane::Shown {
            pane_id: lower_floating_pane_id,
        },
        FloatingPane::Minimized {
            pane_id: untitled_minimized_floating_pane_id,
            pane_title: None,
        },
    ];

    let painted_frame = build_wire_frame(&render_snapshot);

    assert_eq!(
        painted_frame.session_snapshot.floating_pane_slots,
        [
            FrameFloatingPaneSlot {
                pane_id: lower_floating_pane_id,
                outer_rect: lower_outer_rect,
                is_pinned: false,
            },
            FrameFloatingPaneSlot {
                pane_id: upper_floating_pane_id,
                outer_rect: upper_outer_rect,
                is_pinned: true,
            },
        ]
    );
    assert_eq!(
        painted_frame.session_snapshot.floating_panes,
        [
            FrameFloatingPane::Shown {
                pane_id: upper_floating_pane_id,
            },
            FrameFloatingPane::Minimized {
                pane_id: minimized_floating_pane_id,
                pane_title: Some("htop".to_string()),
            },
            FrameFloatingPane::Shown {
                pane_id: lower_floating_pane_id,
            },
            FrameFloatingPane::Minimized {
                pane_id: untitled_minimized_floating_pane_id,
                pane_title: None,
            },
        ]
    );
}

#[test]
fn the_session_tab_slot_and_client_fields_copy_straight_across() {
    let focused_pane_id = PaneId::new();
    let empty_pane_id = PaneId::new();
    let mut render_snapshot = build_render_snapshot(focused_pane_id, empty_pane_id);
    render_snapshot.client_snapshot.active_tab_focused_pane_id = Some(empty_pane_id);
    render_snapshot
        .client_snapshot
        .last_applied_placement_command_id = Some(CommandId::new());

    let painted_frame = build_wire_frame(&render_snapshot);

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
        painted_frame.session_snapshot.active_tab_snapshot.tab_size,
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
                    size: Size {
                        column_count: 5,
                        row_count: 3
                    },
                },
                content_rect: Some(Rect {
                    origin: Point { column: 1, row: 1 },
                    size: Size {
                        column_count: 3,
                        row_count: 1
                    },
                }),
                is_visible: true,
                is_suppressed: false,
            },
            FrameSlot {
                pane_id: empty_pane_id,
                outer_rect: Rect {
                    origin: Point { column: 5, row: 0 },
                    size: Size {
                        column_count: 5,
                        row_count: 3
                    },
                },
                content_rect: None,
                is_visible: false,
                is_suppressed: true,
            },
        ]
    );
    assert_eq!(
        painted_frame.session_snapshot.tab_snapshots,
        vec![
            FrameTabMetadata {
                tab_id: render_snapshot.session_snapshot.active_tab_snapshot.tab_id,
                tab_name: String::from("tab"),
                tab_index: 0,
                is_active: true,
            },
            FrameTabMetadata {
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
            active_tab_focused_pane_id: Some(empty_pane_id),
            lock_mode: LockMode::Locked,
            last_applied_lock_command_id: render_snapshot
                .client_snapshot
                .last_applied_lock_command_id,
            last_applied_floating_pane_move_command_id: render_snapshot
                .client_snapshot
                .last_applied_floating_pane_move_command_id,
            last_applied_placement_command_id: render_snapshot
                .client_snapshot
                .last_applied_placement_command_id,
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

    let painted_frame = build_wire_frame(&render_snapshot);

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
            size: Size {
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
        .is_every_pane_suppressed = true;

    let painted_frame = build_wire_frame(&render_snapshot);

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
