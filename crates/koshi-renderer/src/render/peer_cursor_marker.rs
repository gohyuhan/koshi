//! The cursor markers of the peers: one cell, in a peer's colors, at the
//! cursor of the pane the peer's keys reach, in each pane this viewer draws.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::style::Modifier;

use koshi_core::geometry::Point;

use crate::snapshot::{LayoutAreaSnapshot, RenderSnapshot};
use crate::theme::ClientColor;

use super::{
    compute_client_color_style, find_shown_cursor_cell, place_cell_rect, DrawnRows, FrameLayer,
    PlacedFloatingPane,
};

#[cfg(test)]
mod tests;

/// One peer cursor marker: the layer of the pane it marks, the buffer cell
/// it paints, and the `color_index` and colors of the peer it marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerCursorMarker {
    /// The layer of the pane that holds the cursor.
    pub(crate) frame_layer: FrameLayer,
    /// The buffer cell the marker paints.
    pub(crate) position: Position,
    /// The index of the peer the marker marks.
    pub(crate) color_index: u32,
    /// The peer's colors in the viewer's theme, which the marker cell takes.
    pub(crate) client_color: ClientColor,
}

/// The peer cursor markers of one frame
/// ([`FrameGeometry::get_peer_cursor_markers`](crate::FrameGeometry::get_peer_cursor_markers)),
/// sorted by layer, row, column, then `color_index`. Empty while the session
/// has no peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FramePeerCursorMarkers {
    peer_cursor_markers: Vec<PeerCursorMarker>,
}

impl FramePeerCursorMarkers {
    /// The markers on `frame_layer`, sorted by row, column, then
    /// `color_index`. Empty when the layer holds none.
    pub(crate) fn list_layer_markers(&self, frame_layer: FrameLayer) -> &[PeerCursorMarker] {
        let layer_start_index = self
            .peer_cursor_markers
            .partition_point(|peer_cursor_marker| peer_cursor_marker.frame_layer < frame_layer);
        let layer_end_index = self
            .peer_cursor_markers
            .partition_point(|peer_cursor_marker| peer_cursor_marker.frame_layer <= frame_layer);
        &self.peer_cursor_markers[layer_start_index..layer_end_index]
    }

    /// The row of each marker that one of these markers and
    /// `other_peer_cursor_markers` holds and the other does not, compared by
    /// layer, cell and `color_index`. A row comes once per such marker, in
    /// no set order.
    ///
    /// Example: Bob's marker at buffer cell `(13, 7)` here and at `(13, 8)`
    /// in `other_peer_cursor_markers` gives `7` and `8`.
    pub(super) fn list_unshared_marker_rows<'markers>(
        &'markers self,
        other_peer_cursor_markers: &'markers Self,
    ) -> impl Iterator<Item = u16> + 'markers {
        [
            (self, other_peer_cursor_markers),
            (other_peer_cursor_markers, self),
        ]
        .into_iter()
        .flat_map(|(peer_cursor_markers, compared_peer_cursor_markers)| {
            peer_cursor_markers
                .peer_cursor_markers
                .iter()
                .filter(|peer_cursor_marker| {
                    compared_peer_cursor_markers
                        .peer_cursor_markers
                        .binary_search_by_key(
                            &get_peer_cursor_marker_order_key(peer_cursor_marker),
                            get_peer_cursor_marker_order_key,
                        )
                        .is_err()
                })
                .map(|peer_cursor_marker| peer_cursor_marker.position.y)
        })
    }
}

