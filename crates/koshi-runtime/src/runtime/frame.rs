//! The wire-frame builder: turning the in-process [`RenderSnapshot`] into the
//! [`PaintedFrame`] a client in another process draws.
//!
//! [`build_wire_frame`] is a plain field-for-field mapping of everything but
//! the rows, the cursor and the scroll position: the session, its solved
//! active tab, the tab-bar entries, the floating panes, each pane's title,
//! modes, highlight and image placements, and the viewing client's own state.
//! Those parts already hold shared types
//! ([`Rect`](koshi_core::geometry::Rect),
//! [`Size`](koshi_core::geometry::Size),
//! [`StackHeader`](koshi_layout::solver::StackHeader),
//! [`LayoutMode`](koshi_layout::mode::LayoutMode),
//! [`MouseTracking`](koshi_core::mouse::MouseTracking)), so they copy straight
//! across. The rows, the cursor and the scroll position travel in the painted
//! update ([`crate::runtime::painted_update`]).
//!
//! A placement preview carries each visible pane's rows itself, each row in
//! the row encoding ([`koshi_terminal::row_encoding`]).

use koshi_core::ids::PaneId;
use koshi_ipc::frame::{
    FrameClient, FrameFloatingPaneSlot, FrameGraphicsProtocol, FrameImageAction,
    FrameImageDimension, FrameImageDisplay, FrameImagePlacement, FrameImageRecordHeader,
    FrameImageTransfer, FramePane, FrameSelection, FrameSession, FrameSixelBackground, FrameSlot,
    FrameTab, FrameTabMetadata, PaintedFrame, MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT,
};
use koshi_ipc::placement::{
    EncodedRow, PanePlacementClientSnapshot as WirePlacementClientSnapshot,
    PanePlacementPaneSnapshot as WirePlacementPaneSnapshot,
    PanePlacementSizing as WirePlacementSizing, PanePlacementSnapshot as WirePlacementSnapshot,
    PanePlacementSource as WirePlacementSource,
    PanePlacementTabSnapshot as WirePlacementTabSnapshot, PlacementWindow,
};
use koshi_renderer::snapshot::{
    GridView, ImagePlacementSnapshot, PaneSlot, PaneSnapshot, PlacementPaneSnapshot,
    PlacementSnapshot, PlacementSource, PlacementTabSnapshot, RenderSnapshot, TabMetadata,
};
use koshi_terminal::graphics::{
    GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay, ImageRecord, SixelBackground,
};

/// Turn one painted frame into the form its JSON part travels in.
///
/// Carries the session's identity, its solved active tab, the tab-bar entries,
/// the floating panes the viewing client shows, every pane's title, modes,
/// highlight and image placements, and the viewing client's own state.
#[must_use]
pub fn build_wire_frame(render_snapshot: &RenderSnapshot) -> PaintedFrame {
    let mut next_image_content_id = 1u64;
    build_wire_frame_with_content_ids(render_snapshot, |_, _| {
        let image_content_id = next_image_content_id;
        next_image_content_id = next_image_content_id.saturating_add(1);
        Some(image_content_id)
    })
}

