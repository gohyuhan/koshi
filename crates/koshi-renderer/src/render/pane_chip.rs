//! The pane chips: one number chip per peer whose keys reach a pane that this
//! viewer draws, on that pane's top border, or on its stack header while the
//! pane is a collapsed stack member.

use ratatui::layout::Rect as RatatuiRect;

use koshi_core::geometry::Point;
use koshi_core::ids::PaneId;

use crate::snapshot::{LayoutAreaSnapshot, SessionSnapshot};
use crate::theme::ClientColor;

use super::client_chip::{
    compute_hidden_client_mark_width, solve_client_chip_group, ClientChipGroup,
};
use super::{
    compute_stack_header_indicator_start_column, format_stack_header_indicator, place_cell_rect,
    FrameLayer, PlacedFloatingPane,
};

#[cfg(test)]
mod tests;

/// The chips of the peers whose keys reach one pane, placed on one row of
/// that pane's layer: its stack header when the layer draws one for the pane,
/// else its top border.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneChipGroup {
    /// The layer of the pane.
    pub(crate) frame_layer: FrameLayer,
    /// The pane whose top border or stack header carries the chips.
    pub(crate) pane_id: PaneId,
    /// Whether the chips sit on the pane's stack header, and not on its top
    /// border.
    pub(crate) is_on_stack_header: bool,
    /// The buffer row of the chips.
    pub(crate) row_index: u16,
    /// The chips and the `+N` mark.
    pub(crate) client_chip_group: ClientChipGroup,
    /// The colors of the peers whose keys reach the pane, in `color_index`
    /// order. The chips drawn take the first of them, in order.
    pub(crate) client_colors: Vec<ClientColor>,
    /// The positions in `session_snapshot.peers` of the peers whose keys
    /// reach the pane, in the same order as `client_colors`. Peers
    /// `[bob, carol, dave]` where bob and dave type in the pane give
    /// `[0, 2]`.
    pub(crate) peer_indexes: Vec<usize>,
}

/// The pane chip groups of one frame
/// ([`FrameGeometry::get_pane_chip_groups`](crate::FrameGeometry::get_pane_chip_groups)),
/// sorted by layer, then pane id. Empty while no peer's keys reach a pane
/// that this viewer draws with room for a chip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FramePaneChipGroups {
    pane_chip_groups: Vec<PaneChipGroup>,
}

impl FramePaneChipGroups {
    /// The groups on `frame_layer`, sorted by pane id. Empty when the layer
    /// holds none.
    pub(crate) fn list_layer_groups(&self, frame_layer: FrameLayer) -> &[PaneChipGroup] {
        let layer_start_index = self
            .pane_chip_groups
            .partition_point(|pane_chip_group| pane_chip_group.frame_layer < frame_layer);
        let layer_end_index = self
            .pane_chip_groups
            .partition_point(|pane_chip_group| pane_chip_group.frame_layer <= frame_layer);
        &self.pane_chip_groups[layer_start_index..layer_end_index]
    }

    /// The group of `pane_id` drawn on the pane's stack header when
    /// `is_on_stack_header` is `true`, else on its top border. `None` when the
    /// pane has no such group.
    pub(super) fn find_pane_chip_group(
        &self,
        pane_id: PaneId,
        is_on_stack_header: bool,
    ) -> Option<&PaneChipGroup> {
        self.pane_chip_groups.iter().find(|pane_chip_group| {
            pane_chip_group.pane_id == pane_id
                && pane_chip_group.is_on_stack_header == is_on_stack_header
        })
    }

    /// The row of each group that one of these groups and
    /// `other_pane_chip_groups` holds and the other does not. Two groups are
    /// the same when every field is equal. A row comes once per such group, in
    /// no set order.
    ///
    /// Example: Bob's chip on the top border of a pane at buffer row `1` here,
    /// and on the top border of a pane at buffer row `14` in
    /// `other_pane_chip_groups`, gives `1` and `14`.
    pub(super) fn list_unshared_group_rows<'groups>(
        &'groups self,
        other_pane_chip_groups: &'groups Self,
    ) -> impl Iterator<Item = u16> + 'groups {
        [
            (self, other_pane_chip_groups),
            (other_pane_chip_groups, self),
        ]
        .into_iter()
        .flat_map(|(pane_chip_groups, compared_pane_chip_groups)| {
            pane_chip_groups
                .pane_chip_groups
                .iter()
                .filter(|pane_chip_group| {
                    find_layer_pane_chip_group(
                        compared_pane_chip_groups.list_layer_groups(pane_chip_group.frame_layer),
                        pane_chip_group.pane_id,
                    ) != Some(*pane_chip_group)
                })
                .map(|pane_chip_group| pane_chip_group.row_index)
        })
    }
}

