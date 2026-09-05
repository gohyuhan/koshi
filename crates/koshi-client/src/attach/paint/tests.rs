//! Tests for [`to_snapshot`]: a frame that travels and is read back is the
//! frame that was sent — every style field, a wide glyph and its continuation
//! half, a combining mark, both cursor states, the scroll offset, the mouse
//! mode, a pane with no grid at all, and the highlight rows.

use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_ipc::frame::{
    FrameGraphicsProtocol, FrameImageAction, FrameImageChunk, FrameImageDisplay,
    FrameImagePlacement, FrameImageRecordHeader, FrameImageTransfer, MAX_FRAME_IMAGE_TRANSFERS,
    MAX_FRAME_IMAGE_TRANSFER_BYTES,
};
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::PaneKind;
use koshi_runtime::runtime::frame::wire_frame;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord,
    SixelBackground,
};

use super::*;

fn image_placement() -> ImagePlacementSnapshot {
    ImagePlacementSnapshot::with_content_id(
        41,
        1,
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
                z_index: -2,
                cell_columns: Some(2),
                cell_rows: Some(1),
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

/// The transfer metadata for [`image_placement`].
fn image_transfer(id: u64) -> FrameImageTransfer {
    FrameImageTransfer {
        id,
        record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            width: 2,
            height: 1,
            action: FrameImageAction::TransmitAndDisplay,
            display: FrameImageDisplay {
                width: Some(FrameImageDimension::Cells(3)),
                height: Some(FrameImageDimension::Pixels(1)),
                preserve_aspect_ratio: false,
                sixel_background: Some(FrameSixelBackground::Preserve),
                image_id: Some(7),
                image_number: Some(8),
                placement_id: Some(9),
                usage_hints: 0x12,
                unicode_placeholder: true,
                z_index: -2,
                cell_columns: Some(2),
                cell_rows: Some(1),
                source_offset_x: Some(1),
                source_offset_y: Some(0),
                cell_offset_x: Some(6),
                cell_offset_y: Some(7),
                move_cursor: false,
            },
            anchor: (0, 2),
        },
        byte_len: 8,
    }
}

/// All RGBA bytes for [`image_transfer`] in one final chunk.
fn image_chunk(id: u64) -> FrameImageChunk {
    FrameImageChunk {
        transfer_id: id,
        offset: 0,
        last: true,
        bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
    }
}

/// Every style field set away from its default, so a field lost on the way
/// there or back shows up.
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

/// A 2×4 grid whose first row holds, left to right: a styled `e` carrying a
/// combining acute accent (U+0301), the wide glyph `漢` at width 2, the blank
/// continuation half it occupies at width 0, and one default blank. The second
/// row is all default blanks.
fn grid() -> Grid {
    let mut grid = Grid::blank(2, 4, Style::default());
    let accented = grid.cell_mut(0, 0).expect("the grid has a cell at (0, 0)");
    *accented = Cell::new('e', 1, style());
    accented.push_combining('\u{301}');
    *grid.cell_mut(0, 1).expect("the grid has a cell at (0, 1)") = Cell::new('漢', 2, style());
    *grid.cell_mut(0, 2).expect("the grid has a cell at (0, 2)") = Cell::new(' ', 0, style());
    grid
}

/// The pane holding [`grid`]: scrolled 7 lines back, reporting any-motion mouse
/// tracking, showing a shaped blinking cursor, and highlighting the first row's
/// columns 1 to 2.
fn content_pane(id: PaneId) -> PaneSnapshot {
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
        image_placements: Vec::new(),
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

/// The content pane with one image placement included.
fn content_pane_with_image(id: PaneId) -> PaneSnapshot {
    let mut pane = content_pane(id);
    pane.image_placements.push(image_placement());
    pane
}

/// A pane with no terminal content: no grid, a hidden and unshaped cursor,
/// nothing highlighted.
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

/// A frame with one tab, two slots, and the two panes handed in.
fn snapshot(panes: Vec<PaneSnapshot>) -> RenderSnapshot {
    let content = panes[0].id;
    let empty = panes[1].id;
    let tab = TabId::new();
    let other_tab = TabId::new();
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
                            size: Size { cols: 6, rows: 4 },
                        },
                        inner_rect: Some(Rect {
                            origin: Point { x: 1, y: 1 },
                            size: Size { cols: 4, rows: 2 },
                        }),
                        kind: PaneKind::Terminal,
                        visible: true,
                        suppressed: false,
                        dead: false,
                    },
                    PaneSlot {
                        pane_id: empty,
                        rect: Rect {
                            origin: Point { x: 6, y: 0 },
                            size: Size { cols: 6, rows: 4 },
                        },
                        inner_rect: None,
                        kind: PaneKind::Terminal,
                        visible: false,
                        suppressed: true,
                        dead: true,
                    },
                ],
                effective_size: Size { cols: 12, rows: 4 },
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
        panes,
        client: ClientSnapshot {
            id: ClientId::new(),
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
fn a_frame_that_travels_and_is_read_back_is_the_frame_that_was_sent() {
    let sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    assert_eq!(to_snapshot(&wire_frame(&sent)), sent);
}

#[test]
fn a_frame_draws_an_image_placeholder_then_draws_the_complete_record() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let wire = wire_frame(&sent);
    let mut cache = ImageCache::new();

    let placeholder = cache
        .begin_frame(Box::new(wire))
        .expect("the placement frame reads");
    assert_eq!(placeholder.panes[0].image_placements.len(), 1);
    assert_eq!(placeholder.panes[0].image_placements[0].record(), None);

    cache.start(image_transfer(1)).expect("the transfer starts");
    let received = cache
        .accept(image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the last missing record produces a redraw");

    assert_eq!(received, sent);
    assert_eq!(
        received.panes[0].image_placements[0]
            .record()
            .expect("the complete image has its record")
            .image
            .rgba,
        vec![255, 0, 0, 255, 0, 255, 0, 255]
    );
    assert_eq!(
        received.panes[0].image_placements[0]
            .record()
            .expect("the complete image has its record")
            .display
            .source_offset_y,
        Some(0)
    );
}

#[test]
fn a_complete_cached_image_is_reused_by_the_next_frame() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let wire = wire_frame(&sent);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire.clone()))
        .expect("the first placement frame reads");
    cache.start(image_transfer(1)).expect("the transfer starts");
    cache
        .accept(image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the image produces a redraw");

    let reused = cache
        .begin_frame(Box::new(wire))
        .expect("the repeated placement frame reads");

    assert_eq!(reused, sent);
    assert_eq!(cache.images.len(), 1);
    assert!(cache.missing.is_empty());
    assert_eq!(cache.pending.as_ref().map(|pending| pending.received), None);
}

