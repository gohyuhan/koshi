//! The client chip tooltip: the box that opens under a hovered client chip or
//! `+N` mark and names the clients it stands for.
//!
//! One client's chip opens a box with the client's chip and label in its top
//! border and the client's origin on its one text row. A `+N` mark opens a box
//! titled `N more` with one row per hidden client of that chip group. When the
//! hidden clients do not fit the box, its last row counts the clients left out.
//!
//! The box draws last, over the tray, the notification center, toasts and
//! floating panes. It is not a hit region: a cell under it classifies as the
//! cell beneath it. The box opens below the hovered row, moves left at the
//! viewport's right edge, and opens above the row when it does not fit below.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::text::{Line, Span};

use koshi_core::client::ClientOrigin;
use koshi_core::display_width::compute_text_display_width;
use koshi_core::geometry::{Direction, Point};
use koshi_core::ids::{ClientId, PaneId};

use crate::hit_test::HitRegion;
use crate::notification::find_drawn_notification_toast_layout;
use crate::snapshot::{FrameLayout, PeerSnapshot, RenderSnapshot};
use crate::theme::{ClientColor, Theme};

use super::client_chip::{
    build_client_chip_text, compute_client_chip_width, compute_hidden_client_mark_width,
    ClientChipGroup, ClientChipSpan,
};
use super::floating::clear_overlay_area;
use super::style::compute_focused_border_style;
use super::PaintedClientChipGroups;
use super::{draw_pane_border, place_screen_rect, set_line_clipped, DrawnRows};

#[cfg(test)]
mod tests;

/// The most text rows a `+N` tooltip draws. Further hidden clients are counted
/// by its last row.
const MAX_CLIENT_CHIP_TOOLTIP_TEXT_ROW_COUNT: usize = 10;

/// The cells the box spends on its two border columns and its two padding
/// columns.
const CLIENT_CHIP_TOOLTIP_CHROME_CELL_COUNT: u16 = 4;

/// The narrowest box: its two border columns, its two padding columns, and one
/// text cell.
const MINIMUM_CLIENT_CHIP_TOOLTIP_WIDTH: u16 = 5;

/// The box drawn for the client chip or `+N` mark under the pointer, at
/// `rect` in screen cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientChipTooltip {
    /// The box: its border and every text row.
    rect: RatatuiRect,
    /// The title in the top border, after the left corner.
    title: Line<'static>,
    /// The text rows inside the border, top to bottom.
    text_rows: Vec<Line<'static>>,
}

impl ClientChipTooltip {
    /// The rows the box covers.
    pub fn list_rows(&self) -> std::ops::Range<u16> {
        self.rect.y..self.rect.bottom()
    }

    /// The box's screen rectangle.
    pub fn get_rect(&self) -> RatatuiRect {
        self.rect
    }
}

/// What one hovered chip group is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientChipHover {
    /// The chip group of the tab strip, on `row_index`.
    TabStrip {
        /// The hovered chip or mark.
        target: ClientChipHoverTarget,
        /// The tabline row the chips are drawn on.
        row_index: u16,
        /// The first column of the hovered chip or mark.
        start_column: u16,
    },
    /// The chip group of one pane, on `row_index`.
    Pane {
        /// The pane whose chips are drawn.
        pane_id: PaneId,
        /// Whether the chips are drawn on the pane's stack header, not its
        /// top border.
        is_on_stack_header: bool,
        /// The hovered chip or mark.
        target: ClientChipHoverTarget,
        /// The row the chips are drawn on.
        row_index: u16,
        /// The first column of the hovered chip or mark.
        start_column: u16,
    },
}

impl ClientChipHover {
    /// The hovered chip or mark.
    fn get_target(self) -> ClientChipHoverTarget {
        match self {
            Self::TabStrip { target, .. } | Self::Pane { target, .. } => target,
        }
    }

    /// The row the hovered chip or mark is drawn on.
    fn get_row_index(self) -> u16 {
        match self {
            Self::TabStrip { row_index, .. } | Self::Pane { row_index, .. } => row_index,
        }
    }