/// The group of `pane_id` among `layer_pane_chip_groups`, the groups of one
/// layer sorted by pane id ([`FramePaneChipGroups::list_layer_groups`]).
/// `None` when that pane has no group there.
pub(super) fn find_layer_pane_chip_group(
    layer_pane_chip_groups: &[PaneChipGroup],
    pane_id: PaneId,
) -> Option<&PaneChipGroup> {
    layer_pane_chip_groups
        .binary_search_by_key(&pane_id, |pane_chip_group| pane_chip_group.pane_id)
        .ok()
        .map(|group_index| &layer_pane_chip_groups[group_index])
}

/// The cells of one row that the chip group of one pane may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneChipRoom {
    /// Whether the row is the pane's stack header, and not its top border.
    is_on_stack_header: bool,
    /// The buffer row.
    row_index: u16,
    /// The first column the group may take.
    start_column: u16,
    /// The column after the last one the group may take. The group ends here.
    end_column: u16,
    /// Half of the row's width, rounded down: the cells the group takes at
    /// most, unless the `+N` mark of every peer of the pane is wider.
    half_row_width: u16,
}

/// The pane chip groups of every layer of the frame of `render_snapshot`, in
/// the colors of `client_colors`, the colors of the viewing client and then
/// of each peer in list order ([`super::list_client_colors`]): the groups of
/// the active tab's tiled layout, placed at `layout_origin`, then those of
/// each of `placed_floating_panes`, at its own layout origin. Each group is
/// cut to the columns and rows of `viewport_area`.
///
/// A pane has a group when a peer's `typed_pane_id` or one of its
/// `recent_input_pane_ids` names it, the layout area has a row for its chips
/// ([`find_pane_chip_room`]), and that row has room for the `+N` mark of
/// every such peer. Its chips are those peers, in `color_index` order. The
/// group takes at most half of the row's width, and at least the width of that
/// mark, inside the row's room. A chip that
/// does not fit is dropped whole into the mark ([`solve_client_chip_group`]).
///
/// Example: Bob, of index `1`, typing in a tiled pane whose `30`-column box
/// starts at buffer `(0, 1)` gives ` 2 ` on columns `25` to `27` of row `1`:
/// `┌─── vim ──────────────── 2 ─┐`.
pub(super) fn list_frame_pane_chip_groups(
    session_snapshot: &SessionSnapshot,
    client_colors: &[ClientColor],
    layout_origin: Point,
    viewport_area: RatatuiRect,
    placed_floating_panes: &[PlacedFloatingPane<'_>],
) -> FramePaneChipGroups {
    let peer_client_colors = client_colors.iter().skip(1);
    let mut typed_peers: Vec<(PaneId, u32, ClientColor, usize)> = session_snapshot
        .peers
        .iter()
        .enumerate()
        .zip(peer_client_colors)
        .flat_map(|((peer_index, peer_snapshot), client_color)| {
            peer_snapshot
                .typed_pane_id
                .iter()
                .chain(&peer_snapshot.recent_input_pane_ids)
                .map(move |pane_id| {
                    (
                        *pane_id,
                        peer_snapshot.color_index,
                        *client_color,
                        peer_index,
                    )
                })
        })
        .collect();
    if typed_peers.is_empty() {
        return FramePaneChipGroups::default();
    }
    typed_peers
        .sort_unstable_by_key(|(typed_pane_id, color_index, _, _)| (*typed_pane_id, *color_index));

    let active_tab_layer = (
        FrameLayer::ActiveTab,
        &session_snapshot.active_tab_snapshot.layout_area,
        layout_origin,
    );
    let floating_pane_layers = placed_floating_panes.iter().enumerate().map(
        |(floating_pane_index, placed_floating_pane)| {
            (
                FrameLayer::FloatingPane {
                    floating_pane_index,
                },
                &placed_floating_pane
                    .drawn_floating_pane_slot
                    .floating_pane_slot
                    .layout_area,
                placed_floating_pane.layout_origin,
            )
        },
    );
    let mut pane_chip_groups = Vec::new();
    for (frame_layer, layout_area, layer_layout_origin) in
        std::iter::once(active_tab_layer).chain(floating_pane_layers)
    {
        for pane_peers in
            typed_peers.chunk_by(|first_peer, second_peer| first_peer.0 == second_peer.0)
        {
            let pane_id = pane_peers[0].0;
            let Some(pane_chip_room) =
                find_pane_chip_room(layout_area, layer_layout_origin, pane_id, viewport_area)
            else {
                continue;
            };
            let group_room_width = (pane_chip_room.end_column - pane_chip_room.start_column).min(
                pane_chip_room
                    .half_row_width
                    .max(compute_hidden_client_mark_width(pane_peers.len())),
            );
            let Some(client_chip_group) = solve_client_chip_group(
                pane_peers
                    .iter()
                    .map(|(_, color_index, _, _)| (*color_index, false)),
                pane_peers.len(),
                group_room_width,
                pane_chip_room.end_column,
            ) else {
                continue;
            };
            pane_chip_groups.push(PaneChipGroup {
                frame_layer,
                pane_id,
                is_on_stack_header: pane_chip_room.is_on_stack_header,
                row_index: pane_chip_room.row_index,
                client_chip_group,
                client_colors: pane_peers
                    .iter()
                    .map(|(_, _, client_color, _)| *client_color)
                    .collect(),
                peer_indexes: pane_peers
                    .iter()
                    .map(|(_, _, _, peer_index)| *peer_index)
                    .collect(),
            });
        }
    }
    FramePaneChipGroups { pane_chip_groups }
}

/// The cells of the row where `layout_area`, placed at `layout_origin`, puts
/// the chips of `pane_id`, cut to `viewport_area`:
///
/// 1. A stack header of `pane_id`: its row, from its first cell up to the
///    blank cell before `[n/total]`, which stays blank. Half of its width is
///    half of the cells left of `[n/total]`. A `30`-column header at column
///    `0` showing `[2/3]` gives columns `0` to `23`. A pane that also has a
///    visible slot, as in a frame of a placement animation, takes this row.
/// 2. Otherwise a visible slot of `pane_id`: its top border row, from the cell
///    after the top-left corner up to the `─` before the top-right corner,
///    which stays a border cell. Half of its width is half of the cells
///    between the two corners. A `30`-column box at column `0` gives columns
///    `1` to `27`.
///
/// `None` when `layout_area` has neither, when the row lies outside the rows
/// of `viewport_area`, or when no column of the row is left.
fn find_pane_chip_room(
    layout_area: &LayoutAreaSnapshot,
    layout_origin: Point,
    pane_id: PaneId,
    viewport_area: RatatuiRect,
) -> Option<PaneChipRoom> {
    let pane_chip_room = if let Some(stack_header) = layout_area
        .stack_headers
        .iter()
        .find(|stack_header| stack_header.pane_id == pane_id)
    {
        let header_area = place_cell_rect(stack_header.header_rect, layout_origin);
        if header_area.is_empty() {
            return None;
        }
        let indicator_start_column = compute_stack_header_indicator_start_column(
            header_area,
            &format_stack_header_indicator(stack_header),
        );
        PaneChipRoom {
            is_on_stack_header: true,
            row_index: header_area.y,
            start_column: header_area.x,
            end_column: indicator_start_column.saturating_sub(1),
            half_row_width: (indicator_start_column - header_area.x) / 2,
        }
    } else {
        let pane_slot = layout_area.find_visible_pane_slot(pane_id)?;
        let pane_area = place_cell_rect(pane_slot.outer_rect, layout_origin);
        if pane_area.is_empty() {
            return None;
        }
        PaneChipRoom {
            is_on_stack_header: false,
            row_index: pane_area.y,
            start_column: pane_area.x.saturating_add(1),
            end_column: pane_area.right().saturating_sub(2),
            half_row_width: pane_area.width.saturating_sub(2) / 2,
        }
    };
    if pane_chip_room.row_index < viewport_area.top()
        || pane_chip_room.row_index >= viewport_area.bottom()
    {
        return None;
    }
    let start_column = pane_chip_room.start_column.max(viewport_area.left());
    let end_column = pane_chip_room.end_column.min(viewport_area.right());
    (start_column < end_column).then_some(PaneChipRoom {
        start_column,
        end_column,
        ..pane_chip_room
    })
}