/// Turn one painted frame into wire form with connection-selected image ids.
///
/// `assign_image_content_id` gives each image placement its connection-local
/// content id. A placement it gives `None` travels with its own content id,
/// no record header and `is_available` `false`.
#[must_use]
pub(crate) fn build_wire_frame_with_content_ids(
    render_snapshot: &RenderSnapshot,
    mut assign_image_content_id: impl FnMut(PaneId, &ImagePlacementSnapshot) -> Option<u64>,
) -> PaintedFrame {
    let session_snapshot = &render_snapshot.session_snapshot;
    let active_tab_snapshot = &session_snapshot.active_tab_snapshot;
    let client_snapshot = &render_snapshot.client_snapshot;
    PaintedFrame {
        is_recovery_notice_visible: render_snapshot.is_recovery_notice_visible,
        session_snapshot: FrameSession {
            session_id: session_snapshot.session_id,
            session_revision: session_snapshot.session_revision,
            session_name: session_snapshot.session_name.clone(),
            active_tab_snapshot: FrameTab {
                tab_id: active_tab_snapshot.tab_id,
                tab_name: active_tab_snapshot.tab_name.clone(),
                pane_slots: active_tab_snapshot
                    .pane_slots
                    .iter()
                    .map(build_wire_slot)
                    .collect(),
                tab_size: active_tab_snapshot.tab_size,
                stack_headers: active_tab_snapshot.stack_headers.clone(),
                layout_mode: active_tab_snapshot.layout_mode,
                is_every_pane_suppressed: active_tab_snapshot.is_every_pane_suppressed,
                gap_cell_count: active_tab_snapshot.gap_cell_count,
            },
            tab_snapshots: session_snapshot
                .tabs_metadata
                .iter()
                .map(build_wire_tab_metadata)
                .collect(),
            floating_pane_slots: session_snapshot
                .floating_pane_slots
                .iter()
                .map(|floating_pane_slot| FrameFloatingPaneSlot {
                    pane_id: floating_pane_slot.pane_id,
                    outer_rect: floating_pane_slot.outer_rect,
                    content_rect: floating_pane_slot.content_rect,
                    is_pinned: floating_pane_slot.is_pinned,
                })
                .collect(),
            floating_pane_ids: session_snapshot.floating_pane_ids.clone(),
        },
        pane_snapshots: render_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| build_wire_pane(pane_snapshot, &mut assign_image_content_id))
            .collect(),
        client_snapshot: FrameClient {
            client_id: client_snapshot.client_id,
            client_revision: client_snapshot.client_revision,
            viewport_size: client_snapshot.viewport_size,
            active_tab_id: client_snapshot.active_tab_id,
            focused_pane_id: client_snapshot.focused_pane_id,
            active_tab_focused_pane_id: client_snapshot.active_tab_focused_pane_id,
            lock_mode: client_snapshot.lock_mode,
            last_applied_lock_command_id: client_snapshot.last_applied_lock_command_id,
            last_applied_floating_pane_move_command_id: client_snapshot
                .last_applied_floating_pane_move_command_id,
            is_mouse_selection_enabled: client_snapshot.is_mouse_selection_enabled,
        },
    }
}

/// Turn a read-only placement preview into wire form with local image ids.
#[must_use]
pub(crate) fn build_wire_placement_snapshot(
    placement_snapshot: &PlacementSnapshot,
) -> WirePlacementSnapshot {
    let mut next_image_content_id = 1u64;
    build_wire_placement_snapshot_with_content_ids(placement_snapshot, |_, _| {
        let image_content_id = next_image_content_id;
        next_image_content_id = next_image_content_id.saturating_add(1);
        Some(image_content_id)
    })
}