    /// The first column of the hovered chip or mark.
    fn get_start_column(self) -> u16 {
        match self {
            Self::TabStrip { start_column, .. } | Self::Pane { start_column, .. } => start_column,
        }
    }
}

/// The hovered cell of a chip group: one client's chip, or the `+N` mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientChipHoverTarget {
    /// The viewing client's own chip.
    ViewerClient,
    /// One peer's chip.
    PeerClient {
        /// The peer's id.
        client_id: ClientId,
    },
    /// The `+N` mark right of the chips.
    HiddenClients,
}

/// One client a tooltip names.
#[derive(Debug, Clone, Copy)]
enum TooltipClient<'a> {
    /// The viewing client.
    Viewer {
        /// The viewer's chip color index.
        color_index: u32,
        /// The viewer's label.
        label: &'a str,
    },
    /// A peer of the viewing client.
    Peer(&'a PeerSnapshot),
}

impl<'a> TooltipClient<'a> {
    /// The client's chip color index.
    fn get_color_index(self) -> u32 {
        match self {
            Self::Viewer { color_index, .. } => color_index,
            Self::Peer(peer_snapshot) => peer_snapshot.color_index,
        }
    }

    /// The client's label.
    fn get_label(self) -> &'a str {
        match self {
            Self::Viewer { label, .. } => label,
            Self::Peer(peer_snapshot) => &peer_snapshot.label,
        }
    }

    /// Whether this is the viewing client.
    fn is_viewer(self) -> bool {
        matches!(self, Self::Viewer { .. })
    }

    /// The origin text of the client's tooltip row: `you`, `remote`,
    /// `this account`, or `another account on this machine`.
    fn format_origin_text(self) -> &'static str {
        match self {
            Self::Viewer { .. } => "you",
            Self::Peer(peer_snapshot) => match peer_snapshot.origin {
                ClientOrigin::Remote => "remote",
                ClientOrigin::Local { is_same_user: true } => "this account",
                ClientOrigin::Local {
                    is_same_user: false,
                } => "another account on this machine",
            },
        }
    }
}

/// The side of the hovered row the box opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientChipTooltipSide {
    /// The box starts one row under the hovered row.
    Below,
    /// The box ends one row over the hovered row.
    Above,
}

/// The chip group under `pointer_position`, or `None` when the pointer lies on
/// no chip or mark of the group drawn there.
///
/// `hit_region` is the region [`resolve_hit_region`](crate::resolve_hit_region)
/// gives the pointer cell. Only the tabline, a pane's top border, a pane's
/// placement handle, and a pane's stack header can hold chips. The pointer row
/// must equal the row the chip group draws on. A chip covered by a toast is not
/// hoverable.
pub fn find_client_chip_hover(
    frame_layout: FrameLayout<'_>,
    client_chip_groups: &PaintedClientChipGroups,
    hit_region: HitRegion,
    pointer_position: Point,
) -> Option<ClientChipHover> {
    if is_pointer_over_toast(frame_layout, pointer_position) {
        return None;
    }
    match hit_region {
        HitRegion::Tabline => {
            if pointer_position.row != client_chip_groups.tabline_row? {
                return None;
            }
            let tabline_layout = client_chip_groups.tabline_layout.as_ref()?;
            let client_chip_group = tabline_layout.client_chip_group.as_ref()?;
            let (target, start_column) = find_hovered_chip_target(
                client_chip_group,
                pointer_position.column,
                |span_index, _| {
                    let client_chip_span = client_chip_group.client_chip_spans[span_index];
                    if client_chip_span.is_viewer_chip {
                        return Some(ClientChipHoverTarget::ViewerClient);
                    }
                    let client_id = frame_layout
                        .session_snapshot
                        .peers
                        .iter()
                        .find(|peer_snapshot| {
                            peer_snapshot.color_index == client_chip_span.color_index
                        })
                        .map(|peer_snapshot| peer_snapshot.client_id)?;
                    Some(ClientChipHoverTarget::PeerClient { client_id })
                },
            )?;
            Some(ClientChipHover::TabStrip {
                target,
                row_index: pointer_position.row,
                start_column,
            })
        }
        HitRegion::PaneBorder {
            pane_id,
            side: Direction::Up,
            ..
        }
        | HitRegion::PlacementHandle { pane_id } => {
            solve_pane_chip_hover(client_chip_groups, pane_id, false, pointer_position)
        }
        HitRegion::StackHeader { pane_id } => {
            solve_pane_chip_hover(client_chip_groups, pane_id, true, pointer_position)
        }
        _ => None,
    }
}