#[test]
fn a_cached_record_cannot_hide_an_invalid_new_placement() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire.clone()))
        .expect("the valid frame reads");
    cache.start(image_transfer(1)).expect("the transfer starts");
    cache
        .accept(image_chunk(1))
        .expect("the complete chunk reads")
        .expect("the image produces a redraw");
    wire.panes[0].image_placements[0].anchor = (u16::MAX, 0);
    wire.panes[0].image_placements[0].rows = 2;

    let error = cache
        .begin_frame(Box::new(wire))
        .expect_err("the cached record cannot make bad geometry valid");

    assert_eq!(error, ImageAssemblyError::InvalidPlacement);
    assert_eq!(cache.images.len(), 1);
    assert_eq!(cache.retained_bytes, 8);
}

#[test]
fn a_frame_redraws_only_after_every_missing_image_record_arrives() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    wire.panes[0].image_placements.push(FrameImagePlacement {
        id: 42,
        content_id: 2,
        available: true,
        anchor: (0, 0),
        columns: 1,
        rows: 1,
    });
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire))
        .expect("the placement frame reads");

    cache
        .start(image_transfer(1))
        .expect("the first transfer starts");
    let after_first = cache
        .accept(image_chunk(1))
        .expect("the first complete record reads");
    cache
        .start(image_transfer(2))
        .expect("the second transfer starts");
    let after_second = cache
        .accept(image_chunk(2))
        .expect("the second complete record reads")
        .expect("the last record produces one redraw");

    assert_eq!(after_first, None);
    assert_eq!(after_second.panes[0].image_placements.len(), 2);
    assert_eq!(after_second.panes[0].image_placements[0].content_id(), 1);
    assert_eq!(
        after_second.panes[0].image_placements[0]
            .record()
            .map(|record| record.image.rgba.as_slice()),
        Some([255, 0, 0, 255, 0, 255, 0, 255].as_slice())
    );
    assert_eq!(after_second.panes[0].image_placements[1].content_id(), 2);
    assert_eq!(
        after_second.panes[0].image_placements[1]
            .record()
            .map(|record| record.image.rgba.as_slice()),
        Some([255, 0, 0, 255, 0, 255, 0, 255].as_slice())
    );
}