/// Turn a read-only placement preview into wire form with connection-selected image ids.
///
/// `assign_image_content_id` gives each image placement its connection-local
/// content id. A placement it gives `None` travels with its own content id,
/// no record header and `is_available` `false`.
#[must_use]
pub(crate) fn build_wire_placement_snapshot_with_content_ids(
    placement_snapshot: &PlacementSnapshot,
    mut assign_image_content_id: impl FnMut(PaneId, &ImagePlacementSnapshot) -> Option<u64>,
) -> WirePlacementSnapshot {
    WirePlacementSnapshot {
        session_id: placement_snapshot.session_id,
        source_pane_id: placement_snapshot.source_pane_id,
        source: match &placement_snapshot.source {
            PlacementSource::Tiled(source_tab_snapshot) => WirePlacementSource::Tiled(
                build_wire_placement_tab(source_tab_snapshot, &mut assign_image_content_id),
            ),
            PlacementSource::Floating(source_pane_snapshot) => WirePlacementSource::Floating(
                build_wire_placement_pane(source_pane_snapshot, &mut assign_image_content_id),
            ),
        },
        destination_tab_id: placement_snapshot.destination_tab_id,
        session_placement_revision: placement_snapshot.session_placement_revision,
        client_placement_revision: placement_snapshot.client_placement_revision,
        destination_tab_snapshot: placement_snapshot.destination_tab_snapshot.as_ref().map(
            |tab_snapshot| build_wire_placement_tab(tab_snapshot, &mut assign_image_content_id),
        ),
        client_snapshot: WirePlacementClientSnapshot {
            client_id: placement_snapshot.client_snapshot.client_snapshot.client_id,
            viewport_size: placement_snapshot
                .client_snapshot
                .client_snapshot
                .viewport_size,
            pane_area: placement_snapshot.client_snapshot.reported_pane_area,
            active_tab_id: placement_snapshot
                .client_snapshot
                .client_snapshot
                .active_tab_id,
            focused_pane_id: placement_snapshot
                .client_snapshot
                .client_snapshot
                .focused_pane_id,
        },
        pane_sizing: WirePlacementSizing::from(placement_snapshot.pane_sizing),
    }
}

fn build_wire_placement_tab(
    placement_tab_snapshot: &PlacementTabSnapshot,
    assign_image_content_id: &mut impl FnMut(PaneId, &ImagePlacementSnapshot) -> Option<u64>,
) -> WirePlacementTabSnapshot {
    let tab_snapshot = &placement_tab_snapshot.tab_snapshot;
    WirePlacementTabSnapshot {
        tab_id: tab_snapshot.tab_id,
        tab_name: tab_snapshot.tab_name.clone(),
        layout_tree: placement_tab_snapshot.layout_tree.clone(),
        pane_slots: tab_snapshot
            .pane_slots
            .iter()
            .map(build_wire_slot)
            .collect(),
        tab_size: tab_snapshot.tab_size,
        stack_headers: tab_snapshot.stack_headers.clone(),
        layout_mode: tab_snapshot.layout_mode,
        is_every_pane_suppressed: tab_snapshot.is_every_pane_suppressed,
        gap_cell_count: tab_snapshot.gap_cell_count,
        pane_snapshots: placement_tab_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| build_wire_placement_pane(pane_snapshot, assign_image_content_id))
            .collect(),
    }
}

fn build_wire_placement_pane(
    placement_pane_snapshot: &PlacementPaneSnapshot,
    assign_image_content_id: &mut impl FnMut(PaneId, &ImagePlacementSnapshot) -> Option<u64>,
) -> WirePlacementPaneSnapshot {
    WirePlacementPaneSnapshot {
        pane_id: placement_pane_snapshot.pane_id,
        terminal_window: placement_pane_snapshot
            .terminal_grid_view
            .as_ref()
            .map(build_wire_placement_window),
        image_placement_snapshots: placement_pane_snapshot
            .image_placement_snapshots
            .iter()
            .map(|image_placement_snapshot| {
                build_wire_image_placement(
                    placement_pane_snapshot.pane_id,
                    image_placement_snapshot,
                    assign_image_content_id,
                )
            })
            .collect(),
    }
}

/// Build the metadata sent before one image record's RGBA chunks.
#[must_use]
pub(crate) fn build_wire_image_transfer(
    image_content_id: u64,
    image_record: &ImageRecord,
) -> FrameImageTransfer {
    FrameImageTransfer {
        image_content_id,
        image_record: FrameImageRecordHeader {
            protocol: build_wire_graphics_protocol(image_record.protocol),
            pixel_width: image_record.image.pixel_width,
            pixel_height: image_record.image.pixel_height,
            image_action: build_wire_image_action(image_record.action),
            display: build_wire_image_display(&image_record.display),
            anchor_cell: image_record.anchor,
        },
        image_byte_count: u64::try_from(image_record.image.rgba_bytes.len())
            .expect("an image byte count fits in a frame transfer"),
    }
}