/// Solve the tooltip of `hover`, or `None` when the box has no room for one
/// text row, when the hovered group no longer exists, or when the group holds
/// no hidden client.
pub(crate) fn solve_client_chip_tooltip(
    render_snapshot: &RenderSnapshot,
    viewport_area: RatatuiRect,
    theme: &Theme,
    client_chip_groups: &PaintedClientChipGroups,
    hover: ClientChipHover,
) -> Option<ClientChipTooltip> {
    let (title, text_rows, hidden_client_count) = match hover.get_target() {
        ClientChipHoverTarget::ViewerClient => {
            let client = TooltipClient::Viewer {
                color_index: render_snapshot.client_snapshot.color_index,
                label: &render_snapshot.client_snapshot.label,
            };
            let client_color = theme.compute_client_color(client.get_color_index());
            (
                build_client_title(client, client_color),
                build_client_text_rows(client),
                None,
            )
        }
        ClientChipHoverTarget::PeerClient { client_id } => {
            let client = TooltipClient::Peer(
                render_snapshot
                    .session_snapshot
                    .peers
                    .iter()
                    .find(|peer_snapshot| peer_snapshot.client_id == client_id)?,
            );
            let client_color = theme.compute_client_color(client.get_color_index());
            (
                build_client_title(client, client_color),
                build_client_text_rows(client),
                None,
            )
        }
        ClientChipHoverTarget::HiddenClients => {
            let hidden_clients = list_hidden_clients(render_snapshot, client_chip_groups, hover)?;
            let hidden_client_count = hidden_clients.len();
            (
                Line::from(format!(" {hidden_client_count} more ")),
                build_hidden_client_rows(&hidden_clients, theme),
                Some(hidden_client_count),
            )
        }
    };
    let capped_text_row_count = text_rows.len().min(MAX_CLIENT_CHIP_TOOLTIP_TEXT_ROW_COUNT);
    let (side, text_row_count) = choose_client_chip_tooltip_side(
        hover.get_row_index(),
        capped_text_row_count,
        viewport_area,
    )?;
    let text_rows = match hidden_client_count {
        Some(hidden_client_count) => {
            cut_hidden_client_rows(text_rows, text_row_count, hidden_client_count)
        }
        None => text_rows.into_iter().take(text_row_count).collect(),
    };
    let content_width = compute_client_chip_tooltip_content_width(&title, &text_rows);
    let rect = place_client_chip_tooltip(
        hover.get_row_index(),
        hover.get_start_column(),
        content_width,
        text_rows.len(),
        side,
        viewport_area,
    )?;
    Some(ClientChipTooltip {
        rect,
        title,
        text_rows,
    })
}

/// Draw `tooltip` over every other frame surface. Its title goes on the top
/// border after the left corner. Each text row starts one blank cell after
/// the left border. A row wider than the box ends in `…`.
pub(super) fn draw_client_chip_tooltip(
    tooltip: &ClientChipTooltip,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    clear_overlay_area(tooltip.rect, drawn_rows, screen_buffer);
    draw_pane_border(
        tooltip.rect,
        compute_focused_border_style(theme),
        drawn_rows,
        screen_buffer,
    );
    if drawn_rows.is_row_drawn(tooltip.rect.y) {
        write_line_fitted(
            screen_buffer,
            RatatuiRect {
                x: tooltip.rect.x.saturating_add(1),
                y: tooltip.rect.y,
                width: tooltip.rect.width.saturating_sub(2),
                height: 1,
            },
            &tooltip.title,
        );
    }
    for (text_row_index, text_row) in tooltip.text_rows.iter().enumerate() {
        let row_index = tooltip
            .rect
            .y
            .saturating_add(1)
            .saturating_add(text_row_index as u16);
        if !drawn_rows.is_row_drawn(row_index) {
            continue;
        }
        write_line_fitted(
            screen_buffer,
            RatatuiRect {
                x: tooltip.rect.x.saturating_add(2),
                y: row_index,
                width: tooltip.rect.width.saturating_sub(4),
                height: 1,
            },
            text_row,
        );
    }
}