#[test]
fn a_frame_without_a_cached_placement_releases_its_rgba_bytes() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire.clone()))
        .expect("the placement frame reads");
    cache.start(image_transfer(1)).expect("the transfer starts");
    cache
        .accept(image_chunk(1))
        .expect("the image reads")
        .expect("the image produces a redraw");
    wire.panes[0].image_placements.clear();

    let without_image = cache
        .begin_frame(Box::new(wire))
        .expect("the frame without the placement reads");

    assert_eq!(without_image.panes[0].image_placements, Vec::new());
    assert_eq!(cache.images.len(), 0);
    assert_eq!(cache.retained_bytes, 0);
    assert_eq!(cache.missing.len(), 0);
}

#[test]
fn a_rejected_chunk_closes_its_transfer_and_allows_an_exact_restart() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire_frame(&sent)))
        .expect("the placement frame reads");
    cache
        .start(image_transfer(1))
        .expect("the first transfer starts");
    let error = cache
        .accept(FrameImageChunk {
            transfer_id: 1,
            offset: 1,
            last: true,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
        })
        .expect_err("the wrong offset is refused");

    assert_eq!(
        error,
        ImageAssemblyError::WrongOffset {
            transfer_id: 1,
            expected: 0,
            actual: 1,
        }
    );
    assert_eq!(cache.pending.as_ref().map(|pending| pending.received), None);

    cache
        .start(image_transfer(1))
        .expect("the same transfer can restart");
    let completed = cache
        .accept(image_chunk(1))
        .expect("the restarted transfer reads")
        .expect("the restarted transfer produces a redraw");
    assert_eq!(completed, sent);
}

#[test]
fn one_pane_cannot_repeat_a_terminal_image_placement_identity() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    let mut repeated = wire.panes[0].image_placements[0].clone();
    repeated.content_id = 2;
    wire.panes[0].image_placements.push(repeated);
    let mut cache = ImageCache::new();

    let error = cache
        .begin_frame(Box::new(wire))
        .expect_err("the repeated placement identity is refused");

    assert_eq!(error, ImageAssemblyError::DuplicatePlacement);
    assert_eq!(cache.images.len(), 0);
    assert_eq!(cache.retained_bytes, 0);
    assert_eq!(cache.frame, None);
    assert_eq!(cache.missing.len(), 0);
    assert_eq!(cache.pending.as_ref().map(|pending| pending.received), None);
}

#[test]
fn an_image_cache_reset_discards_complete_and_incomplete_connection_state() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire_frame(&sent)))
        .expect("the placement frame reads");
    cache.start(image_transfer(1)).expect("the transfer starts");

    cache.reset();

    assert_eq!(cache.images.len(), 0);
    assert_eq!(cache.retained_bytes, 0);
    assert_eq!(cache.frame, None);
    assert_eq!(cache.missing.len(), 0);
    assert_eq!(cache.pending.as_ref().map(|pending| pending.received), None);
    assert_eq!(
        cache.start(image_transfer(1)),
        Err(ImageAssemblyError::MissingBaseFrame)
    );
}

#[test]
fn an_image_chunk_with_a_wrong_offset_is_refused_exactly() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let wire = wire_frame(&sent);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire))
        .expect("the placement frame reads");
    cache.start(image_transfer(1)).expect("the transfer starts");
    let error = cache
        .accept(FrameImageChunk {
            transfer_id: 1,
            offset: 1,
            last: true,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 255],
        })
        .expect_err("the first chunk must start at offset zero");

    assert_eq!(
        error,
        ImageAssemblyError::WrongOffset {
            transfer_id: 1,
            expected: 0,
            actual: 1,
        }
    );
}

#[test]
fn image_transfer_metadata_cannot_reserve_more_than_the_frame_limit() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire.clone()))
        .expect("the first placement frame reads");
    cache
        .start(image_transfer(1))
        .expect("the first transfer starts");
    cache
        .accept(image_chunk(1))
        .expect("the first image reads")
        .expect("the first image produces a redraw");
    wire.panes[0].image_placements.push(FrameImagePlacement {
        id: 42,
        content_id: 2,
        available: true,
        anchor: (0, 0),
        columns: 1,
        rows: 1,
    });
    cache
        .begin_frame(Box::new(wire))
        .expect("the two-placement frame reads");
    let oversized = FrameImageTransfer {
        id: 2,
        record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            width: 4_096,
            height: 4_096,
            action: FrameImageAction::Display,
            display: FrameImageDisplay::default(),
            anchor: (0, 0),
        },
        byte_len: MAX_FRAME_IMAGE_TRANSFER_BYTES,
    };

    let error = cache
        .start(oversized)
        .expect_err("the retained and incoming records are over the limit");
    assert_eq!(error, ImageAssemblyError::TransferBytesExceedFrame);
}