/// Return bounded RGBA chunks for one connection-local image record.
pub(crate) fn build_wire_image_chunk_sources(
    image_record: &ImageRecord,
) -> impl Iterator<Item = (u64, bool, &[u8])> {
    let image_byte_count = image_record.image.rgba_bytes.len();
    image_record
        .image
        .rgba_bytes
        .chunks(MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
        .enumerate()
        .map(move |(chunk_index, chunk_bytes)| {
            let chunk_index =
                u64::try_from(chunk_index).expect("an image chunk index fits in a transfer offset");
            let chunk_size = u64::try_from(MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
                .expect("the image chunk size fits in a transfer offset");
            let byte_offset = chunk_index * chunk_size;
            let is_last_chunk = byte_offset.checked_add(
                u64::try_from(chunk_bytes.len()).expect("an image chunk length fits in an offset"),
            ) == Some(
                u64::try_from(image_byte_count).expect("an image byte count fits in a transfer"),
            );
            (byte_offset, is_last_chunk, chunk_bytes)
        })
}

/// One solved pane placement, as it travels.
fn build_wire_slot(pane_slot: &PaneSlot) -> FrameSlot {
    FrameSlot {
        pane_id: pane_slot.pane_id,
        outer_rect: pane_slot.outer_rect,
        content_rect: pane_slot.content_rect,
        is_visible: pane_slot.is_visible,
        is_suppressed: pane_slot.is_suppressed,
    }
}

/// One tab-bar entry, as it travels.
fn build_wire_tab_metadata(tab_metadata: &TabMetadata) -> FrameTabMetadata {
    FrameTabMetadata {
        tab_id: tab_metadata.tab_id,
        tab_name: tab_metadata.tab_name.clone(),
        tab_index: tab_metadata.tab_index,
        is_active: tab_metadata.is_active,
    }
}

/// One pane's title, modes, highlight and image placements, as they travel.
fn build_wire_pane(
    pane_snapshot: &PaneSnapshot,
    assign_image_content_id: &mut impl FnMut(PaneId, &ImagePlacementSnapshot) -> Option<u64>,
) -> FramePane {
    FramePane {
        pane_id: pane_snapshot.pane_id,
        pane_title: pane_snapshot.pane_title.clone(),
        image_placement_snapshots: pane_snapshot
            .image_placement_snapshots
            .iter()
            .map(|image_placement_snapshot| {
                build_wire_image_placement(
                    pane_snapshot.pane_id,
                    image_placement_snapshot,
                    assign_image_content_id,
                )
            })
            .collect(),
        is_reverse_video: pane_snapshot.is_reverse_video,
        mouse_tracking: pane_snapshot.mouse_tracking,
        is_alternate_scroll_enabled: pane_snapshot.is_alternate_scroll_enabled,
        is_on_alt_screen: pane_snapshot.is_on_alternate_screen,
        selection_spans: pane_snapshot
            .selection_spans
            .as_ref()
            .map(|selection_spans| FrameSelection {
                row_spans: selection_spans.row_spans.clone(),
            }),
        has_selection: pane_snapshot.has_selection,
    }
}

/// One validated image placement, as it travels with its pane.
fn build_wire_image_placement(
    pane_id: PaneId,
    image_placement_snapshot: &ImagePlacementSnapshot,
    assign_image_content_id: &mut impl FnMut(PaneId, &ImagePlacementSnapshot) -> Option<u64>,
) -> FrameImagePlacement {
    let (row_count, column_count) = image_placement_snapshot.get_cell_dimensions();
    let assigned_image_content_id = assign_image_content_id(pane_id, image_placement_snapshot);
    let available_image_record =
        assigned_image_content_id.and(image_placement_snapshot.get_image_record());
    FrameImagePlacement {
        placement_id: image_placement_snapshot.get_placement_id(),
        cell_geometry: Some(image_placement_snapshot.get_cell_geometry()),
        image_record: available_image_record
            .map(|image_record| build_wire_image_transfer(1, image_record).image_record),
        image_content_id: assigned_image_content_id
            .unwrap_or_else(|| image_placement_snapshot.get_image_content_id()),
        is_available: available_image_record.is_some(),
        anchor_cell: image_placement_snapshot.get_anchor_cell(),
        column_count,
        row_count,
    }
}

/// The source image protocol in wire form.
fn build_wire_graphics_protocol(protocol: GraphicsProtocol) -> FrameGraphicsProtocol {
    match protocol {
        GraphicsProtocol::Sixel => FrameGraphicsProtocol::Sixel,
        GraphicsProtocol::Kitty => FrameGraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2 => FrameGraphicsProtocol::Iterm2,
    }
}

/// The image operation in wire form.
fn build_wire_image_action(action: ImageAction) -> FrameImageAction {
    match action {
        ImageAction::Transmit => FrameImageAction::Transmit,
        ImageAction::Display => FrameImageAction::Display,
        ImageAction::TransmitAndDisplay => FrameImageAction::TransmitAndDisplay,
    }
}

/// One protocol dimension in wire form.
fn build_wire_image_dimension(dimension: ImageDimension) -> FrameImageDimension {
    match dimension {
        ImageDimension::Cells(cell_count) => FrameImageDimension::Cells(cell_count),
        ImageDimension::Pixels(pixel_count) => FrameImageDimension::Pixels(pixel_count),
        ImageDimension::Percent(percent) => FrameImageDimension::Percent(percent),
        ImageDimension::Auto => FrameImageDimension::Auto,
    }
}

/// One Sixel background rule in wire form.
fn build_wire_sixel_background(background: SixelBackground) -> FrameSixelBackground {
    match background {
        SixelBackground::Terminal => FrameSixelBackground::Terminal,
        SixelBackground::Preserve => FrameSixelBackground::Preserve,
    }
}

/// Display metadata in wire form.
fn build_wire_image_display(display: &ImageDisplay) -> FrameImageDisplay {
    FrameImageDisplay {
        response_suppression_level: display.response_suppression_level,
        requested_width: display.requested_width.map(build_wire_image_dimension),
        requested_height: display.requested_height.map(build_wire_image_dimension),
        is_aspect_ratio_preserved: display.is_aspect_ratio_preserved,
        sixel_background: display.sixel_background.map(build_wire_sixel_background),
        image_id: display.image_id,
        image_number: display.image_number,
        placement_id: display.placement_id,
        usage_hints: display.usage_hints,
        is_unicode_placeholder: display.is_unicode_placeholder,
        z_index: display.z_index,
        relative_image_id: display.relative_image_id,
        relative_placement_id: display.relative_placement_id,
        relative_column_offset: display.relative_column_offset,
        relative_row_offset: display.relative_row_offset,
        requested_column_count: display.requested_column_count,
        requested_row_count: display.requested_row_count,
        source_pixel_offset_x: display.source_pixel_offset_x,
        source_pixel_offset_y: display.source_pixel_offset_y,
        cell_pixel_offset_x: display.cell_pixel_offset_x,
        cell_pixel_offset_y: display.cell_pixel_offset_y,
        should_move_cursor: display.should_move_cursor,
    }
}

/// The rows of one pane of a placement preview, each in the row encoding.
fn build_wire_placement_window(terminal_grid_view: &GridView) -> PlacementWindow {
    PlacementWindow {
        column_count: terminal_grid_view.column_count,
        encoded_rows: terminal_grid_view
            .row_snapshots
            .iter()
            .map(|row_snapshot| EncodedRow {
                encoded_row_bytes: row_snapshot.get_encoded_row_bytes().to_vec(),
            })
            .collect(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