/// Write `line` at `line_area.x`. When the line is wider than
/// `line_area.width`, write its first `width - 1` cells and `…` in the last
/// cell.
fn write_line_fitted(screen_buffer: &mut Buffer, line_area: RatatuiRect, line: &Line<'_>) {
    if compute_line_cell_count(line) <= line_area.width {
        set_line_clipped(
            screen_buffer,
            line_area.x,
            line_area.y,
            line,
            line_area.width,
        );
        return;
    }
    let next_column = set_line_clipped(
        screen_buffer,
        line_area.x,
        line_area.y,
        line,
        line_area.width.saturating_sub(1),
    );
    set_line_clipped(screen_buffer, next_column, line_area.y, &Line::from("…"), 1);
}

/// The width of `line` in cells.
fn compute_line_cell_count(line: &Line<'_>) -> u16 {
    let cell_count = line
        .spans
        .iter()
        .map(|span| compute_text_display_width(&span.content))
        .sum::<usize>();
    u16::try_from(cell_count).unwrap_or(u16::MAX)
}

/// Whether `pointer_position` lies in the cells the toast draws.
fn is_pointer_over_toast(frame_layout: FrameLayout<'_>, pointer_position: Point) -> bool {
    find_drawn_notification_toast_layout(frame_layout)
        .map(|notification_toast_layout| place_screen_rect(notification_toast_layout.drawn_rect))
        .is_some_and(|toast_rect| {
            toast_rect.contains(Position::new(pointer_position.column, pointer_position.row))
        })
}

/// The chip group of `pane_id` and the hovered chip or mark in it.
fn solve_pane_chip_hover(
    client_chip_groups: &PaintedClientChipGroups,
    pane_id: PaneId,
    is_on_stack_header: bool,
    pointer_position: Point,
) -> Option<ClientChipHover> {
    let pane_chip_group = client_chip_groups
        .pane_chip_groups
        .find_pane_chip_group(pane_id, is_on_stack_header)?;
    if pointer_position.row != pane_chip_group.row_index {
        return None;
    }
    let (target, start_column) = find_hovered_chip_target(
        &pane_chip_group.client_chip_group,
        pointer_position.column,
        |span_index, _| {
            pane_chip_group.client_ids.get(span_index).map(|client_id| {
                ClientChipHoverTarget::PeerClient {
                    client_id: *client_id,
                }
            })
        },
    )?;
    Some(ClientChipHover::Pane {
        pane_id,
        is_on_stack_header,
        target,
        row_index: pane_chip_group.row_index,
        start_column,
    })
}

/// The chip or mark of `client_chip_group` that covers `pointer_column`, with
/// its first column. `client_target_for_span` names the target of a chip.
fn find_hovered_chip_target(
    client_chip_group: &ClientChipGroup,
    pointer_column: u16,
    mut client_target_for_span: impl FnMut(usize, &ClientChipSpan) -> Option<ClientChipHoverTarget>,
) -> Option<(ClientChipHoverTarget, u16)> {
    for (span_index, client_chip_span) in client_chip_group.client_chip_spans.iter().enumerate() {
        let chip_end_column = client_chip_span
            .start_column
            .saturating_add(compute_client_chip_width(client_chip_span.color_index));
        if client_chip_span.start_column <= pointer_column && pointer_column < chip_end_column {
            let target = client_target_for_span(span_index, client_chip_span)?;
            return Some((target, client_chip_span.start_column));
        }
    }
    let hidden_client_mark = client_chip_group.hidden_client_mark?;
    let mark_end_column =
        hidden_client_mark
            .start_column
            .saturating_add(compute_hidden_client_mark_width(
                hidden_client_mark.client_count,
            ));
    (hidden_client_mark.start_column <= pointer_column && pointer_column < mark_end_column)
        .then_some((
            ClientChipHoverTarget::HiddenClients,
            hidden_client_mark.start_column,
        ))
}