#[test]
fn image_placements_from_several_panes_do_not_share_one_pane_limit() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    wire.panes[0].image_placements = (0..MAX_FRAME_IMAGE_TRANSFERS)
        .map(|index| FrameImagePlacement {
            id: u64::try_from(index + 1).expect("the placement identity fits"),
            content_id: u64::try_from(index + 1).expect("the content identity fits"),
            available: false,
            anchor: (0, 0),
            columns: 1,
            rows: 1,
        })
        .collect();
    wire.panes[1].image_placements.push(FrameImagePlacement {
        id: 1,
        content_id: u64::try_from(MAX_FRAME_IMAGE_TRANSFERS + 1)
            .expect("the content identity fits"),
        available: false,
        anchor: (0, 0),
        columns: 1,
        rows: 1,
    });
    let mut cache = ImageCache::new();

    let rebuilt = cache
        .begin_frame(Box::new(wire))
        .expect("placements in distinct panes are accepted");

    assert_eq!(rebuilt.panes[0].image_placements.len(), 4_096);
    assert_eq!(rebuilt.panes[1].image_placements.len(), 1);
}

#[test]
fn one_painted_frame_accepts_at_most_4096_image_transfers() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    wire.panes[0].image_placements.push(FrameImagePlacement {
        id: 42,
        content_id: 2,
        available: true,
        anchor: (0, 0),
        columns: 1,
        rows: 1,
    });
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire))
        .expect("the placement frame reads");
    cache.transfer_count = MAX_FRAME_IMAGE_TRANSFERS - 1;
    cache
        .start(image_transfer(1))
        .expect("transfer 4096 starts");
    cache
        .accept(image_chunk(1))
        .expect("transfer 4096 completes");

    let error = cache
        .start(image_transfer(2))
        .expect_err("transfer 4097 is rejected");

    assert_eq!(error, ImageAssemblyError::TransferCountExceedsFrame);
}

#[test]
fn an_unavailable_placement_does_not_hold_back_an_available_image() {
    let sent = snapshot(vec![
        content_pane_with_image(PaneId::new()),
        empty_pane(PaneId::new()),
    ]);
    let mut wire = wire_frame(&sent);
    wire.panes[0].image_placements.push(FrameImagePlacement {
        id: 42,
        content_id: 2,
        available: false,
        anchor: (0, 0),
        columns: 1,
        rows: 1,
    });
    let mut cache = ImageCache::new();
    cache
        .begin_frame(Box::new(wire))
        .expect("the mixed placement frame reads");
    cache.start(image_transfer(1)).expect("the transfer starts");

    let rebuilt = cache
        .accept(image_chunk(1))
        .expect("the available image completes")
        .expect("the available image produces a redraw");
    let transfer = image_transfer(1);
    let expected = to_image_record(&transfer.record, image_chunk(1).bytes);

    assert_eq!(
        rebuilt.panes[0].image_placements[0].record(),
        Some(&expected)
    );
    assert_eq!(rebuilt.panes[0].image_placements[1].record(), None);
    assert_eq!(
        cache.start(image_transfer(2)),
        Err(ImageAssemblyError::UnknownTransfer(2))
    );
}

#[test]
fn the_tabs_gap_arrives_with_the_frame() {
    let sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    let mut wire = wire_frame(&sent);
    wire.session.active_tab.gap = 2;

    assert_eq!(to_snapshot(&wire).session.active_tab.gap, 2);
}