/// Push onto `peer_cursor_markers` the cursor markers that the peers of
/// `render_snapshot` put on the layout area `layout_area` of `frame_layer`,
/// placed at `layout_origin` and cut to `content_clip_area`. Each marker
/// takes its peer's colors from `client_colors`, the colors of the viewing
/// client and then of each peer in list order ([`super::list_client_colors`]).
///
/// A peer puts a marker when all of these hold:
///
/// 1. Its `typed_pane_id` names a pane, and that pane is not the viewing
///    client's `focused_pane_id`.
/// 2. That pane has a visible slot with a content rect in `layout_area`.
/// 3. The pane's cursor shows in that content rect
///    ([`find_shown_cursor_cell`]), inside `content_clip_area`.
///
/// A cursor on the second cell of a 2-column character marks the character's
/// first cell.
fn push_layer_peer_cursor_markers(
    render_snapshot: &RenderSnapshot,
    client_colors: &[ClientColor],
    frame_layer: FrameLayer,
    layout_area: &LayoutAreaSnapshot,
    layout_origin: Point,
    content_clip_area: RatatuiRect,
    peer_cursor_markers: &mut Vec<PeerCursorMarker>,
) {
    let viewer_input_pane_id = render_snapshot.client_snapshot.focused_pane_id;
    let peer_client_colors = client_colors.iter().skip(1);
    peer_cursor_markers.extend(
        render_snapshot
            .session_snapshot
            .peers
            .iter()
            .zip(peer_client_colors)
            .filter_map(|(peer_snapshot, client_color)| {
                let typed_pane_id = peer_snapshot.typed_pane_id?;
                if Some(typed_pane_id) == viewer_input_pane_id {
                    return None;
                }
                let content_rect = layout_area
                    .find_visible_pane_slot(typed_pane_id)?
                    .content_rect?;
                let content_area = place_cell_rect(content_rect, layout_origin);
                let pane_snapshot = render_snapshot.find_pane_snapshot(typed_pane_id)?;
                let mut position = find_shown_cursor_cell(pane_snapshot, content_area)?;
                let is_wide_character_second_cell = pane_snapshot
                    .terminal_grid_view
                    .as_ref()?
                    .get_cell(position.y - content_area.y, position.x - content_area.x)
                    .is_some_and(|cell| cell.get_display_width() == 0);
                if is_wide_character_second_cell && position.x > content_area.x {
                    position.x -= 1;
                }
                content_clip_area
                    .contains(position)
                    .then_some(PeerCursorMarker {
                        frame_layer,
                        position,
                        color_index: peer_snapshot.color_index,
                        client_color: *client_color,
                    })
            }),
    );
}

/// The cursor markers that the peers of `render_snapshot` put on every
/// layer of its frame ([`push_layer_peer_cursor_markers`]), in the colors of
/// `client_colors`: those of the active tab's tiled layout, placed at
/// `layout_origin` and cut to `viewport_area`, then those of each of
/// `placed_floating_panes`, cut to its visible area. Two peers on one cell of
/// one layer put one marker, in the colors of the lower `color_index`.
pub(super) fn list_frame_peer_cursor_markers(
    render_snapshot: &RenderSnapshot,
    client_colors: &[ClientColor],
    layout_origin: Point,
    viewport_area: RatatuiRect,
    placed_floating_panes: &[PlacedFloatingPane<'_>],
) -> FramePeerCursorMarkers {
    let mut peer_cursor_markers = Vec::new();
    push_layer_peer_cursor_markers(
        render_snapshot,
        client_colors,
        FrameLayer::ActiveTab,
        &render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .layout_area,
        layout_origin,
        viewport_area,
        &mut peer_cursor_markers,
    );
    for (floating_pane_index, placed_floating_pane) in placed_floating_panes.iter().enumerate() {
        push_layer_peer_cursor_markers(
            render_snapshot,
            client_colors,
            FrameLayer::FloatingPane {
                floating_pane_index,
            },
            &placed_floating_pane
                .drawn_floating_pane_slot
                .floating_pane_slot
                .layout_area,
            placed_floating_pane.layout_origin,
            placed_floating_pane.visible_area,
            &mut peer_cursor_markers,
        );
    }
    peer_cursor_markers.sort_unstable_by_key(get_peer_cursor_marker_order_key);
    peer_cursor_markers.dedup_by_key(|peer_cursor_marker| {
        (peer_cursor_marker.frame_layer, peer_cursor_marker.position)
    });
    FramePeerCursorMarkers {
        peer_cursor_markers,
    }
}

/// The key that orders peer cursor markers: layer, row, column, then
/// `color_index`.
fn get_peer_cursor_marker_order_key(
    peer_cursor_marker: &PeerCursorMarker,
) -> (FrameLayer, u16, u16, u32) {
    (
        peer_cursor_marker.frame_layer,
        peer_cursor_marker.position.y,
        peer_cursor_marker.position.x,
        peer_cursor_marker.color_index,
    )
}

/// Paint each of `peer_cursor_markers` on the rows `drawn_rows` draws: the
/// cell keeps its symbol, takes the marker's text and fill colors, and loses
/// reverse video, inside this viewer's highlight too.
pub(super) fn draw_peer_cursor_markers(
    peer_cursor_markers: &[PeerCursorMarker],
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    for peer_cursor_marker in peer_cursor_markers {
        if !drawn_rows.is_row_drawn(peer_cursor_marker.position.y) {
            continue;
        }
        let Some(marker_cell) = screen_buffer.cell_mut(peer_cursor_marker.position) else {
            continue;
        };
        marker_cell.set_style(compute_client_color_style(peer_cursor_marker.client_color));
        marker_cell.modifier.remove(Modifier::REVERSED);
    }
}