/// The hidden clients of the group of `hover`, in the group's order.
fn list_hidden_clients<'a>(
    render_snapshot: &'a RenderSnapshot,
    client_chip_groups: &PaintedClientChipGroups,
    hover: ClientChipHover,
) -> Option<Vec<TooltipClient<'a>>> {
    match hover {
        ClientChipHover::TabStrip { .. } => {
            let tabline_layout = client_chip_groups.tabline_layout.as_ref()?;
            let client_chip_group = tabline_layout.client_chip_group.as_ref()?;
            let hidden_client_mark = client_chip_group.hidden_client_mark?;
            let hidden_clients = std::iter::once(TooltipClient::Viewer {
                color_index: render_snapshot.client_snapshot.color_index,
                label: &render_snapshot.client_snapshot.label,
            })
            .chain(
                render_snapshot
                    .session_snapshot
                    .peers
                    .iter()
                    .map(TooltipClient::Peer),
            )
            .skip(client_chip_group.client_chip_spans.len())
            .collect::<Vec<_>>();
            (hidden_clients.len() == hidden_client_mark.client_count).then_some(hidden_clients)
        }
        ClientChipHover::Pane {
            pane_id,
            is_on_stack_header,
            ..
        } => {
            let pane_chip_group = client_chip_groups
                .pane_chip_groups
                .find_pane_chip_group(pane_id, is_on_stack_header)?;
            let client_chip_group = &pane_chip_group.client_chip_group;
            let hidden_client_mark = client_chip_group.hidden_client_mark?;
            let hidden_clients = pane_chip_group
                .client_ids
                .iter()
                .skip(client_chip_group.client_chip_spans.len())
                .filter_map(|client_id| {
                    render_snapshot
                        .session_snapshot
                        .peers
                        .iter()
                        .find(|peer_snapshot| peer_snapshot.client_id == *client_id)
                        .map(TooltipClient::Peer)
                })
                .collect::<Vec<_>>();
            (hidden_clients.len() == hidden_client_mark.client_count).then_some(hidden_clients)
        }
    }
}