#[test]
fn a_soft_wrapped_row_arrives_still_soft_wrapped() {
    // A shell printing a line longer than the pane is wide leaves the first row
    // soft-wrapped and the last row hard-ended. Without the row-end state on the
    // wire every row reads as hard, and copying the text out breaks the line at
    // the wrap.
    let mut grid = Grid::from_rows(
        vec![
            vec![Cell::new('a', 1, Style::default())],
            vec![Cell::new('b', 1, Style::default())],
            vec![Cell::new('c', 1, Style::default())],
        ],
        1,
        Style::default(),
    );
    grid.set_row_end(0, RowEnd::Soft);
    grid.set_row_end(1, RowEnd::SoftWide);

    let mut pane = content_pane(PaneId::new());
    pane.grid_view = Some(GridView {
        grid: Arc::new(grid),
        view_offset: 0,
    });
    let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

    let read_back = to_snapshot(&wire_frame(&sent));

    let arrived = read_back.panes[0]
        .grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(arrived.grid.row_end(0), RowEnd::Soft);
    assert_eq!(arrived.grid.row_end(1), RowEnd::SoftWide);
    assert_eq!(arrived.grid.row_end(2), RowEnd::Hard);
    assert_eq!(read_back, sent);
}

#[test]
fn a_highlight_scrolled_entirely_off_screen_arrives_as_no_highlight() {
    // The session resolves the highlight to the rows this frame shows before
    // it builds the pane, so a highlight above every visible row leaves
    // `selection` empty while `has_selection` still reports it exists.
    let mut off_screen = content_pane(PaneId::new());
    off_screen.selection = None;
    off_screen.has_selection = true;
    let sent = snapshot(vec![off_screen, empty_pane(PaneId::new())]);

    let read_back = to_snapshot(&wire_frame(&sent));

    assert_eq!(read_back.panes[0].selection, None);
    assert!(read_back.panes[0].has_selection);
    assert_eq!(read_back, sent);
}

/// The session decides this viewer's lock mode and whether mouse-select is on,
/// so a painted frame is where both are read from, and the same frame cut down
/// to [`MouseFrame`] is what the next mouse event is placed against.
#[test]
fn adopting_a_frame_takes_the_viewer_state_the_session_decided() {
    let (_events_tx, events_rx) = std::sync::mpsc::sync_channel(8);
    let mut client = crate::Client::new(
        ClientId::new(),
        Size { cols: 20, rows: 6 },
        events_rx,
        koshi_observability::cleanup::TerminalCleanupGuard::new(),
    );
    // A fresh viewer is unlocked with mouse-select off. The frame carries both
    // the other way.
    assert_eq!(client.lock_mode(), LockMode::Normal);
    assert!(!client.mouse_select());

    let sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    let active_tab = sent.client.active_tab;
    let focused_pane = sent.client.focused_pane;

    super::super::adopt_frame(&mut client, &sent);

    assert_eq!(client.lock_mode(), LockMode::Locked);
    assert!(client.mouse_select());

    let frame = koshi_renderer::snapshot::MouseFrame::from(sent);
    assert_eq!(frame.client.active_tab, active_tab);
    assert_eq!(frame.client.focused_pane, focused_pane);
}

/// Each underline style has its own wire spelling, so a variant read back as
/// another one shows up here. The cell also carries a default foreground, which
/// is the one color whose wire spelling names no value.
#[test]
fn every_underline_style_reads_back_as_itself() {
    for underline in [
        UnderlineStyle::None,
        UnderlineStyle::Single,
        UnderlineStyle::Double,
        UnderlineStyle::Curly,
        UnderlineStyle::Dotted,
        UnderlineStyle::Dashed,
    ] {
        let mut cell_style = Style::default();
        cell_style.set_fg(Color::Default);
        cell_style.set_underline(underline);
        let grid = Grid::from_rows(
            vec![vec![Cell::new('x', 1, cell_style)]],
            1,
            Style::default(),
        );
        let mut pane = content_pane(PaneId::new());
        pane.grid_view = Some(GridView {
            grid: Arc::new(grid),
            view_offset: 0,
        });
        let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

        let read_back = to_snapshot(&wire_frame(&sent));

        let arrived = read_back.panes[0]
            .grid_view
            .as_ref()
            .expect("the pane carries a grid");
        let cell = arrived
            .grid
            .cell(0, 0)
            .expect("the grid has a cell at (0, 0)");
        assert_eq!(cell.style().attrs().underline(), underline);
        assert_eq!(cell.style().fg(), Color::Default);
        assert_eq!(read_back, sent);
    }
}

/// Each cursor shape has its own wire spelling, and a pane that named none
/// reads back naming none.
#[test]
fn every_cursor_shape_reads_back_as_itself() {
    for shape in [
        Some(CursorShape::Block),
        Some(CursorShape::Underline),
        Some(CursorShape::Bar),
        None,
    ] {
        let mut pane = content_pane(PaneId::new());
        pane.cursor.shape = shape;
        let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

        let read_back = to_snapshot(&wire_frame(&sent));

        assert_eq!(read_back.panes[0].cursor.shape, shape);
        assert_eq!(read_back, sent);
    }
}

