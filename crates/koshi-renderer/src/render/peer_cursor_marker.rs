//! The cursor markers of the peers: one cell, in a peer's colors, at the
//! cursor of the pane the peer's keys reach, in each pane this viewer draws.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::style::Modifier;

use koshi_core::geometry::Point;

use crate::snapshot::{LayoutAreaSnapshot, RenderSnapshot};
use crate::theme::Theme;

use super::{
    compute_client_color_style, find_shown_cursor_cell, place_cell_rect, DrawnRows, FrameGeometry,
    PlacedLayoutArea,
};

#[cfg(test)]
mod tests;

/// One peer cursor marker: the buffer cell it paints and the `color_index`
/// of the peer whose colors it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PeerCursorMarker {
    /// The buffer cell the marker paints.
    pub(super) position: Position,
    /// The index of the peer whose colors the marker takes.
    pub(super) color_index: u32,
}

/// The cursor markers that the peers of `render_snapshot` put on the layout
/// area `layout_area`, placed at `layout_origin` and cut to
/// `content_clip_area`, sorted by row, then column. Empty while the session
/// has no peer.
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
/// first cell. Two peers on one cell put one marker, in the colors of the
/// lower `color_index`.
fn list_peer_cursor_markers(
    render_snapshot: &RenderSnapshot,
    layout_area: &LayoutAreaSnapshot,
    layout_origin: Point,
    content_clip_area: RatatuiRect,
) -> Vec<PeerCursorMarker> {
    let viewer_input_pane_id = render_snapshot.client_snapshot.focused_pane_id;
    let mut peer_cursor_markers: Vec<PeerCursorMarker> = render_snapshot
        .session_snapshot
        .peers
        .iter()
        .filter_map(|peer_snapshot| {
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
                    position,
                    color_index: peer_snapshot.color_index,
                })
        })
        .collect();
    peer_cursor_markers.sort_unstable_by_key(get_peer_cursor_marker_order_key);
    peer_cursor_markers.dedup_by_key(|peer_cursor_marker| peer_cursor_marker.position);
    peer_cursor_markers
}

/// The cursor markers of the peers on every layer `frame_geometry` places,
/// sorted by row, then column, then `color_index`: those of the active tab,
/// and those of each floating pane this viewer draws, cut to its visible area.
/// The cursors and the peers come from `render_snapshot`; the panes' places
/// come from `frame_geometry`.
pub(super) fn list_frame_peer_cursor_markers(
    render_snapshot: &RenderSnapshot,
    frame_geometry: &FrameGeometry<'_>,
) -> Vec<PeerCursorMarker> {
    let mut peer_cursor_markers = list_peer_cursor_markers(
        render_snapshot,
        &frame_geometry
            .render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .layout_area,
        frame_geometry.get_layout_origin(),
        frame_geometry.viewport_area,
    );
    for placed_floating_pane in &frame_geometry.placed_floating_panes {
        peer_cursor_markers.extend(list_peer_cursor_markers(
            render_snapshot,
            &placed_floating_pane
                .drawn_floating_pane_slot
                .floating_pane_slot
                .layout_area,
            placed_floating_pane.layout_origin,
            placed_floating_pane.visible_area,
        ));
    }
    peer_cursor_markers.sort_unstable_by_key(get_peer_cursor_marker_order_key);
    peer_cursor_markers
}

/// The key that orders peer cursor markers: row, then column, then
/// `color_index`.
pub(super) fn get_peer_cursor_marker_order_key(
    peer_cursor_marker: &PeerCursorMarker,
) -> (u16, u16, u32) {
    (
        peer_cursor_marker.position.y,
        peer_cursor_marker.position.x,
        peer_cursor_marker.color_index,
    )
}

/// Paint each peer cursor marker of `placed_layout_area`
/// ([`list_peer_cursor_markers`]) on the rows `drawn_rows` draws: the
/// cell keeps its symbol, takes the peer's text and fill colors
/// ([`Theme::compute_client_color`](crate::theme::Theme::compute_client_color)),
/// and loses reverse video, inside this viewer's highlight too.
pub(super) fn draw_peer_cursor_markers(
    render_snapshot: &RenderSnapshot,
    placed_layout_area: PlacedLayoutArea<'_>,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    for peer_cursor_marker in list_peer_cursor_markers(
        render_snapshot,
        placed_layout_area.layout_area,
        placed_layout_area.layout_origin,
        placed_layout_area.content_clip_area,
    ) {
        if !drawn_rows.is_row_drawn(peer_cursor_marker.position.y) {
            continue;
        }
        let Some(marker_cell) = screen_buffer.cell_mut(peer_cursor_marker.position) else {
            continue;
        };
        marker_cell.set_style(compute_client_color_style(
            theme.compute_client_color(peer_cursor_marker.color_index),
        ));
        marker_cell.modifier.remove(Modifier::REVERSED);
    }
}