/// The title of one client's tooltip: its chip, its label, and the blank cell
/// after the label.
fn build_client_title(client: TooltipClient<'_>, client_color: ClientColor) -> Line<'static> {
    let mut spans =
        build_client_chip_text(client.get_color_index(), client.is_viewer(), client_color).spans;
    spans.push(Span::raw(format!("{} ", client.get_label())));
    Line::from(spans)
}

/// The text rows of one client's tooltip: its origin.
fn build_client_text_rows(client: TooltipClient<'_>) -> Vec<Line<'static>> {
    vec![Line::from(client.format_origin_text())]
}

/// One row per hidden client: its chip, its label padded to one column after
/// the widest label, and its origin text.
fn build_hidden_client_rows(
    hidden_clients: &[TooltipClient<'_>],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let widest_label_cell_count = hidden_clients
        .iter()
        .map(|hidden_client| compute_text_display_width(hidden_client.get_label()))
        .max()
        .unwrap_or(0);
    let label_field_cell_count = widest_label_cell_count + 1;
    hidden_clients
        .iter()
        .map(|hidden_client| {
            let label = hidden_client.get_label();
            let label_cell_count = compute_text_display_width(label);
            let client_color = theme.compute_client_color(hidden_client.get_color_index());
            let mut spans = build_client_chip_text(
                hidden_client.get_color_index(),
                hidden_client.is_viewer(),
                client_color,
            )
            .spans;
            spans.push(Span::raw(label.to_owned()));
            spans.push(Span::raw(
                " ".repeat(label_field_cell_count.saturating_sub(label_cell_count)),
            ));
            spans.push(Span::raw(hidden_client.format_origin_text()));
            Line::from(spans)
        })
        .collect()
}

/// The first `row_count` rows of `client_text_rows`. When rows do not all fit,
/// the last kept row counts the clients left out.
fn cut_hidden_client_rows(
    mut client_text_rows: Vec<Line<'static>>,
    row_count: usize,
    hidden_client_count: usize,
) -> Vec<Line<'static>> {
    if client_text_rows.len() <= row_count {
        return client_text_rows;
    }
    if row_count == 0 {
        return Vec::new();
    }
    client_text_rows.truncate(row_count - 1);
    let omitted_client_count = hidden_client_count.saturating_sub(row_count - 1);
    client_text_rows.push(Line::from(format!(
        "…and {omitted_client_count} more · koshi list-clients"
    )));
    client_text_rows
}

/// Return the side of the anchor and the number of text rows the box shows.
///
/// The four outcomes, with an example for a 10-row viewport:
/// - all rows below when they fit: anchor row 2, 3 text rows -> below, 3;
/// - all rows above when they fit: anchor row 7, 3 text rows -> above, 3;
/// - the side with more rows, cut to that side's row count: anchor row 4,
///   10 text rows -> below, 3;
/// - no side holds one text row: anchor row 1, 1 text row -> `None`.
fn choose_client_chip_tooltip_side(
    anchor_row_index: u16,
    text_row_count: usize,
    viewport_area: RatatuiRect,
) -> Option<(ClientChipTooltipSide, usize)> {
    if text_row_count == 0 {
        return None;
    }
    let below_row_count = viewport_area
        .bottom()
        .saturating_sub(anchor_row_index.saturating_add(1))
        .saturating_sub(2) as usize;
    let above_row_count = anchor_row_index
        .saturating_sub(viewport_area.y)
        .saturating_sub(2) as usize;
    if below_row_count >= text_row_count {
        return Some((ClientChipTooltipSide::Below, text_row_count));
    }
    if above_row_count >= text_row_count {
        return Some((ClientChipTooltipSide::Above, text_row_count));
    }
    if below_row_count >= above_row_count && below_row_count > 0 {
        return Some((
            ClientChipTooltipSide::Below,
            text_row_count.min(below_row_count),
        ));
    }
    if above_row_count > 0 {
        return Some((
            ClientChipTooltipSide::Above,
            text_row_count.min(above_row_count),
        ));
    }
    None
}

/// The cells the widest title or text row needs.
fn compute_client_chip_tooltip_content_width(title: &Line<'_>, text_rows: &[Line<'_>]) -> u16 {
    std::iter::once(compute_line_cell_count(title))
        .chain(text_rows.iter().map(compute_line_cell_count))
        .max()
        .unwrap_or(0)
}

/// Where the box of `text_row_count` text rows sits, or `None` when the
/// viewport cannot hold it.
fn place_client_chip_tooltip(
    anchor_row_index: u16,
    anchor_start_column: u16,
    content_width: u16,
    text_row_count: usize,
    side: ClientChipTooltipSide,
    viewport_area: RatatuiRect,
) -> Option<RatatuiRect> {
    let text_row_count = u16::try_from(text_row_count).ok()?;
    let box_width = content_width
        .saturating_add(CLIENT_CHIP_TOOLTIP_CHROME_CELL_COUNT)
        .min(viewport_area.width);
    if box_width < MINIMUM_CLIENT_CHIP_TOOLTIP_WIDTH {
        return None;
    }
    let box_height = text_row_count.saturating_add(2);
    let box_column = anchor_start_column
        .min(viewport_area.right().saturating_sub(box_width))
        .max(viewport_area.x);
    let box_row = match side {
        ClientChipTooltipSide::Below => anchor_row_index.saturating_add(1),
        ClientChipTooltipSide::Above => anchor_row_index.saturating_sub(box_height),
    };
    if box_row < viewport_area.y || box_row.saturating_add(box_height) > viewport_area.bottom() {
        return None;
    }
    Some(RatatuiRect {
        x: box_column,
        y: box_row,
        width: box_width,
        height: box_height,
    })
}