/// A run stands for every cell it covers, so a blank 80-column row travels as
/// one run of 80 and rebuilds into 80 cells.
#[test]
fn a_blank_eighty_column_row_travels_as_one_run_and_rebuilds_eighty_cells() {
    let mut pane = content_pane(PaneId::new());
    pane.grid_view = Some(GridView {
        grid: Arc::new(Grid::blank(1, 80, Style::default())),
        view_offset: 0,
    });
    let sent = snapshot(vec![pane, empty_pane(PaneId::new())]);

    let wire = wire_frame(&sent);
    let window = wire.panes[0]
        .window
        .as_ref()
        .expect("the pane carries a window");
    assert_eq!(window.cols, 80);
    assert_eq!(window.rows.len(), 1);
    assert_eq!(window.rows[0].runs.len(), 1);
    assert_eq!(window.rows[0].runs[0].count, 80);

    let read_back = to_snapshot(&wire);
    let arrived = read_back.panes[0]
        .grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(arrived.grid.dimensions(), (1, 80));
    assert_eq!(arrived.grid.rows()[0].len(), 80);
    assert_eq!(read_back, sent);
}

/// A frame whose panes all closed carries no panes, and reads back carrying
/// none.
#[test]
fn a_frame_carrying_no_panes_reads_back_with_no_panes() {
    let mut sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    sent.panes = Vec::new();

    let read_back = to_snapshot(&wire_frame(&sent));

    assert_eq!(read_back.panes, Vec::<PaneSnapshot>::new());
    assert_eq!(read_back, sent);
}

#[test]
fn every_name_the_answering_session_chose_reads_back_filtered() {
    // This process paints all four into its own terminal, and puts the session
    // name and the focused pane's title inside an `OSC 0` window title.
    let pane = PaneId::new();
    let mut sent = snapshot(vec![content_pane(pane), empty_pane(PaneId::new())]);
    sent.session.name = String::from("dev\u{7}\u{1b}]0;owned\u{7}");
    sent.session.active_tab.name = String::from("build\u{1b}[2J");
    sent.session.tabs_metadata[0].name = String::from("build\u{9b}2J");
    sent.session.tabs_metadata[1].name = String::from("\u{202e}gpj.exe");
    sent.panes[0].title = Some(String::from("~/work\u{7}\u{1b}]0;owned\u{7}"));

    let read_back = to_snapshot(&wire_frame(&sent));

    assert_eq!(read_back.session.name, "dev]0;owned");
    assert_eq!(read_back.session.active_tab.name, "build[2J");
    assert_eq!(read_back.session.tabs_metadata[0].name, "build2J");
    assert_eq!(read_back.session.tabs_metadata[1].name, "gpj.exe");
    assert_eq!(
        read_back.panes[0].title.as_deref(),
        Some("~/work]0;owned"),
        "the pane title reaches the window title"
    );
}

#[test]
fn a_name_past_the_reported_text_cap_reads_back_cut_to_it() {
    let cap = koshi_core::text::MAX_REPORTED_TEXT_BYTES;
    let mut sent = snapshot(vec![content_pane(PaneId::new()), empty_pane(PaneId::new())]);
    sent.session.name = "a".repeat(cap + 1);

    let read_back = to_snapshot(&wire_frame(&sent));

    assert_eq!(read_back.session.name, "a".repeat(cap));
}

#[test]
fn a_pane_cell_holding_a_control_character_reads_back_holding_it() {
    // A cell is the pane's own screen. The grid stores what the pane drew, and
    // the renderer places each cell rather than writing it through.
    let pane = PaneId::new();
    let mut sent = snapshot(vec![content_pane(pane), empty_pane(PaneId::new())]);
    let mut grid = grid();
    *grid.cell_mut(1, 0).expect("the grid has a cell at (1, 0)") =
        Cell::new('\u{1b}', 1, Style::default());
    sent.panes[0].grid_view = Some(GridView {
        grid: Arc::new(grid),
        view_offset: 0,
    });

    let read_back = to_snapshot(&wire_frame(&sent));

    let view = read_back.panes[0]
        .grid_view
        .as_ref()
        .expect("the pane carries a grid");
    assert_eq!(
        view.grid.cell(1, 0).expect("the cell survives").ch(),
        '\u{1b}'
    );
}
